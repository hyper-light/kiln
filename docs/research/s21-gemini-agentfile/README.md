# Plan step 21: Gemini CLI's built-in Agentfile (the record)

kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 21: the Gemini CLI Agentfile is embedded in kiln.
Its own plan is 01a1ce705e2374dfbac201fe6ba98184, its research 01a1ce705e2472df9f3dd81be0531c9f
(f1-f37). The first two agents researched it and pinned it ([step1.md](step1.md), d8a8614b5); ROOT's
subagent finished it on branch p/s21-finish-v1. This file is the finish's record.

## Identity

- Role: implementer (ROOT's subagent; ROOT is kiln agent 01a148884a4770fdaf0111f3f792a248).
- POLICY_READ, 2026-11-12: kiln origin/main ed0c9d3bd, AGENTS.md sha256 16b05ef8…447a4c, CLAUDE.md
  (@AGENTS.md) 336cc4fb…ab49, docs/ci.md (§0) be867453…4baa; Shards origin/main d35c4cf79, AGENTS.md
  f66d304c…db41, docs/development-rules.md df2acf47…3d3 (R01-R16), docs/development-evidence.md
  61613a24…fb63. All read in full. The first agents' receipt is [POLICY_READ.json](POLICY_READ.json).
- Owned paths: `.kiln/images/harness-gemini/`, Gemini's row in `crates/kiln/src/agentfiles/builtin.rs`,
  `crates/kiln/src/agentfiles/builtin/gemini_tests.rs`, Gemini's note in `docs/agents.md`, this folder.

## What it is

- `.kiln/images/harness-gemini/`: the Agentfile (Cursor's shape: the base by digest, `HARNESS gemini
  FROM ./gemini-${TARGETARCH}`, the bring-up's three 443 grants with every reach named, `/workspace`
  and `/home/gemini`), harness.json, system-settings.json, pin.json, pin.sh and pin_test.sh.
- Its row in kiln's one table (`builtin.rs`), after Codex's, every other row kept.
- `builtin/gemini_tests.rs`: the pin, the harness file, the system settings, the Agentfile and
  credentials held to the plan and to Kind `gemini`, each test failing on altered copies; and a
  live test of the fetched context, run where `KILN_TEST_LIVE_CONTEXT` names a folder.
- Kind `gemini` was on kiln main already (`program: "gemini"`, `Protocol::Acp(&["--acp"])`).

## Decisions

| Decision | Why (finding, source) |
|---|---|
| The npm package on Node.js | Google ships no Linux executable; the npm package is its Linux build (f5, f6). |
| Node derived, not chosen: v24.22.3 | The newest major Gemini 0.68.0's Linux CI tests (20, 22, 24) that Node still supports (24, to 2028-04-30), at its newest release (f9, f10; R03). |
| The npm tarball as a tree at `.`, `package/` stripped | The one unpacker's tree form (unpack.rs, steps 26 and 27); its 449 files land at the harness's root, beside nothing kiln writes. |
| Node's `bin/node` and `LICENSE` as members at `node/` | Node's archive holds three symlinks the unpacker refuses in a tree (f15); `node/` is off the PATH Shards starts a harness with, its directory then its `bin/` (f28, f37). The LICENSE keeps Node's notice with its binary. |
| No libc variant | Node publishes glibc builds alone for both platforms (no arm64 musl build, f11), as Cursor's and pi's pins have none; one smoke then fits every build. |
| harness.json: `node gemini.js --acp` by absolute path | Kind `gemini`'s arguments (f17); absolute, since the script's path must not depend on the harness's working directory. |
| `HOME=/home/gemini`, a whole-home volume | Gemini keeps everything under `$HOME/.gemini` (f20, f21); the image is read-only, so HOME must be a volume (step 7's shape, as Claude's, Cursor's and agy's). |
| `GEMINI_CLI_SYSTEM_SETTINGS_PATH` and system-settings.json | The update check and its installs, interactive only, are off in every mode by Gemini's own system settings (f23); ACP mode checks for none (bundle: `checkForUpdates` is called from `startInteractiveUI` alone). |
| The smoke: `node/bin/node /harness/gemini/bundle/gemini.js --version` printing `0.68.0` | main's `{program, args, stdout}`; the script by its path in the image, where a smoke runs (`shards run vm … /harness/<id>/<program>`). |

## What ran

- **pin.sh** on adacachy (Linux 7.2.9-2-cachyos x86_64), 2026-11-12T21:15:43Z: Gemini CLI 0.68.0
  (npm's latest) on Node.js v24.22.3, signer 5BE8A3F6C8A5C01D106C0AD820B1A390B168D356, keyring
  nodejs/release-keys 481637f8. pin.json as committed.
- **pin_test.sh** on adacachy, 2026-11-12T21:58:42Z ([finish/pin-test-adacachy.log](finish/pin-test-adacachy.log)):
  - A: 3 of 3 distinct pinned files match a fresh download's SHA-256 and size; 6 of 6 pinned members
    and trees are what GNU tar 1.35 finds in them (each member's own SHA-256 and size; the tree's 449
    entries and 99,143,752 bytes).
  - B: the control and the first agents' eight refusals, 9 of 9.
  - C: npm's record re-signed by the fixture's own key, so the tarball can change and still verify:
    the control pins, and each of 12 changes the unpacker would refuse is refused at pin time, naming
    its check (a link, a path outside package/, a path twice, names that fold for a file and for a
    directory, harness.json, system settings and node/ where kiln writes, sticky and set-ID bits, a
    contiguous file, a file the strip leaves no name), with no pin.json written. 22 of 22 in all.
- **kiln's agentfiles tests** on adacachy at 6e814bff8 (kiln main 3a4b09005 merged in), as a systemd
  user unit at Nice 10 ([finish/tests-adacachy.txt](finish/tests-adacachy.txt)): `cargo test --locked
  -p kiln --lib agentfiles::`, 95 passed, 0 failed, Gemini's 8 among them (its live test skipped there),
  with every other row's, the table's own, the fetcher's and the unpacker's.
- **Live, real Shards and docs** on adacachy, umask 0022 ([finish/live-context-adacachy.txt](finish/live-context-adacachy.txt)):
  - `step21_a_live_gemini_context_holds_the_pinned_builds` with `KILN_TEST_LIVE_CONTEXT`: the fetcher
    and the one unpacker made both platforms' directories from npm and nodejs.org in 12 s; 4 of 4
    members hash to their pins, each tree is 449 files of 99,143,752 bytes, with no link, every
    directory 0755 and no file group- or other-writable.
  - `real_shards` with Shards v0.7.0 (sha256 a9c9092a…, as its SHA256SUMS lists it): 2 passed, every
    built-in Gemini's among them passing Shards' static check, as
    [finish/shards-check-v0.7.0.txt](finish/shards-check-v0.7.0.txt) shows for Gemini's alone.
  - `docs_agents_md` (the metrics table this note sits beside): passed.
- **kiln's clippy gate** (`ci/gate.sh -p kiln clippy`: `cargo clippy --locked -p kiln --all-targets --
  -D warnings`) on adacachy: at 6e814bff8 it failed on one lint of this branch's
  (`explicit_auto_deref` in gemini_tests.rs), fixed in 74c0d233c; at d78d882a6, kiln main edba77bdb
  merged in (pi's row), `GATE: PASS` ([finish/merged-tree-adacachy.txt](finish/merged-tree-adacachy.txt)).
- **The merged tree d78d882a6**, every row and test module kept: `cargo test --locked -p kiln --lib
  agentfiles::`, 101 passed, 0 failed, Gemini's 8 and pi's among them.
- Gemini itself was never run here: smokes are ROOT's.

## Unverified, and owed

- ROOT's: the image built for both platforms by a real Shards, and its smokes (amd64 under KVM, arm64
  on a Mac Studio); that Gemini honors the system settings there (no `Skipping system settings file`
  on stderr); Windows hosts; the two node processes' per-VM memory (R14); the clippy gate on Windows
  at the landed revision. macOS's clippy gate: see the landing's report (the worktree's sync to the
  Mac Studio timed out; a carrier run went to adas-mac-studio-2.local).
- Node's archive's member checks were run on the real archives alone: changing Node's archive breaks
  its signed SHA-256, and no fixture signing key was made for it.
- kiln writes a built-in's own files (harness.json, system-settings.json) under its umask; a context
  made under umask 002 makes the settings group-writable and Gemini skips them (kiln's runners run at
  022). The fetcher also downloads Node's archive once per member. Both are the fetcher's to change.

## R01-R16

| Rule | This step |
|---|---|
| R01 | No production code: a table row of `include_str!`s and tests that return errors, no unwrap or indexing (kiln's denied lints hold, clippy above). pin.sh refuses every malformed input with a message and writes nothing. |
| R02 | Research 01a1ce705e2472df9f3dd81be0531c9f (f1-f37, npm's, Node's and Gemini's own sources) and the bundle read for the update check's one caller; the one unpacker reused, not another written. |
| R03 | Node derived from Gemini's CI and Node's schedule; every literal in pin.sh and the tests named with its source (`/harness/gemini`: Shards' HARNESS layout, f28; the members' modes: an executable and a text; the 443 grants: the bring-up's). |
| R04 | pin.sh: pin.json written whole or a named refusal and none. The tests read texts; no state machine. |
| R05 | No cache, no wait added; pin.sh's work directory is removed on exit. |
| R06, R07, R13 | One check per concern; comments give sources and reasons. |
| R08, R14 | No performance claim. The live context's 12 s and the trees' sizes are single observations. |
| R09 | The one defect found, the clippy lint, came from kiln's own gate and was fixed there. |
| R10 | Logs under finish/; every pin-time refusal exercised by a change that alters one thing; each Rust test fails on its altered copies. |
| R11 | linux/amd64 and linux/arm64 pinned and each one's context made; glibc only, with why; tests and clippy on Linux x86_64; Windows' clippy is ROOT's; Gemini run nowhere. |
| R12 | The one fetcher and unpacker, the one table; pin.sh in the shape of its siblings'. |
| R15 | Implementer scope: unit and light integration tests; smokes and real-VM runs are ROOT's. |
| R16 | Nothing reclaimed; this step's build folders on adacachy are under ~/.cache/s21-finish. |
