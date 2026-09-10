//! The `#const` check: what may run at compile time.
//!
//! Two rules, for two different questions, and they are deliberately not the
//! same rule.
//!
//! # 1. A `#const func` body
//!
//! §5.1: the body is restricted to a compile-time-evaluable subset — no
//! run-time I/O, no run-time allocation, and **only calls to other `#const`
//! functions and intrinsics**. Everything else is fine: locals, loops, `match`,
//! arithmetic. The function may be evaluated at compile time *and* called
//! normally at run time, which is what lets a call to it sit on the right-hand
//! side of a `::`.
//!
//! Enforcement is "is every construct const-safe?" — a structural walk, with no
//! whole-program value tracking. That is the spec's wording and it is also the
//! only thing that can be checked here: whether a particular call *would*
//! terminate, or what value it would produce, is the const evaluator's business,
//! and this pass runs long before it.
//!
//! # 2. A constant expression
//!
//! A **default argument** (§5.2) is not a function body; it is one expression
//! evaluated afresh at each call site, and it must be *compile-time known at
//! that call site*. The admissible forms are much narrower:
//!
//! - a literal;
//! - a path to a `const` item or a `const` generic parameter;
//! - a composite literal, every element of which is const;
//! - a `$cast` of a const;
//! - the location directives, which are per-call-site but still compile-time;
//! - a call to a function marked `#const`.
//!
//! Note the phrasing: **compile-time known at the call site**, not "one fixed
//! value". `#caller_location` differs at every call and is still admissible.
//!
//! This is what makes defaults honest. Without it, `y: i32 := read_config()`
//! parses, type-checks, and silently runs arbitrary code at every call.
//!
//! # Why the IR
//!
//! Because sugar is already gone. An operator here is an explicit call carrying
//! the trait method it resolved to and a `builtin` tag; `for` is a loop; `.?` is
//! a match. On the AST this pass would have to handle every surface form *and*
//! re-derive which operator became which call, redoing work inference already
//! did — and any form it failed to recognize would be a hole.

use crate::common::diagnostic::Diagnostic;
use crate::sema::def::{DefId, DefKind, DefTable};

use crate::ir::{Block, DefaultValue, Dispatch, Expr, ExprKind, Function, IrId, Linked, Meta};

/// Report every construct that may not run at compile time where one must.
pub fn check(defs: &DefTable, meta: &Meta, linked: &Linked, out: &mut Vec<Diagnostic>) {
    let cx = Cx { defs, meta, linked };

    for func in linked.funcs() {
        let Some(body) = &func.body else { continue };
        if meta.has_directive(func.id, "const") {
            cx.check_body(func, body, out);
        }
    }

    // Default arguments, at their declarations. Each is checked exactly once,
    // whether the function is called never, once or a hundred times — the
    // mistake is in the declaration, and reporting it per call site would turn
    // one wrong default into a wall of identical diagnostics.
    for func in linked.funcs() {
        for param in &func.params {
            if let Some(DefaultValue(e)) = meta.get::<DefaultValue>(param.id) {
                cx.check_const_expr(&e, out);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Cx<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    linked: &'a Linked,
}

impl Cx<'_> {
    // ===< 1. A `#const` function body >===

    /// Every call in a `#const` body must reach a `#const` function.
    fn check_body(&self, func: &Function, body: &Block, out: &mut Vec<Diagnostic>) {
        let name = self.defs.canonical_string(func.def);
        self.walk(body, &mut |e| {
            let ExprKind::Call {
                callee,
                builtin,
                dispatch,
                ..
            } = &e.kind
            else {
                return;
            };
            // A builtin operator *is* a machine instruction — there is no
            // function to look up and nothing to evaluate at run time. This is
            // exactly what the `builtin` tag is for: without it the pass would
            // have to recognize `core.Add.add` by name and would be wrong the
            // moment a target added an operator.
            if builtin.is_some() {
                return;
            }
            match dispatch {
                // A vtable call picks its target at run time, so nothing about
                // it can be settled now.
                Dispatch::Virtual { .. } => {
                    self.report(
                        e.id,
                        format!(
                            "`{name}` is `#const`, but this call dispatches through a trait \
                             object"
                        ),
                        "a `dyn` call picks its target at run time; `#const` needs a callee \
                         known now",
                        out,
                    );
                }
                // The impl is chosen by monomorphization, which has not run.
                // Deferring is the honest answer: rejecting here would rule out
                // every generic `#const` function, and accepting silently would
                // be a claim. Monomorphization re-checks once the callee is
                // concrete.
                Dispatch::Generic { .. } => {}
                Dispatch::Static => {
                    let Some(target) = self.callee_def(callee) else {
                        return;
                    };
                    if self.is_const_fn(target) {
                        return;
                    }
                    let callee_name = self.defs.canonical_string(target);
                    self.report(
                        e.id,
                        format!("`{name}` is `#const`, but calls `{callee_name}`, which is not"),
                        "mark the callee `#const`, or drop `#const` from the caller (§5.1)",
                        out,
                    );
                }
            }
        });
    }

    // ===< 2. A constant expression >===

    /// Check one expression against the constant-expression rule, reporting the
    /// **innermost** construct that breaks it.
    ///
    /// One diagnostic per default: a default built out of a runtime call inside
    /// a composite literal is one mistake, and naming the call is more use than
    /// naming the literal that contains it.
    fn check_const_expr(&self, e: &Expr, out: &mut Vec<Diagnostic>) {
        if let Some((at, why)) = self.non_const(e) {
            let mut d = Diagnostic::error(format!(
                "a default argument must be a constant expression, but {why}"
            ));
            if let Some(span) = self.meta.span(at) {
                d = d.with_primary(span, "not a constant expression");
            }
            out.push(d.with_note(
                "a default may be a literal, a `const` item or `const` generic parameter, a \
                 composite literal of constants, a `$cast` of one, a location directive, or a \
                 call to a `#const` function (§5.2)",
            ));
        }
    }

    /// The innermost part of `e` that is not a constant expression, and why.
    fn non_const(&self, e: &Expr) -> Option<(IrId, String)> {
        // A default that reads a parameter of the function being declared is
        // already rejected by §5.2's own rule, with a far better message
        // ("evaluated at the call site, where no parameter of this function
        // exists yet"). Saying "not a constant expression" on top of it — or,
        // worse, about the `.field` wrapped around it — would make one mistake
        // read as two.
        if self.rooted_in_param(e) {
            return None;
        }
        match &e.kind {
            // Compile-time known outright.
            ExprKind::Lit(_) | ExprKind::ConstParam(_) | ExprKind::Error => None,

            // A path to an item. `::` and `const` items have a compile-time
            // value by definition; a function used as a value is a symbol, which
            // is equally fixed. Anything else — a `#static let`, say — is not.
            ExprKind::Global(def) => {
                let d = self.defs.get(self.defs.resolve_alias(*def));
                match d.kind {
                    DefKind::Const | DefKind::Func | DefKind::ConstParam => None,
                    _ if !d.mutable => None,
                    _ => Some((e.id, format!("`{}` is not a constant", d.name))),
                }
            }

            // Any other local is a run-time binding.
            ExprKind::Local(def) => Some((
                e.id,
                format!("`{}` is a run-time binding", self.defs.get(*def).name),
            )),

            // Aggregates: const exactly when every element is.
            ExprKind::Tuple { elems } => elems.iter().find_map(|x| self.non_const(x)),
            ExprKind::Construct { fields, .. } => {
                fields.iter().find_map(|(_, x)| self.non_const(x))
            }
            ExprKind::Variant { args, .. } => args.iter().find_map(|x| self.non_const(x)),

            // Primitive arithmetic on constants is itself constant.
            ExprKind::Binary { lhs, rhs, .. } => {
                self.non_const(lhs).or_else(|| self.non_const(rhs))
            }
            ExprKind::Unary { operand, .. } => self.non_const(operand),

            // `$cast`, `$size_of`, the location directives: intrinsics are
            // compile-time by construction, so what matters is their arguments.
            ExprKind::Intrinsic { args, .. } => args.iter().find_map(|x| self.non_const(x)),

            ExprKind::Call {
                callee,
                args,
                builtin,
                dispatch,
            } => {
                if let Some(bad) = args.iter().find_map(|x| self.non_const(x)) {
                    return Some(bad);
                }
                // A builtin operator is a machine instruction on constants.
                if builtin.is_some() {
                    return self.non_const(callee);
                }
                match dispatch {
                    Dispatch::Static => match self.callee_def(callee) {
                        Some(target) if self.is_const_fn(target) => None,
                        Some(target) => Some((
                            e.id,
                            format!("`{}` is not `#const`", self.defs.canonical_string(target)),
                        )),
                        None => Some((e.id, "the callee is not a known function".to_string())),
                    },
                    _ => Some((e.id, "the callee is not known until run time".to_string())),
                }
            }

            // Everything else needs storage, control flow, or a value that only
            // exists once the program is running.
            ExprKind::Block(_) => Some((e.id, "a block is not a constant expression".into())),
            ExprKind::If { .. } => Some((e.id, "an `if` is not a constant expression".into())),
            ExprKind::Match { .. } => Some((e.id, "a `match` is not a constant expression".into())),
            ExprKind::Loop { .. } => Some((e.id, "a loop is not a constant expression".into())),
            ExprKind::Ref { place, .. } => self
                .non_const(place)
                .or(Some((e.id, "taking an address is not constant".into()))),
            ExprKind::Deref { base } => self.non_const(base).or(Some((
                e.id,
                "reading through a pointer is not constant".into(),
            ))),
            // A projection is constant exactly when what it projects out of is:
            // `SOME_CONST.field` is known now, `read_config().field` is not.
            // Reporting the projection rather than recursing would name the
            // wrong thing in the second case.
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.non_const(base)
            }
            ExprKind::Index { base, index } => {
                self.non_const(base).or_else(|| self.non_const(index))
            }
            ExprKind::DynCast { .. } => {
                Some((e.id, "building a trait object is not constant".into()))
            }
        }
    }

    // ===< Helpers >===

    /// Whether `e` bottoms out in a parameter of the function being declared,
    /// through any chain of projections.
    fn rooted_in_param(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Local(def) => self.defs.get(*def).kind == DefKind::Param,
            ExprKind::Field { base, .. }
            | ExprKind::TupleIndex { base, .. }
            | ExprKind::Index { base, .. }
            | ExprKind::Deref { base } => self.rooted_in_param(base),
            ExprKind::Ref { place, .. } => self.rooted_in_param(place),
            _ => false,
        }
    }

    /// The function a static callee names, if it names one.
    fn callee_def(&self, callee: &Expr) -> Option<DefId> {
        match &callee.kind {
            ExprKind::Global(def) => Some(self.defs.resolve_alias(*def)),
            _ => None,
        }
    }

    /// Whether `def` is a function marked `#const`.
    ///
    /// Read off the *def*, not the lowered function: a callee may be a bodyless
    /// declaration, and one in another file is not the function being walked.
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

    fn report(&self, at: IrId, message: String, note: &str, out: &mut Vec<Diagnostic>) {
        let mut d = Diagnostic::error(message);
        if let Some(span) = self.meta.span(at) {
            d = d.with_primary(span, "not allowed at compile time");
        }
        out.push(d.with_note(note));
    }

    /// Visit every expression in a body.
    fn walk(&self, b: &Block, f: &mut impl FnMut(&Expr)) {
        super::block_children(b, &mut |e| self.visit(e, f));
    }

    fn visit(&self, e: &Expr, f: &mut impl FnMut(&Expr)) {
        f(e);
        super::children_of(e, &mut |c| self.visit(c, f));
    }
}
