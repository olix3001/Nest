//! Top-level items, statements, declarations, decorations, and the remaining
//! `::`-RHS forms (`impl`, `extern` blocks, `import`, `namespace`). All `impl
//! Parser`.

use crate::common::span::Span;
use crate::common::symbol::Symbol;

use super::ast::{AssignOp, ImportPath, Lit, NodeId, NodeKind, VariantArgs};
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
        let mut attrs = self.parse_documented_attributes();
        // An `impl` or `extern` block declares nothing a doc could be recorded
        // on, so a `///` above one is the comment it looks like.
        if matches!(self.peek(), Some(TokenKind::ImplKw | TokenKind::ExternKw)) {
            attrs.retain(|&a| !matches!(&self.node_kind(a), NodeKind::Attribute { name, .. } if name.as_str() == "doc"));
        }
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
            // `assert(...)` and friends: a comptime item (§6.10).
            _ if self.at_comptime_item() => {
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
        self.parse_attributes_with(false)
    }

    /// [`Parser::parse_attributes`] where a declaration is certain to follow —
    /// a namespace item, a field, a variant, a trait member — so a `///` doc
    /// comment above it is its `@doc("...")` (§9.2). The doc is the first
    /// attribute, whatever order it was written in: attributes are unordered.
    pub(crate) fn parse_documented_attributes(&mut self) -> Vec<NodeId> {
        self.parse_attributes_with(true)
    }

    fn parse_attributes_with(&mut self, docs: bool) -> Vec<NodeId> {
        let mut attrs = Vec::new();
        let mut doc: Option<(Span, String)> = None;
        loop {
            // Attributes each sit on their own line before the declaration.
            self.skip_newlines();
            if docs && let Some((span, text)) = self.doc_before() {
                doc = Some(match doc {
                    Some((s, t)) => (s.to(span), format!("{t}\n{text}")),
                    None => (span, text),
                });
            }
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
        if let Some((span, text)) = doc {
            let lit = self.alloc(span, NodeKind::Lit(Lit::Str(text)));
            let attr = self.alloc(
                span,
                NodeKind::Attribute {
                    name: Symbol::new("doc"),
                    args: vec![lit],
                },
            );
            attrs.insert(0, attr);
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
            let (args, end) = if !self.at(&TokenKind::LParen) {
                (Vec::new(), start)
            } else if name.as_str() == "when" {
                self.parse_when_args()
            } else {
                self.parse_call_args()
            };
            directives.push(self.alloc(start.to(end), NodeKind::Directive { name, args }));
        }
        directives
    }

    /// `'(' cond { ',' cond } ')'` — the argument list of `#when`, which has a
    /// grammar of its own.
    ///
    /// Two things keep it out of the expression parser that every other
    /// directive's arguments go through. `not` is a **keyword** (`not x`), so
    /// `not(os = .Linux)` reads there as the operator applied to a parenthesis,
    /// and `=` is a statement in this language, not an expression. And a
    /// condition is not an expression in the first place: it is read before
    /// name resolution and names nothing the program declares
    /// (`crate::sema::when`).
    ///
    /// The nodes are the ordinary ones — an [`NodeKind::Arg`] per condition,
    /// a [`NodeKind::Call`] for a combinator — so that everything downstream of
    /// the parser sees one shape.
    fn parse_when_args(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::LParen);
        let args = self.parse_when_list();
        let end = self.cur_span();
        self.expect(&TokenKind::RParen);
        (args, end)
    }

    /// `cond { ',' cond }`, up to but not consuming the closing paren.
    fn parse_when_list(&mut self) -> Vec<NodeId> {
        let mut args = Vec::new();
        self.skip_newlines();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            args.push(self.parse_when_cond());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        args
    }

    /// One condition: `key = .Variant`, `name(...)`, or a bare flag.
    fn parse_when_cond(&mut self) -> NodeId {
        let start = self.cur_span();
        // `not` is a keyword; as the head of a condition it is a name.
        let name = if self.eat(&TokenKind::NotKw) {
            Symbol::new("not")
        } else {
            self.expect_ident()
        };
        let head = self.alloc(
            start,
            NodeKind::Path {
                segments: vec![name.clone()],
            },
        );
        if self.eat(&TokenKind::Eq) {
            // The value is a **variant literal** of the enum `core/os.nest`
            // declares for the key — `.Windows` for `os` — so that a reader,
            // and a language server, are looking at the same `Os` the program
            // reads at run time through `core/target.nest`.
            let at = self.cur_span();
            let value = if self.eat(&TokenKind::Dot) {
                let end = self.cur_span();
                let variant = self.expect_ident();
                self.alloc(
                    at.to(end),
                    NodeKind::VariantLit {
                        name: variant,
                        args: VariantArgs::None,
                    },
                )
            } else {
                self.error_node(
                    at,
                    format!("expected a variant, found {}", self.describe_next()),
                )
            };
            return self.alloc(
                start.to(self.node_span(value)),
                NodeKind::Arg {
                    name: Some(name),
                    value,
                },
            );
        }
        let value = if self.at(&TokenKind::LParen) {
            self.bump();
            let inner = self.parse_when_list();
            let end = self.cur_span();
            self.expect(&TokenKind::RParen);
            self.alloc(
                start.to(end),
                NodeKind::Call {
                    callee: head,
                    args: inner,
                },
            )
        } else {
            head
        };
        let span = start.to(self.node_span(value));
        self.alloc(span, NodeKind::Arg { name: None, value })
    }

    // ===< Statements >===

    /// Parse one statement, returning its node and whether it is a bare
    /// expression (which, when it is the block's last item, becomes the tail).
    pub(crate) fn parse_stmt(&mut self) -> (NodeId, bool) {
        let start = self.cur_span();
        let attrs = self.parse_attributes();
        let directives = self.parse_directives();
        let decorated = !attrs.is_empty() || !directives.is_empty();

        // `#comptime` is taken first, whatever follows it: the directive says
        // the statement is evaluated when the program is compiled, and the one
        // statement that means for is a `for` (§9's directive family).
        if self.has_directive(&directives, "comptime") {
            if matches!(self.peek(), Some(TokenKind::ForKw)) {
                return (self.parse_comptime_for(start), false);
            }
            self.error(
                start,
                "`#comptime` applies to a `for` loop: `#comptime for i in 0..<4 { … }`",
            );
        }
        match self.peek() {
            Some(TokenKind::LetKw | TokenKind::ConstKw) => {
                self.reject_static_let(&directives, start);
                let ld = self.parse_local_decl();
                (self.finish_decl(attrs, directives, ld, start), false)
            }
            // A local `impl`, for a type the block declares (or any other the
            // rules allow): a definition among the statements, like a local
            // `func` or `struct`.
            Some(TokenKind::ImplKw) if !decorated => (self.parse_impl(), false),
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
                // binding — `#static calls: uint :: 0`, the function-local form
                // of §2.6, among them.
                let is_static = self.has_directive(&directives, "static");
                let cb = self.parse_const_bind(is_static);
                (self.finish_decl(attrs, directives, cb, start), false)
            }
            _ => match self.scan_binding_kind() {
                BindingKind::Const => (self.parse_const_bind(false), false),
                BindingKind::Assign => self.parse_assign(),
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

    /// `place assign_op expr`, and what to do when it is not one after all.
    ///
    /// Answers the pair [`Parser::parse_stmt`] answers: the node, and whether it
    /// is an **expression** (a block's tail value) rather than a statement.
    ///
    /// [`Parser::scan_binding_kind`] classifies by looking ahead to the end of
    /// the **line**, and a line may hold several statements — `f(x) n = 1` is
    /// two. So an assignment operator it found is not necessarily in the
    /// statement at the cursor, and when it is not, what was parsed here is a
    /// complete expression statement and the assignment belongs to the next turn
    /// of the caller's loop. Reporting "expected an assignment operator" instead
    /// rejects a program that is written correctly.
    fn parse_assign(&mut self) -> (NodeId, bool) {
        let start = self.cur_span();
        let place = self.parse_expr();
        let op = match self.peek() {
            Some(TokenKind::Eq) => AssignOp::Assign,
            Some(TokenKind::PlusEq) => AssignOp::Add,
            Some(TokenKind::MinusEq) => AssignOp::Sub,
            Some(TokenKind::StarEq) => AssignOp::Mul,
            Some(TokenKind::SlashEq) => AssignOp::Div,
            Some(TokenKind::PercentEq) => AssignOp::Rem,
            _ => return (place, true),
        };
        self.bump();
        let value = self.parse_expr();
        let span = start.to(self.node_span(value));
        (
            self.alloc(span, NodeKind::Assign { op, place, value }),
            false,
        )
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
        // `NAME: T :: value` — a **typed constant** (§2.5) — and
        // `#static NAME: T [:= value]` — a **region** (§2.6). The type comes
        // before either operator, and that is the whole point: it leaves
        // `NAME :: T` meaning a type alias in every case.
        //
        // **The two operators are not interchangeable.** `::` binds a name to a
        // value the compiler knows, and `:=` initializes storage a program can
        // write to — which is the same split `let x: T := v` already makes
        // inside a function body. A `#static` is storage, so it takes `:=`.
        if self.eat(&TokenKind::Colon) {
            let ty = self.parse_type();
            let mut end = self.node_span(ty);
            let want = if static_storage {
                TokenKind::ColonEq
            } else {
                TokenKind::ColonColon
            };
            let value = if self.eat(&want) {
                let v = self.parse_expr();
                end = self.node_span(v);
                Some(v)
            } else if self.at(&TokenKind::ColonColon) || self.at(&TokenKind::ColonEq) {
                // The other operator. It parses, because what follows it is an
                // expression either way, and saying which one this declaration
                // wanted is more use than "unexpected token".
                let op = self.cur_span();
                self.bump();
                let v = self.parse_expr();
                end = self.node_span(v);
                if static_storage {
                    self.error(
                        op,
                        "a `#static` is a region, not a constant: write \
                         `#static NAME: T := value`",
                    );
                } else {
                    self.error(
                        op,
                        "a constant is bound with `::`; `:=` initializes a region, which needs \
                         `#static`",
                    );
                }
                Some(v)
            } else {
                // A static is storage, and storage is zeroed; every other
                // binding is its value and has nowhere to get one from.
                if !static_storage {
                    self.error(
                        start.to(end),
                        "a typed constant needs a value: write `NAME: T :: value`",
                    );
                }
                None
            };
            let rhs = self.alloc(
                self.node_span(ty).to(end),
                NodeKind::AssocConst { ty, default: value },
            );
            return self.alloc(start.to(end), NodeKind::ConstBind { pattern, rhs });
        }
        self.expect(&TokenKind::ColonColon);
        // A region has a width, and the zeroed form has no initializer to infer
        // one from, so the type is required rather than optional (§2.6).
        if static_storage {
            let span = start.to(self.cur_span());
            self.error(
                span,
                "a `#static` needs an explicit type: write `#static NAME: T := value`, \
                 or `#static NAME: T` for a zeroed region",
            );
        }
        let rhs = self.parse_const_rhs();
        // The retired spelling. `A: u8 :: 5` said the same thing this used to
        // parse, and it is worth naming rather than leaving as "unexpected".
        if self.at(&TokenKind::ColonEq) {
            self.bump();
            let value = self.parse_expr();
            self.error(
                self.node_span(rhs).to(self.node_span(value)),
                "a constant's type goes before the `::`: write `NAME: T :: value`",
            );
        }
        self.alloc(
            start.to(self.node_span(rhs)),
            NodeKind::ConstBind { pattern, rhs },
        )
    }

    /// `#static` decorates a `::` binding, never a `let` (§2.6).
    ///
    /// A static declares a **region**, not a binding: its type comes before the
    /// operator and its value, if any, follows `:=`. What `#static` adds to a
    /// `let` is **lifetime**, not mutability — the region outlives the frame —
    /// and that is the whole of the difference between the two spellings, at
    /// namespace scope where `let` has no home at all and inside a function
    /// where it does.
    fn reject_static_let(&mut self, directives: &[NodeId], start: Span) {
        if self.has_directive(directives, "static") {
            self.error(
                start,
                "`#static` is not a `let`: write `#static name: T := value`. The two \
                 differ in lifetime — a `#static` region outlives the frame",
            );
        }
    }

    /// Whether `directives` — already parsed, sitting in front of a binding —
    /// contains `#name`.
    pub(crate) fn has_directive(&self, directives: &[NodeId], name: &str) -> bool {
        directives.iter().any(|&d| {
            self.with_kind(
                d,
                |k| matches!(k, NodeKind::Directive { name: n, .. } if n.as_str() == name),
            )
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
    ///
    /// A member is decorated the way any other item is — attributes, then
    /// directives — because the block is sugar for a run of bindings and a
    /// desugaring that dropped half the decoration would make the sugar mean
    /// something the long form does not. `@link_name` was already read here;
    /// `#c_vararg` is the directive that made the other half matter.
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
            let directives = self.parse_directives();
            let member = self.parse_const_bind(false);
            self.set_extern_abi(member, &abi);
            out.push(self.finish_decl(attrs, directives, member, start));
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
