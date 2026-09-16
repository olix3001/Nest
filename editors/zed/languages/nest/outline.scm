; What the outline panel and breadcrumbs list: every named binding at item
; level, with what it binds, and every `impl`.

(declaration
  (const_binding
    pattern: (identifier) @name
    value: (func_expression
      "func" @context))) @item

(declaration
  (const_binding
    pattern: (identifier) @name
    value: [
      (struct_type "struct" @context)
      (enum_type "enum" @context)
      (trait_type "trait" @context)
      (namespace_expression "namespace" @context)
      (distinct_type "distinct" @context)
    ])) @item

(declaration
  (directive
    name: (directive_name) @context)?
  (static_declaration
    name: (identifier) @name)) @item

(impl_block
  "impl" @context
  type: (_) @name
  ("for" @context
    target: (_) @name)?) @item
