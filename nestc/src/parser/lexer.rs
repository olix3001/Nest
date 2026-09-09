//! Lexer for the Nest language.
//!
//! Tokenization is eager: Calling [`LogosLexer::new`] runs the whole
//! source through [`logos`] and stores the resulting `(kind, span)` pairs in a [`Vec`].
//! This is to avoid the source lifetime that a lazy
//! [`logos::Lexer<'s, _>`] would otherwise force onto the struct.

use logos::{FilterResult, Logos};

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
    #[regex(r"\$?[_\p{L}][_\p{L}0-9]*", |lex| Symbol::new(lex.slice()))]
    Ident(Symbol),

    #[regex(r"[0-9][0-9_]*", lex_int)]
    #[regex(r"0[xX][0-9a-fA-F_]+", lex_int)]
    #[regex(r"0[oO][0-7_]+", lex_int)]
    #[regex(r"0[bB][01_]+", lex_int)]
    Int(i128),

    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?", lex_float)]
    #[regex(r"[0-9][0-9_]*[eE][+-]?[0-9_]+", lex_float)]
    Float(FloatLit),

    #[token("\"", lex_string)]
    Str(String),

    #[token("'", lex_char)]
    Char(char),

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
    #[error("invalid numeric literal")]
    InvalidNumber,
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
            tokens.push(match result {
                Ok(kind) => Ok(Token::new(kind, span)),
                Err(kind) => Err(LexError { kind, span }),
            });
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
fn lex_int(lex: &mut logos::Lexer<TokenKind>) -> Result<i128, LexErrorKind> {
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
    i128::from_str_radix(&cleaned, radix).map_err(|_| LexErrorKind::InvalidNumber)
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
                TokenKind::Int(123),
                TokenKind::Int(1000),
                TokenKind::Int(255),
                TokenKind::Int(0o17),
                TokenKind::Int(0b1010),
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
    fn keywords_idents_intrinsics() {
        // `self` / `Self` are ordinary identifiers, not keywords.
        assert_eq!(
            kinds("func foo self Self $cast _x"),
            vec![
                TokenKind::FuncKw,
                TokenKind::Ident(Symbol::new("foo")),
                TokenKind::Ident(Symbol::new("self")),
                TokenKind::Ident(Symbol::new("Self")),
                TokenKind::Ident(Symbol::new("$cast")),
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
