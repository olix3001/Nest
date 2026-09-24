//! The definition model shared by every semantic-analysis stage.
//!
//! Where the parser produces an [`Ast`](crate::parser::ast::Ast) per file, the
//! analyzer produces **one** [`DefTable`] for the whole compilation: a flat
//! arena of [`Def`]s addressed by a global [`DefId`]. A `DefId` is stable across
//! files, which is exactly what cross-file name resolution needs — a use of
//! `math.add` in one file resolves to the same `DefId` the definition in
//! `math.nest` was given.
//!
//! Each namespace-like def ([`DefKind::Namespace`], a type, an `impl` target)
//! owns a [`Namespace`]: the name → `DefId` maps for its own members and for the
//! names an `import` brought into it. Name resolution walks these maps; it never
//! needs another file's [`Ast`], only its collected `Namespace`.

use std::collections::HashMap;

use crate::common::span::Span;
use crate::common::symbol::Symbol;
use crate::ir::const_eval::ConstValue;
use crate::parser::ast::{FileId, NodeId};

/// A global definition id: an index into a [`DefTable`]. Stable across files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

/// Whether a def is exported from its enclosing namespace (§4.4). Only `@public`
/// items are reachable through `import` / qualified access from the outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Visibility {
    Public,
    /// `@public(package)` — exported to the package that declares it, and to
    /// nothing outside it (§4.4).
    ///
    /// The unit is the **package**, not the file: a package's files are written
    /// together and released together, so a type one of them declares for the
    /// others is an ordinary thing to want and has nowhere else to live. A
    /// program's own files, which belong to no package, are one such unit
    /// between them.
    Package,
    Private,
}

impl Visibility {
    pub fn is_public(self) -> bool {
        matches!(self, Visibility::Public)
    }

    /// Whether an item with this visibility, declared in package `home`, may be
    /// named from package `at`. `None` is "no package": a program's own files.
    ///
    /// Lexical privacy — a private item inside its own namespace — is the
    /// caller's question and is answered before this one; what is left here is
    /// whether the item is exported far enough to be reached from `at` at all.
    pub fn reaches(self, home: Option<&str>, at: Option<&str>) -> bool {
        match self {
            Visibility::Public => true,
            Visibility::Package => home == at,
            Visibility::Private => false,
        }
    }
}

/// What a [`Def`] denotes. Drives both diagnostics and how member lookup treats
/// the def (namespace-like kinds own a populated [`Namespace`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DefKind {
    /// A namespace value: a whole file, an inline `namespace { ... }`, or a
    /// package root. Owns members.
    Namespace,
    /// A `struct` / `enum` / `trait` type. Owns members (fields, variants, and —
    /// after impl collection — associated items).
    Struct,
    Enum,
    Trait,
    /// A `distinct T` or plain type alias RHS.
    TypeAlias,
    /// A `name :: value` constant binding.
    Const,
    /// A `func` (free function, method, or associated function).
    Func,
    /// An **overload set** — `f :: func { a, b }` (§4.3): one name reaching
    /// several functions, of which a call picks one by what it passes.
    ///
    /// The members are the functions it lists, resolved and recorded in the
    /// declaration table; the set itself has no body, no signature and no
    /// symbol of its own.
    Overload,
    /// A `struct`/`enum` record field.
    Field,
    /// An `enum` variant.
    Variant,
    /// A **closure**'s type (§5.5): an anonymous struct whose fields are what
    /// the closure captured, generic over whatever the function it is written in
    /// is generic over. Its one member, `call`, is the closure's body as a
    /// function taking the closure first.
    Closure,
    /// A function parameter.
    Param,
    /// A generic **type** parameter (`<T>`, `<T: Trait>`).
    TypeParam,
    /// A generic **value** parameter (`<const N: usize>`) — a compile-time
    /// constant, not a type (§5). It names a value in the body and a length in
    /// a type such as `[N]T`.
    ConstParam,
    /// A block-local `let` / `const` / `::` binding, or a pattern binding.
    Local,
    /// A name introduced by an `import` binding (see [`Def::alias`]).
    Import,
    /// A builtin primitive type (`i32`, `string`, ...) with no source.
    Primitive,
    /// A member of a package/file we can name but have not loaded — kept so a use
    /// still resolves to a stable id instead of an error.
    External,
}

impl DefKind {
    /// Whether this kind owns a member [`Namespace`] worth searching on a `.` hop.
    pub fn is_namespace_like(self) -> bool {
        matches!(
            self,
            DefKind::Namespace
                | DefKind::Struct
                | DefKind::Enum
                | DefKind::Trait
                | DefKind::External
        )
    }

    /// A short word for the pretty printer / diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            DefKind::Namespace => "namespace",
            DefKind::Struct => "struct",
            DefKind::Closure => "closure",
            DefKind::Enum => "enum",
            DefKind::Trait => "trait",
            DefKind::TypeAlias => "type",
            DefKind::Const => "const",
            DefKind::Func => "func",
            DefKind::Overload => "overload set",
            DefKind::Field => "field",
            DefKind::Variant => "variant",
            DefKind::Param => "param",
            DefKind::TypeParam => "typeparam",
            DefKind::ConstParam => "constparam",
            DefKind::Local => "local",
            DefKind::Import => "import",
            DefKind::Primitive => "primitive",
            DefKind::External => "external",
        }
    }
}

/// The members a namespace-like [`Def`] exposes, split by how they got there so
/// resolution can honor the §4.6 search order and diagnose glob conflicts.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Namespace {
    /// Members declared directly in this namespace, `name → def`.
    pub members: HashMap<Symbol, DefId>,
    /// Names pulled in by a selective / whole-namespace `import` at this scope.
    pub imported: HashMap<Symbol, DefId>,
    /// Namespaces globbed in (`* :: import <...>`); searched as a fallback, after
    /// direct and selective names. Each entry is a namespace-like def.
    pub globs: Vec<DefId>,
}

impl Namespace {
    /// Look up a name declared or selectively imported here (not through globs).
    pub fn get_direct(&self, name: &Symbol) -> Option<DefId> {
        self.members
            .get(name)
            .or_else(|| self.imported.get(name))
            .copied()
    }
}

/// What a synthesized associated-type parameter stands for: `<base as
/// trait_def>.assoc`. See [`Def::projection`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Projection {
    /// The type parameter the associated type is projected through.
    pub base: DefId,
    /// The trait that declares it — one of `base`'s bounds.
    pub trait_def: DefId,
    /// The associated type's name, as the trait declares it.
    pub assoc: Symbol,
    /// The type node a bound **pinned** it to, if it did: the `i32` of
    /// `<T: Holder.<Item = i32>>`.
    ///
    /// A pinned projection has an answer without waiting for a call site, and
    /// the answer is needed *inside* the generic body — a function declared to
    /// return `i32` whose `t.get()` yields `T.Item` type-checks only because
    /// the bound says those are the same type. The node lives in the file the
    /// bound was written in, which is this def's own.
    pub pinned: Option<NodeId>,
}

/// One `#name(args...)` directive as written on a definition (§9).
///
/// The name is kept verbatim and the arguments are kept as the small literal
/// vocabulary directives draw on — `#align(16)`, `#lang("add")`,
/// `#link_name("printf")`. Nothing here interprets them; a directive this
/// front end has no opinion about still travels, so adding one later is a
/// matter of reading it where it matters rather than re-plumbing it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Directive {
    pub name: Symbol,
    pub args: Vec<DirectiveArg>,
}

impl Directive {
    /// Whether this is the directive called `name`.
    pub fn is(&self, name: &str) -> bool {
        self.name.as_str() == name
    }
}

/// One argument of a [`Directive`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DirectiveArg {
    /// An integer literal: `#align(16)`.
    Int(i128),
    /// A string literal: `#lang("add")`.
    Str(Symbol),
    /// A bare name: `#repr(c)`.
    Name(Symbol),
    /// Anything else the directive vocabulary does not cover; kept so an
    /// unrecognized form is still visible rather than silently dropped.
    Other,
}

/// One `@Name(args)` attribute written on a definition, once its name has been
/// resolved to the `@attribute` struct it denotes (§9's addition).
///
/// The arguments are kept as written — named or positional, in source order —
/// because matching them to the struct's members needs that struct's
/// *declaration order*, which is a thing the layout engine knows and name
/// resolution does not. What resolution does check is that every name is a
/// member and that the count is right, so by the time this travels the only
/// work left is putting the values in order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttrValue {
    /// The `@attribute` struct this is a value of.
    pub def: DefId,
    /// Each argument, with the member name when the source wrote one.
    pub args: Vec<(Option<Symbol>, ConstValue)>,
}

/// One definition. Leaf defs (locals, params, fields) leave `ns` empty; the
/// namespace-like kinds populate it during collection and import wiring.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Def {
    pub id: DefId,
    pub name: Symbol,
    pub kind: DefKind,
    pub vis: Visibility,
    /// Enclosing namespace-like def, or `None` for roots (packages, builtins).
    pub parent: Option<DefId>,
    /// Source location of the definition, when it has one.
    pub file: Option<FileId>,
    pub span: Option<Span>,
    /// The defining node in `file`'s arena, when it has one.
    pub node: Option<NodeId>,
    /// Fully-qualified path from a root, e.g. `["math", "add"]`, `["core",
    /// "Option"]`. This is the "resolved name" a definition is linked to.
    pub canonical: Vec<Symbol>,
    /// The `#lang("tag")` this def is marked with, if any.
    pub lang: Option<Symbol>,
    /// Every directive written on this definition, in source order (§9).
    ///
    /// Directives are *carried*, not acted on, by the front end: `#inline`,
    /// `#packed`, `#soa`, `#align(16)`, `#unsafe` all describe how a later stage
    /// should lay out or emit the thing, and that stage is not this one. Keeping
    /// them on the def rather than only in the AST is what lets the IR — whose
    /// every node names a [`DefId`] and nothing else — still reach them, so a
    /// `#soa` on a struct is available wherever that struct's type turns up.
    pub directives: Vec<Directive>,
    /// Members this def owns (empty for leaf defs).
    pub ns: Namespace,
    /// For a trait's **abstract** associated type (`Item :: type`): the traits
    /// its own bounds name, resolved.
    ///
    /// `None` for anything else, so this doubles as the test for "is this an
    /// abstract associated type" — a question that would otherwise need the
    /// declaring file's syntax tree, which the file *using* the trait does not
    /// have. It is filled in when the trait's own body is resolved, and read
    /// wherever a bound mentioning that trait is.
    ///
    /// The bounds are what makes `T.Item.Item` writable: `Item :: type: Holder`
    /// says the projected type is itself a `Holder`, so it gets associated-type
    /// parameters of its own (see [`Def::projection`]).
    pub assoc_bounds: Option<Vec<DefId>>,
    /// For a **generic type parameter** (`<T: Float>`): the traits its written
    /// constraint named.
    ///
    /// Recorded rather than read back off the constraint's syntax, because a
    /// parameter that arrives with a library has no syntax here — and the
    /// answer is one impl selection cannot do without: a blanket
    /// `impl <T: Float> Display for T` unifies its self type with *anything*,
    /// so the bound is the whole of what keeps it from applying to every type
    /// in the program. `None` means nothing recorded it; an empty list means it
    /// was written with no bounds.
    pub param_bounds: Option<Vec<DefId>>,
    /// For a type parameter **synthesized from a bound's associated type** —
    /// the `Item` of `T.Item` where `<T: Holder>` (§5.4) — what it projects.
    ///
    /// A bound is a promise that whatever instantiates `T` has an impl, and an
    /// associated type of that impl is a type the signature may name. It is not
    /// known until the call site says what `T` is, which is exactly what a type
    /// parameter is — so it *is* one, and this records the equation that solves
    /// it: `<base as trait_def>.assoc`.
    pub projection: Option<Projection>,
    /// For an [`DefKind::Import`] binding: the def it aliases (a namespace or a
    /// selected member). Following `alias` reaches the real target.
    pub alias: Option<DefId>,
    /// For a [`DefKind::Field`]: the field carries `@using`, so its struct
    /// implicitly upcasts to the field's type (§3.10). At most one field per
    /// struct may set this.
    pub using: bool,
    /// Whether this def is an `@attribute` — a struct a program may write on a
    /// declaration, whose value then appears on that declaration's descriptor
    /// (§9's addition). Only a struct may be one.
    pub attribute: bool,
    /// The `@Name(args)` attributes written on this def, resolved.
    ///
    /// Empty for everything else, which is nearly everything: today's
    /// attributes are a fixed set the compiler reads (`@public`,
    /// `@link_name`), and these are the ones a *program* declared.
    pub attrs: Vec<AttrValue>,
    /// Whether this binding may be **assigned to** (§2.3).
    ///
    /// True for a `let` local and for a pattern binding written `mut`; false for
    /// `const`, for `::`, for a function parameter (§5.2: parameters are
    /// immutable bindings — rebind with `let` for a mutable copy), and for a
    /// plain pattern binding in a `match` arm.
    ///
    /// This is **binding** mutability, which §2.3 is careful to keep independent
    /// of **reference** mutability: `const p := &mut x` is an immutable binding
    /// holding a mutable pointer, so `p.* = 1` is legal and `p = &mut y` is not.
    /// Whether a write *through* something is allowed comes from the `*mut` /
    /// `[]mut` in its type, never from here.
    pub mutable: bool,
    /// For a [`DefKind::TypeParam`]: whether it is the type an `impl Bounds`
    /// **return** type stands for (§5.4) — one type the function's body
    /// decides, not one a call site chooses.
    ///
    /// Callers see it only through its bounds, the way a generic body sees a
    /// parameter; what it is is recorded once the body is typed
    /// ([`super::decl::ParamDecl::revealed`]) and put in its place before
    /// monomorphization.
    pub opaque: bool,
}

impl Def {
    /// The def an [`DefKind::Import`] alias ultimately points at (or itself).
    pub fn target(&self) -> DefId {
        self.alias.unwrap_or(self.id)
    }

    /// Whether this def is one of the two integer **family constructors**,
    /// `int` or `uint` (§3.1).
    ///
    /// They are primitives like `bool` and `usize`, but unlike those they are
    /// not types on their own: `int` names a family and only `int.<N>` names a
    /// member of it. Every place that asks "does this name a type?" has to say
    /// no for these — otherwise `impl <const N: usize> int.<N>` binds `Self` to
    /// the *constructor*, and the width the impl is generic over is lost.
    pub fn is_int_family(&self) -> bool {
        self.kind == DefKind::Primitive && matches!(self.name.as_str(), "int" | "uint")
    }

    /// The intrinsic tag this definition claims, if it is marked `#intrinsic`
    /// (§6.4, §9).
    ///
    /// `#intrinsic("size_of")` names the intrinsic explicitly; a bare
    /// `#intrinsic` means "the tag is the declared name". The explicit form is
    /// what keeps `core` renameable — the compiler recognizes an intrinsic by
    /// its tag, never by the name or path a library happens to give it, exactly
    /// as it finds a `#lang` item by tag.
    pub fn intrinsic_tag(&self) -> Option<Symbol> {
        let d = self.directives.iter().find(|d| d.is("intrinsic"))?;
        Some(match d.args.first() {
            Some(DirectiveArg::Str(tag)) => tag.clone(),
            _ => self.name.clone(),
        })
    }

    /// Whether this declaration is `#c_vararg` — a C function whose declared
    /// parameters are the **fixed** ones and which accepts a tail of further
    /// arguments passed under the platform's variadic convention (§9).
    ///
    /// The tail has no type, because C's does not: each argument is checked and
    /// promoted at the call site on its own. That is also why this is a fact
    /// about the *declaration* rather than about its [`Ty::Func`](super::ty::Ty)
    /// — a variadic signature has no function-pointer type here, and a
    /// declaration is the only place one can be written.
    pub fn is_c_variadic(&self) -> bool {
        self.directives.iter().any(|d| d.is("c_vararg"))
    }
}

impl DefTable {
    /// The `@using` field of a struct, if it declares one — the field an
    /// implicit upcast of that struct goes through (§3.10).
    pub fn using_field(&self, nominal: DefId) -> Option<DefId> {
        self.get(nominal)
            .ns
            .members
            .values()
            .copied()
            .find(|&m| self.get(m).using)
    }
}

/// The whole-compilation arena of [`Def`]s.
#[derive(Debug, Default)]
pub struct DefTable {
    defs: Vec<Def>,
}

impl DefTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a def, assigning its [`DefId`]. `canonical` is completed by the
    /// caller (usually parent's path + `name`).
    #[allow(clippy::too_many_arguments)]
    pub fn alloc(
        &mut self,
        name: Symbol,
        kind: DefKind,
        vis: Visibility,
        parent: Option<DefId>,
        file: Option<FileId>,
        span: Option<Span>,
        node: Option<NodeId>,
        canonical: Vec<Symbol>,
    ) -> DefId {
        let id = DefId(self.defs.len() as u32);
        self.defs.push(Def {
            id,
            name,
            kind,
            vis,
            parent,
            file,
            span,
            node,
            canonical,
            lang: None,
            directives: Vec::new(),
            ns: Namespace::default(),
            assoc_bounds: None,
            param_bounds: None,
            projection: None,
            alias: None,
            using: false,
            attribute: false,
            attrs: Vec::new(),
            mutable: false,
            opaque: false,
        });
        id
    }

    /// Append a def read from a library's metadata, whose id was translated to
    /// the slot it lands in (`crate::library::codec`) — so it must be that slot.
    pub fn push(&mut self, def: Def) -> Result<DefId, String> {
        let id = DefId(self.defs.len() as u32);
        if def.id != id {
            return Err(format!(
                "`{}` was numbered {} but lands at {}",
                def.name, def.id.0, id.0
            ));
        }
        self.defs.push(def);
        Ok(id)
    }

    /// The primitive `name` in the builtins namespace `builtins`, made the first
    /// time it is asked for: `u65536` is a type, and there is no list of them.
    pub fn intern_primitive(&mut self, builtins: DefId, name: &Symbol) -> DefId {
        if let Some(&existing) = self.get(builtins).ns.members.get(name) {
            return existing;
        }
        let id = self.alloc(
            name.clone(),
            DefKind::Primitive,
            Visibility::Public,
            Some(builtins),
            None,
            None,
            None,
            vec![name.clone()],
        );
        self.get_mut(builtins).ns.members.insert(name.clone(), id);
        id
    }

    pub fn get(&self, id: DefId) -> &Def {
        &self.defs[id.0 as usize]
    }

    pub fn get_mut(&mut self, id: DefId) -> &mut Def {
        &mut self.defs[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Def> {
        self.defs.iter()
    }

    /// The canonical path of `id` rendered as a dotted string, e.g. `math.add`.
    pub fn canonical_string(&self, id: DefId) -> String {
        let def = self.get(id);
        if def.canonical.is_empty() {
            def.name.as_str().to_string()
        } else {
            def.canonical
                .iter()
                .map(Symbol::as_str)
                .collect::<Vec<_>>()
                .join(".")
        }
    }

    /// Follow an import-alias chain to the concrete def it names (cycle-guarded).
    pub fn resolve_alias(&self, mut id: DefId) -> DefId {
        let mut seen = 0;
        while let Some(next) = self.get(id).alias {
            if next == id || seen > self.defs.len() {
                break;
            }
            id = next;
            seen += 1;
        }
        id
    }
}

/// The registry mapping each `#lang("tag")` to the def that carries it (§9.3).
/// Populated during collection; consumed by desugaring.
///
/// A tag may be claimed twice, once by `core` and once by the program: the
/// program's claim wins and `core`'s is the **default**. That rule is what makes
/// `#lang("panic_handler")` replaceable without any of the machinery being
/// special to panicking — `core` is by definition the fallback library, so
/// "found by tag, never by name" only holds up if a tag can be re-answered. Two
/// claims from the same side stay the duplicate error they were.
#[derive(Debug, Default)]
pub struct LangItems {
    map: HashMap<Symbol, Claim>,
}

/// One `#lang` claim: which def, and whether it came from `core`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Claim {
    def: DefId,
    from_core: bool,
}

impl LangItems {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tag → def`. `from_core` says whether the declaration lives in
    /// the `core` package. Returns the previous def only when the two claims
    /// genuinely collide — a duplicate-`#lang` error the caller reports; a
    /// program overriding one of `core`'s defaults returns `None`.
    pub fn set(&mut self, tag: Symbol, def: DefId, from_core: bool) -> Option<DefId> {
        let claim = Claim { def, from_core };
        match self.map.get(&tag).copied() {
            // `core` supplies a default for a tag the program already answered:
            // keep the program's.
            Some(prev) if from_core && !prev.from_core => None,
            // The program answers a tag `core` had a default for: take over.
            Some(prev) if !from_core && prev.from_core => {
                self.map.insert(tag, claim);
                None
            }
            Some(prev) => {
                self.map.insert(tag, claim);
                Some(prev.def)
            }
            None => {
                self.map.insert(tag, claim);
                None
            }
        }
    }

    pub fn get(&self, tag: &str) -> Option<DefId> {
        self.map.get(&Symbol::new(tag)).map(|c| c.def)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, DefId)> {
        self.map.iter().map(|(t, c)| (t, c.def))
    }

    /// Every claim with whether `core` made it, which is what setting it again
    /// in another session needs (`crate::library`).
    pub fn claims(&self) -> impl Iterator<Item = (&Symbol, DefId, bool)> {
        self.map.iter().map(|(t, c)| (t, c.def, c.from_core))
    }
}
