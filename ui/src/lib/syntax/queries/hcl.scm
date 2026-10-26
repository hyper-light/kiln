; kiln's highlights for HCL (Terraform, Nomad): tree-sitter-hcl comes without any. What's general
; comes first and what's particular after it, as the later pattern wins.

(comment) @comment
(identifier) @variable

[(string_lit) (quoted_template) (heredoc_template)] @string
(template_literal) @string
[(heredoc_start) (heredoc_identifier)] @punctuation.delimiter
(numeric_lit) @number
(bool_lit) @boolean
(null_lit) @constant.builtin

(attribute (identifier) @property)
(object_elem key: (expression (variable_expr (identifier) @property)))
(get_attr (identifier) @property)
(block (identifier) @type)
(function_call (identifier) @function)
(for_intro (identifier) @variable.parameter)
(template_for_start (identifier) @variable.parameter)

((variable_expr (identifier) @variable.builtin)
  (#any-of? @variable.builtin "var" "local" "module" "data" "path" "terraform" "each" "count" "self"))

(template_interpolation) @embedded
[(template_interpolation_start) (template_interpolation_end)] @punctuation.special
[(template_directive_start) (template_directive_end)] @punctuation.special
(strip_marker) @punctuation.special

["for" "endfor" "in" "if" "else" "endif"] @keyword

[(block_start) (block_end) (object_start) (object_end) (tuple_start) (tuple_end) "(" ")" "[" "]"] @punctuation.bracket
["." ".*" "," "[*]"] @punctuation.delimiter
(ellipsis) @punctuation.special
["!" "!=" "%" "&&" "*" "+" "-" "/" "<" "<=" "=" "==" "=>" ">" ">=" "?" ":" "||"] @operator
