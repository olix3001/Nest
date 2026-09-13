//! The program's entry point, synthesized.
//!
//! A linker looks for `main`, the C runtime's startup calls it, and neither of
//! them knows anything about this language: `main :: func ()` (§5.6) is an
//! ordinary Nest function with a mangled symbol, and something has to stand
//! between the two. That something is this — a second function, named `main`
//! and *symbolled* `main`, which calls the runtime's initializer and then the
//! program's.
//!
//! **It is built here rather than in a backend**, because it is not a fact about
//! any machine: it is two calls and a return, which LIR can already say. A
//! backend that invented it would invent it again, differently, for the next
//! target — and a debugger would find a frame no dump had ever printed.
//!
//! **It is built here rather than in the C runtime**, which is the other place
//! it could go. A `main` in `nest_runtime.c` would have to name the program's
//! entry symbol, and that symbol is mangled by a scheme the runtime would then
//! have to encode — and would have to encode *again* for a `main` returning a
//! status rather than nothing. The shim stays six functions that know no names.

use super::{
    Block, BlockId, CastKind, Callee, FuncId, Function, FunctionAttrs, Local, LocalId, Operand,
    Place, Rvalue, Stmt, StmtKind, Terminator, TermKind, Ty, Unit,
};
use crate::common::source::FileSpan;
use crate::common::symbol::Symbol;

/// The symbol the C runtime's startup calls. Not mangled, because the caller is
/// not this compiler.
const SYMBOL: &str = "main";

/// What the function is *called*, which is deliberately not `main`.
///
/// The `name` on a [`Function`] is its unmangled source name, and this function
/// has none — it is not in the source. Calling it `main` too would print two of
/// them in every dump of a program, one calling the other, distinguishable only
/// by a symbol comment. A stack frame labelled `entry` sitting under `main` says
/// what it is.
const NAME: &str = "entry";

/// The runtime's initializer (`runtime/nest_runtime.c`). Called once, first.
const INIT: &str = "nest_init";

/// What `main` returns to the operating system. C's `int` on every target this
/// compiler can name.
fn status_ty() -> Ty {
    Ty::Int {
        bits: 32,
        signed: true,
    }
}

/// Add the entry point to `unit`, calling the program's `main` (`entry`).
///
/// The synthesized function takes the program `main`'s **span**, so the codegen
/// unit split (§11) puts it in the same unit as the function it calls rather
/// than in a unit of its own.
pub fn synthesize(unit: &mut Unit, entry: FuncId) {
    let called = &unit.funcs[entry.0 as usize];
    let span = called.span;
    let ret = called.ret.clone();

    let init = declare_init(unit);

    let mut locals: Vec<Local> = Vec::new();
    let mut stmts = vec![Stmt::new(
        StmtKind::Call {
            dest: None,
            callee: Callee::Static(init),
            args: Vec::new(),
        },
        span,
    )];

    // `main` may return nothing, a status, or not at all (§5.6, and
    // `ir::check::declarations` is what refuses everything else). The three
    // differ only in what this function returns afterwards.
    let term = match &ret {
        // `-> never`: the call does not come back, so there is nothing after it
        // and no value to return. Every block still ends, and this is how a
        // block that cannot be left says so.
        Ty::Never => {
            stmts.push(Stmt::new(
                StmtKind::Call {
                    dest: None,
                    callee: Callee::Static(entry),
                    args: Vec::new(),
                },
                span,
            ));
            Terminator::new(TermKind::Unreachable, span)
        }
        // A status. It is the program's answer, so it is returned — converted
        // when the program's integer is not C's, which `CastKind` names rather
        // than leaving to a backend.
        Ty::Int { .. } => {
            let raw = push_local(&mut locals, ret.clone(), span);
            stmts.push(Stmt::new(
                StmtKind::Call {
                    dest: Some(Place::local(raw)),
                    callee: Callee::Static(entry),
                    args: Vec::new(),
                },
                span,
            ));
            let status = if ret == status_ty() {
                Operand::Copy(Place::local(raw))
            } else {
                let converted = push_local(&mut locals, status_ty(), span);
                stmts.push(Stmt::new(
                    StmtKind::Assign {
                        place: Place::local(converted),
                        value: Rvalue::Cast {
                            value: Operand::Copy(Place::local(raw)),
                            kind: CastKind::of(&ret, &status_ty()),
                            from: ret.clone(),
                            to: status_ty(),
                        },
                    },
                    span,
                ));
                Operand::Copy(Place::local(converted))
            };
            Terminator::new(TermKind::Return(Some(status)), span)
        }
        // Nothing, which is a successful exit: a program that says nothing about
        // its status has not failed.
        _ => {
            stmts.push(Stmt::new(
                StmtKind::Call {
                    dest: None,
                    callee: Callee::Static(entry),
                    args: Vec::new(),
                },
                span,
            ));
            Terminator::new(
                TermKind::Return(Some(Operand::int(0))),
                span,
            )
        }
    };

    unit.funcs.push(Function {
        name: NAME.to_string(),
        symbol: Symbol::new(SYMBOL),
        locals,
        params: 0,
        ret: status_ty(),
        blocks: vec![Block {
            id: BlockId(0),
            stmts,
            term,
            label: Some("entry point".to_string()),
        }],
        // The ABI is C's, and saying so is not decoration: this is the one
        // function in the program whose caller is not this compiler.
        extern_abi: Some(Symbol::new("c")),
        span,
        attrs: FunctionAttrs {
            // The linker has to see it, which is the whole point of it existing.
            public: true,
            ..FunctionAttrs::default()
        },
    });
}

/// The declaration of `nest_init`, reusing one the program already has.
///
/// A program is free to declare `extern("c") nest_init :: func ()` itself — it
/// is an ordinary C function — and two declarations of one symbol is a thing a
/// backend would have to resolve or reject.
fn declare_init(unit: &mut Unit) -> FuncId {
    if let Some(i) = unit.funcs.iter().position(|f| f.symbol.as_str() == INIT) {
        return FuncId(i as u32);
    }
    let id = FuncId(unit.funcs.len() as u32);
    unit.funcs.push(Function {
        name: INIT.to_string(),
        symbol: Symbol::new(INIT),
        locals: Vec::new(),
        params: 0,
        ret: Ty::Void,
        blocks: Vec::new(),
        extern_abi: Some(Symbol::new("c")),
        span: None,
        attrs: FunctionAttrs {
            public: true,
            ..FunctionAttrs::default()
        },
    });
    id
}

/// Append a local and name its slot.
fn push_local(locals: &mut Vec<Local>, ty: Ty, span: Option<FileSpan>) -> LocalId {
    let id = LocalId(locals.len() as u32);
    locals.push(Local {
        id,
        name: None,
        ty,
        span,
    });
    id
}

