/**
 * @file Nest grammar for tree-sitter
 * @license MIT
 *
 * Follows `spec/13-grammar.md` and, where they differ, what `nestc`'s parser
 * accepts. It is for an editor: it has to parse every file `nestc` parses and
 * say what each token is, and it does not have to reject what `nestc` rejects.
 *
 * Newlines end statements, as in `nestc` (`filter_newlines`): the external
 * scanner emits `_terminator` for a newline only where the grammar can end a
 * statement, and not before a line starting with `.`, `+` or `::`, which
 * continues the one before.
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  range: 1,
  or: 2,
  and: 3,
  compare: 4,
  bitor: 5,
  bitxor: 6,
  bitand: 7,
  shift: 8,
  add: 9,
  multiply: 10,
  unary: 11,
  postfix: 12,
};

const commaSep = (rule) => optional(commaSep1(rule));
const sep1 = (rule, sep) => seq(rule, repeat(seq(sep, rule)));
const commaSep1 = (rule) => seq(rule, repeat(seq(',', rule)), optional(','));

module.exports = grammar({
  name: 'nest',

  externals: $ => [
    $._terminator,
    $.block_comment,
    $._error_sentinel,
  ],

  extras: $ => [
    /\s/,
    $.line_comment,
    $.block_comment,
  ],

  word: $ => $.identifier,

  supertypes: $ => [
    $._expression,
    $._type,
    $._pattern,
  ],

  conflicts: $ => [
    [$._expression, $.literal_pattern],
    [$.static_declaration, $._pattern],
    [$.type_path, $._expression],
    [$._expression, $._pattern],
    [$.tuple_expression, $.tuple_pattern],
    [$.block, $.destructure_pattern],
    [$.static_declaration, $.closure_parameter, $._pattern, $.field_pattern],
    [$._expression, $.field_pattern],
    [$.closure_parameter, $.field_pattern],
    [$.enum_literal, $.variant_pattern],
    [$.anonymous_composite, $.destructure_pattern],
    [$._expression, $.composite_literal, $.trailing_closure],
    [$._expression, $.composite_literal],
    [$._expression, $.trailing_closure],
    [$.defer_statement, $._expression],
    [$.static_declaration, $.closure_parameter, $._pattern],
    [$._expression, $.mut_pattern],
    [$.block, $.composite_literal],
    [$.static_declaration, $.closure_parameter, $.field_initializer, $._pattern],
    [$.closure_parameter, $._expression],
    [$.expression_statement, $._composite_body],
    [$.type_path],
    [$.type_path, $._pattern],
    [$.tuple_type, $.tuple_pattern],
    [$._type, $._expression],
    [$.tuple_type, $.tuple_expression],
    [$.func_type, $.func_expression],
    [$.capture_list, $._expression, $._pattern],
    [$.capture_list, $._pattern],
  ],

  rules: {
    source_file: $ => optional($._items),

    _items: $ => seq(
      $._item,
      repeat(seq($._separator, $._item)),
      optional($._separator),
    ),

    _separator: $ => choice($._terminator, ';'),

    _item: $ => choice(
      $.declaration,
      $.impl_block,
      $.extern_block,
      $.expression_statement,
    ),

    // ===< Declarations >===

    declaration: $ => seq(
      repeat($.attribute),
      repeat($.directive),
      choice($.const_binding, $.static_declaration, $.let_declaration),
    ),

    attribute: $ => prec.right(seq(
      '@',
      field('name', alias($.identifier, $.attribute_name)),
      optional($.arguments),
    )),

    directive: $ => prec.right(choice(
      seq(field('name', $.directive_name), optional($.arguments)),
      // `#when` has an argument grammar of its own: `=` is not an expression
      // and `not` is a keyword, so its conditions cannot be `arguments`.
      seq(field('name', $.when_directive_name), optional($.when_arguments)),
    )),

    directive_name: _ => token(seq('#', /[A-Za-z_][A-Za-z0-9_]*/)),

    when_directive_name: _ => token(prec(1, '#when')),

    when_arguments: $ => seq('(', commaSep1($.when_condition), ')'),

    when_condition: $ => choice(
      seq(field('key', $.identifier), '=', field('value', $.enum_literal)),
      seq(
        field('name', choice($.identifier, 'not')),
        '(',
        commaSep1($.when_condition),
        ')',
      ),
      field('flag', $.identifier),
    ),

    const_binding: $ => seq(
      field('pattern', $._pattern),
      optional(seq(':', field('type', $._type))),
      '::',
      repeat($.directive),
      field('value', $._binding_value),
    ),

    // `#static name: T := value`, and a trait's `NAME: T` with no value.
    static_declaration: $ => seq(
      field('name', $.identifier),
      ':',
      field('type', $._type),
      optional(seq(choice(':=', '='), field('value', $._expression))),
    ),

    _binding_value: $ => choice(
      $._expression,
      $.import_expression,
      $.struct_type,
      $.enum_type,
      $.trait_type,
      $.namespace_expression,
      $.distinct_type,
      $.pointer_type,
      $.dyn_type,
    ),

    let_declaration: $ => seq(
      choice('let', 'const'),
      field('pattern', $._pattern),
      optional(seq(':', field('type', $._type))),
      ':=',
      field('value', $._expression),
    ),

    import_expression: $ => seq(
      'import',
      field('path', choice($.package_path, $.string)),
    ),

    package_path: _ => token(seq('<', /[A-Za-z_][A-Za-z0-9_]*(\/[A-Za-z_][A-Za-z0-9_]*)*/, '>')),

    impl_block: $ => seq(
      'impl',
      optional(field('generics', $.generic_parameters)),
      field('type', $._type),
      optional(seq('for', field('target', $._type))),
      field('body', $.item_body),
    ),

    extern_block: $ => seq(
      'extern',
      '(',
      field('abi', $.string),
      ')',
      field('body', $.item_body),
    ),

    item_body: $ => seq('{', optional($._items), '}'),

    namespace_expression: $ => seq('namespace', field('body', $.item_body)),

    // ===< Types >===

    _type: $ => choice(
      $.type_path,
      $.pointer_type,
      $.slice_type,
      $.array_type,
      $.tuple_type,
      $.dyn_type,
      $.func_type,
      $.impl_type,
      $.callable_type,
    ),

    type_path: $ => prec.right(seq(
      choice(
        alias($.identifier, $.type_identifier),
        seq($.identifier, repeat1(seq('.', $.identifier))),
      ),
      optional($.generic_arguments),
      repeat(seq('.', $.identifier, optional($.generic_arguments))),
    )),

    generic_arguments: $ => seq(
      '.<',
      commaSep1(choice(
        $._type,
        $.integer,
        $.associated_binding,
      )),
      '>',
    ),

    associated_binding: $ => seq(
      field('name', $.identifier),
      '=',
      field('type', $._type),
    ),

    pointer_type: $ => prec.right(seq('*', optional('mut'), $._type)),

    slice_type: $ => prec.right(seq('[', ']', optional('mut'), $._type)),

    array_type: $ => prec.right(seq('[', field('length', $._expression), ']', optional('mut'), $._type)),

    tuple_type: $ => seq('(', commaSep($._type), ')'),

    dyn_type: $ => prec.right(seq('dyn', $._type)),

    // `impl Display + Debug` — a type the program leaves unnamed (spec §5.4).
    impl_type: $ => prec.right(seq('impl', sep1($._type, '+'))),

    // `Func(i32, i32) -> i32` — a trait over a call's shape, spelled the way the
    // call is (spec §5.5).
    callable_type: $ => prec.right(1, seq(
      field('trait', $.type_path),
      '(',
      commaSep($._type),
      ')',
      optional(seq('->', field('return_type', $._type))),
    )),

    // Only ever behind a `*` (spec §3.5): `*func(i32) -> i32`, or
    // `*extern("c") func(...)` for a C callback.
    func_type: $ => prec.right(seq(
      optional($.extern_specifier),
      'func',
      optional($.generic_parameters),
      $.parameters,
      optional(seq('->', field('return_type', $._type))),
    )),

    distinct_type: $ => seq('distinct', $._type),

    struct_type: $ => prec.right(seq(
      'struct',
      optional(field('generics', $.generic_parameters)),
      optional(choice($.field_list, $.tuple_type)),
    )),

    field_list: $ => seq('{', commaSep($.field_declaration), '}'),

    field_declaration: $ => seq(
      repeat($.attribute),
      field('name', alias($.identifier, $.field_identifier)),
      ':',
      field('type', $._type),
    ),

    enum_type: $ => seq(
      'enum',
      optional(field('generics', $.generic_parameters)),
      '{',
      commaSep($.variant),
      '}',
    ),

    variant: $ => seq(
      repeat($.attribute),
      field('name', $.identifier),
      optional(choice($.tuple_type, $.field_list)),
      optional(seq('=', field('value', $._expression))),
    ),

    trait_type: $ => seq(
      'trait',
      optional(field('generics', $.generic_parameters)),
      field('body', $.item_body),
    ),

    generic_parameters: $ => seq(
      '<',
      commaSep1($.generic_parameter),
      '>',
    ),

    generic_parameter: $ => choice(
      seq(
        field('name', alias($.identifier, $.type_identifier)),
        optional(seq(':', field('bounds', $.bounds))),
      ),
      seq('const', field('name', $.identifier), ':', field('type', $._type)),
      // `Self.Item: Ord` on a trait's method (§3.4) — a bound on one of the
      // trait's associated types, not a parameter. `Self: Sized` is the first
      // form above.
      seq(
        field('name', alias($.identifier, $.type_identifier)),
        '.',
        field('member', alias($.identifier, $.type_identifier)),
        ':',
        field('bounds', $.bounds),
      ),
    ),

    bounds: $ => prec.right(seq($._type, repeat(seq('+', $._type)))),

    // ===< Functions >===

    func_expression: $ => prec.right(seq(
      optional($.extern_specifier),
      'func',
      optional(field('generics', $.generic_parameters)),
      field('parameters', $.parameters),
      optional(seq('->', field('return_type', $._type))),
      optional(field('body', $.block)),
    )),

    // `func { a, b, m.c }` — an overload set: one name for several functions
    // (spec §4.3). A function always writes its parameter list, so the `{`
    // after `func` tells the two apart.
    overload_set: $ => seq(
      'func',
      '{',
      optional(commaSep1(field('member', $._expression))),
      optional(','),
      '}',
    ),

    extern_specifier: $ => seq('extern', '(', $.string, ')'),

    parameters: $ => seq('(', commaSep(choice($.parameter, $._type)), ')'),

    parameter: $ => seq(
      optional('mut'),
      field('name', $.identifier),
      ':',
      field('type', $._type),
      optional(seq(':=', field('default', choice($._expression, $.directive)))),
    ),

    // ===< Statements >===

    block: $ => seq(
      '{',
      optional($._statements),
      '}',
    ),

    // `{ [n] x, y -> i32 in body }` — a closure (spec §5.5). The header is what
    // tells it from a block: `in` ends it, and `{ in body }` takes nothing.
    closure_expression: $ => seq(
      '{',
      optional(field('captures', $.capture_list)),
      commaSep($.closure_parameter),
      optional(seq('->', field('return_type', $._type))),
      'in',
      optional($._statements),
      '}',
    ),

    capture_list: $ => seq('[', commaSep1($.identifier), ']'),

    closure_parameter: $ => seq(
      field('name', $.identifier),
      optional(seq(':', field('type', $._type))),
    ),

    _statements: $ => seq(
      $._statement,
      repeat(seq($._separator, $._statement)),
      optional($._separator),
    ),

    _statement: $ => choice(
      $.declaration,
      $.assignment,
      $.defer_statement,
      $.return_statement,
      $.break_statement,
      $.continue_statement,
      $.while_statement,
      $.for_statement,
      $.expression_statement,
    ),

    expression_statement: $ => $._expression,

    assignment: $ => seq(
      field('left', $._expression),
      field('operator', choice('=', '+=', '-=', '*=', '/=', '%=')),
      field('right', $._expression),
    ),

    defer_statement: $ => seq('defer', choice($._expression, $.block)),

    return_statement: $ => prec.right(seq('return', optional($._expression))),

    break_statement: $ => prec.right(seq('break', optional($._expression))),

    continue_statement: _ => 'continue',

    loop_expression: $ => seq('loop', field('body', $.block)),

    while_statement: $ => seq(
      'while',
      field('condition', $._expression),
      field('body', $.block),
    ),

    for_statement: $ => seq(
      'for',
      field('pattern', $._pattern),
      'in',
      field('iterator', $._expression),
      field('body', $.block),
    ),

    // ===< Expressions >===

    _expression: $ => choice(
      $.identifier,
      $.integer,
      $.float,
      $.boolean,
      $.string,
      $.byte_string,
      $.c_string,
      $.char,
      $.interpolated_string,
      $.parenthesized_expression,
      $.tuple_expression,
      $.anonymous_composite,
      $.enum_literal,
      $.block,
      $.if_expression,
      $.if_match_expression,
      $.match_expression,
      $.func_expression,
      $.overload_set,
      $.loop_expression,
      $.slice_type,
      $.array_type,
      $.unary_expression,
      $.binary_expression,
      $.range_expression,
      $.field_expression,
      $.generic_expression,
      $.call_expression,
      $.index_expression,
      $.dereference_expression,
      $.try_expression,
      $.method_match_expression,
      $.composite_literal,
      $.trailing_closure,
      $.closure_expression,
    ),

    parenthesized_expression: $ => seq('(', $._expression, ')'),

    // An element may be a pointer type: `Item :: (K, *mut V)` in an impl is a
    // tuple of types, written where the right side of `::` is read as a value.
    // No expression begins with `*` (a dereference is the postfix `.*`).
    tuple_expression: $ => choice(
      seq('(', ')'),
      seq('(', $._tuple_element, ',', commaSep($._tuple_element), ')'),
    ),

    _tuple_element: $ => choice($._expression, $.pointer_type),

    anonymous_composite: $ => seq('.{', optional($._composite_body), '}'),

    _composite_body: $ => choice(
      seq(
        commaSep1(choice($.field_initializer, $.base_initializer)),
      ),
      commaSep1($._expression),
      seq($._expression, ';', $._expression),
    ),

    field_initializer: $ => seq(
      field('name', alias($.identifier, $.field_identifier)),
      ':',
      field('value', $._expression),
    ),

    base_initializer: $ => seq('..', $._expression),

    // `Name { ... }` and `f(x) { ... }` read as a block after a condition in
    // `nestc`, which does not take them in an `if`, `while`, `for` or `match`
    // head. Here both readings are kept (`conflicts`), and the one that fails
    // is dropped; when both parse, the block wins.
    composite_literal: $ => prec.dynamic(-1, seq(
      field('type', choice(
        $.identifier,
        $.field_expression,
        $.generic_expression,
        $.array_type,
        $.slice_type,
        $.enum_literal,
      )),
      '{',
      optional($._composite_body),
      '}',
    )),

    // A trailing block always follows a call's `)` (spec §5.3): after a bare
    // name, `{` opens a composite literal.
    trailing_closure: $ => prec.dynamic(-1, seq(
      field('function', $.call_expression),
      field('closure', choice($.closure_expression, $.block)),
    )),

    enum_literal: $ => seq('.', field('name', $.identifier)),

    if_expression: $ => prec.right(seq(
      'if',
      field('condition', $._expression),
      field('consequence', $.block),
      optional(seq('else', field('alternative', choice($.if_expression, $.if_match_expression, $.block)))),
    )),

    if_match_expression: $ => prec.right(seq(
      'if',
      'match',
      field('pattern', $._pattern),
      ':=',
      field('value', $._expression),
      field('consequence', $.block),
      optional(seq('else', field('alternative', choice($.if_expression, $.if_match_expression, $.block)))),
    )),

    match_expression: $ => seq('match', field('value', $._expression), field('body', $.match_block)),

    method_match_expression: $ => prec(PREC.postfix, seq(
      field('value', $._expression),
      '.',
      'match',
      field('body', $.match_block),
    )),

    match_block: $ => seq('{', commaSep($.match_arm), '}'),

    match_arm: $ => seq(
      field('pattern', $._pattern),
      optional(seq('if', field('guard', $._expression))),
      '=>',
      field('value', $._expression),
    ),

    unary_expression: $ => prec(PREC.unary, seq(
      field('operator', choice(seq('&', optional('mut')), '-', '!', 'not', '~')),
      field('operand', $._expression),
    )),

    binary_expression: $ => {
      const table = [
        [PREC.or, choice('||', 'or')],
        [PREC.and, choice('&&', 'and')],
        [PREC.compare, choice('==', '!=', '<', '<=', '>', '>=')],
        [PREC.bitor, '|'],
        [PREC.bitxor, '^'],
        [PREC.bitand, '&'],
        [PREC.shift, choice('<<', '>>')],
        [PREC.add, choice('+', '-')],
        [PREC.multiply, choice('*', '/', '%')],
      ];
      return choice(...table.map(([precedence, operator]) => prec.left(precedence, seq(
        field('left', $._expression),
        // @ts-ignore
        field('operator', operator),
        field('right', $._expression),
      ))));
    },

    range_expression: $ => prec.left(PREC.range, choice(
      seq($._expression, choice('..<', '..='), $._expression),
      seq($._expression, '..'),
      seq(choice('..<', '..='), $._expression),
      '..',
    )),

    field_expression: $ => prec(PREC.postfix, seq(
      field('value', $._expression),
      '.',
      field('field', choice(alias($.identifier, $.field_identifier), $.integer)),
    )),

    generic_expression: $ => prec(PREC.postfix, seq(
      field('value', $._expression),
      field('arguments', $.generic_arguments),
    )),

    call_expression: $ => prec(PREC.postfix, seq(
      field('function', $._expression),
      field('arguments', $.arguments),
    )),

    arguments: $ => seq('(', commaSep(choice($._expression, $.named_argument)), ')'),

    named_argument: $ => seq(
      field('name', $.identifier),
      ':',
      field('value', $._expression),
    ),

    index_expression: $ => prec(PREC.postfix, seq(
      field('value', $._expression),
      '[',
      field('index', $._expression),
      ']',
    )),

    dereference_expression: $ => prec(PREC.postfix, seq($._expression, '.*')),

    try_expression: $ => prec(PREC.postfix, seq(
      $._expression,
      field('operator', choice('.?', '.!')),
    )),

    // ===< Patterns >===

    _pattern: $ => choice(
      $.identifier,
      $.mut_pattern,
      $.glob_pattern,
      $.literal_pattern,
      $.range_pattern,
      $.variant_pattern,
      $.destructure_pattern,
      $.tuple_struct_pattern,
      $.tuple_pattern,
      $.slice_pattern,
      $.reference_pattern,
      $.or_pattern,
      $.binding_pattern,
    ),

    mut_pattern: $ => seq('mut', $.identifier),

    glob_pattern: _ => '*',

    literal_pattern: $ => choice(
      $.integer,
      seq('-', $.integer),
      $.float,
      $.boolean,
      $.string,
      $.char,
    ),

    range_pattern: $ => prec.left(choice(
      seq($.literal_pattern, choice('..<', '..='), $.literal_pattern),
      seq($.literal_pattern, '..'),
      seq(choice('..<', '..='), $.literal_pattern),
    )),

    variant_pattern: $ => prec.right(seq(
      '.',
      field('name', $.identifier),
      optional(choice(
        seq('(', commaSep(choice($._pattern, '..')), ')'),
        seq('{', commaSep(choice($.field_pattern, '..')), '}'),
      )),
    )),

    destructure_pattern: $ => seq(
      choice('{', '.{'),
      commaSep(choice($.field_pattern, '..')),
      '}',
    ),

    field_pattern: $ => choice(
      seq(optional('mut'), field('name', $.identifier)),
      seq(field('name', $.identifier), ':', field('pattern', $._pattern)),
      '*',
    ),

    tuple_struct_pattern: $ => seq(
      field('type', $.type_path),
      '(',
      commaSep(choice($._pattern, '..')),
      ')',
    ),

    tuple_pattern: $ => seq('(', commaSep(choice($._pattern, '..')), ')'),

    slice_pattern: $ => seq(
      '[',
      commaSep(choice($._pattern, seq('..', optional($.identifier)))),
      ']',
    ),

    reference_pattern: $ => prec(PREC.unary, seq('&', $._pattern)),

    or_pattern: $ => prec.left(seq($._pattern, '|', $._pattern)),

    binding_pattern: $ => prec(PREC.unary, seq($.identifier, '@', $._pattern)),

    // ===< Literals >===

    boolean: _ => choice('true', 'false'),

    integer: _ => token(choice(
      /[0-9][0-9_]*/,
      /0[xX][0-9a-fA-F_]+/,
      /0[oO][0-7_]+/,
      /0[bB][01_]+/,
    )),

    float: _ => token(choice(
      /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?/,
      /[0-9][0-9_]*[eE][+-]?[0-9_]+/,
    )),

    escape_sequence: _ => token.immediate(seq(
      '\\',
      choice(
        /[nrt0\\'"]/,
        /x[0-9a-fA-F]{2}/,
        /u\{[0-9a-fA-F]+\}/,
      ),
    )),

    string: $ => seq(
      '"',
      repeat(choice(alias(token.immediate(prec(1, /[^"\\]+/)), $.string_content), $.escape_sequence)),
      token.immediate('"'),
    ),

    byte_string: $ => seq(
      'b"',
      repeat(choice(alias(token.immediate(prec(1, /[^"\\]+/)), $.string_content), $.escape_sequence)),
      token.immediate('"'),
    ),

    c_string: $ => seq(
      'c"',
      repeat(choice(alias(token.immediate(prec(1, /[^"\\]+/)), $.string_content), $.escape_sequence)),
      token.immediate('"'),
    ),

    interpolated_string: $ => seq(
      'f"',
      repeat(choice(
        alias(token.immediate(prec(1, /[^"\\{}]+/)), $.string_content),
        $.escape_sequence,
        alias(token.immediate('{{'), $.escape_sequence),
        alias(token.immediate('}}'), $.escape_sequence),
        $.interpolation,
      )),
      token.immediate('"'),
    ),

    interpolation: $ => seq(
      token.immediate('{'),
      $._expression,
      optional($.format_spec),
      '}',
    ),

    // `:>8`, `:?` — the specifier of one hole (§1.5). One token, because it is
    // not Nest syntax: `>8` and `#x` are read as characters by the compiler's
    // lexer, and giving them rules here would be a second, disagreeing grammar
    // for them.
    format_spec: _ => token(seq(':', /[^}"\n]*/)),

    char: _ => token(seq(
      '\'',
      choice(
        /[^'\\\n]/,
        /\\[nrt0\\'"]/,
        /\\x[0-9a-fA-F]{2}/,
        /\\u\{[0-9a-fA-F]+\}/,
      ),
      '\'',
    )),

    identifier: _ => /[_\p{L}][_\p{L}0-9]*/,

    line_comment: _ => token(seq('//', /[^\n]*/)),
  },
});
