; kiln's: a Dockerfile's RUN, CMD and ENTRYPOINT in their shell form read as shell.

((shell_command) @injection.content
  (#set! injection.language "bash")
  (#set! injection.include-children))
