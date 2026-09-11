# Handoff: Phase 7 is done — layout, plus four fixes to phase 6

**Generated**: 2026-09-11
**Branch**: `main`
**Status**: **417 tests pass**, `cargo clippy` reports 73 warnings (the 84
baseline minus the dead-code entries these phases made live, plus three of the
same kind — items only the tests use). Every file in `examples/*.nest` compiles.

**Read `design/roadmap.md` §7 first**, then its **Open questions** section —
there is one decision there waiting on you. This document is the *state*.
**Phase 8 (LIR: shape and control flow) is next and is unblocked.**

## What is built (this session)

### Four fixes to phase 6, all with tests

- [x] **`[SIZE * 2]T` is a legal array length.** §3.2 says a length is a
      compile-time constant and an expression over constants is one. **Read the
      roadmap's Open questions section**: this was built differently from the
      note you left there, and the difference is yours to confirm.
- [x] **A generic with no finite set of instantiations is reported.**
      `grow.<T>` calling `grow.<Box.<T>>` used to run the compiler out of stack.
      There is now a depth budget (`mono::INSTANTIATION_DEPTH`, 64) and one
      diagnostic per declaration.
- [x] **A bound's trait arguments reach monomorphization.** `impl Conv.<i32> for
      Vec3` and `impl Conv.<bool> for Vec3` are coherent (§4.9) and both apply to
      `Vec3`; selection used to pick one silently.
- [x] **Two impls of one trait for one type no longer share a symbol.** Their
      members have the same name *and* the same canonical path, so the
      implemented trait is now part of the symbol (`_NC4Vec3XN4ConvIi32E2to`) and
      of the name (`Vec3.<as Conv.<i32>>.to`). `design/lir.md` §7 records the
      encoding.

### Phase 7 — layout

`nestc/src/ir/layout.rs` computes; `nestc/src/ir/check/layouts.rs` asks about
every declared type and stamps the answer. The **rules** are in
`design/lir.md` §7cc, and they are rules rather than derivations — a backend has
to agree with them.

- [x] **Sizes, alignments and offsets** for every shape: scalars, pointers,
      slices, arrays, tuples, structs, enums, `distinct`s.
- [x] **A query, not a pass.** Asked about a concrete `Ty`, memoized by
      `mono::type_key` — the mangled encoding, whose one job is injectivity, so
      two types share it precisely when they are the same type. `Ty` cannot be a
      key: a `const` argument may hold a float, so there is no `Hash`.
- [x] **`#packed` and `#align(N)`**, on a type *and* on a field. A member now
      carries its own directives into the IR; before this, `#align(8)` on a field
      was silently ignored. `#packed` / `#soa` on a field are rejected.
- [x] **`size_of` / `align_of` fold to constants**, including inside a generic
      `#const` function.
- [x] **The target comes back, here and only here.** A *pointer's* width is
      written nowhere in a type; `usize`'s is (phase 5). `layout` asks the
      target; everything else — the const evaluator included — asks `layout`.
- [x] **Layouts print in the IR dump** (`}  // size 12, align 4`), which is what
      the 22 regenerated snapshots assert.

## The three load-bearing decisions

### One arithmetic, two walks

`ir::const_eval::binary_values` / `int_binary` / `float_binary` / `unary_op` are
**free functions over `ConstValue`s** with no tree and no node behind them. The
IR evaluator calls them; so does inference's fold over the AST, which is what
makes `[SIZE * 2]T` work.

That shape is the whole point. `SIZE * 2` must not mean one thing in a type and
another in a value, so there is exactly one implementation of the semantics —
and two walks, because the trees genuinely differ and one runs before the other
exists. The error is a plain `String` for the same reason: the message about
`2 / 0` is the same everywhere, and only the *place* to report it differs.

### Layout is a query, and type definitions stay generic

`TypeDef`s are **definition-relative**: a field of `Pair.<T>` is a `T`, exactly
as lowering left it. Substituting the use site's arguments is layout's job.

That is why monomorphization instantiates *functions* and not types, and it is
what keeps one `Pair` in the program instead of one per instantiation. A generic
type is skipped by the stamping pass for the same reason `T` is: it has no
layout, and its instantiations do.

### The layout pass stamps and does not complain

It would be natural for it to report every type it cannot lay out. It does not,
because **every one of those is already reported** by the check that owns the
question: an unsized member by §3.4's rule, a cycle by
`declarations::recursive_layouts` (which now marks the types it rejected, so
layout stays quiet), an errored member by inference. A second diagnostic for one
mistake is what the whole diagnostic discipline here is arranged to avoid.

## Failed approaches (don't repeat these)

Everything in the previous handoffs' lists still stands. New this session:

- **Rooting monomorphization at `main`** — already recorded, and the regression
  test (`every_call_names_a_function_the_program_still_has`) is what caught it.
- **Reporting from the layout pass.** Built, and every message was a second one
  for a mistake already reported: `dyn Trait` by value, and a self-containing
  type. Both showed up as test failures asserting *exactly one* diagnostic — the
  guard working as designed.
- **`Const::Unevaluated(DefId)` for a computed array length.** It cannot work: a
  length is part of a type's identity (§3.2), so `[SIZE * 2]T` has to unify with
  `[8]T` *during* inference, and an unevaluated length unifies with nothing until
  after linking.
- **Suggesting "bind it to a `::` constant first" for a call in a length.** The
  first message said that; following the constant arrives right back at the call,
  so the advice was wrong. The message now states the ordering limit instead.
- **Putting a `Target` back into `ConstEval`.** Phase 5 took it out on purpose.
  What it needed was not the target but a *layout*, so it holds a `&Layouts` —
  the answerer, not the answer.
- **Reading `size_of.<T>()`'s type from the signature.** There is none:
  `func <T> () -> usize` mentions `T` nowhere. It comes from the call's
  `Instantiation`, which meant carrying that onto **intrinsic** nodes too — the
  intrinsic branch of `lower_call` returns early and was skipping it.
- **Binding only `const` generic arguments in the evaluator's frame.** A
  `size_of.<T>()` inside a generic `#const` body then asked about `T` itself.
  There is now a `ty_frames` stack beside `frames`, and a nested call resolves
  `T` against the frame it came from rather than passing the name on.
- **Assuming a member's directives reach the IR.** They did not:
  `Lowerer::member` built a `Member` with a type and a span and nothing else.

## Key decisions

| Decision | Rationale |
|---|---|
| One arithmetic, two walks | `SIZE * 2` must not mean two things. The trees differ and one runs first; the semantics do not and must not |
| A length is folded during inference | It is part of a type's identity, so it has to be a number before unification asks whether two types are the same |
| A **call** in a type is refused, not worked around | A body is not compiled until its types are known, and a length is one of them. Naming it through a constant does not help |
| Layout is a query, memoized by the mangled type encoding | "Every type" is not enumerable; injectivity is exactly what a cache key wants, and `Ty` has no `Hash` |
| Type definitions stay definition-relative | One `Pair` in the program, not one per instantiation — and it is why mono instantiates functions, not types |
| An integer's size rounds up to a power-of-two alignment, capped at 16 | `u24` has no three-byte load; `[N]u24` would otherwise have a stride nothing can use. The cap is because a 512-byte alignment for a `u4096` is absurd |
| `Layout::size` is the **stride** | Every consumer wants it; two numbers would mean every one of them choosing |
| Fields are never reordered | `#packed` is defined as removing *padding*, which only means something if the order is the written one |
| An enum's payload overlaps | One variant is live at a time; end-to-end would make an enum as big as all of them |
| The layout pass stamps and does not report | Every failure it can see is already reported by the check that owns the question |
| `#soa` warns rather than being ignored | A silently ignored directive is worse than an unimplemented one. It waits on what a place projection *is*, which is phase 8 |
| A trait impl's member carries its trait in the symbol | Two coherent impls for one type share a name and a path; without it they share a symbol |
| `ConstEval` holds a `&Layouts`, not a `Target` | Phase 5's line still holds: how wide a pointer is is layout's question alone |

## Current state

**Working**: everything. `cd nestc && cargo test` → **417 passed**. `cargo
clippy` → 73 warnings. Every file in `examples/*.nest` compiles clean. The
roadmap, `design/lir.md` §7 and §7cc, `spec/03-types.md` §3.2 and this document
all describe what is actually built.

**Broken**: nothing.

**Uncommitted changes**: none.

**Deliberately not done**:

- **`#soa`.** Warned about, not consumed. It waits on the LIR's place
  projection: storing `[N]Particle` column-wise means `&a[i]` no longer names a
  contiguous `Particle`, and what a projection *is* is phase 8's to say.
- **A call or `size_of` inside a type.** `[double(4)]T` and
  `[size_of.<H>()]u8` are both refused. Neither is a layout or a folding
  question — both want dependency-ordered analysis, which is a phase of its own.
  It is in the roadmap's Open questions.
- **ABI classification.** How a struct is *passed* — registers, stack, hidden
  pointer — is a different question from how it is stored. It belongs with §11.3
  and codegen.
- **Dead-code elimination** and **cross-compilation-unit generics**: unchanged
  from the phase 6 handoff.
- **Moving `+`, `-` and the rest out of `sema::builtins`** into `core` impls:
  unchanged, and still cleanup rather than a fix.

## Files to know

| File | Why it matters |
|---|---|
| `design/roadmap.md` §7, §8, **Open questions** | §7 is what was built; §8 is next; Open questions has one waiting on you. |
| `design/lir.md` §1, 2, 4 | Phase 8's brief: basic blocks, places, terminators, the `match` decision tree. |
| `design/lir.md` §7b, §7cc, §7c, §7d | Aggregate flattening, the layout rules, debug info, what a build setting changes. All four are phase 8 input. |
| `nestc/src/ir/layout.rs` | `Layouts::of` / `fields` / `enum_layout`, the rules, `subst_ty` (shared with the const evaluator). |
| `nestc/src/ir/check/layouts.rs` | The stamping pass, and why it reports nothing. |
| `nestc/src/ir/mono.rs` | Phase 6. `type_key` is here and layout uses it. |
| `nestc/src/ir/const_eval.rs` | `binary_values` and friends (the shared arithmetic, at the bottom of the file); `frames` / `ty_frames`; `$size_of`. |
| `nestc/src/sema/infer.rs` | `const_value_in` / `const_operand` (the AST fold), `Generics`, `Instantiation`, `ImplTarget`. |
| `nestc/src/common/options.rs` | `Target` — `pointer_bits` is what layout reads. |

## Code context

```nest
Header  :: #packed struct { magic: u32, len: u16, tag: u8 }   // size 7,  align 1
Padded  :: struct { a: u8, b: u32, c: u8 }                    // size 12, align 4
Wide    :: #align(16) struct { x: f32, y: f32 }               // size 16, align 16
Over    :: struct { a: u8, #align(8) b: u8, c: u8 }           // size 16, align 8
Colour  :: enum { red, green, blue }                          // size 1,  align 1
Payload :: enum { none, num(i64), pair(u8, u8) }              // size 16, align 8

SIZE: usize :: 4
buf :: func (x: [SIZE * 2]i32) -> usize { return x.len() }    // x: [8]i32
```

```rust
// ir/layout.rs
pub struct Layout { pub size: u64, pub align: u64 }   // `size` is the stride
pub struct Fields { pub layout: Layout, pub offsets: Vec<u64> }
pub struct EnumLayout { pub layout: Layout, pub tag: Layout, pub payload_at: u64,
                        pub payload: Layout, pub variants: Vec<Fields> }
impl Layouts<'_> {
    pub fn of(&self, ty: &Ty) -> Result<Layout, LayoutError>;
    pub fn fields(&self, ty: &Ty) -> Option<Result<Fields, LayoutError>>;
    pub fn enum_layout(&self, ty: &Ty) -> Option<Result<EnumLayout, LayoutError>>;
}

// ir/const_eval.rs — the shared arithmetic, no tree behind it
pub fn binary_values(op: BinOp, a: &ConstValue, b: &ConstValue) -> Result<ConstValue, String>;
pub fn unary_op(op: UnOp, v: &ConstValue) -> Result<ConstValue, String>;
```

**The non-obvious bits.**

*A `Layout` is stamped on a `TypeDef`'s `IrId`, and the per-file `Program`s share
those ids with `Linked`* — so the IR dump shows layouts even though it renders
what lowering produced. That is the documented behaviour of `link` (it clones,
preserving ids), and the const-value comments in the same dumps have always
worked the same way.

*`layout::subst_ty` and `mono`'s are different functions on purpose.* Mono's also
substitutes `const` parameters into widths and array lengths; layout's has no
const map and does not need one. Do not "unify" them without giving layout the
map.

*The layout pass runs **last** in `check::run`.* Laying out a cycle does not fail,
it does not terminate, so `declarations::recursive_layouts` has to have run and
marked its types first.

*`#align` on a member is read from `meta.directives(member.id)`*, which only
exists because `Lowerer::member` now copies the field def's directives. Anything
else that wants a member's directives gets them the same way.

## Resume instructions

1. `cd nestc && cargo test` — expect **417 passed**.
2. See the phase working:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
   { size_of, align_of } :: import <core/mem>
   Header  :: #packed struct { magic: u32, len: u16, tag: u8 }
   Padded  :: struct { a: u8, b: u32, c: u8 }
   Payload :: enum { none, num(i64), pair(u8, u8) }
   SIZE: usize :: 4
   A: usize :: size_of.<Header>()
   B: usize :: size_of.<Payload>()
   C: usize :: align_of.<Padded>()
   buf :: func (x: [SIZE * 2]i32) -> usize { return x.len() }
   @public main :: func () { const z := A }
   EOF
   ./target/debug/nestc /tmp/e.nest | grep -E 'size |= [0-9]|func buf'
   ```
   Expected: `Header` 7/1, `Padded` 12/4, `Payload` 16/8, `A = 7`, `B = 16`,
   `C = 4`, and `buf(x: [8]i32)`.
3. **Answer the one open question** in `design/roadmap.md` — how `[SIZE * 2]T`
   was built versus the note you left.
4. **Phase 8 (LIR: shape and control flow) is next.** `design/roadmap.md` §8 is
   the plan and `design/lir.md` §1, 2, 4, 7b is the content: basic blocks,
   places, terminators, the `match` decision tree, aggregates flattened to
   structs. Two things already in hand that it consumes: every function has a
   symbol (phase 6) and every concrete type has a layout (phase 7). One thing it
   has to decide before `#soa` can be consumed: what a place projection through a
   column-wise array *is*.
5. Whatever you touch, verify with all three:
   - `cargo test` (417 and rising)
   - `for f in ../examples/*.nest; do ./target/debug/nestc "$f" >/dev/null || echo "FAIL $f"; done`
   - `cargo clippy` — compare the warning **set**, not the count.

## Edge cases and known limits

Everything in the previous handoff's list still holds. New or changed:

- **A `const` generic argument takes a literal or a named constant, not an
  expression.** `repeat.<N * 2>` does not parse, and that is the **grammar**:
  inside `.<...>` a `>` is the closing bracket. An array length has no such
  problem because `]` closes it. Rust solves this with braces
  (`foo::<{ N * 2 }>()`); Nest has not decided.
- **`[N * 2]T` with `N` a `const` generic parameter is refused.** `[N]T` is fine.
  A `Const` has no shape for an unevaluated expression, so the combined form
  would have to stay symbolic to monomorphization — and it says so rather than
  guessing.
- **`int.<N>` inside a family impl has no layout**, and the stamping pass skips
  it along with every other generic. The width is a `const` parameter, so the
  type is as generic as one over a `T`.
- **A zero-sized type is a real thing.** `void` and `never` are size 0, align 1,
  and a member of one costs nothing and keeps its name.
- **`f80` is size 16, align 16.** Ten bytes of data and six of padding, which is
  what every ABI that has the type does. What a *slot* costs is what a layout is.
- **`mono::type_key` is public** and is the key for anything keyed by a type.
  Prefer it over a display string, which is not injective — two types in
  different namespaces can print alike.

## Warnings

- **Run everything from `nestc/`.** `packages/` and `examples/` are one level up.
  A `cd` inside a compound Bash command silently changes the working directory
  for later calls — use absolute paths.
- **`cargo test` does not rebuild `target/debug/nestc`.** Run `cargo build`
  before the examples loop or you are testing a stale binary.
- **Compare the clippy warning *set*, not the count.** `cargo clippy
  --message-format=short 2>&1 | grep -E '^src|^warning: ' | sed
  's/:[0-9]*:[0-9]*:/:/' | sort`, then `comm` against the baseline. Note the
  redirection: `2>&1 | grep`, **not** `2>&1 > file` — the second sends clippy's
  stderr to the terminal and leaves you an empty file and a clean-looking diff.
- **`cargo clippy` caches.** `touch` the file you changed, or you will compare
  against a stale run.
- **`rustfmt --edition 2024 <file>` follows `mod` declarations** and reformats
  every file it reaches. **Never** run `cargo fmt` with no arguments. Note
  `src/sema/infer.rs`, `lower.rs`, `session.rs` and `tests.rs` were *already*
  unformatted before any of this work; leave them.
- **Regenerating snapshots**: `INSTA_UPDATE=always cargo test`, then
  `rm -f src/*/snapshots/*.snap.new` and
  `sed -i '' '/^assertion_line: /d' src/*/snapshots/*.snap`. Then **read every
  diff** — against `git`, not against `.snap.new`.
- **Many tests assert *exactly one* diagnostic.** Deliberate: the regression
  guard against cascades. Fix the cascade rather than loosening the assertion.
  Both layout cascades this session were caught by exactly that.
- **A new intrinsic is a row in `sema/intrinsics.rs` first.**
- **`#lang` discovery is by tag only.** Never hardcode a core type's name, file
  or position.
- **Nest syntax worth restating**: `f :: func () { … }`, not `func f() { … }`;
  `let x: T := v`; a typed constant is `A: u8 :: 5` (the type goes **before** the
  binder); a directive is its own line above the binding (`#const\nf :: func …`);
  an `extern` function is `puts :: extern("c") func (…)`; an array literal is
  `[_]T { a, b }`; match arms are comma-separated.
- **Comments explain *why*, at length, and cite spec sections inline.**

## User notes

- Commits are **title-only**: no body, no co-author trailer, changes bundled as
  `feat: a + feat: b + fix: c`.
- The design moves. Commit green states often so a redesign costs one commit,
  not a session.
- When a note in `design/roadmap.md` is departed from, say so *there* and in the
  reply — the `[SIZE * 2]T` row is the worked example.
