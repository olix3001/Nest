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
use crate::parser::ast::{FileId, NodeId};

/// A global definition id: an index into a [`DefTable`]. Stable across files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

/// Whether a def is exported from its enclosing namespace (§4.4). Only `@public`
/// items are reachable through `import` / qualified access from the outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

impl Visibility {
    pub fn is_public(self) -> bool {
        matches!(self, Visibility::Public)
    }
}

/// What a [`Def`] denotes. Drives both diagnostics and how member lookup treats
/// the def (namespace-like kinds own a populated [`Namespace`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A `struct`/`enum` record field.
    Field,
    /// An `enum` variant.
    Variant,
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
    /// A builtin `$`-intrinsic.
    Intrinsic,
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
            DefKind::Enum => "enum",
            DefKind::Trait => "trait",
            DefKind::TypeAlias => "type",
            DefKind::Const => "const",
            DefKind::Func => "func",
            DefKind::Field => "field",
            DefKind::Variant => "variant",
            DefKind::Param => "param",
            DefKind::TypeParam => "typeparam",
            DefKind::ConstParam => "constparam",
            DefKind::Local => "local",
            DefKind::Import => "import",
            DefKind::Primitive => "primitive",
            DefKind::Intrinsic => "intrinsic",
            DefKind::External => "external",
        }
    }
}

/// The members a namespace-like [`Def`] exposes, split by how they got there so
/// resolution can honor the §4.6 search order and diagnose glob conflicts.
#[derive(Debug, Default, Clone)]
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

/// One `#name(args...)` directive as written on a definition (§9).
///
/// The name is kept verbatim and the arguments are kept as the small literal
/// vocabulary directives draw on — `#align(16)`, `#lang("add")`,
/// `#link_name("printf")`. Nothing here interprets them; a directive this
/// front end has no opinion about still travels, so adding one later is a
/// matter of reading it where it matters rather than re-plumbing it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// One definition. Leaf defs (locals, params, fields) leave `ns` empty; the
/// namespace-like kinds populate it during collection and import wiring.
#[derive(Debug, Clone)]
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
    /// For an [`DefKind::Import`] binding: the def it aliases (a namespace or a
    /// selected member). Following `alias` reaches the real target.
    pub alias: Option<DefId>,
    /// For a [`DefKind::Field`]: the field carries `@using`, so its struct
    /// implicitly upcasts to the field's type (§3.10). At most one field per
    /// struct may set this.
    pub using: bool,
}

impl Def {
    /// The def an [`DefKind::Import`] alias ultimately points at (or itself).
    pub fn target(&self) -> DefId {
        self.alias.unwrap_or(self.id)
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
            alias: None,
            using: false,
        });
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

/// The registry mapping each `#lang("tag")` to the single def that carries it
/// (§9.3). Populated during collection; consumed by desugaring.
#[derive(Debug, Default)]
pub struct LangItems {
    map: HashMap<Symbol, DefId>,
}

impl LangItems {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tag → def`. Returns the previous def if the tag was already
    /// claimed (a duplicate-`#lang` error the caller reports).
    pub fn set(&mut self, tag: Symbol, def: DefId) -> Option<DefId> {
        self.map.insert(tag, def)
    }

    pub fn get(&self, tag: &str) -> Option<DefId> {
        self.map.get(&Symbol::new(tag)).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &DefId)> {
        self.map.iter()
    }
}
