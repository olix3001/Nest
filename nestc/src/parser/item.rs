//! Top-level items, statements, declarations, decorations, and the remaining
//! `::`-RHS forms (`impl`, `extern` blocks, `import`, `namespace`). All `impl
//! Parser`.

use crate::common::span::Span;
use crate::common::symbol::Symbol;

use super::ast::{AssignOp, ImportPath, NodeId, NodeKind};
use super::lexer::TokenKind;
use super::parse::Parser;

/// What a statement's leading tokens turn out to introduce (decided by
/// [`Parser::scan_binding_kind`] without allocating).
enum BindingKind {
    /// `pattern :: rhs`
    Const,
    /// `place op= expr`
    Assign,
    /// a bare expression
    Expr,
}

impl Parser {
    // ===< File / namespace item lists >===

    /// Parse the whole file as an anonymous namespace, returning the
    /// [`NodeKind::File`] node.
    pub(crate) fn file_root(&mut self) -> NodeId {
        let start = self.cur_span();
        let mut items = Vec::new();
        while !self.at_eof() {
            self.skip_newlines();
            if self.at_eof() {
                break;
            }
            self.parse_file_item(&mut items);
        }
        let end = items.last().map_or(start, |&i| self.node_span(i));
        self.alloc(start.to(end), NodeKind::File { items })
    }

    /// Parse items until `closer` (or end of input), collecting into `out`.
    fn parse_items_until(&mut self, closer: &TokenKind, out: &mut Vec<NodeId>) {
        loop {
            self.skip_newlines();
            if self.at(closer) || self.at_eof() {
                break;
            }
            self.parse_file_item(out);
        }
    }

    /// Parse a single file/namespace item, pushing its node(s). An `extern`
    /// block expands to several bindings, hence the `out` sink.
    fn parse_file_item(&mut self, out: &mut Vec<NodeId>) {
        let start = self.cur_span();
        let attrs = self.parse_attributes();
        let directives = self.parse_directives();
        let decorated = !attrs.is_empty() || !directives.is_empty();

        match self.peek() {
            Some(TokenKind::ImplKw) => {
                if decorated {
                    self.error(
                        start,
                        "attributes/directives are not allowed on an `impl` block",
                    );
                }
                let node = self.parse_impl();
                out.push(node);
            }
            // An `extern("c") { ... }` block heads an item; `name :: extern("c")
            // func ...` instead starts with a pattern, so a leading `extern` is
            // unambiguously the block form.
            Some(TokenKind::ExternKw) => {
                if decorated {
                    self.error(
                        start,
                        "attributes/directives are not allowed on an `extern` block",
                    );
                }
                self.parse_extern_block(out);
            }
            Some(TokenKind::LetKw | TokenKind::ConstKw) => {
                self.reject_static_let(&directives, start);
                let ld = self.parse_local_decl();
                out.push(self.finish_decl(attrs, directives, ld, start));
            }
            // `$assert(...)` and friends: a comptime item.
            Some(TokenKind::Ident(s)) if s.as_str().starts_with('$') => {
                if decorated {
                    self.error(
                        start,
                        "attributes/directives are not allowed on a comptime item",
                    );
                }
                let e = self.parse_expr();
                out.push(e);
            }
            _ => {
                let is_static = self.has_directive(&directives, "static");
                let cb = self.parse_const_bind(is_static);
                out.push(self.finish_decl(attrs, directives, cb, start));
            }
        }
    }

    /// Wrap `item` in a [`NodeKind::Decl`] iff it carries decorations; otherwise
    /// return it bare.
    fn finish_decl(
        &mut self,
        attrs: Vec<NodeId>,
        directives: Vec<NodeId>,
        item: NodeId,
        start: Span,
    ) -> NodeId {
        if attrs.is_empty() && directives.is_empty() {
            return item;
        }
        let span = start.to(self.node_span(item));
        self.alloc(
            span,
            NodeKind::Decl {
                attrs,
                directives,
                item,
            },
        )
    }

    // ===< Decorations >===

    /// `{ '@' name [ '(' args ')' ] }` — the attributes preceding a declaration.
    pub(crate) fn parse_attributes(&mut self) -> Vec<NodeId> {
        let mut attrs = Vec::new();
        loop {
            // Attributes each sit on their own line before the declaration.
            self.skip_newlines();
            if !self.at(&TokenKind::At) {
                break;
            }
            let start = self.cur_span();
            self.bump();
            let name = self.expect_ident();
            let (args, end) = if self.at(&TokenKind::LParen) {
                self.parse_call_args()
            } else {
                (Vec::new(), start)
            };
            attrs.push(self.alloc(start.to(end), NodeKind::Attribute { name, args }));
        }
        attrs
    }

    /// `{ '#' name [ '(' args ')' ] }` — a run of directives. `#const` is
    /// accepted even though `const` is a keyword. The directive name is not
    /// validated here — the set (`#packed`, `#inline`, `#lang`, …) is a semantic
    /// concern; `#lang("add")` parses as any other `#name(args)` directive.
    pub(crate) fn parse_directives(&mut self) -> Vec<NodeId> {
        let mut directives = Vec::new();
        loop {
            self.skip_newlines();
            if !self.at(&TokenKind::Hash) {
                break;
            }
            let start = self.cur_span();
            self.bump();
            let name = if self.eat(&TokenKind::ConstKw) {
                Symbol::new("const")
            } else {
                self.expect_ident()
            };
            let (args, end) = if self.at(&TokenKind::LParen) {
                self.parse_call_args()
            } else {
                (Vec::new(), start)
            };
            directives.push(self.alloc(start.to(end), NodeKind::Directive { name, args }));
        }
        directives
    }

    // ===< Statements >===

    /// Parse one statement, returning its node and whether it is a bare
    /// expression (which, when it is the block's last item, becomes the tail).
    pub(crate) fn parse_stmt(&mut self) -> (NodeId, bool) {
        let start = self.cur_span();
        let attrs = self.parse_attributes();
        let directives = self.parse_directives();
        let decorated = !attrs.is_empty() || !directives.is_empty();

        match self.peek() {
            Some(TokenKind::LetKw | TokenKind::ConstKw) => {
                self.reject_static_let(&directives, start);
                let ld = self.parse_local_decl();
                (self.finish_decl(attrs, directives, ld, start), false)
            }
            Some(TokenKind::DeferKw) if !decorated => (self.parse_defer(), false),
            Some(TokenKind::ReturnKw) if !decorated => (self.parse_return(), false),
            Some(TokenKind::BreakKw) if !decorated => (self.parse_break(), false),
            Some(TokenKind::ContinueKw) if !decorated => {
                let span = self.cur_span();
                self.bump();
                (self.alloc(span, NodeKind::Continue), false)
            }
            _ if decorated => {
                // Any decorated statement that is not a `let`/`const` is a `::`
                // binding — `#static calls :: uint := 0`, the function-local form
                // of §2.6, among them.
                let is_static = self.has_directive(&directives, "static");
                let cb = self.parse_const_bind(is_static);
                (self.finish_decl(attrs, directives, cb, start), false)
            }
            _ => match self.scan_binding_kind() {
                BindingKind::Const => (self.parse_const_bind(false), false),
                BindingKind::Assign => (self.parse_assign(), false),
                BindingKind::Expr => (self.parse_expr(), true),
            },
        }
    }

    /// Classify the statement at the cursor by scanning for the first top-level
    /// `::` (a `::` binding), assignment operator (an assignment), or statement
    /// terminator (a bare expression) — without allocating or consuming.
    fn scan_binding_kind(&self) -> BindingKind {
        let mut depth = 0i32;
        let mut i = 0;
        while let Some(kind) = self.peek_nth(i) {
            match kind {
                TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                | TokenKind::DotLt
                | TokenKind::DotLBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                // Close a `.< … >` turbofish so generic calls do not leave depth
                // stuck above zero.
                TokenKind::Gt | TokenKind::GtEq if depth > 0 => depth -= 1,
                TokenKind::Shr if depth > 0 => depth = (depth - 2).max(0),
                TokenKind::Newline | TokenKind::Semicolon if depth == 0 => break,
                TokenKind::ColonColon if depth == 0 => return BindingKind::Const,
                TokenKind::Eq
                | TokenKind::PlusEq
                | TokenKind::MinusEq
                | TokenKind::StarEq
                | TokenKind::SlashEq
                | TokenKind::PercentEq
                    if depth == 0 =>
                {
                    return BindingKind::Assign;
                }
                _ => {}
            }
            i += 1;
        }
        BindingKind::Expr
    }

    /// `( 'let' | 'const' ) pattern [ ':' type ] ':=' expr`.
    fn parse_local_decl(&mut self) -> NodeId {
        let start = self.cur_span();
        let is_const = self.at(&TokenKind::ConstKw);
        self.bump(); // 'let' or 'const'
        let pattern = self.parse_pattern();
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type())
        } else {
            None
        };
        self.expect(&TokenKind::ColonEq);
        let value = self.parse_expr();
        let span = start.to(self.node_span(value));
        self.alloc(
            span,
            NodeKind::LocalDecl {
                is_const,
                pattern,
                ty,
                value,
            },
        )
    }

    /// `place assign_op expr`.
    fn parse_assign(&mut self) -> NodeId {
        let start = self.cur_span();
        let place = self.parse_expr();
        let op = match self.peek() {
            Some(TokenKind::Eq) => AssignOp::Assign,
            Some(TokenKind::PlusEq) => AssignOp::Add,
            Some(TokenKind::MinusEq) => AssignOp::Sub,
            Some(TokenKind::StarEq) => AssignOp::Mul,
            Some(TokenKind::SlashEq) => AssignOp::Div,
            Some(TokenKind::PercentEq) => AssignOp::Rem,
            _ => {
                let span = self.cur_span();
                return self.error_node(span, "expected an assignment operator");
            }
        };
        self.bump();
        let value = self.parse_expr();
        let span = start.to(self.node_span(value));
        self.alloc(span, NodeKind::Assign { op, place, value })
    }

    /// `defer ( expr | block )`.
    fn parse_defer(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let body = if self.at(&TokenKind::LBrace) {
            self.parse_block()
        } else {
            self.parse_expr()
        };
        self.alloc(start.to(self.node_span(body)), NodeKind::Defer { body })
    }

    /// `return [ expr ]`.
    fn parse_return(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let value = if self.at_stmt_end() {
            None
        } else {
            Some(self.parse_expr())
        };
        let end = value.map_or(start, |v| self.node_span(v));
        self.alloc(start.to(end), NodeKind::Return { value })
    }

    /// `break [ expr ]`.
    fn parse_break(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let value = if self.at_stmt_end() {
            None
        } else {
            Some(self.parse_expr())
        };
        let end = value.map_or(start, |v| self.node_span(v));
        self.alloc(start.to(end), NodeKind::Break { value })
    }

    // ===< `::` bindings and their RHS >===

    /// `pattern '::' const_rhs`.
    fn parse_const_bind(&mut self, static_storage: bool) -> NodeId {
        let start = self.cur_span();
        let pattern = self.parse_pattern();
        self.expect(&TokenKind::ColonColon);
        // `#static name :: T [ ':=' init ]` (§2.6). The directive is what decides
        // how to read the RHS: without it `name :: [4096]u8` is a *type alias*
        // and `name :: 0` is a value, because a `::` RHS holds either and only
        // its shape says which. A static declares neither — it declares a
        // **region**, so the RHS is its type and the value, if any, comes after
        // `:=`. That is exactly the associated-constant shape (`MAX :: i32 :=
        // 100`), and it reuses the same parse.
        let rhs = if static_storage {
            self.parse_assoc_const()
        } else {
            self.typed_const_rhs()
        };
        self.alloc(
            start.to(self.node_span(rhs)),
            NodeKind::ConstBind { pattern, rhs },
        )
    }

    /// The RHS of an ordinary `::` binding, admitting the **typed constant**
    /// form `A :: u8 := 5` (§2.5).
    ///
    /// A constant with no declared type keeps its literal's comptime-ness and
    /// settles per use site, which is usually what is wanted; writing the type
    /// pins it instead. The two are told apart by what follows: `A :: u8` is a
    /// type alias, and `A :: u8 := 5` is a `u8` constant. So the RHS is parsed
    /// first and only *then* re-read as a type, when a `:=` turns out to follow
    /// it — the same `T := value` shape an associated constant and a `#static`
    /// region already use, so there is one rule for where a written type goes.
    fn typed_const_rhs(&mut self) -> NodeId {
        let rhs = self.parse_const_rhs();
        if !self.at(&TokenKind::ColonEq) {
            return rhs;
        }
        self.bump();
        let value = self.parse_expr();
        let span = self.node_span(rhs).to(self.node_span(value));
        self.alloc(
            span,
            NodeKind::AssocConst {
                ty: rhs,
                default: Some(value),
            },
        )
    }

    /// `#static` decorates a `::` binding, never a `let` (§2.6).
    ///
    /// A static declares a **region**, not a binding: its RHS is a type and its
    /// value, if any, follows `:=`. That is the `::` shape, and it is the same
    /// one everywhere — at namespace scope, where `let` has no home at all, and
    /// inside a function, where `#static let` was once the only form that
    /// spelled a program-lifetime local differently from the global it behaves
    /// exactly like.
    fn reject_static_let(&mut self, directives: &[NodeId], start: Span) {
        if self.has_directive(directives, "static") {
            self.error(
                start,
                "`#static` decorates a `::` binding, not a `let`: write \
                 `#static name :: T := value`",
            );
        }
    }

    /// Whether `directives` — already parsed, sitting in front of a binding —
    /// contains `#name`.
    pub(crate) fn has_directive(&self, directives: &[NodeId], name: &str) -> bool {
        directives.iter().any(|&d| {
            self.with_kind(d, |k| {
                matches!(k, NodeKind::Directive { name: n, .. } if n.as_str() == name)
            })
        })
    }

    /// The RHS of a `::` binding: an `import`, a directive-led / keyword-led type
    /// or `func` literal, or a value expression.
    fn parse_const_rhs(&mut self) -> NodeId {
        if self.at(&TokenKind::ImportKw) {
            return self.parse_import();
        }
        if self.at(&TokenKind::Hash) {
            let directives = self.parse_directives();
            return match self.peek() {
                Some(TokenKind::FuncKw | TokenKind::ExternKw) => self.parse_func_expr(directives),
                Some(TokenKind::StructKw) => self.parse_struct_type(directives),
                Some(TokenKind::EnumKw) => self.parse_enum_type(directives),
                Some(TokenKind::TraitKw) => self.parse_trait_type(directives),
                Some(TokenKind::NamespaceKw) => self.parse_namespace(directives),
                Some(TokenKind::LBracket) => self.parse_bracket_type(directives),
                // `#lang("str") distinct []u8` — a `distinct` type is a
                // declaration and carries directives like any other.
                Some(TokenKind::DistinctKw) => {
                    let start = directives
                        .first()
                        .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
                    self.parse_distinct_type(directives, start)
                }
                _ => {
                    for &d in &directives {
                        let span = self.node_span(d);
                        self.error(
                            span,
                            "these directives do not modify the following construct",
                        );
                    }
                    self.parse_type()
                }
            };
        }
        match self.peek() {
            Some(TokenKind::FuncKw | TokenKind::ExternKw) => self.parse_func_expr(Vec::new()),
            Some(TokenKind::StructKw) => self.parse_struct_type(Vec::new()),
            Some(TokenKind::EnumKw) => self.parse_enum_type(Vec::new()),
            Some(TokenKind::TraitKw) => self.parse_trait_type(Vec::new()),
            Some(TokenKind::NamespaceKw) => self.parse_namespace(Vec::new()),
            // Explicit type-forming syntax on the RHS (aliases like `*T`, `[]T`,
            // `dyn T`, `distinct T`).
            Some(
                TokenKind::DistinctKw | TokenKind::DynKw | TokenKind::Star | TokenKind::LBracket,
            ) => self.parse_type(),
            // Otherwise a value expression (a bare name alias reads as a `Path`).
            _ => self.parse_expr(),
        }
    }

    /// `namespace { items }`, with any leading directives supplied by the caller.
    fn parse_namespace(&mut self, directives: Vec<NodeId>) -> NodeId {
        let start = directives
            .first()
            .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
        self.expect(&TokenKind::NamespaceKw);
        self.expect(&TokenKind::LBrace);
        let mut items = Vec::new();
        self.parse_items_until(&TokenKind::RBrace, &mut items);
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        self.alloc(start.to(end), NodeKind::NamespaceExpr { directives, items })
    }

    /// `impl [<g>] type [ 'for' target ] { items }`.
    fn parse_impl(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // 'impl'
        let generics = self.parse_generics();
        let ty = self.parse_type();
        let for_ty = if self.eat(&TokenKind::ForKw) {
            Some(self.parse_type())
        } else {
            None
        };
        self.expect(&TokenKind::LBrace);
        let mut items = Vec::new();
        self.parse_items_until(&TokenKind::RBrace, &mut items);
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        self.alloc(
            start.to(end),
            NodeKind::ImplBlock {
                generics,
                ty,
                for_ty,
                items,
            },
        )
    }

    /// `extern '(' abi ')' '{' declaration* '}'` — desugars each bodyless member
    /// to a standalone binding carrying that ABI (no distinct block node).
    fn parse_extern_block(&mut self, out: &mut Vec<NodeId>) {
        self.bump(); // 'extern'
        self.expect(&TokenKind::LParen);
        let abi = match self.peek() {
            Some(TokenKind::Str(s)) => {
                let sym = Symbol::new(s);
                self.bump();
                sym
            }
            _ => {
                let span = self.cur_span();
                self.error(span, "expected an ABI string after `extern(`");
                Symbol::new("c")
            }
        };
        self.expect(&TokenKind::RParen);
        self.expect(&TokenKind::LBrace);
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            let start = self.cur_span();
            let attrs = self.parse_attributes();
            let member = self.parse_const_bind(false);
            self.set_extern_abi(member, &abi);
            out.push(self.finish_decl(attrs, Vec::new(), member, start));
            self.skip_newlines();
            self.eat(&TokenKind::Comma);
        }
        self.expect(&TokenKind::RBrace);
    }

    /// Stamp the ABI onto the `func` RHS of an `extern` block member.
    fn set_extern_abi(&self, bind: NodeId, abi: &Symbol) {
        let rhs = self.with_kind(bind, |k| match k {
            NodeKind::ConstBind { rhs, .. } => Some(*rhs),
            _ => None,
        });
        if let Some(rhs) = rhs {
            let mut kind = self.clone_kind(rhs);
            if let NodeKind::FuncExpr { extern_abi, .. } = &mut kind {
                *extern_abi = Some(abi.clone());
            }
            let span = self.node_span(rhs);
            self.set_node(rhs, span, kind);
        }
    }

    /// `import ( '<' pkg_path '>' | string )` — always the RHS of a `::`.
    pub(crate) fn parse_import(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // 'import'
        match self.peek() {
            Some(TokenKind::Str(s)) => {
                let path = ImportPath::File(s.clone());
                let end = self.cur_span();
                self.bump();
                self.alloc(start.to(end), NodeKind::Import { path })
            }
            Some(TokenKind::Lt) => {
                self.bump(); // '<'
                let mut segments = vec![self.expect_ident()];
                while self.eat(&TokenKind::Slash) {
                    segments.push(self.expect_ident());
                }
                let end = self.cur_span();
                if !self.eat_gt() {
                    self.error(end, "expected `>` to close a package import path");
                }
                self.alloc(
                    start.to(end),
                    NodeKind::Import {
                        path: ImportPath::Package(segments),
                    },
                )
            }
            _ => {
                let span = self.cur_span();
                self.error_node(span, "expected `<pkg/path>` or a \"file\" after `import`")
            }
        }
    }
}
