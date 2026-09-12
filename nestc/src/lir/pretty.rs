//! A textual dump of the LIR, in the shape `design/lir.md` §1 writes it.
//!
//! It is not decoration. A CFG is the one representation where a reader cannot
//! reconstruct intent from the shape — every construct becomes the same jumps —
//! so the dump carries three things the structures hold for exactly this reason:
//! each block's **label**, each statement's **span**, and each local's **source
//! name**. Together they are what turns a list of blocks back into a program
//! somebody wrote.
//!
//! The one convention worth stating: `:=` **introduces** a value into a local
//! and `=` **stores** into a place. That is the same distinction the source
//! language draws, so it costs a reader nothing to carry across.

use std::fmt::Write;

use crate::common::source::SourceMap;
use crate::sema::def::DefTable;

use super::{
    AggregateKind, Base, Block, Callee, Constant, Function, Operand, Origin, Place, Program,
    Projection, Rvalue, Stmt, StmtKind, TermKind, TypeDef, Vtable,
};

/// Render a whole [`Program`].
///
/// `sources` is optional: with it, every instruction carries `file:line:col` the
/// way §1's example does; without it the spans are left off, which is what a
/// test comparing structure wants.
pub fn program_to_string(defs: &DefTable, sources: Option<&SourceMap>, p: &Program) -> String {
    let mut pr = Printer {
        defs,
        sources,
        out: String::new(),
    };
    // Types first, then the data, then the code: a function's locals read better
    // once a reader has seen what their types are made of.
    for t in &p.types {
        pr.type_def(t);
    }
    for g in &p.globals {
        let init = match &g.init {
            Some(v) => v.display(),
            None => "zeroed".to_string(),
        };
        pr.line(&format!(
            "global {}: {} = {init}",
            g.name,
            g.ty.display(defs)
        ));
    }
    for v in &p.vtables {
        pr.vtable(v);
    }
    for f in &p.funcs {
        pr.function(f);
    }
    pr.out
}

struct Printer<'a> {
    defs: &'a DefTable,
    sources: Option<&'a SourceMap>,
    out: String,
}

impl Printer<'_> {
    fn line(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn type_def(&mut self, t: &TypeDef) {
        let shape = match &t.origin {
            Origin::Struct(_) => "struct",
            Origin::Enum { .. } => "enum",
            Origin::Tuple => "tuple",
            Origin::Slice => "slice",
            Origin::Dyn(_) => "dyn",
        };
        self.line(&format!(
            "type {} = struct {{           // was a {shape}; size {}, align {}",
            t.name, t.layout.size, t.layout.align
        ));
        for m in &t.members {
            self.line(&format!(
                "  {}: {}                    // +{}",
                m.name,
                m.ty.display(self.defs),
                m.offset
            ));
        }
        self.line("}");
        // The variants survive the tag they became: a debugger showing `2`
        // instead of `.green` is a worse debugger (§7c).
        if let Origin::Enum { variants, .. } = &t.origin {
            for v in variants {
                let members: Vec<String> = v
                    .members
                    .iter()
                    .map(|m| {
                        format!(
                            "{}: {} +{}",
                            m.name,
                            m.ty.display(self.defs),
                            m.offset
                        )
                    })
                    .collect();
                // A payload written positionally prints in brackets and a
                // record one in braces, which is how it was written — the tag
                // it became does not remember, and the definition does.
                let (open, close) = if v.tuple { ("(", ")") } else { (" { ", " }") };
                if members.is_empty() {
                    self.line(&format!("  // tag {} => .{}", v.tag, v.name));
                } else {
                    self.line(&format!(
                        "  // tag {} => .{}{open}{}{close}",
                        v.tag,
                        v.name,
                        members.join(", ")
                    ));
                }
            }
        }
    }

    fn vtable(&mut self, v: &Vtable) {
        self.line(&format!(
            "vtable {} for {} as {} {{",
            v.symbol,
            v.concrete.display(self.defs),
            self.defs.canonical_string(v.trait_def)
        ));
        for (i, slot) in v.slots.iter().enumerate() {
            match slot {
                Some(s) => self.line(&format!("  [{i}] {} = {}", s.method, s.symbol)),
                // Object safety should have made this unreachable; a hole is
                // how a defect shows up as a defect rather than as a call to
                // the wrong function.
                None => self.line(&format!("  [{i}] <unfilled>")),
            }
        }
        self.line("}");
    }

    fn function(&mut self, f: &Function) {
        let params: Vec<String> = f.locals[..f.params]
            .iter()
            .map(|l| format!("{}: {}", self.local_name(f, l.id), l.ty.display(self.defs)))
            .collect();
        let abi = match &f.extern_abi {
            Some(a) => format!("extern(\"{a}\") "),
            None => String::new(),
        };
        let mut tags = String::new();
        for d in &f.directives {
            let _ = write!(tags, " {}", crate::ir::pretty::directive_str(d));
        }
        self.line(&format!(
            "\n{abi}func {}({}) -> {}{tags}  // {}{}",
            f.name,
            params.join(", "),
            f.ret.display(self.defs),
            f.symbol,
            self.at(f.span)
        ));
        if f.blocks.is_empty() {
            // A declaration: a signature and a symbol, and no code of its own.
            self.line("  // declaration only");
            return;
        }
        // Locals are declared up front (§1), parameters excluded: those are in
        // the signature above.
        for l in &f.locals[f.params..] {
            self.line(&format!(
                "  let {}: {}{}",
                self.local_name(f, l.id),
                l.ty.display(self.defs),
                self.at(l.span)
            ));
        }
        for b in &f.blocks {
            self.block(f, b);
        }
    }

    /// A local's source name, or its slot number when nothing named it.
    ///
    /// A temporary no source name produced simply has none, and showing it as a
    /// slot is honest — better than inventing a name (§7c).
    fn local_name(&self, f: &Function, id: super::LocalId) -> String {
        match &f.locals[id.0 as usize].name {
            Some(n) => format!("{n}_{}", id.0),
            None => format!("_{}", id.0),
        }
    }

    fn block(&mut self, f: &Function, b: &Block) {
        let label = match &b.label {
            Some(l) => format!("                    // {l}"),
            None => String::new(),
        };
        self.line(&format!("bb{}:{label}", b.id.0));
        for s in &b.stmts {
            let text = self.stmt(f, s);
            self.line(&format!("  {text}{}", self.at(s.span)));
            self.safepoint(f, s.safepoint.as_ref());
        }
        let term = match &b.term.kind {
            TermKind::Goto(t) => format!("goto bb{}", t.0),
            TermKind::Switch {
                value,
                arms,
                otherwise,
            } => {
                let arms: Vec<String> = arms
                    .iter()
                    .map(|(v, t)| format!("{v} => bb{}", t.0))
                    .collect();
                format!(
                    "switch {} {{ {}, _ => bb{} }}",
                    self.operand(f, value),
                    arms.join(", "),
                    otherwise.0
                )
            }
            TermKind::Return(Some(v)) => format!("return {}", self.operand(f, v)),
            TermKind::Return(None) => "return".to_string(),
            TermKind::Unreachable => "unreachable".to_string(),
        };
        self.line(&format!("  {term}{}", self.at(b.term.span)));
        self.safepoint(f, b.term.safepoint.as_ref());
    }

    /// A safepoint, written the way §6 writes it: the live set, then one
    /// `reloc` line per root, because a redefinition is what the list *means*.
    fn safepoint(&mut self, f: &Function, sp: Option<&super::Safepoint>) {
        let Some(sp) = sp else { return };
        let live: Vec<String> = sp.live.iter().map(|l| self.local_name(f, *l)).collect();
        // Nothing to trace is still a point the collector may run at, and saying
        // so on one line keeps a dump readable when most of them are empty.
        if live.is_empty() {
            self.line("    @safepoint { live: [] }");
            return;
        }
        self.line(&format!("    @safepoint {{ live: [{}]", live.join(", ")));
        for l in &sp.live {
            let n = self.local_name(f, *l);
            self.line(&format!("      {n} := reloc {n}"));
        }
        self.line("    }");
    }

    fn stmt(&self, f: &Function, s: &Stmt) -> String {
        match &s.kind {
            StmtKind::Assign { place, value } => {
                // `:=` introduces into a local; `=` stores into a place. The
                // same distinction the source draws.
                let op = if place.is_whole_local() { ":=" } else { "=" };
                format!(
                    "{} {op} {}",
                    self.place(f, place),
                    self.rvalue(f, value)
                )
            }
            StmtKind::Intrinsic { dest, name, args } => {
                let args: Vec<String> = args.iter().map(|a| self.operand(f, a)).collect();
                let call = format!("${name}({})", args.join(", "));
                match dest {
                    Some(d) if d.is_whole_local() => format!("{} := {call}", self.place(f, d)),
                    Some(d) => format!("{} = {call}", self.place(f, d)),
                    None => call,
                }
            }
            StmtKind::Drop(o) => format!("drop {}", self.operand(f, o)),
            StmtKind::Call { dest, callee, args } => {
                let args: Vec<String> = args.iter().map(|a| self.operand(f, a)).collect();
                let call = match callee {
                    Callee::Static { name, .. } => format!("call {name}({})", args.join(", ")),
                    Callee::Indirect(o) => {
                        format!("call ({})({})", self.operand(f, o), args.join(", "))
                    }
                };
                match dest {
                    Some(d) if d.is_whole_local() => format!("{} := {call}", self.place(f, d)),
                    Some(d) => format!("{} = {call}", self.place(f, d)),
                    None => call,
                }
            }
        }
    }

    fn rvalue(&self, f: &Function, v: &Rvalue) -> String {
        match v {
            Rvalue::Use(o) => self.operand(f, o),
            Rvalue::Ref { mutable, place } => {
                let m = if *mutable { "&mut " } else { "&" };
                format!("{m}{}", self.place(f, place))
            }
            Rvalue::Builtin { op, args, checked } => {
                let args: Vec<String> = args.iter().map(|a| self.operand(f, a)).collect();
                let c = if *checked { "checked_" } else { "" };
                // Only the operation's name is lowercased; an operand may be a
                // string or a type name, and case is part of it.
                format!("{c}{}({})", format!("{op:?}").to_lowercase(), args.join(", "))
            }
            Rvalue::Binary { op, lhs, rhs } => format!(
                "{} {} {}",
                self.operand(f, lhs),
                bin_str(*op),
                self.operand(f, rhs)
            ),
            Rvalue::Unary { op, operand } => {
                format!("{}{}", un_str(*op), self.operand(f, operand))
            }
            Rvalue::Cast { value, from, to } => format!(
                "cast {} : {} -> {}",
                self.operand(f, value),
                from.display(self.defs),
                to.display(self.defs)
            ),
            Rvalue::Aggregate { kind, fields } => {
                let fields: Vec<String> = fields.iter().map(|x| self.operand(f, x)).collect();
                let head = match kind {
                    AggregateKind::Struct(d) => self.defs.get(*d).name.to_string(),
                    AggregateKind::Tuple => String::new(),
                    AggregateKind::Array => "array".to_string(),
                    AggregateKind::Variant { name, index, .. } => format!(".{name}#{index}"),
                    AggregateKind::Slice => "slice".to_string(),
                    AggregateKind::Dyn => "dyn".to_string(),
                };
                format!("{head}({})", fields.join(", "))
            }
            Rvalue::Offset { ptr, index, elem } => format!(
                "{} + {} * stride({})",
                self.operand(f, ptr),
                self.operand(f, index),
                elem.display(self.defs)
            ),
        }
    }

    fn operand(&self, f: &Function, o: &Operand) -> String {
        match o {
            Operand::Copy(p) => self.place(f, p),
            Operand::Const(c) => match c {
                Constant::Value(v) => v.display(),
                Constant::Func { name, .. } => format!("&{name}"),
                Constant::Vtable(id) => format!("&vtable#{}", id.0),
                Constant::Undef => "undef".to_string(),
            },
        }
    }

    /// A place, printed as §1 writes one: `p.x`, `p.*`, `p[i]`,
    /// `(p as Some).0`.
    fn place(&self, f: &Function, p: &Place) -> String {
        let mut s = match p.base {
            Base::Local(id) => self.local_name(f, id),
            Base::Global(def) => format!("@{}", self.defs.get(def).name),
        };
        for proj in &p.projection {
            match proj {
                // By **name**, not by index (§1). The index is what codegen
                // wants; a reader debugging a mis-lowered access needs to know
                // which field it was, and after `#packed` / `#align` / `#soa`
                // have had their say the index alone does not tell them.
                Projection::Field { name, .. } => s = format!("{s}.{name}"),
                Projection::Index(i) => s = format!("{s}[{}]", self.operand(f, i)),
                Projection::Deref => s = format!("{s}.*"),
                Projection::Variant { name, .. } => s = format!("({s} as {name})"),
            }
        }
        s
    }

    /// `file:line:col`, when a source map was supplied.
    fn at(&self, span: Option<crate::common::source::FileSpan>) -> String {
        let (Some(sources), Some(span)) = (self.sources, span) else {
            return String::new();
        };
        let Some(file) = sources.file(span.file) else {
            return String::new();
        };
        let pos = file.line_col(span.span.start);
        // The file's base name: a dump is read beside the source, and the
        // directory the compiler was run from says nothing about the program.
        let name = file.name.rsplit('/').next().unwrap_or(&file.name);
        format!("   // {name}:{}:{}", pos.line, pos.column)
    }
}

fn bin_str(op: crate::parser::ast::BinOp) -> &'static str {
    use crate::parser::ast::BinOp::*;
    match op {
        And => "&&",
        Or => "||",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        BitOr => "|",
        BitXor => "^",
        BitAnd => "&",
        Shl => "<<",
        Shr => ">>",
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
    }
}

fn un_str(op: crate::parser::ast::UnOp) -> &'static str {
    use crate::parser::ast::UnOp::*;
    match op {
        Ref => "&",
        RefMut => "&mut ",
        Neg => "-",
        Not => "!",
        BitNot => "~",
    }
}
