# Editor support

- `tree-sitter-nest/` — the tree-sitter grammar.
- `zed/` — the Zed extension, which builds that grammar and starts the language
  server (`nestc/lsp`).

## Changing the grammar

```sh
cd editors/tree-sitter-nest
npm install              # tree-sitter-cli, pinned
npx tree-sitter generate # rewrites src/parser.c, which is committed
npx tree-sitter test     # test/corpus
```

Every `.nest` file in the repository should parse with no `ERROR` or `MISSING`
node:

```sh
npx tree-sitter parse --quiet --stat $(git -C ../.. ls-files '*.nest' | sed 's|^|../../|')
```

`queries/highlights.scm` is the source of truth. `zed/languages/nest/highlights.scm`
is a copy of it, so copy it again after editing.

## Installing in Zed

Zed builds the grammar from `repository` at `rev` (see `zed/extension.toml`), so
that commit has to be reachable: pushed to GitHub. To try an unpushed grammar,
point `repository` at `file:///<absolute path to this repository>` and `rev` at a
local commit. Don't commit that change.

Then run `zed: install dev extension` and pick `editors/zed`.

## The language server

```sh
cd nestc
cargo build --release -p nest-lsp   # target/release/nest-lsp
```

Put `nest-lsp`, `twig` and `nestc` on `PATH`, or say where they are in Zed's
settings:

```json
"lsp": {
  "nest-lsp": {
    "binary": { "path": "/path/to/nest-lsp" },
    "settings": { "twig": "/path/to/twig", "nestc": "/path/to/nestc" }
  }
}
```

`twig` and `nestc` there become `nest-lsp --twig <path> --nestc <path>`. Without
`nestc`, twig finds it as it always does (`NESTC`, then `PATH`). twig's `nestc`
must be the same build as the server: the server reads libraries it wrote.

The server answers diagnostics, hover, go-to-definition and completion. Hover
shows a definition's `///` lines as its documentation. Completion offers names
that are not imported yet, and adds the import when one is chosen.
