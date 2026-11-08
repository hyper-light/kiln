; kiln's (docs/ci.md W29-R05): a workflow's or an action's `run:` reads as the shell it runs in.
; GitHub's default off Windows is bash; a `shell:` says otherwise, the nearest one first: the step's
; own, else its job's `defaults.run.shell`, else the workflow's. Each `shell:` pattern says
; `kiln.replaces`, so it takes the place of earlier ones in the same text (engine.ts): the nearer
; `shell:` comes later in this file. `kiln.block-scalar` reads a `|` block as YAML does: from the line
; after it, without its indentation.

; Every `run:` that's a script, as bash.
(block_mapping_pair
  key: (flow_node) @_run
  value: [
    (flow_node (plain_scalar))
    (flow_node (double_quote_scalar))
    (flow_node (single_quote_scalar))
    (block_node (block_scalar))
  ] @injection.content
  (#eq? @_run "run")
  (#set! injection.language "bash")
  (#set! injection.include-children)
  (#set! kiln.block-scalar))

; The workflow's `defaults.run.shell`, for each step of each job, before or after its `jobs:`.
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_defaults
    value: (block_node (block_mapping (block_mapping_pair
      key: (flow_node) @_dr
      value: (block_node (block_mapping (block_mapping_pair
        key: (flow_node) @_shell
        value: (flow_node) @injection.language)))))))
  (block_mapping_pair
    key: (flow_node) @_jobs
    value: (block_node (block_mapping (block_mapping_pair
      value: (block_node (block_mapping (block_mapping_pair
        key: (flow_node) @_steps
        value: (block_node (block_sequence (block_sequence_item (block_node (block_mapping (block_mapping_pair
          key: (flow_node) @_run
          value: [
            (flow_node (plain_scalar))
            (flow_node (double_quote_scalar))
            (flow_node (single_quote_scalar))
            (block_node (block_scalar))
          ] @injection.content)))))))))))))
  (#eq? @_defaults "defaults")
  (#eq? @_dr "run")
  (#eq? @_shell "shell")
  (#eq? @_jobs "jobs")
  (#eq? @_steps "steps")
  (#eq? @_run "run")
  (#set! injection.include-children)
  (#set! kiln.block-scalar)
  (#set! kiln.replaces))

(block_mapping
  (block_mapping_pair
    key: (flow_node) @_jobs
    value: (block_node (block_mapping (block_mapping_pair
      value: (block_node (block_mapping (block_mapping_pair
        key: (flow_node) @_steps
        value: (block_node (block_sequence (block_sequence_item (block_node (block_mapping (block_mapping_pair
          key: (flow_node) @_run
          value: [
            (flow_node (plain_scalar))
            (flow_node (double_quote_scalar))
            (flow_node (single_quote_scalar))
            (block_node (block_scalar))
          ] @injection.content)))))))))))))
  (block_mapping_pair
    key: (flow_node) @_defaults
    value: (block_node (block_mapping (block_mapping_pair
      key: (flow_node) @_dr
      value: (block_node (block_mapping (block_mapping_pair
        key: (flow_node) @_shell
        value: (flow_node) @injection.language)))))))
  (#eq? @_defaults "defaults")
  (#eq? @_dr "run")
  (#eq? @_shell "shell")
  (#eq? @_jobs "jobs")
  (#eq? @_steps "steps")
  (#eq? @_run "run")
  (#set! injection.include-children)
  (#set! kiln.block-scalar)
  (#set! kiln.replaces))

; A job's `defaults.run.shell`, for each of its steps, before or after its `steps:`.
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_defaults
    value: (block_node (block_mapping (block_mapping_pair
      key: (flow_node) @_dr
      value: (block_node (block_mapping (block_mapping_pair
        key: (flow_node) @_shell
        value: (flow_node) @injection.language)))))))
  (block_mapping_pair
    key: (flow_node) @_steps
    value: (block_node (block_sequence (block_sequence_item (block_node (block_mapping (block_mapping_pair
      key: (flow_node) @_run
      value: [
        (flow_node (plain_scalar))
        (flow_node (double_quote_scalar))
        (flow_node (single_quote_scalar))
        (block_node (block_scalar))
      ] @injection.content)))))))
  (#eq? @_defaults "defaults")
  (#eq? @_dr "run")
  (#eq? @_shell "shell")
  (#eq? @_steps "steps")
  (#eq? @_run "run")
  (#set! injection.include-children)
  (#set! kiln.block-scalar)
  (#set! kiln.replaces))

(block_mapping
  (block_mapping_pair
    key: (flow_node) @_steps
    value: (block_node (block_sequence (block_sequence_item (block_node (block_mapping (block_mapping_pair
      key: (flow_node) @_run
      value: [
        (flow_node (plain_scalar))
        (flow_node (double_quote_scalar))
        (flow_node (single_quote_scalar))
        (block_node (block_scalar))
      ] @injection.content)))))))
  (block_mapping_pair
    key: (flow_node) @_defaults
    value: (block_node (block_mapping (block_mapping_pair
      key: (flow_node) @_dr
      value: (block_node (block_mapping (block_mapping_pair
        key: (flow_node) @_shell
        value: (flow_node) @injection.language)))))))
  (#eq? @_defaults "defaults")
  (#eq? @_dr "run")
  (#eq? @_shell "shell")
  (#eq? @_steps "steps")
  (#eq? @_run "run")
  (#set! injection.include-children)
  (#set! kiln.block-scalar)
  (#set! kiln.replaces))

; The step's own `shell:`, before or after its `run:`.
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_shell
    value: (flow_node) @injection.language)
  (block_mapping_pair
    key: (flow_node) @_run
    value: [
      (flow_node (plain_scalar))
      (flow_node (double_quote_scalar))
      (flow_node (single_quote_scalar))
      (block_node (block_scalar))
    ] @injection.content)
  (#eq? @_shell "shell")
  (#eq? @_run "run")
  (#set! injection.include-children)
  (#set! kiln.block-scalar)
  (#set! kiln.replaces))

(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run
    value: [
      (flow_node (plain_scalar))
      (flow_node (double_quote_scalar))
      (flow_node (single_quote_scalar))
      (block_node (block_scalar))
    ] @injection.content)
  (block_mapping_pair
    key: (flow_node) @_shell
    value: (flow_node) @injection.language)
  (#eq? @_shell "shell")
  (#eq? @_run "run")
  (#set! injection.include-children)
  (#set! kiln.block-scalar)
  (#set! kiln.replaces))
