//! Desugaring — the stage that lowers surface control-flow sugar to the core
//! forms the rest of the compiler handles uniformly, wiring each to its `#lang`
//! item (§6.13, §8.3).
//!
//! Implemented lowerings:
//!
//! - `for pat in it { body }` → a `loop` over `IntoIterator.into_iter` /
//!   `Iterator.next`, using an `if match` to bind each element and `break` when
//!   the iterator is exhausted.
//! - `base.?` (propagate) → a `match` on `Try.branch(base)` that yields the
//!   success value and, on failure, `return`s the residual rebuilt as the
//!   enclosing function's type via a static `FromResidual.from_residual` call.
//! - `base.!` (abort) → `Try.unwrap(base)`.
//! - `f"...{e}..."` → a `core` format buffer, a `Display.display` call per
//!   piece, and the bytes that came out.
//!
//! Operators (`+`, `<`, `a[i]`, `a += b`, …) are **not** touched here: they stay
//! as their parsed [`Binary`](NodeKind::Binary) / [`Index`](NodeKind::Index) /
//! [`Assign`](NodeKind::Assign) nodes and are lowered to trait calls in a later
//! pass, once type resolution can pick the right `impl`.
//!
//! Rewrites happen **in place** (a node keeps its [`NodeId`], so its parent needs
//! no fix-up); helper nodes are appended to the arena. Synthetic bindings
//! (`__it`, `__try`, …) get fresh [`DefKind::Local`] defs and their uses are
//! resolved on the spot; the lang-item method names (`next`, `branch`) are left
//! for the type checker, exactly like any hand-written method call.

use num_bigint::BigInt;

use crate::common::diagnostic::Diagnostic;
use crate::common::source::{FileId, FileSpan};
use crate::common::span::Span;
use crate::common::symbol::Symbol;
use crate::parser::ast::{
    AssignOp, Ast, BinOp, CompositeBody, Lit, NodeId, NodeKind, TryKind, UnOp, VariantArgs,
    VariantPatArgs,
};
use crate::parser::fmt::{FormatCall, FormatSpec, SpecKind};

use super::def::{DefId, DefKind, DefTable, LangItems, Visibility};
use super::{DefMeta, Resolution, SpreadBase};

/// Desugar every `for` / `.?` / `.!` in `file`.
pub fn desugar_file(
    ast: &mut Ast,
    defs: &mut DefTable,
    lang: &LangItems,
    diags: &mut Vec<Diagnostic>,
    file: FileId,
    file_ns: DefId,
) {
    let mut d = Desugar {
        ast,
        defs,
        lang,
        diags,
        file,
        file_ns,
        counter: 0,
    };
    if let Some(root) = d.ast.root() {
        d.walk(root);
    }
}

struct Desugar<'a> {
    ast: &'a mut Ast,
    defs: &'a mut DefTable,
    lang: &'a LangItems,
    diags: &'a mut Vec<Diagnostic>,
    file: FileId,
    file_ns: DefId,
    counter: usize,
}

impl Desugar<'_> {
    /// Post-order walk: lower children before the node, so a `for` whose body
    /// contains a `.?` is fully lowered.
    fn walk(&mut self, id: NodeId) {
        let kind = self.ast.node(id).kind.clone();
        for child in kind.children() {
            self.walk(child);
        }
        match kind {
            NodeKind::For {
                pattern,
                iter,
                body,
            } => self.lower_for(id, pattern, iter, body),
            NodeKind::Try { base, kind } => self.lower_try(id, base, kind),
            NodeKind::InterpolatedStr { parts } => self.lower_interpolation(id, parts),
            NodeKind::CStr { bytes } => self.lower_cstr(id, bytes),
            // `a op= b` → `a = a op b`, so the resulting `op` lowers through the
            // operator trait like any other binary. `a` is shared between the
            // place and the operator's left operand (both already resolved).
            NodeKind::Assign { op, place, value } if op != AssignOp::Assign => {
                self.lower_compound_assign(id, op, place, value)
            }
            // A `defer`'s operands are evaluated **where it is written** (§8.4),
            // which is a fact about the block holding it rather than about the
            // `defer` node: the bindings have to land in front of it, in the
            // same scope, or they would be a scope of their own that ends
            // immediately.
            NodeKind::Block { stmts, tail } => self.capture_defers(id, stmts, tail),
            // A spread already bound to a temporary is this pass's own output;
            // only a written one needs binding.
            NodeKind::CompositeLit {
                ty,
                body: CompositeBody::Named { fields, spread },
            } if spread.is_some_and(|s| self.ast.meta::<SpreadBase>(s).is_none()) => {
                self.lower_spread(id, ty, fields, spread.expect("checked"))
            }
            _ => {}
        }
    }

    // ===< `..` spread >===

    /// `P { x: 5, ..rest }` → `{ __spread1 :: rest  P { x: 5, ..__spread1 } }`.
    ///
    /// Only the **temporary** is introduced here; the spread itself stays on the
    /// literal and is expanded into field reads by lowering. The split is what
    /// lets `.{ x: 5, ..rest }` work too: the fields a spread fills come from the
    /// literal's type, and desugaring runs before there is one, while lowering
    /// runs after inference has settled it.
    ///
    /// Binding the temporary *here* rather than there is deliberate: a new local
    /// needs a [`DefId`], and this is the pass that allocates them. It also gives
    /// the "evaluated once" rule a place a reader can see it —
    /// `P { x: 5, ..Default.default() }` calls `default` one time, not one time
    /// per field it fills, because the call is bound to a name before any field
    /// reads it.
    fn lower_spread(
        &mut self,
        id: NodeId,
        ty: Option<NodeId>,
        fields: Vec<NodeId>,
        spread: NodeId,
    ) {
        let span = self.ast.node(id).span;
        let name = self.fresh("spread");
        let (pat, local) = self.binding_pat(span, &name, false);
        let bind = self.alloc(
            span,
            NodeKind::ConstBind {
                pattern: pat,
                rhs: spread,
            },
        );
        let base = self.local_ref(span, &name, local);
        self.ast.set_meta(base, SpreadBase);
        let lit = self.alloc(
            span,
            NodeKind::CompositeLit {
                ty,
                body: CompositeBody::Named {
                    fields,
                    spread: Some(base),
                },
            },
        );
        self.replace(
            id,
            NodeKind::Block {
                stmts: vec![bind],
                tail: Some(lit),
            },
        );
    }

    // ===< `defer` argument capture >===

    /// Bind each deferred call's arguments **at the `defer`**, so the body that
    /// runs on the way out sees the values the program had when it registered
    /// it (§8.4: "capturing the current values it references").
    ///
    /// ```text
    /// defer log(n)        →     __defer1 :: n
    ///                           defer log(__defer1)
    /// ```
    ///
    /// Without it the body is simply re-evaluated at every exit, so a `defer`
    /// reads whatever its operands hold *then* — `n` after the loop rather than
    /// `n` when the line ran, which is the opposite of what the construct is
    /// for.
    ///
    /// **The receiver is not captured**, and that is deliberate rather than
    /// missing: this pass runs before inference, so it cannot see whether
    /// `x.close()` takes `self` by value or as a `*mut Self`, and binding a
    /// mutating method's receiver to a copy would silently defer the mutation
    /// to a temporary. Reading the place at exit is what a captured *pointer*
    /// would have done anyway, and it differs only for a receiver that is
    /// reassigned between the `defer` and the exit.
    fn capture_defers(&mut self, id: NodeId, stmts: Vec<NodeId>, tail: Option<NodeId>) {
        let mut out: Vec<NodeId> = Vec::with_capacity(stmts.len());
        let mut changed = false;
        for stmt in stmts {
            let NodeKind::Defer { body } = self.ast.node(stmt).kind else {
                out.push(stmt);
                continue;
            };
            let NodeKind::Call { callee, args } = self.ast.node(body).kind.clone() else {
                out.push(stmt);
                continue;
            };
            if args.is_empty() {
                out.push(stmt);
                continue;
            }
            let captured = args
                .into_iter()
                .map(|arg| {
                    let span = self.ast.node(arg).span;
                    let name = self.fresh("defer");
                    let (pat, local) = self.binding_pat(span, &name, false);
                    let bind = self.alloc(
                        span,
                        NodeKind::ConstBind {
                            pattern: pat,
                            rhs: arg,
                        },
                    );
                    out.push(bind);
                    self.local_ref(span, &name, local)
                })
                .collect();
            self.replace(
                body,
                NodeKind::Call {
                    callee,
                    args: captured,
                },
            );
            out.push(stmt);
            changed = true;
        }
        if changed {
            self.replace(id, NodeKind::Block { stmts: out, tail });
        }
    }

    // ===< compound assignment >===

    fn lower_compound_assign(&mut self, id: NodeId, op: AssignOp, place: NodeId, value: NodeId) {
        let Some(bin_op) = compound_binop(op) else {
            return;
        };
        let span = self.ast.node(id).span;
        let binary = self.alloc(
            span,
            NodeKind::Binary {
                op: bin_op,
                lhs: place,
                rhs: value,
            },
        );
        self.replace(
            id,
            NodeKind::Assign {
                op: AssignOp::Assign,
                place,
                value: binary,
            },
        );
    }

    // ===< for >===

    fn lower_for(&mut self, id: NodeId, pattern: NodeId, iter: NodeId, body: NodeId) {
        if self.lang.get("iterator").is_none() {
            self.report(id, "`for` requires the `#lang(\"iterator\")` item");
            return;
        }
        let span = self.ast.node(id).span;
        let it_name = self.fresh("it");

        // __it :: (iter).into_iter()
        let into = self.method_call(span, iter, "into_iter", vec![]);
        let (it_pat, it_local) = self.binding_pat(span, &it_name, true);
        let it_bind = self.alloc(
            span,
            NodeKind::ConstBind {
                pattern: it_pat,
                rhs: into,
            },
        );

        // if match .some(pat) := __it.next() then body else { break }
        let it_ref = self.local_ref(span, &it_name, it_local);
        let next = self.method_call(span, it_ref, "next", vec![]);
        let some_pat = self.alloc(
            span,
            NodeKind::VariantPat {
                name: Symbol::new("some"),
                args: VariantPatArgs::Tuple(vec![pattern]),
            },
        );
        let brk = self.alloc(span, NodeKind::Break { value: None });
        let els = self.block(span, vec![brk], None);
        let if_match = self.alloc(
            span,
            NodeKind::IfMatch {
                pattern: some_pat,
                value: next,
                then: body,
                els: Some(els),
            },
        );
        let loop_body = self.block(span, vec![], Some(if_match));
        let loop_node = self.alloc(span, NodeKind::Loop { body: loop_body });
        self.replace(
            id,
            NodeKind::Block {
                stmts: vec![it_bind],
                tail: Some(loop_node),
            },
        );
    }

    // ===< f"..." >===

    /// `f"a{x}b"` → a buffer, a `display` call per piece, and the bytes that
    /// came out (§1.5, §6.11):
    ///
    /// ```text
    /// {
    ///   let mut __fmt1 := format.start()
    ///   "a".display(&mut __fmt1)
    ///   x.display(&mut __fmt1)
    ///   "b".display(&mut __fmt1)
    ///   format.end(&mut __fmt1)
    /// }
    /// ```
    ///
    /// **A literal segment goes through the same call an embedded expression
    /// does.** The parser left them interleaved as ordinary `Lit::Str` nodes,
    /// `str` implements `Display` in `core`, and so there is one kind of call
    /// here instead of two — nothing in the compiler knows that some of these
    /// pieces were typed as text.
    ///
    /// Which is also why there is no `format` intrinsic any more. What a value
    /// looks like is the most library-ish question there is; a compiler that
    /// answered it would have left a user's own type with nowhere to.
    fn lower_interpolation(&mut self, id: NodeId, parts: Vec<NodeId>) {
        let (Some(start), Some(end)) = (
            self.lang
                .get("format_start")
                .map(|d| self.defs.resolve_alias(d)),
            self.lang
                .get("format_end")
                .map(|d| self.defs.resolve_alias(d)),
        ) else {
            self.report(
                id,
                "`f\"...\"` requires the `#lang(\"format_start\")` and `#lang(\"format_end\")` items",
            );
            return;
        };
        if self.lang.get("display").is_none() {
            self.report(id, "`f\"...\"` requires the `#lang(\"display\")` item");
            return;
        }
        let span = self.ast.node(id).span;
        let name = self.fresh("fmt");

        // let mut __fmt := format.start()
        let (buf_pat, buf_local) = self.binding_pat(span, &name, true);
        let make = self.static_call(span, start, vec![]);
        let bind = self.alloc(
            span,
            NodeKind::ConstBind {
                pattern: buf_pat,
                rhs: make,
            },
        );

        // One call per piece, at the piece's own span, so "no impl of `Display`"
        // points at the `{x}` that has none rather than at the whole string.
        let mut stmts = vec![bind];
        for piece in parts {
            let spec = self.ast.meta::<FormatSpec>(piece).unwrap_or_default();
            self.write_piece(&mut stmts, piece, spec, &name, buf_local);
        }

        let out = self.buf_ref(span, &name, buf_local);
        let finish = self.static_call(span, end, vec![out]);
        self.replace(
            id,
            NodeKind::Block {
                stmts,
                tail: Some(finish),
            },
        );
    }

    /// One piece of an `f"..."`, with whatever its specifier asks for written
    /// around it (§6.11).
    ///
    /// ```text
    /// {x}      →  x.display(&mut __fmt1)
    /// {x:?}    →  x.debug(&mut __fmt1)
    /// {x:.2}   →  x.with_precision(&mut __fmt1, 2)
    /// {x:>8}   →  __mark2 :: format.mark(&mut __fmt1)
    ///             x.display(&mut __fmt1)
    ///             format.pad(&mut __fmt1, __mark2, 8, ' ', 2, 0, false)
    /// ```
    ///
    /// The specifier is **spent here**: it decides which method the hole calls
    /// and which calls surround it, and nothing about it reaches the program.
    /// A hole without one costs exactly what it cost before there were any.
    fn write_piece(
        &mut self,
        stmts: &mut Vec<NodeId>,
        piece: NodeId,
        spec: FormatSpec,
        buf: &Symbol,
        buf_local: DefId,
    ) {
        let at = self.ast.node(piece).span;
        // The type character *is* the choice of method: the hole calls one name
        // or another, and no base and no flag is passed to anything. Two of
        // them are traits `core` tags, and the radices are inherent methods on
        // the integer families — a base is a fact about bits, and a type that
        // is not a number has no spelling in one.
        let method = match spec.kind {
            SpecKind::Display => "display",
            SpecKind::Debug => "debug",
            SpecKind::LowerHex => "lower_hex",
            SpecKind::UpperHex => "upper_hex",
            SpecKind::Binary => "binary",
            SpecKind::Octal => "octal",
        };
        if spec.kind == SpecKind::Debug && self.lang.get("debug").is_none() {
            self.report(piece, "`{...:?}` requires the `#lang(\"debug\")` item");
            return;
        }
        // A precision changes what the value writes rather than what is done to
        // it afterwards, so it is a **different call** and not another wrapper:
        // `{x:.3}` asks the value for three digits. It is the one specifier
        // that cannot be combined with a type character — a radix has no
        // fraction, and what `Debug` writes is the value's own shape.
        let method = match spec.precision {
            None => method,
            // `core` hands the number to C as an `i32`, where `-1` is the "none
            // was written" the desugaring uses for a hole with no precision. A
            // number that does not fit would wrap into one that does, so it is
            // refused here rather than silently becoming no precision at all.
            Some(digits) if digits > i32::MAX as u32 => {
                self.report(
                    piece,
                    format!("a precision of {digits} digits is more than one value can be written to"),
                );
                return;
            }
            Some(_) if spec.kind == SpecKind::Display => "with_precision",
            Some(_) => {
                self.report(
                    piece,
                    format!(
                        "a precision writes digits after the point, and `{{...:{}}}` has none",
                        spec.kind.letter()
                    ),
                );
                return;
            }
        };
        // `#` is the radix prefix and nothing else: Rust's `{x:#?}`, which
        // pretty-prints, is a second `Debug` and not a flag on this one.
        let prefix = match (spec.alternate, spec.kind.alternate_prefix()) {
            (false, _) => None,
            (true, Some(text)) => Some(text),
            (true, None) => {
                self.report(
                    piece,
                    "`#` writes a radix prefix, and this `{...}` has no radix",
                );
                return;
            }
        };

        // Where the value's own bytes begin. Only a specifier that writes
        // something around them needs to know.
        let mark = if spec.wraps() {
            let Some(def) = self.lang_def("format_mark") else {
                self.report(
                    piece,
                    "a padded `{...}` requires the `#lang(\"format_mark\")` item",
                );
                return;
            };
            let name = self.fresh("mark");
            let (pat, local) = self.binding_pat(at, &name, false);
            let out = self.buf_ref(at, buf, buf_local);
            let call = self.static_call(at, def, vec![out]);
            stmts.push(self.alloc(
                at,
                NodeKind::ConstBind {
                    pattern: pat,
                    rhs: call,
                },
            ));
            Some((name, local))
        } else {
            None
        };

        // The prefix goes through `Display` like any other text, and inside the
        // mark, so that a width counts it and zero-padding lands after it.
        if let Some(text) = prefix {
            let lit = self.alloc(at, NodeKind::Lit(Lit::Str(text.to_string())));
            let out = self.buf_ref(at, buf, buf_local);
            stmts.push(self.method_call(at, lit, "display", vec![out]));
        }

        let out = self.buf_ref(at, buf, buf_local);
        let mut args = vec![out];
        if let Some(digits) = spec.precision {
            args.push(self.int_lit(at, digits));
        }
        let call = self.method_call(at, piece, method, args);
        // What the call came from, so that a receiver without the method is told
        // about the specifier rather than about the name it produced.
        if let NodeKind::Call { callee, .. } = self.ast.node(call).kind {
            self.ast.set_meta(
                callee,
                FormatCall {
                    kind: spec.kind,
                    precision: spec.precision.is_some(),
                },
            );
        }
        stmts.push(call);

        let Some((mark, mark_local)) = mark else {
            return;
        };

        // `+` is applied to what was written rather than decided in front of
        // it: whether a value has a sign of its own is a run-time question, and
        // the bytes answer it.
        if spec.plus {
            let Some(def) = self.lang_def("format_plus") else {
                self.report(
                    piece,
                    "`{...:+}` requires the `#lang(\"format_plus\")` item",
                );
                return;
            };
            let out = self.buf_ref(at, buf, buf_local);
            let from = self.local_ref(at, &mark, mark_local);
            let call = self.static_call(at, def, vec![out, from]);
            stmts.push(call);
        }

        if let Some(width) = spec.width {
            let Some(def) = self.lang_def("format_pad") else {
                self.report(piece, "a width requires the `#lang(\"format_pad\")` item");
                return;
            };
            let out = self.buf_ref(at, buf, buf_local);
            let from = self.local_ref(at, &mark, mark_local);
            let width = self.int_lit(at, width);
            let fill = self.alloc(at, NodeKind::Lit(Lit::Char(spec.padding())));
            let align = self.int_lit(at, spec.alignment().code());
            // How much of what was written is the radix prefix: padding goes
            // after it, and the width counts it.
            let prefix = self.int_lit(at, prefix.map_or(0, |t| t.chars().count()) as u32);
            let after_sign = self.alloc(at, NodeKind::Lit(Lit::Bool(spec.zero)));
            let call = self.static_call(
                at,
                def,
                vec![out, from, width, fill, align, prefix, after_sign],
            );
            stmts.push(call);
        }
    }

    /// The definition a `#lang` tag names, with an alias followed through.
    fn lang_def(&self, tag: &str) -> Option<DefId> {
        self.lang.get(tag).map(|d| self.defs.resolve_alias(d))
    }

    fn int_lit(&mut self, span: Span, value: impl Into<BigInt>) -> NodeId {
        self.alloc(span, NodeKind::Lit(Lit::Int(value.into())))
    }

    // ===< c"..." >===

    /// `c"hi"` → `cstr_of("hi\0")`.
    ///
    /// The literal is already a `str` whose bytes end in a NUL (the parser put
    /// it there), so all that is left is the address of its first byte — which
    /// is a library question, not a compiler one, and is answered by whatever
    /// claims `#lang("cstr_of")`. A `c"..."` therefore costs exactly what a
    /// `"..."` costs: bytes in read-only data and nothing at run time.
    fn lower_cstr(&mut self, id: NodeId, bytes: NodeId) {
        let Some(of) = self.lang.get("cstr_of").map(|d| self.defs.resolve_alias(d)) else {
            self.report(id, "`c\"...\"` requires the `#lang(\"cstr_of\")` item");
            return;
        };
        let span = self.ast.node(id).span;
        let call = self.static_call(span, of, vec![bytes]);
        let kind = self.ast.node(call).kind.clone();
        self.replace(id, kind);
    }

    /// `&mut __fmt` — a fresh reference for each call, because a node is used
    /// once.
    fn buf_ref(&mut self, span: Span, name: &Symbol, def: DefId) -> NodeId {
        let base = self.local_ref(span, name, def);
        self.alloc(
            span,
            NodeKind::Unary {
                op: UnOp::RefMut,
                operand: base,
            },
        )
    }

    // ===< .? / .! >===

    fn lower_try(&mut self, id: NodeId, base: NodeId, kind: TryKind) {
        if self.lang.get("try").is_none() {
            self.report(id, "`.?` / `.!` require the `#lang(\"try\")` item");
            return;
        }
        match kind {
            TryKind::Abort => self.lower_try_abort(id, base),
            TryKind::Propagate => self.lower_try_propagate(id, base),
        }
    }

    /// `base.!` → `Try.unwrap(base)` (§8.3). The abort on failure lives in the
    /// `unwrap` body the selected impl provides, so this is a plain method call.
    fn lower_try_abort(&mut self, id: NodeId, base: NodeId) {
        let span = self.ast.node(id).span;
        let call = self.method_call(span, base, "unwrap", vec![]);
        let kind = self.ast.node(call).kind.clone();
        self.replace(id, kind);
    }

    /// `base.?` → branch on the value and either continue with its output or
    /// return the residual, rebuilt as the *enclosing function's* type:
    ///
    /// ```text
    /// {
    ///   __try :: base.branch()
    ///   __try.match {
    ///     .proceed(__v) => __v,
    ///     .stop(__r)    => { return FromResidual.from_residual(__r) },
    ///   }
    /// }
    /// ```
    ///
    /// `FromResidual.from_residual` is an ordinary **static trait call**: it has
    /// no receiver, so `Self` is whatever the context wants — here the enclosing
    /// function's return type, which the `return` supplies. That is what makes
    /// this work for *any* `Try` type, and what lets a residual cross error
    /// types when a conversion impl exists (§8.3). Nothing here names `Result`
    /// or `Option`.
    fn lower_try_propagate(&mut self, id: NodeId, base: NodeId) {
        let Some(rebuild) = self.trait_member("from_residual", "from_residual") else {
            self.report(
                id,
                "`.?` requires the `#lang(\"from_residual\")` item, with a `from_residual` member",
            );
            return;
        };
        let span = self.ast.node(id).span;
        let tmp = self.fresh("try");
        let v = self.fresh("v");
        let r = self.fresh("r");

        // __try :: Try.branch(base)
        let branch = self.method_call(span, base, "branch", vec![]);
        let (tmp_pat, tmp_local) = self.binding_pat(span, &tmp, false);
        let tmp_bind = self.alloc(
            span,
            NodeKind::ConstBind {
                pattern: tmp_pat,
                rhs: branch,
            },
        );

        // .proceed(__v) => __v
        let (v_pat, v_local) = self.binding_pat(span, &v, false);
        let ok_pat = self.variant_pat(span, "proceed", vec![v_pat]);
        let v_ref = self.local_ref(span, &v, v_local);
        let ok_arm = self.match_arm(span, ok_pat, v_ref);

        // .stop(__r) => { return $from_residual(__r) }
        let (r_pat, r_local) = self.binding_pat(span, &r, false);
        let stop_pat = self.variant_pat(span, "stop", vec![r_pat]);
        let r_ref = self.local_ref(span, &r, r_local);
        let rebuilt = self.static_call(span, rebuild, vec![r_ref]);
        let ret = self.alloc(
            span,
            NodeKind::Return {
                value: Some(rebuilt),
            },
        );
        let fail_block = self.block(span, vec![ret], None);
        let stop_arm = self.match_arm(span, stop_pat, fail_block);

        let tmp_ref = self.local_ref(span, &tmp, tmp_local);
        let match_expr = self.alloc(
            span,
            NodeKind::MatchExpr {
                scrutinee: tmp_ref,
                arms: vec![ok_arm, stop_arm],
            },
        );
        self.replace(
            id,
            NodeKind::Block {
                stmts: vec![tmp_bind],
                tail: Some(match_expr),
            },
        );
    }

    // ===< node builders >===

    fn alloc(&mut self, span: Span, kind: NodeKind) -> NodeId {
        self.ast.alloc(span, self.file, kind)
    }

    /// Overwrite a node's kind in place, keeping its id and span.
    fn replace(&mut self, id: NodeId, kind: NodeKind) {
        self.ast.node_mut(id).kind = kind;
    }

    fn block(&mut self, span: Span, stmts: Vec<NodeId>, tail: Option<NodeId>) -> NodeId {
        self.alloc(span, NodeKind::Block { stmts, tail })
    }

    fn method_call(&mut self, span: Span, recv: NodeId, name: &str, args: Vec<NodeId>) -> NodeId {
        let callee = self.alloc(
            span,
            NodeKind::FieldAccess {
                base: recv,
                name: Symbol::new(name),
            },
        );
        self.alloc(span, NodeKind::Call { callee, args })
    }

    fn match_arm(&mut self, span: Span, pattern: NodeId, body: NodeId) -> NodeId {
        self.alloc(
            span,
            NodeKind::MatchArm {
                pattern,
                guard: None,
                body,
            },
        )
    }

    fn variant_pat(&mut self, span: Span, name: &str, elems: Vec<NodeId>) -> NodeId {
        self.alloc(
            span,
            NodeKind::VariantPat {
                name: Symbol::new(name),
                args: VariantPatArgs::Tuple(elems),
            },
        )
    }

    fn variant_lit(&mut self, span: Span, name: &str, value: NodeId) -> NodeId {
        let arg = self.alloc(span, NodeKind::Arg { name: None, value });
        self.alloc(
            span,
            NodeKind::VariantLit {
                name: Symbol::new(name),
                args: VariantArgs::Tuple(vec![arg]),
            },
        )
    }

    /// A fresh `BindingPat` plus its `Local` def; returns `(pattern, def)`.
    /// A synthetic binding and the local def it introduces.
    ///
    /// `mutable` matters: a `for` loop's iterator is storage the loop writes
    /// through — `Iterator.next` takes `*mut self`, so the desugaring's own
    /// `next(&mut __it)` is a mutable borrow of it. Binding it immutably made
    /// the IR mutability check reject every `for` loop in the language, which is
    /// the check working: an immutable binding really cannot be lent that way.
    fn binding_pat(&mut self, span: Span, name: &Symbol, mutable: bool) -> (NodeId, DefId) {
        let pat = self.alloc(
            span,
            NodeKind::BindingPat {
                mutable,
                name: name.clone(),
            },
        );
        let def = self.defs.alloc(
            name.clone(),
            DefKind::Local,
            Visibility::Private,
            Some(self.file_ns),
            Some(self.file),
            Some(span),
            Some(pat),
            vec![name.clone()],
        );
        self.defs.get_mut(def).mutable = mutable;
        self.ast.set_meta(pat, DefMeta(def));
        (pat, def)
    }

    /// The member `name` of the trait carrying `#lang(tag)`.
    fn trait_member(&self, tag: &str, name: &str) -> Option<DefId> {
        let trait_def = self.defs.resolve_alias(self.lang.get(tag)?);
        let d = self.defs.get(trait_def);
        (d.kind == DefKind::Trait)
            .then(|| d.ns.members.get(&Symbol::new(name)).copied())
            .flatten()
    }

    /// A call to a trait member with **no receiver** — `Trait.member(args)`.
    ///
    /// `Self` is not any argument here; the type checker solves it from the
    /// context the call sits in and then selects the impl (see
    /// `infer::open_trait_self`). That is what lets `.?` name
    /// `FromResidual.from_residual` without knowing which type the enclosing
    /// function returns.
    fn static_call(&mut self, span: Span, member: DefId, args: Vec<NodeId>) -> NodeId {
        let name = self.defs.get(member).name.clone();
        let callee = self.alloc(
            span,
            NodeKind::Path {
                segments: vec![name],
            },
        );
        self.ast.set_meta(callee, Resolution::Def(member));
        self.alloc(span, NodeKind::Call { callee, args })
    }

    /// A `Path` referencing a synthetic local, pre-resolved to `def`.
    fn local_ref(&mut self, span: Span, name: &Symbol, def: DefId) -> NodeId {
        let node = self.alloc(
            span,
            NodeKind::Path {
                segments: vec![name.clone()],
            },
        );
        self.ast.set_meta(node, Resolution::Def(def));
        node
    }

    fn fresh(&mut self, tag: &str) -> Symbol {
        self.counter += 1;
        Symbol::new(&format!("__{tag}{}", self.counter))
    }

    fn report(&mut self, node: NodeId, message: impl Into<String>) {
        let span = self.ast.node(node).span;
        self.diags
            .push(Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""));
    }
}

/// The binary operator a compound assignment (`+=`, `*=`, …) expands to.
fn compound_binop(op: AssignOp) -> Option<BinOp> {
    match op {
        AssignOp::Add => Some(BinOp::Add),
        AssignOp::Sub => Some(BinOp::Sub),
        AssignOp::Mul => Some(BinOp::Mul),
        AssignOp::Div => Some(BinOp::Div),
        AssignOp::Rem => Some(BinOp::Rem),
        AssignOp::Assign => None,
    }
}
