# Plan step 20 [kiln], step 2: the Agentfile, harness.json and harness-codex's entry (evidence)

Plan 01a1ce705d7076648c1d9f9ee26214d3 (step 20 [kiln], the Codex Agentfile), step 2 of 6. This
records what step 2 built, what it ran and what it found. Step 6 joins it into the step's evidence
record. Policy readback as in step1.md (rechecked 2026-11-12).

## What it is

- `.kiln/images/harness-codex/Agentfile`, in this order:
  - global `ARG BASE` (step 19's buildpack-deps:trixie-scm, pinned by digest) and `FROM ${BASE}`;
  - `ARG TARGETARCH` and `HARNESS codex FROM ./codex-${TARGETARCH}`;
  - the bring-up's three provider grants (Shards' docs/examples/harness-codex, lines 20-22);
  - `VOLUME --target-kind=harness /workspace FOR codex` and `VOLUME --target-kind=harness
    /home/codex/.codex FOR codex`.
  - No ENV, no RUN. Comments say why each line is there.
- `.kiln/images/harness-codex/harness.json`, the full OSI form: `{"schemaVersion": 1, "name": "codex",
  "version": "0.167.2", "run": {"command": ["codex", "app-server", "-c",
  "check_for_update_on_startup=false"], "env": ["CODEX_HOME=/home/codex/.codex"]}}`.
- `crates/kiln/src/agentfiles/builtin.rs`: `ALL` holds harness-codex beside Goose's, with its
  Agentfile, pin.json and harness.json by `include_str!`. The table is main's: Goose's step (23) landed
  `Builtin`'s `pin` and `files` and kiln's Dockerfile copy of `.kiln/images/` first (kiln main
  263cad6b4). This branch was rebased onto it, so it adds the entry and nothing of the struct.
  `.dockerignore`'s comment now names `.kiln/images/` too, as step 22's branch has it.
- `crates/kiln/src/agentfiles/builtin/codex_tests.rs`, beside main's `goose_tests.rs`. The table's own
  tests also run with the entry: `every_builtin_is_a_valid_agentfile_once`, the Dockerfile check
  `step23_kilns_dockerfile_copies_what_builtins_embed`, and, given a Shards, `step23_a_real_shards_checks_every_builtin`.
  - the pin: well formed, both platforms once each, arm64's rg in the package's place, the smoke, the
    verification record;
  - the Agentfile: exactly its instructions;
  - harness.json: kiln's command for Kind codex (`Kind::get("codex")`, `codex::args()`) plus the update
    check off, and CODEX_HOME alone, in the Agentfile's volume;
  - a secrets scan of the embedded files.
  - Each check also gets changed copies, which must fail it.
- The version smoke kiln checks a built image by is in pin.json, as steps 21 and 22 keep theirs:
  `{"program": "bin/codex", "args": ["--version"], "stdout": "codex-cli 0.167.2"}`. pin.sh writes it
  from the version it verified. Step 19's image() isn't written yet (its step 5), so this is what it
  reads.

## The pin again, and what the ELF survey found

pin.sh now writes the smoke, and each package tree's totals (`unpacked_bytes`, `entries`), which the
shared unpack holds a tree to under step 27's conversion of step 25's module. Step 3's agent found that
contract refuses a tree without them. So the pin ran again on kiln-a094a12d
(`scripts/run-step2-pin.sh OUT rust-v0.167.2`, 2026-11-12T15:18:14Z):

- record: `evidence/e_step2_pin.txt`, pin.sh sha256 f20720e8…;
- the same tag and the same three archives;
- each package 52 entries (44 files and 8 directories): 540223046 bytes unpacked on amd64 and
  481289300 on arm64, arm64's skipped codex-path/rg counted, as the archive's own totals;
- MUTATIONS passed=21 failed=0;
- FRESH archives=3 matching=3 members=1 matching=1 trees=2 matching=2. A fresh download's entries and
  bytes, counted again, are the pin's.

The survey (`scripts/elf-survey.sh`, `evidence/e_step2_elf.txt`, 32 ELF files per package) shows the
package isn't all static, as research finding f10 had it:

- Static: bin/codex, bin/codex-code-mode-host, bin/codex-exec-server and codex-resources/bwrap on
  both arches, and codex-path/rg on amd64.
- Needs glibc's loader: arm64's codex-path/rg, as found before.
- Also glibc builds: codex-resources/voice/bin/codex-voice-host (`/lib64/ld-linux-x86-64.so.2`,
  `/lib/ld-linux-aarch64.so.1`) and the 26 shared libraries beside it (they need libc.so.6).

OpenAI builds the voice resources for the `-gnu` target even in a `-musl` package: its
.github/scripts/build-codex-package-archive.sh at the tag, lines 209-213 (`voice_target="${target%-musl}-gnu"`,
`evidence/pages/`). kiln never starts them: its Codex adapter refuses `/voice` (crates/kiln/src/harness/codex.rs,
`no("voice", …)`) and asks Codex for no voice session.

So:

- pin.sh's static check, which named five programs, now reads every ELF file of a package. A file that
  asks an interpreter is refused, unless it is under codex-resources/voice/ (named in a NOTE) or is the
  rg the pin replaces. Before this, a new dynamic helper anywhere else would have passed.
- A new mutation, `crafted: dynamic` (bin/codex-helper asking `/lib/ld-musl-x86_64.so.1`), is refused
  by name by `pin.sh inspect`.
- The Agentfile's comment says the voice resources are glibc builds that kiln never starts.

## Tests

All ran on kiln-a094a12d (Fedora, Linux 6.19.10-300.fc44.x86_64, x86_64), 2026-11-12. The worktree
won't sync to a runner (`did not answer the sync in 60s`), so `scripts/run-step2-tests.sh` takes kiln
at c09d23207 (this branch's base) from the runner's own clone by `git archive`, read only. It lays
this step's eight changed files over it from a tar.gz (sha256 94a8a2d7…; each file's SHA-256 is in the
record), and runs as .kiln/ci.yaml's rust job does (`CARGO_PROFILE_DEV_DEBUG=line-tables-only`,
`RUST_TEST_THREADS=4`; 8 build jobs).

- `evidence/e_step2_tests.txt`:
  - `cargo clippy --locked -p kiln --all-targets -- -D warnings` exit 0 (6m 33s). The only warnings
    are vendor/focal-bus's focal-memory, a vendored dependency (`fetch_update`, renamed `try_update`),
    not this change's.
  - `cargo test --locked -p kiln --lib agentfiles` exit 0: 23 passed, among them the four
    codex_tests and `every_builtin_is_a_valid_agentfile_once` with harness-codex in `ALL`.
- `evidence/e_step2_mutants.txt` (`scripts/run-step2-mutations.sh`, the same tree, each change alone
  and then the file restored and touched): MUTANTS passed=2 failed=0.
  - `codex::args()` given `--strict`: `codex_harness_json_runs_kilns_command_with_codex_home_in_its_volume`
    FAILED;
  - `ENV OPENAI_API_KEY=x` in the embedded Agentfile: `codex_embedded_files_hold_no_credential` and
    `codex_agentfile_is_exactly_its_base_harness_grants_and_volumes` FAILED;
  - the untouched tree again: 4 passed.
- Each test's own changed copies (all refused), in `codex_tests.rs`:
  - the pin: a 63-character digest, a missing platform, a member path with `..`, a file path with
    `..`, the package's rg not skipped, mode 0777, another smoke, another tag's identity;
  - harness.json: other arguments for Kind codex, a CODEX_HOME outside the volume, another version;
  - the Agentfile: a RUN, an ENV, another base, a fourth grant, no CODEX_HOME volume;
  - the secrets scan: `ENV OPENAI_API_KEY=x`, `ARG CODEX_TOKEN`, an `sk-` key, auth.json.

### At the rebased head: ce8ad3845, on kiln main 263cad6b4

`scripts/run-step2-tests-at.sh` (`evidence/e_step2_tests_head.txt`, kiln-a094a12d, 2026-11-12T15:21-15:44Z)
works in a `--shared` clone of the runner's kiln checkout, the branch fetched from a 233 KB git
bundle, at the commit itself. At its end the tree was clean.

- clippy -D warnings on `-p kiln --all-targets`: exit 0.
- `--lib agentfiles`: 30 passed. These include Goose's tests, the four codex_tests, and the table's own
  (every built-in once; the Dockerfile copies what built-ins embed).
- `--lib real_shards` with `KILN_TEST_SHARDS`: 2 passed. Every built-in, Codex's among them, passes a
  real Shards' static check (v0.7.0, sha256 a9c9092a…, as its SHA256SUMS lists it).
- `--test agentfiles`: 1 passed. `step30_agentfiles_on_every_surface` lists Goose's and Codex's
  built-ins, through this branch's 5e296cac2.
- M1 (`codex::args()` given `--strict`) and M2 (`ENV OPENAI_API_KEY=x` in the Agentfile): each exit 101,
  their tests FAILED.
- **kiln main 263cad6b4 itself fails** `step30_agentfiles_on_every_surface` (exit 101): the test holds
  the list to `[]`, and Goose's entry is on main. 5e296cac2 (step 20's step-2 subagent's a690a02c5,
  the same lines as step 25's s25-devin-row) fixes it. ROOT was told; mail was refused (429), so a
  resend runs until it is accepted.

## What changed from the plan's text, and why

- The smoke's expectation lives in pin.json, as steps 21 and 22 keep theirs, since step 19's image()
  doesn't exist yet.
- The pin's shape is step 25's, as step1.md says. arm64's rg replacement waits on step 25's
  coordinator: a tree `skip` or a per-file `replace`.
