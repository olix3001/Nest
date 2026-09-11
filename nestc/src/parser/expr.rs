//! Expression parsing: the precedence ladder (§13.7), unary and postfix
//! operators, primaries, composite literals, ranges, `if` / `if match`, blocks,
//! and call/argument lists. All `impl Parser`.
//!
//! The associating binary-operator bands are all the same shape — parse a
//! higher-precedence operand, then fold `{ op operand }` left to right — so they
//! are stamped out by the [`binary_level!`] macro rather than hand-copied. Only
//! the non-associating comparison band is written out, because it must *reject*
//! a second comparison rather than fold it.

use crate::common::span::Span;

use super::ast::{
    BinOp, CompositeBody, Lit, NodeId, NodeKind, RangeKind, TryKind, UnOp, VariantArgs,
};
use super::lexer::TokenKind;
use super::parse::Parser;

/// Generate one left-associative binary-precedence method: parse a `$next`
/// operand, then while the lookahead matches a listed token, consume it and fold
/// another `$next` operand into a [`NodeKind::Binary`].
macro_rules! binary_level {
    ($(#[$doc:meta])* $name:ident = $next:ident { $($tok:pat => $op:expr),+ $(,)? }) => {
        $(#[$doc])*
        fn $name(&mut self) -> NodeId {
            let mut lhs = self.$next();
            loop {
                let op = match self.peek() {
                    $( Some($tok) => $op, )+
                    _ => break,
                };
                self.bump();
                self.skip_newlines();
                let rhs = self.$next();
                let span = self.node_span(lhs).to(self.node_span(rhs));
                lhs = self.alloc(span, NodeKind::Binary { op, lhs, rhs });
            }
            lhs
        }
    };
}

impl Parser {
    /// Parse a full expression, including a trailing or leading range
    /// (`a..<b`, `..=b`, `lo..`, `..`).
    pub(crate) fn parse_expr(&mut self) -> NodeId {
        // Leading unbounded range: `..< b`, `..= b`, `..`.
        if matches!(
            self.peek(),
            Some(TokenKind::DotDot | TokenKind::DotDotLt | TokenKind::DotDotEq)
        ) {
            return self.parse_range_tail(None);
        }
        let lhs = self.parse_or();
        if matches!(
            self.peek(),
            Some(TokenKind::DotDot | TokenKind::DotDotLt | TokenKind::DotDotEq)
        ) {
            return self.parse_range_tail(Some(lhs));
        }
        lhs
    }

    /// Finish a range once the (optional) start has been parsed and a range
    /// token is next.
    fn parse_range_tail(&mut self, start: Option<NodeId>) -> NodeId {
        let start_span = start.map_or_else(|| self.cur_span(), |s| self.node_span(s));
        let kind = match self.bump().kind {
            TokenKind::DotDotLt => RangeKind::HalfOpen,
            TokenKind::DotDotEq => RangeKind::Closed,
            _ => RangeKind::Open, // `..`
        };
        // `..` is always unbounded above; `..<`/`..=` take an end when one is present.
        let end = if kind != RangeKind::Open && self.can_start_expr() {
            Some(self.parse_or())
        } else {
            None
        };
        let end_span = end.map_or(start_span, |e| self.node_span(e));
        self.alloc(
            start_span.to(end_span),
            NodeKind::Range { start, end, kind },
        )
    }

    binary_level! {
        /// `or_expr = and_expr { ('||' | 'or') and_expr }`
        parse_or = parse_and { TokenKind::PipePipe | TokenKind::OrKw => BinOp::Or }
    }
    binary_level! {
        /// `and_expr = cmp_expr { ('&&' | 'and') cmp_expr }`
        parse_and = parse_cmp { TokenKind::AmpAmp | TokenKind::AndKw => BinOp::And }
    }

    /// `cmp_expr = bitor_expr [ cmp_op bitor_expr ]` — **non-associating**: a
    /// second comparison (`a < b < c`) is a diagnosed error, not a fold.
    fn parse_cmp(&mut self) -> NodeId {
        let lhs = self.parse_bitor();
        let op = match self.peek() {
            Some(TokenKind::EqEq) => BinOp::Eq,
            Some(TokenKind::BangEq) => BinOp::Ne,
            Some(TokenKind::Lt) => BinOp::Lt,
            Some(TokenKind::LtEq) => BinOp::Le,
            Some(TokenKind::Gt) => BinOp::Gt,
            Some(TokenKind::GtEq) => BinOp::Ge,
            _ => return lhs,
        };
        self.bump();
        self.skip_newlines();
        let rhs = self.parse_bitor();
        let span = self.node_span(lhs).to(self.node_span(rhs));
        let node = self.alloc(span, NodeKind::Binary { op, lhs, rhs });
        if matches!(
            self.peek(),
            Some(
                TokenKind::EqEq
                    | TokenKind::BangEq
                    | TokenKind::Lt
                    | TokenKind::LtEq
                    | TokenKind::Gt
                    | TokenKind::GtEq
            )
        ) {
            let span = self.cur_span();
            self.error(span, "comparison operators do not chain; use `&&`");
        }
        node
    }

    binary_level! {
        /// `bitor_expr = bitxor_expr { '|' bitxor_expr }`
        parse_bitor = parse_bitxor { TokenKind::Pipe => BinOp::BitOr }
    }
    binary_level! {
        /// `bitxor_expr = bitand_expr { '^' bitand_expr }`
        parse_bitxor = parse_bitand { TokenKind::Caret => BinOp::BitXor }
    }
    binary_level! {
        /// `bitand_expr = shift_expr { '&' shift_expr }`
        parse_bitand = parse_shift { TokenKind::Amp => BinOp::BitAnd }
    }
    binary_level! {
        /// `shift_expr = add_expr { ('<<' | '>>') add_expr }`
        parse_shift = parse_add { TokenKind::Shl => BinOp::Shl, TokenKind::Shr => BinOp::Shr }
    }
    binary_level! {
        /// `add_expr = mul_expr { ('+' | '-') mul_expr }`
        parse_add = parse_mul { TokenKind::Plus => BinOp::Add, TokenKind::Minus => BinOp::Sub }
    }
    binary_level! {
        /// `mul_expr = unary_expr { ('*' | '/' | '%') unary_expr }`
        parse_mul = parse_unary {
            TokenKind::Star => BinOp::Mul,
            TokenKind::Slash => BinOp::Div,
            TokenKind::Percent => BinOp::Rem,
        }
    }

    /// `unary_expr = ('&' ['mut'] | '-' | '!' | 'not' | '~') unary_expr | postfix_expr`.
    /// Deref is postfix (`.*`), so there is no prefix `*` here.
    fn parse_unary(&mut self) -> NodeId {
        let start = self.cur_span();
        let op = match self.peek() {
            Some(TokenKind::Amp) => {
                self.bump();
                if self.eat(&TokenKind::MutKw) {
                    UnOp::RefMut
                } else {
                    UnOp::Ref
                }
            }
            Some(TokenKind::Minus) => {
                self.bump();
                UnOp::Neg
            }
            Some(TokenKind::Bang | TokenKind::NotKw) => {
                self.bump();
                UnOp::Not
            }
            Some(TokenKind::Tilde) => {
                self.bump();
                UnOp::BitNot
            }
            _ => return self.parse_postfix(),
        };
        let operand = self.parse_unary();
        let span = start.to(self.node_span(operand));
        self.alloc(span, NodeKind::Unary { op, operand })
    }

    /// `postfix_expr = primary { postfix_op }`.
    fn parse_postfix(&mut self) -> NodeId {
        let mut e = self.parse_primary();
        loop {
            e = match self.peek() {
                Some(TokenKind::Dot) => self.parse_dot_postfix(e),
                Some(TokenKind::DotLt) => {
                    let (args, end) = self.parse_generic_args();
                    let span = self.node_span(e).to(end);
                    self.alloc(span, NodeKind::GenericApply { base: e, args })
                }
                Some(TokenKind::LParen) => {
                    let (args, end) = self.parse_call_args();
                    let span = self.node_span(e).to(end);
                    self.alloc(span, NodeKind::Call { callee: e, args })
                }
                Some(TokenKind::LBracket) => self.parse_index_or_slice(e),
                Some(TokenKind::DotStar) => {
                    let span = self.node_span(e).to(self.cur_span());
                    self.bump();
                    self.alloc(span, NodeKind::Deref { base: e })
                }
                Some(TokenKind::DotQuestion) => {
                    let span = self.node_span(e).to(self.cur_span());
                    self.bump();
                    self.alloc(
                        span,
                        NodeKind::Try {
                            base: e,
                            kind: TryKind::Propagate,
                        },
                    )
                }
                Some(TokenKind::DotBang) => {
                    let span = self.node_span(e).to(self.cur_span());
                    self.bump();
                    self.alloc(
                        span,
                        NodeKind::Try {
                            base: e,
                            kind: TryKind::Abort,
                        },
                    )
                }
                // A `{` after a callee: a trailing-closure argument. After a path/
                // generic-apply: a struct literal. Both are suppressed in a
                // control-flow head (see `no_struct_lit`).
                Some(TokenKind::LBrace) if !self.struct_lit_suppressed() => {
                    if self.node_is_call(e) {
                        self.attach_trailing_closure(e)
                    } else if self.node_is_pathlike(e) {
                        self.parse_typed_composite(e)
                    } else {
                        break;
                    }
                }
                _ => break,
            };
        }
        e
    }

    /// A postfix beginning with `.`: field access, tuple index, or `.match`.
    fn parse_dot_postfix(&mut self, base: NodeId) -> NodeId {
        let base_span = self.node_span(base);
        self.bump(); // '.'
        match self.peek() {
            Some(TokenKind::Ident(_)) => {
                let end = self.cur_span();
                let name = self.expect_ident();
                self.alloc(base_span.to(end), NodeKind::FieldAccess { base, name })
            }
            Some(TokenKind::Int(i)) => {
                let index = u64::try_from(i).unwrap_or(0);
                let end = self.cur_span();
                self.bump();
                self.alloc(base_span.to(end), NodeKind::TupleIndex { base, index })
            }
            Some(TokenKind::MatchKw) => {
                self.bump();
                let (arms, end) = self.parse_match_block();
                self.alloc(
                    base_span.to(end),
                    NodeKind::MatchExpr {
                        scrutinee: base,
                        arms,
                    },
                )
            }
            _ => {
                let span = self.cur_span();
                self.error_node(
                    span,
                    "expected a field name, tuple index, or `match` after `.`",
                )
            }
        }
    }

    /// `[ expr ]` index or `[ range ]` slice.
    fn parse_index_or_slice(&mut self, base: NodeId) -> NodeId {
        let base_span = self.node_span(base);
        self.bump(); // '['
        let inner = self.allowing_struct_lit(Parser::parse_expr);
        let end = self.cur_span();
        self.expect(&TokenKind::RBracket);
        let span = base_span.to(end);
        if self.with_kind(inner, |k| matches!(k, NodeKind::Range { .. })) {
            self.alloc(span, NodeKind::Slice { base, range: inner })
        } else {
            self.alloc(span, NodeKind::Index { base, index: inner })
        }
    }

    // ===< Primaries >===

    /// `primary` (§13.7): literals, names, `$`-intrinsics, `self`/`Self`,
    /// grouping/tuple, composite literals, closures, `if`, blocks, loops, and
    /// `import`.
    fn parse_primary(&mut self) -> NodeId {
        let span = self.cur_span();
        match self.peek() {
            Some(TokenKind::Int(n)) => {
                let n = n.clone();
                self.bump();
                self.alloc(span, NodeKind::Lit(Lit::Int(n)))
            }
            Some(TokenKind::Float(x)) => {
                let x = *x;
                self.bump();
                let node = self.alloc(span, NodeKind::Lit(Lit::Float(x.value)));
                // Remember that this literal outruns `f64`, so inference can
                // reject the default collapse at the use site.
                if x.wide {
                    self.set_meta(node, crate::parser::ast::WideFloat);
                }
                node
            }
            Some(TokenKind::Str(s)) => {
                let s = s.clone();
                self.bump();
                self.alloc(span, NodeKind::Lit(Lit::Str(s)))
            }
            Some(TokenKind::Bytes(b)) => {
                let b = b.clone();
                self.bump();
                self.alloc(span, NodeKind::Lit(Lit::Bytes(b)))
            }
            Some(TokenKind::Char(c)) => {
                let c = *c;
                self.bump();
                self.alloc(span, NodeKind::Lit(Lit::Char(c)))
            }
            Some(TokenKind::TrueKw) => {
                self.bump();
                self.alloc(span, NodeKind::Lit(Lit::Bool(true)))
            }
            Some(TokenKind::FalseKw) => {
                self.bump();
                self.alloc(span, NodeKind::Lit(Lit::Bool(false)))
            }
            // `#caller_location` in expression position. It is a directive
            // spelling rather than a name so that it cannot be shadowed,
            // re-exported, or passed around: the only place it means anything is
            // a default argument, and inference enforces that.
            Some(TokenKind::Hash) => self.parse_location_directive(),
            Some(TokenKind::Ident(_)) => {
                let name = self.expect_ident();
                self.alloc(
                    span,
                    NodeKind::Path {
                        segments: vec![name],
                    },
                )
            }
            // A leading `[` heads an explicit array/slice-typed composite literal
            // (`[_]int32 { 1, 2, 3 }`, `[]T { ... }`). Indexing/slicing is postfix
            // and never starts an expression.
            Some(TokenKind::LBracket) => {
                let ty = self.parse_type();
                if self.at(&TokenKind::LBrace) && !self.struct_lit_suppressed() {
                    self.parse_typed_composite(ty)
                } else {
                    ty
                }
            }
            Some(TokenKind::LParen) => self.parse_paren_or_tuple(),
            Some(TokenKind::DotLBrace) => self.parse_inferred_composite(),
            Some(TokenKind::Dot) if matches!(self.peek_nth(1), Some(TokenKind::Ident(_))) => {
                self.parse_variant_literal()
            }
            Some(TokenKind::FuncKw | TokenKind::ExternKw) => self.parse_func_expr(Vec::new()),
            Some(TokenKind::IfKw) => self.parse_if(),
            Some(TokenKind::MatchKw) => self.parse_match(),
            Some(TokenKind::LBrace) => self.parse_block(),
            Some(TokenKind::LoopKw) => self.parse_loop(),
            Some(TokenKind::WhileKw) => self.parse_while(),
            Some(TokenKind::ForKw) => self.parse_for(),
            Some(TokenKind::ImportKw) => self.parse_import(),
            _ => {
                let msg = "expected an expression";
                if !self.at_eof() {
                    self.bump(); // consume to guarantee progress
                }
                self.error_node(span, msg)
            }
        }
    }

    /// `#caller_location` — the one directive that is an *expression* (§5.2).
    fn parse_location_directive(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // '#'
        let name = self.expect_ident();
        let span = start.to(self.cur_span());
        if name.as_str() != "caller_location" {
            let msg = format!("`#{name}` is not an expression");
            self.error(span, msg);
            return self.error_node(span, "expected an expression");
        }
        self.alloc(span, NodeKind::CallerLocation)
    }

    /// `( )` unit, `( expr )` grouping, or `( expr { ',' expr } )` tuple.
    fn parse_paren_or_tuple(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // '('
        self.allowing_struct_lit(|p| {
            p.skip_newlines();
            if p.at(&TokenKind::RParen) {
                let end = p.cur_span();
                p.bump();
                return p.alloc(start.to(end), NodeKind::Tuple { elems: Vec::new() });
            }
            let first = p.parse_expr();
            p.skip_newlines();
            if !p.at(&TokenKind::Comma) {
                p.expect(&TokenKind::RParen);
                return first; // grouping
            }
            let mut elems = vec![first];
            while p.eat(&TokenKind::Comma) {
                p.skip_newlines();
                if p.at(&TokenKind::RParen) {
                    break;
                }
                elems.push(p.parse_expr());
                p.skip_newlines();
            }
            let end = p.cur_span();
            p.expect(&TokenKind::RParen);
            p.alloc(start.to(end), NodeKind::Tuple { elems })
        })
    }

    /// `.{ composite_body }` — an inferred record / array / tuple literal.
    fn parse_inferred_composite(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // '.{'
        let (body, end) = self.allowing_struct_lit(Parser::parse_composite_body);
        self.alloc(start.to(end), NodeKind::CompositeLit { ty: None, body })
    }

    /// `Type { composite_body }` — a typed record / array literal, given the
    /// already-parsed type/path expression as `ty`.
    fn parse_typed_composite(&mut self, ty: NodeId) -> NodeId {
        let start = self.node_span(ty);
        self.bump(); // '{'  (the caller confirmed LBrace)
        // Re-enter body parsing having consumed '{'; parse_composite_body expects
        // to open its own brace, so hand it back by parsing entries directly.
        let (body, end) = self.allowing_struct_lit(Parser::parse_composite_body_inner);
        self.alloc(start.to(end), NodeKind::CompositeLit { ty: Some(ty), body })
    }

    /// Parse a composite body *including* its opening `.{`-style brace already
    /// consumed by the caller? No — this opens the brace itself. Used by the
    /// inferred `.{ ... }` form.
    fn parse_composite_body(&mut self) -> (CompositeBody, Span) {
        // `.{` already consumed the brace; `.{` is one token, so no LBrace here.
        self.parse_composite_body_inner()
    }

    /// The shared body of a composite literal, with the opening brace already
    /// consumed: named `field: value` entries, positional entries, or a
    /// `value ; count` array repeat.
    fn parse_composite_body_inner(&mut self) -> (CompositeBody, Span) {
        self.skip_newlines();
        if self.at(&TokenKind::RBrace) {
            let end = self.cur_span();
            self.bump();
            return (CompositeBody::Positional(Vec::new()), end);
        }
        // Named body: `ident :` (but not `ident ::`, which would be a binding),
        // or a body that is nothing but a spread (`P { ..d }`).
        if (matches!(self.peek(), Some(TokenKind::Ident(_)))
            && matches!(self.peek_nth(1), Some(TokenKind::Colon)))
            || self.at(&TokenKind::DotDot)
        {
            let mut fields = Vec::new();
            let mut spread = None;
            loop {
                self.skip_newlines();
                if self.at(&TokenKind::RBrace) || self.at_eof() {
                    break;
                }
                // `..expr` supplies every field not written (§3.3). It is last
                // by construction: what follows it would have nothing to mean.
                if self.eat(&TokenKind::DotDot) {
                    spread = Some(self.parse_expr());
                    self.skip_newlines();
                    self.eat(&TokenKind::Comma);
                    break;
                }
                fields.push(self.parse_field_init());
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.skip_newlines();
            let end = self.cur_span();
            self.expect(&TokenKind::RBrace);
            return (CompositeBody::Named { fields, spread }, end);
        }
        // Positional or repeat.
        let first = self.parse_expr();
        if self.eat(&TokenKind::Semicolon) {
            let count = self.parse_expr();
            self.skip_newlines();
            let end = self.cur_span();
            self.expect(&TokenKind::RBrace);
            return (
                CompositeBody::Repeat {
                    value: first,
                    count,
                },
                end,
            );
        }
        let mut elems = vec![first];
        self.skip_newlines();
        while self.eat(&TokenKind::Comma) {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            elems.push(self.parse_expr());
            self.skip_newlines();
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        (CompositeBody::Positional(elems), end)
    }

    /// `ident ':' expr` — one named composite entry.
    fn parse_field_init(&mut self) -> NodeId {
        let start = self.cur_span();
        let name = self.expect_ident();
        self.expect(&TokenKind::Colon);
        let value = self.parse_expr();
        self.alloc(
            start.to(self.node_span(value)),
            NodeKind::FieldInit { name, value },
        )
    }

    /// `.name [ '(' args ')' | '{' field_inits '}' ]` — an inferred enum variant.
    fn parse_variant_literal(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // '.'
        let name = self.expect_ident();
        let mut end = start;
        let args = match self.peek() {
            Some(TokenKind::LParen) => {
                let (args, span) = self.parse_call_args();
                end = span;
                VariantArgs::Tuple(args)
            }
            Some(TokenKind::LBrace) if !self.struct_lit_suppressed() => {
                self.bump();
                let mut fields = Vec::new();
                loop {
                    self.skip_newlines();
                    if self.at(&TokenKind::RBrace) || self.at_eof() {
                        break;
                    }
                    fields.push(self.parse_field_init());
                    self.skip_newlines();
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.skip_newlines();
                end = self.cur_span();
                self.expect(&TokenKind::RBrace);
                VariantArgs::Record(fields)
            }
            _ => VariantArgs::None,
        };
        self.alloc(start.to(end), NodeKind::VariantLit { name, args })
    }

    // ===< Calls and arguments >===

    /// `( [ arg { ',' arg } ] )` — a parenthesised argument list. Returns the
    /// [`NodeKind::Arg`] nodes and the closing-paren span.
    pub(crate) fn parse_call_args(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::LParen);
        let mut args = Vec::new();
        self.allowing_struct_lit(|p| {
            p.skip_newlines();
            while !p.at(&TokenKind::RParen) && !p.at_eof() {
                args.push(p.parse_arg());
                p.skip_newlines();
                if !p.eat(&TokenKind::Comma) {
                    break;
                }
                p.skip_newlines();
            }
        });
        let end = self.cur_span();
        self.expect(&TokenKind::RParen);
        (args, end)
    }

    /// `[ ident ':' ] expr` — one call argument (positional or named).
    fn parse_arg(&mut self) -> NodeId {
        let start = self.cur_span();
        let name = if matches!(self.peek(), Some(TokenKind::Ident(_)))
            && matches!(self.peek_nth(1), Some(TokenKind::Colon))
        {
            let name = self.expect_ident();
            self.bump(); // ':'
            Some(name)
        } else {
            None
        };
        let value = self.parse_expr();
        self.alloc(
            start.to(self.node_span(value)),
            NodeKind::Arg { name, value },
        )
    }

    /// Parse a trailing `{ [closure_header] block }` after a call and append it
    /// as a final closure argument, returning the extended call node.
    fn attach_trailing_closure(&mut self, call: NodeId) -> NodeId {
        let closure = self.parse_trailing_closure();
        let closure_span = self.node_span(closure);
        // Wrap the closure as a positional Arg and push onto the call.
        let arg = self.alloc(
            closure_span,
            NodeKind::Arg {
                name: None,
                value: closure,
            },
        );
        let mut node = self.clone_kind(call);
        if let NodeKind::Call { args, .. } = &mut node {
            args.push(arg);
        }
        let span = self.node_span(call).to(closure_span);
        self.set_node(call, span, node);
        call
    }

    /// `{ [ param { ',' param } '=>' ] statements [ tail ] }` — a trailing
    /// closure block, represented as a bodyless-less [`NodeKind::FuncExpr`].
    fn parse_trailing_closure(&mut self) -> NodeId {
        let start = self.cur_span();
        // Detect a header by scanning for a top-level `=>` before the block ends.
        let params = if self.closure_header_ahead() {
            let mut params = Vec::new();
            self.bump(); // '{'
            loop {
                self.skip_newlines();
                params.push(self.parse_closure_param());
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::FatArrow);
            Some(params)
        } else {
            None
        };
        let body = if params.is_some() {
            // Brace already consumed; parse the remaining statements as a block.
            self.parse_block_rest(start)
        } else {
            self.parse_block()
        };
        let span = start.to(self.node_span(body));
        self.alloc(
            span,
            NodeKind::FuncExpr {
                directives: Vec::new(),
                extern_abi: None,
                generics: Vec::new(),
                params: params.unwrap_or_default(),
                ret: None,
                body: Some(body),
            },
        )
    }

    /// `ident [ ':' type ]` — a closure-header parameter (type optional).
    fn parse_closure_param(&mut self) -> NodeId {
        let start = self.cur_span();
        let name = self.expect_ident();
        let mut span = start;
        let ty = if self.eat(&TokenKind::Colon) {
            let ty = self.parse_type();
            span = span.to(self.node_span(ty));
            Some(ty)
        } else {
            None
        };
        // A closure parameter takes no default: a closure is written at the point
        // it is passed, so an omitted argument has no call site to fill it in from.
        self.alloc(
            span,
            NodeKind::Param {
                name,
                ty,
                default: None,
            },
        )
    }

    /// Whether the `{` at the cursor opens a closure header — i.e. a top-level
    /// `=>` appears before the block's first statement terminator or its close.
    fn closure_header_ahead(&self) -> bool {
        let mut depth = 0i32;
        let mut i = 0;
        while let Some(kind) = self.peek_nth(i) {
            match kind {
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    depth -= 1;
                    if depth <= 0 {
                        return false;
                    }
                }
                TokenKind::FatArrow if depth == 1 => return true,
                TokenKind::Semicolon | TokenKind::Newline if depth == 1 => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    // ===< if / if match >===

    /// `if expr block [ else (if | block) ]` or `if match pattern := expr block
    /// [ else block ]`.
    fn parse_if(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump(); // 'if'
        if self.eat(&TokenKind::MatchKw) {
            let pattern = self.parse_pattern();
            self.expect(&TokenKind::ColonEq);
            let value = self.suppressing_struct_lit(Parser::parse_expr);
            let then = self.parse_block();
            let mut end = self.node_span(then);
            let els = if self.eat(&TokenKind::ElseKw) {
                let b = self.parse_block();
                end = self.node_span(b);
                Some(b)
            } else {
                None
            };
            return self.alloc(
                start.to(end),
                NodeKind::IfMatch {
                    pattern,
                    value,
                    then,
                    els,
                },
            );
        }
        let cond = self.suppressing_struct_lit(Parser::parse_expr);
        let then = self.parse_block();
        let mut end = self.node_span(then);
        let els = if self.eat(&TokenKind::ElseKw) {
            let e = if self.at(&TokenKind::IfKw) {
                self.parse_if()
            } else {
                self.parse_block()
            };
            end = self.node_span(e);
            Some(e)
        } else {
            None
        };
        self.alloc(start.to(end), NodeKind::If { cond, then, els })
    }

    /// `{ arm { ',' arm } }` — a match block; returns arms and closing span.
    /// `match scrutinee { arms }` — the prefix form.
    ///
    /// The postfix `scrutinee.match { arms }` is sugar for this: both build the
    /// same [`NodeKind::MatchExpr`], so nothing after the parser knows which one
    /// was written. Prefix reads better when the scrutinee is short (`match c {`)
    /// and postfix when it is the tail of a chain (`xs.first().match {`).
    ///
    /// The scrutinee is parsed with struct literals suppressed, exactly as an
    /// `if` condition is: otherwise `match p { ... }` would read `p { ... }` as a
    /// struct literal and then find no match block.
    fn parse_match(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let scrutinee = self.suppressing_struct_lit(|p| p.parse_expr());
        let (arms, end) = self.parse_match_block();
        self.alloc(start.to(end), NodeKind::MatchExpr { scrutinee, arms })
    }

    fn parse_match_block(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::LBrace);
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            arms.push(self.parse_match_arm());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.skip_newlines();
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        (arms, end)
    }

    /// `pattern [ 'if' guard ] '=>' (expr | block)` — one match arm.
    fn parse_match_arm(&mut self) -> NodeId {
        let start = self.cur_span();
        let pattern = self.parse_pattern();
        let guard = if self.eat(&TokenKind::IfKw) {
            Some(self.suppressing_struct_lit(Parser::parse_expr))
        } else {
            None
        };
        self.expect(&TokenKind::FatArrow);
        let body = if self.at(&TokenKind::LBrace) {
            self.parse_block()
        } else {
            self.parse_expr()
        };
        self.alloc(
            start.to(self.node_span(body)),
            NodeKind::MatchArm {
                pattern,
                guard,
                body,
            },
        )
    }

    // ===< Blocks and loops >===

    /// `{ statement* [ tail_expr ] }`.
    pub(crate) fn parse_block(&mut self) -> NodeId {
        let start = self.cur_span();
        self.expect(&TokenKind::LBrace);
        self.parse_block_rest(start)
    }

    /// The body of a block after its opening `{` has been consumed.
    fn parse_block_rest(&mut self, start: Span) -> NodeId {
        let mut stmts = Vec::new();
        let mut tail = None;
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            let (node, is_expr) = self.allowing_struct_lit(Parser::parse_stmt);
            let had_semi = self.eat(&TokenKind::Semicolon);
            self.skip_newlines();
            if is_expr && !had_semi && self.at(&TokenKind::RBrace) {
                tail = Some(node);
                break;
            }
            stmts.push(node);
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        self.alloc(start.to(end), NodeKind::Block { stmts, tail })
    }

    /// `loop block`.
    fn parse_loop(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let body = self.parse_block();
        self.alloc(start.to(self.node_span(body)), NodeKind::Loop { body })
    }

    /// `while cond block`.
    fn parse_while(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let cond = self.suppressing_struct_lit(Parser::parse_expr);
        let body = self.parse_block();
        self.alloc(
            start.to(self.node_span(body)),
            NodeKind::While { cond, body },
        )
    }

    /// `for pattern in iterable block` (`in` is a contextual keyword).
    fn parse_for(&mut self) -> NodeId {
        let start = self.cur_span();
        self.bump();
        let pattern = self.parse_pattern();
        if !self.eat_contextual("in") {
            let span = self.cur_span();
            self.error(span, "expected `in` in a `for` loop");
        }
        let iter = self.suppressing_struct_lit(Parser::parse_expr);
        let body = self.parse_block();
        self.alloc(
            start.to(self.node_span(body)),
            NodeKind::For {
                pattern,
                iter,
                body,
            },
        )
    }

    // ===< Small node helpers >===

    /// Whether `id` is a [`NodeKind::Path`] or a generic application of one — the
    /// shapes that can head a struct literal.
    /// Whether `id` could name a **type**, and so could be the head of a
    /// `Type { ... }` composite literal.
    ///
    /// A dotted path through a namespace — `shapes.Circle` — parses as a
    /// [`NodeKind::FieldAccess`], not a `Path`: whether a segment is a namespace
    /// hop or a field access is resolution's answer, not the parser's. So a
    /// field-access chain rooted in a path counts too. Without it, an imported
    /// type could not be constructed by name at all: the `{` was left dangling
    /// and reported as "expected an expression".
    fn node_is_pathlike(&self, id: NodeId) -> bool {
        self.with_kind(id, |k| match k {
            NodeKind::Path { .. } | NodeKind::GenericApply { .. } => true,
            NodeKind::FieldAccess { base, .. } => self.node_is_pathlike(*base),
            _ => false,
        })
    }

    /// Whether `id` is a [`NodeKind::Call`].
    fn node_is_call(&self, id: NodeId) -> bool {
        self.with_kind(id, |k| matches!(k, NodeKind::Call { .. }))
    }

    /// Whether the token at the cursor can begin an expression (used to decide
    /// whether a range has an upper bound).
    fn can_start_expr(&self) -> bool {
        !matches!(
            self.peek(),
            None | Some(
                TokenKind::RParen
                    | TokenKind::RBracket
                    | TokenKind::RBrace
                    | TokenKind::Comma
                    | TokenKind::Semicolon
                    | TokenKind::Newline
                    | TokenKind::LBrace
            )
        )
    }
}
