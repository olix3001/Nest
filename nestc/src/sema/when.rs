//! Conditional compilation: `#when(...)` on a declaration.
//!
//! A declaration whose condition does not hold is **removed from the tree**,
//! here, between parsing and collection. Nothing downstream ever sees it: it
//! defines no name, so it does not resolve, does not type-check, and is not
//! emitted. That order is the whole point — a namespace kept out of a build
//! must not resolve, not merely go unemitted, or a `tests` namespace naming a
//! symbol that this target does not have would still be an error.
//!
//! The condition is a small closed language rather than a Nest constant
//! expression, and it has to be: a constant expression is read by name
//! resolution, and this runs before name resolution. So `#when` reads what the
//! *build* is — facts [`Options`] already holds — and nothing a program
//! declares.
//!
//! ```text
//! #when(test)                                    a test build of this package
//! #when(os = .Macos)                             one operating system
//! #when(all(test, not(os = .Windows)))           combinators
//! #when(any(arch = .X86_64, arch = .Aarch64))
//! ```
//!
//! A value is a **variant of the enum `core/os.nest` declares for that key** —
//! `Os`, `Arch`, `Profile`, the same three a program reads at run time through
//! the generated `core/target.nest`. Written that way, `.Windows` in a
//! condition and `.Windows` in an `if` are the same word about the same type,
//! and a language server has something to offer completions from. What the
//! compiler does with it is still a string comparison: name resolution has not
//! run yet, so nothing here is *resolved* to that enum, only spelled as it.

use crate::common::diagnostic::Diagnostic;
use crate::common::options::{ARCHES, OSES, Options, PROFILES, variant_name};
use crate::common::source::{FileId, FileSpan};
use crate::common::symbol::Symbol;
use crate::parser::ast::{Ast, NodeId, NodeKind};

/// What a `#when` condition is evaluated against: the build, as this file sees
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conditions {
    /// `test` — a test build **of the package this file belongs to**.
    ///
    /// Per file, not per build, and for the same reason
    /// [`crate::sema::session::Session::entry_package_tests`] is: `--test`
    /// builds the *entry* package as a test binary, so a dependency compiled in
    /// the same invocation is not under test and keeps none of its tests.
    pub test: bool,
    /// `os = .Variant` — the target's operating system, one of [`OSES`].
    pub os: &'static str,
    /// `arch = .Variant` — the target's architecture, one of [`ARCHES`].
    pub arch: &'static str,
    /// `profile = .Variant` — the build profile's name, one of [`PROFILES`].
    pub profile: &'static str,
}

impl Conditions {
    /// What `file` is compiled under. `in_entry_package` is what decides
    /// `test`; see the field.
    pub fn new(options: &Options, in_entry_package: bool) -> Conditions {
        Conditions {
            test: options.test && in_entry_package,
            os: options.target.os,
            arch: options.target.arch,
            profile: options.profile,
        }
    }

    /// The value of a key, and the list its value is checked against.
    fn key(&self, name: &str) -> Option<(&'static str, &'static [&'static str])> {
        match name {
            "os" => Some((self.os, OSES)),
            "arch" => Some((self.arch, ARCHES)),
            "profile" => Some((self.profile, PROFILES)),
            _ => None,
        }
    }
}

/// The keys a condition may name, for the diagnostic that lists them.
const KEYS: &[&str] = &["os", "arch", "profile"];

/// Remove every declaration of `file` whose `#when(...)` does not hold.
///
/// Runs over the file's items and, for a namespace that is kept, over its items
/// in turn — a namespace excluded as a whole is not descended into, since its
/// contents are gone with it.
pub fn strip(ast: &Ast, file: FileId, conds: &Conditions, diagnostics: &mut Vec<Diagnostic>) {
    let Some(root) = ast.root() else { return };
    let mut stripper = Stripper {
        ast,
        file,
        conds,
        diagnostics,
    };
    stripper.items(root);
}

struct Stripper<'a> {
    ast: &'a Ast,
    file: FileId,
    conds: &'a Conditions,
    diagnostics: &'a mut Vec<Diagnostic>,
}

impl Stripper<'_> {
    /// Filter the item list `node` holds, then descend into what survived.
    fn items(&mut self, node: NodeId) {
        let items = match &self.ast.node(node).kind {
            NodeKind::File { items } => items.clone(),
            NodeKind::NamespaceExpr { items, .. } => items.clone(),
            _ => return,
        };
        let mut kept = Vec::with_capacity(items.len());
        for item in items {
            if self.keep(item) {
                kept.push(item);
            } else {
                self.erase(item);
            }
        }
        for &item in &kept {
            if let Some(body) = self.namespace_body(item) {
                self.items(body);
            }
        }
        match &mut self.ast.node_mut(node).kind {
            NodeKind::File { items } => *items = kept,
            NodeKind::NamespaceExpr { items, .. } => *items = kept,
            _ => {}
        }
    }

    /// Blank an excluded declaration's whole subtree.
    ///
    /// Unlinking it from its namespace's item list is not enough. Several
    /// passes walk the **arena** rather than the tree — `infer` gathers the
    /// functions to type-check by looking for every `FuncExpr` node in the
    /// file, not by asking the def table — so a body left in place would still
    /// be checked, and checked against names that were never resolved, since
    /// collection never saw it. Every node of the subtree becomes
    /// [`NodeKind::Error`], which every pass already steps over.
    fn erase(&self, node: NodeId) {
        let mut stack = vec![node];
        while let Some(id) = stack.pop() {
            stack.extend(self.ast.children(id));
            self.ast.node_mut(id).kind = NodeKind::Error;
        }
    }

    /// The `namespace { ... }` an item binds, if it binds one.
    fn namespace_body(&self, item: NodeId) -> Option<NodeId> {
        let rhs = self.rhs_of(item)?;
        matches!(self.ast.node(rhs).kind, NodeKind::NamespaceExpr { .. }).then_some(rhs)
    }

    /// The construct an item's binding is bound to (`f :: <rhs>`).
    fn rhs_of(&self, item: NodeId) -> Option<NodeId> {
        let inner = match &self.ast.node(item).kind {
            NodeKind::Decl { item, .. } => *item,
            _ => item,
        };
        match &self.ast.node(inner).kind {
            NodeKind::ConstBind { rhs, .. } => Some(*rhs),
            _ => None,
        }
    }

    /// Whether `item` survives. Several `#when` on one declaration all have to
    /// hold, which is what writing two of them plainly means.
    fn keep(&mut self, item: NodeId) -> bool {
        self.whens(item)
            .into_iter()
            .all(|d| self.directive(d).unwrap_or(true))
    }

    /// Every `#when` directive on `item`, written either before the binding
    /// (`#when(test)\nf :: func ...`) or on the construct itself
    /// (`f :: #when(test) func ...`). The two spellings mean the same thing, so
    /// both are read.
    fn whens(&self, item: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        if let NodeKind::Decl { directives, .. } = &self.ast.node(item).kind {
            out.extend(directives.iter().copied());
        }
        if let Some(rhs) = self.rhs_of(item) {
            out.extend(construct_directives(&self.ast.node(rhs).kind));
        }
        out.retain(
            |&d| matches!(&self.ast.node(d).kind, NodeKind::Directive { name, .. } if name.as_str() == "when"),
        );
        out
    }

    /// Evaluate one `#when(...)`. `None` is a condition that was not
    /// understood — already reported, and the declaration is **kept**, since
    /// deleting code over a diagnostic would hide every later error in it.
    fn directive(&mut self, directive: NodeId) -> Option<bool> {
        let args = match &self.ast.node(directive).kind {
            NodeKind::Directive { args, .. } => args.clone(),
            _ => return None,
        };
        if args.is_empty() {
            self.report(
                directive,
                "`#when` takes a condition: `#when(test)`, `#when(os = .Macos)`, or one of \
                 `all(...)`, `any(...)`, `not(...)` over them",
            );
            return None;
        }
        // A list at the top level is a conjunction, which is what `#when(a, b)`
        // reads as and what stacking two `#when` already means.
        self.all(&args)
    }

    /// Every one of `args` holds. A single unreadable argument poisons the
    /// whole condition rather than being skipped: a condition half of which was
    /// not understood has no truth value.
    fn all(&mut self, args: &[NodeId]) -> Option<bool> {
        let mut out = true;
        for &arg in args {
            out &= self.predicate(arg)?;
        }
        Some(out)
    }

    /// One predicate: a flag, a `key: "value"`, or a combinator.
    fn predicate(&mut self, arg: NodeId) -> Option<bool> {
        let (name, value) = match &self.ast.node(arg).kind {
            NodeKind::Arg { name, value } => (name.clone(), *value),
            _ => (None, arg),
        };
        match name {
            Some(key) => self.key_value(arg, &key, value),
            None => self.bare(value),
        }
    }

    /// `key = .Variant` — the build's value for `key` is that variant.
    fn key_value(&mut self, arg: NodeId, key: &Symbol, value: NodeId) -> Option<bool> {
        let Some((actual, allowed)) = self.conds.key(key.as_str()) else {
            self.report(
                arg,
                format!(
                    "`{key}` is not a condition `#when` knows; it reads {}, and the flag `test`",
                    list(KEYS)
                ),
            );
            return None;
        };
        let NodeKind::VariantLit { name: written, args } = &self.ast.node(value).kind else {
            self.report(
                value,
                format!(
                    "`{key}` is compared against a variant of the enum `core/os.nest` declares \
                     for it: `{key} = .{}`",
                    variant_name(actual)
                ),
            );
            return None;
        };
        if !matches!(args, crate::parser::ast::VariantArgs::None) {
            self.report(value, "a `#when` condition names a variant, and takes no payload");
            return None;
        }
        let written = written.clone();
        // Refused rather than answered `false`: a typo that silently excluded a
        // declaration on every target is a build that compiles and is missing
        // something, which is the worst shape a mistake can take.
        if !allowed.iter().any(|a| variant_name(a) == written.as_str()) {
            let names: Vec<String> = allowed.iter().map(|a| variant_name(a)).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            self.report(
                value,
                format!(
                    "`.{written}` is not a {key} this compiler knows; it has {}",
                    dotted(&names)
                ),
            );
            return None;
        }
        Some(written.as_str() == variant_name(actual))
    }

    /// A bare name: a combinator call, or a flag.
    fn bare(&mut self, value: NodeId) -> Option<bool> {
        match &self.ast.node(value).kind {
            NodeKind::Call { callee, args } => {
                let (callee, args) = (*callee, args.clone());
                self.combinator(value, callee, &args)
            }
            NodeKind::Path { segments } if segments.len() == 1 => {
                let name = segments[0].clone();
                match name.as_str() {
                    "test" => Some(self.conds.test),
                    "all" | "any" | "not" => {
                        self.report(
                            value,
                            format!("`{name}` takes conditions: `{name}(...)`"),
                        );
                        None
                    }
                    _ => {
                        self.report(
                            value,
                            format!(
                                "`{name}` is not a condition `#when` knows; the flag it has is \
                                 `test`, and it reads {}",
                                list(KEYS)
                            ),
                        );
                        None
                    }
                }
            }
            _ => {
                self.report(
                    value,
                    "a `#when` condition is the flag `test`, a `key = .Variant`, or one of \
                     `all(...)`, `any(...)`, `not(...)` over them",
                );
                None
            }
        }
    }

    /// `all(...)`, `any(...)` or `not(...)`.
    fn combinator(&mut self, call: NodeId, callee: NodeId, args: &[NodeId]) -> Option<bool> {
        let NodeKind::Path { segments } = &self.ast.node(callee).kind else {
            self.report(call, "a `#when` condition calls only `all`, `any` or `not`");
            return None;
        };
        let name = match segments.as_slice() {
            [one] => one.clone(),
            _ => {
                self.report(call, "a `#when` condition calls only `all`, `any` or `not`");
                return None;
            }
        };
        match name.as_str() {
            "all" => self.all(args),
            "any" => {
                let mut out = false;
                for &arg in args {
                    out |= self.predicate(arg)?;
                }
                Some(out)
            }
            "not" => match args {
                [one] => Some(!self.predicate(*one)?),
                _ => {
                    self.report(call, "`not` takes one condition");
                    None
                }
            },
            _ => {
                self.report(
                    call,
                    format!("`{name}` is not a `#when` combinator; they are `all`, `any` and `not`"),
                );
                None
            }
        }
    }

    fn report(&mut self, at: NodeId, message: impl Into<String>) {
        let span = self.ast.node(at).span;
        self.diagnostics
            .push(Diagnostic::error(message).with_primary(FileSpan::new(self.file, span), ""));
    }
}

/// The directives written on a construct itself (`f :: #when(test) func ...`).
fn construct_directives(kind: &NodeKind) -> Vec<NodeId> {
    match kind {
        NodeKind::NamespaceExpr { directives, .. }
        | NodeKind::FuncExpr { directives, .. }
        | NodeKind::StructType { directives, .. }
        | NodeKind::EnumType { directives, .. }
        | NodeKind::TraitType { directives, .. }
        | NodeKind::DistinctType { directives, .. } => directives.clone(),
        _ => Vec::new(),
    }
}

/// `.A`, `.B` and `.C` — a variant list for a diagnostic.
fn dotted(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`.{n}`")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// `a`, `b` and `c` — a list for a diagnostic.
fn list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}
