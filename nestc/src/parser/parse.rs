//! Recursive-descent parser for the Nest language.
//!
//! Hand-written, single pass, no parser-generator. The parser pulls an eager
//! token vector from [`LogosLexer`], **filters insignificant newlines** (§1.1 of
//! the spec — a newline that sits where the statement is syntactically
//! incomplete is not a terminator), then descends the grammar building nodes in
//! the flat [`Ast`] arena. Each `parse_*` method returns the [`NodeId`] it
//! allocated.
//!
//! Error handling is **non-fatal**: a syntax error is recorded in
//! [`Parser::errors`] and an [`NodeKind::Error`] placeholder is produced so the
//! walk can continue, so one bad construct does not abandon the rest of the
//! file. The implementation is split across sibling modules by grammar area
//! ([`expr`](super::expr), [`types`](super::types), [`pattern`](super::pattern),
//! [`item`](super::item)); this module owns the [`Parser`] struct and the token
//! plumbing every area shares.

use crate::common::span::Span;
use crate::common::symbol::Symbol;

use super::ast::{Ast, FileId, NodeId, NodeKind};
use super::lexer::{LogosLexer, Token, TokenKind};

/// A syntax (or surfaced lexical) error: a message pinned to a source [`Span`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

/// The recursive-descent parser: a filtered token stream plus the arena being
/// built.
pub struct Parser {
    /// Tokens after newline filtering (see [`filter_newlines`]).
    tokens: Vec<Token>,
    /// Index of the next unconsumed token.
    ///
    /// Readable and writable within the parser because one construct rewinds
    /// it: `#comptime for` parses its body once per iteration, and a copy of a
    /// body is the same tokens read again (see `parse_comptime_for`).
    pub(crate) pos: usize,
    /// The arena under construction.
    ast: Ast,
    /// File every node is tagged with.
    file: FileId,
    /// Byte length of the source, for the end-of-input span.
    src_len: usize,
    /// The source itself, read for `///` doc comments (see
    /// [`Parser::doc_before`]): the lexer skips every `//` comment, and a doc
    /// comment is only a doc where a declaration follows it.
    src: String,
    /// Accumulated diagnostics; parsing never aborts on the first one.
    errors: Vec<ParseError>,
    /// When set, a bare `Name { ... }` is **not** treated as a struct literal —
    /// the `{` opens a control-flow body instead. Set only while parsing the
    /// head expression of `if` / `while` / `for … in`; a parenthesised
    /// sub-expression clears it (see [`Parser::allowing_struct_lit`]).
    no_struct_lit: bool,
}

/// An empty literal of the kind whose text starts at `span`, when it is a
/// string literal of some kind.
fn literal_stand_in(source: &str, span: Span) -> Option<TokenKind> {
    let text = source.get(span.start..)?;
    if text.starts_with("b\"") {
        Some(TokenKind::Bytes(Vec::new()))
    } else if text.starts_with("c\"") {
        Some(TokenKind::CStr(String::new()))
    } else if text.starts_with('"') || text.starts_with("f\"") {
        Some(TokenKind::Str(String::new()))
    } else {
        None
    }
}

impl Parser {
    /// Tokenize and newline-filter `source`, ready to parse as `file`.
    pub fn new(source: &str, file: FileId) -> Self {
        let lexer = LogosLexer::new(source);
        let mut errors = Vec::new();
        let mut raw = Vec::new();
        for entry in lexer.as_slice() {
            match entry {
                Ok(token) => raw.push(token.clone()),
                Err(err) => {
                    errors.push(ParseError {
                        span: err.span,
                        message: err.kind.to_string(),
                    });
                    // A string literal that failed to lex is still a string
                    // literal where it stands: an empty one of its kind keeps
                    // the statement around it whole, rather than every parse
                    // error after it being the same mistake again.
                    if let Some(kind) = literal_stand_in(source, err.span) {
                        raw.push(Token::new(kind, err.span));
                    }
                }
            }
        }
        Self {
            tokens: filter_newlines(raw),
            pos: 0,
            ast: Ast::new(),
            file,
            src_len: source.len(),
            src: source.to_string(),
            errors,
            no_struct_lit: false,
        }
    }

    /// Parse `source` as a whole file, returning the finished arena (its root is
    /// the [`NodeKind::File`] node) and every diagnostic collected on the way.
    pub fn parse_file(source: &str, file: FileId) -> (Ast, Vec<ParseError>) {
        let mut parser = Self::new(source, file);
        let root = parser.file_root();
        parser.ast.set_root(root);
        (parser.ast, parser.errors)
    }

    // ===< Arena access >===

    /// Allocate a node and return its id.
    pub(crate) fn alloc(&mut self, span: Span, kind: NodeKind) -> NodeId {
        self.ast.alloc(span, self.file, kind)
    }

    /// Attach metadata to an already-allocated node.
    pub(crate) fn set_meta<T: std::any::Any + Send>(&mut self, id: NodeId, value: T) {
        self.ast.set_meta(id, value);
    }

    /// The span of an already-allocated node.
    /// The kind of a node already built. Used by the one construct that has to
    /// *read back* what it just parsed: `#comptime for` asks whether its
    /// iterable is a range of literals.
    pub(crate) fn node_kind(&self, id: NodeId) -> NodeKind {
        self.ast.node(id).kind.clone()
    }

    pub(crate) fn node_span(&self, id: NodeId) -> Span {
        self.ast.node(id).span
    }

    /// Inspect an allocated node's kind through `f` without keeping the borrow.
    pub(crate) fn with_kind<R>(&self, id: NodeId, f: impl FnOnce(&NodeKind) -> R) -> R {
        f(&self.ast.node(id).kind)
    }

    /// Clone an allocated node's kind (used when appending a trailing-closure
    /// argument to an already-built call).
    pub(crate) fn clone_kind(&self, id: NodeId) -> NodeKind {
        self.ast.node(id).kind.clone()
    }

    /// Overwrite an allocated node's span and kind in place.
    pub(crate) fn set_node(&self, id: NodeId, span: Span, kind: NodeKind) {
        let mut node = self.ast.node_mut(id);
        node.span = span;
        node.kind = kind;
    }

    /// Record a diagnostic; the caller decides what placeholder to emit.
    pub(crate) fn error(&mut self, span: Span, message: impl Into<String>) {
        self.errors.push(ParseError {
            span,
            message: message.into(),
        });
    }

    /// How many diagnostics have been recorded — a mark, so a caller can tell
    /// whether the parse it just ran reported anything of its own.
    pub(crate) fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// Emit an [`NodeKind::Error`] placeholder at `span` after recording `msg`.
    pub(crate) fn error_node(&mut self, span: Span, msg: impl Into<String>) -> NodeId {
        self.error(span, msg);
        self.alloc(span, NodeKind::Error)
    }

    // ===< Token cursor >===

    /// The next token, if any (newlines included — they survive filtering only
    /// at statement boundaries).
    pub(crate) fn peek_tok(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    /// The kind of the next token.
    pub(crate) fn peek(&self) -> Option<&TokenKind> {
        self.peek_tok().map(|t| &t.kind)
    }

    /// The kind `n` tokens ahead (`0` == [`Parser::peek`]).
    /// Whether the parser is looking at a **comptime item** — a bare call in a
    /// position that otherwise holds declarations (§6.10).
    ///
    /// `assert(size_of.<Self>() == 64)` may sit among a struct's fields, a
    /// trait's members, or a namespace's items. With `$assert` retired the
    /// sigil no longer marks it, so the shape does: every declaration in these
    /// positions is `name :: rhs` (or `name: ty` for a field), and a call is an
    /// identifier followed by anything else. One token of lookahead decides it.
    pub(crate) fn at_comptime_item(&self) -> bool {
        matches!(self.peek(), Some(TokenKind::Ident(_)))
            && matches!(
                self.peek_nth(1),
                Some(TokenKind::LParen | TokenKind::Dot | TokenKind::DotLt | TokenKind::LBracket)
            )
    }

    /// The parser's current position, for a caller that needs to prove it moved.
    ///
    /// A body loop that parses members until `}` spins forever if an iteration
    /// consumes nothing, and `expect_*` deliberately reports without consuming so
    /// that a missing token is recovered at the *next* item rather than by
    /// eating the one that follows. The two together are a hang, so every such
    /// loop checks this and bumps when it has not moved. `P :: struct { x: i32
    /// := 1 }` hung the compiler until it did.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Guarantee progress in a body loop: bump if `before` is still the cursor.
    /// Returns whether the loop should stop (end of input).
    pub(crate) fn ensure_progress(&mut self, before: usize) -> bool {
        if self.pos != before {
            return false;
        }
        if self.at_eof() {
            return true;
        }
        self.bump();
        false
    }

    pub(crate) fn peek_nth(&self, n: usize) -> Option<&TokenKind> {
        self.tokens.get(self.pos + n).map(|t| &t.kind)
    }

    /// Whether every token has been consumed.
    pub(crate) fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// Span of the next token, or a zero-width span at end of input.
    pub(crate) fn cur_span(&self) -> Span {
        self.peek_tok()
            .map(|t| t.span)
            .unwrap_or_else(|| Span::new(self.src_len, self.src_len))
    }

    /// Whether the next token equals `kind` (payload included). Intended for
    /// payload-free tokens — keywords and punctuation.
    pub(crate) fn at(&self, kind: &TokenKind) -> bool {
        self.peek() == Some(kind)
    }

    /// Consume and return the next token unconditionally. Only call when not at
    /// end of input.
    pub(crate) fn bump(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        self.pos += 1;
        token
    }

    /// Consume the next token iff it equals `kind`; report whether it did.
    pub(crate) fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume `kind` or record a diagnostic (without consuming) if it is
    /// absent. Returns whether it was present.
    pub(crate) fn expect(&mut self, kind: &TokenKind) -> bool {
        if self.eat(kind) {
            true
        } else {
            let span = self.cur_span();
            self.error(
                span,
                format!("expected {kind:?}, found {}", self.describe_next()),
            );
            false
        }
    }

    /// The `///` lines written right before the next token, without their
    /// slashes, and where they are — or `None` when there are none.
    ///
    /// The lexer skips every comment, so this reads the source between the last
    /// token that is not a newline and the next one: what is there is blank
    /// space and comments. A plain `//` line after a doc line ends that doc
    /// (it documented something else, or nothing); a blank line does not.
    /// `////` is a plain comment, as it is in Rust.
    pub(crate) fn doc_before(&self) -> Option<(Span, String)> {
        let mut back = self.pos;
        while back > 0 && matches!(self.tokens[back - 1].kind, TokenKind::Newline) {
            back -= 1;
        }
        let from = if back == 0 { 0 } else { self.tokens[back - 1].span.end };
        let to = self.cur_span().start.min(self.src.len());
        let gap = self.src.get(from..to)?;
        let mut lines: Vec<&str> = Vec::new();
        let mut first = None;
        let mut offset = from;
        for line in gap.split_inclusive('\n') {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("///").filter(|_| !t.starts_with("////")) {
                if lines.is_empty() {
                    first = Some(offset + (line.len() - line.trim_start().len()));
                }
                lines.push(rest.strip_prefix(' ').unwrap_or(rest));
            } else if t.starts_with("//") {
                lines.clear();
                first = None;
            }
            offset += line.len();
        }
        let start = first?;
        Some((Span::new(start, to), lines.join("\n")))
    }

    /// Consume runs of [`TokenKind::Newline`].
    pub(crate) fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(TokenKind::Newline)) {
            self.pos += 1;
        }
    }

    /// Whether the next token ends the current statement (a significant newline,
    /// a `;`, a closing `}`, or end of input).
    pub(crate) fn at_stmt_end(&self) -> bool {
        matches!(
            self.peek(),
            None | Some(TokenKind::Newline | TokenKind::Semicolon | TokenKind::RBrace)
        )
    }

    // ===< Identifiers and contextual keywords >===

    /// Consume the next token if it is an identifier, returning its symbol.
    pub(crate) fn eat_ident(&mut self) -> Option<Symbol> {
        if let Some(TokenKind::Ident(sym)) = self.peek() {
            let sym = sym.clone();
            self.pos += 1;
            Some(sym)
        } else {
            None
        }
    }

    /// Consume an identifier, or record an error and return `_error` as a
    /// stand-in name so the caller can keep building a node.
    pub(crate) fn expect_ident(&mut self) -> Symbol {
        match self.eat_ident() {
            Some(sym) => sym,
            None => {
                let span = self.cur_span();
                self.error(
                    span,
                    format!("expected identifier, found {}", self.describe_next()),
                );
                Symbol::new("_error")
            }
        }
    }

    /// Whether the next token is the contextual keyword `word` (an identifier
    /// spelled exactly `word`, e.g. `in` or `type`, which the lexer does not
    /// reserve).
    pub(crate) fn at_contextual(&self, word: &str) -> bool {
        matches!(self.peek(), Some(TokenKind::Ident(sym)) if sym.as_str() == word)
    }

    /// Consume the contextual keyword `word` if present.
    pub(crate) fn eat_contextual(&mut self, word: &str) -> bool {
        if self.at_contextual(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // ===< `>` splitting for nested generics >===

    /// Consume a closing `>` for a generic-argument or parameter list, splitting
    /// a `>>` (`Shr`) or `>=` (`GtEq`) token so the surplus `>`/`=` stays in the
    /// stream. This is what lets `Vector.<Storage.<int>>` close cleanly even
    /// though the lexer greedily formed a single `>>`.
    pub(crate) fn eat_gt(&mut self) -> bool {
        match self.peek() {
            Some(TokenKind::Gt) => {
                self.pos += 1;
                true
            }
            Some(TokenKind::Shr) => {
                let sp = self.tokens[self.pos].span;
                self.tokens[self.pos] = Token::new(TokenKind::Gt, Span::new(sp.start + 1, sp.end));
                true
            }
            Some(TokenKind::GtEq) => {
                let sp = self.tokens[self.pos].span;
                self.tokens[self.pos] = Token::new(TokenKind::Eq, Span::new(sp.start + 1, sp.end));
                true
            }
            _ => false,
        }
    }

    // ===< Struct-literal suppression >===

    /// Run `f` with struct literals re-enabled (used inside parentheses, call
    /// arguments, brackets, and other unambiguous sub-expression positions),
    /// restoring the previous flag afterwards.
    pub(crate) fn allowing_struct_lit<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.no_struct_lit, false);
        let out = f(self);
        self.no_struct_lit = saved;
        out
    }

    /// Run `f` with struct literals suppressed (the head expression of a
    /// control-flow construct).
    pub(crate) fn suppressing_struct_lit<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.no_struct_lit, true);
        let out = f(self);
        self.no_struct_lit = saved;
        out
    }

    /// Whether struct literals are currently suppressed.
    pub(crate) fn struct_lit_suppressed(&self) -> bool {
        self.no_struct_lit
    }

    /// A short human description of the next token, for diagnostics.
    pub(crate) fn describe_next(&self) -> String {
        match self.peek() {
            None => "end of input".into(),
            Some(kind) => format!("{kind:?}"),
        }
    }
}

/// Whether a line **ending** with `kind` leaves the statement syntactically
/// incomplete, so the following newline is not a terminator (spec §1.1).
///
/// Comparison and shift closers (`<`, `>`, `>>`, …) are deliberately excluded:
/// a line commonly *ends* on a generic close such as `f.<int32>`, and treating
/// that `>`/`>>` as "needs a right operand" would wrongly splice the next line.
fn continues_after(kind: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        // open delimiters
        LParen | LBracket | LBrace | DotLt | DotLBrace
        // arithmetic / logical / bitwise binaries
        | Plus | Minus | Star | Slash | Percent
        | AmpAmp | PipePipe | AndKw | OrKw
        | Amp | Pipe | Caret
        // assignment
        | Eq | PlusEq | MinusEq | StarEq | SlashEq | PercentEq
        // separators that demand more
        | Comma | Arrow | FatArrow | ColonColon | ColonEq | Dot
    )
}

/// Whether a line **beginning** with `kind` continues the previous statement
/// (spec §1.1: a next line starting with `+`, `::`, or `.`).
fn continues_before(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Plus | TokenKind::ColonColon | TokenKind::Dot
    )
}

/// Drop newlines that fall in a continuation position and collapse consecutive
/// significant newlines to one, so the parser sees a `Newline` token only where
/// a statement genuinely ends.
fn filter_newlines(raw: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(raw.len());
    for (i, token) in raw.iter().enumerate() {
        if token.kind != TokenKind::Newline {
            out.push(token.clone());
            continue;
        }
        // Continuation forced by the token before the newline.
        if out.last().is_some_and(|prev| continues_after(&prev.kind)) {
            continue;
        }
        // Continuation forced by the next non-newline token.
        let next = raw[i + 1..].iter().find(|t| t.kind != TokenKind::Newline);
        if next.is_some_and(|t| continues_before(&t.kind)) {
            continue;
        }
        // Significant, but never at the very start or as a duplicate.
        if out.is_empty() || out.last().map(|t| &t.kind) == Some(&TokenKind::Newline) {
            continue;
        }
        out.push(token.clone());
    }
    // A trailing significant newline carries no statement after it.
    if out.last().map(|t| &t.kind) == Some(&TokenKind::Newline) {
        out.pop();
    }
    out
}
