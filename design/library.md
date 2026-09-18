# `.nlib`, the library format

**Status**: a record of what `nestc/src/library/` does today.

A package is compiled **once**, on its own. What it leaves behind is a library,
and everything a later compilation asks about that package is answered out of it
rather than by analyzing its source again — which is why a package's source need
not exist where it is used.

**One artifact**, written by `--emit nlib`: a `.nlib`, an `ar` archive of three
kinds of member.

| Member | What it is |
|---|---|
| `nest.nmeta` | the package, analyzed: what typechecking against it needs. |
| `nest.nir` | its IR, before monomorphization. |
| `u0.o`, `u1.o`, … | one object per codegen unit. |

There is no second file. A package's metadata alone is not a thing a
compilation can be given: a generic in it is instantiated by whoever calls it,
so the IR travels with it or the library is not usable. Compiling against a
package reads the first two members; linking a program reads the objects back
out and hands those to the linker.

## `nest.nmeta`

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
- **every definition**, with its namespace, and **what it declares**
  (`sema::decl`) — the parameters of a function, the generics and fields of a
  type, what a trait member asks of an impl. These are the answers a package
  compiled against this one would otherwise read off a tree it does not have;
- the **`#lang` tags** the package claims.

The text of each file travels because a diagnostic pointing into a library still
wants to show the line.

## `nest.nir`

Each file's IR and the facts the passes left on it. The IR travels
**unmonomorphized** because a generic function is instantiated by whoever calls
it, so its body has to be there.

It is a member of its own rather than part of the metadata because it is read
for a different reason: a compilation that only typechecks against this package
never looks at it. The two are nonetheless written and read **together**, under
one encoding, because the ids in them are the same ids and the header that says
how to translate them is the metadata's.

Loading a library (`library::read::load`) puts both into a session as though the
package had been analyzed there, and the analysis passes skip its files.

## Ids

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

## The facts, and adding one

A `MetaStore` is type-indexed and cannot be walked without knowing the types, so
the types are listed in `library::metas`: `MetaValue` for a fact that travels,
`DERIVED` for a cache a later pass rebuilds. **A type in neither is refused when a
store is written**, by name — a new pass that stores a new fact cannot quietly
produce metadata that leaves it out. Adding a side table therefore means adding
it to one of those two lists, and a change to what a persisted type serializes
means raising `FORMAT` (after which the `build/` directories have to go).

## Fingerprints

The header's fingerprint is FNV-1a over everything the compilation read: the
compiler, the target, the settings, each of the package's own files, and the
fingerprint of each library it was compiled against. "Is this library stale" is
computing that hash again from what is on disk now and comparing — no clock and
no timestamps, so a file touched and left unchanged is not a change, and a
dependency rebuilt from the same inputs is the same dependency.

## The archive

A Unix `ar` archive: the global header `!<arch>\n`, then each member behind a
60-byte header of fixed-width text fields. Written by hand (`library::archive`)
because it is that small.

Members are the metadata under the name `nest.nmeta`, the IR under `nest.nir`,
and one object file per codegen unit, named `u0.o`, `u1.o`, … in order. There is
**no symbol table**: a linker is never given the archive itself, only the objects
read back out of it, so nothing needs to search it.
