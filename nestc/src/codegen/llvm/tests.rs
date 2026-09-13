//! What the LLVM backend has to get right, checked against real modules.
//!
//! Each test compiles source the whole way — parse, infer, monomorphize, lay
//! out, lower, split, emit — and then reads the LLVM IR that came out. Reading
//! the text rather than the object is deliberate: a `.o` is only inspectable
//! with more tools, and every property below is one the IR states outright.
//!
//! **`module.verify()` runs on every emission** (see the parent module), so each
//! of these is also an assertion that LLVM accepts what was built. That is the
//! part that catches the mistakes worth catching — a type that disagrees with a
//! signature, a block with two terminators — and it is how the `*dyn Trait`
//! receiver bug was found.

use std::path::PathBuf;

use super::*;
use crate::codegen::Codegen;
use crate::sema::analyze;
use crate::sema::session::{MemLoader, Session};

/// Compile `src` and hand back the LLVM IR of its one unit.
fn ir(src: &str) -> String {
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", src)));
    let file = session.load_entry("main").expect("entry loads");
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let layouts = crate::ir::layout::Layouts::new(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        session.options.target,
    );
    let program = crate::lir::lower(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        &layouts,
        &session.options,
        &session.lang_items,
        &session.sources,
    );

    let mut backend = LlvmBackend::default();
    backend.target_info(None).expect("the host resolves");
    let dir = std::env::temp_dir().join("nestc-llvm-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let out: PathBuf = dir.join(format!("{:x}.{}.ll", hash(src), unique()));
    backend
        .emit_unit(program.unit(), OutputKind::Ir, &out)
        .unwrap_or_else(|e| panic!("emitting:\n{e}"));
    std::fs::read_to_string(&out).unwrap()
}

/// A name for a temporary file that two tests running at once cannot share.
///
/// The hash alone is not enough: two tests may compile the *same* source to
/// check two different things about it, and the test harness runs them on
/// different threads — so they would write and read one file and one of them
/// would see the other's truncation. The counter is what keeps them apart.
fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// **A function comes out, with its mangled symbol and its signature.**
///
/// The floor: if this fails nothing else here means anything.
#[test]
fn a_function_is_emitted_under_its_symbol() {
    let text = ir("@public add :: func (a: i32, b: i32) -> i32 { return a + b }\n");
    assert!(text.contains("define i32 @_NC3add(i32 %0, i32 %1)"), "{text}");
}

/// **A `-C overflow=trap` build really does check.**
///
/// The checked opcode is not a flag on an add (§7d) — it is LLVM's
/// `with.overflow` intrinsic, the pair it returns, and a branch on the second
/// member. Seeing the intrinsic is what says the trap survived to the machine.
#[test]
fn a_checked_add_is_llvms_overflow_intrinsic() {
    let text = ir("@public add :: func (a: i32, b: i32) -> i32 { return a + b }\n");
    assert!(
        text.contains("@llvm.sadd.with.overflow.i32"),
        "a trapping build emitted no overflow check:\n{text}"
    );
}

/// **A wrapping build emits the plain instruction, with no `nsw`.**
///
/// `add` is *defined* to wrap (§10), so the flag that would let LLVM assume the
/// overflow did not happen must not be set — that flag is exactly how a
/// wrapping program becomes undefined behaviour.
#[test]
fn a_wrapping_add_is_a_plain_add_with_no_overflow_flag() {
    let mut session =
        Session::with_loader(Box::new(MemLoader::new().with(
            "main",
            "@public add :: func (a: i32, b: i32) -> i32 { return a + b }\n",
        )));
    session.options.overflow = crate::common::options::OverflowMode::Wrap;
    let file = session.load_entry("main").expect("entry loads");
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let layouts = crate::ir::layout::Layouts::new(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        session.options.target,
    );
    let program = crate::lir::lower(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        &layouts,
        &session.options,
        &session.lang_items,
        &session.sources,
    );
    let mut backend = LlvmBackend::default();
    backend.target_info(None).unwrap();
    let dir = std::env::temp_dir().join("nestc-llvm-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("wrapping.ll");
    backend.emit_unit(program.unit(), OutputKind::Ir, &out).unwrap();
    let text = std::fs::read_to_string(&out).unwrap();

    assert!(!text.contains("with.overflow"), "a wrapping build checked:\n{text}");
    assert!(text.contains("add i32"), "{text}");
    assert!(
        !text.contains("add nsw") && !text.contains("add nuw"),
        "a wrapping add carries an overflow flag, which makes it undefined:\n{text}"
    );
}

/// **An aggregate is a packed struct whose padding is written out.**
///
/// LIR decided the layout; the type here has to *be* that layout rather than one
/// LLVM derived. Packed with explicit padding is what makes those the same
/// thing, and a `<{` in the text is how the IR says packed.
#[test]
fn an_aggregate_is_packed_with_its_padding_explicit() {
    let text = ir("\
P :: struct { a: u8, b: i32 }
@public get :: func (p: P) -> i32 { return p.b }
");
    assert!(text.contains("<{"), "an aggregate is not packed:\n{text}");
    // `a` at +0, three bytes of padding, `b` at +4 — the layout engine's answer,
    // reproduced exactly.
    assert!(
        text.contains("i8, [3 x i8], i32"),
        "the padding does not match the layout:\n{text}"
    );
}

/// **A member read is a byte offset, not a struct index.**
///
/// The `getelementptr i8` is the whole of the second design note in `unit.rs`:
/// one layout engine, and it already ran.
#[test]
fn a_member_read_is_a_byte_offset() {
    let text = ir("\
P :: struct { a: u8, b: i32 }
@public get :: func (p: P) -> i32 { return p.b }
");
    assert!(
        text.contains("getelementptr i8"),
        "a member read went through a typed index:\n{text}"
    );
}

/// **A `bool` is a byte in every slot.**
///
/// An `i1` in memory is the mistake this is about: it makes a struct member
/// sometimes one bit and sometimes one byte, and the wrong answer is an offset
/// rather than an error. The `i1` a comparison produces is widened at once.
#[test]
fn a_bool_is_a_byte_and_a_comparison_is_widened_into_one() {
    let text = ir("@public less :: func (a: i32, b: i32) -> bool { return a < b }\n");
    assert!(text.contains("define i8 @_NC4less"), "a bool is not a byte:\n{text}");
    assert!(text.contains("zext i1"), "the comparison was not widened:\n{text}");
    assert!(!text.contains("alloca i1"), "an i1 reached a slot:\n{text}");
}

/// **A `-> void` function returns nothing**, rather than an `undef` of a type no
/// machine has. §9 erases `void` from every slot, parameter and argument, and
/// the terminator is part of "every".
#[test]
fn a_void_function_returns_void() {
    let text = ir("@public nothing :: func (a: i32) -> void { let b := a + 1 }\n");
    assert!(text.contains("define void @_NC7nothing"), "{text}");
    assert!(text.contains("ret void"), "{text}");
}

/// **A dynamic dispatch passes the receiver's data pointer**, not the fat
/// pointer.
///
/// A `*dyn Trait` is `{ data, vtable }` and the slot's type is `func(*void, …)`.
/// Passing the pair would be a two-word argument against a one-word parameter —
/// which reads fine in a LIR dump and is the reason this test exists at the
/// level that can see it. LLVM's verifier is what caught it.
#[test]
fn a_dynamic_call_passes_the_data_pointer() {
    let text = ir("\
Describe :: trait { weight :: func (self: *Self) -> i32 }
Rock :: struct { kg: i32 }
impl Describe for Rock { weight :: func (self: *Rock) -> i32 { return self.kg } }
@public heft :: func (d: *dyn Describe) -> i32 { return d.weight() }
");
    // One `ptr` argument, from the vtable slot's own signature.
    assert!(
        text.contains("call i32 %") || text.contains("call i32 ("),
        "no indirect call:\n{text}"
    );
    assert!(
        !text.contains("call i32 %4(%\"*dyn Describe\""),
        "the fat pointer was passed whole:\n{text}"
    );
}

/// **A vtable is an immutable global of function pointers** (§7b), reached by an
/// ordinary member read — not a construct the backend has a case for.
#[test]
fn a_vtable_is_a_constant_global() {
    // The **coercion** is what builds one: a unit that only ever receives a
    // `*dyn Describe` names no concrete type and needs no table.
    let text = ir("\
Describe :: trait { weight :: func (self: *Self) -> i32 }
Rock :: struct { kg: i32 }
impl Describe for Rock { weight :: func (self: *Rock) -> i32 { return self.kg } }
@public heft :: func (r: *Rock) -> i32 {
  let d: *dyn Describe := cast.<*dyn Describe>(r)
  return d.weight()
}
");
    assert!(text.contains("vtable"), "no vtable at all:\n{text}");
    assert!(
        text.lines().any(|l| l.contains("vtable") && l.contains("constant")),
        "the vtable is not an immutable global:\n{text}"
    );
}

/// **Every branch is a `switch`** — one form for all of them, so there is no
/// `br`/`switch` pair to keep in step (§10).
///
/// Including the two-way ones: a match on integer literals is a *comparison
/// chain* (§4), so what comes out is `eq.i32` and then a switch on the `bool`
/// that produced — an `i8` here, since a `bool` is a byte. A one-bit switch is
/// fine on every target, and having no second form is the point.
#[test]
fn every_branch_is_a_switch() {
    let text = ir("\
@public pick :: func (a: i32) -> i32 {
  match a { 1 => { return 10 }, 2 => { return 20 }, _ => { return 0 } }
}
");
    assert!(text.contains("switch i8"), "{text}");
    assert!(text.contains("icmp eq i32"), "the arms were not compared:\n{text}");
    // No conditional branch anywhere: the switch is the only branching form.
    assert!(!text.contains("br i1"), "a second branching form appeared:\n{text}");
}

/// **A string's bytes are a private constant**, and a `str` is the header over
/// them (§9). Private because nothing can name it, so each unit gets its own
/// copy and the linker never sees it (§11).
#[test]
fn a_string_is_private_bytes_and_a_header() {
    let text = ir("@public greet :: func () -> str { return \"hi\" }\n");
    assert!(
        text.contains("private constant") && text.contains("c\"hi\""),
        "{text}"
    );
}

/// **The host resolves, and `--target` is honored.**
///
/// The second half is the one that matters: a backend that parsed the triple and
/// then emitted for the host would pass every other test in this file.
#[test]
fn a_target_triple_decides_the_module() {
    let mut backend = LlvmBackend::default();
    let host = backend.target_info(None).expect("the host resolves");
    assert!(matches!(host.pointer_bits, 32 | 64));
    assert!(!host.triple.is_empty());

    let mut backend = LlvmBackend::default();
    let wasm = backend
        .target_info(Some("wasm32-unknown-unknown"))
        .expect("wasm32 resolves");
    assert_eq!(wasm.pointer_bits, 32);
    assert_eq!(wasm.arch, "wasm32");
    assert_eq!(wasm.os, "none");
}

/// **A triple this compiler has no *name* for is refused**, even when LLVM knows
/// it perfectly well.
///
/// The names reach source: `core`'s generated `target.nest` declares
/// `ARCH: Arch :: .x86_64` and a program branches on it. Guessing one produces a
/// `core` that does not compile, which is a confusing way to find out that a
/// target is unsupported.
#[test]
fn a_triple_with_no_name_here_is_refused() {
    let mut backend = LlvmBackend::default();
    let err = backend
        .target_info(Some("powerpc64-unknown-linux-gnu"))
        .expect_err("this compiler has no `powerpc64`");
    assert!(matches!(err, CodegenError::Unsupported(_)), "{err:?}");
}

/// **Every example emits an object file.**
///
/// The failures worth catching are the ones a hand-written test does not
/// contain — a type only `core`'s `Result` reaches, an intrinsic one example
/// uses, a `void` member of a `ControlFlow.<void, T>`. Every one of the four
/// bugs this backend has found so far came from here rather than from the tests
/// above it.
#[test]
fn every_example_emits_an_object() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples");
    let out_dir = std::env::temp_dir().join("nestc-llvm-examples");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut emitted = 0;
    let mut pending: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(dir).expect("examples dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("nest") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let mut session = Session::new();
        let file = session
            .sources
            .add(path.to_string_lossy().into_owned(), src.clone());
        let (ast, errs) = crate::parser::parse::Parser::parse_file(&src, file);
        assert!(errs.is_empty(), "parse errors in {path:?}");
        session.asts.insert(file, ast);
        analyze(&mut session, file);
        assert!(!session.has_errors(), "{path:?}: {:#?}", session.diagnostics);
        let layouts = crate::ir::layout::Layouts::new(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            session.options.target,
        );
        let program = crate::lir::lower(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &layouts,
            &session.options,
            &session.lang_items,
            &session.sources,
        );

        let mut backend = LlvmBackend::default();
        backend.target_info(None).unwrap();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let out = out_dir.join(format!("{name}.o"));
        match backend.emit_unit(program.unit(), OutputKind::Object, &out) {
            Ok(()) => {
                assert!(out.exists(), "{name}: emitted nothing");
                emitted += 1;
            }
            // An intrinsic with no lowering yet. Named, not swallowed.
            Err(CodegenError::Unsupported(why)) => pending.push(format!("{name}: {why}")),
            Err(e) => panic!("{name}: {e}"),
        }
    }

    assert!(emitted > 0, "no example emitted an object");
    // **Empty.** Every example compiles, and an intrinsic that grows a hole
    // again is a test failure rather than a quiet entry on a list.
    assert!(
        pending.is_empty(),
        "some examples no longer emit:\n{}",
        pending.join("\n")
    );
}
