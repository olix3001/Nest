//! A compact textual dump of the IR, for `--emit=ir`, tests, and eyeballing a
//! lowering. Not a source-faithful printer: it shows structure and types.

use std::fmt::Write;

use crate::parser::ast::Lit;

use super::{
    Arm, Block, Dispatch, Expr, ExprKind, Function, IrId, Member, Meta, Pattern, PatternKind,
    Program, Recv, Stmt, StmtKind, TypeDef, TypeDefKind, Variant,
};
use crate::sema::def::{DefTable, Directive, DirectiveArg};

/// Render a whole [`Program`].
///
/// `meta` is not optional decoration: since types are per-node metadata rather
/// than fields, it is where every type in the output comes from.
pub fn program_to_string(defs: &DefTable, meta: &Meta, program: &Program) -> String {
    let mut p = Printer {
        defs,
        meta,
        out: String::new(),
        indent: 0,
    };
    // Types first: a function's signature reads better once the reader has seen
    // what its types are made of.
    for t in &program.types {
        p.type_def(t);
    }
    for f in &program.funcs {
        p.function(f);
    }
    p.out
}

struct Printer<'a> {
    defs: &'a DefTable,
    meta: &'a Meta,
    out: String,
    indent: usize,
}

impl Printer<'_> {
    /// A node's type, rendered. Every type in a dump comes through here.
    fn ty(&self, id: IrId) -> String {
        self.meta
            .with_ty(id, |t| t.display(self.defs))
            .unwrap_or_else(|| "<untyped>".to_string())
    }

    /// A function's declared return type, read out of the signature its node is
    /// typed with.
    fn ret_of(&self, f: &Function) -> String {
        self.meta
            .with_ty(f.id, |t| match t {
                crate::sema::ty::Ty::Func { ret, .. } => ret.display(self.defs),
                other => other.display(self.defs),
            })
            .unwrap_or_else(|| "<untyped>".to_string())
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn type_def(&mut self, t: &TypeDef) {
        let mut tags = String::new();
        for d in &self.meta.directives(t.id) {
            let _ = write!(tags, " {}", directive_str(d));
        }
        match &t.kind {
            TypeDefKind::Struct { members } => {
                if members.is_empty() {
                    self.line(&format!("struct {}{tags} {{}}", t.name));
                    return;
                }
                self.line(&format!("struct {}{tags} {{", t.name));
                self.indent += 1;
                for m in members {
                    self.member(m);
                }
                self.indent -= 1;
                self.line("}");
            }
            TypeDefKind::Enum { variants } => {
                self.line(&format!("enum {}{tags} {{", t.name));
                self.indent += 1;
                for v in variants {
                    self.variant(v);
                }
                self.indent -= 1;
                self.line("}");
            }
            TypeDefKind::Distinct { repr } => {
                let ty = self.ty(repr.id);
                self.line(&format!("distinct {}{tags} = {ty}", t.name));
            }
            TypeDefKind::Trait {
                methods,
                assoc_consts,
            } => {
                self.line(&format!("trait {}{tags} {{", t.name));
                self.indent += 1;
                // Slot order is the declaration order, and a dump is where a
                // reader checks it, so the index is printed with each method.
                for (i, m) in methods.iter().enumerate() {
                    let ty = self.ty(m.id);
                    let mut tags = String::new();
                    // In a trait, the *absence* of a receiver is worth saying:
                    // it is what makes the method un-callable through a trait
                    // object, so a reader checking object safety needs to see it.
                    match m.recv {
                        Recv::None => tags.push_str(" #no-self"),
                        other => tags.push_str(recv_str(other)),
                    }
                    if m.generic {
                        tags.push_str(" #generic");
                    }
                    if m.has_default {
                        tags.push_str(" #default");
                    }
                    self.line(&format!("[{i}] {}: {ty}{tags}", m.name));
                }
                for c in assoc_consts {
                    self.line(&format!("const {c}"));
                }
                self.indent -= 1;
                self.line("}");
            }
        }
    }

    fn member(&mut self, m: &Member) {
        let ty = self.ty(m.id);
        self.line(&format!("{}: {ty}", m.name));
    }

    fn variant(&mut self, v: &Variant) {
        if v.members.is_empty() {
            self.line(&format!(".{}", v.name));
            return;
        }
        if v.tuple {
            let tys = v
                .members
                .iter()
                .map(|m| self.ty(m.id))
                .collect::<Vec<_>>()
                .join(", ");
            self.line(&format!(".{}({tys})", v.name));
            return;
        }
        self.line(&format!(".{} {{", v.name));
        self.indent += 1;
        for m in &v.members {
            self.member(m);
        }
        self.indent -= 1;
        self.line("}");
    }

    fn function(&mut self, f: &Function) {
        let params = f
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, self.ty(p.id)))
            .collect::<Vec<_>>()
            .join(", ");
        // The receiver / mutation tags print only when they say something: an
        // unannotated `func` is a non-method that cannot write through anything.
        let mut tags = String::new();
        for d in &self.meta.directives(f.id) {
            let _ = write!(tags, " {}", directive_str(d));
        }
        // The receiver prints the way it is written in source. A dump should not
        // need a glossary: `#self(*mut)` says "this is a method whose receiver is
        // `self: *mut Self`", which is the whole content of `Recv`.
        tags.push_str(recv_str(f.recv));
        if f.mutating {
            tags.push_str(" #mutating");
        }
        let abi = match &f.extern_abi {
            Some(a) => format!("extern(\"{a}\") "),
            None => String::new(),
        };
        let header = format!(
            "{abi}func {}({}) -> {}{tags}",
            f.name,
            params,
            self.ret_of(f)
        );
        // A declaration has no body to open a brace for — `extern("c") func
        // strlen(...) -> usize` is the whole of it.
        let Some(body) = &f.body else {
            self.line(&header);
            return;
        };
        self.line(&format!("{header} {{"));
        self.indent += 1;
        self.block(body);
        self.indent -= 1;
        self.line("}");
    }

    fn block(&mut self, b: &Block) {
        // The block's defers print first, before its statements: they are a
        // property of the scope, not a step in its sequence.
        for d in &b.defers {
            let e = self.expr(d);
            self.line(&format!("defer {e}"));
        }
        for s in &b.stmts {
            self.stmt(s);
        }
        if let Some(t) = &b.tail {
            let e = self.expr(t);
            self.line(&format!("tail {e}"));
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { pattern, init } => {
                let ty = self.ty(init.id);
                let e = self.expr(init);
                self.line(&format!(
                    "let {}: {ty} = {e}",
                    pattern_str(self.defs, pattern),
                ));
            }
            StmtKind::Assign { place, value } => {
                let p = self.expr(place);
                let v = self.expr(value);
                self.line(&format!("{p} = {v}"));
            }
            StmtKind::Expr(e) => {
                let e = self.expr(e);
                self.line(&e);
            }
            StmtKind::Return(e) => {
                let e = e.as_ref().map(|e| self.expr(e)).unwrap_or_default();
                self.line(&format!("return {e}"));
            }
            StmtKind::Break(e) => {
                let e = e.as_ref().map(|e| self.expr(e)).unwrap_or_default();
                self.line(&format!("break {e}"));
            }
            StmtKind::Continue => self.line("continue"),
        }
    }

    /// Render an expression to a single-line string. Blocks/if/match/loop print
    /// their bodies inline via nested lines instead, so those emit through
    /// `self.line` and return a short header.
    fn expr(&mut self, e: &Expr) -> String {
        let ty = self.ty(e.id);
        match &e.kind {
            ExprKind::Lit(l) => format!("{}: {ty}", lit_str(l)),
            ExprKind::Local(d) => format!("{}: {ty}", self.defs.get(*d).name),
            ExprKind::Global(d) => format!("{}: {ty}", self.defs.canonical_string(*d)),
            ExprKind::ConstParam(d) => format!("const {}: {ty}", self.defs.get(*d).name),
            ExprKind::Call {
                callee,
                args,
                builtin,
                dispatch,
                ..
            } => {
                let c = self.expr(callee);
                let a = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                // A builtin primitive operator prints its tag so the O(1)
                // codegen marker is visible in the dump; a non-static dispatch
                // prints the trait whose slot (or impl) the call still needs.
                let mut prefix = String::new();
                if let Some(op) = builtin {
                    let _ = write!(prefix, "#builtin({op:?}) ");
                }
                match dispatch {
                    Dispatch::Static => {}
                    Dispatch::Virtual { trait_def, .. } => {
                        let _ = write!(
                            prefix,
                            "#virtual({}) ",
                            self.defs.canonical_string(*trait_def)
                        );
                    }
                    Dispatch::Generic {
                        trait_def, self_ty, ..
                    } => {
                        let _ = write!(
                            prefix,
                            "#generic({}, {}) ",
                            self.defs.canonical_string(*trait_def),
                            self_ty.display(self.defs)
                        );
                    }
                }
                format!("{prefix}({c})({a}): {ty}")
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                let l = self.expr(lhs);
                let r = self.expr(rhs);
                format!("({l} {op:?} {r}): {ty}")
            }
            ExprKind::Unary { op, operand, .. } => {
                let o = self.expr(operand);
                format!("({op:?} {o}): {ty}")
            }
            ExprKind::Ref { mutable, place, .. } => {
                let p = self.expr(place);
                format!("(&{}{p}): {ty}", if *mutable { "mut " } else { "" })
            }
            ExprKind::Deref { base, .. } => {
                let b = self.expr(base);
                format!("({b}.*): {ty}")
            }
            ExprKind::Field { base, name, .. } => {
                let b = self.expr(base);
                format!("({b}.{name}): {ty}")
            }
            ExprKind::TupleIndex { base, index, .. } => {
                let b = self.expr(base);
                format!("({b}.{index}): {ty}")
            }
            ExprKind::Index { base, index, .. } => {
                let b = self.expr(base);
                let i = self.expr(index);
                format!("({b}[{i}]): {ty}")
            }
            ExprKind::Tuple { elems, .. } => {
                let es = elems
                    .iter()
                    .map(|e| self.expr(e))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({es}): {ty}")
            }
            ExprKind::Construct { def, fields, .. } => {
                let fs = fields
                    .iter()
                    .map(|(n, e)| format!("{n}: {}", self.expr(e)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} {{ {fs} }}: {ty}", self.defs.canonical_string(*def))
            }
            ExprKind::Variant { name, args, .. } if args.is_empty() => format!(".{name}: {ty}"),
            ExprKind::Variant { name, args, .. } => {
                let a = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(".{name}({a}): {ty}")
            }
            ExprKind::DynCast {
                value, concrete, ..
            } => {
                let v = self.expr(value);
                format!("({v} as {ty} from {})", concrete.display(self.defs))
            }
            ExprKind::Intrinsic { name, args, .. } => {
                let a = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("${name}({a}): {ty}")
            }
            ExprKind::Block(b) => {
                self.line(&format!("block: {ty} {{"));
                self.indent += 1;
                self.block(b);
                self.indent -= 1;
                self.line("}");
                format!("<block: {ty}>")
            }
            ExprKind::If {
                cond, then, els, ..
            } => {
                let c = self.expr(cond);
                self.line(&format!("if {c} {{"));
                self.indent += 1;
                self.block(then);
                self.indent -= 1;
                if let Some(e) = els {
                    self.line("} else {");
                    self.indent += 1;
                    self.block(e);
                    self.indent -= 1;
                }
                self.line("}");
                format!("<if: {ty}>")
            }
            ExprKind::Match {
                scrutinee, arms, ..
            } => {
                let s = self.expr(scrutinee);
                self.line(&format!("match {s} {{"));
                self.indent += 1;
                for a in arms {
                    self.arm(a);
                }
                self.indent -= 1;
                self.line("}");
                format!("<match: {ty}>")
            }
            ExprKind::Loop { body, .. } => {
                self.line("loop {");
                self.indent += 1;
                self.block(body);
                self.indent -= 1;
                self.line("}");
                format!("<loop: {ty}>")
            }
            ExprKind::Error => format!("<error: {ty}>"),
        }
    }

    fn arm(&mut self, a: &Arm) {
        let mut header = format!("{} =>", pattern_str(self.defs, &a.pattern));
        if let Some(g) = &a.guard {
            let g = self.expr(g);
            let _ = write!(header, " if {g}");
        }
        let body = self.expr(&a.body);
        self.line(&format!("{header} {body}"));
    }
}

fn pattern_str(defs: &DefTable, p: &Pattern) -> String {
    match &p.kind {
        PatternKind::Wildcard => "_".into(),
        PatternKind::Binding { name, .. } => name.to_string(),
        PatternKind::Lit(l) => lit_str(l),
        PatternKind::Variant { name, sub } => {
            if sub.is_empty() {
                format!(".{name}")
            } else {
                let s = sub
                    .iter()
                    .map(|p| pattern_str(defs, p))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(".{name}({s})")
            }
        }
        PatternKind::Tuple(ps) => {
            let s = ps
                .iter()
                .map(|p| pattern_str(defs, p))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({s})")
        }
        PatternKind::Or(ps) => ps
            .iter()
            .map(|p| pattern_str(defs, p))
            .collect::<Vec<_>>()
            .join(" | "),
        PatternKind::Struct { def, fields, rest } => {
            let mut parts: Vec<String> = fields
                .iter()
                .map(|(n, p)| format!("{n}: {}", pattern_str(defs, p)))
                .collect();
            if *rest {
                parts.push("..".into());
            }
            let head = def.map_or_else(
                || ".".to_string(),
                |d| format!("{} ", defs.canonical_string(d)),
            );
            format!("{head}{{ {} }}", parts.join(", "))
        }
        PatternKind::TupleStruct { def, elems, rest } => {
            let mut parts: Vec<String> = elems.iter().map(|p| pattern_str(defs, p)).collect();
            if *rest {
                parts.push("..".into());
            }
            let head = def.map_or_else(String::new, |d| defs.canonical_string(d));
            format!("{head}({})", parts.join(", "))
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            let mut parts: Vec<String> = prefix.iter().map(|p| pattern_str(defs, p)).collect();
            if let Some(binding) = rest {
                parts.push(match binding {
                    Some(b) => format!(".. {}", b.name),
                    None => "..".into(),
                });
            }
            parts.extend(suffix.iter().map(|p| pattern_str(defs, p)));
            format!("[{}]", parts.join(", "))
        }
        PatternKind::Range {
            start,
            end,
            inclusive,
        } => {
            let op = if *inclusive { "..=" } else { "..<" };
            let s = start.as_ref().map(lit_str).unwrap_or_default();
            let e = end.as_ref().map(lit_str).unwrap_or_default();
            format!("{s}{op}{e}")
        }
        PatternKind::At { binding, pattern } => {
            format!("{} @ {}", binding.name, pattern_str(defs, pattern))
        }
        PatternKind::Deref(p) => format!("&{}", pattern_str(defs, p)),
    }
}

/// How a function takes its receiver, spelled as the parameter would be. Empty
/// for a function that is not a method.
fn recv_str(recv: Recv) -> &'static str {
    match recv {
        Recv::None => "",
        Recv::Value => " #self",
        Recv::Ptr => " #self(*)",
        Recv::MutPtr => " #self(*mut)",
    }
}

/// A directive as `#name(args)`, the way it was written.
pub fn directive_str(d: &Directive) -> String {
    if d.args.is_empty() {
        return format!("#{}", d.name);
    }
    let args = d
        .args
        .iter()
        .map(|a| match a {
            DirectiveArg::Int(n) => n.to_string(),
            DirectiveArg::Str(s) => format!("{s:?}"),
            DirectiveArg::Name(n) => n.to_string(),
            DirectiveArg::Other => "?".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("#{}({args})", d.name)
}

fn lit_str(l: &Lit) -> String {
    match l {
        Lit::Int(n) => n.to_string(),
        Lit::Float(x) => x.to_string(),
        Lit::Str(s) => format!("{s:?}"),
        Lit::Char(c) => format!("{c:?}"),
        Lit::Bool(b) => b.to_string(),
    }
}
