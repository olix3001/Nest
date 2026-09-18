; Nest highlights.
;
; General patterns first, specific ones after: in Zed and Neovim a later match
; on the same node wins.

(identifier) @variable

; A capitalized name is a type or a constant by convention: `Vec`, `MAX`.
((identifier) @type
  (#match? @type "^[A-Z]"))

((identifier) @constant
  (#match? @constant "^[A-Z][A-Z0-9_]+$"))

((identifier) @variable.special
  (#any-of? @variable.special "self" "Self"))

; ===< Types >===

(type_identifier) @type

(type_path
  (identifier) @namespace
  "."
  (identifier) @type .)

((type_identifier) @type.builtin
  (#match? @type.builtin "^([iu][0-9]+|isize|usize|f16|f32|f64|f128|bool|char|str|void|never|comptime_int|comptime_float|type)$"))

(generic_parameter
  name: (identifier) @type)

(associated_binding
  name: (identifier) @type)

; ===< Declarations >===

(const_binding
  pattern: (identifier) @type
  value: [(struct_type) (enum_type) (trait_type) (distinct_type)])

(const_binding
  pattern: (identifier) @function
  value: (func_expression))

(const_binding
  pattern: (identifier) @namespace
  value: [(import_expression) (namespace_expression)])

(static_declaration
  name: (identifier) @variable)

(parameter
  name: (identifier) @variable.parameter)

(closure_parameter
  name: (identifier) @variable.parameter)

(field_identifier) @property

(field_declaration
  name: (field_identifier) @property)

(variant
  name: (identifier) @constructor)

(enum_literal
  name: (identifier) @constructor)

(variant_pattern
  name: (identifier) @constructor)

(attribute
  "@" @attribute
  name: (attribute_name) @attribute)

(directive_name) @preproc
(when_directive_name) @preproc

; `#when`'s conditions: the key and the combinators read as the compiler's own
; words, the variant as the `core/os.nest` variant it is spelled after.
(when_condition
  key: (identifier) @property)
(when_condition
  name: (identifier) @function.builtin)
(when_condition
  flag: (identifier) @constant.builtin)
(when_condition
  "not" @keyword.operator)

(package_path) @string.special

; ===< Calls >===

(call_expression
  function: (identifier) @function)

(call_expression
  function: (field_expression
    field: (field_identifier) @function.method))

(call_expression
  function: (generic_expression
    value: (identifier) @function))

(call_expression
  function: (generic_expression
    value: (field_expression
      field: (field_identifier) @function.method)))

(call_expression
  function: (enum_literal
    name: (identifier) @constructor))

(trailing_closure
  function: (identifier) @function)

(trailing_closure
  function: (field_expression
    field: (field_identifier) @function.method))

(named_argument
  name: (identifier) @property)

(field_initializer
  name: (field_identifier) @property)

(field_pattern
  name: (identifier) @property)

; ===< Literals >===

(integer) @number
(float) @number
(boolean) @boolean
(char) @string
[(string) (byte_string) (c_string) (interpolated_string)] @string
(escape_sequence) @string.escape

(interpolation
  "{" @punctuation.special
  "}" @punctuation.special) @embedded

[(line_comment) (block_comment)] @comment

; ===< Keywords >===

[
  "func"
  "extern"
  "struct"
  "enum"
  "trait"
  "impl"
  "namespace"
  "distinct"
  "dyn"
  "import"
] @keyword

[
  "let"
  "const"
  "mut"
] @keyword

[
  "if"
  "else"
  "match"
  "for"
  "in"
  "while"
  "loop"
  "break"
  "return"
  "defer"
] @keyword

(continue_statement) @keyword

[
  "and"
  "or"
  "not"
] @keyword.operator

; ===< Operators and punctuation >===

[
  "::"
  ":="
  "="
  "+="
  "-="
  "*="
  "/="
  "%="
  "->"
  "=>"
  "+"
  "-"
  "*"
  "/"
  "%"
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "&&"
  "||"
  "!"
  "&"
  "|"
  "^"
  "~"
  "<<"
  ">>"
  ".."
  "..<"
  "..="
  ".*"
  ".?"
  ".!"
] @operator

["(" ")" "[" "]" "{" "}" ".{"] @punctuation.bracket

(generic_arguments
  [".<" ">"] @punctuation.bracket)

(generic_parameters
  ["<" ">"] @punctuation.bracket)

["," ":" ";" "."] @punctuation.delimiter
