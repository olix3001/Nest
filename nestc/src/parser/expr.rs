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
use crate::common::symbol::Symbol;

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
            // No expression begins with `*` — a dereference is the postfix
            // `.*` — so one here is a pointer *type* written where a value
            // goes: `Item :: (K, *mut V)` in an impl, whose right side is
            // parsed as an expression until it is known to be a type.
            Some(TokenKind::Star) => return self.parse_type(),
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
                    } else if self.node_is_pathlike(e) && self.braced_closure_ahead() {
                        // `app.use { ctx, next in … }`: a call whose only
                        // argument is the trailing closure, with no `()`.
                        let span = self.node_span(e);
                        let call = self.alloc(span, NodeKind::Call { callee: e, args: Vec::new() });
                        self.attach_trailing_closure(call)
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

    /// `f"...{e}..."` (§1.5, §6.11) — the pieces the lexer already separated.
    ///
    /// The parts are kept **interleaved in source order**, literal segments and
    /// embedded expressions together, because the order is the whole content of
    /// the literal: `f"{a}b"` and `f"b{a}"` differ in nothing else. A segment is
    /// an ordinary `Lit::Str` node, so it types and lowers as the string it is
    /// and desugaring needs no third case.
    fn parse_interpolated_str(&mut self) -> NodeId {
        let open = self.cur_span();
        self.bump();
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                Some(TokenKind::InterpEnd) | None => break,
                Some(TokenKind::Str(s)) => {
                    let s = s.clone();
                    let at = self.cur_span();
                    self.bump();
                    parts.push(self.alloc(at, NodeKind::Lit(Lit::Str(s))));
                }
                Some(TokenKind::InterpOpen) => {
                    self.bump();
                    let expr = self.parse_expr();
                    // `{x:>8}` — the specifier the lexer read is kept beside the
                    // expression rather than inside the node, because it is a
                    // fact about *this hole* and not about the expression, which
                    // types and lowers as the ordinary one it is (§6.11).
                    if let Some(TokenKind::InterpSpec(spec)) = self.peek() {
                        let spec = *spec;
                        self.bump();
                        self.set_meta(expr, spec);
                    }
                    parts.push(expr);
                    // The lexer matched these braces by depth, so a failure here
                    // is the expression parser having stopped early — the error
                    // belongs at the token it stopped on, and the loop carries
                    // on to the closer rather than abandoning the literal.
                    self.expect(&TokenKind::InterpClose);
                }
                _ => {
                    let at = self.cur_span();
                    let found = self.describe_next();
                    self.error(
                        at,
                        format!("expected the rest of the string, found {found}"),
                    );
                    self.bump();
                }
            }
        }
        let end = self.cur_span();
        self.expect(&TokenKind::InterpEnd);
        self.alloc(open.to(end), NodeKind::InterpolatedStr { parts })
    }

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
            // The NUL goes on here, in the one place that knows the literal is
            // a C string: everything downstream sees an ordinary `str` whose
            // last byte happens to be zero.
            Some(TokenKind::CStr(s)) => {
                let mut s = s.clone();
                self.bump();
                s.push('\0');
                let bytes = self.alloc(span, NodeKind::Lit(Lit::Str(s)));
                self.alloc(span, NodeKind::CStr { bytes })
            }
            Some(TokenKind::InterpStart) => self.parse_interpolated_str(),
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
            Some(TokenKind::FuncKw | TokenKind::ExternKw) => self.parse_func_literal(),
            Some(TokenKind::IfKw) => self.parse_if(),
            Some(TokenKind::MatchKw) => self.parse_match(),
            Some(TokenKind::LBrace) if self.closure_header_ahead() => self.parse_closure(),
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

    /// A trailing block: a closure whether or not it has a header, since a
    /// block after a call's `)` can only be its last argument (§5.3).
    fn parse_trailing_closure(&mut self) -> NodeId {
        if self.closure_header_ahead() {
            return self.parse_closure();
        }
        let start = self.cur_span();
        let body = self.parse_block();
        let span = start.to(self.node_span(body));
        self.alloc(
            span,
            NodeKind::Closure {
                captures: Vec::new(),
                params: Vec::new(),
                ret: None,
                body,
            },
        )
    }

    /// `{ [ '[' name { ',' name } ']' ] [ param { ',' param } ] [ '->' type ] 'in'
    /// statements [ tail ] }` — a closure (§5.5), with the cursor on its `{`.
    ///
    /// [`Parser::closure_header_ahead`] has already said this is one, so the
    /// header is parsed without backtracking.
    fn parse_closure(&mut self) -> NodeId {
        let start = self.cur_span();
        self.expect(&TokenKind::LBrace);
        self.skip_newlines();
        let mut captures = Vec::new();
        if self.eat(&TokenKind::LBracket) {
            loop {
                self.skip_newlines();
                if self.at(&TokenKind::RBracket) {
                    break;
                }
                let span = self.cur_span();
                let name = self.expect_ident();
                captures.push(self.alloc(span, NodeKind::Capture { name }));
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RBracket);
        }
        let mut params = Vec::new();
        while !self.at_contextual("in") && !self.at(&TokenKind::Arrow) && !self.at_eof() {
            self.skip_newlines();
            params.push(self.parse_closure_param());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type())
        } else {
            None
        };
        if !self.eat_contextual("in") {
            let span = self.cur_span();
            self.error(span, "expected `in` to end the closure's parameters");
        }
        let body = self.parse_block_rest(start);
        let span = start.to(self.node_span(body));
        self.alloc(
            span,
            NodeKind::Closure {
                captures,
                params,
                ret,
                body,
            },
        )
    }

    /// A `func (params) -> ret { body }` literal written where a value goes: a
    /// closure spelled with its types, which is what it is (§5.5). Only a `::`
    /// binding makes a `func` literal a definition.
    fn parse_func_literal(&mut self) -> NodeId {
        let func = self.parse_func_expr(Vec::new());
        let NodeKind::FuncExpr {
            extern_abi,
            generics,
            params,
            ret,
            body,
            ..
        } = self.clone_kind(func)
        else {
            // An overload set: a set is not a value, which sema says.
            return func;
        };
        let span = self.node_span(func);
        if extern_abi.is_some() {
            self.error(
                span,
                "an `extern` function is a definition: bind it with `::` to give it a name C can call",
            );
        }
        if !generics.is_empty() {
            self.error(
                span,
                "a closure cannot be generic; bind a generic function with `::`",
            );
        }
        let Some(body) = body else {
            return self.error_node(span, "a function literal used as a value needs a body");
        };
        self.alloc(
            span,
            NodeKind::Closure {
                captures: Vec::new(),
                params,
                ret,
                body,
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

    /// Whether the `{` at the cursor, written after a path, is a closure rather
    /// than a struct literal: it opens a closure header **and** an `in` stands
    /// at the brace's own level before its `}`.
    ///
    /// The header alone does not decide it here — `P { a, b }` and `P { x: 1 }`
    /// start the way `{ a, b in … }` and `{ x: i32 in … }` do. The `in` does: a
    /// struct literal's own level holds fields and values, and an `in` can only
    /// stand inside something nested in one (a block, a closure), never beside
    /// its fields.
    fn braced_closure_ahead(&self) -> bool {
        if !self.closure_header_ahead() {
            return false;
        }
        let mut depth = 0usize;
        let mut i = 0;
        while let Some(t) = self.peek_nth(i) {
            match t {
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return false;
                    }
                }
                TokenKind::Ident(s) if depth == 1 && s.as_str() == "in" => return true,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Whether the `{` at the cursor opens a closure **header**, which is what
    /// tells a closure from a block (§5.5).
    ///
    /// Two tokens decide it, after an optional `[captures]`: `in` on its own, or
    /// a name followed by `,`, `:`, `->` or `in`. No statement starts that way —
    /// a local is `let x` or `const x`, and a name is never followed by any of
    /// the four — so there is no reading of a block this takes away. The
    /// capture list is skipped only when it is a list of names; `[N]u8 { … }` is
    /// an array literal, and the `u8 {` after it is not a header.
    fn closure_header_ahead(&self) -> bool {
        if !matches!(self.peek(), Some(TokenKind::LBrace)) {
            return false;
        }
        let mut i = 1;
        let skip_newlines = |i: &mut usize| {
            while matches!(self.peek_nth(*i), Some(TokenKind::Newline)) {
                *i += 1;
            }
        };
        skip_newlines(&mut i);
        let is_in =
            |t: Option<&TokenKind>| matches!(t, Some(TokenKind::Ident(s)) if s.as_str() == "in");
        if matches!(self.peek_nth(i), Some(TokenKind::LBracket)) {
            i += 1;
            loop {
                skip_newlines(&mut i);
                match self.peek_nth(i) {
                    Some(TokenKind::RBracket) => {
                        i += 1;
                        break;
                    }
                    Some(TokenKind::Ident(_)) => {
                        i += 1;
                        skip_newlines(&mut i);
                        match self.peek_nth(i) {
                            Some(TokenKind::Comma) => i += 1,
                            Some(TokenKind::RBracket) => {}
                            _ => return false,
                        }
                    }
                    _ => return false,
                }
            }
            skip_newlines(&mut i);
        }
        if is_in(self.peek_nth(i)) {
            return true;
        }
        if matches!(self.peek_nth(i), Some(TokenKind::Arrow)) {
            return true;
        }
        if !matches!(self.peek_nth(i), Some(TokenKind::Ident(_))) {
            return false;
        }
        let next = self.peek_nth(i + 1);
        is_in(next)
            || matches!(
                next,
                Some(TokenKind::Comma | TokenKind::Colon | TokenKind::Arrow)
            )
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
            let before = self.error_count();
            let (node, is_expr) = self.allowing_struct_lit(Parser::parse_stmt);
            // Two statements on one line need a `;` between them (spec §1.1). A
            // newline is still a separator, so only a statement that ran into
            // the next one on the same line is refused — and only when the
            // statement itself parsed, so a recovery does not report twice.
            if !self.at_stmt_end() && self.error_count() == before {
                let span = self.cur_span();
                self.error(
                    span,
                    format!(
                        "expected `;` or a newline after a statement, found {}",
                        self.describe_next()
                    ),
                );
            }
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

    /// `#comptime for pattern in a..<b block` — the loop, unrolled **here**.
    ///
    /// It is a parser rewrite rather than a later one, and the reason is what
    /// the directive is *for*: an unrolled body has to be typed once per
    /// iteration, and every stage after this one has already resolved names and
    /// stamped defs onto the body's declarations. Copying a resolved body is
    /// copying its bindings; re-**parsing** it is what makes each copy an
    /// independent piece of program, which is the whole point of unrolling.
    ///
    /// The parser has the tokens and an index into them, so a copy is a rewind:
    /// the body is parsed once per value, each inside a block that binds the
    /// loop variable to that value as a `::` constant.
    ///
    /// The sequence is a range of **integer literals**. A bound this stage
    /// cannot read is an error at the loop — which is the point of the step: a
    /// loop that does not unroll should say so where it is written, rather than
    /// at whatever the body did with a variable that never settled.
    pub(crate) fn parse_comptime_for(&mut self, start: Span) -> NodeId {
        self.bump(); // `for`
        let pat_start = self.pos;
        let pattern = self.parse_pattern();
        if !self.eat_contextual("in") {
            let span = self.cur_span();
            self.error(span, "expected `in` in a `for` loop");
        }
        let iter = self.suppressing_struct_lit(Parser::parse_expr);
        let body_start = self.pos;
        let Some(values) = self.const_range(iter) else {
            self.error(
                self.node_span(iter),
                "a `#comptime for` iterates a range of integer literals, as in `0..<4`",
            );
            let body = self.parse_block();
            return self.alloc(
                start.to(self.node_span(body)),
                NodeKind::For {
                    pattern,
                    iter,
                    body,
                },
            );
        };
        let mut stmts = Vec::new();
        for (i, v) in values.iter().enumerate() {
            // The last copy leaves the cursor after the body; the others rewind
            // to parse it again.
            self.pos = pat_start;
            let pat = self.parse_pattern();
            self.pos = body_start;
            let body = self.parse_block();
            let span = self.node_span(body);
            let value = self.alloc(span, NodeKind::Lit(Lit::Int(v.clone())));
            let bind = self.alloc(
                span,
                NodeKind::ConstBind {
                    pattern: pat,
                    rhs: value,
                },
            );
            // `#comptime` on the binding, so the resolver introduces a
            // compile-time constant rather than a local: the body may be typed
            // with the loop variable, and `[i]u8` is a type only a constant can
            // name.
            let directive = self.alloc(
                span,
                NodeKind::Directive {
                    name: Symbol::new("comptime"),
                    args: Vec::new(),
                },
            );
            let bind = self.alloc(
                span,
                NodeKind::Decl {
                    attrs: Vec::new(),
                    directives: vec![directive],
                    item: bind,
                },
            );
            stmts.push(self.alloc(
                span,
                NodeKind::Block {
                    stmts: vec![bind, body],
                    tail: None,
                },
            ));
            let _ = i;
        }
        // No iterations: the body is still parsed once, so the cursor ends up
        // past it and the tokens are not read as statements of the enclosing
        // block.
        if stmts.is_empty() {
            self.pos = body_start;
            let _ = self.parse_block();
        }
        self.alloc(
            start.to(self.cur_span()),
            NodeKind::Block { stmts, tail: None },
        )
    }

    /// The values an `a..<b` / `a..=b` of integer literals stands for.
    fn const_range(&self, iter: NodeId) -> Option<Vec<num_bigint::BigInt>> {
        let NodeKind::Range { start, end, kind } = self.node_kind(iter) else {
            return None;
        };
        let (Some(start), Some(end)) = (start, end) else {
            return None;
        };
        let lo = self.int_literal(start)?;
        let hi = self.int_literal(end)?;
        let hi = match kind {
            RangeKind::HalfOpen => hi,
            RangeKind::Closed => hi + 1,
            RangeKind::Open => return None,
        };
        let mut out = Vec::new();
        let mut n = lo;
        // A bound that runs backwards is an empty sequence, as a `for` over it
        // would be — not an error.
        while n < hi {
            out.push(n.clone());
            n += 1;
        }
        Some(out)
    }

    fn int_literal(&self, node: NodeId) -> Option<num_bigint::BigInt> {
        match self.node_kind(node) {
            NodeKind::Lit(Lit::Int(n)) => Some(n),
            _ => None,
        }
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
