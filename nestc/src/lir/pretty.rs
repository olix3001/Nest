//! A textual dump of the LIR, in the shape `design/lir.md` §1 writes it.
//!
//! It is not decoration. A CFG is the one representation where a reader cannot
//! reconstruct intent from the shape — every construct becomes the same jumps —
//! so the dump carries three things the structures hold for exactly this reason:
//! each block's **label**, each statement's **span**, and each local's **source
//! name**. Together they are what turns a list of blocks back into a program
//! somebody wrote.
//!
//! **Everything is written out in full.** A type is named the way the source
//! names it (`core.Vec.<i32>`, `Shape.circle`), a function by its whole path
//! with its symbol beside it, a global by its name and never by its index, an
//! operation by a word (`add_checked.i32`) and never by a sigil that a reader
//! has to look up. The indices the structures use are how a *backend* resolves a
//! reference; a person reading a dump should never have to.
//!
//! The one convention worth stating: `:=` **introduces** a value into a local
//! and `=` **stores** into a place. That is the same distinction the source
//! language draws, so it costs a reader nothing to carry across.

use std::fmt::Write;

use crate::common::source::SourceMap;

use super::{
    Aggregate, Base, Block, Callee, Constant, Function, Global, Operand, Origin, Place, Program,
    Projection, Rvalue, Stmt, StmtKind, TermKind, Ty, TypeDef, Unit,
};

/// Render a whole [`Program`] — every codegen unit in it.
///
/// `sources` is optional: with it, every instruction carries `file:line:col` the
/// way §1's example does; without it the spans are left off, which is what a
/// test comparing structure wants.
pub fn program_to_string(sources: Option<&SourceMap>, p: &Program) -> String {
    let mut out = String::new();
    for (i, u) in p.units.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&unit_to_string(sources, u));
    }
    out
}

/// Render one codegen unit.
pub fn unit_to_string(sources: Option<&SourceMap>, u: &Unit) -> String {
    let mut pr = Printer {
        sources,
        unit: u,
        out: String::new(),
    };
    pr.line(&format!("unit {} {{", u.name));
    // Types first, then the data, then the code: a function's locals read better
    // once a reader has seen what their types are made of.
    for t in &u.types {
        pr.type_def(t);
    }
    for g in &u.globals {
        pr.global(g);
    }
    for f in &u.funcs {
        pr.function(f);
    }
    pr.line("}");
    pr.out
}

struct Printer<'a> {
    sources: Option<&'a SourceMap>,
    unit: &'a Unit,
    out: String,
}

impl Printer<'_> {
    fn line(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }

    // ===< Types >===

    fn type_def(&mut self, t: &TypeDef) {
        let shape = match &t.origin {
            Origin::Struct => "struct",
            Origin::Enum { .. } => "enum",
            Origin::Tuple => "tuple",
            Origin::Slice => "slice",
            Origin::Dyn { trait_name } => &format!("dyn {trait_name}"),
            Origin::Variant { parent } => &format!("variant of {parent}"),
            Origin::Vtable { trait_name } => &format!("vtable for {trait_name}"),
        };
        self.line(&format!(
            "  type {} = struct {{           // was a {shape}; size {}, align {}",
            t.name, t.layout.size, t.layout.align
        ));
        for m in &t.members {
            let ty = self.ty(&m.ty);
            self.line(&format!(
                "    {}: {ty}                    // +{}",
                m.name, m.offset
            ));
        }
        self.line("  }");
        // The variants survive the tag they became: a debugger showing `2`
        // instead of `.green` is a worse debugger (§7c). Each one names the type
        // its payload is read as, which is where its members' offsets are.
        if let Origin::Enum { variants } = &t.origin {
            for v in variants {
                let name = self.unit.ty(v.ty).name.clone();
                let (open, close) = if v.tuple { ("(", ")") } else { (" { ", " }") };
                let members: Vec<String> = self
                    .unit
                    .ty(v.ty)
                    .members
                    .iter()
                    .map(|m| format!("{}: {} +{}", m.name, self.ty(&m.ty), m.offset))
                    .collect();
                if members.is_empty() {
                    self.line(&format!("    // tag {} => .{} as {name}", v.tag, v.name));
                } else {
                    self.line(&format!(
                        "    // tag {} => .{} as {name}{open}{}{close}",
                        v.tag,
                        v.name,
                        members.join(", ")
                    ));
                }
            }
        }
    }

    /// A type, written the way the source writes it.
    fn ty(&self, ty: &Ty) -> String {
        match ty {
            Ty::Int { bits, signed } => {
                format!("{}{bits}", if *signed { "i" } else { "u" })
            }
            Ty::Float { bits } => format!("f{bits}"),
            Ty::Bool => "bool".to_string(),
            Ty::Ptr(inner) => format!("*{}", self.ty(inner)),
            Ty::Array { len, elem } => format!("[{len}]{}", self.ty(elem)),
            Ty::Func { params, ret } => {
                let ps: Vec<String> = params.iter().map(|p| self.ty(p)).collect();
                format!("func({}) -> {}", ps.join(", "), self.ty(ret))
            }
            Ty::Named(id) => self
                .unit
                .types
                .get(id.0 as usize)
                .map(|t| t.name.clone())
                // A type id with nothing behind it is a defect in the split, and
                // saying so is better than printing a number.
                .unwrap_or_else(|| format!("<unknown type #{}>", id.0)),
            Ty::Void => "void".to_string(),
            Ty::Never => "never".to_string(),
        }
    }

    // ===< Data >===

    fn global(&mut self, g: &Global) {
        let ty = self.ty(&g.ty);
        let kind = if g.mutable { "global" } else { "const" };
        if g.linkage == super::Linkage::Imported {
            // Another unit defines it; this one only needs the linker to know
            // the name and the shape (§11).
            self.line(&format!("  extern {kind} {}: {ty}  // {}", g.name, g.symbol));
            return;
        }
        let init = match &g.init {
            Some(c) => self.constant(c),
            None => "zeroed".to_string(),
        };
        // `private` is the linkage, printed because it is the difference between
        // a name the linker resolves and one it never sees (§11).
        let vis = if g.linkage == super::Linkage::Internal {
            "private "
        } else {
            ""
        };
        self.line(&format!(
            "  {vis}{kind} {}: {ty} = {init}  // {}{}",
            g.name,
            g.symbol,
            self.at(g.span)
        ));
    }

    fn constant(&self, c: &Constant) -> String {
        match c {
            Constant::Int(n) => n.to_string(),
            // `.0` on a whole number, so a dump never reads a float as an
            // integer: `x == 0` and `x == 0.0` are different instructions, and
            // the point of the dump is to say which one this is.
            Constant::Float(f) if f.fract() == 0.0 && f.is_finite() => format!("{f:.1}"),
            Constant::Float(f) => f.to_string(),
            Constant::Bool(b) => b.to_string(),
            Constant::Func(id) => match self.unit.funcs.get(id.0 as usize) {
                Some(f) => format!("&{}", f.name),
                None => format!("&<unknown function #{}>", id.0),
            },
            Constant::Global(id) => match self.unit.globals.get(id.0 as usize) {
                Some(g) => format!("&{}", g.name),
                None => format!("&<unknown global #{}>", id.0),
            },
            Constant::Aggregate(items) => {
                let parts: Vec<String> = items.iter().map(|i| self.constant(i)).collect();
                format!("{{ {} }}", parts.join(", "))
            }
            Constant::Bytes(b) => crate::parser::ast::bytes_repr(b),
            Constant::Variant { name, payload, tag } if payload.is_empty() => {
                format!(".{name}#{tag}")
            }
            Constant::Variant { name, payload, tag } => {
                let parts: Vec<String> = payload.iter().map(|i| self.constant(i)).collect();
                format!(".{name}#{tag}({})", parts.join(", "))
            }
            Constant::Undef => "undef".to_string(),
        }
    }

    // ===< Code >===

    fn function(&mut self, f: &Function) {
        let params: Vec<String> = f.locals[..f.params.min(f.locals.len())]
            .iter()
            .map(|l| format!("{}: {}", self.local_name(f, l.id), self.ty(&l.ty)))
            .collect();
        let abi = match &f.extern_abi {
            Some(a) => format!("extern(\"{a}\") "),
            None => String::new(),
        };
        let mut tags = String::new();
        if let Some(s) = &f.attrs.section {
            let _ = write!(tags, " #section(\"{s}\")");
        }
        match f.attrs.inline {
            super::Inline::Always => tags.push_str(" #inline"),
            super::Inline::Never => tags.push_str(" #inline(never)"),
            super::Inline::Default => {}
        }
        if let Some(n) = f.attrs.offset {
            let _ = write!(tags, " #offset({n})");
        }
        if f.attrs.public {
            tags.push_str(" @public");
        }
        if f.attrs.unchecked {
            tags.push_str(" #unsafe");
        }
        let ret = self.ty(&f.ret);
        let decl = if f.blocks.is_empty() { "declare " } else { "" };
        self.line(&format!(
            "\n  {decl}{abi}func {}({}) -> {ret}{tags}  // {}{}",
            f.name,
            params.join(", "),
            f.symbol,
            self.at(f.span)
        ));
        if f.blocks.is_empty() {
            // A declaration: a signature and a symbol, and no code of its own —
            // an `extern` function, or one another unit defines (§11).
            return;
        }
        // Locals are declared up front (§1), parameters excluded: those are in
        // the signature above.
        for l in &f.locals[f.params.min(f.locals.len())..] {
            let ty = self.ty(&l.ty);
            self.line(&format!(
                "    let {}: {ty}{}",
                self.local_name(f, l.id),
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
        match f.locals.get(id.0 as usize).and_then(|l| l.name.as_ref()) {
            Some(n) => format!("{n}_{}", id.0),
            None => format!("_{}", id.0),
        }
    }

    fn block(&mut self, f: &Function, b: &Block) {
        let label = match &b.label {
            Some(l) => format!("                    // {l}"),
            None => String::new(),
        };
        self.line(&format!("  bb{}:{label}", b.id.0));
        for s in &b.stmts {
            let text = self.stmt(f, s);
            self.line(&format!("    {text}{}", self.at(s.span)));
            self.safepoint(f, s.safepoint.as_ref());
        }
        let term = match &b.term.kind {
            TermKind::Goto(t) => format!("goto bb{}", t.0),
            TermKind::Switch {
                value,
                ty,
                arms,
                otherwise,
            } => {
                let arms: Vec<String> = arms
                    .iter()
                    .map(|(v, t)| format!("{v} => bb{}", t.0))
                    .collect();
                format!(
                    "switch.{} {} {{ {}, _ => bb{} }}",
                    self.ty(ty),
                    self.operand(f, value),
                    arms.join(", "),
                    otherwise.0
                )
            }
            TermKind::Return(Some(v)) => format!("return {}", self.operand(f, v)),
            TermKind::Return(None) => "return".to_string(),
            TermKind::Unreachable => "unreachable".to_string(),
        };
        self.line(&format!("    {term}{}", self.at(b.term.span)));
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
            self.line("      @safepoint { live: [] }");
            return;
        }
        self.line(&format!("      @safepoint {{ live: [{}]", live.join(", ")));
        for l in &sp.live {
            let n = self.local_name(f, *l);
            self.line(&format!("        {n} := reloc {n}"));
        }
        self.line("      }");
    }

    fn stmt(&self, f: &Function, s: &Stmt) -> String {
        match &s.kind {
            StmtKind::Assign { place, value } => {
                // `:=` introduces into a local; `=` stores into a place. The
                // same distinction the source draws.
                let op = if place.is_whole_local() { ":=" } else { "=" };
                format!("{} {op} {}", self.place(f, place), self.rvalue(f, value))
            }
            StmtKind::Drop(o) => format!("drop {}", self.operand(f, o)),
            StmtKind::Call { dest, callee, args } => {
                let args: Vec<String> = args.iter().map(|a| self.operand(f, a)).collect();
                let call = match callee {
                    Callee::Static(id) => {
                        let name = match self.unit.funcs.get(id.0 as usize) {
                            Some(g) => g.name.clone(),
                            None => format!("<unknown function #{}>", id.0),
                        };
                        format!("call {name}({})", args.join(", "))
                    }
                    Callee::Indirect(o) => {
                        format!("call ({})({})", self.operand(f, o), args.join(", "))
                    }
                    Callee::Intrinsic(i) => format!("${}({})", i.name(), args.join(", ")),
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
            Rvalue::Ref(place) => format!("&{}", self.place(f, place)),
            // The operation, the type it runs at, then its operands. The type is
            // part of the instruction because `lt.u64` and `lt.i64` are
            // different instructions and a constant operand carries no type.
            Rvalue::Op { op, ty, args } => {
                let args: Vec<String> = args.iter().map(|a| self.operand(f, a)).collect();
                format!("{}.{} {}", op.name(), self.ty(ty), args.join(", "))
            }
            // The conversion names itself, the way an operation does: a reader
            // should not have to compare two widths to see that this one loses.
            Rvalue::Cast {
                value,
                kind,
                from,
                to,
            } => format!(
                "cast.{} {} : {} -> {}",
                kind.name(),
                self.operand(f, value),
                self.ty(from),
                self.ty(to)
            ),
            Rvalue::Aggregate { kind, fields } => {
                let fields: Vec<String> = fields.iter().map(|x| self.operand(f, x)).collect();
                let head = match kind {
                    Aggregate::Struct(id) => match self.unit.types.get(id.0 as usize) {
                        Some(t) => t.name.clone(),
                        None => format!("<unknown type #{}>", id.0),
                    },
                    Aggregate::Array => "array".to_string(),
                    Aggregate::Variant {
                        ty, name, index, ..
                    } => {
                        let e = match self.unit.types.get(ty.0 as usize) {
                            Some(t) => t.name.clone(),
                            None => format!("<unknown type #{}>", ty.0),
                        };
                        format!("{e}.{name}#{index}")
                    }
                };
                format!("{head}({})", fields.join(", "))
            }
            Rvalue::Offset { ptr, index, stride } => format!(
                "{} + {} * {stride}",
                self.operand(f, ptr),
                self.operand(f, index)
            ),
        }
    }

    fn operand(&self, f: &Function, o: &Operand) -> String {
        match o {
            Operand::Copy(p) => self.place(f, p),
            Operand::Const(c) => self.constant(c),
        }
    }

    /// A place, printed as §1 writes one: `p.x`, `p.*`, `p[i]`,
    /// `(p.payload as Shape.circle)`.
    fn place(&self, f: &Function, p: &Place) -> String {
        let mut s = match p.base {
            Base::Local(id) => self.local_name(f, id),
            Base::Global(id) => match self.unit.globals.get(id.0 as usize) {
                Some(g) => format!("@{}", g.name),
                None => format!("@<unknown global #{}>", id.0),
            },
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
                Projection::Cast(t) => {
                    let name = match self.unit.types.get(t.0 as usize) {
                        Some(d) => d.name.clone(),
                        None => format!("<unknown type #{}>", t.0),
                    };
                    s = format!("({s} as {name})");
                }
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
