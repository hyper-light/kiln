; kiln's highlights for GraphQL: tree-sitter-graphql comes without any. What's general comes first
; and what's particular after it, as the later pattern wins.

(comment) @comment
(string_value) @string
(description (string_value) @string.documentation)
[(int_value) (float_value)] @number
(boolean_value) @boolean
(null_value) @constant.builtin
(enum_value (name) @constant)
(directive_location) @constant

(field (name) @property)
(field (alias (name) @property))
(object_field (name) @property)
(field_definition (name) @property)
(argument (name) @variable.parameter)
(input_value_definition (name) @variable.parameter)
(variable) @variable.parameter

(named_type (name) @type)
(object_type_definition (name) @type)
(interface_type_definition (name) @type)
(union_type_definition (name) @type)
(enum_type_definition (name) @type)
(input_object_type_definition (name) @type)
(scalar_type_definition (name) @type)

(operation_definition (name) @function)
(fragment_name (name) @function)
(directive "@" @attribute (name) @attribute)
(directive_definition "@" @attribute (name) @attribute)

(operation_type) @keyword
["fragment" "on" "type" "interface" "union" "enum" "input" "scalar" "schema" "extend" "implements" "directive" "repeatable"] @keyword

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
[":" "=" "|" "&" "!" "..."] @operator
(comma) @punctuation.delimiter
