; kiln's, after tree-sitter-rust's own: a name in capitals is a constant (its query says so before
; it says any capitalised name is a constructor, and the later pattern wins).

((identifier) @constant
  (#match? @constant "^[A-Z][A-Z\\d_]+$"))
