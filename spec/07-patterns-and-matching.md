# 07 — Patterns and Matching

A **pattern** describes the shape of a value and binds names to its parts. One
pattern grammar is reused in three places:

1. the left-hand side of a `::` binding (including `import` destructuring),
2. the left-hand side of a `let` / `const` binding,
3. the arms of a `match`, and the `for` loop binding.

## 7.1 Pattern grammar

```
pattern =
    '_'                                   // wildcard: matches anything, binds nothing
  | '*'                                   // glob: import all public members into scope (namespace value only)
  | [ 'mut' ] identifier                  // binding (optionally mutable), matches anything
  | identifier '@' pattern                // binding + sub-pattern (bind the whole, match inside)
  | literal                               // matches an equal value
  | range_pat                             // numeric / char range
  | '.' variant_name [ variant_payload_pat ]   // enum variant
  | [ '.' ] '{' field_pat { ',' field_pat } [ ',' '..' ] '}'  // struct / namespace
  | type '(' pattern { ',' pattern } [ ',' '..' ] ')'          // tuple struct
  | '(' pattern { ',' pattern } ')'       // tuple
  | '[' pattern { ',' pattern } [ ',' '..' [ identifier ] ] ']'  // slice / array
  | pattern '|' pattern                   // or-pattern (alternatives)
  | '&' pattern                           // dereference: match through a pointer
  | pattern 'if' expr                     // guard (match arms only)

range_pat =
    expr '..<' expr        // half-open  lo..<hi   (lo inclusive, hi exclusive)
  | expr '..=' expr        // closed     lo..=hi
  | '..<' expr             //   ..<hi     (up to, exclusive)
  | '..=' expr             //   ..=hi     (up to, inclusive)
  | expr '..'              //   lo..      (from, unbounded)

variant_payload_pat =
    '(' pattern { ',' pattern } ')'       // tuple payload
  | '{' field_pat { ',' field_pat } [ ',' '..' ] '}'   // record payload

field_pat =
    [ 'mut' ] identifier                  // shorthand: bind field to same name
  | identifier ':' pattern                // rename / nested destructure
```

Pattern power, at a glance: wildcards, the `*` **glob** (namespace-import only),
(mutable) bindings, `@`-bindings, literal and **range** matches, enum variants
with tuple or record payloads, struct / tuple / **slice** destructuring with a
`..` rest, **or-patterns**, pointer **dereference** patterns, and **guards**.

The `*` glob is special: it is valid only when the operand is a `namespace` value
(i.e. destructuring an `import`), where it means "bring every public member into
scope" rather than binding a name. Either as the whole pattern (`* :: import
<std>`) or inside a field (`{ http: * } :: import <std>`). See
[04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.5.

## 7.2 Destructuring bindings

A `::` / `let` / `const` left-hand side is a pattern, so a compound value is
pulled apart at the binding site:

```
{ CatImage, CatId, HttpPort } :: import "models.nest"
{ http: { Client, Router, Response } } :: import "network.nest"
{ http: * } :: import <std>                 // glob std.http's members into scope

const .{ width, height } := cat            // bind two fields
const (a, b) := pair                        // tuple
const .{ url: u, .. } := cat                // bind `url` as `u`, ignore the rest
let   [first, .. rest] := xs                // slice: head + remaining
```

For **bindings** (not `match`), the pattern must be **irrefutable** — it must
match every value of the operand's type. Struct, tuple, slice-with-rest, and
namespace patterns are irrefutable; enum-variant, literal, and range patterns are
refutable and thus only allowed in `match` (or an `if match`, §7.6).

## 7.3 `match`

```
match = expr '.match' '{' arm { ',' arm } '}'
arm   = pattern '=>' ( expr | block )
```

`match` is postfix on the scrutinee, evaluates the first arm whose pattern
matches, and yields that arm's value. All arms share a common type; the whole
`match` is an expression.

```
return result.match {
  .ok(cats) => {
    assert(cats.len() > 0, "Cat array was empty")     // std runtime assert
    let cat := cats[0]
    const json := $cast.<*dyn ToJson>(&cat).render()
    io.println(json)
    return Response.redirect(cat.url)
  },

  .err(e) => e.match {
    .network_error(msg) => Response.internal_server_error(msg),
    .parse_error        => Response.bad_request("Invalid payload format"),
  },
}
```

## 7.4 Rich pattern examples

```
// literal + range + or-pattern
code.match {
  200          => "ok",
  301 | 302    => "redirect",
  400..=499    => "client error",
  500..        => "server error",   // from 500, unbounded
  _            => "unknown",
}

// @-binding: keep the whole value and inspect inside
msg.match {
  m @ .text(s) if s.len() > 280 => truncate(m),
  m                           => m,
}

// dereference pattern: match through a pointer
node.match {
  &.leaf(v)      => v,
  &.branch(l, r) => sum(l) + sum(r),
}

// slice patterns
xs.match {
  []             => "empty",
  [only]         => "one",
  [first, .. _]  => "many",
}

// record-payload variant, ignoring some fields
shape.match {
  .rect { w, h }        => w * h,
  .circle { radius: r } => 3.14159 * r * r,
  .point                => 0.0,
}
```

## 7.5 Exhaustiveness

A `match` over an enum, `Option`, `Result`, or bounded value must be
**exhaustive**: every case is covered, or a wildcard `_` / bare-identifier arm
handles the rest. A non-exhaustive match is a compile error — this is what forces
every `.err` / `.none` case to be considered.

Guards do not count toward exhaustiveness: a guarded arm cannot be the sole
coverage of a case, since its guard might be false. Range and or-patterns *are*
considered — `0..=255` fully covers a `uint8`.

Range **patterns** use the same explicit `..<` / `..=` operators as range
expressions (§6.12); `lo..` matches from `lo` upward.

## 7.6 `if match` (refutable one-armed match)

A single refutable pattern can be tested inline with `if match`, binding on
success:

```
if match .some(v) := lookup(key) {
  use(v)
} else {
  handle_missing()
}
```

This is sugar for a two-arm `match` and is the idiomatic way to handle one case
without full exhaustiveness.
