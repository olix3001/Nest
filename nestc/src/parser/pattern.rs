//! Pattern parsing (§13.8). One grammar drives `::` / `let` / `const` binding
//! LHSs, `for` bindings, and `match` arms. All `impl Parser`.
//!
//! Range endpoints are parsed as **expressions** (a `Lit` or a `Path`), matching
//! how [`NodeKind::RangePat`] stores them; a plain literal with no range token
//! following becomes a [`NodeKind::LitPat`]. Only literal / char range bounds
//! (and leading `..<` / `..=` bounds) are recognised — identifier bounds are
//! treated as ordinary bindings.

use crate::common::span::Span;

use super::ast::{Lit, NodeId, NodeKind, RangeKind, SliceRest, VariantPatArgs};
use super::lexer::TokenKind;
use super::parse::Parser;

impl Parser {
    /// A full pattern, including a top-level or-pattern (`a | b | c`).
    pub(crate) fn parse_pattern(&mut self) -> NodeId {
        let first = self.parse_pattern_no_or();
        if !self.at(&TokenKind::Pipe) {
            return first;
        }
        let start = self.node_span(first);
        let mut alternatives = vec![first];
        while self.eat(&TokenKind::Pipe) {
            alternatives.push(self.parse_pattern_no_or());
        }
        let end = self.node_span(*alternatives.last().unwrap());
        self.alloc(start.to(end), NodeKind::OrPat { alternatives })
    }

    /// A single alternative (no top-level `|`), including range patterns.
    fn parse_pattern_no_or(&mut self) -> NodeId {
        // Leading unbounded range: `..< hi`, `..= hi`.
        if matches!(self.peek(), Some(TokenKind::DotDotLt | TokenKind::DotDotEq)) {
            return self.parse_range_pat(None);
        }
        // A literal may start a range (`400..=499`, `500..`).
        if self.at_literal() {
            let start = self.cur_span();
            let lit = self.parse_literal_value();
            if matches!(
                self.peek(),
                Some(TokenKind::DotDot | TokenKind::DotDotLt | TokenKind::DotDotEq)
            ) {
                let start_expr = self.alloc(start, NodeKind::Lit(lit));
                return self.parse_range_pat(Some(start_expr));
            }
            return self.alloc(start, NodeKind::LitPat(lit));
        }
        self.parse_pattern_atom()
    }

    /// Finish a range pattern once the optional start expression and the range
    /// token position are reached.
    fn parse_range_pat(&mut self, start: Option<NodeId>) -> NodeId {
        let start_span = start.map_or_else(|| self.cur_span(), |s| self.node_span(s));
        let kind = match self.bump().kind {
            TokenKind::DotDotLt => RangeKind::HalfOpen,
            TokenKind::DotDotEq => RangeKind::Closed,
            _ => RangeKind::Open, // `..`
        };
        let end = if kind != RangeKind::Open && self.at_literal() {
            let span = self.cur_span();
            let lit = self.parse_literal_value();
            Some(self.alloc(span, NodeKind::Lit(lit)))
        } else {
            None
        };
        let end_span = end.map_or(start_span, |e| self.node_span(e));
        self.alloc(
            start_span.to(end_span),
            NodeKind::RangePat { start, end, kind },
        )
    }

    /// A non-literal, non-range pattern atom.
    fn parse_pattern_atom(&mut self) -> NodeId {
        let start = self.cur_span();
        match self.peek() {
            Some(TokenKind::Star) => {
                self.bump();
                self.alloc(start, NodeKind::GlobPat)
            }
            Some(TokenKind::MutKw) => {
                self.bump();
                let name = self.expect_ident();
                self.alloc(
                    start.to(self.cur_span()),
                    NodeKind::BindingPat {
                        mutable: true,
                        name,
                    },
                )
            }
            Some(TokenKind::Amp) => {
                self.bump();
                let pattern = self.parse_pattern_no_or();
                self.alloc(
                    start.to(self.node_span(pattern)),
                    NodeKind::RefPat { pattern },
                )
            }
            Some(TokenKind::Dot) => self.parse_variant_pat(start),
            Some(TokenKind::DotLBrace) => {
                self.bump();
                self.parse_struct_pat_body(start)
            }
            Some(TokenKind::LBrace) => {
                self.bump();
                self.parse_struct_pat_body(start)
            }
            Some(TokenKind::LParen) => self.parse_tuple_pat(start),
            Some(TokenKind::LBracket) => self.parse_slice_pat(start),
            Some(TokenKind::Ident(sym)) => {
                // `_` wildcard, `name @ pat`, `Path(...)` tuple struct, or a plain
                // binding.
                if sym.as_str() == "_" {
                    self.bump();
                    return self.alloc(start, NodeKind::WildcardPat);
                }
                if matches!(self.peek_nth(1), Some(TokenKind::At)) {
                    let name = self.expect_ident();
                    self.bump(); // '@'
                    let pattern = self.parse_pattern_no_or();
                    return self.alloc(
                        start.to(self.node_span(pattern)),
                        NodeKind::AtPat { name, pattern },
                    );
                }
                if self.tuple_struct_ahead() {
                    let path = self.parse_type_path();
                    return self.parse_tuple_struct_pat(path, start);
                }
                let name = self.expect_ident();
                self.alloc(
                    start,
                    NodeKind::BindingPat {
                        mutable: false,
                        name,
                    },
                )
            }
            _ => {
                let span = self.cur_span();
                if !self.at_eof() {
                    self.bump();
                }
                self.error_node(span, "expected a pattern")
            }
        }
    }

    /// Whether the identifier at the cursor begins a `Path ( … )` tuple-struct
    /// pattern (possibly through a `.`-qualified or `.<…>`-instantiated path).
    fn tuple_struct_ahead(&self) -> bool {
        let mut i = 1;
        loop {
            match self.peek_nth(i) {
                Some(TokenKind::LParen) => return true,
                // Walk across `.ident` and a `.<…>` turbofish.
                Some(TokenKind::Dot)
                    if matches!(self.peek_nth(i + 1), Some(TokenKind::Ident(_))) =>
                {
                    i += 2;
                }
                Some(TokenKind::DotLt) => {
                    // Skip to the matching `>` (shallow; good enough for lookahead).
                    let mut depth = 1;
                    i += 1;
                    while depth > 0 {
                        match self.peek_nth(i) {
                            Some(TokenKind::DotLt) => depth += 1,
                            Some(TokenKind::Gt) => depth -= 1,
                            Some(TokenKind::Shr) => depth -= 2,
                            None => return false,
                            _ => {}
                        }
                        i += 1;
                    }
                }
                _ => return false,
            }
        }
    }

    /// `.name [ ( pats ) | { field_pats } ]` — an enum-variant pattern.
    fn parse_variant_pat(&mut self, start: Span) -> NodeId {
        self.bump(); // '.'
        let name = self.expect_ident();
        let mut end = self.cur_span();
        let args = match self.peek() {
            Some(TokenKind::LParen) => {
                let (elems, span) = self.parse_paren_pat_list();
                end = span;
                VariantPatArgs::Tuple(elems)
            }
            Some(TokenKind::LBrace) => {
                self.bump();
                let (fields, rest, span) = self.parse_field_pat_list();
                end = span;
                VariantPatArgs::Record { fields, rest }
            }
            _ => VariantPatArgs::None,
        };
        self.alloc(start.to(end), NodeKind::VariantPat { name, args })
    }

    /// The body of a struct/namespace pattern (opening brace already consumed):
    /// `field_pat { ',' field_pat } [ ',' '..' ]`.
    fn parse_struct_pat_body(&mut self, start: Span) -> NodeId {
        let (fields, rest, end) = self.parse_field_pat_list();
        self.alloc(
            start.to(end),
            NodeKind::StructPat {
                path: None,
                fields,
                rest,
            },
        )
    }

    /// `field_pat { ',' field_pat } [ ',' '..' ] '}'`. Returns fields, whether a
    /// `..` rest was present, and the closing-brace span.
    fn parse_field_pat_list(&mut self) -> (Vec<NodeId>, bool, Span) {
        let mut fields = Vec::new();
        let mut rest = false;
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            if self.eat(&TokenKind::DotDot) {
                rest = true;
                self.skip_newlines();
                break;
            }
            fields.push(self.parse_field_pat());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.skip_newlines();
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        (fields, rest, end)
    }

    /// `[mut] ident` (shorthand) or `ident ':' pattern` (rename / nested).
    fn parse_field_pat(&mut self) -> NodeId {
        let start = self.cur_span();
        let mutable = self.eat(&TokenKind::MutKw);
        let name = self.expect_ident();
        let mut span = start;
        let pattern = if self.eat(&TokenKind::Colon) {
            let p = self.parse_pattern();
            span = span.to(self.node_span(p));
            Some(p)
        } else {
            None
        };
        self.alloc(
            span,
            NodeKind::FieldPat {
                mutable,
                name,
                pattern,
            },
        )
    }

    /// `( pattern { ',' pattern } )` — a tuple pattern; `( p )` is grouping.
    fn parse_tuple_pat(&mut self, start: Span) -> NodeId {
        self.bump(); // '('
        self.skip_newlines();
        if self.at(&TokenKind::RParen) {
            let end = self.cur_span();
            self.bump();
            return self.alloc(start.to(end), NodeKind::TuplePat { elems: Vec::new() });
        }
        let first = self.parse_pattern();
        self.skip_newlines();
        if !self.at(&TokenKind::Comma) {
            self.expect(&TokenKind::RParen);
            return first; // grouping
        }
        let mut elems = vec![first];
        while self.eat(&TokenKind::Comma) {
            self.skip_newlines();
            if self.at(&TokenKind::RParen) {
                break;
            }
            elems.push(self.parse_pattern());
            self.skip_newlines();
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RParen);
        self.alloc(start.to(end), NodeKind::TuplePat { elems })
    }

    /// `( pattern { ',' pattern } [ ',' '..' ] )` — a parenthesised list used by
    /// tuple-struct patterns. Returns elements and closing span (the `..` rest,
    /// if any, is folded into the caller).
    fn parse_paren_pat_list(&mut self) -> (Vec<NodeId>, Span) {
        self.bump(); // '('
        let mut elems = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RParen) || self.at_eof() {
                break;
            }
            if self.at(&TokenKind::DotDot) {
                self.bump();
                self.skip_newlines();
                break;
            }
            elems.push(self.parse_pattern());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.skip_newlines();
        let end = self.cur_span();
        self.expect(&TokenKind::RParen);
        (elems, end)
    }

    /// `Path ( pattern { ',' pattern } [ ',' '..' ] )` — a tuple-struct pattern.
    fn parse_tuple_struct_pat(&mut self, path: NodeId, start: Span) -> NodeId {
        // Re-scan for a trailing `..` since `parse_paren_pat_list` swallows it.
        let rest = self.paren_list_has_rest();
        let (elems, end) = self.parse_paren_pat_list();
        self.alloc(
            start.to(end),
            NodeKind::TupleStructPat { path, elems, rest },
        )
    }

    /// Whether the parenthesised list at the cursor ends with a `..` rest.
    fn paren_list_has_rest(&self) -> bool {
        let mut depth = 0i32;
        let mut i = 0;
        while let Some(kind) = self.peek_nth(i) {
            match kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                TokenKind::DotDot if depth == 1 => return true,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// `[ pattern { ',' pattern } [ ',' '..' [ ident ] ] ]` — a slice pattern.
    fn parse_slice_pat(&mut self, start: Span) -> NodeId {
        self.bump(); // '['
        let mut elems = Vec::new();
        let mut rest = None;
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBracket) || self.at_eof() {
                break;
            }
            if self.eat(&TokenKind::DotDot) {
                // Optional binding for the rest: `.. name`. Its position splits
                // `elems` into the prefix already parsed and the suffix to come.
                let name = self.eat_ident();
                rest = Some(SliceRest {
                    at: elems.len(),
                    name,
                });
                self.skip_newlines();
                self.eat(&TokenKind::Comma);
                continue;
            }
            elems.push(self.parse_pattern());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.skip_newlines();
        let end = self.cur_span();
        self.expect(&TokenKind::RBracket);
        self.alloc(start.to(end), NodeKind::SlicePat { elems, rest })
    }

    // ===< Literal helpers >===

    /// Whether the cursor is on a literal token.
    fn at_literal(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                TokenKind::Int(_)
                    | TokenKind::Float(_)
                    | TokenKind::Str(_)
                    | TokenKind::Bytes(_)
                    | TokenKind::Char(_)
                    | TokenKind::TrueKw
                    | TokenKind::FalseKw
            )
        )
    }

    /// Consume a literal token and return its [`Lit`] value.
    fn parse_literal_value(&mut self) -> Lit {
        match self.bump().kind {
            TokenKind::Int(n) => Lit::Int(n),
            TokenKind::Float(x) => Lit::Float(x.value),
            TokenKind::Str(s) => Lit::Str(s),
            TokenKind::Bytes(b) => Lit::Bytes(b),
            TokenKind::Char(c) => Lit::Char(c),
            TokenKind::TrueKw => Lit::Bool(true),
            TokenKind::FalseKw => Lit::Bool(false),
            _ => unreachable!("guarded by at_literal"),
        }
    }
}
