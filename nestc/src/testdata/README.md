# Test corpus

Programs the compiler's own tests read. `examples/` at the repository root is a
showcase and may change at will; these may not, without the tests that read
them changing too.

- `programs/` — every `.nest` here is analyzed (`sema::tests`), lowered at four
  codegen-unit settings (`lir::tests`) and emitted as an object
  (`codegen::llvm::tests`).
- `gc/` — three programs about the collector, run by `codegen::llvm::tests`;
  each one's exit status is its answer.
- `packages/` — a program importing two packages, analyzed by `sema::tests`.
