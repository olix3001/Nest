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
//! status rather than nothing. The shim stays a handful of functions that know
//! no names.
//!
//! **It takes `argc` and `argv`**, because the call to `main` is the only
//! portable moment they exist: a program's arguments arrive once, in the frame
//! the operating system built, and nothing in C or POSIX hands them back to a
//! running program afterwards.
//!
//! What happens to them is **not** decided here. A library that claims
//! `#lang("start")` gets them, along with the program's own `main` as a function
//! pointer, and the whole of starting a Nest program is that library's from
//! there — `std/sys` is what claims it. This function is then two calls and a
//! return, and the part of it that is a *decision* rather than a fact about the
//! machine has left the compiler.
//!
//! **With no `#lang("start")` there is nothing to hand them to**, and the entry
//! calls the program's `main` directly. That is not a fallback so much as the
//! only thing left to do: a program built without `std` has no way to ask what
//! its arguments were, so there is nothing to keep them for.
//!
//! The environment is **not** passed, though `main` receives one: `envp` is a
//! snapshot, and `setenv` may replace the table under it. The runtime reads
//! `environ` instead, which is the live one.

use super::{
    Block, BlockId, CastKind, Callee, Constant, FuncId, Function, FunctionAttrs, Global, GlobalId,
    Linkage, Local, LocalId, Operand, Place, Rvalue, Stmt, StmtKind, Terminator, TermKind, Ty,
    Unit,
};
use crate::common::options::Target;
use crate::common::source::{FileSpan, SourceMap};
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

/// The runtime's initializer (`runtime/nest_runtime.c`). Called once, first,
/// and given nothing: what it prepares is the collector, and what the process
/// was started with belongs to whoever claims `#lang("start")`.
const INIT: &str = "nest_init";

/// The wrapper that turns the program's `main` into the `func () -> i32` a
/// `#lang("start")` takes a pointer to. Named, like `entry`, for a dump.
const STATUS_NAME: &str = "entry.status";

/// Its symbol. Not mangled — there is no path to mangle, because there is no
/// declaration in any source — and prefixed so nothing a program can write
/// collides with it.
const STATUS_SYMBOL: &str = "_NEstatus";

/// `argc`. C's `int`, which is what the startup passes and what `argv` is
/// counted in.
fn argc_ty() -> Ty {
    status_ty()
}

/// `argv`: a `char **`, and **an integer here**, not a [`Ty::Ptr`].
///
/// The width is the target's, so it is a pointer's width and passes in a
/// pointer's register — the C ABI sees no difference. What changes is what the
/// collector sees: [`super::safepoint`] treats every `Ptr` local as a root, and
/// these two address memory the startup owns and the collector has never heard
/// of. Relocating one at a safepoint would move a pointer into C's stack, and
/// keeping it in a root set is a claim about an object that does not exist.
///
/// It is the same answer `core/c` already gives: a `c.ptr.<T>` is one `usize`
/// in a struct, untraced by construction, and that is the type `std/process`
/// receives this as.
fn argv_ty(target: Target) -> Ty {
    Ty::Int {
        bits: target.pointer_bits as u16,
        signed: false,
    }
}

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
pub fn synthesize(unit: &mut Unit, entry: FuncId, start: Option<FuncId>, target: Target) {
    let called = &unit.funcs[entry.0 as usize];
    let span = called.span;
    let ret = called.ret.clone();

    let init = declare_init(unit);

    // The parameters come first, because LIR's parameters *are* the leading
    // locals (§7). The two are C's own, in C's own order.
    let mut locals: Vec<Local> = Vec::new();
    let argc = push_local(&mut locals, argc_ty(), span);
    let argv = push_local(&mut locals, argv_ty(target), span);

    // The collector first, and before any of the program's code: `GC_INIT()` has
    // to happen on the main thread before the first allocation, and the call to
    // `#lang("start")` below is already the program's code.
    let mut stmts = vec![Stmt::new(
        StmtKind::Call {
            dest: None,
            callee: Callee::Static(init),
            args: Vec::new(),
        },
        span,
    )];

    // A program that claims `#lang("start")` starts itself: everything from here
    // is one call, with the program's `main` as a function pointer and the two
    // arguments the operating system passed.
    if let Some(start) = start {
        let status = push_local(&mut locals, status_ty(), span);
        let main = status_fn(unit, entry, span);
        stmts.push(Stmt::new(
            StmtKind::Call {
                dest: Some(Place::local(status)),
                callee: Callee::Static(start),
                args: vec![
                    Operand::Const(Constant::Func(main)),
                    Operand::Copy(Place::local(argc)),
                    Operand::Copy(Place::local(argv)),
                ],
            },
            span,
        ));
        push_entry(
            unit,
            locals,
            stmts,
            Terminator::new(TermKind::Return(Some(Operand::Copy(Place::local(status)))), span),
            span,
        );
        return;
    }

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

    push_entry(unit, locals, stmts, term, span);
}

/// The entry point itself, once its body is built.
fn push_entry(
    unit: &mut Unit,
    locals: Vec<Local>,
    stmts: Vec<Stmt>,
    term: Terminator,
    span: Option<FileSpan>,
) {
    unit.funcs.push(Function {
        name: NAME.to_string(),
        symbol: Symbol::new(SYMBOL),
        locals,
        params: 2,
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

/// The program's `main` as a `func () -> i32`, which is the one shape
/// `#lang("start")` can take a pointer to.
///
/// A `main` that already returns a status **is** that function, and is passed
/// as it stands. The other two shapes §5.6 allows get a wrapper, because the
/// difference between them is exactly the conversion the entry used to do
/// inline: nothing returned is a successful exit, and `never` does not come
/// back at all. The wrapper is where that conversion goes once the entry stops
/// performing it.
fn status_fn(unit: &mut Unit, entry: FuncId, span: Option<FileSpan>) -> FuncId {
    let ret = unit.funcs[entry.0 as usize].ret.clone();
    if ret == status_ty() {
        return entry;
    }
    let id = FuncId(unit.funcs.len() as u32);
    let mut locals: Vec<Local> = Vec::new();
    let mut stmts = Vec::new();
    let term = match &ret {
        // `-> never`: the call does not come back, so there is nothing after it.
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
        // A status of another width, converted — `CastKind` names the conversion
        // rather than leaving it to a backend.
        Ty::Int { .. } => {
            let raw = push_local(&mut locals, ret.clone(), span);
            let converted = push_local(&mut locals, status_ty(), span);
            stmts.push(Stmt::new(
                StmtKind::Call {
                    dest: Some(Place::local(raw)),
                    callee: Callee::Static(entry),
                    args: Vec::new(),
                },
                span,
            ));
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
            Terminator::new(
                TermKind::Return(Some(Operand::Copy(Place::local(converted)))),
                span,
            )
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
            Terminator::new(TermKind::Return(Some(Operand::int(0))), span)
        }
    };
    unit.funcs.push(Function {
        name: STATUS_NAME.to_string(),
        symbol: Symbol::new(STATUS_SYMBOL),
        locals,
        params: 0,
        ret: status_ty(),
        blocks: vec![Block {
            id: BlockId(0),
            stmts,
            term,
            label: Some("status".to_string()),
        }],
        extern_abi: None,
        span,
        attrs: FunctionAttrs::default(),
    });
    id
}

/// The declaration of `nest_init`, reusing one the program already has.
///
/// A program is free to declare `extern("c") nest_init :: func (...)` itself —
/// it is an ordinary C function — and two declarations of one symbol is a thing
/// a backend would have to resolve or reject. A program that declares it with a
/// *different* signature has declared a different function under one symbol,
/// which is the ordinary C hazard and not one this can see.
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

// ===< A test binary's entry (`--test`) >===

/// What the table of tests is called, in a dump and to the linker.
const TABLE_NAME: &str = "test.cases";
const TABLE_SYMBOL: &str = "_NEtests";

/// The function the entry calls instead of the program's `main`, and its symbol.
const TEST_NAME: &str = "test.main";
const TEST_SYMBOL: &str = "_NEtestmain";

/// Add the entry point of a **test binary**.
///
/// The shape is the ordinary one with a different middle: the C `main` is
/// unchanged, `#lang("start")` still gets the arguments, and what it is handed a
/// pointer to is a synthesized `func () -> i32` that calls the runner rather than
/// the program's own `main`. A test binary is a program like any other, and
/// everything that is true of starting one stays true.
///
/// `tests` is every `@test` function of the entry package, with the name it is
/// reported under. `runner` is whatever claimed `#lang("test_runner")`, and
/// `failed` whatever claimed `#lang("test_failed")` — the two halves of what a
/// test run *is*, which is why neither is written here.
#[allow(clippy::too_many_arguments)]
pub fn synthesize_tests(
    unit: &mut Unit,
    tests: &[Case],
    runner: FuncId,
    failed: Option<FuncId>,
    start: Option<FuncId>,
    target: Target,
    sources: &SourceMap,
) {
    let main = test_main(unit, tests, runner, failed, sources);
    synthesize(unit, main, start, target);
}

/// One test the entry point will run.
pub struct Case {
    /// What it is reported under: its canonical path.
    pub name: String,
    pub func: FuncId,
    /// The `#lang("test_result")` instantiation for this test's error type,
    /// when it returns a `Result` and `core` claims the item. It is what turns
    /// an `.err` into a failure that says what the error *was*.
    pub wrapper: Option<FuncId>,
}

/// `func () -> i32`: build the table, hand it to the runner, return its answer.
fn test_main(
    unit: &mut Unit,
    tests: &[Case],
    runner: FuncId,
    failed: Option<FuncId>,
    sources: &SourceMap,
) -> FuncId {
    // The **first test's** span, not the runner's. The unit split puts a
    // function where its span says it belongs (§11), and the runner lives in
    // whichever package claimed the tag — so taking its span would file the
    // table and the entry under `core` and leave the program's own unit without
    // the code that starts it.
    let span = tests
        .first()
        .and_then(|t| unit.funcs[t.func.0 as usize].span)
        .or(unit.funcs[runner.0 as usize].span);
    // The runner's own parameter says what a table of tests looks like: it takes
    // `[]Case`, so the slice type and the case type are read off the signature
    // rather than rebuilt from a shape this file would have to agree with.
    let slice_ty = unit.funcs[runner.0 as usize]
        .locals
        .first()
        .map(|l| l.ty.clone())
        .unwrap_or(Ty::Void);
    let case_ty = match &slice_ty {
        Ty::Named(id) => match unit.types[id.0 as usize].members.first().map(|m| &m.ty) {
            Some(Ty::Ptr(inner)) => (**inner).clone(),
            _ => Ty::Void,
        },
        _ => Ty::Void,
    };

    // Each case, as data: its name, and the `func () -> void` to call. A test
    // that returns a `Result` is not one of those, and gets a wrapper that turns
    // the `.err` it may return into the failure it means (see [`result_thunk`]).
    let mut elements = Vec::new();
    for test in tests {
        let call = match (unit.funcs[test.func.0 as usize].ret == Ty::Void, test.wrapper) {
            (true, _) => test.func,
            (false, Some(w)) => wrapper_thunk(unit, test.func, w, sources, &test.name),
            (false, None) => result_thunk(unit, test.func, failed, &test.name),
        };
        let bytes = bytes_global(unit, test.name.as_bytes(), span);
        elements.push(Constant::Aggregate(vec![
            Constant::Aggregate(vec![
                Constant::Global(bytes),
                Constant::Int((test.name.len() as i128).into()),
            ]),
            Constant::Func(call),
        ]));
    }
    let table = GlobalId(unit.globals.len() as u32);
    unit.globals.push(Global {
        name: TABLE_NAME.to_string(),
        symbol: Symbol::new(TABLE_SYMBOL),
        ty: Ty::Array {
            len: elements.len() as u64,
            elem: Box::new(case_ty),
        },
        init: Some(Constant::Aggregate(elements)),
        mutable: false,
        linkage: Linkage::Internal,
        span,
    });

    let id = FuncId(unit.funcs.len() as u32);
    let mut locals: Vec<Local> = Vec::new();
    let cases = push_local(&mut locals, slice_ty.clone(), span);
    let status = push_local(&mut locals, status_ty(), span);
    let mut stmts = Vec::new();
    // The slice, as a slice is: the table's address and how many are in it
    // (§7b). It is assembled in a local rather than passed as a constant because
    // an operand is a value and this is an aggregate.
    if let Ty::Named(slice_id) = slice_ty {
        stmts.push(Stmt::new(
            StmtKind::Assign {
                place: Place::local(cases),
                value: Rvalue::Aggregate {
                    kind: super::Aggregate::Struct(slice_id),
                    fields: vec![
                        Operand::Const(Constant::Global(table)),
                        Operand::int(tests.len() as i128),
                    ],
                },
            },
            span,
        ));
    }
    stmts.push(Stmt::new(
        StmtKind::Call {
            dest: Some(Place::local(status)),
            callee: Callee::Static(runner),
            args: vec![Operand::local(cases)],
        },
        span,
    ));
    unit.funcs.push(Function {
        name: TEST_NAME.to_string(),
        symbol: Symbol::new(TEST_SYMBOL),
        locals,
        params: 0,
        ret: status_ty(),
        blocks: vec![Block {
            id: BlockId(0),
            stmts,
            term: Terminator::new(
                TermKind::Return(Some(Operand::local(status))),
                span,
            ),
            label: Some("run the tests".to_string()),
        }],
        extern_abi: None,
        span,
        attrs: FunctionAttrs::default(),
    });
    id
}

/// A `func () -> void` around a test that returns `Result.<void, E>`, built out
/// of the `#lang("test_result")` instantiation for that `E`.
///
/// The thunk exists because the runner calls **one** shape of function and the
/// wrapper takes two arguments. Both are constants: the test's address, and a
/// `Location` built from the test's own span — so a failure reports the `@test`
/// line rather than the line inside `core` that raised it. Everything else
/// about failing, the message included, is written in Nest.
fn wrapper_thunk(
    unit: &mut Unit,
    test: FuncId,
    wrapper: FuncId,
    sources: &SourceMap,
    name: &str,
) -> FuncId {
    let span = unit.funcs[test.0 as usize].span;
    // The wrapper's own second parameter says what a `Location` is, the way the
    // runner's first says what a table of tests is: the type is read off the
    // signature rather than rebuilt from a shape this file would have to agree
    // with.
    let loc_ty = unit.funcs[wrapper.0 as usize]
        .locals
        .get(1)
        .map(|l| l.ty.clone())
        .unwrap_or(Ty::Void);
    let text_ty = match &loc_ty {
        Ty::Named(id) => unit.types[id.0 as usize]
            .members
            .first()
            .map(|m| m.ty.clone())
            .unwrap_or(Ty::Void),
        _ => Ty::Void,
    };
    let (file, line, column) = match span.and_then(|s| {
        sources
            .file(s.file)
            .map(|f| (f.name.clone(), f.line_col(s.span.start)))
    }) {
        Some((name, at)) => (name, at.line, at.column),
        None => (String::new(), 0, 0),
    };
    let bytes = bytes_global(unit, file.as_bytes(), span);

    let id = FuncId(unit.funcs.len() as u32);
    let mut locals: Vec<Local> = Vec::new();
    let path = push_local(&mut locals, text_ty.clone(), span);
    let at = push_local(&mut locals, loc_ty.clone(), span);
    let mut stmts = Vec::new();
    // The file name, then the location around it. Both are assembled in locals
    // for the reason the test table is: an operand is a value, and these are
    // aggregates (§7b).
    if let Ty::Named(text_id) = text_ty {
        stmts.push(Stmt::new(
            StmtKind::Assign {
                place: Place::local(path),
                value: Rvalue::Aggregate {
                    kind: super::Aggregate::Struct(text_id),
                    fields: vec![
                        Operand::Const(Constant::Global(bytes)),
                        Operand::int(file.len() as i128),
                    ],
                },
            },
            span,
        ));
    }
    if let Ty::Named(loc_id) = loc_ty {
        stmts.push(Stmt::new(
            StmtKind::Assign {
                place: Place::local(at),
                value: Rvalue::Aggregate {
                    kind: super::Aggregate::Struct(loc_id),
                    fields: vec![
                        Operand::local(path),
                        Operand::int(line as i128),
                        Operand::int(column as i128),
                    ],
                },
            },
            span,
        ));
    }
    stmts.push(Stmt::new(
        StmtKind::Call {
            dest: None,
            callee: Callee::Static(wrapper),
            args: vec![Operand::Const(Constant::Func(test)), Operand::local(at)],
        },
        span,
    ));
    unit.funcs.push(Function {
        name: format!("test.run({name})"),
        symbol: Symbol::new(&format!("_NEtestrun{}", id.0)),
        locals,
        params: 0,
        ret: Ty::Void,
        blocks: vec![Block {
            id: BlockId(0),
            stmts,
            term: Terminator::new(TermKind::Return(None), span),
            label: Some("run it".to_string()),
        }],
        extern_abi: None,
        span,
        attrs: FunctionAttrs::default(),
    });
    id
}

/// [`wrapper_thunk`]'s **fallback**: the same shape, for a `core` that claims no
/// `#lang("test_result")`.
///
/// It can say only *that* an error came back — it is built here, after
/// monomorphization, where there is no `Debug` left to reach for, which is the
/// whole reason the wrapper above is instantiated instead. Failing still goes
/// through Nest: whatever claimed `#lang("test_failed")`.
///
/// An enum is `{ tag, payload }` after §7b, so which variant it holds is member
/// zero, and which number `.ok` is comes from the type table rather than from an
/// assumption about declaration order.
fn result_thunk(unit: &mut Unit, test: FuncId, failed: Option<FuncId>, name: &str) -> FuncId {
    let span = unit.funcs[test.0 as usize].span;
    let ret = unit.funcs[test.0 as usize].ret.clone();
    let (tag_ty, ok_tag) = match &ret {
        Ty::Named(id) => {
            let def = &unit.types[id.0 as usize];
            let tag_ty = def
                .members
                .first()
                .map(|m| m.ty.clone())
                .unwrap_or(status_ty());
            let ok = match &def.origin {
                super::Origin::Enum { variants } => variants
                    .iter()
                    .find(|v| v.name.as_str() == "ok")
                    .map(|v| v.tag),
                _ => None,
            };
            (tag_ty, ok.unwrap_or(0))
        }
        _ => (status_ty(), 0),
    };

    let id = FuncId(unit.funcs.len() as u32);
    let mut locals: Vec<Local> = Vec::new();
    let result = push_local(&mut locals, ret, span);
    let tag = push_local(&mut locals, tag_ty.clone(), span);
    let call = vec![
        Stmt::new(
            StmtKind::Call {
                dest: Some(Place::local(result)),
                callee: Callee::Static(test),
                args: Vec::new(),
            },
            span,
        ),
        Stmt::new(
            StmtKind::Assign {
                place: Place::local(tag),
                value: Rvalue::Use(Operand::Copy(Place {
                    base: super::Base::Local(result),
                    projection: vec![super::Projection::Field {
                        index: 0,
                        name: Symbol::new("tag"),
                    }],
                })),
            },
            span,
        ),
    ];
    // Three blocks: the call and the test of its tag, the way out, and the
    // failure. The `.ok` arm is the *named* one because it is the one the type
    // table can name — every other tag is an error, whatever the enum calls it.
    let blocks = vec![
        Block {
            id: BlockId(0),
            stmts: call,
            term: Terminator::new(
                TermKind::Switch {
                    value: Operand::local(tag),
                    ty: tag_ty,
                    arms: vec![(ok_tag, BlockId(1))],
                    otherwise: BlockId(2),
                },
                span,
            ),
            label: Some("the result".to_string()),
        },
        Block {
            id: BlockId(1),
            stmts: Vec::new(),
            term: Terminator::new(TermKind::Return(None), span),
            label: Some("ok".to_string()),
        },
        Block {
            id: BlockId(2),
            stmts: failed
                .map(|f| {
                    vec![Stmt::new(
                        StmtKind::Call {
                            dest: None,
                            callee: Callee::Static(f),
                            args: Vec::new(),
                        },
                        span,
                    )]
                })
                .unwrap_or_default(),
            term: Terminator::new(TermKind::Return(None), span),
            label: Some("err".to_string()),
        },
    ];
    unit.funcs.push(Function {
        name: format!("test.wrap({name})"),
        symbol: Symbol::new(&format!("_NEtestwrap{}", id.0)),
        locals,
        params: 0,
        ret: Ty::Void,
        blocks,
        extern_abi: None,
        span,
        attrs: FunctionAttrs::default(),
    });
    id
}

/// The storage holding `bytes`, as its own read-only global.
fn bytes_global(unit: &mut Unit, bytes: &[u8], span: Option<FileSpan>) -> GlobalId {
    let id = GlobalId(unit.globals.len() as u32);
    unit.globals.push(Global {
        name: format!("test.name{}", id.0),
        symbol: Symbol::new(&format!("_NEtestname{}", id.0)),
        ty: Ty::Array {
            len: bytes.len() as u64,
            elem: Box::new(Ty::Int {
                bits: 8,
                signed: false,
            }),
        },
        init: Some(Constant::Bytes(bytes.to_vec())),
        mutable: false,
        linkage: Linkage::Internal,
        span,
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

