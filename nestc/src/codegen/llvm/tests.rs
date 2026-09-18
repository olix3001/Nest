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
    assert!(
        text.contains("define i32 @_NC3add(i32 %0, i32 %1)"),
        "{text}"
    );
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
    let mut session = Session::with_loader(Box::new(MemLoader::new().with(
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
    backend
        .emit_unit(program.unit(), OutputKind::Ir, &out)
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();

    assert!(
        !text.contains("with.overflow"),
        "a wrapping build checked:\n{text}"
    );
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
    assert!(
        text.contains("define i8 @_NC4less"),
        "a bool is not a byte:\n{text}"
    );
    assert!(
        text.contains("zext i1"),
        "the comparison was not widened:\n{text}"
    );
    // The comma matters: `alloca i1` is a prefix of `alloca i128`, and `core`
    // has a 128-bit local in it (`reflect.TypeId`).
    assert!(
        !text.contains("alloca i1,"),
        "an i1 reached a slot:\n{text}"
    );
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
        text.lines()
            .any(|l| l.contains("vtable") && l.contains("constant")),
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
    assert!(
        text.contains("icmp eq i32"),
        "the arms were not compared:\n{text}"
    );
    // No conditional branch anywhere: the switch is the only branching form.
    assert!(
        !text.contains("br i1"),
        "a second branching form appeared:\n{text}"
    );
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
        assert!(
            !session.has_errors(),
            "{path:?}: {:#?}",
            session.diagnostics
        );
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

/// **A program compiles, links and runs.**
///
/// Everything above this reads what the compiler produced; this runs it. The
/// whole path is here — the synthesized entry point (`lir::entry`) calling
/// `nest_init` and then the program's `main`, an object from this backend, the
/// C runtime, and the platform's linker — and the only thing asserted is what a
/// person at a shell would see: the process's exit status.
///
/// It is **skipped** when no runtime was built beside the compiler, which is the
/// one thing here that needs a C toolchain at build time.
#[test]
fn a_program_links_and_runs() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    // A status `main`, so the answer travels out through the process's exit
    // code; and a `void` one, which is a program that exits successfully.
    for (src, status) in [
        (
            "add :: func (a: i32, b: i32) -> i32 { return a + b }\nmain :: func () -> i32 { return add(2, 3) }\n",
            5,
        ),
        (
            "main :: func () { let mut n := 0\n  while n < 3 { n = n + 1 } }\n",
            0,
        ),
        // `value ; count` in both shapes: an aggregate over an array whose
        // length is in its type, and a `make` plus a loop over a slice whose
        // count is a run-time value. 7 + 5 + 3.
        (
            "main :: func () -> i32 {\n\
            \x20 let a: [4]i32 := .{ 7; 4 }\n\
            \x20 let n: usize := 3\n\
            \x20 let s: []i32 := .{ 5; n }\n\
            \x20 return a[2] + s[1] + cast.<i32>(s.len())\n\
             }\n",
            15,
        ),
        // An interpolated string, end to end: the lexer splitting it, the
        // parser keeping the pieces in order, desugaring turning each into a
        // `Display.display` call, and `core/fmt` writing the bytes.
        (
            "main :: func () -> i32 {\n\
            \x20 let w: i32 := 3\n\
            \x20 let h: i32 := 40\n\
            \x20 let s: str := f\"dim: {w}x{h + 2} {{ok}}\"\n\
            \x20 if s == \"dim: 3x42 {ok}\" { return 9 }\n\
            \x20 return 0\n\
             }\n",
            9,
        ),
        // Every `Display` impl `core` ships, including the two values that are
        // their own edge case: the signed minimum, whose magnitude the type
        // cannot hold, and zero.
        (
            "{ Display, start, end } :: import <core/fmt>\n\
             show :: func <T: Display> (v: T) -> str {\n\
            \x20 let mut b := start()\n\
            \x20 v.display(&mut b)\n\
            \x20 return end(&mut b)\n\
             }\n\
             main :: func () -> i32 {\n\
            \x20 let a: i32 := -2147483648\n\
            \x20 let b: u8 := 255\n\
            \x20 let c: usize := 0\n\
            \x20 let d: bool := false\n\
            \x20 let e: char := 'Z'\n\
            \x20 let mut ok: i32 := 0\n\
            \x20 if show(a) == \"-2147483648\" { ok = ok + 1 }\n\
            \x20 if show(b) == \"255\" { ok = ok + 2 }\n\
            \x20 if show(c) == \"0\" { ok = ok + 4 }\n\
            \x20 if show(d) == \"false\" { ok = ok + 8 }\n\
            \x20 if show(e) == \"Z\" { ok = ok + 16 }\n\
            \x20 return ok\n\
             }\n",
            31,
        ),
        // A method call on a literal receiver, through the three impl shapes:
        // concrete, family and blanket. The literal settles on its default
        // before the lookup runs, so each finds the impl a `let x: isize`
        // would. 7 + 11 + 23.
        (
            "A :: trait { a :: func (self: Self) -> i32 }\n\
             B :: trait { b :: func (self: Self) -> i32 }\n\
             C :: trait { c :: func (self: Self) -> i32 }\n\
             impl A for isize { a :: func (self: Self) -> i32 { return 7 } }\n\
             impl <const N: u16> B for int.<N> { b :: func (self: Self) -> i32 { return 11 } }\n\
             impl <T> C for T { c :: func (self: Self) -> i32 { return 23 } }\n\
             main :: func () -> i32 { return (5).a() + (5).b() + (5).c() }\n",
            41,
        ),
        // The C boundary: a libc call declared with `core/c`'s types, a
        // `c.ptr` built from a traced one, and a `c"..."` literal whose
        // trailing NUL is what `strlen` finds. 19 bytes written, and the
        // literal's 12 back through `from_cstr`.
        (
            "c :: import <core/c>\n\
             extern(\"c\") {\n\
            \x20 write :: func (fd: c.int, buf: c.ptr.<c.uchar>, n: c.size_t) -> c.ssize_t\n\
            \x20 strlen :: func (s: c.cstr) -> c.size_t\n\
             }\n\
             main :: func () -> i32 {\n\
            \x20 let s: str := \"write through libc\\n\"\n\
            \x20 let b: []u8 := s.as_bytes()\n\
            \x20 let n: c.ssize_t := write(1, c.from_ptr.<u8>(&b[0]), b.len())\n\
            \x20 let lit: c.cstr := c\"hello from C\"\n\
            \x20 if c.null.<u8>().is_null() == false { return 1 }\n\
            \x20 if c.from_cstr(lit) != \"hello from C\" { return 2 }\n\
            \x20 if strlen(lit) != 12 { return 3 }\n\
            \x20 return cast.<i32>(n)\n\
             }\n",
            19,
        ),
        // Reflection, at run time: a walk over a struct's members, each read
        // back through the checked read, and a `TypeId` that tells `usize`
        // from `u64` because the key it hashes keeps `distinct` where LIR
        // erases it. 7 + 11 + 100.
        (
            "r :: import <core/reflect>\n\
             P :: struct { x: i32, y: i64, tag: bool }\n\
             main :: func () -> i32 {\n\
            \x20 let p: P := P { x: 7, y: 11, tag: true }\n\
            \x20 let info: r.TypeInfo := r.type_info.<P>()\n\
            \x20 if info.members.len() != 3 { return 1 }\n\
            \x20 if info.size != 24 { return 2 }\n\
            \x20 if r.type_id.<usize>() == r.type_id.<u64>() { return 3 }\n\
            \x20 if r.type_id.<i32>() != r.type_id.<i32>() { return 4 }\n\
            \x20 let mut total: i32 := 0\n\
            \x20 let mut i: usize := 0\n\
            \x20 while i < info.members.len() {\n\
            \x20   let m: r.Member := info.members[i]\n\
            \x20   if m.name == \"x\" { total = total + r.member_read.<P, i32>(&p, m) }\n\
            \x20   if m.name == \"y\" { total = total + cast.<i32>(r.member_read.<P, i64>(&p, m)) }\n\
            \x20   if m.name == \"tag\" {\n\
            \x20     if r.member_read.<P, bool>(&p, m) { total = total + 100 }\n\
            \x20   }\n\
            \x20   i = i + 1\n\
            \x20 }\n\
            \x20 return total\n\
             }\n",
            118,
        ),
        // `Any`, over the `{ data, vtable }` pair that already existed: a
        // blanket impl answers `type_id_of` through the vtable the compiler
        // built for the trait object, and a downcast is that answer compared
        // against a constant. 12 + 5.
        (
            "r :: import <core/reflect>\n\
             { Any } :: import <core/reflect>\n\
             P :: struct { n: i32 }\n\
             main :: func () -> i32 {\n\
            \x20 let p: P := P { n: 12 }\n\
            \x20 let x: i32 := 5\n\
            \x20 let d: *dyn Any := &p\n\
            \x20 let e: *dyn Any := &x\n\
            \x20 if d.type_id_of() != r.type_id.<P>() { return 1 }\n\
            \x20 if r.downcast.<i32>(d).match { .some(_) => true, .none => false } { return 2 }\n\
            \x20 return r.downcast.<P>(d).!.*.n + r.downcast.<i32>(e).!.*\n\
             }\n",
            17,
        ),
        // A user-declared `@attribute`, read back off the descriptor it was
        // written on. No expansion pass and no generated code: the value is a
        // constant in read-only data and the member points at it.
        (
            "r :: import <core/reflect>\n\
             @attribute Json :: struct { rename: str, skip: bool }\n\
             P :: struct {\n\
            \x20 @Json(rename: \"user_id\", skip: false) id: i32,\n\
            \x20 n: i64,\n\
             }\n\
             main :: func () -> i32 {\n\
            \x20 let info: r.TypeInfo := r.type_info.<P>()\n\
            \x20 if info.members[0].attrs.len() != 1 { return 1 }\n\
            \x20 if info.members[1].attrs.len() != 0 { return 2 }\n\
            \x20 let j: Json := r.attr_of.<Json>(info.members[0].attrs).!\n\
            \x20 if j.rename != \"user_id\" { return 3 }\n\
            \x20 if j.skip { return 4 }\n\
            \x20 return 7\n\
             }\n",
            7,
        ),
        // Every `Kind` arm the compiler has a type for, a tuple's positions as
        // members named `0` and `1`, a `distinct` described as the declaration
        // with its representation's description in the payload, and a pointer
        // payload that leads back to the type it sits in.
        (
            "r :: import <core/reflect>\n\
             Box :: struct <T> { item: T, n: u8 }\n\
             Color :: enum { Red, Green }\n\
             Meters :: distinct f64\n\
             Node :: struct { v: i32, next: *Node }\n\
             k :: func (t: r.TypeInfo) -> i32 {\n\
            \x20 return t.kind.match {\n\
            \x20   .Void => 1, .Bool => 2, .Int => 3, .Uint => 4, .Float => 5, .Ptr(_) => 6,\n\
            \x20   .Slice(_) => 7, .Array(_, _) => 8, .Tuple => 9, .Struct => 10, .Enum => 11,\n\
            \x20   .Distinct(_) => 12, _ => 0,\n\
            \x20 }\n\
             }\n\
             main :: func () -> i32 {\n\
            \x20 if k(r.type_info.<void>()) != 1 { return 1 }\n\
            \x20 if k(r.type_info.<bool>()) != 2 { return 2 }\n\
            \x20 if k(r.type_info.<i16>()) != 3 { return 3 }\n\
            \x20 if k(r.type_info.<u8>()) != 4 { return 4 }\n\
            \x20 if k(r.type_info.<f32>()) != 5 { return 5 }\n\
            \x20 if k(r.type_info.<*i32>()) != 6 { return 6 }\n\
            \x20 if k(r.type_info.<[]u8>()) != 7 { return 7 }\n\
            \x20 if k(r.type_info.<[3]u16>()) != 8 || r.type_info.<[3]u16>().size != 6 { return 8 }\n\
            \x20 if k(r.type_info.<Color>()) != 11 { return 9 }\n\
            \x20 let b: r.TypeInfo := r.type_info.<Box.<i64>>()\n\
            \x20 if k(b) != 10 || b.members.len() != 2 || b.members[1].offset != 8 { return 10 }\n\
            \x20 let t: r.TypeInfo := r.type_info.<(i32, f64)>()\n\
            \x20 if k(t) != 9 || t.members.len() != 2 { return 11 }\n\
            \x20 if t.members[1].name != \"1\" || t.members[1].offset != 8 { return 12 }\n\
            \x20 let m: r.TypeInfo := r.type_info.<Meters>()\n\
            \x20 if k(m) != 12 || m.members.len() != 0 { return 13 }\n\
            \x20 if m.kind.match { .Distinct(i) => k(i.*) != 5 || i.size != 8, _ => true } { return 15 }\n\
            \x20 let node: r.TypeInfo := r.type_info.<Node>()\n\
            \x20 if node.members[1].kind.match { .Ptr(p) => p.id != node.id, _ => true } { return 16 }\n\
            \x20 if r.type_info.<[3]u16>().kind.match { .Array(n, e) => n != 3 || e.size != 2, _ => true } { return 17 }\n\
            \x20 return 42\n\
             }\n",
            42,
        ),
        // A trait's default body calling another method of the trait on
        // `self`: instantiated once per implementing type, so `self.put` is
        // that type's `put` — through a static call (10) and through a vtable
        // slot the default fills (5 + 5 + 1 more on the same accumulator).
        (
            "Sink :: trait {\n\
            \x20 put :: func (self: *mut Self, n: i64) -> i64\n\
            \x20 twice :: func (self: *mut Self, n: i64) -> i64 { self.put(n); return self.put(n) }\n\
             }\n\
             Acc :: struct { total: i64 }\n\
             impl Sink for Acc {\n\
            \x20 put :: func (self: *mut Self, n: i64) -> i64 { self.total = self.total + n; return self.total }\n\
             }\n\
             main :: func () -> i32 {\n\
            \x20 let mut acc: Acc := Acc { total: 0 }\n\
            \x20 let a: i64 := acc.twice(5)\n\
            \x20 let d: *mut dyn Sink := &mut acc\n\
            \x20 return cast.<i32>(a + d.twice(5) + d.put(1))\n\
             }\n",
            51,
        ),
        // `member_dyn`: a blanket impl that walks a type's members at run time
        // and hands each one to *its own* impl through a trait object — a
        // concrete one for `i32` and `u8`, the blanket one again for a nested
        // struct and a tuple. 1 + 20 + 3·10 + 40 + 5·10.
        (
            "r :: import <core/reflect>\n\
             Sum :: trait { sum :: func (self: *Self) -> i64 }\n\
             member_sum :: #intrinsic(\"member_dyn\") func <T> (v: *T, m: r.Member) -> *dyn Sum\n\
             impl Sum for i32 { sum :: func (self: *Self) -> i64 { return cast.<i64>(self.*) } }\n\
             impl Sum for u8 { sum :: func (self: *Self) -> i64 { return cast.<i64>(self.*) * 10 } }\n\
             impl <T> Sum for T {\n\
            \x20 sum :: func (self: *Self) -> i64 {\n\
            \x20   let t: r.TypeInfo := r.type_info.<T>()\n\
            \x20   let mut total: i64 := 0\n\
            \x20   let mut i: usize := 0\n\
            \x20   while i < t.members.len() {\n\
            \x20     total = total + member_sum.<T>(self, t.members[i]).sum()\n\
            \x20     i = i + 1\n\
            \x20   }\n\
            \x20   return total\n\
            \x20 }\n\
             }\n\
             Inner :: struct { a: i32, b: u8 }\n\
             Outer :: struct { x: i32, inner: Inner, y: (i32, u8) }\n\
             main :: func () -> i32 {\n\
            \x20 let o: Outer := Outer { x: 1, inner: Inner { a: 20, b: 3 }, y: .{ 40, 5 } }\n\
            \x20 return cast.<i32>(o.sum())\n\
             }\n",
            141,
        ),
        // An enum's variants, described: their names, their tags, whether the
        // payload was written positionally, and payload members that go into
        // the checked read exactly as a struct's do because their offsets are
        // from the start of the *value*. The indexes count across the whole
        // enum, which is what `member_dyn` over one needs. 7 + 35.
        (
            "r :: import <core/reflect>\n\
             E :: enum { None, Code(i32), Named { name: str, n: u8 } }\n\
             main :: func () -> i32 {\n\
            \x20 let info: r.TypeInfo := r.type_info.<E>()\n\
            \x20 if info.variants.len() != 3 { return 1 }\n\
            \x20 if info.variants[0].name != \"None\" { return 2 }\n\
            \x20 if info.variants[0].payload.len() != 0 { return 3 }\n\
            \x20 if info.variants[1].name != \"Code\" { return 4 }\n\
            \x20 if info.variants[1].tuple == false { return 5 }\n\
            \x20 if info.variants[2].tuple { return 6 }\n\
            \x20 if info.variants[2].payload.len() != 2 { return 7 }\n\
            \x20 if info.variants[2].payload[1].name != \"n\" { return 8 }\n\
            \x20 if info.variants[1].payload[0].index != 0 { return 9 }\n\
            \x20 if info.variants[2].payload[0].index != 1 { return 10 }\n\
            \x20 if info.variants[2].payload[1].index != 2 { return 11 }\n\
            \x20 if r.type_info.<i32>().variants.len() != 0 { return 12 }\n\
            \x20 let v: E := .Code(7)\n\
            \x20 if r.variant_tag.<E>(&v) != 1 { return 13 }\n\
            \x20 let got: r.Variant := r.variant_of.<E>(&v).!\n\
            \x20 if got.name != \"Code\" || got.index != 1 { return 14 }\n\
            \x20 return r.member_read.<E, i32>(&v, got.payload[0]) + 35\n\
             }\n",
            42,
        ),
        // `member_dyn` over an **enum**: the table it indexes holds one vtable
        // per payload member of every variant, flattened in variant order, so
        // the descriptor a `variant_of` handed back reaches its own impl. 3 + 4
        // through `u8`, then 5 through `i32`, which counts ten times.
        (
            "r :: import <core/reflect>\n\
             Sum :: trait { sum :: func (self: *Self) -> i64 }\n\
             member_sum :: #intrinsic(\"member_dyn\") func <T> (v: *T, m: r.Member) -> *dyn Sum\n\
             impl Sum for i32 { sum :: func (self: *Self) -> i64 { return cast.<i64>(self.*) * 10 } }\n\
             impl Sum for u8 { sum :: func (self: *Self) -> i64 { return cast.<i64>(self.*) } }\n\
             E :: enum { None, Code(i32), Pair(u8, u8) }\n\
             main :: func () -> i32 {\n\
            \x20 let v: E := .Pair(3, 4)\n\
            \x20 let va: r.Variant := r.variant_of.<E>(&v).!\n\
            \x20 let mut total: i64 := 0\n\
            \x20 let mut i: usize := 0\n\
            \x20 while i < va.payload.len() {\n\
            \x20   total = total + member_sum.<E>(&v, va.payload[i]).sum()\n\
            \x20   i = i + 1\n\
            \x20 }\n\
            \x20 let w: E := .Code(5)\n\
            \x20 let wa: r.Variant := r.variant_of.<E>(&w).!\n\
            \x20 return cast.<i32>(total + member_sum.<E>(&w, wa.payload[0]).sum())\n\
             }\n",
            57,
        ),
        // `Debug`, which every type has: concrete impls for the scalars and
        // text, and a blanket one that reads the description for everything
        // else — a struct by its members, a tuple by its positions, an enum by
        // the variant it holds, a `distinct` by what it is distinct from. The
        // quoting is the half `Display` does not do.
        (
            "fmt :: import <core/fmt>\n\
            { Debug } :: import <core/fmt>\n\
            E :: enum { None, Code(i32), Named { name: str, n: u8 } }\n\
            P :: struct { x: i32, s: str, inner: (u8, bool) }\n\
            Meters :: distinct i32\n\
            show :: func <T: Debug> (v: *T) -> str {\n\
            \x20   let mut b: fmt.Buf := fmt.start()\n\
            \x20   v.debug(&mut b)\n\
            \x20   return fmt.end(&mut b)\n\
            }\n\
            main :: func () -> i32 {\n\
            \x20   let a: E := .Code(7)\n\
            \x20   if show.<E>(&a) != \"E.Code(7)\" { return 1 }\n\
            \x20   let b: E := .None\n\
            \x20   if show.<E>(&b) != \"E.None\" { return 2 }\n\
            \x20   let c: E := .Named { name: \"hi\\n\", n: 3 }\n\
            \x20   if show.<E>(&c) != \"E.Named { name: \\\"hi\\\\n\\\", n: 3 }\" { return 3 }\n\
            \x20   let p: P := P { x: -2, s: \"a\\\"b\", inner: .{ 1, true } }\n\
            \x20   if show.<P>(&p) != \"P { x: -2, s: \\\"a\\\\\\\"b\\\", inner: (1, true) }\" { return 4 }\n\
            \x20   let m: Meters := cast.<Meters>(9)\n\
            \x20   if show.<Meters>(&m) != \"9\" { return 5 }\n\
            \x20   let ch: char := 'q'\n\
            \x20   if show.<char>(&ch) != \"'q'\" { return 6 }\n\
            \x20   let o: Option.<u8> := .some(4)\n\
            \x20   if show.<Option.<u8>>(&o) != \"Option.some(4)\" { return 7 }\n\
            \x20   return 42\n\
            }\n",
            42,
        ),
        // The write half of the checked read, `member_of`, an `@attribute` on
        // the *type* rather than a member, and a `*dyn reflect.Any` whose trait
        // is named only through the namespace. 40 + 2.
        (
            "r :: import <core/reflect>\n\
             @attribute Tag :: struct { n: i32 }\n\
             @Tag(n: 2)\n\
             P :: struct { x: i32, y: f64 }\n\
             Q :: struct { q: i8 }\n\
             main :: func () -> i32 {\n\
            \x20 let mut p: P := P { x: 1, y: 0.5 }\n\
            \x20 let y: r.Member := r.member_of.<P>(\"y\").!\n\
            \x20 r.member_write.<P, f64>(&mut p, y, 4.0)\n\
            \x20 if p.y != 4.0 { return 1 }\n\
            \x20 if r.member_of.<P>(\"w\").match { .some(_) => true, .none => false } { return 2 }\n\
            \x20 let a: *dyn r.Any := &p\n\
            \x20 if r.downcast.<Q>(a).match { .some(_) => true, .none => false } { return 3 }\n\
            \x20 let tag: Tag := r.attr_of.<Tag>(r.type_info.<P>().attrs).!\n\
            \x20 return cast.<i32>(r.downcast.<P>(a).!.*.y) * 10 + tag.n\n\
             }\n",
            42,
        ),
        // **A `for` loop runs its body.** `core`'s `Iterator` impls for a range
        // and for a slice were stubs answering `.none`, so every `for` in
        // every program ran zero times and said nothing — which is the worst
        // shape a bug can have. The element type is inferred from the *body*
        // here, which is what the single `impl <T: Step> Iterator for Range`
        // buys: a set of impls per integer family would have to choose one
        // before the body was read, and the literal would settle on `isize`.
        //
        // `250..=255` over a `u8` is the case the inclusive range is written
        // the way it is for: stepping past the last element would compute
        // `255 + 1` and trap. 6 + 6 + 12 + 100.
        (
            "{ make } :: import <core/mem>\n             main :: func () -> i32 {\n            \x20 let mut total: i32 := 0\n            \x20 for x in 0..<4 { total = total + x }\n            \x20 let mut n: usize := 0\n            \x20 for k in 1..=3 { n = n + k }\n            \x20 let xs: []mut i32 := make.<[]i32>(4)\n            \x20 for i in 0..<4 { xs[i] = cast.<i32>(i) * 2 }\n            \x20 let mut sum: i32 := 0\n            \x20 for v in cast.<[]i32>(xs) { sum = sum + v }\n            \x20 let mut top: u8 := 0\n            \x20 for b in 250..=255 { top = b }\n            \x20 for z in 5..<5 { sum = sum + 1000 }\n            \x20 if top != 255 { return 1 }\n            \x20 return total + cast.<i32>(n) + sum + 100\n             }\n",
            124,
        ),
        // **`.?` carries the error out.** The `from_residual` it desugars to is
        // a static trait call, and the generic arguments recorded for it were
        // the *trait's* — one — where `impl <T, E> FromResidual.<E> for
        // Result.<T, E>` has two. `T` was left unbound and the rebuilt error
        // lowered to `undef`, so every propagated error was garbage. Across two
        // different `Result` types, with a struct payload, which is the shape
        // `std` uses everywhere.
        (
            "E :: struct { op: str, code: i32 }\n             inner :: func () -> Result.<i32, E> { return .err(E { op: \"open\", code: 7 }) }\n             outer :: func () -> Result.<[]u8, E> {\n            \x20 let v: i32 := inner().?\n            \x20 return .ok(\"x\".as_bytes())\n             }\n             main :: func () -> i32 {\n            \x20 return outer().match {\n            \x20   .ok(_) => 0,\n            \x20   .err(e) => { if e.op != \"open\" { return 1 }; return e.code },\n            \x20 }\n             }\n",
            7,
        ),
        // `#comptime for`, unrolled: four copies of the body, each typed on its
        // own, and the loop variable a compile-time constant — which is what
        // lets `[i]u8` be a different type in each. 0+1+2+3, then 1+1+2+2+3+3.
        (
            "{ size_of } :: import <core/mem>\n\
             main :: func () -> i32 {\n\
            \x20 let mut total: i32 := 0\n\
            \x20 #comptime for i in 0..<4 { total = total + i }\n\
            \x20 let mut n: usize := 0\n\
            \x20 #comptime for k in 1..=3 {\n\
            \x20   let a: [k]u8 := .{ 0; k }\n\
            \x20   n = n + a.len() + size_of.<[k]u8>()\n\
            \x20 }\n\
            \x20 #comptime for z in 5..<5 { n = n + 1000 }\n\
            \x20 return total + cast.<i32>(n)\n\
             }\n",
            18,
        ),
        // A call through a bound on a method taking `self: *Self` reaches the
        // concrete impl rather than a blanket one at `T = *bool`, and the
        // `int.<N>` and `uint.<N>` impls of one trait are two functions rather
        // than one symbol. 4 + 16 + 32.
        (
            "Tag :: trait { tag :: func (self: *Self) -> i32 }\n\
             impl <T> Tag for T { tag :: func (self: *Self) -> i32 { return 1 } }\n\
             impl Tag for bool { tag :: func (self: *Self) -> i32 { return 4 } }\n\
             Sign :: trait { sign :: func (self: *Self) -> i32 }\n\
             impl <const N: u16> Sign for int.<N> { sign :: func (self: *Self) -> i32 { return 16 } }\n\
             impl <const N: u16> Sign for uint.<N> { sign :: func (self: *Self) -> i32 { return 32 } }\n\
             tag_of :: func <T: Tag> (x: T) -> i32 { return x.tag() }\n\
             sign_of :: func <T: Sign> (x: T) -> i32 { return x.sign() }\n\
             main :: func () -> i32 {\n\
            \x20 return tag_of(true) + sign_of(cast.<i8>(1)) + sign_of(cast.<u8>(1))\n\
             }\n",
            52,
        ),
        // A slice constant is a view of an array of its own, whatever its
        // elements are: text, numbers, or slices again. 2 + 3 + 6 + 3.
        (
            "NAMES: []str :: .{ \"ab\", \"cde\" }\n\
             NUMS: []i32 :: .{ 4, 5, 6 }\n\
             NESTED: [][]i32 :: .{ .{ 1 }, .{ 2, 3 } }\n\
             main :: func () -> i32 {\n\
            \x20 return cast.<i32>(NAMES.len() + NAMES[1].len()) + NUMS[2] + NESTED[1][1]\n\
             }\n",
            14,
        ),
        // An **anonymous struct** (§3.8), in every position one can be written:
        // an annotation, a parameter, a return type, a field of a named struct,
        // and none at all — `.{ ... }` with nothing to type it is a value of the
        // anonymous struct whose fields are the ones written. Plus both
        // conversions §3.8 names: the implicit anonymous→named one, and the
        // explicit `cast` back. 3 + 9 + 5 + 4 + 5 + 7.
        (
            "P :: struct { x: i32, y: i32 }\n\
             Box :: struct { inner: struct { n: i32 } }\n\
             take :: func (p: P) -> i32 { return p.x + p.y }\n\
             anon :: func (a: struct { x: i32, y: i32 }) -> i32 { return a.x - a.y }\n\
             ret :: func () -> struct { n: i32 } { return .{ n: 5 } }\n\
             main :: func () -> i32 {\n\
            \x20 let a := .{ x: 7, y: 2 }\n\
            \x20 let p: P := a\n\
            \x20 let q := cast.<struct { x: i32, y: i32 }>(p)\n\
            \x20 let b: Box := .{ inner: .{ n: 4 } }\n\
            \x20 let s: struct { x: i32, y: i32 } := .{ y: 2, x: 7 }\n\
            \x20 return take(.{ x: 1, y: 2 }) + take(p) + anon(q) + b.inner.n + ret().n + s.x\n\
             }\n",
            33,
        ),
        // **Writing through more than one level of indexing.** Reading
        // `g[1][2]` always worked and so did `g[1] = ...`; only the nested
        // write failed, because typing the inner index handed back the variable
        // its `Index.Output` projection would solve to and the write was sent
        // to `IndexMut` — which the built-in sequences deliberately do not
        // implement. Arrays three deep, slices of slices, and a read-modify-
        // write that is both sides at once. 9 + 4 + 3.
        (
            "{ make } :: import <core/mem>\n\
             main :: func () -> i32 {\n\
            \x20 let mut g: [2][2][2]i32 := .{ .{ .{ 0; 2 }; 2 }; 2 }\n\
            \x20 g[1][0][1] = 9\n\
            \x20 let mut s: []mut []mut i32 := make.<[][]mut i32>(2)\n\
            \x20 s[0] = make.<[]i32>(2)\n\
            \x20 s[1] = make.<[]i32>(2)\n\
            \x20 s[1][0] = 4\n\
            \x20 let mut a: [2][2]i32 := .{ .{ 0; 2 }; 2 }\n\
            \x20 a[0][1] = a[0][1] + 3\n\
            \x20 return g[1][0][1] + s[1][0] + a[0][1]\n\
             }\n",
            16,
        ),
        // **A bounded type parameter coerces to its trait object.** `&a` where
        // `impl T for A` always did; the same coercion from a parameter that the
        // trait bounds did not, because the search was for an *impl* and a
        // parameter has none — the bound is the promise that stands in for one,
        // and monomorphization builds a vtable per instantiation from it. Two
        // instantiations, and the `*mut X` direction. 3 + 40 + 40.
        (
            "T :: trait { v :: func (self: *Self) -> i32 }\n\
             call :: func (d: *dyn T) -> i32 { return d.v() }\n\
             wrap :: func <X: T> (x: *X) -> i32 { return call(x) }\n\
             mwrap :: func <X: T> (x: *mut X) -> i32 { return call(x) }\n\
             A :: struct { n: i32 }\n\
             B :: struct { m: i32 }\n\
             impl T for A { v :: func (self: *Self) -> i32 { return self.n } }\n\
             impl T for B { v :: func (self: *Self) -> i32 { return self.m * 10 } }\n\
             main :: func () -> i32 {\n\
            \x20 let a: A := .{ n: 3 }\n\
            \x20 let mut b: B := .{ m: 4 }\n\
            \x20 return wrap(&a) + wrap(&b) + mwrap(&mut b)\n\
             }\n",
            83,
        ),
        // **An associated type projected through a type parameter** (§5.4):
        // `T.Item`, which used to be "cannot resolve name". A bound's
        // associated types are parameters of the function too — one per
        // (parameter, associated type) — solved from the impl once a call site
        // says what `T` is, which is why two instantiations get two different
        // item types. `N.Inner.Item` is the same thing twice over, and the
        // pinned spelling `<Item = i32>` still means what it did. 30 + 4 + 21.
        (
            "Holder :: trait { Item :: type\n\
            \x20 get :: func (self: *Self) -> Self.Item }\n\
             Nest :: trait { Inner :: type: Holder\n\
            \x20 peel :: func (self: *Self) -> Self.Inner }\n\
             Ai :: struct { v: i32 }\n\
             Bu :: struct { v: u8 }\n\
             impl Holder for Ai { Item :: i32\n\
            \x20 get :: func (self: *Self) -> Self.Item { return self.v } }\n\
             impl Holder for Bu { Item :: u8\n\
            \x20 get :: func (self: *Self) -> Self.Item { return self.v } }\n\
             Outer :: struct { l: Ai }\n\
             impl Nest for Outer { Inner :: Ai\n\
            \x20 peel :: func (self: *Self) -> Self.Inner { return self.l } }\n\
             grab :: func <T: Holder> (t: *T) -> T.Item { return t.get() }\n\
             pinned :: func <T: Holder.<Item = i32>> (t: *T) -> i32 { return t.get() }\n\
             deep :: func <N: Nest> (n: *N) -> N.Inner.Item {\n\
            \x20 let mid := n.peel()\n\
            \x20 return mid.get()\n\
             }\n\
             main :: func () -> i32 {\n\
            \x20 let a: Ai := .{ v: 30 }\n\
            \x20 let b: Bu := .{ v: 4 }\n\
            \x20 let o: Outer := .{ l: .{ v: 21 } }\n\
            \x20 let x: i32 := grab(&a)\n\
            \x20 let y: u8 := grab(&b)\n\
            \x20 if pinned(&a) != 30 { return 1 }\n\
            \x20 return x + cast.<i32>(y) + deep(&o)\n\
             }\n",
            55,
        ),
        // **A repeat is a fill, not `n` operands.** `.{ 0; N }` used to be
        // written out element by element, which made the *compiler* do work
        // proportional to `N` — 50 000 took two seconds and a million never
        // finished. A uniform byte pattern is a `memset` and anything else is a
        // loop, both a fixed amount of LIR. The long array is here to be long;
        // the short ones check that the unrolled form still folds, and that a
        // repeat the fill cannot express (a four-byte element, a value that is
        // not a constant) still produces the right elements.
        (
            "seed :: func () -> i32 { return 5 }\n\
             main :: func () -> i32 {\n\
            \x20 let big: [100000]u8 := .{ 0; 100000 }\n\
            \x20 let ones: [64]u8 := .{ 1; 64 }\n\
            \x20 let wide: [100]i32 := .{ 3; 100 }\n\
            \x20 let run: [100]i32 := .{ seed(); 100 }\n\
            \x20 return cast.<i32>(big[99999]) + cast.<i32>(ones[63]) + wide[99] + run[99]\n\
             }\n",
            9,
        ),
        // `core/mem`'s bulk moves, and the `std` that is built on them: a
        // block copy between slices, a byte fill, and the `Vec` methods that
        // grow and copy through the same two instructions. 3 + 3 + 2 + 0.
        (
            "c :: import <std/collections>\n\
             m :: import <std/mem>\n\
             { make } :: import <core/mem>\n\
             main :: func () -> i32 {\n\
            \x20 let mut v := c.from_slice.<i32>(.{ 1, 2, 3 })\n\
            \x20 v.push(4)\n\
            \x20 v.extend(.{ 5, 6 })\n\
            \x20 let dst: []mut i32 := make.<[]i32>(3)\n\
            \x20 let n := m.copy.<i32>(dst, v.as_slice())\n\
            \x20 v.fill(2)\n\
            \x20 let mut z := c.from_slice.<i32>(.{ 9, 9 })\n\
            \x20 z.zero()\n\
            \x20 return dst[2] + cast.<i32>(n) + v.as_slice()[5] + z.as_slice()[1]\n\
             }\n",
            8,
        ),
    ] {
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

        let dir = std::env::temp_dir().join("nestc-link-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let stem = format!("{:x}.{}", hash(src), unique());
        let object = dir.join(format!("{stem}.o"));
        let exe = dir.join(&stem);

        let mut backend = LlvmBackend::default();
        backend.target_info(None).expect("the host resolves");
        backend
            .emit_unit(program.unit(), OutputKind::Object, &object)
            .unwrap_or_else(|e| panic!("emitting:\n{e}"));
        crate::codegen::link::link(
            &[object],
            &exe,
            &crate::codegen::link::LinkOptions::default(),
        )
        .unwrap_or_else(|e| panic!("linking:\n{e}"));

        let ran = std::process::Command::new(&exe).status().expect("it runs");
        assert_eq!(ran.code(), Some(status), "{src:?} exited {ran}");
    }
}

/// **However many codegen units, one object.**
///
/// A unit is a unit of work (§11) — four of them is how four cores compile a
/// program — and nothing downstream should have to learn that a program is four
/// files today and three tomorrow. The parts are emitted separately and merged
/// with a partial link, and the merged object is a real one: it links and runs.
#[test]
fn many_units_make_one_object() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let src = "add :: func (a: i32, b: i32) -> i32 { return a + b }\nmain :: func () -> i32 { return add(2, 3) }\n";

    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", src)));
    session.options.codegen_units = 4;
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
    assert!(program.units.len() > 1, "the split did not happen");

    // A directory of its own, because the assertion below is that **nothing
    // else** is in it: another test's artifacts in a shared one would read as
    // parts left behind.
    let stem = format!("{:x}.{}", hash(src), unique());
    // The process id is in the path because the directory outlives the run: the
    // same source hashes to the same name every time, and last run's executable
    // sitting there would read as a part left behind.
    let dir = std::env::temp_dir()
        .join(format!("nestc-merge-tests-{}", std::process::id()))
        .join(&stem);
    std::fs::create_dir_all(&dir).unwrap();
    let object = dir.join("prog.o");
    let exe = dir.join("prog");

    let mut backend = LlvmBackend::default();
    backend.target_info(None).expect("the host resolves");
    crate::driver::write_object(
        &mut backend,
        &program,
        Some(&object),
        "main.nest",
        true,
        &crate::codegen::link::LinkOptions::default(),
    )
    .unwrap_or_else(|e| panic!("emitting:\n{e}"));

    // One file, and the parts are not beside it.
    assert!(object.exists(), "no object");
    let strays: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "prog.o")
        .collect();
    assert!(strays.is_empty(), "the parts were left behind: {strays:?}");

    crate::codegen::link::link(
        &[object],
        &exe,
        &crate::codegen::link::LinkOptions::default(),
    )
    .unwrap_or_else(|e| panic!("linking:\n{e}"));
    let ran = std::process::Command::new(&exe).status().expect("it runs");
    assert_eq!(ran.code(), Some(5), "the merged object ran wrong: {ran}");
}

/// **The `std` floor, as one program** (`design/toolchain.md` step 6).
///
/// The step is *done when* a Nest program reads a file, writes to stdout,
/// spawns a process and reads its arguments and environment — so that is one
/// program, run, with its output and its status both checked.
///
/// Every layer is in it: `core/c`'s types and `c.to_cstr`, the `nest_open`
/// shim, `sys`'s `errno` reading, `io`'s `Write` impl and its buffer-free
/// `print`, `fs`'s whole-file read, `process`'s `fork`/`execvp`/`waitpid`, and
/// the arguments the synthesized entry point now takes from the startup and
/// hands to the runtime. A failure anywhere shows up here as a status naming
/// the check that failed, which is why each one returns its own number.
#[test]
fn the_std_floor_reads_writes_spawns_and_reads_its_arguments() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let dir = std::env::temp_dir().join("nestc-std-floor");
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = dir.join(format!("floor.{}.txt", unique()));
    // Left over from an earlier run with a different `open` would defeat the
    // point: the program creates this itself.
    let _ = std::fs::remove_file(&scratch);

    let src = format!(
        "\
io :: import <std/io>
fs :: import <std/fs>
process :: import <std/process>
s :: import <std/str>
col :: import <std/collections>

PATH_OF: str :: \"{path}\"

main :: func () -> i32 {{
  // Written, read back, and gone again.
  fs.write(PATH_OF, \"alpha\\nbeta\\n\".as_bytes()).match {{ .ok(_) => (), .err(_) => {{ return 1 }} }}
  fs.append(PATH_OF, \"gamma\\n\".as_bytes()).match {{ .ok(_) => (), .err(_) => {{ return 2 }} }}
  let text: str := fs.read_to_string(PATH_OF).match {{ .ok(t) => t, .err(_) => {{ return 3 }} }}
  if text != \"alpha\\nbeta\\ngamma\\n\" {{ return 4 }}
  if s.lines(text).len() != 3 {{ return 5 }}
  if fs.size(PATH_OF).match {{ .ok(n) => n, .err(_) => 0 }} != 17 {{ return 6 }}
  fs.remove(PATH_OF).match {{ .ok(_) => (), .err(_) => {{ return 7 }} }}
  if fs.exists(PATH_OF) {{ return 8 }}

  // A missing file is an error that says which file and why, not a trap.
  fs.read(\"/nest/no/such/file\").match {{
    .ok(_) => {{ return 9 }},
    .err(e) => {{ if e.not_found() == false {{ return 10 }} }},
  }}

  // Arguments: `argv[0]` plus the two this test passes.
  let args: []str := process.args()
  if args.len() != 3 {{ return 11 }}
  if args[1] != \"first\" {{ return 12 }}
  if args[2] != \"second\" {{ return 13 }}

  // The environment, read whole and by name.
  process.set_env(\"NEST_FLOOR\", \"set\").match {{ .ok(_) => (), .err(_) => {{ return 14 }} }}
  if process.env(\"NEST_FLOOR\").match {{ .some(v) => v, .none => \"\" }} != \"set\" {{ return 15 }}
  if process.env(\"NEST_DEFINITELY_UNSET\").match {{ .some(_) => true, .none => false }} {{ return 16 }}
  let mut seen: col.HashMap.<str, str> := col.map.<str, str>()
  for v in process.env_vars() {{ seen.insert(v.name, v.value).match {{ .some(_) => (), .none => () }} }}
  if seen.contains(\"NEST_FLOOR\") == false {{ return 17 }}

  // A child process, run to completion, and one that does not exist.
  let ran: process.Status := process.run(\"true\", .{{ }}).match {{ .ok(st) => st, .err(_) => {{ return 18 }} }}
  if ran.ok() == false {{ return 19 }}
  let missing: process.Status := process.run(\"nest_no_such_program\", .{{ }}).match {{
    .ok(st) => st, .err(_) => {{ return 20 }},
  }}
  if missing.code() != 127 {{ return 21 }}

  // And stdout, which is what the test reads back.
  io.println(\"the floor holds\")
  return 0
}}
",
        path = scratch.to_string_lossy()
    );

    // **The target has to be the host's**, and asking the backend is the only
    // way to know it: `Options::default` is a 64-bit Linux, which is a fallback
    // rather than an answer (`common::options`). Nothing above this test needed
    // the difference; `std` does, because `core/target.nest` is generated from
    // it and `std/libc` reads `OS` to decide which numbers this platform spells
    // its `open` flags with. Getting it wrong builds a program that opens files
    // with Linux's bits — on macOS, Linux's `O_APPEND` is `O_TRUNC`, so an
    // append emptied the file instead.
    let mut probe = LlvmBackend::default();
    let info = probe.target_info(None).expect("the host resolves");
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", &src)));
    session.options.target = info.target();
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

    let stem = format!("{:x}.{}", hash(&src), unique());
    let object = dir.join(format!("{stem}.o"));
    let exe = dir.join(&stem);
    let mut backend = LlvmBackend::default();
    backend.target_info(None).expect("the host resolves");
    backend.configure(&session.options);
    backend
        .emit_unit(program.unit(), OutputKind::Object, &object)
        .unwrap_or_else(|e| panic!("emitting:\n{e}"));
    crate::codegen::link::link(
        &[object],
        &exe,
        &crate::codegen::link::LinkOptions::default(),
    )
    .unwrap_or_else(|e| panic!("linking:\n{e}"));

    let ran = std::process::Command::new(&exe)
        .args(["first", "second"])
        .output()
        .expect("it runs");
    assert_eq!(
        ran.status.code(),
        Some(0),
        "exited {}, stderr: {}",
        ran.status,
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout),
        "the floor holds\n",
        "stdout"
    );
    // The program removed it; a leftover means `fs.remove` did nothing.
    assert!(!scratch.exists(), "the scratch file is still there");
}

/// Build `src` for the host, link it, run it, and hand back what it did.
///
/// The host target for the reason [`the_std_floor_reads_writes_spawns_and_reads_its_arguments`]
/// gives: anything reaching `std/libc` reads `OS` from it.
fn run_on_host(src: &str) -> std::process::Output {
    run_on_host_with(src, &[])
}

/// [`run_on_host`], as a **test binary**: the `@test` functions are what runs.
fn run_tests_on_host(src: &str) -> std::process::Output {
    run_on_host_in_mode(src, &[], &[], true)
}

/// [`run_on_host`], with `-C` settings applied on top of the host's.
fn run_on_host_with(src: &str, settings: &[(&str, &str)]) -> std::process::Output {
    run_on_host_in(src, settings, &[])
}

/// [`run_on_host_with`], with variables added to the program's environment.
fn run_on_host_in(
    src: &str,
    settings: &[(&str, &str)],
    env: &[(&str, &str)],
) -> std::process::Output {
    run_on_host_in_mode(src, settings, env, false)
}

/// [`run_on_host_in`], saying whether this is a `--test` build.
fn run_on_host_in_mode(
    src: &str,
    settings: &[(&str, &str)],
    env: &[(&str, &str)],
    test: bool,
) -> std::process::Output {
    let mut probe = LlvmBackend::default();
    let info = probe.target_info(None).expect("the host resolves");
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", src)));
    session.options.target = info.target();
    session.options.test = test;
    for (key, value) in settings {
        session.options.set(key, value).expect("a valid setting");
    }
    let file = session.load_entry("main").expect("entry loads");
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let layouts = crate::ir::layout::Layouts::new(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        session.options.target,
    );
    let tests = if session.options.test {
        session.entry_package_tests()
    } else {
        Vec::new()
    };
    let program = crate::lir::lower::lower_against_libraries(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        &layouts,
        &session.options,
        &session.lang_items,
        &session.sources,
        &tests,
        &|_| false,
    );

    let dir = std::env::temp_dir().join("nestc-host-runs");
    std::fs::create_dir_all(&dir).unwrap();
    let stem = format!("{:x}.{}", hash(src), unique());
    let object = dir.join(format!("{stem}.o"));
    let exe = dir.join(&stem);
    let mut backend = LlvmBackend::default();
    backend.target_info(None).expect("the host resolves");
    backend
        .emit_unit(program.unit(), OutputKind::Object, &object)
        .unwrap_or_else(|e| panic!("emitting:\n{e}"));
    crate::codegen::link::link(
        &[object],
        &exe,
        &crate::codegen::link::LinkOptions::default(),
    )
    .unwrap_or_else(|e| panic!("linking:\n{e}"));
    std::process::Command::new(&exe)
        .envs(env.iter().copied())
        .output()
        .expect("it runs")
}

// ===< The collector >===
//
// The programs are the ones in `examples/gc`, where each says what it checks and
// what its exit status means.

/// Allocating far more than fits in memory, with a little of it kept, leaves the
/// collector's heap small.
#[test]
fn memory_nothing_reaches_is_collected() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let out = run_on_host(include_str!("../../../../examples/gc/collects.nest"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "the heap grew past 64 MB: {out:?}"
    );
}

/// No allocation escape analysis frees is still reachable, however it left its
/// scope. Freed memory is poisoned, so reading it is seen every time.
#[test]
fn nothing_is_freed_while_something_still_reaches_it() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let out = run_on_host_in(
        include_str!("../../../../examples/gc/escapes.nest"),
        &[],
        &[("NEST_GC_POISON", "1")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the case numbered by the status read freed memory: {out:?}"
    );
}

/// A leaked object outlives everything that reached it, until it is dropped,
/// and dropping some leaked objects keeps the rest.
#[test]
fn a_leaked_object_lives_until_it_is_dropped() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let out = run_on_host(include_str!("../../../../examples/gc/leak.nest"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "a leaked node was collected: {out:?}"
    );
}

/// **Recursion too deep to fit is a trap, not a segmentation fault.**
///
/// The stack has an end, and reaching it used to be a `SIGSEGV` from whichever
/// instruction happened to touch the guard page — no message, and nothing
/// naming the runaway function. Every function that can reach itself now checks
/// its own frame against the floor the runtime recorded at startup, so the
/// failure arrives the way the language's other failures do. A recursion that
/// *does* fit is untouched, which is the half worth guarding: a check that
/// fired early would make the language's own depth limit smaller than the
/// platform's.
#[test]
fn recursion_past_the_end_of_the_stack_traps() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let deep = run_on_host(
        "f :: func (n: i32) -> i32 { if n == 0 { return 0 }; return f(n - 1) }\n\
         main :: func () -> i32 { return f(100000) }\n",
    );
    assert_eq!(
        deep.status.code(),
        None,
        "it exited instead of trapping: {deep:?}"
    );
    let said = String::from_utf8_lossy(&deep.stderr);
    assert!(
        said.contains("stack overflow"),
        "it did not say what happened: {said}"
    );

    let shallow = run_on_host(
        "f :: func (n: i32) -> i32 { if n == 0 { return 0 }; return f(n - 1) + 1 }\n\
         main :: func () -> i32 { return f(1000) - 990 }\n",
    );
    assert_eq!(
        shallow.status.code(),
        Some(10),
        "a recursion that fits ran wrong: {shallow:?}"
    );
}

/// **A test binary runs every test, and one failing test does not end the run.**
///
/// This is the whole of `@test` end to end: the attribute collected, the
/// functions kept, the table built, the runner called, a panic caught and the
/// next test started anyway. It is written as one program because that is the
/// only shape in which the last of those is a question — a suite whose second
/// test traps proves nothing about the third unless the third is there.
///
/// Both shapes a test may have are here (§5.6's neighbour rule, in
/// `ir::check::declarations`): one returning nothing and failing by trapping,
/// one returning a `Result` and failing by returning `.err`.
#[test]
fn a_test_binary_runs_every_test_and_survives_a_failure() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let ran = run_tests_on_host(
        "add :: func (a: i32, b: i32) -> i32 { return a + b }\n\
         @test\n\
         adds :: func () { assert(add(2, 3) == 5) }\n\
         @test\n\
         divides :: func () { let z: i32 := 0; assert(add(1, 1) / z == 0) }\n\
         @test\n\
         ok_result :: func () -> Result.<void, str> { return .ok(()) }\n\
         @test\n\
         err_result :: func () -> Result.<void, str> { return .err(\"nope\") }\n\
         main :: func () -> i32 { return 7 }\n",
    );
    let said = String::from_utf8_lossy(&ran.stderr);
    // Every one of them reported, the two that failed included — which is the
    // point: a trap in the second would otherwise have ended the process.
    for name in ["adds", "divides", "ok_result", "err_result"] {
        assert!(said.contains(&format!("test {name} ...")), "{said}");
    }
    assert!(said.contains("2 passed; 2 failed"), "{said}");
    // The failures said what they were, through the ordinary panic report.
    assert!(said.contains("division by zero"), "{said}");
    assert!(
        said.contains("the test returned an error: \"nope\""),
        "{said}"
    );
    // A failing suite is a failing process.
    assert_eq!(ran.status.code(), Some(1), "{ran:?}");
}

/// **A failing `Result` test says what the error was, and where the test is.**
///
/// The message is `Debug`'s, so an enum arrives as the variant it holds and
/// text arrives quoted; the location is the `@test` function's own, passed into
/// `#lang("test_result")` rather than filled in by `#caller_location`, which
/// would have named the line inside `core` that raised the panic.
#[test]
fn a_failing_result_test_reports_its_error() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let ran = run_tests_on_host(
        "E :: enum { NotFound(i32), Broken { why: str } }\n\
         @test\n\
         missing :: func () -> Result.<void, E> { return .err(.NotFound(7)) }\n\
         @test\n\
         broken :: func () -> Result.<void, E> { return .err(.Broken { why: \"no reason\" }) }\n\
         @test\n\
         text :: func () -> Result.<void, str> { return .err(\"nope\") }\n\
         main :: func () -> i32 { return 0 }\n",
    );
    let said = String::from_utf8_lossy(&ran.stderr);
    assert!(
        said.contains("the test returned an error: E.NotFound(7)"),
        "{said}"
    );
    assert!(
        said.contains("the test returned an error: E.Broken { why: \"no reason\" }"),
        "{said}"
    );
    assert!(
        said.contains("the test returned an error: \"nope\""),
        "{said}"
    );
    // The entry file, which is what the program was written in — not `core`.
    // `mem:` is the in-memory loader's prefix on a file's name.
    assert!(said.contains("\n  at mem:main:"), "{said}");
    assert!(!said.contains("test.nest"), "{said}");
    assert!(said.contains("0 passed; 3 failed"), "{said}");
    assert_eq!(ran.status.code(), Some(1), "{ran:?}");
}

/// A suite that passes exits `0`, and `main` is not what ran.
#[test]
fn a_passing_suite_exits_zero_and_does_not_run_main() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let ran = run_tests_on_host(
        "@test\n\
         passes :: func () { assert(1 == 1) }\n\
         main :: func () -> i32 { return 42 }\n",
    );
    let said = String::from_utf8_lossy(&ran.stderr);
    assert!(said.contains("1 passed; 0 failed"), "{said}");
    assert_eq!(ran.status.code(), Some(0), "{ran:?}");
}

/// **An ordinary build is unchanged**: `main` runs, and a panic still stops the
/// program rather than returning into a guard nothing armed.
#[test]
fn a_test_function_changes_nothing_about_an_ordinary_build() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let ran = run_on_host(
        "@test\n\
         never_runs :: func () { assert(false) }\n\
         main :: func () -> i32 { return 9 }\n",
    );
    assert_eq!(ran.status.code(), Some(9), "{ran:?}");
    assert!(String::from_utf8_lossy(&ran.stderr).is_empty());

    let failed = run_on_host("main :: func () -> i32 { assert(false); return 0 }\n");
    assert_eq!(
        failed.status.code(),
        None,
        "it exited instead of trapping: {failed:?}"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("assertion failed"),
        "{failed:?}"
    );
}

/// **Optimizing changes nothing a program does**: at every `opt-level`, and for
/// the processor compiling, the same output, and an overflow that traps at `0`
/// still traps — the checked arithmetic is not something LLVM may fold away.
#[test]
fn every_opt_level_runs_the_same_program() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let src = "io :: import <std/io>\n\
               process :: import <std/process>\n\
               sum :: func (n: i32) -> i32 {\n\
               \x20 let mut t: i32 := 0\n\
               \x20 for i in 0..<n { t = t + i }\n\
               \x20 return t\n\
               }\n\
               main :: func () -> i32 {\n\
               \x20 io.println(f\"sum={sum(10)}\")\n\
               \x20 let big: i32 := 2147483600 + cast.<i32>(process.args().len()) * 100\n\
               \x20 io.println(f\"big={big}\")\n\
               \x20 return 0\n\
               }\n";
    for level in ["0", "1", "2", "3", "s", "z"] {
        let ran = run_on_host_with(src, &[("opt-level", level), ("target-cpu", "native")]);
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            "sum=45\n",
            "at opt-level={level}"
        );
        assert!(
            !ran.status.success(),
            "the overflow trapped at opt-level={level}"
        );
    }
}

/// The types `std/serialize`'s tests write and read: every shape the blanket
/// impl walks by reflection, and every concrete impl beside it.
const SERIALIZE_TYPES: &str = r##"
io :: import <std/io>
string :: import <std/string>
{ String } :: import <std/string>
col :: import <std/collections>
{ Vec } :: import <std/collections>
{ rename, skip } :: import <std/serialize>
json :: import <std/serialize/json>
toml :: import <std/serialize/toml>

Id :: distinct usize
Server :: struct { host: str, port: u16, ratio: f64 }
Dep :: struct { name: String, optional: bool }
Config :: struct {
  title: String,
  @rename(name: "max-count") max: i64,
  @skip cache: i32,
  tags: Vec.<str>,
  server: Server,
  deps: Vec.<Dep>,
  maybe: Option.<i32>,
  nothing: Option.<i32>,
  pair: (i32, bool),
  id: Id,
  letter: char,
}

sample :: func () -> Config {
  let mut tags: Vec.<str> := col.new.<str>()
  tags.push("a")
  tags.push("b \"c\"\n")
  let mut deps: Vec.<Dep> := col.new.<Dep>()
  deps.push(Dep { name: string.from("core"), optional: false })
  deps.push(Dep { name: string.from("é☃"), optional: true })
  return Config {
    title: string.from("t"), max: -3, cache: 9, tags: tags,
    server: Server { host: "localhost", port: 8080, ratio: 0.1 },
    deps: deps, maybe: .some(4), nothing: .none, pair: (1, true),
    id: cast.<Id>(42), letter: 'λ',
  }
}
"##;

/// **A struct round-trips through JSON**, a renamed member is written under its
/// attribute's key and a skipped one not at all, and a malformed document is an
/// error that says where — not a trap.
#[test]
fn a_struct_round_trips_through_json() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let src = format!(
        "{SERIALIZE_TYPES}{}",
        r##"
main :: func () -> i32 {
  let c: Config := sample()
  let s: String := json.to_string(c).match { .ok(s) => s, .err(e) => { io.println(f"{e}"); return 1 } }
  io.println(s.as_str())
  let back: Config := json.from_str.<Config>(s.as_str()).match { .ok(v) => v, .err(e) => { io.println(f"{e}"); return 2 } }
  if back.cache != 0 { return 3 }
  if json.to_string(back).!.as_str() != s.as_str() { return 4 }
  io.println(json.to_string_pretty(back.server).!.as_str())

  // Unknown keys are skipped, escapes and surrogate pairs decode, an integer
  // reads as a float, and a missing `Option` is `.none`.
  let loose: str := "{\"extra\": {\"a\": [1, 2.5e3, null, true, \"x\"]}, \"title\": \"\\u00e9\\ud83d\\ude00\", \"max-count\": 1, \"tags\": [], \"server\": {\"host\": \"h\", \"port\": 1, \"ratio\": 2}, \"deps\": [], \"pair\": [0, false], \"id\": 7, \"letter\": \"x\"}"
  let l: Config := json.from_str.<Config>(loose).match { .ok(v) => v, .err(e) => { io.println(f"{e}"); return 5 } }
  io.println(json.to_string(l).!.as_str())

  let bad: []str := .{
    "{\"title\": \"t\",\n  \"tags\": [\"a\",]}",
    "{\"title\": \"t\"}",
    "{\"title\": 5}",
    "[1, 2",
  }
  for b in bad {
    json.from_str.<Config>(b).match { .ok(_) => { return 6 }, .err(e) => io.println(f"{e}") }
  }
  json.from_str.<Vec.<i32>>("[1, 2] x").match { .ok(_) => { return 8 }, .err(e) => io.println(f"{e}") }
  json.from_str.<Vec.<i32>>("[1, 2").match { .ok(_) => { return 9 }, .err(e) => io.println(f"{e}") }
  json.from_str.<Server>("{\"host\": \"h\", \"port\": 65536, \"ratio\": 1}").match {
    .ok(_) => { return 7 },
    .err(e) => io.println(f"{e}"),
  }
  return 0
}
"##
    );
    let ran = run_on_host(&src);
    let stdout = String::from_utf8_lossy(&ran.stdout);
    assert_eq!(
        ran.status.code(),
        Some(0),
        "exited {}, stdout:\n{stdout}",
        ran.status
    );
    assert_eq!(
        stdout,
        "{\"title\":\"t\",\"max-count\":-3,\"tags\":[\"a\",\"b \\\"c\\\"\\n\"],\"server\":{\"host\":\"localhost\",\"port\":8080,\"ratio\":0.1},\"deps\":[{\"name\":\"core\",\"optional\":false},{\"name\":\"é☃\",\"optional\":true}],\"maybe\":4,\"nothing\":null,\"pair\":[1,true],\"id\":42,\"letter\":\"λ\"}\n\
         {\n  \"host\": \"localhost\",\n  \"port\": 8080,\n  \"ratio\": 0.1\n}\n\
         {\"title\":\"é😀\",\"max-count\":1,\"tags\":[],\"server\":{\"host\":\"h\",\"port\":1,\"ratio\":2.0},\"deps\":[],\"maybe\":null,\"nothing\":null,\"pair\":[0,false],\"id\":7,\"letter\":\"x\"}\n\
         2:16: a trailing `,` in an array\n\
         1:15: missing key `max-count`\n\
         1:11: expected a string, found `5`\n\
         1:1: expected `{`, found `[`\n\
         1:8: expected the end of the document, found `x`\n\
         1:6: expected `,` or `]`, found the end of the document\n\
         1:28: 65536 does not fit in a `u16`\n"
    );
}

/// **A struct round-trips through TOML**: plain keys first, then `[tables]`,
/// then `[[arrays of tables]]`; a `.none` is left out and reads back as one;
/// and an error in the text says where in it, one in the values says which key.
#[test]
fn a_struct_round_trips_through_toml() {
    let Some(_) = crate::codegen::link::built_runtime() else {
        return;
    };
    let src = format!(
        "{SERIALIZE_TYPES}{}",
        r##"
main :: func () -> i32 {
  let c: Config := sample()
  let s: String := toml.to_string(c).match { .ok(s) => s, .err(e) => { io.println(f"{e}"); return 1 } }
  io.print(s.as_str())
  let back: Config := toml.from_str.<Config>(s.as_str()).match { .ok(v) => v, .err(e) => { io.println(f"{e}"); return 2 } }
  if back.cache != 0 { return 3 }
  if toml.to_string(back).!.as_str() != s.as_str() { return 4 }

  let doc: str := "# comment\ntitle = 'lit'\nmax-count = 0xf_f\ntags = [\n  \"x\", # here\n  \"y\",\n]\npair = [2, false]\nid = 1_000\nletter = \"\\u03BB\"\n\n[server]\nhost = \"h\"\nport = 1\nratio = 3\nextra.dotted = inf\n\n[[deps]]\nname = \"a\"\noptional = false\n"
  let d: Config := toml.from_str.<Config>(doc).match { .ok(v) => v, .err(e) => { io.println(f"{e}"); return 5 } }
  io.println("---")
  io.print(toml.to_string(d).!.as_str())
  io.println("---")

  let bad: []str := .{
    "title = \"x\"\ntitle = \"y\"\n",
    "[a]\nb = 1\n[a]\n",
    "n = 0x_ff\n",
    "s = \"open\n",
    "d = 1979-05-27\n",
    "title = \"x\"\nmax-count = 1\ntags = []\npair = [1, true]\nid = 1\nletter = \"l\"\n[server]\nhost = \"h\"\nport = 70000\nratio = 1.0\n",
    "title = \"x\"\nmax-count = 1\ntags = []\npair = [1, true]\nid = 1\nletter = \"l\"\n[server]\nhost = \"h\"\nport = 7\nratio = 1.0\n[[deps]]\nname = 3\n",
  }
  for b in bad {
    toml.from_str.<Config>(b).match { .ok(_) => { return 6 }, .err(e) => io.println(f"{e}") }
  }
  toml.to_string(c.tags).match { .ok(_) => { return 7 }, .err(e) => io.println(f"{e}") }
  return 0
}
"##
    );
    let ran = run_on_host(&src);
    let stdout = String::from_utf8_lossy(&ran.stdout);
    assert_eq!(
        ran.status.code(),
        Some(0),
        "exited {}, stdout:\n{stdout}",
        ran.status
    );
    assert_eq!(
        stdout,
        "title = \"t\"\n\
         max-count = -3\n\
         tags = [\"a\", \"b \\\"c\\\"\\n\"]\n\
         maybe = 4\n\
         pair = [1, true]\n\
         id = 42\n\
         letter = \"λ\"\n\
         \n\
         [server]\n\
         host = \"localhost\"\n\
         port = 8080\n\
         ratio = 0.1\n\
         \n\
         [[deps]]\n\
         name = \"core\"\n\
         optional = false\n\
         \n\
         [[deps]]\n\
         name = \"é☃\"\n\
         optional = true\n\
         ---\n\
         title = \"lit\"\n\
         max-count = 255\n\
         tags = [\"x\", \"y\"]\n\
         pair = [2, false]\n\
         id = 1000\n\
         letter = \"λ\"\n\
         \n\
         [server]\n\
         host = \"h\"\n\
         port = 1\n\
         ratio = 3.0\n\
         \n\
         [[deps]]\n\
         name = \"a\"\n\
         optional = false\n\
         ---\n\
         2:9: the key `title` is defined twice\n\
         3:4: the table `a` is defined twice\n\
         1:7: expected a digit, found `_`\n\
         1:10: a string with no closing `\"`\n\
         1:9: dates, times and other values are not supported\n\
         server.port: 70000 does not fit in a `u16`\n\
         deps[0].name: expected a string, found an integer\n\
         a TOML document is a table, and this is an array\n"
    );
}
