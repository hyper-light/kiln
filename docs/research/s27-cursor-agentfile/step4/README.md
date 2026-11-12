# Step 4's harness, prepared for ROOT and not run

Plan 01a1ce705e5c7504b88732f443d99288, step 4 (both platforms built, the smokes printing the version), is not step
27's to run: image builds, smoke runs, harness turns and runs on KVM, HVF or WHP hosts are ROOT's (the person's
rule, ROOT's goal for step 27, 2026-11-12). These scripts are step 19's step-5 harness for Claude Code (kiln main,
`docs/research/s19-claude-agentfile/step5/`) made Cursor's, for that run. **None of them has been run.**

- `smoke.sh SHARDS KILN KSRC WORK [all|arch]`: Cursor's context by kiln's own fetcher (it needs the one unpacker
  on main: `../step2/record.md`), one image for linux/amd64 and linux/arm64 and its OCI index, the version smoke
  (`/harness/cursor/cursor-agent --disable-auto-update --version` printing pin.json's version and a newline), the
  real image's process record held to harness.json; with `all`, the probe and its control, debian:trixie-slim
  (must pass the smoke) and Alpine 3.24.4 (must fail to start: cursor-agent is a bash script, and Cursor's node
  needs the glibc loader), and the credentials scan with its `ENV CURSOR_API_KEY` mutation.
- `probe.sh`: the probe's text, run as `sh -c` by a test-only harness.json carrying the real run.env.
- `scan.py`: step 19's checks made Cursor's. Its context check measures each platform's directory as the
  unpacker counts a tree (directories and regular files, the stripped `dist-package/` being the directory
  itself, harness.json aside; bytes, the regular files' sizes), not with `du -b`, which counts directories'
  own sizes. Its credentials scan takes step 3's patterns and a JWT's shape (RFC 7519) for a token, and passes
  over the names in Cursor's own files: those whose SHA-256 equals one of the trees the fetcher unpacked from
  the pinned archives.

Step 19's `selftest.py`, `run.sh` and build scripts carry over as they are; `selftest.py` would need Cursor's
inputs for the changed `context`, `secrets` and `homedir` checks. A shards with 997f4e47e (run.env sets HOME) is
needed: on kiln-a094a12d, step 19's build of Shards main 7ad28fab1 (`shards version 0.7.0, build 7ad28fa`,
sha256 9b460c47ca38dbc5f8c1bde704a3ef5a72692e3788d93d41ef3f52ecf37d89f7) holds it.
