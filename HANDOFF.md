# Handoff: six bugs under `twig`, and `twig` itself not started

**Generated**: 2026-09-14
**Branch**: `main` (ahead 39)
**Status**: **589 tests pass** without LLVM, **606** with it. `cargo build`
reports 48 warnings (46 with `--features llvm`), all dead-code-shaped —
unchanged. **Nothing is uncommitted.**

**Read `design/toolchain.md`** — its ten-step order of work is the plan, and it
is **unchanged this session**: steps 1–6 are done, 7 through 10 are not. You
asked for **step 9, `twig`**, out of order — skipping `std/json` (7) and the
library format (8). **No `twig` code exists yet.** What this session produced is
the two commits below, which are the language bugs that writing `twig`'s data
model immediately hit.

## What happened this session

Two commits, six bugs, no feature. The probing was ordinary — an enum with
payloads, a `Vec` of structs, a `[]str`, a qualified name — and five of the six
were found in the first twenty minutes of it. **`std` was written almost
entirely in unqualified, same-file style**, so the qualified-name paths below had
never been exercised by anything.

### The failure mode five of them shared

`Ty::Error` unifies with everything. A path that *produces* one without
reporting leaves a signature that type-checks against nothing, and the first
thing to object is the **backend**:

```
nestc: llvm: main: Void is not a type a value can have
```

No span, no source line, and **named in the function that *called* the one with
the mistake in it**. Every silent error type in this compiler surfaces exactly
that way, which is why they were hard to find and why the backstop below exists.

### Commit 1 (`8e9ab2c`) — a name in type position that is not a type

An `import` binding is an ordinary name, so it **shadows**:

```nest
str :: import <std/str>
show :: func (v: i32) -> str { … }   // `str` is the namespace, not the type
```

`typepath_ty`'s catch-all was `_ => Ty::Error`, silent. It now reports, once per
written name (`TyPathReported`, the same dedup as `ConstSlotReported` — a type
node is resolved once per *use*, so one written name would otherwise be as many
diagnostics as the program has uses):

```
error: `str` is a namespace, not a type
  = note: a binding of `str` shadows any type of that name — bind the import
          under another name to write both
```

- The message quotes the path the **program wrote** (`written_path_in`), not the
  def's own name — the latter printed `std`, a package the program never named.
- `DefKind::External` stays silent: a member of a deliberately-unloaded package
  is not something we know is not a type.

### Commit 2 (`da00e40`) — five more, and the backstop

| # | Bug | Why it happened |
|---|---|---|
| 1 | **`ns.P { … }` was a silent error type** | A composite literal's head is parsed as an **expression** — the tuple-struct form `Type(a, b)` is syntactically a `Call` — so a qualified type name arrives as a `FieldAccess`, not a `Path`. Resolution got it right; `ty_from_node_in` had no arm for it |
| 2 | **A qualified trait in a bound resolved and then could not be called** | `func <T: cmp.Eq> … a.eq(b)` → "no method `eq` on `T`". Impl selection draws from the traits a **file** can name (`in_scope_traits`), and a bound names one without importing it |
| 3 | **`f(x) n = 1` was rejected** | `scan_binding_kind` looks ahead to the end of the **line**, and a line holds several statements — so an `=` belonging to the *next* statement classified the first as an assignment, and `parse_assign` then reported "expected an assignment operator" at `n` |
| 4 | **`f() = 3` was accepted** | `mutability.rs` asked whether the place was *writable* and never whether it was a place. The IR carried an `Assign` whose left side was a call, and the program compiled and ran |
| 5 | **`defer` re-evaluated its body at every exit** | §8.4 says it captures where it is written. It did not, so a deferred call read whatever its operands held *then* — the opposite of what the construct is for, and silently wrong rather than an error |

**Bug 2 is what the previous handoff recorded as "a trait must be imported by
name for its impls to apply."** That diagnosis was wrong. `{ Eq } :: import
<core/cmp>` did not fix the case it was written from — bug 1 did, because the
*receiver* `(p.P { x: 1 })` was untyped and the trait had nothing to do with it.
Both are now fixed and the item is off the list.

**Where each fix lives:**

- 1: `sema/infer.rs` — a `NodeKind::FieldAccess` arm in `ty_from_node_in`,
  straight through `typepath_ty`, which reads only the resolved def.
- 2: `sema/infer.rs` — `bound_traits`, walking the file's `NodeKind::Bounds` and
  extending `in_scope_traits`. Per file, because selection is per file.
- 3: `parser/item.rs` — `parse_assign` answers `(NodeId, bool)` and falls back to
  the expression it already parsed when no operator follows.
- 4: `ir/check/mutability.rs` — `is_place` in front of the permission question.
- 5: `sema/desugar.rs` — `capture_defers`, on the **block**, not the `defer`.

### `defer`, precisely

```
defer log(n)        →     __defer1 :: n
                          defer log(__defer1)
```

Two things about it are deliberate and neither is a stopgap:

- **The bindings land in the enclosing block, in front of the `defer`.** Wrapping
  the `defer` in a block of its own would be a scope that ends immediately, which
  runs the body there.
- **The receiver is not captured.** Desugar runs **before inference**, so it
  cannot see whether `x.close()` takes `self` by value or as a `*mut Self`, and
  binding a mutating method's receiver to a copy would silently defer the
  mutation to a temporary. Reading the place at exit is what a captured
  *pointer* would have done anyway; the two differ only for a receiver
  reassigned between the `defer` and the exit. Doing better means capturing in
  **sema lowering**, where the types are known — and `Lowerer` holds `defs:
  &DefTable`, immutable, so that is a plumbing job, not a rewrite.

It changed one existing test's premise: `defer sink(p.*.x)` after `drop(p)` is
no longer a use-after-drop, because the read happens at registration. The test
now asserts **both** — the block form `defer { sink(p.*.x) }` is still refused,
the call form is not — because the difference between them is the whole point.

### The backstop: `ir/check/residue.rs`

An error type that reaches code generation **with no diagnostic behind it** is
now an `internal:` report with a span, at most once per function body:

```
error: internal: an error type reached code generation in `main`
   | this expression has no type
   = note: nothing was reported about it, so inference produced an error type
           without a diagnostic — this is a compiler defect
```

- **It runs only when nothing else spoke.** After a real diagnostic the IR is
  full of error types by design, so the condition in `sema/mod.rs` is what keeps
  it quiet — the same discipline `Ty::mentions_error` exists for.
- It points at the **innermost** erroneous expression: an error type propagates
  outwards, so the outermost node carrying one is usually the whole statement.
- Added **no** failures across 589 tests, which is the evidence that nothing else
  currently leaks one.

## `twig`: what was settled before stopping, and what blocks it

None of this is written. It is the design the probing was for.

| Question | Where it lands, and why |
|---|---|
| **Diagnostics** | `std/process` has **no pipes** — no way to capture a child's stderr, and no `dup2` in `sys`. So `twig` inherits stdio and uses `--error-format=human`. Forwarding `rendered` verbatim needs pipes **and** `std/json` (step 7). The plan sanctions either, but only one is writable today |
| **Dependencies** | Step 8 is not done, so a dependency is **source**: resolve the graph transitively, dedupe, detect cycles, then **one** `nestc` invocation with every `--package name=path`. The graph work is real and survives `.nlib`; the per-package invocation does not exist yet |
| **The manifest** | A **flattened** TOML document — `dependencies.bar.path` → value — rather than a recursive `Value` tree. No recursive enum to prove out, and the manifest's needs are narrow. `Value` is `string / integer / boolean / list([]str)`; inline tables flatten into dotted keys |
| **Package root** | The repo's own convention: a package `foo` at path `P` has root `P/foo.nest`, which is what `-L` already means (`<foo/…>` is `<dir>/foo/foo.nest`). A manifest `root = "…"` overrides it |
| **Commands** | `build`, `run`, `check`, `clean`, and `--release`. `twig run` is `build` plus `process.run` on what it produced |

**What `std` is missing for it**, beyond the previous handoff's list:

- **Pipes** (`pipe`, `dup2`, and a `Child` that owns two descriptors). Needed
  the moment `twig` wants to *read* what `nestc` said rather than let it through.
- **Buffered I/O.** Still the first thing `twig` will want that is not there.

## Not Yet Done (compiler)

- [ ] **A `void` member should be erased from a `TypeDef` too.** §9 erases `void`
      from slots, parameters and arguments but not from a type's members. **Not
      done because `Projection::Field` indices are positional.**
- [ ] **`-C opt-level` and `-C target-cpu`.** The backend hardcodes
      `OptimizationLevel::None`, `RelocMode::PIC`, `CodeModel::Default`.
- [ ] **Parallel code generation.** What is missing is a backend instance and an
      LLVM context per thread.
- [ ] **A library format** (`.nlib` / `.nmeta`). A dependency is source today.
- [ ] **Debug info.** Nothing emits DWARF. **Disableable by config** (your note).
- [ ] **`core.panic` prints nothing useful.** A trap says `nest: trap`. **`std`
      exists now**, so the `#lang("panic_handler")` a program may replace could
      finally print one — but `core` may not import `std`, so this is a decision
      about where the default handler lives rather than a missing function.
- [ ] **`str.to_string(7)` where `str` is the *type*** reports "type annotations
      needed", twice. A bad message for a program that is genuinely wrong — not
      wrong behaviour, so it was left.
- [ ] **Two declarations of one `@link_name`.** Still noted, still not
      reproduced: both declarations were eliminated as dead code before the
      mangler saw them, so the collision needs a program that actually calls both.
- [ ] **`defer` does not capture its receiver** — see above. It needs the capture
      to move into sema lowering, which needs a mutable `DefTable` there.
- [ ] **Optimization across units**, per-function escape summaries, the
      object-start table, narrowing what counts as a root (§5/§6).

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs still stands. New this session:

- **`cargo fmt`.** The committed source is **not** rustfmt-clean: one run
  rewrote 34 unrelated files (`+732 / −401`) and the churn landed in a commit
  meant to be two files. The repo's hand-wrapped `assert_eq!`s and the long
  inline Nest source strings in `src/sema/tests.rs` are deliberate. **Format by
  hand, matching the surrounding style.** If it has already run, restore with
  `git checkout -- $(git diff --name-only | grep -v <your files>)` and re-apply
  your edits to pristine copies — the churn reaches inside your files too.
- **Trusting a previous handoff's *diagnosis*** where it did not name a fix. The
  "trait must be imported by name" item was a real symptom attached to the wrong
  cause, and following it would have been an afternoon in `in_scope_traits` for a
  bug that lived in `ty_from_node_in`.
- **Writing a probe with `str :: import <std/str>`.** It shadows the prelude's
  `str`. Bind it as `text` — which is also why the diagnostic above carries a
  note rather than just a message.

## Key Decisions

Everything in the previous handoff still stands. New this session:

| Decision | Rationale |
|---|---|
| A name in type position that is not a type is **reported**, not silently an error type | It is the only place that can say it, and nothing downstream can recover the span |
| The message quotes the path the program **wrote** | The def's name is `std` for a `str :: import <std/str>`, which points at a package the program never named |
| A trait named in a **bound** is in scope for it | Writing the bound is as clear a statement that the trait is wanted as importing its name |
| …and the set is **per file**, not per generic list | Selection is per file already; a trait bounding one function is not a surprising candidate inside another beside it |
| `parse_assign` **falls back** rather than reporting | The classifier's lookahead is a line, and a line is not a statement |
| A non-place assignment is the **mutability** pass's job | It already owns the left side of an assignment, and "not writable" and "not a place" are the same question asked twice |
| A `defer` captures its **arguments**, not its receiver | Desugar runs before inference; a receiver captured by value would defer a mutating method's effect to a copy |
| The capture binds into the **enclosing** block | A block of its own is a scope that ends immediately |
| An unreported error type at codegen is an **`internal:`** diagnostic | It is a defect in this compiler, and it should read like one instead of like an LLVM crash |
| …and it runs **only when nothing else spoke** | After a real error the IR is full of error types by design |

## Current State

**Working**: everything. `cd nestc && cargo test` → **589**;
`LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 cargo test --features llvm` →
**606**.

**Broken**: nothing known.

**Uncommitted**: nothing.

## Files to Know

| File | Why it matters |
|---|---|
| `design/toolchain.md` | **The plan.** Unchanged this session; step 9 is `twig` |
| `design/lir.md` | The specification. §7d overflow, §10 the backend's brief, §11 the unit split |
| `nestc/src/sema/infer.rs` | `typepath_ty` (the `FieldAccess` arm, the report), `written_path_in`, `bound_traits` |
| `nestc/src/sema/desugar.rs` | `capture_defers` — and the comment saying what it deliberately does not capture |
| `nestc/src/ir/check/residue.rs` | **The backstop.** New file, and the one to read first when the backend crashes with no span |
| `nestc/src/ir/check/mutability.rs` | `is_place`, in front of the permission question |
| `nestc/src/parser/item.rs` | `scan_binding_kind` and `parse_assign` — the line-versus-statement seam |
| `packages/std/` | Seven namespaces over `sys`. Written in same-file, unqualified style, which is why it never hit any of this |

## Resume Instructions

1. `cd nestc && cargo test` — expect **589 passed**.
2. With the backend:
   ```
   export LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21
   cargo test --features llvm          # expect 606
   cargo build --features llvm
   ```
3. **See all six fixes hold at once** — this program uses a qualified literal, a
   qualified bound, a call followed by an assignment on one line, and a `defer`
   whose argument must be read at registration:
   ```
   cat > /tmp/big.nest <<'EOF'
   io   :: import <std/io>
   text :: import <std/str>
   col  :: import <std/collections>
   cmp  :: import <core/cmp>
   Kind :: enum { word(str), number(i64) }
   Entry :: struct { key: str, value: Kind }
   render :: func (v: Kind) -> str {
     return v.match { .word(s) => s, .number(n) => text.to_string(n) }
   }
   same :: func <T: cmp.Eq> (a: T, b: T) -> bool { return a.eq(b) }
   main :: func () -> i32 {
     defer io.println("-- done")
     let mut es: col.Vec.<Entry> := col.new.<Entry>()
     es.push(Entry { key: "name", value: .word("twig") })
     es.push(Entry { key: "count", value: .number(3) })
     let mut i: usize := 0
     for k in 0..<es.len() {
       let e: Entry := es.get(k).match { .some(v) => v, .none => Entry { key: "", value: .number(0) } }
       io.println(text.concat(text.concat(e.key, "="), render(e.value))) i = i + 1
     }
     io.println(same("a", "a").match { true => "eq", false => "ne" })
     return cast.<i32>(i)
   }
   EOF
   ./target/debug/nestc -o /tmp/big /tmp/big.nest && /tmp/big; echo $?   # 2, "-- done" last
   ```
4. **Then `twig`**, which is what was asked for and is not started. The design
   above is settled; the first file to write is the TOML reader, because it is
   the only part with nothing to copy from. *Done when*: `twig build` builds a
   package with a dependency, and `twig run` runs it.

## Edge Cases & Error Handling

Everything in the previous handoff still stands. New this session:

- **A `defer` with no arguments is left alone.** `defer f()` has nothing to
  capture, and rewriting it would only add nodes.
- **A `defer` whose body is a block is left alone too.** Only a `Call` is
  rewritten, which is why the block form still catches a use-after-drop.
- **The residue pass reports one expression per function**, not one per node: an
  error type propagates to every parent, so a body would otherwise be a wall.
- **A non-place assignment is not reported when the place's type already
  mentions an error** — it would be the second sentence about one mistake.
- **`written_path_in` walks a `FieldAccess` chain**, so a qualified name in a
  message reads `ns.P` rather than its last segment.

## User Notes (standing)

- **Ask questions in batches.**
- **If something can be simplified in LIR while building codegen, do it**, and
  pass this instruction on in every future handoff. `embed_file` is still the
  only intrinsic left on that list that is more than one instruction or one
  runtime call.
- **Casts should be explicit about which conversion they are.** Done.
- **The GC is not the priority.** An existing collector behind a thin shim.
- **Codegen is a trait**, and it supplies the target info. Done.
- **LLVM codegen uses inkwell and produces object files.** Done.
- **Everything should be easily usable by the future CLI tool.**
- **`nestc` before `twig`.** Done.
- **`std` is not linked automatically**; that is `twig`'s job. Done — it is a
  resolvable package, not a linked one.
- **`twig` is written in Nest**, so `std` needs fs, io and JSON serialization.
  fs and io exist; JSON is step 7 and is still not written.
- **`nestc` supports structured output as JSON.** Done.
- **There should be `insta` snapshot tests over the generated LLVM**, with no
  target-specific content. *Still not done.*
- **Conditional compilation** is wanted for building `std`. `#when` is not in
  the parser, the spec or the grammar — **deferred until blocking**, your
  decision, and it has not blocked yet.
- **One object file out of `nestc`, always.** Done.
- **A runtime loop over a type's members is fine.** Only typed access must unroll.
- **Debug info should be disableable by config.**
- **Be compact in commit comments; do not edit existing comments.**
- **If you are unsure or need further guidance, ask.**

## User Notes
- **You asked for `twig` (step 9), out of order**, then for the bugs to be fixed
  first. The bugs are fixed and committed; **`twig` is not started.**
- **`std/json` (7) and the library format (8) are still unwritten**, and `twig`
  as designed above does not need either — but it cannot render a child's
  diagnostics without both pipes and JSON.
