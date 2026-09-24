# Nest docs

The Nest language website: [Astro](https://astro.build) +
[Starlight](https://starlight.astro.build). Content lives in
`src/content/docs/`, sourced from `../spec/` and `../examples/`; every
snippet should compile against the current compiler. Syntax highlighting is
a hand-written TextMate grammar at `src/nest/nest.tmLanguage.json`,
registered with Expressive Code/Shiki in `astro.config.mjs` — kept in sync
with `../editors/tree-sitter-nest/grammar.js` and its
`queries/highlights.scm`, the sources of truth for what's a keyword/type/etc.

## Commands

Run from `docs/`, or via `just docs` / `just docs-build` from the repo root:

| Command | Action |
| :--- | :--- |
| `pnpm install` | Install dependencies |
| `pnpm dev` | Local dev server at `localhost:4321` |
| `pnpm build` | Build the production site to `./dist/` |
| `pnpm preview` | Preview a build locally |

## Structure

```
.
├── public/
├── src/
│   ├── assets/
│   ├── content/docs/       # the pages — getting-started, language/*, toolchain
│   ├── nest/                # the Nest TextMate grammar for Shiki
│   └── content.config.ts
├── astro.config.mjs         # sidebar nav + the custom Shiki language
├── package.json
└── tsconfig.json
```
