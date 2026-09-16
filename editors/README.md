# Editor support

- `tree-sitter-nest/` — the tree-sitter grammar.
- `zed/` — the Zed extension, which builds that grammar.

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
