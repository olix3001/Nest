//! Snapshot tests for the recursive-descent parser.
//!
//! Each test feeds a source fragment through [`Parser::parse_file`], asserts the
//! fragment parsed cleanly (unless it is an error-recovery case), and captures
//! the [`tree_to_string`] rendering with `insta`. Review a failing snapshot with
//! `cargo insta review`; the committed `.snap` files are the expected output.

use insta::assert_snapshot;

use super::ast::FileId;
use super::parse::{ParseError, Parser};
use super::pretty::tree_to_string;

/// Parse `src`, requiring it to be error-free, and render the tree.
fn tree(src: &str) -> String {
    let (ast, errors) = Parser::parse_file(src, FileId(0));
    assert!(errors.is_empty(), "unexpected parse errors: {errors:#?}");
    tree_to_string(&ast)
}

/// Parse `src` allowing errors; render the tree followed by the diagnostics.
fn tree_with_errors(src: &str) -> String {
    let (ast, errors) = Parser::parse_file(src, FileId(0));
    let mut out = tree_to_string(&ast);
    out.push_str("\n--- errors ---\n");
    for ParseError { span, message } in &errors {
        out.push_str(&format!("{}..{}: {message}\n", span.start, span.end));
    }
    out
}

#[test]
fn a_member_body_always_makes_progress() {
    // Every `{ member* }` loop used to spin forever on input it could not
    // consume: `expect_*` reports *without* consuming, so that a missing token is
    // recovered at the next item rather than by eating the one after it, and a
    // loop whose iteration consumed nothing never reaches `}`.
    //
    // `P :: struct { x: i32 := 1 }` hung the compiler — a struct field has no
    // default (§13.3), so `parse_field` stopped at the type and left `:=` where
    // it was. These parse to *something* with diagnostics, and the test is that
    // they return at all.
    for src in [
        "P :: struct { x: i32 := 1 }\n",
        "P :: struct { x: i32 := 1, y: i32 := 2 }\n",
        "E :: enum { a, := , b }\n",
        "T :: trait { := }\n",
        "P :: struct { := }\n",
    ] {
        let (_, errors) = Parser::parse_file(src, FileId(0));
        assert!(!errors.is_empty(), "expected a diagnostic for {src:?}");
    }
}

#[test]
fn a_struct_field_default_is_refused_by_name() {
    // The language has no field defaults: a struct literal never silently omits
    // a field. A type that wants filled-in values implements `Default` and a
    // literal spreads it, so the diagnostic says that rather than "unexpected".
    let (_, errors) = Parser::parse_file("P :: struct { x: i32 := 1 }\n", FileId(0));
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("a struct field has no default value")),
        "{errors:#?}"
    );
}

#[test]
fn constants_and_binding_forms() {
    assert_snapshot!(tree(
        "\
SERVER_PORT :: 8080
SERVER_ADDR :: \"127.0.0.1\"
Alias :: OtherType
CatId :: distinct string
Slice :: []mut int32
Handle :: *mut Buffer
"
    ));
}

#[test]
fn operator_precedence_and_unary() {
    assert_snapshot!(tree(
        "\
main :: func () {
  const a := 1 + 2 * 3 - 4 / 2
  const b := -x + ~y
  const c := a == b && c != d || e
  const d := 1 << 2 | 3 & 4 ^ 5
  const e := not ready or done
}
"
    ));
}

#[test]
fn comparison_does_not_chain() {
    assert_snapshot!(tree_with_errors("main :: func () { const x := a < b < c }"));
}

#[test]
fn struct_enum_trait_with_generics_and_decorations() {
    assert_snapshot!(tree(
        "\
@public(all)
CatImage :: #packed #align(4) struct {
  id: CatId,
  @private cache: Option.<string>,
}

Storage :: struct <T> { items: []mut T }

Response :: enum {
  ok(string),
  rect { w: f64, h: f64 },
  not_found,
}

Into :: trait <T> {
  into :: func (self: Self) -> T
  Item :: type: Iterator + Clone
}
"
    ));
}

#[test]
fn functions_generics_extern_and_directives() {
    assert_snapshot!(tree(
        "\
@public
render :: #inline func (self: *CatImage) -> string { return self.url }

get :: func <T> (self: *Client, url: string) -> Result.<T, FetchError> {
  return .err(.network_error(\"x\"))
}

zeros :: func <const N: uint32> () -> [N]uint8 { return .{ 0; N } }

@link_name(\"LLVMThing\")
some_fn :: extern(\"c\") func (m: *Module) -> int32
"
    ));
}

#[test]
fn lang_items_and_operator_traits() {
    // `#lang("...")` tags a core-library item as a language item: the compiler
    // reaches operator traits, `Try`, and `Result` through these tags. Purely a
    // directive — it parses like any other `#name(args)`.
    assert_snapshot!(tree(
        "\
Add :: #lang(\"add\") trait <Rhs> {
  Output :: type
  add :: func (self: Self, rhs: Rhs) -> Self.Output
}

Ordering :: #lang(\"ordering\") enum { less, equal, greater }

Result :: #lang(\"result\") enum <T, E> { ok(T), err(E) }

sum :: #lang(\"builtin_add\") func (a: int32, b: int32) -> int32 { return a + b }
"
    ));
}

#[test]
fn impl_blocks_inherent_and_trait() {
    assert_snapshot!(tree(
        "\
impl CatImage {
  @public
  new :: func (id: CatId, url: string) -> CatImage {
    return .{ id: id, url: url }
  }
}

impl <T: ToJson> ToJson for Storage.<T> {
  @public
  render :: func (self: *Storage.<T>) -> string { return \"[]\" }
}
"
    ));
}

#[test]
fn namespaces_and_imports() {
    assert_snapshot!(tree(
        "\
io :: import <std/io>
{ http: { Client, Router } } :: import \"network.nest\"
@public * :: import <prelude>

config :: namespace {
  @public SERVER_PORT :: 8080
}
"
    ));
}

#[test]
fn extern_block_desugars_per_function() {
    assert_snapshot!(tree(
        "\
extern(\"c\") {
  strlen :: func (s: *char) -> uint
  malloc :: func (n: uint) -> *uint8
}
"
    ));
}

#[test]
fn composite_literals_and_arrays() {
    assert_snapshot!(tree(
        "\
main :: func () {
  const a := Point { x: 1, y: 2 }
  const b := .{ id: id, url: url }
  const c := .{ 1, 2, 3 }
  const d := .{ 0; 16 }
  const e := Pair(1, 2)
  const f := [_]int32 { 1, 2, 3 }
}
"
    ));
}

#[test]
fn control_flow_and_ranges() {
    assert_snapshot!(tree(
        "\
main :: func () {
  let i := 0
  while i < 10 { i += 1 }
  for x in 0..<n { total = total + x }
  const first := loop { if done { break x } }
  const label := if port == 80 { \"http\" } else { \"custom\" }
  const s := xs[1..<3]
  const t := xs[..]
}
"
    ));
}

#[test]
fn struct_literal_suppressed_in_condition() {
    // `Router { ... }` in an `if` head is NOT a struct literal: the `{` opens the
    // body. Parens are needed to write a literal there.
    assert_snapshot!(tree(
        "\
main :: func () {
  if ready { work() }
  if (Router { logging: true }).ok { work() }
}
"
    ));
}

#[test]
fn match_and_patterns() {
    assert_snapshot!(tree(
        "\
classify :: func (code: int, node: *Tree) -> string {
  return code.match {
    200 => \"ok\",
    301 | 302 => \"redirect\",
    400..=499 => \"client\",
    500.. => \"server\",
    m @ .text(s) if s.len() > 280 => \"long\",
    .rect { w, h } => \"rect\",
    [first, .. rest] => \"slice\",
    &.leaf(v) => \"leaf\",
    _ => \"other\",
  }
}
"
    ));
}

#[test]
fn error_handling_and_intrinsics() {
    assert_snapshot!(tree(
        "\
load :: func () -> Result.<Config, Error> {
  const text := read_file(\"c\").?
  const n := size_of.<CatImage>()
  const p := new.<CatImage>()
  const port := to_port(8080).!
  defer file.close()
  const bits := cast.<*dyn ToJson>(&cat)
  return .ok(parsed)
}
"
    ));
}

#[test]
fn destructuring_and_defer_block() {
    assert_snapshot!(tree(
        "\
main :: func () {
  const .{ width, height } := cat
  const (a, b) := pair
  let [head, .. tail] := xs
  defer { cleanup() }
  const p := &mut buf
  p.* = 1
}
"
    ));
}

#[test]
fn associated_type_bounds_and_generic_aliases() {
    assert_snapshot!(tree(
        "\
IntVec :: Vector.<int32>
Pairs :: Map.<string, Vector.<int32>>
IntoI32 :: Into.<Output = int32>

sum :: func <I: Iterator.<Item = int32> + Clone> (it: I) -> int32 { return 0 }
"
    ));
}

#[test]
fn block_value_from_match_arm() {
    assert_snapshot!(tree(
        "\
pick :: func (b: bool) -> int {
  return b.match {
    true => {
      const y := 10
      y + 1
    },
    false => 0,
  }
}
"
    ));
}

#[test]
fn newline_continuation_rules() {
    // Line ends with `+` -> one statement; next line starts with `.` -> chains;
    // a bare `-` on the next line -> a new (unary) statement.
    assert_snapshot!(tree(
        "\
main :: func () {
  const x := a +
             b
  const y := foo()
    .bar()
    .baz
}
"
    ));
}

#[test]
fn nested_functions() {
    assert_snapshot!(tree(
        "\
main :: func () {
    nested :: func () {
        5
    }
    const y := nested()
    y + 1
}
"
    ))
}

#[test]
fn default_arguments_parse() {
    // `:=` binds a default to a parameter (§5.2). The parser records what was
    // written and nothing more — the trailing-order rule, the constant
    // restriction and the type check all belong to later stages — so a default
    // here is an ordinary expression subtree hanging off the `Param`.
    assert_snapshot!(tree(
        "\
pad :: func (s: string, width: usize := 8, fill: char := ' ') -> string {
    s
}
mk :: func (c: Cfg := .{ a: 1 }, n: i32 := cast.<i32>(2)) {
}
"
    ))
}

/// `f"..."` parses to its pieces in source order — literal segments and embedded
/// expressions together, because the order is the whole content of the literal.
#[test]
fn an_interpolated_string_keeps_its_pieces_in_order() {
    assert_snapshot!(tree(
        "greet :: func (n: str, k: i32) -> str { return f\"hi {n}, {k + 1}!\" }\n"
    ));
}

/// The expression inside the braces is parsed by the **ordinary** expression
/// parser, so everything it can parse works here: a struct literal with braces
/// of its own, a call with a string argument containing a brace, a nested
/// interpolation.
#[test]
fn an_interpolation_holds_an_ordinary_expression() {
    assert_snapshot!(tree(
        "P :: struct { x: i32 }\n\
         f :: func (s: str) -> str { return s }\n\
         go :: func () -> str { return f\"{ P { x: 1 }.x } { f(\"}\") } { f\"{1}\" }\" }\n"
    ));
}

/// A specifier the grammar has no place for is refused, and the message names
/// the character it stopped at (§1.5, §6.11).
///
/// The span is the literal's, as it is for every other lexing failure inside an
/// `f"..."`: the specifier is read by a `logos` callback, which yields a reason
/// and lets the lexer place it. What follows the failure is re-lexed as ordinary
/// source, which is where the errors after the first come from.
#[test]
fn a_malformed_format_specifier_is_refused_at_itself() {
    assert_snapshot!(tree_with_errors(
        "go :: func (n: i32) -> str { return f\"{n:q}\" }\n"
    ));
}

/// Several statements may share a line when a `;` separates them, and the
/// classifier that decides what a statement *is* looks ahead to the end of that
/// statement.
///
/// So `f(x); n = 1` is two statements whose scan finds an assignment operator
/// belonging to the second one. Taking that as evidence about the first parsed
/// `f(x)` as an assignment's place and then reported "expected an assignment
/// operator" at `n` — for a program that is written correctly.
#[test]
fn a_call_may_be_followed_by_an_assignment_on_one_line() {
    assert_snapshot!(tree(
        "f :: func (n: i32) {}\n\
         go :: func () -> i32 { let mut n: i32 := 0; f(n); n = n + 1; return n }\n"
    ))
}

/// Two statements on **one line** need a `;` between them (spec §1.1).
///
/// A newline is a separator and stays one, so the rule only bites where a
/// statement ran straight into the next one on the same line — which used to
/// parse, and read as one thing while meaning two: `if n == 0 { put(48) return }`
/// is a call and a `return`, not a call whose result is returned.
#[test]
fn two_statements_on_one_line_need_a_semicolon() {
    for src in [
        "go :: func () { f() g() }\n",
        "go :: func () -> i32 { let n: i32 := 1 return n }\n",
        "go :: func () { let mut n: i32 := 0 n = 1 }\n",
        "go :: func (n: i32) { if n == 0 { f() return } }\n",
    ] {
        let (_, errors) = Parser::parse_file(src, FileId(0));
        assert_eq!(errors.len(), 1, "one diagnostic for {src:?}: {errors:#?}");
        assert!(
            errors[0].message.contains("expected `;` or a newline"),
            "{:#?}",
            errors[0]
        );
    }
}

/// What the rule leaves alone: a `;`, a newline, a statement the closing `}`
/// follows, and a line the **next** one continues.
///
/// The continuation is the case worth pinning down, because it is the one place
/// a newline is *not* a separator: §1.1 joins a line that begins with `.`, `+`
/// or `::` to the one before it, so the joined text is a single statement and
/// the rule must not see two.
#[test]
fn a_separator_is_a_semicolon_a_newline_or_a_continuation() {
    for src in [
        "go :: func () -> i32 { let n: i32 := 1; return n }\n",
        "go :: func () -> i32 { let n: i32 := 1\n  return n }\n",
        "go :: func () -> i32 { let n: i32 := 1\n  n }\n",
        "go :: func (s: P) -> i32 { let n := s\n  .x\n  return n }\n",
        "go :: func (s: P) -> i32 { return s\n  .x }\n",
    ] {
        let (_, errors) = Parser::parse_file(src, FileId(0));
        assert!(errors.is_empty(), "for {src:?}: {errors:#?}");
    }
}
