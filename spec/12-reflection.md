# 12 — Reflection (Planned)

Strong compile-time reflection is a **planned** feature and an area of active
design. This chapter records the intent and the constraints it places on the
rest of the language; it does **not** yet specify the API, and the details below
are subject to change.

## 12.1 Intent

The language is monomorphizing and treats types as first-class compile-time
values, so a reflective query over a concrete type resolves to concrete,
compile-time-known information. The goals are:

- Inspect a type's structure at compile time — its kind (struct/enum/trait/…),
  fields (name, type, offset), enum variants, and methods.
- Read back **attributes** (`@public`, user-defined `@name(args)`) attached to a
  declaration, so that data-driven tooling — serializers, routers, test
  discovery, formatters for interpolated strings — can be generated without
  special compiler support.
- Optionally **reify** a subset of this metadata into the binary so that limited
  reflection is available at run time (e.g. behind a `dyn` trait object), with the
  cost paid only for metadata that run-time code actually reaches.

## 12.2 Constraints already fixed

Even though the API is undecided, these decisions elsewhere in the spec exist to
support reflection and will not be walked back:

- **Attributes are metadata, not behavior** (see
  [09-directives-and-attributes.md](09-directives-and-attributes.md)): user
  `@name(args)` attributes are preserved on declarations precisely so reflection
  can read them.
- **Layout is well-defined**: `#packed`, `#align(N)`, and `#soa` give types a
  known size / alignment / field offset that reflection will report accurately,
  and the `size_of` / `align_of` intrinsics already expose the scalar parts.
- **Types are compile-time values**, passable to generics and bindable with `::`,
  which is the substrate a reflection API will build on.

## 12.3 Open questions (not yet decided)

- The surface API: intrinsics (`type_of`, `fields`, …) vs. a std `reflect`
  namespace vs. methods on a `Type` value — undecided.
- The exact `Type` / `Field` / `Variant` / `Attribute` data model.
- How much metadata is reified for run-time use by default, and how it is opted
  into or out of.
- Whether reflection may drive code generation (compile-time codegen from
  reflected shape) and, if so, through what mechanism.

Until these are settled, treat reflection as a design placeholder. Nothing else
in the spec depends on the unspecified parts.
