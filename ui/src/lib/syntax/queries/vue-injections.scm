; kiln's, after tree-sitter-vue's own: a <style> that names no language is CSS (its query injects
; CSS only where `lang="css"` says so).

(style_element
  (raw_text) @injection.content
  (#set! injection.language "css"))
