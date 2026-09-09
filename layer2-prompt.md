# nest trait system: obligations, impl selection, projection, operators-as-traits

**Context.** `nestc/` — Rust bootstrap compiler for nest (self-hosted later; keep it simple). Pipeline `src/sema/mod.rs`: collect → imports → resolve → desugar → infer → lower. Read first: `ty.rs` (`Ty` + `InferCtxt`, pure-unification union-find), `infer.rs` (HM per fn body; already has generic instantiation + name-based method resolution), `lower.rs` + `ir/` (typed structured IR; operators are currently `ir::Expr::Binary`), `collect.rs` (`collect_impl` parks impls, does not index them), `def.rs` (`DefKind`, `LangItems`), `core/prelude.nest` (`Add`/`Sub`/`Mul`/`Div`/`Rem`/`Eq`/`Ord`/… with `#lang` tags).

**Why one deliverable.** Selecting `impl Add for T` needs `T` (inference); typing `a+b` needs the impl's `Output` (selection); the deciding type can arrive later. Pure unification can't hold a decision open. So add an obligation layer interleaved with unification. This is **not** "loop Layer 1": re-running unification finds no new equalities — the missing thing is a *search* (impl selection) plus *deferred* obligations, a different operation.

**Do:**
1. **Impl index** — in collection, record every `impl [<g>] Trait[.<args>] for Self` (and inherent) keyed by `(trait, self-head)`: generics, self-node, trait-args, members, assoc-type bindings (`Name :: <type>`).
2. **Obligations** in `InferCtxt` — queue of `Trait{self,trait,args}` and `Projection{...,assoc,out}`. Add `register` + `select_where_possible`; leave `unify` as is.
3. **Selection** — trial-unify candidate impls (generics → fresh vars) with the obligation. Concrete beats generic. Equally-specific → **ambiguity error**. None + known self → "T does not implement Trait". Unknown self → defer. **Only traits in scope at the use site are candidates** — imported or defined in the same/enclosing scope, Rust-style; filter candidates by the in-scope trait set.
4. **Projection** — on selecting, substitute the impl's solved generics into its assoc binding, unify with `out`. `Self.Output` / `I.Item` type-exprs → fresh var + `Projection` obligation (not an opaque nominal).
5. **Fulfillment** — after each fn body, run selection to a fixpoint, then `finalize`. Unsatisfied → diagnostics. Remove the silent-`Error` concession in `infer.rs::finish`; unsolved var → real "type annotations needed".
6. **Methods via impls** — if a method isn't an inherent member, search in-scope trait impls, select, resolve in the chosen impl. Keep the inherent fast path.
7. **Builtin primitive impls — ONE place.** `src/sema/builtins.rs`: a compact table `[(lang-tag, applies-to, Output rule, BuiltinOp)]` → bodyless impls for `iN`/`uN`/`fN` (`Add/Sub/Mul/Div/Rem` minimum). Adding an intrinsic = **one table entry**. Selection, projection, and the codegen tag all read this table. Builtins selected uniformly with user impls (spec §6: "int+int and Vec3+Vec3 are the same construct").
8. **Operators** — infer `+ - * /` (and the rest, if cheap) via the operator-trait obligation; result = projected `Output`. Lower to a **uniform** `ir::Expr::Call` to the resolved method. Codegen must recognize a builtin primitive op in **O(1)** (tag the builtin method def, or stamp `builtin: Option<BuiltinOp>` on the IR call). Document the contract.

**Constraints.** Bootstrap-sized: no coherence/overlap graph (specificity + ambiguity-error only); bounds = direct trait bounds on generics. Keep the IR structured (no CFG). Don't regress the current 81 tests or the examples. Match the files' heavy doc-comment style.

**Tests — test this system extensively; it is the hard part.** Every new capability gets dedicated coverage, positive **and** negative. Do not consider a work item done until it is tested. At minimum:

- **Selection:** concrete beats generic; generic impl over a family; ambiguity (two equally-specific) → error; no matching impl on a known type → error; **a trait not in scope is not a candidate even if an impl exists** (and works once imported); selection deferred while the self type is a variable, then resolved once it is known.
- **Projection:** `Add.Output` for a user impl; `Self.Output` inside a trait method; a projection that only resolves after backward flow; a projection whose impl is chosen by a generic argument.
- **Obligations / fulfillment:** backward flow (`let x := f(); use_as_i32(x)` picks the impl late); a chain where solving one obligation unblocks another; an obligation that stays unsatisfiable → precise diagnostic; leftover ambiguous var → "type annotations needed".
- **Operators:** `i32 + i32` and mixed widths; every builtin op in the table; a user `impl Add for Vec3` (`Vec3 + Vec3 : Vec3`); an operator on a type with no impl → error; that the builtin call is O(1)-recognizable (assert the tag/marker).
- **Regression:** all existing tests and every `examples/*.nest` stay green (`examples_analyze_without_errors`); add a new example exercising a user operator overload + projection.

Use **unit tests** for the solver internals (selection, projection, obligation fulfillment — assert `Ty`s and diagnostics directly) and **insta snapshots** (`ir_text` helper, `src/sema/snapshots/`) for the lowered IR: `i32 + i32` → uniform `Add.add : i32`, `Vec3 + Vec3 : Vec3` as a trait call, a projection resolving to a concrete type. Prefer many small, focused tests over few broad ones. End state: `cargo test` fully green, `cargo clippy` no new real nits.

**Done when.** Primitives and user `impl Add` both lower to a uniform trait `Call`, fully typed via projected `Output`; builtins are O(1)-recognizable by codegen; a new intrinsic is one table entry; no node in a resolvable program stays `Ty::Error`; genuine failures give precise diagnostics. Update memory `nest-type-system-decisions.md`.
