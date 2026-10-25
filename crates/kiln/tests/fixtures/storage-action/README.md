# W08-AC01: the storage clients action

docs/ci.md W08-AC01. `src/` is bundled into `dist/` with `@actions/core` 3.0.1, `@actions/cache`
6.3.0 and `@actions/artifact` 6.3.1:

```
esbuild src/main.mjs --bundle --platform=node --target=node20 --format=esm --minify \
  --outfile=dist/main.mjs --legal-comments=none \
  "--banner:js=import{createRequire as __kilnCR}from'module';const require=__kilnCR(import.meta.url);"
```

`crates/kiln/tests/storage.rs` runs it on the host executor against a test server.
