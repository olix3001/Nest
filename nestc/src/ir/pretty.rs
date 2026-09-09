//! A compact textual dump of the IR, for `--emit=ir`, tests, and eyeballing a
//! lowering. Not a source-faithful printer: it shows structure and types.

use std::fmt::Write;

use crate::parser::ast::Lit;

use super::{Arm, Block, Expr, Function, Pattern, Program, Stmt};
use crate::sema::def::DefTable;

/// Render a whole [`Program`].
pub fn program_to_string(defs: &DefTable, program: &Program) -> String {
    let mut p = Printer {
        defs,
        out: String::new(),
        indent: 0,
    };
    for f in &program.funcs {
        p.function(f);
    }
    p.out
}

struct Printer<'a> {
    defs: &'a DefTable,
    out: String,
    indent: usize,
}

impl Printer<'_> {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn function(&mut self, f: &Function) {
        let params = f
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, p.ty.display(self.defs)))
            .collect::<Vec<_>>()
            .join(", ");
        self.line(&format!(
            "func {}({}) -> {} {{",
            f.name,
            params,
            f.ret.display(self.defs)
        ));
        self.indent += 1;
        self.block(&f.body);
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
        match s {
            Stmt::Let { name, ty, init, .. } => {
                let e = self.expr(init);
                self.line(&format!("let {name}: {} = {e}", ty.display(self.defs)));
            }
            Stmt::Assign { place, value } => {
                let p = self.expr(place);
                let v = self.expr(value);
                self.line(&format!("{p} = {v}"));
            }
            Stmt::Expr(e) => {
                let e = self.expr(e);
                self.line(&e);
            }
            Stmt::Return(e) => {
                let e = e.as_ref().map(|e| self.expr(e)).unwrap_or_default();
                self.line(&format!("return {e}"));
            }
            Stmt::Break(e) => {
                let e = e.as_ref().map(|e| self.expr(e)).unwrap_or_default();
                self.line(&format!("break {e}"));
            }
            Stmt::Continue => self.line("continue"),
        }
    }

    /// Render an expression to a single-line string. Blocks/if/match/loop print
    /// their bodies inline via nested lines instead, so those emit through
    /// `self.line` and return a short header.
    fn expr(&mut self, e: &Expr) -> String {
        let ty = e.ty().display(self.defs);
        match e {
            Expr::Lit(l, _) => format!("{}: {ty}", lit_str(l)),
            Expr::Local(d, _) => format!("{}: {ty}", self.defs.get(*d).name),
            Expr::Global(d, _) => format!("{}: {ty}", self.defs.canonical_string(*d)),
            Expr::Call {
                callee,
                args,
                builtin,
                ..
            } => {
                let c = self.expr(callee);
                let a = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                // A builtin primitive operator prints its tag so the O(1)
                // codegen marker is visible in the dump.
                match builtin {
                    Some(op) => format!("#builtin({op:?}) ({c})({a}): {ty}"),
                    None => format!("({c})({a}): {ty}"),
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let l = self.expr(lhs);
                let r = self.expr(rhs);
                format!("({l} {op:?} {r}): {ty}")
            }
            Expr::Unary { op, operand, .. } => {
                let o = self.expr(operand);
                format!("({op:?} {o}): {ty}")
            }
            Expr::Ref { mutable, place, .. } => {
                let p = self.expr(place);
                format!("(&{}{p}): {ty}", if *mutable { "mut " } else { "" })
            }
            Expr::Deref { base, .. } => {
                let b = self.expr(base);
                format!("({b}.*): {ty}")
            }
            Expr::Field { base, name, .. } => {
                let b = self.expr(base);
                format!("({b}.{name}): {ty}")
            }
            Expr::TupleIndex { base, index, .. } => {
                let b = self.expr(base);
                format!("({b}.{index}): {ty}")
            }
            Expr::Index { base, index, .. } => {
                let b = self.expr(base);
                let i = self.expr(index);
                format!("({b}[{i}]): {ty}")
            }
            Expr::Tuple { elems, .. } => {
                let es = elems
                    .iter()
                    .map(|e| self.expr(e))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({es}): {ty}")
            }
            Expr::Construct { def, fields, .. } => {
                let fs = fields
                    .iter()
                    .map(|(n, e)| format!("{n}: {}", self.expr(e)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} {{ {fs} }}: {ty}", self.defs.canonical_string(*def))
            }
            Expr::Variant { name, args, .. } if args.is_empty() => format!(".{name}: {ty}"),
            Expr::Variant { name, args, .. } => {
                let a = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(".{name}({a}): {ty}")
            }
            Expr::DynCast {
                value, concrete, ..
            } => {
                let v = self.expr(value);
                format!("({v} as {ty} from {})", concrete.display(self.defs))
            }
            Expr::Intrinsic { name, args, .. } => {
                let a = args
                    .iter()
                    .map(|a| self.expr(a))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("${name}({a}): {ty}")
            }
            Expr::Block(b) => {
                self.line(&format!("block: {ty} {{"));
                self.indent += 1;
                self.block(b);
                self.indent -= 1;
                self.line("}");
                format!("<block: {ty}>")
            }
            Expr::If {
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
            Expr::Match {
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
            Expr::Loop { body, .. } => {
                self.line("loop {");
                self.indent += 1;
                self.block(body);
                self.indent -= 1;
                self.line("}");
                format!("<loop: {ty}>")
            }
            Expr::Error(_) => format!("<error: {ty}>"),
        }
    }

    fn arm(&mut self, a: &Arm) {
        let mut header = format!("{} =>", pattern_str(&a.pattern));
        if let Some(g) = &a.guard {
            let g = self.expr(g);
            let _ = write!(header, " if {g}");
        }
        let body = self.expr(&a.body);
        self.line(&format!("{header} {body}"));
    }
}

fn pattern_str(p: &Pattern) -> String {
    match p {
        Pattern::Wildcard => "_".into(),
        Pattern::Binding { name, .. } => name.to_string(),
        Pattern::Lit(l) => lit_str(l),
        Pattern::Variant { name, sub } => {
            if sub.is_empty() {
                format!(".{name}")
            } else {
                let s = sub.iter().map(pattern_str).collect::<Vec<_>>().join(", ");
                format!(".{name}({s})")
            }
        }
        Pattern::Tuple(ps) => {
            let s = ps.iter().map(pattern_str).collect::<Vec<_>>().join(", ");
            format!("({s})")
        }
        Pattern::Or(ps) => ps.iter().map(pattern_str).collect::<Vec<_>>().join(" | "),
        Pattern::Struct { def, fields, rest } => {
            let mut parts: Vec<String> = fields
                .iter()
                .map(|(n, p)| format!("{n}: {}", pattern_str(p)))
                .collect();
            if *rest {
                parts.push("..".into());
            }
            let head = def.map_or_else(|| ".".to_string(), |d| format!("#{} ", d.0));
            format!("{head}{{ {} }}", parts.join(", "))
        }
        Pattern::TupleStruct { def, elems, rest } => {
            let mut parts: Vec<String> = elems.iter().map(pattern_str).collect();
            if *rest {
                parts.push("..".into());
            }
            let head = def.map_or_else(String::new, |d| format!("#{}", d.0));
            format!("{head}({})", parts.join(", "))
        }
        Pattern::Slice {
            prefix,
            rest,
            suffix,
        } => {
            let mut parts: Vec<String> = prefix.iter().map(pattern_str).collect();
            if let Some(binding) = rest {
                parts.push(match binding {
                    Some(b) => format!(".. {}", b.name),
                    None => "..".into(),
                });
            }
            parts.extend(suffix.iter().map(pattern_str));
            format!("[{}]", parts.join(", "))
        }
        Pattern::Range {
            start,
            end,
            inclusive,
        } => {
            let op = if *inclusive { "..=" } else { "..<" };
            let s = start.as_ref().map(lit_str).unwrap_or_default();
            let e = end.as_ref().map(lit_str).unwrap_or_default();
            format!("{s}{op}{e}")
        }
        Pattern::At { binding, pattern } => {
            format!("{} @ {}", binding.name, pattern_str(pattern))
        }
        Pattern::Deref(p) => format!("&{}", pattern_str(p)),
    }
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
