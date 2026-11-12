# pi's built-in Agentfile, ported onto main's design

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 28: pi's Agentfile embedded in kiln. Step 28's
agent finished its work in 43eaaceab, on branch p/agentfile-pi-v2, and then stopped at a weekly usage
limit. Its branch predated main's design, so it couldn't merge as it was. This port brings 43eaaceab
onto kiln main (e3f137930), whose table, pins and one unpacker have moved since. It is the step's
evidence record for the port. It lists every record, what stayed unverified, and how each of Shards'
rules R01 to R16 applies.

## POLICY_READ

[POLICY_READ.json](POLICY_READ.json):
- kiln origin/main ed0c9d3bd: AGENTS.md `16b05ef8…`, CLAUDE.md (`@AGENTS.md`) `336cc4fb…`, and
  docs/ci.md §0 `be867453…`. These files are unchanged at e3f137930 and at 3a4b09005.
- Shards origin/main c2c4547c1: docs/development-rules.md `df2acf47…` (R01-R16), AGENTS.md
  `f66d304c…`, and docs/development-evidence.md `61613a24…`.
- Role: implementer. ROOT, the coordinator, is session g121-46 (kiln agent
  01a148884a4770fdaf0111f3f792a248), and ROOT's rule for this work is that the implementer merges to
  kiln main itself.
- Owned paths: `.kiln/images/harness-pi/`; pi's row and test module in
  `crates/kiln/src/agentfiles/builtin.rs`; `builtin/pi_tests.rs`; and this directory.

## What changed from 43eaaceab, and why

- **pi's row.** 43eaaceab's row set `smoke: Smoke {...}`, a field `Builtin` no longer has, because
  346929ba5 moved every harness's smoke into its pin.json. Here pi's row is main's shape, after
  Codex's, and every row on main is kept.
- **pi's smoke** is in its pin, in main's format, and pin.sh writes it as every harness's pin.sh
  writes its own: `{"program": "pi", "args": ["--version"], "stdout": "1.5.2"}`.
  - `pi` is the executable at the harness directory's root. It is the tarball's `pi/pi`, which
    pin.sh requires, with `pi/` stripped.
  - `stdout` is the line without its newline. The bytes 1.5.2's x64 build wrote were 31 2e 35 2e
    32 0a: step 2's step2-run.sh checks them, and this port's live test checks them again.
- **The whole-tree check.** 43eaaceab widened the pin checks that builtin.rs's tests shared at the
  time. 346929ba5 split those checks into each harness's own test file, so main has no shared pin
  check in builtin.rs. The check that every pin meets on main is the fetcher's: before it fetches
  anything, it reads each archive's `unpack` the way the one unpacker does (step 26's
  `unpack::Spec`). So pi's check, in [builtin/pi_tests.rs](../../../../crates/kiln/src/agentfiles/builtin/pi_tests.rs),
  reads each `unpack` with `unpack::Spec` itself:
  - an `unpack` holds a member's fields or a tree's, every field its form needs and none of the
    other form's, and no other field;
  - its refusals come in the unpacker's own words.
  pi's directory then needs more of each form:
  - a tree sits at the harness root (`.`) or a folder inside it, with no mode, and both totals
    above 0;
  - a member is a file inside the directory with its permissions, from an entry inside the
    archive's folder, with that entry's own SHA-256.
- **pi's tests** are in builtin/pi_tests.rs, as each harness's tests are in its own file. Each test
  also hands its check altered copies, and each copy must fail it. They cover:
  - its pin and smoke;
  - its harness file, at the pin's version and on the pin's base;
  - credentials and pi's auth.json;
  - `pi --mode rpc`, as kiln's Pi kind starts it, with PI_CODING_AGENT_DIR on its volume;
  - the plan's nine-line Agentfile, with rg and fd at bin/.
  A live test, gated by `KILN_TEST_LIVE_CONTEXT` as Claude Code's and Antigravity's are, has kiln's
  fetcher and the one unpacker make pi's real context for both platforms. It then runs this
  machine's own pi with `--version` alone.

## Records

[run.sh](run.sh) runs the light tests from a git bundle of this branch over kiln main. Its steps:
- `pin`: pin.sh run again for v1.5.2, 15.2.0 and v10.5.2;
- `unit`: the agentfiles module's tests;
- `live`: pi's live context and `--version`;
- `api`: the built-ins on the API's list;
- `shards`: the real Shards' static check of every built-in;
- `clippy`.

Each record names its host, toolchain, revision and the files' SHA-256. Every run was at
da5e752be, the tree landed on kiln main as edba77bdb with main's ui/ and docs/ changes merged in.

- [e_linux.txt](e_linux.txt): adacachy (Linux x86_64, cargo 1.99.1), every step passed.
  - `pin`: pin.sh's pin.json is the committed one, read_at aside, and has the smoke in the same
    place.
  - `unit`: 93 passed, 0 failed.
  - `live` made pi's context for both platforms:
    - pi's tree met its pinned totals as the unpacker counts them;
    - bin/rg and bin/fd matched their members' SHA-256 at 0755;
    - the amd64 `pi --version` wrote 31 2e 35 2e 32 0a, with 0 bytes on its standard error.
  - `api`: the API's list test passed.
  - `shards`: shards v0.7.0's release binary, checked against its SHA256SUMS, checks every
    built-in.
  - `clippy -p kiln --all-targets -D warnings` is clean.
- [e_macos.txt](e_macos.txt): the Mac Studio adas-mac-studio.local-2 (macOS arm64). Passed:
  - `unit`: 93 passed;
  - `clippy`: clean;
  - `live`: pi's context made on APFS, with rg and fd matching.
  The first try's download stopped at 53 MB for 16 minutes (below), and the second passed in
  219 s. Then `api` passed, and `shards` passed with ROOT's recorded Shards main build
  (ac87190d3, `~/.cache/s28-shards-ac87190d3/shards` on that Mac).
- [e_case.txt](e_case.txt): pi's two pinned tarballs, 234 entries each, as the pin's `entries`
  says. None of their names collide with case ignored, so the tree unpacks the same on APFS and
  NTFS.

## Not verified here, or owed

- Clippy on Windows: ROOT's, on the landed revision (jupiter had no idle core).
- arm64 pi `--version`: there is no arm64 Linux host here. ROOT's smoke runs it in a microVM.
- The image build, the smoke in a microVM and harness turns are ROOT's.
- pin-tests.sh, step 28's mutation tests of pin.sh, was not run again. Its refusal paths are
  unchanged: the writer gained the smoke alone, and `pin` checked the writer's output.

## Seen on the way, not changed

- **The fetcher hangs on a stalled download.** On the Mac Studio, pi's tarball stopped arriving
  at about 53 MB, and the fetcher waited 16 minutes with no error. It would have waited longer:
  the run was stopped there. `context::fetch` gives its request DEFAULT_TIMEOUT (30 s), yet
  nothing ended the wait on a body that had stopped.
- **Devin's pin isn't in a form the unpacker takes.** Its pin.json's archives have
  `unpack: {"format": "tar.gz"}` alone, which is neither of the one unpacker's forms
  (`Why::Neither`). By the code, not run here: `kiln agentfiles context devin` is refused by
  `context::spec` before it fetches anything.

## R01-R16

| Rule | How it applies |
|---|---|
| R01 | No production path changed beyond the table's const row, made of compile-time `include_str!`. The tests use `ensure!` and `?`, with no unwrap, expect or indexing; clippy is clean on Linux and macOS. |
| R02 | No new design. The smoke's shape is main's (346929ba5), and the tree semantics are the one unpacker's own `unpack::Spec` (step 26). |
| R03 | Every literal is measured. The smoke line is from pi's bytes, measured in step 2 and again live here. The tree totals are pin.sh's measurement of the archives. Each test's expected layout and Agentfile cite the step's research findings. |
| R04 | No state machine was touched. |
| R05 | No wait or cache was touched. The fetcher's unbounded wait on a stalled body is reported above, not changed. |
| R06, R07, R13 | One check per test, and comments only for sources and non-obvious rules. |
| R08, R14 | No performance is claimed. |
| R09, R10 | The live runs and the altered copies in each test are the evidence, all recorded here. |
| R11, R12 | Linux x86_64 and macOS arm64 ran here. Windows clippy and arm64 Linux are owed to ROOT, as above. |
| R15 | The implementer's light tests only. The implementer merged to kiln main itself, as the person's rule for this work (2026-11-12) has it. |
| R16 | The work folders on adacachy and the Mac Studio (~/.cache/s28-port) are removed once these records are in. |
