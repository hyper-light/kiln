; kiln's, after tree-sitter-sql's own: its numbers are matched with Lua's `%d`, which a regular
; expression reads as the letters "%d", so they read as strings. These read them as numbers.

((literal) @number
  (#match? @number "^[-+]?\\d+$"))
((literal) @number
  (#match? @number "^[-+]?\\d*\\.\\d*$"))
