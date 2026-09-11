//! The const-evaluation engine: running a slice of the language at compile time.
//!
//! Four things in Nest need a value before the program does: a `const` generic
//! argument, an array length, a `#static` region's initial contents (§2.6), and
//! the right-hand side of any `::` constant — including `A :: my_const_func()`,
//! which is the whole reason a *checker* is not enough. §5.1 says a `#const`
//! function may be evaluated at compile time; this is the thing that evaluates
//! it.
//!
//! # An interpreter, not a folder
//!
//! It executes IR: locals, assignment, `if`, `match`, `loop`, calls into other
//! `#const` functions. That is what the spec promises — `#const` restricts
//! *calls* and run-time effects, not control flow — and a constant folder that
//! only handled `2 + 3` would reject `A :: factorial(5)`, which is exactly the
//! case the feature exists for.
//!
//! Running on the IR rather than the AST is what makes it tractable. Sugar is
//! gone, every node is typed, an operator is an explicit [`ExprKind::Call`]
//! carrying a `builtin` tag that says it *is* a machine instruction, and a
//! method call has already picked its callee. An AST evaluator would have to
//! re-derive all of that, and every surface form it failed to recognize would be
//! a hole.
//!
//! # The subset, and why it is stated as a subset
//!
//! [`ConstValue`] holds integers, floats, booleans, characters, `void`, and
//! composites of those. It deliberately holds **no pointer, no reference and no
//! heap value**: a compile-time address has nothing to point at in the compiled
//! program, so `&x` is refused rather than given a meaning that would not
//! survive to run time. Nest is garbage-collected, and a value that lives in the
//! collector's heap cannot be baked into a `.data` section either — which is the
//! same reason §2.6 keeps a static's initializer `#const` in the first place.
//!
//! Everything outside the subset is refused **with a reason and a span**, never
//! silently mis-evaluated. That is the important direction of the error: an
//! evaluator that guesses produces a program whose compile-time and run-time
//! answers differ.
//!
//! # Integers are arbitrary precision
//!
//! Values are [`BigInt`], as [`Lit::Int`] already is. A `comptime_int` has no
//! width by definition (§2.5), and even a typed constant's arithmetic is done
//! exactly and range-checked at the end rather than wrapped along the way — a
//! compile-time computation that silently wrapped would be a worse answer than a
//! rejected one. Division by zero and a shift by a negative amount are errors,
//! not traps.
//!
//! # Termination
//!
//! A `#const` function may loop, so the evaluator can be handed a program that
//! does not stop. It runs on a **step budget** and reports exhaustion as an
//! ordinary diagnostic. There is no cleverness here on purpose: the halting
//! problem is not solved by a compiler pass, and a budget turns "the build
//! hangs" into "this constant did not finish", which a person can act on.

use std::collections::HashMap;

use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive, Zero};

use crate::common::diagnostic::Diagnostic;
use crate::common::symbol::Symbol;
use crate::common::options::Target;
use crate::parser::ast::{BinOp, Lit, UnOp};
use crate::sema::builtins::BuiltinOp;
use crate::sema::def::{DefId, DefKind, DefTable};
use crate::sema::ty::{FloatWidth, Ty, float_fits, int_truncate};

use super::{
    Arm, Block, Dispatch, Expr, ExprKind, ImplicitCast, IrId, Linked, Meta, Pattern, PatternKind,
    Stmt, StmtKind, TypeDefKind,
};

/// How many expression evaluations one top-level request may take before the
/// evaluator gives up.
///
/// Large enough that no honest constant reaches it, small enough that a runaway
/// `#const` loop is reported in well under a second.
const STEP_BUDGET: u32 = 1_000_000;

/// How deep `#const` calls may nest. A separate limit from the step budget
/// because unbounded *recursion* would exhaust the host stack long before it
/// exhausted a step count.
const DEPTH_BUDGET: u32 = 128;

/// Which promise a `$cast` carries — see [`ConstEval::cast`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CastMode {
    /// The compiler inserted it, settling a literal on its use site's type. It
    /// must be exact.
    Implicit,
    /// The program wrote it. It converts, and may lose precision.
    Explicit,
}

// ===< Values >===

/// A value the const evaluator can produce.
///
/// The composite cases are all one variant deliberately: a tuple, a struct, a
/// fixed array and a variant payload are the same thing to every consumer of a
/// constant — an ordered list of values — and the IR node that built it already
/// says which shape it was. This is the same flattening `design/lir.md` §7b
/// applies to aggregates, for the same reason: each shape that keeps its own
/// case is one more case every consumer must learn.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstValue {
    Int(BigInt),
    Float(f64),
    Bool(bool),
    Char(char),
    /// The unit value — what a `#const` function with no result returns.
    Void,
    /// A `comptime_str` or a `str`: UTF-8 text, held as text because that is
    /// what the literal is. It is *data*, not a heap value — a string literal
    /// lives in the program's read-only section, so unlike a pointer it has
    /// something to point at in the compiled program (§1.5).
    Str(String),
    /// A `[]u8`: bytes with no UTF-8 promise, from a `b"..."` literal or from a
    /// string literal that settled on `[]u8`.
    Bytes(Vec<u8>),
    /// A tuple, a struct, or a fixed-size array, in declaration order.
    Aggregate(Vec<ConstValue>),
    /// An enum variant and its payload.
    Variant {
        name: Symbol,
        payload: Vec<ConstValue>,
    },
}

impl ConstValue {
    /// The value as a `u64`, for the consumers that need a length or an index.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            ConstValue::Int(n) => n.to_u64(),
            _ => None,
        }
    }

    /// A short rendering, for diagnostics and IR dumps.
    pub fn display(&self) -> String {
        match self {
            ConstValue::Int(n) => n.to_string(),
            ConstValue::Float(f) => f.to_string(),
            ConstValue::Bool(b) => b.to_string(),
            ConstValue::Char(c) => format!("'{c}'"),
            ConstValue::Void => "void".to_string(),
            ConstValue::Str(s) => format!("{s:?}"),
            ConstValue::Bytes(b) => crate::parser::ast::bytes_repr(b),
            ConstValue::Aggregate(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.display()).collect();
                format!("{{ {} }}", inner.join(", "))
            }
            ConstValue::Variant { name, payload } if payload.is_empty() => format!(".{name}"),
            ConstValue::Variant { name, payload } => {
                let inner: Vec<String> = payload.iter().map(|v| v.display()).collect();
                format!(".{name}({})", inner.join(", "))
            }
        }
    }
}

/// Why an evaluation could not produce a value.
///
/// It carries the [`IrId`] of the **innermost** construct that was the problem,
/// so the diagnostic points at the call or the operator that could not be run
/// rather than at the constant that happens to contain it.
#[derive(Debug, Clone)]
pub struct ConstError {
    pub at: IrId,
    pub message: String,
}

impl ConstError {
    fn new(at: IrId, message: impl Into<String>) -> Self {
        ConstError {
            at,
            message: message.into(),
        }
    }

    /// Render as a diagnostic, pointed at the span the failing node carries.
    pub fn to_diagnostic(&self, meta: &Meta, what: &str) -> Diagnostic {
        let mut d = Diagnostic::error(format!("{what} could not be evaluated: {}", self.message));
        if let Some(span) = meta.span(self.at) {
            d = d.with_primary(span, "not evaluable at compile time");
        }
        d
    }
}

type EvalResult = Result<ConstValue, ConstError>;

/// Where a block stopped, so an enclosing construct knows whether to keep going.
///
/// Structured control flow needs this because the IR keeps `return` / `break` /
/// `continue` as statements rather than edges: without a signal, a `return`
/// inside a loop inside an `if` would fall out as an ordinary value and the code
/// after it would run.
#[derive(Debug, Clone)]
enum Flow {
    /// The block finished; this is its value.
    Value(ConstValue),
    Return(ConstValue),
    Break(ConstValue),
    Continue,
}

// ===< The evaluator >===

/// The engine. One per top-level request, so its budget and its cycle set cover
/// the whole of that request rather than resetting per call.
pub struct ConstEval<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
    /// Locals in scope, innermost frame last. A `#const` function's frame does
    /// not see its caller's — a call pushes a fresh one.
    frames: Vec<HashMap<DefId, ConstValue>>,
    /// Globals currently being evaluated, to catch `A :: B` / `B :: A`. A cycle
    /// has no value, and following it would not terminate.
    in_progress: Vec<DefId>,
    steps: u32,
    depth: u32,
    /// The machine being compiled for, which fixes the width of `isize` /
    /// `usize` and so decides whether a constant of one of those types fits.
    target: Target,
    /// A `return` / `break` / `continue` that is unwinding.
    ///
    /// `if`, `match` and a bare block are **expressions** in Nest, so control
    /// flow can leave one from a value position: `let x := if c { break } else
    /// { 1 }` is legal, and `while` lowers to exactly that shape — a `loop`
    /// whose guard is `if !cond { break }`. An evaluator that returned only
    /// values would swallow those, so a pending flow is parked here and every
    /// step checks for it before doing more work.
    flow: Option<Flow>,
}

impl<'a> ConstEval<'a> {
    pub fn new(defs: &'a DefTable, meta: &'a Meta, linked: &'a Linked, target: Target) -> Self {
        ConstEval {
            defs,
            meta,
            linked,
            frames: vec![HashMap::new()],
            in_progress: Vec::new(),
            steps: 0,
            depth: 0,
            target,
            flow: None,
        }
    }

    /// Evaluate one expression, from a clean environment.
    pub fn eval(&mut self, e: &Expr) -> EvalResult {
        self.steps = 0;
        self.depth = 0;
        self.flow = None;
        let v = self.expr(e)?;
        // A `return` at the top of a constant's initializer is its value; a
        // stray `break` has no loop and is a mistake the evaluator should name.
        match self.flow.take() {
            None => Ok(v),
            Some(Flow::Return(v)) | Some(Flow::Value(v)) => Ok(v),
            Some(_) => Err(ConstError::new(e.id, "`break` / `continue` outside a loop")),
        }
    }

    /// Whether control flow is unwinding, in which case no further value is
    /// worth computing.
    fn unwinding(&self) -> bool {
        self.flow.is_some()
    }

    /// Turn a block's exit into the value its **expression** position yields,
    /// parking a `return` / `break` / `continue` so it keeps travelling
    /// outwards to the loop or function that owns it.
    fn escaping(&mut self, flow: Flow) -> ConstValue {
        match flow {
            Flow::Value(v) => v,
            other => {
                self.flow = Some(other);
                ConstValue::Void
            }
        }
    }

    /// Evaluate the initializer of the constant or static region `def` names.
    ///
    /// This is also how a reference to one is resolved mid-expression, which is
    /// why the cycle set lives on the evaluator rather than on the call.
    pub fn eval_global(&mut self, def: DefId) -> EvalResult {
        let def = self.defs.resolve_alias(def);
        let Some(global) = self.linked.global(def) else {
            // A def with no lowered global: a function used as a value, an
            // extern, or a trait's associated constant, which is a requirement
            // rather than a definition.
            return Err(ConstError::new(
                IrId(0),
                format!("`{}` has no compile-time value", self.defs.get(def).name),
            ));
        };
        if self.in_progress.contains(&def) {
            return Err(ConstError::new(
                global.id,
                format!("`{}` is defined in terms of itself", global.name),
            ));
        }
        let Some(init) = &global.init else {
            // A `#static` with no initializer is zeroed (§2.6). There is no
            // *value* to fold, and asking for one is a mistake in the caller.
            return Err(ConstError::new(
                global.id,
                format!("`{}` has no initializer; its region is zeroed", global.name),
            ));
        };
        self.in_progress.push(def);
        let out = self.expr(init);
        self.in_progress.pop();
        out
    }

    // ===< Expressions >===

    fn expr(&mut self, e: &Expr) -> EvalResult {
        // Control flow is leaving; nothing further contributes a value.
        if self.unwinding() {
            return Ok(ConstValue::Void);
        }
        self.steps += 1;
        if self.steps > STEP_BUDGET {
            return Err(ConstError::new(
                e.id,
                "compile-time evaluation did not finish within the step budget",
            ));
        }
        match &e.kind {
            ExprKind::Lit(l) => self.lit(l),

            ExprKind::Local(def) => self.lookup(e.id, *def),

            // A path to an item. A `const` item has a value; a `#static` region
            // does not — its contents are whatever the running program last
            // wrote, which is precisely what a compile-time answer cannot know.
            ExprKind::Global(def) => {
                let def = self.defs.resolve_alias(*def);
                let d = self.defs.get(def);
                if d.mutable {
                    return Err(ConstError::new(
                        e.id,
                        format!(
                            "`{}` is a `#static` region; its contents are a run-time value",
                            d.name
                        ),
                    ));
                }
                self.eval_global(def).map_err(|err| {
                    // Re-point an error raised against the *definition* at the
                    // use site when the definition has no span of its own.
                    if self.meta.span(err.at).is_none() {
                        ConstError::new(e.id, err.message)
                    } else {
                        err
                    }
                })
            }

            // A `<const N>` parameter has a value only once an instantiation has
            // chosen one. Before monomorphization it is a symbol, and saying so
            // is the honest answer.
            ExprKind::ConstParam(def) => self.lookup(e.id, *def).map_err(|_| {
                ConstError::new(
                    e.id,
                    format!(
                        "`{}` is a `const` generic parameter; its value is not known until \
                         the function is instantiated",
                        self.defs.get(*def).name
                    ),
                )
            }),

            ExprKind::Binary { op, lhs, rhs } => self.binary(e.id, *op, lhs, rhs),
            ExprKind::Unary { op, operand } => {
                let v = self.expr(operand)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                self.unary(e.id, *op, v)
            }

            ExprKind::Call {
                callee,
                args,
                builtin,
                dispatch,
            } => self.call(e, callee, args, *builtin, dispatch),

            ExprKind::Tuple { elems } => {
                let mut out = Vec::with_capacity(elems.len());
                for x in elems {
                    out.push(self.expr(x)?);
                    if self.unwinding() {
                        return Ok(ConstValue::Void);
                    }
                }
                Ok(ConstValue::Aggregate(out))
            }

            // A struct literal, in **declaration** order rather than written
            // order: a constant is a layout, and two literals that named the
            // same fields in a different order have to produce the same value.
            ExprKind::Construct { def, fields } => {
                let mut written: Vec<(Symbol, ConstValue)> = Vec::with_capacity(fields.len());
                for (name, x) in fields {
                    written.push((name.clone(), self.expr(x)?));
                    if self.unwinding() {
                        return Ok(ConstValue::Void);
                    }
                }
                Ok(ConstValue::Aggregate(self.in_member_order(*def, written)))
            }

            ExprKind::Variant { name, args } => {
                let mut payload = Vec::with_capacity(args.len());
                for x in args {
                    payload.push(self.expr(x)?);
                    if self.unwinding() {
                        return Ok(ConstValue::Void);
                    }
                }
                Ok(ConstValue::Variant {
                    name: name.clone(),
                    payload,
                })
            }

            ExprKind::Field { base, name, def } => {
                let v = self.expr(base)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                let index = self.field_index(base, *def, name).ok_or_else(|| {
                    ConstError::new(e.id, format!("`{name}` is not a field of this value"))
                })?;
                Self::project(e.id, v, index)
            }
            ExprKind::TupleIndex { base, index } => {
                let v = self.expr(base)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                Self::project(e.id, v, *index as usize)
            }
            ExprKind::Index { base, index } => {
                let v = self.expr(base)?;
                let i = self.expr(index)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                let Some(i) = i.as_u64() else {
                    return Err(ConstError::new(e.id, "an index must be an integer"));
                };
                Self::project(e.id, v, i as usize)
            }

            ExprKind::Block(b) => {
                let flow = self.block(b)?;
                Ok(self.escaping(flow))
            }

            ExprKind::If { cond, then, els } => {
                let c = self.truth(cond)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                let taken = if c { Some(then) } else { els.as_ref() };
                match taken {
                    None => Ok(ConstValue::Void),
                    Some(b) => {
                        let flow = self.block(b)?;
                        Ok(self.escaping(flow))
                    }
                }
            }

            ExprKind::Match { scrutinee, arms } => {
                let v = self.expr(scrutinee)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                self.match_arms(e.id, &v, arms)
            }

            ExprKind::Loop { body } => self.loop_(e.id, body),

            // `$cast` is the only intrinsic with a compile-time meaning here:
            // it is what lowering emits for a `comptime_int` becoming a runtime
            // integer, so *every* typed constant goes through one. The rest —
            // `$size_of`, the location directives, the GC intrinsics — need a
            // target layout or a running program, neither of which exists yet.
            ExprKind::Intrinsic { name, args } => self.intrinsic(e, name, args),

            ExprKind::Ref { .. } => Err(ConstError::new(
                e.id,
                "taking an address has no compile-time value",
            )),
            ExprKind::Deref { .. } => Err(ConstError::new(
                e.id,
                "reading through a pointer has no compile-time value",
            )),
            ExprKind::DynCast { .. } => Err(ConstError::new(
                e.id,
                "building a trait object needs a vtable, which does not exist yet",
            )),
            // Something earlier already reported why. Failing quietly here keeps
            // one mistake from reading as two.
            ExprKind::Error => Err(ConstError::new(e.id, "this expression is in error")),
        }
    }

    fn lit(&self, l: &Lit) -> EvalResult {
        match l {
            Lit::Int(n) => Ok(ConstValue::Int(n.clone())),
            Lit::Float(f) => Ok(ConstValue::Float(*f)),
            Lit::Bool(b) => Ok(ConstValue::Bool(*b)),
            Lit::Char(c) => Ok(ConstValue::Char(*c)),
            Lit::Str(s) => Ok(ConstValue::Str(s.clone())),
            Lit::Bytes(b) => Ok(ConstValue::Bytes(b.clone())),
        }
    }

    fn lookup(&self, at: IrId, def: DefId) -> EvalResult {
        for frame in self.frames.iter().rev() {
            if let Some(v) = frame.get(&def) {
                return Ok(v.clone());
            }
        }
        Err(ConstError::new(
            at,
            format!(
                "`{}` is a run-time binding with no compile-time value",
                self.defs.get(def).name
            ),
        ))
    }

    fn bind(&mut self, def: DefId, value: ConstValue) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(def, value);
        }
    }

    // ===< Calls >===

    fn call(
        &mut self,
        e: &Expr,
        callee: &Expr,
        args: &[Expr],
        builtin: Option<BuiltinOp>,
        dispatch: &Dispatch,
    ) -> EvalResult {
        // A builtin operator *is* a machine instruction: there is no function to
        // look up, and the `builtin` tag is exactly what makes recognizing one
        // O(1) rather than a name match against `core.Add.add`.
        if let Some(op) = builtin {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(self.expr(a)?);
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
            }
            let value = self.builtin_op(e.id, op, &vals)?;
            return self.fits_result(e.id, value);
        }
        match dispatch {
            Dispatch::Virtual { .. } => Err(ConstError::new(
                e.id,
                "a `dyn` call picks its target at run time",
            )),
            // The impl is chosen by monomorphization, which has not run. This is
            // the same deferral `check::constness` makes for a generic `#const`
            // body, and for the same reason.
            Dispatch::Generic { .. } => Err(ConstError::new(
                e.id,
                "the callee is chosen by monomorphization, which has not run yet",
            )),
            Dispatch::Static => {
                let Some(target) = self.callee_def(callee) else {
                    return Err(ConstError::new(e.id, "the callee is not a known function"));
                };
                if !self.is_const_fn(target) {
                    return Err(ConstError::new(
                        e.id,
                        format!(
                            "`{}` is not `#const`, so it cannot run at compile time (§5.1)",
                            self.defs.canonical_string(target)
                        ),
                    ));
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.expr(a)?);
                    if self.unwinding() {
                        return Ok(ConstValue::Void);
                    }
                }
                self.call_const_fn(e.id, target, vals)
            }
        }
    }

    fn call_const_fn(&mut self, at: IrId, target: DefId, args: Vec<ConstValue>) -> EvalResult {
        if self.depth >= DEPTH_BUDGET {
            return Err(ConstError::new(
                at,
                "compile-time evaluation recursed too deeply",
            ));
        }
        let Some(func) = self.linked.get(target) else {
            return Err(ConstError::new(
                at,
                format!(
                    "`{}` has no body to run at compile time",
                    self.defs.canonical_string(target)
                ),
            ));
        };
        let Some(body) = func.body.clone() else {
            return Err(ConstError::new(
                at,
                format!(
                    "`{}` is a declaration with no body",
                    self.defs.canonical_string(target)
                ),
            ));
        };
        // A callee's frame is fresh: it sees its parameters and nothing of the
        // caller's locals.
        let mut frame = HashMap::new();
        for (param, value) in func.params.iter().zip(args) {
            frame.insert(param.def, value);
        }
        self.frames.push(frame);
        self.depth += 1;
        let out = self.block(&body);
        self.depth -= 1;
        self.frames.pop();
        match out? {
            // A body that falls off its end with no tail returns `void`; one
            // whose tail is the result returns that.
            Flow::Value(v) | Flow::Return(v) => Ok(v),
            Flow::Break(_) | Flow::Continue => Err(ConstError::new(
                at,
                "`break` / `continue` escaped the function body",
            )),
        }
    }

    fn callee_def(&self, callee: &Expr) -> Option<DefId> {
        match &callee.kind {
            ExprKind::Global(def) => Some(self.defs.resolve_alias(*def)),
            _ => None,
        }
    }

    /// Whether `def` is a function marked `#const`.
    ///
    /// Read off the *def* as well as the lowered function: a callee in another
    /// file is not the function being walked, and a declaration has no lowered
    /// body to carry directives.
    fn is_const_fn(&self, def: DefId) -> bool {
        if let Some(f) = self.linked.get(def)
            && self.meta.has_directive(f.id, "const")
        {
            return true;
        }
        self.defs
            .get(def)
            .directives
            .iter()
            .any(|d| d.name.as_str() == "const")
    }

    // ===< Statements and control flow >===

    fn block(&mut self, b: &Block) -> Result<Flow, ConstError> {
        // `defer` bodies run on every exit from a block, which for a compile-time
        // value means they can only be effects on locals that are about to go out
        // of scope. Refusing is the honest answer rather than running them in the
        // wrong order.
        if !b.defers.is_empty() {
            return Err(ConstError::new(b.id, "`defer` has no compile-time meaning"));
        }
        for s in &b.stmts {
            let flow = self.stmt(s)?;
            // A flow parked by a nested expression takes precedence: the
            // statement "finished", but only because something is unwinding
            // through it.
            if let Some(parked) = self.flow.take() {
                return Ok(parked);
            }
            match flow {
                Flow::Value(_) => {}
                other => return Ok(other),
            }
        }
        match &b.tail {
            Some(t) => {
                let v = self.expr(t)?;
                match self.flow.take() {
                    Some(parked) => Ok(parked),
                    None => Ok(Flow::Value(v)),
                }
            }
            None => Ok(Flow::Value(ConstValue::Void)),
        }
    }

    fn stmt(&mut self, s: &Stmt) -> Result<Flow, ConstError> {
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                let v = self.expr(init)?;
                // A `let` pattern is irrefutable, so a failure to bind is an
                // evaluator bug rather than a program error — but say so rather
                // than binding nothing and reading a stale value later.
                if !self.bind_pattern(pattern, &v) {
                    return Err(ConstError::new(
                        s.id,
                        "this binding pattern could not be matched at compile time",
                    ));
                }
                Ok(Flow::Value(ConstValue::Void))
            }
            // Only a whole local may be assigned. A projection (`x.f = 1`) would
            // need places rather than values, which is a representation this
            // subset does not have.
            StmtKind::Assign { place, value } => {
                let v = self.expr(value)?;
                match &place.kind {
                    ExprKind::Local(def) => {
                        self.bind(*def, v);
                        Ok(Flow::Value(ConstValue::Void))
                    }
                    _ => Err(ConstError::new(
                        place.id,
                        "only a whole local may be assigned at compile time",
                    )),
                }
            }
            StmtKind::Expr(e) => {
                self.expr(e)?;
                Ok(Flow::Value(ConstValue::Void))
            }
            StmtKind::Return(v) => Ok(Flow::Return(match v {
                Some(e) => self.expr(e)?,
                None => ConstValue::Void,
            })),
            StmtKind::Break(v) => Ok(Flow::Break(match v {
                Some(e) => self.expr(e)?,
                None => ConstValue::Void,
            })),
            StmtKind::Continue => Ok(Flow::Continue),
        }
    }

    /// The IR has one infinite `Loop`; `while` and `for` became this before
    /// lowering finished, so there is a single form to run.
    fn loop_(&mut self, at: IrId, body: &Block) -> EvalResult {
        loop {
            self.steps += 1;
            if self.steps > STEP_BUDGET {
                return Err(ConstError::new(
                    at,
                    "compile-time evaluation did not finish within the step budget",
                ));
            }
            match self.block(body)? {
                Flow::Value(_) | Flow::Continue => {}
                // The loop is what a `break` was travelling to, so it stops
                // here and becomes the loop's value.
                Flow::Break(v) => return Ok(v),
                // A `return` leaves the enclosing *function*, so it has to keep
                // going past this loop.
                Flow::Return(v) => {
                    self.flow = Some(Flow::Return(v));
                    return Ok(ConstValue::Void);
                }
            }
        }
    }

    fn match_arms(&mut self, at: IrId, scrutinee: &ConstValue, arms: &[Arm]) -> EvalResult {
        for arm in arms {
            // Each arm's bindings are its own; a failed match must leave nothing
            // behind for the next arm to read.
            self.frames.push(HashMap::new());
            let matched = self.bind_pattern(&arm.pattern, scrutinee);
            let guard_ok = match (&matched, &arm.guard) {
                (true, Some(g)) => match self.truth(g) {
                    Ok(b) => b,
                    Err(e) => {
                        self.frames.pop();
                        return Err(e);
                    }
                },
                (true, None) => true,
                (false, _) => false,
            };
            if !guard_ok {
                self.frames.pop();
                continue;
            }
            let out = self.expr(&arm.body);
            self.frames.pop();
            return out;
        }
        // Exhaustiveness is checked by its own pass, so reaching here means the
        // scrutinee is a value that pass could not enumerate — a float or a
        // large integer range — rather than a hole in the program.
        Err(ConstError::new(at, "no arm matched at compile time"))
    }

    fn truth(&mut self, e: &Expr) -> Result<bool, ConstError> {
        let v = self.expr(e)?;
        // Unwinding: the caller checks `unwinding()` and discards this.
        if self.unwinding() {
            return Ok(false);
        }
        match v {
            ConstValue::Bool(b) => Ok(b),
            other => Err(ConstError::new(
                e.id,
                format!("expected a boolean, found `{}`", other.display()),
            )),
        }
    }

    // ===< Patterns >===

    /// Match `value` against `pattern`, binding what it names. Returns whether
    /// it matched; bindings made by a partial match are left in the current
    /// frame, which the caller discards.
    fn bind_pattern(&mut self, pattern: &Pattern, value: &ConstValue) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard => true,
            PatternKind::Binding { def, .. } => {
                self.bind(*def, value.clone());
                true
            }
            PatternKind::Lit(l) => match (l, value) {
                (Lit::Int(a), ConstValue::Int(b)) => a == b,
                (Lit::Bool(a), ConstValue::Bool(b)) => a == b,
                (Lit::Char(a), ConstValue::Char(b)) => a == b,
                (Lit::Float(a), ConstValue::Float(b)) => a == b,
                _ => false,
            },
            PatternKind::Or(alts) => alts.iter().any(|p| self.bind_pattern(p, value)),
            PatternKind::Tuple(subs) => match value {
                ConstValue::Aggregate(items) if items.len() == subs.len() => subs
                    .iter()
                    .zip(items)
                    .all(|(p, v)| self.bind_pattern(p, &v.clone())),
                _ => false,
            },
            PatternKind::Variant { name, sub } => match value {
                ConstValue::Variant { name: got, payload }
                    if got == name && payload.len() >= sub.len() =>
                {
                    sub.iter()
                        .zip(payload)
                        .all(|(p, v)| self.bind_pattern(p, &v.clone()))
                }
                _ => false,
            },
            PatternKind::Struct { def, fields, .. } => {
                let ConstValue::Aggregate(items) = value else {
                    return false;
                };
                let items = items.clone();
                fields.iter().all(|(name, p)| {
                    match def.and_then(|d| self.member_index(d, name)) {
                        Some(i) if i < items.len() => self.bind_pattern(p, &items[i]),
                        // Without a named struct there is no declaration order
                        // to look the field up in.
                        _ => false,
                    }
                })
            }
            PatternKind::TupleStruct { elems, rest, .. } => match value {
                ConstValue::Aggregate(items)
                    if items.len() == elems.len() || (*rest && items.len() >= elems.len()) =>
                {
                    let items = items.clone();
                    elems
                        .iter()
                        .zip(&items)
                        .all(|(p, v)| self.bind_pattern(p, v))
                }
                _ => false,
            },
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let ConstValue::Aggregate(items) = value else {
                    return false;
                };
                let items = items.clone();
                let least = prefix.len() + suffix.len();
                let long_enough = match rest {
                    Some(_) => items.len() >= least,
                    None => items.len() == least,
                };
                if !long_enough {
                    return false;
                }
                for (p, v) in prefix.iter().zip(&items) {
                    if !self.bind_pattern(p, v) {
                        return false;
                    }
                }
                for (p, v) in suffix
                    .iter()
                    .zip(items[items.len() - suffix.len()..].iter())
                {
                    if !self.bind_pattern(p, v) {
                        return false;
                    }
                }
                // The `..` may name the middle; it is a slice, which this subset
                // has no value for, so a named rest cannot be bound.
                if let Some(Some(_)) = rest {
                    return false;
                }
                true
            }
            PatternKind::Range {
                start,
                end,
                inclusive,
            } => Self::in_range(value, start.as_ref(), end.as_ref(), *inclusive),
            // `name @ p` binds the whole value and keeps testing it, so both
            // halves have to happen — and in that order, since the inner test
            // may fail after the binding was made and the caller discards the
            // frame either way.
            PatternKind::At { binding, pattern } => {
                self.bind(binding.def, value.clone());
                self.bind_pattern(pattern, value)
            }
            // `&p` matches through a reference. There is no pointer in this
            // subset to look through, so there is nothing this can match.
            PatternKind::Deref(_) => false,
        }
    }

    fn in_range(
        value: &ConstValue,
        start: Option<&Lit>,
        end: Option<&Lit>,
        inclusive: bool,
    ) -> bool {
        let ConstValue::Int(v) = value else {
            // Only integer ranges are in the subset; a `char` or float range
            // would need the same care and has no use site yet.
            return false;
        };
        let lit_int = |l: &Lit| match l {
            Lit::Int(n) => Some(n.clone()),
            _ => None,
        };
        if let Some(s) = start.and_then(lit_int)
            && *v < s
        {
            return false;
        }
        match end.and_then(lit_int) {
            Some(e) if inclusive => *v <= e,
            Some(e) => *v < e,
            None => true,
        }
    }

    // ===< Operators >===

    fn binary(&mut self, at: IrId, op: BinOp, lhs: &Expr, rhs: &Expr) -> EvalResult {
        // `&&` / `||` short-circuit, which matters: the right operand of a
        // guarded `&&` may be the one that would fail to evaluate.
        match op {
            BinOp::And => {
                return Ok(ConstValue::Bool(self.truth(lhs)? && self.truth(rhs)?));
            }
            BinOp::Or => {
                return Ok(ConstValue::Bool(self.truth(lhs)? || self.truth(rhs)?));
            }
            _ => {}
        }
        let a = self.expr(lhs)?;
        let b = self.expr(rhs)?;
        if self.unwinding() {
            return Ok(ConstValue::Void);
        }
        match op {
            BinOp::Eq => return Ok(ConstValue::Bool(a == b)),
            BinOp::Ne => return Ok(ConstValue::Bool(a != b)),
            _ => {}
        }
        if let (ConstValue::Int(x), ConstValue::Int(y)) = (&a, &b) {
            return self.fits_result(at, Self::int_binary(at, op, x, y)?);
        }
        if let (ConstValue::Float(x), ConstValue::Float(y)) = (&a, &b) {
            return Self::float_binary(at, op, *x, *y);
        }
        if let (ConstValue::Char(x), ConstValue::Char(y)) = (&a, &b) {
            return match op {
                BinOp::Lt => Ok(ConstValue::Bool(x < y)),
                BinOp::Le => Ok(ConstValue::Bool(x <= y)),
                BinOp::Gt => Ok(ConstValue::Bool(x > y)),
                BinOp::Ge => Ok(ConstValue::Bool(x >= y)),
                _ => Err(ConstError::new(
                    at,
                    "this operator does not apply to `char`",
                )),
            };
        }
        Err(ConstError::new(
            at,
            format!(
                "cannot apply this operator to `{}` and `{}`",
                a.display(),
                b.display()
            ),
        ))
    }

    fn int_binary(at: IrId, op: BinOp, x: &BigInt, y: &BigInt) -> EvalResult {
        // Division and remainder by zero are a *trap* at run time; at compile
        // time there is nothing to trap, so they are an error with a reason.
        let nonzero = |v: &BigInt| {
            if v.is_zero() {
                Err(ConstError::new(at, "division by zero"))
            } else {
                Ok(())
            }
        };
        // A shift amount is a count, not a value: a negative or absurd one has
        // no meaning rather than a wrapped one.
        let shift_amount = |v: &BigInt| {
            v.to_u32().filter(|n| *n < 4096).ok_or_else(|| {
                ConstError::new(at, "shift amount is negative or unreasonably large")
            })
        };
        let v = match op {
            BinOp::Add => ConstValue::Int(x + y),
            BinOp::Sub => ConstValue::Int(x - y),
            BinOp::Mul => ConstValue::Int(x * y),
            BinOp::Div => {
                nonzero(y)?;
                ConstValue::Int(x / y)
            }
            BinOp::Rem => {
                nonzero(y)?;
                ConstValue::Int(x % y)
            }
            BinOp::BitAnd => ConstValue::Int(x & y),
            BinOp::BitOr => ConstValue::Int(x | y),
            BinOp::BitXor => ConstValue::Int(x ^ y),
            BinOp::Shl => ConstValue::Int(x << shift_amount(y)?),
            BinOp::Shr => ConstValue::Int(x >> shift_amount(y)?),
            BinOp::Lt => ConstValue::Bool(x < y),
            BinOp::Le => ConstValue::Bool(x <= y),
            BinOp::Gt => ConstValue::Bool(x > y),
            BinOp::Ge => ConstValue::Bool(x >= y),
            BinOp::Eq | BinOp::Ne | BinOp::And | BinOp::Or => {
                return Err(ConstError::new(at, "unexpected operator on integers"));
            }
        };
        Ok(v)
    }

    fn float_binary(at: IrId, op: BinOp, x: f64, y: f64) -> EvalResult {
        let v = match op {
            BinOp::Add => ConstValue::Float(x + y),
            BinOp::Sub => ConstValue::Float(x - y),
            BinOp::Mul => ConstValue::Float(x * y),
            BinOp::Div => ConstValue::Float(x / y),
            BinOp::Rem => ConstValue::Float(x % y),
            BinOp::Lt => ConstValue::Bool(x < y),
            BinOp::Le => ConstValue::Bool(x <= y),
            BinOp::Gt => ConstValue::Bool(x > y),
            BinOp::Ge => ConstValue::Bool(x >= y),
            _ => {
                return Err(ConstError::new(
                    at,
                    "this operator does not apply to floats",
                ));
            }
        };
        Ok(v)
    }

    fn unary(&self, at: IrId, op: UnOp, v: ConstValue) -> EvalResult {
        match (op, v) {
            (UnOp::Neg, ConstValue::Int(n)) => Ok(ConstValue::Int(-n)),
            (UnOp::Neg, ConstValue::Float(f)) => Ok(ConstValue::Float(-f)),
            (UnOp::Not, ConstValue::Bool(b)) => Ok(ConstValue::Bool(!b)),
            (UnOp::BitNot, ConstValue::Int(n)) => Ok(ConstValue::Int(!n)),
            (UnOp::Ref | UnOp::RefMut, _) => Err(ConstError::new(
                at,
                "taking an address has no compile-time value",
            )),
            (_, other) => Err(ConstError::new(
                at,
                format!("cannot apply this operator to `{}`", other.display()),
            )),
        }
    }

    /// A primitive operator that reached the IR as a tagged [`ExprKind::Call`].
    ///
    /// The operands are already values, so this is the same arithmetic as
    /// [`Self::binary`] reached by the other route — an `i32 + i32` and a user
    /// `impl Add` are one node in the IR, and only the tag separates them.
    fn builtin_op(&self, at: IrId, op: BuiltinOp, args: &[ConstValue]) -> EvalResult {
        let bin = |o: BinOp| -> EvalResult {
            let [a, b] = args else {
                return Err(ConstError::new(at, "a binary operator wants two operands"));
            };
            match (a, b) {
                (ConstValue::Int(x), ConstValue::Int(y)) => Self::int_binary(at, o, x, y),
                (ConstValue::Float(x), ConstValue::Float(y)) => Self::float_binary(at, o, *x, *y),
                _ => Err(ConstError::new(at, "this operator applies to numbers only")),
            }
        };
        match op {
            BuiltinOp::Add => bin(BinOp::Add),
            BuiltinOp::Sub => bin(BinOp::Sub),
            BuiltinOp::Mul => bin(BinOp::Mul),
            BuiltinOp::Div => bin(BinOp::Div),
            BuiltinOp::Rem => bin(BinOp::Rem),
            BuiltinOp::BitAnd => bin(BinOp::BitAnd),
            BuiltinOp::BitOr => bin(BinOp::BitOr),
            BuiltinOp::BitXor => bin(BinOp::BitXor),
            BuiltinOp::Shl => bin(BinOp::Shl),
            BuiltinOp::Shr => bin(BinOp::Shr),
            BuiltinOp::Neg => match args {
                [v] => self.unary(at, UnOp::Neg, v.clone()),
                _ => Err(ConstError::new(at, "`-` wants one operand")),
            },
            BuiltinOp::BitNot => match args {
                [v] => self.unary(at, UnOp::BitNot, v.clone()),
                _ => Err(ConstError::new(at, "`~` wants one operand")),
            },
        }
    }

    // ===< Intrinsics >===

    fn intrinsic(&mut self, e: &Expr, name: &Symbol, args: &[Expr]) -> EvalResult {
        match name.as_str() {
            // Every typed constant reaches here: `A: u8 :: 5` lowers the `5`
            // as a `comptime_int` and casts it, which is how the conversion is
            // made explicit rather than silent (§2.5).
            "cast" => {
                let [v] = args else {
                    return Err(ConstError::new(e.id, "`$cast` wants one argument"));
                };
                let value = self.expr(v)?;
                if self.unwinding() {
                    return Ok(ConstValue::Void);
                }
                // Whether this cast is the compiler's or the program's decides
                // what it promises — see [`ImplicitCast`].
                let mode = match self.meta.get::<ImplicitCast>(e.id) {
                    Some(_) => CastMode::Implicit,
                    None => CastMode::Explicit,
                };
                self.cast(e.id, value, &self.meta.ty_or_error(e.id), mode)
            }
            other => Err(ConstError::new(
                e.id,
                format!("`${other}` has no compile-time value in this subset"),
            )),
        }
    }

    /// The signedness and width of the integer type a cast targets.
    ///
    /// A member of the family whose arguments are still symbolic — `int.<N, S>`
    /// inside the impl in `core` — has no width until monomorphization chooses
    /// one, and no constant can be narrowed to it here. Nothing reaches this
    /// today, because a generic `#const` function's body is not evaluated until
    /// it is instantiated (phase 6); reporting rather than passing the value
    /// through is what keeps it from silently producing an unchecked constant
    /// if that ever changes.
    fn int_parts_at(&self, at: IrId, to: &Ty) -> Result<(bool, u32), ConstError> {
        to.int_parts(self.target).ok_or_else(|| {
            ConstError::new(
                at,
                format!(
                    "`{}` has no width until it is instantiated",
                    to.display(self.defs)
                ),
            )
        })
    }

    /// Convert a value to the type a `$cast` names.
    ///
    /// What "convert" means depends on **who wrote the cast** (see
    /// [`ImplicitCast`]):
    ///
    /// - A cast the compiler inserted must be **exact**. It is the conversion
    ///   that settles an untyped literal on the type its use site asked for
    ///   (§2.5), and nothing in the source said `300` should become `44`, so a
    ///   value the target cannot hold is a mistake. This is the only place it
    ///   can be caught exactly, with the arbitrary-precision number still in
    ///   hand.
    /// - A cast the **program** wrote may lose precision, because that is what
    ///   it does at run time: `$cast.<u8>(300)` is `44` in a constant for the
    ///   same reason it is `44` in a running program. A compile-time answer that
    ///   differed from the run-time one would be worse than either.
    fn cast(&self, at: IrId, value: ConstValue, to: &Ty, mode: CastMode) -> EvalResult {
        let exact = mode == CastMode::Implicit;
        match (&value, to) {
            (_, Ty::Error) => Ok(value),
            // Every integer arm below wants a concrete width, and gets it from
            // `int_parts_at`.
            (ConstValue::Int(n), Ty::Int { .. }) => {
                let (signed, bits) = self.int_parts_at(at, to)?;
                if !exact {
                    return Ok(ConstValue::Int(int_truncate(n, signed, bits)));
                }
                if !int_fits(n, signed, bits) {
                    return Err(ConstError::new(
                        at,
                        format!("`{n}` does not fit in `{}`", to.display(self.defs)),
                    ));
                }
                Ok(ConstValue::Int(n.clone()))
            }
            (ConstValue::Int(_), Ty::ComptimeInt) => Ok(value),
            (ConstValue::Int(n), Ty::Float(w)) => {
                let f = n.to_f64().ok_or_else(|| {
                    ConstError::new(at, "this integer is not representable as a float")
                })?;
                self.float_to(at, f, Some(*w), exact, to)
            }
            (ConstValue::Float(f), Ty::Float(w)) => self.float_to(at, *f, Some(*w), exact, to),
            (ConstValue::Float(_), Ty::ComptimeFloat) => Ok(value),
            // Only a written `$cast` reaches this: the language has no implicit
            // float-to-integer conversion, so there is no exact form of it to
            // define. It truncates toward zero, as the machine does.
            (ConstValue::Float(f), Ty::Int { .. }) => {
                let (signed, bits) = self.int_parts_at(at, to)?;
                if !f.is_finite() {
                    return Err(ConstError::new(
                        at,
                        "an infinite or NaN float has no integer value",
                    ));
                }
                let truncated = BigInt::from_f64(f.trunc()).ok_or_else(|| {
                    ConstError::new(at, "this float has no integer value at compile time")
                })?;
                // Out of range is undefined at run time and unknowable here, so
                // it is reported rather than guessed at — unlike an integer
                // narrowing, which has one answer the machine agrees with.
                if !int_fits(&truncated, signed, bits) {
                    return Err(ConstError::new(
                        at,
                        format!("`{truncated}` does not fit in `{}`", to.display(self.defs)),
                    ));
                }
                Ok(ConstValue::Int(truncated))
            }
            (ConstValue::Char(c), Ty::Int { .. }) => {
                let (signed, bits) = self.int_parts_at(at, to)?;
                let n = BigInt::from(*c as u32);
                if !exact {
                    return Ok(ConstValue::Int(int_truncate(&n, signed, bits)));
                }
                if !int_fits(&n, signed, bits) {
                    return Err(ConstError::new(at, "this `char` does not fit"));
                }
                Ok(ConstValue::Int(n))
            }
            // A `comptime_str` materializing as one of the three types §1.5
            // lets it become. `str` is a `distinct []u8` and falls to the
            // nominal case below, keeping the text; `[]u8` is those same bytes;
            // `[]char` is a real transcoding, and this is the compile time the
            // spec says it happens at.
            (ConstValue::Str(_), Ty::ComptimeStr) => Ok(value),
            (ConstValue::Str(s), Ty::Slice { inner, .. }) => match **inner {
                Ty::Char => Ok(ConstValue::Aggregate(
                    s.chars().map(ConstValue::Char).collect(),
                )),
                _ => Ok(ConstValue::Bytes(s.clone().into_bytes())),
            },
            (ConstValue::Bytes(_), Ty::Slice { .. }) => Ok(value),
            // A `distinct` type is a newtype over its representation (§2.4), so
            // the value passes through unchanged.
            (_, Ty::Nominal { .. }) => Ok(value),
            (ConstValue::Bool(_), Ty::Bool) => Ok(value),
            (ConstValue::Char(_), Ty::Char) => Ok(value),
            _ => Err(ConstError::new(
                at,
                format!(
                    "cannot convert `{}` to `{}` at compile time",
                    value.display(),
                    to.display(self.defs)
                ),
            )),
        }
    }

    // ===< Aggregates >===

    /// Reorder a struct literal's written fields into the type's **declaration**
    /// order, which is what every consumer of a constant aggregate expects.
    fn in_member_order(&self, def: DefId, written: Vec<(Symbol, ConstValue)>) -> Vec<ConstValue> {
        let Some(order) = self.member_names(def) else {
            return written.into_iter().map(|(_, v)| v).collect();
        };
        let mut by_name: HashMap<Symbol, ConstValue> = written.into_iter().collect();
        order
            .iter()
            .map(|n| by_name.remove(n).unwrap_or(ConstValue::Void))
            .collect()
    }

    fn member_names(&self, def: DefId) -> Option<Vec<Symbol>> {
        use super::TypeDefKind;
        match &self.linked.ty(def)?.kind {
            TypeDefKind::Struct { members } => {
                Some(members.iter().map(|m| m.name.clone()).collect())
            }
            TypeDefKind::Distinct { repr } => Some(vec![repr.name.clone()]),
            _ => None,
        }
    }

    fn member_index(&self, def: DefId, name: &Symbol) -> Option<usize> {
        self.member_names(def)?.iter().position(|n| n == name)
    }

    /// The position of the field a [`ExprKind::Field`] names, found through the
    /// *base's type* rather than the field's own def — a positional member of a
    /// tuple struct has no def of its own.
    fn field_index(&self, base: &Expr, def: Option<DefId>, name: &Symbol) -> Option<usize> {
        if let Some(fd) = def
            && let Some(parent) = self.defs.get(fd).parent
            && self.defs.get(parent).kind != DefKind::Namespace
            && let Some(i) = self.member_index(parent, name)
        {
            return Some(i);
        }
        match self.meta.ty_or_error(base.id) {
            Ty::Nominal { def, .. } => self.member_index(def, name),
            _ => None,
        }
    }

    /// Reject an arithmetic result the type it was computed at cannot hold.
    ///
    /// `P: u8 :: 200 * 2` multiplies **at `u8`** — the operands were settled
    /// on `u8` before the multiply — so `400` is not a `u8` constant, and
    /// storing it would make the compiler claim a byte holds four hundred.
    /// Refusing is the same answer division by zero already gets, for the same
    /// reason: at compile time there is nothing to trap, and inventing a
    /// wrapped value would decide on the language's behalf that arithmetic
    /// wraps — which the spec does not yet say. A written `$cast` is still the
    /// way to ask for the low bits.
    ///
    /// A `comptime_int` result is *not* checked: it has no width by definition,
    /// and exact arbitrary-precision arithmetic is the whole point of one.
    fn fits_result(&self, at: IrId, value: ConstValue) -> EvalResult {
        let ConstValue::Int(n) = &value else {
            return Ok(value);
        };
        let declared = self.meta.ty_or_error(at);
        // A symbolic `int.<N, S>` has no range to check against, so there is
        // nothing to say until monomorphization picks the width.
        let Some((signed, bits)) = self.repr_of(&declared).int_parts(self.target) else {
            return Ok(value);
        };
        if int_fits(n, signed, bits) {
            return Ok(value);
        }
        Err(ConstError::new(
            at,
            format!("`{n}` does not fit in `{}`", declared.display(self.defs)),
        ))
    }

    /// The primitive a value of this type is stored as: the type itself, or —
    /// for a `distinct` numeric — what it stands over (§2.4), following a chain
    /// of them.
    fn repr_of(&self, ty: &Ty) -> Ty {
        let mut cur = ty.clone();
        for _ in 0..16 {
            let Ty::Nominal { def, .. } = cur else {
                return cur;
            };
            let Some(TypeDefKind::Distinct { repr }) = self.linked.ty(def).map(|t| &t.kind) else {
                return Ty::Nominal {
                    def,
                    args: Vec::new(),
                };
            };
            cur = self.meta.ty_or_error(repr.id);
        }
        cur
    }

    /// Narrow a float to `width`, rounding as the machine would.
    ///
    /// An exact (compiler-inserted) conversion additionally refuses a value the
    /// width cannot hold **at all** — one that overflows to infinity, or a
    /// non-zero one that underflows to zero. Ordinary rounding is never an
    /// error: `0.1` is not an `f64` either, and rejecting it would reject nearly
    /// every float literal ever written (see
    /// [`float_fits`](crate::sema::ty::float_fits)).
    fn float_to(
        &self,
        at: IrId,
        value: f64,
        width: Option<FloatWidth>,
        exact: bool,
        to: &Ty,
    ) -> EvalResult {
        let Some(width) = width else {
            return Ok(ConstValue::Float(value));
        };
        if exact && !float_fits(value, width) {
            return Err(ConstError::new(
                at,
                format!("`{value:?}` does not fit in `{}`", to.display(self.defs)),
            ));
        }
        // Store what the target actually holds, so a constant and the same
        // expression at run time are the same number. `f16` is range-checked
        // above but not rounded: the host has no `f16` to round through, and
        // the bootstrap emits no code yet that would notice.
        let stored = match width {
            FloatWidth::F32 => value as f32 as f64,
            _ => value,
        };
        Ok(ConstValue::Float(stored))
    }

    fn project(at: IrId, value: ConstValue, index: usize) -> EvalResult {
        match value {
            ConstValue::Aggregate(items) => items.into_iter().nth(index).ok_or_else(|| {
                ConstError::new(at, format!("index {index} is out of range for this value"))
            }),
            other => Err(ConstError::new(
                at,
                format!("`{}` has no members to project out of", other.display()),
            )),
        }
    }
}

/// Whether `n` is representable in an integer type of this width and signedness.
///
/// `bits` is already resolved, which is where the pointer-sized case is dealt
/// with: the question "does this constant fit a `usize`" has no answer that is
/// independent of the machine, so the [`Target`] is consulted by the caller —
/// through [`Ty::int_parts`] — rather than assumed here.
fn int_fits(n: &BigInt, signed: bool, bits: u32) -> bool {
    if bits == 0 {
        return n.is_zero();
    }
    if signed {
        let bound = BigInt::from(1) << (bits - 1);
        *n >= -bound.clone() && *n < bound
    } else {
        *n >= BigInt::from(0) && *n < (BigInt::from(1) << bits)
    }
}
