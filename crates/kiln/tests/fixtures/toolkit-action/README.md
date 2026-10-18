# F-20: the toolkit conformance action

docs/ci.md W06-AC01. `src/` is bundled into `dist/` with `@actions/core` 3.0.1, `@actions/exec`
3.0.0 and `@actions/io` 3.0.2:

```
esbuild src/main.mjs src/post.mjs --bundle --platform=node --target=node20 --format=esm \
  --splitting --minify --chunk-names=shared --outdir=dist --out-extension:.js=.mjs --legal-comments=none \
  "--banner:js=import{createRequire as __kilnCR}from'module';const require=__kilnCR(import.meta.url);"
```

`crates/kiln/src/runner/runtime.rs`'s `toolkit_conformance` test runs it on the host executor.
