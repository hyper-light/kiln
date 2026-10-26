; kiln's highlights for Perl: tree-sitter-perl comes without any. What's general comes first and
; what's particular after it, as the later pattern wins.

[(comments) (pod_statement) (data_not_for_compiler)] @comment

[
  (string_single_quoted)
  (string_double_quoted)
  (string_q_quoted)
  (string_qq_quoted)
  (backtick_quoted)
  (command_qx_quoted)
  (word_list_qw)
  (heredoc_body_statement)
] @string
(escape_sequence) @string.escape
[(pattern_matcher) (pattern_matcher_m) (regex_pattern_qr) (substitution_pattern_s) (transliteration_tr_or_y)] @string.regexp
[(heredoc_initializer) (heredoc_end_identifier)] @punctuation.delimiter

[(integer) (floating_point) (scientific_notation) (hexadecimal) (octal) (version)] @number
[(true) (false)] @boolean
(special_literal) @constant.builtin

[(scalar_variable) (array_variable) (hash_variable) (package_variable) (typeglob)] @variable
(special_scalar_variable) @variable.builtin
(super) @variable.builtin
(interpolation) @embedded

[(package_name) (module_name)] @module
(function_definition name: (identifier) @function)
(call_expression_with_bareword function_name: (identifier) @function.call)
(method_invocation function_name: (identifier) @function.method)
(use_constant_statement constant: (identifier) @constant)
(hash_access_variable key: (identifier) @property)
(hash_access_variable_simple key: (identifier) @property)
(label label: (identifier) @label)
(loop_control_statement label: (identifier) @label)
(file_handle) @variable.builtin

[
  "my" "our" "local" "state" "sub" "package" "use" "no" "require" "return" "if" "elsif" "else" "unless" "while" "until"
  "for" "foreach" "last" "next" "redo" "goto" "when" "bless" "constant" "parent" "feature" "isa" "method"
] @keyword
(loop_control_keyword) @keyword
["and" "or" "not" "xor" "eq" "ne" "lt" "gt" "le" "ge" "cmp"] @keyword.operator
["q" "qq" "qw" "qx" "qr" "m" "s" "tr" "y"] @keyword

[(arrow_operator) (fat_comma)] @operator
(normal_comma) @punctuation.delimiter
