//! Type-forming syntax: the `type` grammar (§13.3), generic parameter/argument
//! lists (§13.5), function parameters, and the `struct` / `enum` / `trait` /
//! `func` literals that double as anonymous types. All of it is `impl Parser`.

use crate::common::span::Span;
use crate::common::symbol::Symbol;

use super::ast::{NodeId, NodeKind, StructKind, VariantPayload};
use super::lexer::TokenKind;
use super::parse::Parser;

impl Parser {
    /// Parse a type expression (§13.3). Leading `#` directives (e.g. `#soa`,
    /// `#raw`, `#packed`) bind to the slice / array / struct / enum / trait that
    /// follows.
    pub(crate) fn parse_type(&mut self) -> NodeId {
        let start = self.cur_span();
        let directives = self.parse_directives();

        match self.peek() {
            Some(TokenKind::DistinctKw) => {
                self.reject_directives(&directives, "distinct type");
                self.bump();
                let inner = self.parse_type();
                let span = start.to(self.node_span(inner));
                self.alloc(span, NodeKind::DistinctType { inner })
            }
            Some(TokenKind::StructKw) => self.parse_struct_type(directives),
            Some(TokenKind::EnumKw) => self.parse_enum_type(directives),
            Some(TokenKind::TraitKw) => self.parse_trait_type(directives),
            Some(TokenKind::DynKw) => {
                self.reject_directives(&directives, "dyn type");
                self.bump();
                let inner = self.parse_type();
                let span = start.to(self.node_span(inner));
                self.alloc(span, NodeKind::DynType { inner })
            }
            Some(TokenKind::FuncKw) => {
                self.reject_directives(&directives, "function type");
                self.parse_func_type()
            }
            Some(TokenKind::Star) => {
                self.reject_directives(&directives, "pointer type");
                self.bump();
                let mutable = self.eat(&TokenKind::MutKw);
                let inner = self.parse_type();
                let span = start.to(self.node_span(inner));
                self.alloc(span, NodeKind::PtrType { mutable, inner })
            }
            Some(TokenKind::LBracket) => self.parse_bracket_type(directives),
            Some(TokenKind::LParen) => {
                self.reject_directives(&directives, "tuple type");
                self.parse_tuple_type(start)
            }
            // `Self`, an identifier, or either as the root of a `.`-qualified,
            // optionally `.<…>`-instantiated path.
            _ => {
                self.reject_directives(&directives, "named type");
                self.parse_type_path()
            }
        }
    }

    /// `[]` slice or `[len]` array, with any leading directives already parsed.
    pub(crate) fn parse_bracket_type(&mut self, directives: Vec<NodeId>) -> NodeId {
        let start = directives
            .first()
            .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
        self.expect(&TokenKind::LBracket);
        if self.eat(&TokenKind::RBracket) {
            let mutable = self.eat(&TokenKind::MutKw);
            let inner = self.parse_type();
            let span = start.to(self.node_span(inner));
            return self.alloc(
                span,
                NodeKind::SliceType {
                    directives,
                    mutable,
                    inner,
                },
            );
        }
        // `[_]T` — the length is left to inference, filled in by the composite
        // literal that builds the array (§3.2).
        let len = if matches!(self.peek(), Some(TokenKind::Ident(sym)) if sym.as_str() == "_")
            && matches!(self.peek_nth(1), Some(TokenKind::RBracket))
        {
            let span = self.cur_span();
            self.bump();
            self.alloc(span, NodeKind::TypeHole)
        } else {
            self.allowing_struct_lit(Parser::parse_expr)
        };
        self.expect(&TokenKind::RBracket);
        let mutable = self.eat(&TokenKind::MutKw);
        let inner = self.parse_type();
        let span = start.to(self.node_span(inner));
        self.alloc(
            span,
            NodeKind::ArrayType {
                directives,
                len,
                mutable,
                inner,
            },
        )
    }

    /// `( [type {, type}] )` — a tuple type; `()` is `void`.
    fn parse_tuple_type(&mut self, start: Span) -> NodeId {
        self.expect(&TokenKind::LParen);
        let mut elems = Vec::new();
        self.skip_newlines();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            elems.push(self.parse_type());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RParen);
        self.alloc(start.to(end), NodeKind::TupleType { elems })
    }

    /// `qualified_name [ generic_args ]` — a named (possibly instantiated) type.
    pub(crate) fn parse_type_path(&mut self) -> NodeId {
        let path = self.parse_qualified_name();
        let mut span = self.node_span(path);
        let generic_args = if self.at(&TokenKind::DotLt) {
            let (args, end) = self.parse_generic_args();
            span = span.to(end);
            args
        } else {
            Vec::new()
        };
        self.alloc(span, NodeKind::TypePath { path, generic_args })
    }

    /// `ident { '.' ident }` collected into a single multi-segment
    /// [`NodeKind::Path`]. `self` / `Self` are ordinary identifiers here, so
    /// `Self`, `Self.Residual`, and `Self.Item.<T>` are just paths whose root
    /// name resolution reads as the implementing type.
    pub(crate) fn parse_qualified_name(&mut self) -> NodeId {
        let start = self.cur_span();
        let mut segments = vec![self.expect_ident()];
        let mut end = start;
        while matches!(self.peek(), Some(TokenKind::Dot))
            && matches!(self.peek_nth(1), Some(TokenKind::Ident(_)))
        {
            self.bump(); // '.'
            end = self.cur_span();
            segments.push(self.expect_ident());
        }
        self.alloc(start.to(end), NodeKind::Path { segments })
    }

    /// `.< arg { ',' arg } >` — a turbofish argument list, where each `arg` is a
    /// type, a `_` hole, or a `name = type` associated-type binding
    /// (`Iterator.<Item = int32>`). Returns the argument nodes and the closing
    /// `>` span.
    pub(crate) fn parse_generic_args(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::DotLt);
        let mut args = Vec::new();
        self.skip_newlines();
        while !self.at_gt_close() && !self.at_eof() {
            args.push(self.parse_generic_arg());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        let end = self.cur_span();
        self.eat_gt();
        (args, end)
    }

    /// One turbofish argument: a `name = type` associated-type binding, a `_`
    /// hole, an integer literal filling a `const` parameter, or a type.
    fn parse_generic_arg(&mut self) -> NodeId {
        // `zeros.<4>()` — a value argument for a `<const N: usize>` parameter
        // (§5). Only a literal is accepted here; a named constant reaches a
        // `const` slot as an ordinary path, handled by the type branch below.
        if let Some(TokenKind::Int(n)) = self.peek() {
            let n = n.clone();
            let span = self.cur_span();
            self.bump();
            return self.alloc(span, NodeKind::Lit(crate::parser::ast::Lit::Int(n)));
        }
        // `Item = type` — an associated-type constraint. A plain type is never
        // followed by `=` inside `.<...>`, so `ident '='` is unambiguous.
        if matches!(self.peek(), Some(TokenKind::Ident(_)))
            && matches!(self.peek_nth(1), Some(TokenKind::Eq))
        {
            let start = self.cur_span();
            let name = self.expect_ident();
            self.bump(); // '='
            let ty = self.parse_type();
            return self.alloc(
                start.to(self.node_span(ty)),
                NodeKind::AssocBinding { name, ty },
            );
        }
        self.parse_type_or_hole()
    }

    /// `type | '_'` — one turbofish argument; `_` requests inference.
    fn parse_type_or_hole(&mut self) -> NodeId {
        if matches!(self.peek(), Some(TokenKind::Ident(sym)) if sym.as_str() == "_")
            && !matches!(self.peek_nth(1), Some(TokenKind::Dot | TokenKind::DotLt))
        {
            let span = self.cur_span();
            self.bump();
            return self.alloc(span, NodeKind::TypeHole);
        }
        self.parse_type()
    }

    /// Whether the next token can close a `< … >` list (a `>`, or a `>>` / `>=`
    /// whose leading `>` closes it).
    fn at_gt_close(&self) -> bool {
        matches!(
            self.peek(),
            Some(TokenKind::Gt | TokenKind::Shr | TokenKind::GtEq)
        )
    }

    // ===< Generic parameter declarations >===

    /// `< generic_param { ',' generic_param } >` — the declaration form used by
    /// `func <T>`, `struct <T>`, `enum <T>`, `trait <T>`, and `impl <T>`.
    /// Returns an empty list when no `<` is present.
    pub(crate) fn parse_generics(&mut self) -> Vec<NodeId> {
        if !self.eat(&TokenKind::Lt) {
            return Vec::new();
        }
        let mut params = Vec::new();
        self.skip_newlines();
        while !self.at_gt_close() && !self.at_eof() {
            params.push(self.parse_generic_param());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.eat_gt();
        params
    }

    /// `const ident ':' type` (value param) or `ident [ ':' bounds ]` (type param).
    fn parse_generic_param(&mut self) -> NodeId {
        let start = self.cur_span();
        if self.eat(&TokenKind::ConstKw) {
            let name = self.expect_ident();
            self.expect(&TokenKind::Colon);
            let ty = self.parse_type();
            let span = start.to(self.node_span(ty));
            return self.alloc(span, NodeKind::GenericConstParam { name, ty });
        }
        let name = self.expect_ident();
        let mut span = start;
        let constraint = if self.eat(&TokenKind::Colon) {
            let bounds = self.parse_bounds();
            span = span.to(self.node_span(bounds));
            Some(bounds)
        } else {
            None
        };
        self.alloc(span, NodeKind::GenericTypeParam { name, constraint })
    }

    /// `type { '+' type }` — a `+`-separated bound list, wrapped in
    /// [`NodeKind::Bounds`].
    pub(crate) fn parse_bounds(&mut self) -> NodeId {
        let start = self.cur_span();
        let mut bounds = vec![self.parse_type()];
        let mut end = self.node_span(bounds[0]);
        while self.eat(&TokenKind::Plus) {
            let ty = self.parse_type();
            end = self.node_span(ty);
            bounds.push(ty);
        }
        self.alloc(start.to(end), NodeKind::Bounds { bounds })
    }

    // ===< Function parameters >===

    /// `( [ param { ',' param } ] )` — a parenthesised parameter list for a
    /// `func` literal. `named` decides whether bare-name closure params (no
    /// type) are accepted.
    pub(crate) fn parse_params(&mut self) -> Vec<NodeId> {
        self.expect(&TokenKind::LParen);
        let mut params = Vec::new();
        self.skip_newlines();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            params.push(self.parse_param());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(&TokenKind::RParen);
        params
    }

    /// `ident [ ':' type ]` — one parameter. The receiver `self` is just the
    /// parameter named `self`; its type is optional (defaulting to `Self`), as it
    /// is for inferred closure parameters.
    fn parse_param(&mut self) -> NodeId {
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
        self.alloc(span, NodeKind::Param { name, ty })
    }

    // ===< struct / enum / trait literals >===

    /// `[directives] struct [<g>] [body]`.
    pub(crate) fn parse_struct_type(&mut self, directives: Vec<NodeId>) -> NodeId {
        let start = directives
            .first()
            .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
        self.expect(&TokenKind::StructKw);
        let generics = self.parse_generics();
        let mut end = self.cur_span();

        let kind = match self.peek() {
            Some(TokenKind::LBrace) => {
                let (members, span) = self.parse_struct_record_body();
                end = span;
                StructKind::Record(members)
            }
            Some(TokenKind::LParen) => {
                let (types, span) = self.parse_tuple_struct_body();
                end = span;
                StructKind::Tuple(types)
            }
            _ => StructKind::Unit,
        };
        self.alloc(
            start.to(end),
            NodeKind::StructType {
                directives,
                generics,
                kind,
            },
        )
    }

    /// `{ (field | comptime_item)* }` — a record struct body.
    fn parse_struct_record_body(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::LBrace);
        let mut members = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            // A `$intrinsic(...)` comptime item may sit among the fields.
            if matches!(self.peek(), Some(TokenKind::Ident(s)) if s.as_str().starts_with('$')) {
                members.push(self.parse_expr());
            } else {
                members.push(self.parse_field());
            }
            self.skip_newlines();
            self.eat(&TokenKind::Comma);
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        (members, end)
    }

    /// `( type { ',' type } )` — a tuple-struct body.
    fn parse_tuple_struct_body(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::LParen);
        let mut types = Vec::new();
        self.skip_newlines();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            types.push(self.parse_type());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RParen);
        (types, end)
    }

    /// `[attrs] ident ':' type` — one record field.
    fn parse_field(&mut self) -> NodeId {
        let start = self.cur_span();
        let attrs = self.parse_attributes();
        let name = self.expect_ident();
        self.expect(&TokenKind::Colon);
        let ty = self.parse_type();
        let span = start.to(self.node_span(ty));
        self.alloc(span, NodeKind::Field { attrs, name, ty })
    }

    /// `[directives] enum [<g>] { variant* }`.
    pub(crate) fn parse_enum_type(&mut self, directives: Vec<NodeId>) -> NodeId {
        let start = directives
            .first()
            .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
        self.expect(&TokenKind::EnumKw);
        let generics = self.parse_generics();
        self.expect(&TokenKind::LBrace);
        let mut variants = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            variants.push(self.parse_variant());
            self.skip_newlines();
            self.eat(&TokenKind::Comma);
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        self.alloc(
            start.to(end),
            NodeKind::EnumType {
                directives,
                generics,
                variants,
            },
        )
    }

    /// `[attrs] name [ payload ]` — one enum variant declaration.
    fn parse_variant(&mut self) -> NodeId {
        let start = self.cur_span();
        let attrs = self.parse_attributes();
        let name = self.expect_ident();
        let mut end = start;
        let payload = match self.peek() {
            Some(TokenKind::LParen) => {
                let (types, span) = self.parse_tuple_struct_body();
                end = span;
                VariantPayload::Tuple(types)
            }
            Some(TokenKind::LBrace) => {
                let (fields, span) = self.parse_variant_record_body();
                end = span;
                VariantPayload::Record(fields)
            }
            _ => VariantPayload::None,
        };
        self.alloc(
            start.to(end),
            NodeKind::Variant {
                attrs,
                name,
                payload,
            },
        )
    }

    /// `{ field { ',' field } }` — a record-payload variant body.
    fn parse_variant_record_body(&mut self) -> (Vec<NodeId>, Span) {
        self.expect(&TokenKind::LBrace);
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            fields.push(self.parse_field());
            self.skip_newlines();
            self.eat(&TokenKind::Comma);
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        (fields, end)
    }

    /// `[directives] trait [<g>] { member* }` — members are `::` bindings
    /// (method signatures / associated types) and comptime items.
    pub(crate) fn parse_trait_type(&mut self, directives: Vec<NodeId>) -> NodeId {
        let start = directives
            .first()
            .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
        self.expect(&TokenKind::TraitKw);
        let generics = self.parse_generics();
        self.expect(&TokenKind::LBrace);
        let mut members = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            members.push(self.parse_trait_member());
            self.skip_newlines();
            self.eat(&TokenKind::Comma);
        }
        let end = self.cur_span();
        self.expect(&TokenKind::RBrace);
        self.alloc(
            start.to(end),
            NodeKind::TraitType {
                directives,
                generics,
                members,
            },
        )
    }

    /// One trait member: a `$intrinsic(...)` comptime item, or a `name :: rhs`
    /// binding whose RHS is either a bodyless `func` (a method signature) or the
    /// contextual `type [ ':' bounds ]` (an associated type).
    fn parse_trait_member(&mut self) -> NodeId {
        if matches!(self.peek(), Some(TokenKind::Ident(s)) if s.as_str().starts_with('$')) {
            return self.parse_expr();
        }
        let start = self.cur_span();
        let pattern = self.parse_pattern();
        self.expect(&TokenKind::ColonColon);

        let rhs = if self.at_contextual("type") {
            self.parse_assoc_type()
        } else {
            let directives = self.parse_directives();
            self.parse_func_expr(directives)
        };
        let span = start.to(self.node_span(rhs));
        self.alloc(span, NodeKind::ConstBind { pattern, rhs })
    }

    /// The `type [ ':' bounds ]` RHS of an associated-type binding.
    fn parse_assoc_type(&mut self) -> NodeId {
        let start = self.cur_span();
        self.eat_contextual("type");
        let mut end = start;
        let bounds = if self.eat(&TokenKind::Colon) {
            let mut list = Vec::new();
            list.push(self.parse_type());
            end = self.node_span(list[0]);
            while self.eat(&TokenKind::Plus) {
                let ty = self.parse_type();
                end = self.node_span(ty);
                list.push(ty);
            }
            list
        } else {
            Vec::new()
        };
        self.alloc(start.to(end), NodeKind::AssocType { bounds })
    }

    // ===< func literal / func type >===

    /// `[extern(abi)] func [<g>] ( params ) [ '-> ret ] [ block ]`. A missing
    /// body is an external (bodyless) declaration. Used for definitions and
    /// closures; the caller supplies any leading directives.
    pub(crate) fn parse_func_expr(&mut self, directives: Vec<NodeId>) -> NodeId {
        let start = directives
            .first()
            .map_or_else(|| self.cur_span(), |&d| self.node_span(d));
        let extern_abi = self.parse_extern_spec();
        self.expect(&TokenKind::FuncKw);
        let generics = self.parse_generics();
        let params = self.parse_params();
        let mut end = self.cur_span();
        let ret = if self.eat(&TokenKind::Arrow) {
            let ty = self.parse_type();
            end = self.node_span(ty);
            Some(ty)
        } else {
            None
        };
        let body = if self.at(&TokenKind::LBrace) {
            let block = self.parse_block();
            end = self.node_span(block);
            Some(block)
        } else {
            None
        };
        self.alloc(
            start.to(end),
            NodeKind::FuncExpr {
                directives,
                extern_abi,
                generics,
                params,
                ret,
                body,
            },
        )
    }

    /// `func [<g>] ( param_types ) [ '-> ret ]` — a function *type* (bare-type
    /// parameters, never a body).
    fn parse_func_type(&mut self) -> NodeId {
        let start = self.cur_span();
        self.expect(&TokenKind::FuncKw);
        let generics = self.parse_generics();
        self.expect(&TokenKind::LParen);
        let mut params = Vec::new();
        self.skip_newlines();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            params.push(self.parse_type());
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        let mut end = self.cur_span();
        self.expect(&TokenKind::RParen);
        let ret = if self.eat(&TokenKind::Arrow) {
            let ty = self.parse_type();
            end = self.node_span(ty);
            Some(ty)
        } else {
            None
        };
        self.alloc(
            start.to(end),
            NodeKind::FuncType {
                generics,
                params,
                ret,
            },
        )
    }

    /// `extern '(' string ')'` — the ABI selector before `func`; `None` when
    /// absent.
    pub(crate) fn parse_extern_spec(&mut self) -> Option<Symbol> {
        if !self.eat(&TokenKind::ExternKw) {
            return None;
        }
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
        Some(abi)
    }

    /// Record an error for each directive that decorated a construct that cannot
    /// carry directives.
    fn reject_directives(&mut self, directives: &[NodeId], what: &str) {
        for &d in directives {
            let span = self.node_span(d);
            self.error(span, format!("directives are not allowed on a {what}"));
        }
    }
}
