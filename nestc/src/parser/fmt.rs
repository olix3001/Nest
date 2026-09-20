//! The format specifier on an interpolation hole — `{x:?}`, `{s:>8}`, `{n:#x}`
//! (§1.5, §6.11).
//!
//! What a specifier produces is a **compile-time** description and nothing else.
//! There is no formatting object at run time and no field on the buffer holding
//! a width: the specifier decides *which call* desugaring writes for the hole —
//! `{n}` is `display`, `{n:?}` is `debug`, `{n:x}` is the lower-hex trait — and
//! width, fill and alignment become calls the desugaring adds either side of it.
//! A program with no specifier anywhere pays for none of this.
//!
//! The grammar is Rust's, minus the parts that need a run-time value:
//!
//! ```text
//! spec = [[fill] align] ['+'] ['#'] ['0'] [width] ['.' precision] [type]
//! align = '<' | '^' | '>'
//! type  = '?' | 'x' | 'X' | 'b' | 'o'
//! ```
//!
//! `width` and `precision` are decimal literals. Rust's `{x:>w$}`, which names a
//! binding for the width, is deliberately absent: every number here is known
//! while the string is being lexed, which is what keeps the whole feature a
//! desugaring.

use super::lexer::LexErrorKind;

/// Which end of the field the value sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// `<`
    Left,
    /// `^`
    Center,
    /// `>`
    Right,
}

impl Align {
    /// The number the desugaring hands `core`'s padding helper.
    ///
    /// A `core` enum would read better at the call, but desugaring would then
    /// have to name a `core` type by path — which is the one thing an `f"..."`
    /// does not do (§6.11: every name it reaches is a `#lang` tag).
    pub fn code(self) -> u8 {
        match self {
            Align::Left => 0,
            Align::Center => 1,
            Align::Right => 2,
        }
    }
}

/// What the value is written **as**: the type character at the end of a
/// specifier, and so which trait the hole calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpecKind {
    /// No type character — `#lang("display")`.
    #[default]
    Display,
    /// `?` — `#lang("debug")`.
    Debug,
    /// `x` — `#lang("lower_hex")`.
    LowerHex,
    /// `X` — `#lang("upper_hex")`.
    UpperHex,
    /// `b` — `#lang("binary")`.
    Binary,
    /// `o` — `#lang("octal")`.
    Octal,
}

impl SpecKind {
    /// The prefix `#` writes in front of the digits, where the kind has one.
    pub fn alternate_prefix(self) -> Option<&'static str> {
        match self {
            SpecKind::LowerHex => Some("0x"),
            SpecKind::UpperHex => Some("0x"),
            SpecKind::Binary => Some("0b"),
            SpecKind::Octal => Some("0o"),
            SpecKind::Display | SpecKind::Debug => None,
        }
    }
}

/// One hole's specifier, as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatSpec {
    /// The character a too-narrow value is padded with. `' '` unless written.
    pub fill: char,
    /// Which end the value sits at, when it is padded. Absent means left —
    /// see [`FormatSpec::alignment`].
    pub align: Option<Align>,
    /// `+`: write a `+` in front of a value that has no sign of its own.
    pub plus: bool,
    /// `#`: write the radix prefix.
    pub alternate: bool,
    /// `0`: pad with zeroes, *after* the sign rather than in front of it.
    pub zero: bool,
    pub width: Option<u32>,
    pub precision: Option<u32>,
    pub kind: SpecKind,
}

impl Default for FormatSpec {
    fn default() -> Self {
        Self {
            fill: ' ',
            align: None,
            plus: false,
            alternate: false,
            zero: false,
            width: None,
            precision: None,
            kind: SpecKind::Display,
        }
    }
}

impl FormatSpec {
    /// Whether the hole needs anything written around the value's own call.
    pub fn wraps(&self) -> bool {
        self.width.is_some() || self.plus
    }

    /// Where the value sits in its field.
    ///
    /// Unwritten alignment is **left** for every type, and zero-padding is
    /// right — which is where Rust and this part company. Rust's default is
    /// left for text and right for numbers, and it can tell the two apart
    /// because the choice is made inside each `Display` impl, at run time.
    /// Here the choice is made while lexing, where there are no types yet, so
    /// one rule has to cover both and the one that does not depend on knowing
    /// is the one to take.
    pub fn alignment(&self) -> Align {
        match self.align {
            Some(a) => a,
            None if self.zero => Align::Right,
            None => Align::Left,
        }
    }

    /// The character padding is made of: `0` when the zero flag is on and no
    /// fill was written, and the fill otherwise.
    pub fn padding(&self) -> char {
        if self.zero && self.align.is_none() {
            '0'
        } else {
            self.fill
        }
    }
}

/// Read the text between a hole's `:` and its `}`.
///
/// An empty specifier (`{x:}`) is the default one: the colon is allowed to be
/// there and say nothing, which keeps a program that is in the middle of being
/// written from failing to lex.
pub fn parse(text: &str) -> Result<FormatSpec, LexErrorKind> {
    let chars: Vec<char> = text.chars().collect();
    let mut spec = FormatSpec::default();
    let mut i = 0;

    // `[[fill] align]`. The fill is read only when an alignment follows it,
    // which is what makes `{x:<8}` an alignment and `{x:x}` a type: one
    // character is never a fill.
    if chars.len() > 1 && align_of(chars[1]).is_some() {
        spec.fill = chars[0];
        spec.align = align_of(chars[1]);
        i = 2;
    } else if let Some(align) = chars.first().copied().and_then(align_of) {
        spec.align = Some(align);
        i = 1;
    }

    if chars.get(i) == Some(&'+') {
        spec.plus = true;
        i += 1;
    }
    if chars.get(i) == Some(&'#') {
        spec.alternate = true;
        i += 1;
    }
    // A leading zero is the flag, not the first digit of the width: a field of
    // width zero is a field nothing is ever padded into, so there is nothing to
    // read it as instead.
    if chars.get(i) == Some(&'0') {
        spec.zero = true;
        i += 1;
    }

    let (width, next) = number(&chars, i);
    spec.width = width;
    i = next;

    if chars.get(i) == Some(&'.') {
        let (precision, next) = number(&chars, i + 1);
        // `.` with no digits after it says nothing and is almost certainly a
        // typed-too-early `{x:.}`.
        spec.precision = Some(precision.ok_or(LexErrorKind::MalformedFormatSpec)?);
        i = next;
    }

    if let Some(&c) = chars.get(i) {
        spec.kind = match c {
            '?' => SpecKind::Debug,
            'x' => SpecKind::LowerHex,
            'X' => SpecKind::UpperHex,
            'b' => SpecKind::Binary,
            'o' => SpecKind::Octal,
            _ => return Err(LexErrorKind::UnknownFormatType(c)),
        };
        i += 1;
    }

    // Anything left is not part of the grammar, and naming the character it
    // stopped at is more use than naming the whole specifier.
    match chars.get(i) {
        None => Ok(spec),
        Some(&c) => Err(LexErrorKind::UnknownFormatType(c)),
    }
}

fn align_of(c: char) -> Option<Align> {
    match c {
        '<' => Some(Align::Left),
        '^' => Some(Align::Center),
        '>' => Some(Align::Right),
        _ => None,
    }
}

/// The decimal number at `i`, and where it ended.
fn number(chars: &[char], mut i: usize) -> (Option<u32>, usize) {
    let start = i;
    let mut value: u32 = 0;
    while let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
        // A width no buffer could hold is a mistake, and saturating keeps the
        // parse from wrapping into a small one.
        value = value.saturating_mul(10).saturating_add(d);
        i += 1;
    }
    if i == start {
        (None, i)
    } else {
        (Some(value), i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty specifier is the default one: `{x:}` says nothing, and a hole
    /// half-written is not a reason to fail to lex.
    #[test]
    fn an_empty_specifier_is_the_default_one() {
        assert_eq!(parse("").expect("empty"), FormatSpec::default());
    }

    /// The type character decides which trait the hole calls, and it is the only
    /// thing a one-character specifier can be — `{x:x}` is hexadecimal and not a
    /// fill of `x`.
    #[test]
    fn a_type_character_is_read_as_one() {
        for (text, kind) in [
            ("?", SpecKind::Debug),
            ("x", SpecKind::LowerHex),
            ("X", SpecKind::UpperHex),
            ("b", SpecKind::Binary),
            ("o", SpecKind::Octal),
        ] {
            let spec = parse(text).unwrap_or_else(|e| panic!("`{text}`: {e}"));
            assert_eq!(spec.kind, kind, "`{text}`");
            assert_eq!(spec.fill, ' ', "`{text}` read a fill");
        }
    }

    /// A fill is read only when an alignment follows it, so the one-character
    /// forms stay what they look like and `{s:x>4}` is still padding with `x`.
    #[test]
    fn a_fill_needs_an_alignment_after_it() {
        let spec = parse("x>4").expect("fill and align");
        assert_eq!(spec.fill, 'x');
        assert_eq!(spec.align, Some(Align::Right));
        assert_eq!(spec.width, Some(4));
        assert_eq!(spec.kind, SpecKind::Display);

        let bare = parse("<4").expect("align alone");
        assert_eq!(bare.fill, ' ');
        assert_eq!(bare.align, Some(Align::Left));
    }

    /// Every flag at once, in the order the grammar gives them, each landing
    /// where it belongs.
    #[test]
    fn the_flags_are_read_in_order() {
        let spec = parse("+#08.3x").expect("all of them");
        assert!(spec.plus && spec.alternate && spec.zero);
        assert_eq!(spec.width, Some(8));
        assert_eq!(spec.precision, Some(3));
        assert_eq!(spec.kind, SpecKind::LowerHex);
        // Zero-padding is right-aligned and made of zeroes, without either
        // being written.
        assert_eq!(spec.alignment(), Align::Right);
        assert_eq!(spec.padding(), '0');
    }

    /// A written fill beats the zero flag: `{n:x<08}` pads with `x`, because the
    /// fill is the more specific of the two and both were asked for.
    #[test]
    fn a_written_fill_beats_the_zero_flag() {
        let spec = parse("x<08").expect("both");
        assert!(spec.zero);
        assert_eq!(spec.padding(), 'x');
        assert_eq!(spec.alignment(), Align::Left);
    }

    /// The leading `0` is the flag rather than the width's first digit, and the
    /// digits after it are the width.
    #[test]
    fn a_leading_zero_is_the_flag() {
        let spec = parse("06").expect("zero and width");
        assert!(spec.zero);
        assert_eq!(spec.width, Some(6));
    }

    /// What is not in the grammar is refused, and the character it stopped at is
    /// what the message names.
    #[test]
    fn a_character_the_grammar_has_no_place_for_is_refused() {
        assert_eq!(parse("q"), Err(LexErrorKind::UnknownFormatType('q')));
        assert_eq!(parse("8q"), Err(LexErrorKind::UnknownFormatType('q')));
        // A type character is the last thing a specifier may hold.
        assert_eq!(parse("?8"), Err(LexErrorKind::UnknownFormatType('8')));
        // A `.` with no digits after it is a precision that says nothing.
        assert_eq!(parse("."), Err(LexErrorKind::MalformedFormatSpec));
    }

    /// Unwritten alignment is left, whatever the value turns out to be — the
    /// choice is made here, where there are no types yet.
    #[test]
    fn unwritten_alignment_is_left() {
        assert_eq!(parse("8").expect("width").alignment(), Align::Left);
    }
}
