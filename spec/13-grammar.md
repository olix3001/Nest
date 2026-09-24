# 13 — Consolidated Grammar

Non-normative EBNF summary of the whole surface syntax. `[x]` optional, `{x}`
zero-or-more, `x | y` alternation, `'x'` literal terminal. Lexical productions
(`identifier`, `literal`, comments, string/number
forms) are defined in [01-lexical-structure.md](01-lexical-structure.md). Where
this grammar and the prose chapters disagree, the prose wins.

## 13.1 Files and declarations

```
file        = { file_item }
file_item   = declaration
            | impl_block
            | extern_block
            | comptime_item

// Groups external declarations under one ABI. Desugars to individual bodyless
// `func_expr`s each carrying that `extern(abi)` — purely surface sugar, no
// distinct AST node. Members are function declarations only (no bodies).
extern_block = 'extern' '(' string ')' '{' { declaration } '}'

// A source file is an anonymous namespace: its file_items are its members.
// There is no file header and no bodyless `namespace name` form. Another file's
// namespace is obtained with `import` (see 04). An `import` is always the RHS of
// a `const_bind`; there is no standalone import statement.

declaration = { attribute } [ directive ] ( const_bind | local_decl )

attribute   = '@' identifier [ '(' [ attr_arg { ',' attr_arg } ] ')' ]
directive   = '#' ( identifier | 'const' ) [ '(' [ arg { ',' arg } ] ')' ] { directive }
comptime_item = call                               // e.g. comptime_assert(...)  (returns void)

const_bind  = pattern [ ':' type ] '::' const_rhs
            | '#static' ... identifier ':' type [ '::' expr ]   // zeroed if omitted
const_rhs   = expr
            | type_expr                   // a type alias / assoc-type binding
            | func_expr
            | overload_set
            | trait_expr
            | namespace_expr
            | import_expr
```

A `directive` name is drawn from the compiler's fixed set (`packed`, `align`,
`soa`, `inline`, `const`, `static`, `raw`, `unsafe`, `lang`, `intrinsic`, …); `#lang(str)`
tags a core-library item as a language item (see
[09-directives-and-attributes.md](09-directives-and-attributes.md) §9.3 and
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.13).

`const_bind` is the single `::` binding form; the RHS category (value, type,
func, trait, namespace, import) determines what is bound. A type written
**before** the `::` makes it a **typed** binding — a pinned constant (§2.5), an
associated constant (§3.4), or, under `#static`, a program-lifetime mutable
region (§2.6). That is what leaves `name :: type` meaning a type alias in every
case: `#static s: [4]u8` is a zeroed region and `s :: [4]u8` is an alias, and
the two are told apart by the directive and the colon rather than by what
follows three tokens later. A `#static` **must** write its type. A `field_item`
inside a
struct/enum/trait/namespace body may also be a `comptime_item` (e.g.
`comptime_assert(...)`). A `local_decl` at namespace scope is rejected outright: `let`
binds a stack slot and there is no call there — use `::`, with `#static` for a
mutable region. See
[02-declarations-and-bindings.md](02-declarations-and-bindings.md).

## 13.2 Imports

```
import_expr = 'import' import_path              // see 04; always the RHS of a `::`
import_path = '<' pkg_path '>'                  // package: std / third-party
            | string                            // file: resolved on the file system
pkg_path    = identifier { '/' identifier }     // first segment = package root

// binding forms (the LHS pattern decides the effect):
//   name        :: import ...     bind whole namespace
//   *           :: import ...     glob all public members into scope
//   { ... }     :: import ...     selective / nested destructure ('*' allowed as a field value)
//   [ '@public' ] pattern :: import ...   additionally re-export what it brings in
```

## 13.3 Type-forming expressions

```
type_expr   = distinct_type | struct_type | enum_type | trait_expr | func_type | type

distinct_type = 'distinct' type
struct_type   = { directive } 'struct' [ struct_body ]
struct_body   = '{' { field | comptime_item } '}'      // record
              | '(' type { ',' type } ')'              // tuple struct
                                                       // (absent) => unit struct
field         = { attribute } identifier ':' type ','    // no default; see 3.4
enum_type     = { directive } 'enum' [ generics ] '{' { variant } '}'
variant       = { attribute } snake_ident [ variant_payload ] [ '=' expr ] ','
variant_payload = '(' type { ',' type } ')' | '{' { field } '}'
trait_expr    = { directive } 'trait' [ generics ] '{' { trait_member } '}'
trait_member  = method_sig | assoc_type | assoc_const
method_sig    = identifier '::' 'func' [ generics ] '(' [ params ] ')' [ '->' type ]
assoc_type    = identifier '::' 'type' [ ':' bounds ]  // e.g. Item :: type: Iterator + Clone
assoc_const   = identifier ':' type [ '::' expr ]      // e.g. MAX: i32 :: 100
bounds        = type { '+' type }                      // trait bounds only; bare `type` kind is not a bound

type        = type_core
type_core   = qualified_name [ generic_args ]
            | '*' [ 'mut' ] type                        // pointer
            | '[' ']' [ 'mut' ] type                    // slice
            | '[' expr ']' [ 'mut' ] type               // array
            | '(' [ type { ',' type } ] ')'             // tuple / void
            | 'dyn' type                                // trait object
            | '*' func_type                             // function pointer (§3.5); never bare
generic_args = '.<' generic_arg { ',' generic_arg } '>'   // always dotted; bare `<...>` never valid here
generic_arg  = type_or_hole | assoc_binding
type_or_hole = type | '_'                               // '_' = infer this argument
assoc_binding = identifier '=' type                     // associated-type equality: Iterator.<Item = int32>
qualified_name = ( identifier | 'Self' ) { '.' identifier }   // 'Self' may head a type-level path: Self, Self.Residual
```

`Self` (a keyword) may head a **type-level** `qualified_name` — `Self`,
`Self.Residual`, `Self.Item.<T>` — resolving its root to the implementing type
inside a `trait` / `impl` (see [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.7). Since a type is an ordinary
compile-time value, the trailing `.<...>` applies to whatever the path denotes,
so any `A.B.C.<X, Y, Item = int32>` is a well-formed type expression.

There is no `?T`; optionals are `Option.<T>`. `dyn Trait` is the only vtable-bearing
type (usually `*dyn Trait`).

An `assoc_binding` argument constrains a trait's associated type to a concrete
type inside the turbofish: `Iterator.<Item = int32>` is the `Iterator` trait with
its `Item` associated type pinned to `int32`. It is unambiguous against a
positional `type` argument because a type is never followed by `=` in this
position. Associated bindings and positional type arguments may be mixed
(`Trait.<K, Item = V>`); each `name` must be an associated type declared by the
trait. Because such a constrained trait is itself a `type`, it may appear
anywhere a bound may — see [05-functions-and-generics.md](05-functions-and-generics.md) §5.4.

## 13.4 Namespaces and impls

```
namespace_expr = 'namespace' '{' { file_item } '}'
impl_block     = 'impl' [ generics ] type [ 'for' type ] '{' { file_item } '}'
               // no 'for'  => inherent impl, the type is the target
               // with 'for' => trait impl, `impl <g> Trait for Target`
```

`impl T {...}` adds inherent items to `T`; `impl Trait for T {...}` implements
`Trait` for `T`. Optional `generics` (`impl <T> ...`, same `< >` declaration form
as `func <T>`) parameterize the impl over a family of types — blanket,
generic-trait, and conditional impls; on overlap the **most specific** matching
impl is selected, and incomparable overlap is an error. See
[04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md).

## 13.5 Functions

```
func_expr = { directive } [ extern_spec ] 'func' [ generics ] '(' [ params ] ')' [ '->' type ] [ block ]
                                          // block omitted => external declaration (extern, no body)
overload_set = 'func' '{' [ path { ',' path } [ ',' ] ] '}'   // one name for several functions (§4.3)
func_type = [ extern_spec ] 'func' [ generics ] '(' [ param_types ] ')' [ '->' type ]
                                          // only behind '*': `*func(...)`, `*extern("c") func(...)`
extern_spec = 'extern' '(' string ')'      // ABI selector, next to `func`; string is e.g. "c"

generics      = '<' generic_param { ',' generic_param } '>'
generic_param = identifier [ ':' constraint ]     // type param; bare `T` is unconstrained
              | 'const' identifier ':' type        // compile-time value param
constraint    = type { '+' type }                 // trait bounds; a bare param is already a type

params    = param { ',' param }
param     = 'self' [ ':' type ]
          | identifier ':' type
param_types = type { ',' type }
```

## 13.6 Statements

```
block     = '{' { statement stmt_end } [ expr ] '}'
stmt_end  = ';' | newline | &'}'                 // two statements on one line need the ';' (1.1)
statement = local_decl
          | const_bind
          | assign_stmt
          | defer_stmt
          | return_stmt
          | break_stmt | continue_stmt
          | loop_stmt
          | expr                                 // includes an intrinsic call, e.g. comptime_assert(...)

local_decl  = ( 'let' | 'const' ) pattern [ ':' type ] ':=' expr
assign_stmt = place assign_op expr               // place must be mutable
assign_op   = '=' | '+=' | '-=' | '*=' | '/=' | '%='
defer_stmt  = 'defer' ( expr | block )
return_stmt = 'return' [ expr ]
break_stmt  = 'break' [ expr ]                    // value only inside 'loop'
continue_stmt = 'continue'

loop_stmt = 'loop' block
          | 'while' expr block
          | 'for' pattern 'in' expr block         // iterates an Iterator (see 10)
```

`place` is an assignable postfix expression whose root is a `let` variable (or a
field/index/`.*` through a `*mut`/`[]mut`). See
[02-declarations-and-bindings.md](02-declarations-and-bindings.md) §2.3.

## 13.7 Expressions

```
expr    = or_expr
or_expr = and_expr { ('||' | 'or') and_expr }
and_expr= cmp_expr { ('&&' | 'and') cmp_expr }
cmp_expr= bitor_expr [ cmp_op bitor_expr ]              // non-associating
cmp_op  = '==' | '!=' | '<' | '<=' | '>' | '>='
bitor_expr = bitxor_expr { '|' bitxor_expr }
bitxor_expr= bitand_expr { '^' bitand_expr }
bitand_expr= shift_expr { '&' shift_expr }
shift_expr = add_expr { ('<<' | '>>') add_expr }
add_expr   = mul_expr { ('+' | '-') mul_expr }
mul_expr   = unary_expr { ('*' | '/' | '%') unary_expr }
unary_expr = ( '&' [ 'mut' ] | '-' | '!' | 'not' | '~' ) unary_expr
           | postfix_expr
                                                // note: deref is postfix '.*', not prefix '*'

postfix_expr = primary { postfix_op }
postfix_op   = '.' identifier
             | '.' integer
             | generic_args                    // '.<' type_or_hole,... '>'
             | '(' [ args ] ')'
             | '[' expr ']'
             | '[' range_expr ']'              // slicing
             | '.*'                             // dereference
             | '.?'                             // Try: unwrap-or-return   (see 08)
             | '.!'                             // Try: unwrap-or-abort     (see 08)
             | '.match' match_block          // sugar for `match_expr`

primary = literal
        | interpolated_string                   // f"...{ expr }..."; see 1.5, 6.11
        | qualified_name
        | 'self' | 'Self'
        | 'true' | 'false'
        | '(' expr ')'
        | '(' expr { ',' expr } ')'            // tuple
        | composite_literal
        | func_expr                            // closure
        | if_expr
        | if_match_expr
        | match_expr
        | block
        | import_expr                           // '<pkg>' or "file"; see 13.2


// A lexical production: the pieces alternate, `{{` / `}}` stand for one brace,
// and a lone `}` is an error. The braces around an embedded expression are
// matched by lexing it, so it may contain a string holding a brace or a nested
// interpolation, but it may not contain a newline.
interpolated_string = 'f"' { string_segment | '{' expr [ ':' format_spec ] '}' } '"'
format_spec         = [ [ fill ] align ] [ '+' ] [ '#' ] [ '0' ] [ width ]
                      [ '.' precision ] [ format_type ]
align               = '<' | '^' | '>'
format_type         = '?' | 'x' | 'X' | 'b' | 'o'

composite_literal =
    type '{' composite_body '}'                      // typed record OR array (by type)
  | type '(' [ args ] ')'                            // typed tuple struct
  | '.{' composite_body '}'                          // inferred record | array | tuple
  | '.' snake_ident [ '(' [ args ] ')' | '{' [ field_init { ',' field_init } ] '}' ]  // enum variant
composite_body =
    [ field_init { ',' field_init } [ ',' '..' expr ] ]  // named   -> record / struct
                                                        //   (also after '.{')
  | [ expr { ',' expr } ]                            // positional  -> array / tuple
  | expr ';' expr                                    // repeat: value ; count -> array
field_init = identifier ':' expr

args = arg { ',' arg }
arg  = [ identifier ':' ] expr                 // positional or named

range_expr = expr '..<' expr | expr '..=' expr | expr '..'
           | '..<' expr | '..=' expr | '..'

if_expr       = 'if' expr block [ 'else' ( if_expr | block ) ]
match_expr    = 'match' expr match_block
if_match_expr = 'if' 'match' pattern ':=' expr block [ 'else' block ]

match_block = '{' arm { ',' arm } '}'
arm         = pattern [ 'if' expr ] '=>' ( expr | block )
```

Trailing-block call sugar (a final `func`-typed argument written as a block after
`)`, optionally with a parameter header):

```
trailing_call  = callee [ '(' [ args ] ')' ] closure_block
closure_block  = '{' [ closure_header ] { statement stmt_end } [ expr ] '}'
closure_header = param { ',' param } '=>'      // params; types optional (inferred)
```

See [05-functions-and-generics.md](05-functions-and-generics.md) §5.3. There is no
bare `expr?`; propagation is the postfix `.?`.

## 13.8 Patterns

```
pattern = '_'
        | '*'                                            // glob (namespace import only)
        | [ 'mut' ] identifier
        | identifier '@' pattern
        | literal
        | range_pat
        | '.' snake_ident [ variant_payload_pat ]        // enum variant
        | [ '.' ] '{' field_pat { ',' field_pat } [ ',' '..' ] '}'   // struct / namespace
        | type '(' pattern { ',' pattern } [ ',' '..' ] ')'          // tuple struct
        | '(' pattern { ',' pattern } ')'                            // tuple
        | '[' [ pattern { ',' pattern } ] [ ',' '..' [ identifier ] ] ']'  // slice
        | '&' pattern                                    // dereference
        | pattern '|' pattern                            // or-pattern

range_pat = expr '..<' expr | expr '..=' expr | expr '..'
          | '..<' expr | '..=' expr
variant_payload_pat = '(' pattern { ',' pattern } ')'
                    | '{' field_pat { ',' field_pat } [ ',' '..' ] '}'
field_pat = [ 'mut' ] identifier | identifier ':' pattern
```

Patterns appear on the LHS of `::`, `let`/`const`, `for … in`, and `match` arms.
Binding sites require irrefutable patterns; `match` arms and `if match` may be
refutable. See [07-patterns-and-matching.md](07-patterns-and-matching.md).
