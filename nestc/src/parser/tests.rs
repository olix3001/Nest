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
