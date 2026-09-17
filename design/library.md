# `.nlib` and `.nmeta`, the library formats

**Status**: a record of what `nestc/src/library/` does today.

A package is compiled **once**, on its own. What it leaves behind is a library,
and everything a later compilation asks about that package is answered out of it
rather than by analyzing its source again — which is why a package's source need
not exist where it is used.

Two artifacts, one of which contains the other:

| Written by | What it is |
|---|---|
| `--emit nmeta` | `.nmeta`: the package, analyzed. |
| `--emit nlib` | `.nlib`: an `ar` archive of that metadata and the package's object files. |

Compiling against a package reads only the metadata; linking a program reads the
objects back out of the archive and hands those to the linker.

## `.nmeta`

```
"NESTMETA"            8 bytes
<header length>       u32, little-endian
<header>              postcard
<body>                postcard
```

The header is read first and on its own, so a library that cannot be used is
refused by name rather than misread. It carries `FORMAT` (the layout version,
raised whenever anything written changes shape), the compiler that wrote it, the
package's name, the target it was compiled for, the packages its ids belong to,
how many ids of each kind it owns, the primitives its body names, the files it
was compiled from, the settings, and a fingerprint.

The body is everything a package compiled against this one needs:

- **each file**: its name, its text, the namespace it is, its AST, and the facts
  the passes left on that tree (resolutions, types, coercions, …);
- **every definition**, with its namespace;
- the **`#lang` tags** the package claims;
- **each file's IR**, before monomorphization, and the facts on it.

The text of each file travels because a diagnostic pointing into a library still
wants to show the line. The IR travels **unmonomorphized** because a generic
function is instantiated by whoever calls it, so its body has to be there.

Loading one (`library::read::load`) puts all of that into a session as though the
package had been analyzed there, and the analysis passes skip its files.

### Ids

`DefId`, `FileId` and `IrId` are indices into tables a session owns, and another
session numbers its tables differently — it loads other packages, in another
order. So an id is never written as a number. It is written as **whose** it is
and **where in that owner's own numbering** (`codec::IdRef`), or, for a
primitive, as its name: `u65536` exists in a session only once something asks for
it.

Ids sit deep inside everything the body holds, so the translation is not a pass
over the data: it is the ids' own `Serialize` / `Deserialize`, reading the
translation from a thread-local that `codec::encode` and `codec::decode` set for
as long as they run. Each package's ids land as one contiguous run per kind, so
translating one is an addition to that run's base.

### The facts, and adding one

A `MetaStore` is type-indexed and cannot be walked without knowing the types, so
the types are listed in `library::metas`: `MetaValue` for a fact that travels,
`DERIVED` for a cache a later pass rebuilds. **A type in neither is refused when a
store is written**, by name — a new pass that stores a new fact cannot quietly
produce metadata that leaves it out. Adding a side table therefore means adding
it to one of those two lists, and a change to what a persisted type serializes
means raising `FORMAT` (after which the `build/` directories have to go).

### Fingerprints

The header's fingerprint is FNV-1a over everything the compilation read: the
compiler, the target, the settings, each of the package's own files, and the
fingerprint of each library it was compiled against. "Is this library stale" is
computing that hash again from what is on disk now and comparing — no clock and
no timestamps, so a file touched and left unchanged is not a change, and a
dependency rebuilt from the same inputs is the same dependency.

## `.nlib`

A Unix `ar` archive: the global header `!<arch>\n`, then each member behind a
60-byte header of fixed-width text fields. Written by hand (`library::archive`)
because it is that small.

Members are the metadata, under the name `nest.nmeta`, and one object file per
codegen unit, named `u0.o`, `u1.o`, … in order. There is **no symbol table**: a
linker is never given the archive itself, only the objects read back out of it,
so nothing needs to search it.
