//! Lexer for the Nest language.
//!
//! Tokenization is eager: Calling [`LogosLexer::new`] runs the whole
//! source through [`logos`] and stores the resulting `(kind, span)` pairs in a [`Vec`].
//! This is to avoid the source lifetime that a lazy
//! [`logos::Lexer<'s, _>`] would otherwise force onto the struct.

use logos::{FilterResult, Logos};
use num_bigint::BigInt;

use super::fmt::{self, FormatSpec};
use crate::common::span::Span;
use crate::common::symbol::Symbol;

/// The lexical category of a token.
/// Can carry a decoded literal payload.
#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(error = LexErrorKind)]
#[logos(skip r"[ \t\f\r]+")]
#[logos(skip("//[^\n]*", allow_greedy = true))]
#[rustfmt::skip]
pub enum TokenKind {
    /// Newline character. Multiple consecutive newlines are still represented
    /// by a single `Newline` token. This works similar to a ; but can be ignored
    /// if the line ends on `+`, `::`, or other stuff that makes it incomplete.
    #[regex(r"(\r?\n)+")]
    Newline,

    /// `/* ... */`, nesting. Never emitted — matching it skips the comment; an
    /// unterminated one yields [`LexErrorKind::UnterminatedBlockComment`].
    #[token("/*", block_comment)]
    BlockComment,

    // ===< Identifiers and literals >===
    #[regex(r"[_\p{L}][_\p{L}0-9]*", |lex| Symbol::new(lex.slice()))]
    Ident(Symbol),

    #[regex(r"[0-9][0-9_]*", lex_int)]
    #[regex(r"0[xX][0-9a-fA-F_]+", lex_int)]
    #[regex(r"0[oO][0-7_]+", lex_int)]
    #[regex(r"0[bB][01_]+", lex_int)]
    Int(BigInt),

    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?", lex_float)]
    #[regex(r"[0-9][0-9_]*[eE][+-]?[0-9_]+", lex_float)]
    Float(FloatLit),

    #[token("\"", lex_string)]
    Str(String),

    /// `b"..."` — a **byte** string: the bytes as written, with no UTF-8
    /// promise, so `\xNN` may name any octet (§1.5).
    ///
    /// The two-character opener is what keeps this apart from the identifier
    /// `b` followed by a string: the longer match wins, so `b"hi"` is one token
    /// and `b "hi"` is still two.
    #[token("b\"", lex_byte_string)]
    Bytes(Vec<u8>),

    /// `c"..."` — a **C** string: the bytes as written plus a trailing NUL,
    /// which is the whole of what makes it one (§11). The NUL is added where
    /// the literal is desugared, so what this holds is the text.
    ///
    /// It lexes exactly as a `"..."` does — same escapes, same UTF-8 — and is
    /// kept apart from the identifier `c` by the two-character opener, as
    /// `b"..."` and `f"..."` are.
    #[token("c\"", lex_string)]
    CStr(String),

    #[token("'", lex_char)]
    Char(char),

    /// `f"...{e}..."` — an interpolated string, whole (§1.5, §6.11).
    ///
    /// **Never reaches the parser.** [`LogosLexer::new`] expands one of these
    /// into the five tokens below, because a `logos` callback yields one token
    /// and an interpolated string is a *sequence*: an opener, alternating
    /// literal segments and embedded expressions, and a closer. The parser then
    /// reads the embedded expressions with the ordinary expression parser
    /// rather than with a second one written for the inside of a string.
    ///
    /// The two-character opener is what keeps this apart from the identifier
    /// `f` followed by a string, exactly as `b"..."` is kept apart.
    #[token("f\"", lex_interp_string)]
    InterpStr(Vec<InterpPiece>),

    /// The `f"` that opens an interpolated string. Synthesized.
    InterpStart,
    /// The `{` that opens an embedded expression. Synthesized.
    InterpOpen,
    /// The `}` that closes one. Synthesized.
    InterpClose,
    /// The `:spec` at the end of an embedded expression — `{x:>8}`, `{x:?}`
    /// (§6.11). Synthesized, and present only where one was written.
    InterpSpec(FormatSpec),
    /// The `"` that closes an interpolated string. Synthesized.
    InterpEnd,

    // ===< Keywords >===
    #[token("func")]      FuncKw,
    #[token("extern")]    ExternKw,
    #[token("struct")]    StructKw,
    #[token("enum")]      EnumKw,
    #[token("trait")]     TraitKw,
    #[token("impl")]      ImplKw,
    #[token("namespace")] NamespaceKw,
    #[token("distinct")]  DistinctKw,
    #[token("let")]       LetKw,
    #[token("const")]     ConstKw,
    #[token("mut")]       MutKw,
    #[token("return")]    ReturnKw,
    #[token("defer")]     DeferKw,
    #[token("match")]     MatchKw,
    #[token("import")]    ImportKw,
    #[token("if")]        IfKw,
    #[token("else")]      ElseKw,
    #[token("for")]       ForKw,
    #[token("while")]     WhileKw,
    #[token("loop")]      LoopKw,
    #[token("break")]     BreakKw,
    #[token("continue")]  ContinueKw,
    #[token("dyn")]       DynKw,
    #[token("true")]      TrueKw,
    #[token("false")]     FalseKw,
    // `self` / `Self` are intentionally NOT keywords: `self` is an ordinary
    // parameter binding (the receiver) and `Self` is an ordinary name resolved to
    // the implementing type inside a `trait`/`impl`. They lex as identifiers;
    // name resolution reserves them.
    #[token("and")]       AndKw,
    #[token("or")]        OrKw,
    #[token("not")]       NotKw,

    // ===< Colons >===
    #[token("::")]
    ColonColon,
    #[token(":=")]
    ColonEq,
    #[token(":")]
    Colon,

    // ===< Dots >===
    #[token(".")]   Dot,
    #[token("..")]  DotDot,
    #[token("..<")] DotDotLt,
    #[token("..=")] DotDotEq,
    #[token(".<")]  DotLt,
    #[token(".{")]  DotLBrace,
    #[token(".*")]  DotStar,
    #[token(".?")]  DotQuestion,
    #[token(".!")]  DotBang,

    // ===< Arrows >===
    #[token("->")] Arrow,
    #[token("=>")] FatArrow,

    // ===< Arithmetic and assignment >===
    #[token("+")]  Plus,
    #[token("-")]  Minus,
    #[token("*")]  Star,
    #[token("/")]  Slash,
    #[token("%")]  Percent,
    #[token("=")]  Eq,
    #[token("+=")] PlusEq,
    #[token("-=")] MinusEq,
    #[token("*=")] StarEq,
    #[token("/=")] SlashEq,
    #[token("%=")] PercentEq,

    // ===< Comparison >===
    #[token("==")] EqEq,
    #[token("!=")] BangEq,
    #[token("<")]  Lt,
    #[token("<=")] LtEq,
    #[token(">")]  Gt,
    #[token(">=")] GtEq,

    // ===< Logical >===
    #[token("&&")] AmpAmp,
    #[token("||")] PipePipe,
    #[token("!")]  Bang,

    // ===< Bitwise >===
    #[token("&")]  Amp,
    #[token("|")]  Pipe,
    #[token("^")]  Caret,
    #[token("~")]  Tilde,
    #[token("<<")] Shl,
    #[token(">>")] Shr,

    // ===< Sigils >===
    #[token("@")] At,
    #[token("#")] Hash,

    // ===< Delimiters and punctuation >===
    #[token("(")] LParen,
    #[token(")")] RParen,
    #[token("{")] LBrace,
    #[token("}")] RBrace,
    #[token("[")] LBracket,
    #[token("]")] RBracket,
    #[token(",")] Comma,
    #[token(";")] Semicolon,
}

/// Append `entry`, unfolding an [`TokenKind::InterpStr`] into the sequence the
/// parser reads.
///
/// The literal segments come through as ordinary [`TokenKind::Str`] tokens —
/// a segment *is* a string literal, and giving it a kind of its own would be a
/// second spelling of one thing. The four markers around them are what the
/// parser matches on, and they are distinct from `{` and `}` on purpose:
/// reusing the real braces would make the end of an interpolation and the end of
/// a struct literal the same token, so recovering from a malformed one would
/// have to guess.
///
/// Recursive, because a nested `f"..."` inside an embedded expression arrives as
/// another `InterpStr`.
fn push_expanded(tokens: &mut Vec<LexResult<Token>>, entry: LexResult<Token>) {
    let Ok(Token {
        kind: TokenKind::InterpStr(pieces),
        span,
    }) = entry
    else {
        tokens.push(entry);
        return;
    };
    tokens.push(Ok(Token::new(
        TokenKind::InterpStart,
        Span::new(span.start, span.start + 2),
    )));
    for piece in pieces {
        match piece {
            InterpPiece::Lit(text, at) => tokens.push(Ok(Token::new(TokenKind::Str(text), at))),
            InterpPiece::Expr(sub, spec, at) => {
                tokens.push(Ok(Token::new(
                    TokenKind::InterpOpen,
                    Span::new(at.start, at.start + 1),
                )));
                for token in sub {
                    push_expanded(tokens, token);
                }
                // After the expression's tokens, because that is where it was
                // written and because the parser has then already read the
                // expression it belongs to.
                if let Some((spec, at)) = spec {
                    tokens.push(Ok(Token::new(TokenKind::InterpSpec(spec), at)));
                }
                tokens.push(Ok(Token::new(
                    TokenKind::InterpClose,
                    Span::new(at.end - 1, at.end),
                )));
            }
        }
    }
    tokens.push(Ok(Token::new(
        TokenKind::InterpEnd,
        Span::new(span.end - 1, span.end),
    )));
}

/// One piece of an `f"..."` literal, as the lexer found it.
///
/// A literal chunk arrives decoded — escapes resolved, `{{` and `}}` reduced to
/// one brace — so nothing downstream decodes a string twice. An embedded
/// expression arrives as **tokens**, lexed with the enclosing file's own
/// offsets, so every span inside `f"{a + b}"` points where a reader would point.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpPiece {
    /// A run of literal text, and the source it came from.
    Lit(String, Span),
    /// The tokens of one `{ expr }`, the specifier written after its `:` (with
    /// the span of the specifier's own text), and the span of the braces around
    /// the whole hole.
    Expr(Vec<LexResult<Token>>, Option<(FormatSpec, Span)>, Span),
}

/// A lexed token: a [`TokenKind`] paired with its source [`Span`].
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// The reason a chunk of source failed to lex.
/// Position information lives on [`LexError`],
/// which pairs one of these with a [`Span`].
#[derive(Debug, Clone, Default, PartialEq, Eq, thiserror::Error)]
pub enum LexErrorKind {
    #[default]
    #[error("unexpected character")]
    UnexpectedCharacter,
    #[error("unterminated string literal")]
    UnterminatedString,
    #[error("unterminated block comment")]
    UnterminatedBlockComment,
    #[error("unterminated character literal")]
    UnterminatedChar,
    #[error("a character literal must contain exactly one character")]
    MalformedChar,
    #[error("invalid escape sequence")]
    InvalidEscape,
    #[error("invalid unicode escape")]
    InvalidUnicodeEscape,
    #[error("a byte-string escape must name one byte: `\\u{{...}}` is not one")]
    UnicodeEscapeInByteString,
    #[error("a byte string may only contain ASCII; use `\\xNN` for other bytes")]
    NonAsciiByteString,
    #[error("invalid `\\xNN` escape")]
    InvalidByteEscape,
    #[error("invalid numeric literal")]
    InvalidNumber,
    #[error("`{{}}` interpolates nothing; write `{{{{}}}}` for a literal pair of braces")]
    EmptyInterpolation,
    #[error("an unmatched `}}` in an interpolated string; write `}}}}` for a literal one")]
    UnmatchedInterpolation,
    #[error(
        "`{0}` is not a format specifier; write `{{value:[[fill]align][+][#][0][width][.precision][type]}}`, \
         where `align` is `<`, `^` or `>` and `type` is `?`, `x`, `X`, `b` or `o`"
    )]
    UnknownFormatType(char),
    #[error("a malformed format specifier")]
    MalformedFormatSpec,
}

/// Lexing error with [`Span`] included.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} at {}..{}", span.start, span.end)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub span: Span,
}

/// A result whose error is a spanned [`LexError`].
pub type LexResult<T> = Result<T, LexError>;

/// A cursor over a lexed source file.
/// A conenient way to move around in a parser.
pub trait Lexer {
    /// Consume and return the next entry, or `None` at end of input.
    fn next_token(&mut self) -> LexResult<Option<Token>>;

    /// Borrow the next entry without consuming it.
    fn peek(&self) -> LexResult<Option<&Token>>;

    /// Borrow the entry `n` positions ahead (`0` == [`Lexer::peek`]).
    fn peek_nth(&self, n: usize) -> LexResult<Option<&Token>>;

    /// Whether every entry has been consumed.
    fn is_eof(&self) -> bool;

    /// Borrow the kind of the next token, if the next entry is an `Ok` token.
    fn peek_kind(&self) -> LexResult<Option<&TokenKind>> {
        match self.peek()? {
            Some(token) => Ok(Some(&token.kind)),
            _ => Ok(None),
        }
    }

    /// Consume and return the next token only if its kind equals `kind`.
    /// Returns `Ok(None)` (without advancing) when the next token differs or the
    /// stream is at end of input. A pending lex error is propagated.
    fn eat(&mut self, kind: &TokenKind) -> LexResult<Option<Token>> {
        if self.peek_kind()? == Some(kind) {
            self.next_token()
        } else {
            Ok(None)
        }
    }

    /// Consume runs of [`TokenKind::Newline`]. Handy at points where blank lines
    /// carry no meaning.
    fn skip_newlines(&mut self) -> LexResult<()> {
        while matches!(self.peek_kind()?, Some(TokenKind::Newline)) {
            self.next_token()?;
        }
        Ok(())
    }
}

/// The default [`Lexer`], backed by an eager token vector.
///
/// Carries no lifetime: [`LogosLexer::new`] borrows the source only for the
/// duration of tokenization and stores owned results.
#[derive(Debug, Clone)]
pub struct LogosLexer {
    tokens: Vec<LexResult<Token>>,
    pos: usize,
}

impl LogosLexer {
    /// Tokenize `source` in full.
    pub fn new(source: &str) -> Self {
        let mut lexer = TokenKind::lexer(source);
        let mut tokens = Vec::new();

        while let Some(result) = lexer.next() {
            let range = lexer.span();
            let span = Span::new(range.start, range.end);
            push_expanded(
                &mut tokens,
                match result {
                    Ok(kind) => Ok(Token::new(kind, span)),
                    Err(kind) => Err(LexError { kind, span }),
                },
            );
        }

        Self { tokens, pos: 0 }
    }

    /// The current cursor position (index of the next entry).
    pub fn position(&self) -> usize {
        self.pos
    }

    /// All entries, for callers that want the whole vector at once.
    pub fn as_slice(&self) -> &[LexResult<Token>] {
        &self.tokens
    }
}

impl Lexer for LogosLexer {
    fn next_token(&mut self) -> LexResult<Option<Token>> {
        match self.tokens.get(self.pos) {
            None => Ok(None),
            Some(entry) => {
                self.pos += 1;
                entry.clone().map(Some)
            }
        }
    }

    fn peek(&self) -> LexResult<Option<&Token>> {
        self.peek_nth(0)
    }

    fn peek_nth(&self, n: usize) -> LexResult<Option<&Token>> {
        match self.tokens.get(self.pos + n) {
            None => Ok(None),
            Some(Ok(token)) => Ok(Some(token)),
            Some(Err(err)) => Err(err.clone()),
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }
}

// ============================================================================
// Custom logos callbacks.
// ============================================================================

/// Skip a `/* ... */` block comment, honoring nesting.
fn block_comment(lex: &mut logos::Lexer<TokenKind>) -> FilterResult<(), LexErrorKind> {
    let bytes = lex.remainder().as_bytes();
    let mut depth = 1usize;
    let mut i = 0;

    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    lex.bump(i);
                    return FilterResult::Skip;
                }
            }
            _ => i += 1,
        }
    }

    lex.bump(bytes.len());
    FilterResult::Error(LexErrorKind::UnterminatedBlockComment)
}

/// Parse an integer literal in any base, ignoring `_` digit separators.
///
/// The result is arbitrary-precision: an integer literal is a `comptime_int`
/// and must keep its exact value until something casts it to a runtime width.
fn lex_int(lex: &mut logos::Lexer<TokenKind>) -> Result<BigInt, LexErrorKind> {
    let slice = lex.slice();
    let (digits, radix) = if let Some(rest) = strip_prefix_ci(slice, "0x") {
        (rest, 16)
    } else if let Some(rest) = strip_prefix_ci(slice, "0o") {
        (rest, 8)
    } else if let Some(rest) = strip_prefix_ci(slice, "0b") {
        (rest, 2)
    } else {
        (slice, 10)
    };

    let cleaned: String = digits.chars().filter(|&c| c != '_').collect();
    BigInt::parse_bytes(cleaned.as_bytes(), radix).ok_or(LexErrorKind::InvalidNumber)
}

/// A lexed float literal: its `f64` value plus whether the source text asked for
/// more than an `f64` can hold.
///
/// A float literal is `comptime_float` — conceptually `f128` — and collapses to
/// `f64` when nothing in its use pins a width. [`wide`](FloatLit::wide) marks the
/// literals for which that collapse would lose the value, so inference can
/// reject them unless the use site really is an `f80` / `f128`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatLit {
    /// The value as an `f64` — infinite when the text overflows the format.
    pub value: f64,
    /// The text does not survive the `comptime_float` → `f64` collapse.
    pub wide: bool,
}

/// Parse a floating-point literal, ignoring `_` digit separators.
fn lex_float(lex: &mut logos::Lexer<TokenKind>) -> Result<FloatLit, LexErrorKind> {
    let cleaned: String = lex.slice().chars().filter(|&c| c != '_').collect();
    let value = cleaned
        .parse::<f64>()
        .map_err(|_| LexErrorKind::InvalidNumber)?;
    Ok(FloatLit {
        value,
        wide: exceeds_f64(&cleaned, value),
    })
}

/// Whether a float literal's text carries more than an `f64` can represent: it
/// overflows to infinity, flushes a nonzero value to zero, or names more
/// significant decimal digits than `f64`'s 17-digit round-trip budget.
///
/// `text` has already had its `_` separators removed.
fn exceeds_f64(text: &str, value: f64) -> bool {
    if !value.is_finite() {
        return true;
    }
    // Only the mantissa's digits matter; the exponent is already accounted for
    // by the overflow / flush-to-zero checks above and below.
    let mantissa = text.split(['e', 'E']).next().unwrap_or(text);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    // Leading zeros are placeholders, and trailing zeros in the text add no
    // information the value does not already have.
    let significant = digits.trim_start_matches('0').trim_end_matches('0').len();
    if value == 0.0 {
        return significant != 0;
    }
    significant > F64_ROUND_TRIP_DIGITS
}

/// The decimal digits an `f64` round-trips (`f64::DIGITS` is the *guaranteed*
/// 15; 17 is the number that always recovers the same bit pattern).
const F64_ROUND_TRIP_DIGITS: usize = 17;

/// Decode the body of a `"..."` string, resolving escapes. Called with the
/// cursor positioned just past the opening quote.
fn lex_string(lex: &mut logos::Lexer<TokenKind>) -> Result<String, LexErrorKind> {
    let rest = lex.remainder();
    let mut chars = rest.char_indices();
    let mut out = String::new();

    while let Some((idx, c)) = chars.next() {
        match c {
            '"' => {
                lex.bump(idx + 1);
                return Ok(out);
            }
            '\n' => return Err(LexErrorKind::UnterminatedString),
            '\\' => out.push(unescape(&mut chars)?),
            _ => out.push(c),
        }
    }

    Err(LexErrorKind::UnterminatedString)
}

/// Decode the body of an `f"..."` interpolated string (§1.5, §6.11). Called with
/// the cursor just past the opening `f"`.
///
/// Outside the braces this is `lex_string` with two more cases: `{{` and `}}`
/// stand for one brace each, and a lone `{` opens an embedded expression. A lone
/// `}` is an error rather than a literal brace, because a program that meant one
/// and typed one would otherwise get it — and the same program with a `{` added
/// later would change meaning silently.
///
/// **Inside** the braces the lexer calls itself rather than scanning for the
/// matching `}`. Scanning would have to know that the `}` in `f"{ g("}") }"` is
/// inside a string and that the `{` in `f"{ P { x: 1 } }"` opens a struct
/// literal — which is to say it would have to be a lexer, so it may as well be
/// this one. Depth counting over real tokens gets both right for free, and a
/// nested `f"..."` comes back as an [`TokenKind::InterpStr`] that the expansion
/// below unfolds like any other.
fn lex_interp_string(lex: &mut logos::Lexer<TokenKind>) -> Result<Vec<InterpPiece>, LexErrorKind> {
    // Where `rest` sits in the file, so every span below is the file's own.
    let base = lex.span().end;
    let rest = lex.remainder().to_string();
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut lit_start = 0usize;
    let mut i = 0usize;

    // A literal run becomes a piece only when it is non-empty: `f"{a}{b}"` has
    // no text in it, and a segment holding none would be a store of nothing.
    macro_rules! flush {
        ($end:expr) => {
            if !lit.is_empty() {
                pieces.push(InterpPiece::Lit(
                    std::mem::take(&mut lit),
                    Span::new(base + lit_start, base + $end),
                ));
            }
        };
    }

    while i < rest.len() {
        let c = rest[i..]
            .chars()
            .next()
            .expect("in bounds and on a boundary");
        match c {
            '"' => {
                flush!(i);
                lex.bump(i + 1);
                return Ok(pieces);
            }
            '\n' => return Err(LexErrorKind::UnterminatedString),
            '\\' => {
                let mut chars = rest[i + 1..].char_indices();
                lit.push(unescape(&mut chars)?);
                i = rest.len() - chars.as_str().len();
            }
            '{' if rest[i..].starts_with("{{") => {
                lit.push('{');
                i += 2;
            }
            '}' if rest[i..].starts_with("}}") => {
                lit.push('}');
                i += 2;
            }
            '}' => return Err(LexErrorKind::UnmatchedInterpolation),
            '{' => {
                flush!(i);
                let (tokens, spec, end) = lex_interp_expr(&rest, i + 1, base)?;
                pieces.push(InterpPiece::Expr(
                    tokens,
                    spec,
                    Span::new(base + i, base + end + 1),
                ));
                i = end + 1;
                lit_start = i;
            }
            _ => {
                lit.push(c);
                i += c.len_utf8();
            }
        }
    }

    Err(LexErrorKind::UnterminatedString)
}

/// The tokens of one `{ expr }`, the specifier after its `:` if it has one, and
/// the offset of the `}` that closed it.
///
/// `base` is where `rest` sits in the file; every span handed back is absolute.
///
/// A `:` at **depth zero** ends the expression and begins the specifier, which
/// is the same rule Rust's holes have and is unambiguous for the same reason:
/// every colon an expression can contain is inside something — a struct
/// literal's braces, a `match`'s arms — and so is at a depth above zero. The
/// specifier itself is not lexed as tokens: `>8`, `#x` and `.3` are not Nest
/// expressions, and reading them as characters is what [`super::fmt`] does.
fn lex_interp_expr(
    rest: &str,
    start: usize,
    base: usize,
) -> Result<(Vec<LexResult<Token>>, Option<(FormatSpec, Span)>, usize), LexErrorKind> {
    let mut sub = TokenKind::lexer(&rest[start..]);
    let mut tokens: Vec<LexResult<Token>> = Vec::new();
    let mut depth = 0usize;

    loop {
        let Some(result) = sub.next() else {
            return Err(LexErrorKind::UnterminatedString);
        };
        let at = sub.span();
        let span = Span::new(base + start + at.start, base + start + at.end);
        match result {
            // The one that ends it, and the ones that do not: a `}` closing a
            // struct literal or a block inside the expression is the expression's
            // own, and only the one at depth zero is the interpolation's.
            Ok(TokenKind::RBrace) if depth == 0 => {
                if tokens.is_empty() {
                    return Err(LexErrorKind::EmptyInterpolation);
                }
                return Ok((tokens, None, start + at.start));
            }
            // `{x:>8}`: the expression stops here and the rest of the hole is
            // the specifier.
            Ok(TokenKind::Colon) if depth == 0 => {
                if tokens.is_empty() {
                    return Err(LexErrorKind::EmptyInterpolation);
                }
                let from = start + at.end;
                // The specifier runs to the `}`. A newline or a quote before it
                // is the literal ending without one, which is the same mistake
                // an unterminated string is and reads best as that.
                let end = match rest[from..].find(['}', '\n', '"']) {
                    Some(len) if rest[from + len..].starts_with('}') => from + len,
                    _ => return Err(LexErrorKind::UnterminatedString),
                };
                let spec = fmt::parse(&rest[from..end])?;
                // The span covers the `:` as well as what follows it, so that a
                // hole written `{x:}` still has somewhere to point.
                let at = Span::new(base + start + at.start, base + end);
                return Ok((tokens, Some((spec, at)), end));
            }
            Ok(TokenKind::RBrace) => {
                depth -= 1;
                tokens.push(Ok(Token::new(TokenKind::RBrace, span)));
            }
            Ok(TokenKind::LBrace) => {
                depth += 1;
                tokens.push(Ok(Token::new(TokenKind::LBrace, span)));
            }
            // A string literal is one line, and so is what is spliced into it.
            // Stopping here names the mistake where it is; letting the newline
            // through would report an unterminated string at the end of the
            // file.
            Ok(TokenKind::Newline) => return Err(LexErrorKind::UnterminatedString),
            Ok(kind) => tokens.push(Ok(Token::new(kind, span))),
            // A bad token inside the braces travels as a bad token, so the
            // parser reports it at its own span rather than the whole literal's.
            Err(kind) => tokens.push(Err(LexError { kind, span })),
        }
    }
}

/// Decode the body of a `b"..."` byte string. Called with the cursor just past
/// the opening `b"`.
///
/// Deliberately *not* `lex_string` plus a conversion: a byte string is not text
/// that happens to be stored as bytes. `\xNN` names any octet, including ones
/// no UTF-8 sequence can produce, which is the whole reason the form exists —
/// and `\u{...}` is refused for the mirror-image reason, since a code point
/// above 127 is more than one byte and the literal would silently mean
/// something other than it says.
fn lex_byte_string(lex: &mut logos::Lexer<TokenKind>) -> Result<Vec<u8>, LexErrorKind> {
    let rest = lex.remainder();
    let mut chars = rest.char_indices();
    let mut out: Vec<u8> = Vec::new();

    while let Some((idx, c)) = chars.next() {
        match c {
            '"' => {
                lex.bump(idx + 1);
                return Ok(out);
            }
            '\n' => return Err(LexErrorKind::UnterminatedString),
            '\\' => match chars.next() {
                None => return Err(LexErrorKind::InvalidEscape),
                Some((_, 'x')) => out.push(byte_escape(&mut chars)?),
                Some((_, 'u')) => return Err(LexErrorKind::UnicodeEscapeInByteString),
                Some((_, 'n')) => out.push(b'\n'),
                Some((_, 't')) => out.push(b'\t'),
                Some((_, 'r')) => out.push(b'\r'),
                Some((_, '0')) => out.push(0),
                Some((_, '\\')) => out.push(b'\\'),
                Some((_, '"')) => out.push(b'"'),
                Some((_, '\'')) => out.push(b'\''),
                Some(_) => return Err(LexErrorKind::InvalidEscape),
            },
            c if c.is_ascii() => out.push(c as u8),
            // A non-ASCII character in the source would be several bytes, and
            // which ones depends on an encoding the literal never states.
            // Writing them out is what `\xNN` is for.
            _ => return Err(LexErrorKind::NonAsciiByteString),
        }
    }

    Err(LexErrorKind::UnterminatedString)
}

/// Resolve the two hex digits of a `\xNN` escape. The `\x` has already been
/// consumed.
fn byte_escape(chars: &mut std::str::CharIndices<'_>) -> Result<u8, LexErrorKind> {
    let mut hex = String::new();
    for _ in 0..2 {
        match chars.next() {
            Some((_, c)) => hex.push(c),
            None => return Err(LexErrorKind::InvalidByteEscape),
        }
    }
    u8::from_str_radix(&hex, 16).map_err(|_| LexErrorKind::InvalidByteEscape)
}

/// Decode a `'c'` character literal. Called with the cursor just past the
/// opening quote.
fn lex_char(lex: &mut logos::Lexer<TokenKind>) -> Result<char, LexErrorKind> {
    let rest = lex.remainder();
    let mut chars = rest.char_indices();

    let value = match chars.next() {
        None => return Err(LexErrorKind::UnterminatedChar),
        Some((_, '\'')) => return Err(LexErrorKind::MalformedChar), // empty ''
        Some((_, '\n')) => return Err(LexErrorKind::UnterminatedChar),
        Some((_, '\\')) => unescape(&mut chars)?,
        Some((_, c)) => c,
    };

    match chars.next() {
        Some((idx, '\'')) => {
            lex.bump(idx + 1);
            Ok(value)
        }
        Some(_) => Err(LexErrorKind::MalformedChar), // more than one char
        None => Err(LexErrorKind::UnterminatedChar),
    }
}

/// Resolve the escape following a `\`, consuming characters from `chars`. The
/// leading backslash has already been consumed.
fn unescape(chars: &mut std::str::CharIndices<'_>) -> Result<char, LexErrorKind> {
    let (_, esc) = chars.next().ok_or(LexErrorKind::InvalidEscape)?;
    Ok(match esc {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        'u' => unescape_unicode(chars)?,
        _ => return Err(LexErrorKind::InvalidEscape),
    })
}

/// Resolve a `\u{XXXX}` escape. The `\u` has already been consumed.
fn unescape_unicode(chars: &mut std::str::CharIndices<'_>) -> Result<char, LexErrorKind> {
    match chars.next() {
        Some((_, '{')) => {}
        _ => return Err(LexErrorKind::InvalidEscape),
    }

    let mut hex = String::new();
    loop {
        match chars.next() {
            Some((_, '}')) => break,
            Some((_, c)) => hex.push(c),
            None => return Err(LexErrorKind::InvalidUnicodeEscape),
        }
    }

    let code = u32::from_str_radix(&hex, 16).map_err(|_| LexErrorKind::InvalidUnicodeEscape)?;
    char::from_u32(code).ok_or(LexErrorKind::InvalidUnicodeEscape)
}

/// Case-insensitive `strip_prefix` for the two-char numeric base markers.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = s.as_bytes();
    let pfx = prefix.as_bytes();
    if bytes.len() >= pfx.len() && bytes[..pfx.len()].eq_ignore_ascii_case(pfx) {
        Some(&s[pfx.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        LogosLexer::new(src)
            .as_slice()
            .iter()
            .map(|r| r.as_ref().expect("lex error").kind.clone())
            .collect()
    }

    #[test]
    fn punctuation_longest_match() {
        assert_eq!(
            kinds("a.<T> b..<c ..= .* .? .! .{ :: := : -> =>"),
            vec![
                TokenKind::Ident(Symbol::new("a")),
                TokenKind::DotLt,
                TokenKind::Ident(Symbol::new("T")),
                TokenKind::Gt,
                TokenKind::Ident(Symbol::new("b")),
                TokenKind::DotDotLt,
                TokenKind::Ident(Symbol::new("c")),
                TokenKind::DotDotEq,
                TokenKind::DotStar,
                TokenKind::DotQuestion,
                TokenKind::DotBang,
                TokenKind::DotLBrace,
                TokenKind::ColonColon,
                TokenKind::ColonEq,
                TokenKind::Colon,
                TokenKind::Arrow,
                TokenKind::FatArrow,
            ]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds("123 1_000 0xFF 0o17 0b1010 3.14 1.0e-9 6.022e23"),
            vec![
                TokenKind::Int(123.into()),
                TokenKind::Int(1000.into()),
                TokenKind::Int(255.into()),
                TokenKind::Int(0o17.into()),
                TokenKind::Int(0b1010.into()),
                TokenKind::Float(narrow(3.14)),
                TokenKind::Float(narrow(1.0e-9)),
                TokenKind::Float(narrow(6.022e23)),
            ]
        );
    }

    /// A float literal that fits `f64` (the common case).
    fn narrow(value: f64) -> FloatLit {
        FloatLit { value, wide: false }
    }

    #[test]
    fn float_literals_are_flagged_when_they_outrun_f64() {
        let wide = |src: &str| match &kinds(src)[0] {
            TokenKind::Float(f) => f.wide,
            other => panic!("not a float: {other:?}"),
        };
        // Ordinary literals collapse to `f64` losslessly.
        assert!(!wide("3.14"));
        assert!(!wide("1.0e-9"));
        // Trailing and leading zeros carry no information.
        assert!(!wide("0.1000000000000000000000"));
        assert!(!wide("0.000000000000000000001"));
        // More significant digits than `f64` round-trips.
        assert!(wide("1.00000000000000000001"));
        // Beyond `f64`'s range entirely.
        assert!(wide("1.0e400"));
    }

    #[test]
    fn strings_and_chars() {
        assert_eq!(
            kinds(r#""hi\n\tthere" "quote \" done" 'a' '\n' '\u{1F600}'"#),
            vec![
                TokenKind::Str("hi\n\tthere".into()),
                TokenKind::Str("quote \" done".into()),
                TokenKind::Char('a'),
                TokenKind::Char('\n'),
                TokenKind::Char('\u{1F600}'),
            ]
        );
    }

    #[test]
    fn byte_strings() {
        assert_eq!(
            kinds(r#"b"GET " b"\x00\xff\n""#),
            vec![
                TokenKind::Bytes(b"GET ".to_vec()),
                TokenKind::Bytes(vec![0x00, 0xff, b'\n']),
            ]
        );
        // `b` on its own is still an identifier; only `b"` opens a byte string.
        assert_eq!(
            kinds(r#"b "hi""#),
            vec![
                TokenKind::Ident(Symbol::new("b")),
                TokenKind::Str("hi".into()),
            ]
        );
    }

    #[test]
    fn interpolated_strings_expand_to_a_sequence() {
        use TokenKind::*;
        assert_eq!(
            kinds(r#"f"dim: {w}x{h}""#),
            vec![
                InterpStart,
                Str("dim: ".into()),
                InterpOpen,
                Ident(Symbol::new("w")),
                InterpClose,
                Str("x".into()),
                InterpOpen,
                Ident(Symbol::new("h")),
                InterpClose,
                InterpEnd,
            ]
        );
        // A doubled brace is one literal brace, and escapes work as they do in
        // any other string. Adjacent interpolations leave no segment between
        // them, because there is no text there to store.
        assert_eq!(
            kinds(r#"f"{{a}}\n{x}{y}""#),
            vec![
                InterpStart,
                Str("{a}\n".into()),
                InterpOpen,
                Ident(Symbol::new("x")),
                InterpClose,
                InterpOpen,
                Ident(Symbol::new("y")),
                InterpClose,
                InterpEnd,
            ]
        );
        // `f` on its own is still an identifier; only `f"` opens one.
        assert_eq!(
            kinds(r#"f "hi""#),
            vec![Ident(Symbol::new("f")), Str("hi".into())]
        );
    }

    /// The braces are matched by **lexing**, not by scanning for a `}`. A
    /// closing brace inside a nested string, and one closing a struct literal,
    /// are both the expression's own.
    #[test]
    fn an_interpolation_ends_at_its_own_brace() {
        use TokenKind::*;
        assert_eq!(
            kinds(r#"f"{ g("}") }""#),
            vec![
                InterpStart,
                InterpOpen,
                Ident(Symbol::new("g")),
                LParen,
                Str("}".into()),
                RParen,
                InterpClose,
                InterpEnd,
            ]
        );
        assert_eq!(
            kinds(r#"f"{ P { x } }""#),
            vec![
                InterpStart,
                InterpOpen,
                Ident(Symbol::new("P")),
                LBrace,
                Ident(Symbol::new("x")),
                RBrace,
                InterpClose,
                InterpEnd,
            ]
        );
        // And a nested one unfolds like any other.
        assert_eq!(
            kinds(r#"f"{ f"{x}" }""#),
            vec![
                InterpStart,
                InterpOpen,
                InterpStart,
                InterpOpen,
                Ident(Symbol::new("x")),
                InterpClose,
                InterpEnd,
                InterpClose,
                InterpEnd,
            ]
        );
    }

    #[test]
    fn interpolated_strings_reject_what_has_no_meaning() {
        let err = |src: &str| {
            LogosLexer::new(src).as_slice()[0]
                .as_ref()
                .expect_err("should not lex")
                .kind
                .clone()
        };
        // A lone `}` would otherwise be a literal brace that changes meaning the
        // day someone adds a `{` before it.
        assert_eq!(err(r#"f"a}b""#), LexErrorKind::UnmatchedInterpolation);
        assert_eq!(err(r#"f"{}""#), LexErrorKind::EmptyInterpolation);
        assert_eq!(err("f\"a\nb\""), LexErrorKind::UnterminatedString);
        // A string literal is one line, and so is what is spliced into it.
        assert_eq!(err("f\"{ a +\n b }\""), LexErrorKind::UnterminatedString);
        assert_eq!(err(r#"f"{ x ""#), LexErrorKind::UnterminatedString);
    }

    /// Every span an interpolation produces is the **file's**, so a diagnostic
    /// about `{h}` points at the `h` a reader can see.
    #[test]
    fn interpolation_spans_are_the_file_s_own() {
        let src = r#"f"a{bc}""#;
        let tokens = LogosLexer::new(src);
        let at = |n: usize| {
            let t = tokens.as_slice()[n].as_ref().expect("lexes");
            &src[t.span.start..t.span.end]
        };
        assert_eq!(at(0), "f\"");
        assert_eq!(at(1), "a");
        assert_eq!(at(2), "{");
        assert_eq!(at(3), "bc");
        assert_eq!(at(4), "}");
        assert_eq!(at(5), "\"");
    }

    #[test]
    fn byte_strings_reject_what_is_not_one_byte() {
        // A code point above 127 is more than one byte, so neither the escape
        // nor the character itself may stand for it.
        let uni = LogosLexer::new(r#"b"\u{41}""#);
        assert_eq!(
            uni.as_slice()[0].as_ref().unwrap_err().kind,
            LexErrorKind::UnicodeEscapeInByteString
        );
        let raw = LogosLexer::new("b\"é\"");
        assert_eq!(
            raw.as_slice()[0].as_ref().unwrap_err().kind,
            LexErrorKind::NonAsciiByteString
        );
        let hex = LogosLexer::new(r#"b"\xZZ""#);
        assert_eq!(
            hex.as_slice()[0].as_ref().unwrap_err().kind,
            LexErrorKind::InvalidByteEscape
        );
    }

    #[test]
    fn keywords_idents_intrinsics() {
        // `self` / `Self` are ordinary identifiers, not keywords.
        assert_eq!(
            kinds("func foo self Self cast _x"),
            vec![
                TokenKind::FuncKw,
                TokenKind::Ident(Symbol::new("foo")),
                TokenKind::Ident(Symbol::new("self")),
                TokenKind::Ident(Symbol::new("Self")),
                TokenKind::Ident(Symbol::new("cast")),
                TokenKind::Ident(Symbol::new("_x")),
            ]
        );
    }

    #[test]
    fn newlines_surfaced_comments_skipped() {
        assert_eq!(
            kinds("a // trailing\n/* block /* nested */ */\nb\n\n\nc"),
            vec![
                TokenKind::Ident(Symbol::new("a")),
                TokenKind::Newline, // before the block comment
                TokenKind::Newline, // after it (trivia breaks the run)
                TokenKind::Ident(Symbol::new("b")),
                TokenKind::Newline,
                TokenKind::Ident(Symbol::new("c")),
            ]
        );
    }

    #[test]
    fn spans_are_tracked() {
        let stream = LogosLexer::new("ab cd");
        let toks: Vec<_> = stream
            .as_slice()
            .iter()
            .map(|r| r.clone().unwrap())
            .collect();
        assert_eq!(toks[0].span, Span::new(0, 2));
        assert_eq!(toks[1].span, Span::new(3, 5));
    }

    #[test]
    fn errors_carry_span() {
        let stream = LogosLexer::new("\"unterminated");
        let err = stream.as_slice()[0].as_ref().unwrap_err();
        assert_eq!(err.kind, LexErrorKind::UnterminatedString);
    }

    #[test]
    fn peek_and_next() {
        let mut stream = LogosLexer::new("x y");
        assert_eq!(
            stream.peek_kind().unwrap(),
            Some(&TokenKind::Ident(Symbol::new("x")))
        );
        assert_eq!(
            stream.peek_nth(1).unwrap().unwrap().kind,
            TokenKind::Ident(Symbol::new("y"))
        );
        stream.next_token().unwrap();
        assert_eq!(
            stream.peek_kind().unwrap(),
            Some(&TokenKind::Ident(Symbol::new("y")))
        );
        stream.next_token().unwrap();
        assert!(stream.is_eof());
    }

    #[test]
    fn eat_matches_only_expected_kind() {
        let mut stream = LogosLexer::new("( )");

        // Wrong kind: no advance, Ok(None).
        assert_eq!(stream.eat(&TokenKind::RParen).unwrap(), None);
        // Right kind: advances, returns the token.
        let lparen = stream.eat(&TokenKind::LParen).unwrap().unwrap();
        assert_eq!(lparen.kind, TokenKind::LParen);
        // Now at the RParen.
        assert!(stream.eat(&TokenKind::RParen).unwrap().is_some());
        assert!(stream.is_eof());
        // Eating at EOF is a benign miss.
        assert_eq!(stream.eat(&TokenKind::RParen).unwrap(), None);
    }
}
