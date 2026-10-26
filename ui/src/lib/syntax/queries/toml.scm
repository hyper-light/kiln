; kiln's, after tree-sitter-toml's own: a pair's key is a property (its query colours every bare
; key as a table's name, and captures the pair itself as the property).

(pair (bare_key) @property)
(pair (quoted_key) @property)
(pair (dotted_key (bare_key) @property))
