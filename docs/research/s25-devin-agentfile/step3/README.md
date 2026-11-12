# Step 3: Devin's Agentfile, harness.json and its row in kiln

Kiln plan 01a1ce705e4d7008993f5c7291ef4476 (version 2), step 3 of 6, on research
01a1ce705e4d70089c04cee721525c8f (fN below). Implementer: kiln agent
01a1cf40527c731c9107df5d9b744455; coordinator 01a1ce6f55c3749ca87f3ef3393ec8b0 (step 25's agent);
ROOT 01a148884a4770fdaf0111f3f792a248. The policy read before any edit is
[POLICY_READ.json](POLICY_READ.json): Shards main ac87190d3 (development-rules.md df2acf47…,
R01-R16), kiln main c09d23207 (AGENTS.md 16b05ef8…, docs/ci.md §0.3).

Branch `s25-devin-row`: step 1's pin (162f1f757), kiln main c09d23207 (step 30's table) merged in
(19b4ee5c0), this step's commits (365af3037, 4cefe7ada), then kiln main 172176b23 merged in
(f3a10164b). That main has step 23's Goose row, the table's first, so the branch takes its layout
(below, "After main moved").

## What it adds

| Path | What |
|---|---|
| `.kiln/images/harness-devin/Agentfile` | `ARG BASE` (pin.json's base by digest), `FROM ${BASE}`, `ARG TARGETARCH`, `HARNESS devin FROM ./devin-${TARGETARCH}`, the bring-up's three provider grants, three harness-scoped volumes (/workspace, /home/devin/.config/devin, /home/devin/.local/share/devin); no RUN, no ENV. Comments cite why: static builds (f5), the grants until step 9 (f14), Devin's XDG directories (f10, f12), no credential (f13), no self-update (f26). |
| `.kiln/images/harness-devin/harness.json` | Shards' full OSI form: schemaVersion 1, name devin, version 3000.12.43 (the pin's), run.command `["devin", "acp"]` (the devin Kind's program and ACP arguments, f9), run.env the two XDG roots (f12). No update setting (f26). |
| `crates/kiln/src/agentfiles/builtin.rs` | Devin's row in `ALL`, after Goose's, by `include_str!` of the three files, and `mod devin_tests`. (Before the merge: `Builtin` gaining `pin` and `files`, and six tests in `mod tests`.) |
| `crates/kiln/src/agentfiles/builtin/devin_tests.rs` | Devin's four tests: step25_devins_pin_is_cognitions_two_linux_bundles, step25_devins_harness_json_starts_it_as_kiln_does, step25_devins_agentfile_is_the_layouts, step25_devins_files_carry_no_credential. |
| `crates/kiln/tests/agentfiles.rs` | The listing's `builtin` is held to `kiln::agentfiles::builtin::views()`, not `[]`, so each harness's row passes it unchanged. |
| `Dockerfile` | The build stage copies `.kiln/images/` before cargo builds kiln (f25). Since the merge, main's own line, which Goose landed. |
| `.dockerignore` | Its header names `.kiln/images/` among the context it keeps (a comment). |

## Decisions

- **The row's shape.** `pin: &'static str` and `files: &'static [(&'static str, &'static str)]`, the
  shape the opencode (s22), goose (s23) and amp (s24) branches add, so whichever harness lands first
  defines it and the others merge (step 19's shared layout, item 2: never a second table). The
  Dockerfile's three lines were byte-identical to s22's. Goose landed first, with this shape and a
  COPY line of its own, which the merge takes.
- **No smoke data on the row.** Step 19 has landed no row carrying any, and nothing in kiln reads one
  yet. Step 5 gives step 19's `image()` fallback Devin's smoke where its reader is: program
  `--version`, expected prefix `devin <pin version> (` (f8). The coordinator's word: "if the row needs
  smoke data, write it your way, and say so in the commit".
- **harness.json's fields** come from Shards' source at main ac87190d3: `crates/image/src/osi.rs`
  (`Config`: schemaVersion, name, version, description, platform, run, skills, mcp, asks; `Run`:
  command, env, workdir; both `deny_unknown_fields`) and init's `crates/init/src/domains.rs`
  (`harness_env`: PATH with the harness's directory and its bin first, `HOME=/tmp`, `SHARDS_DOMAIN`,
  then run.env, the last given winning; a bare command found on that PATH, so `devin` is
  `/harness/devin/bin/devin`).
- **The secrets scan's names** are the plan's: WINDSURF_API_KEY and DEVIN_API_KEY (f13),
  credentials.toml (f10), `cog_`, the prefix of Cognition's service-user keys (docs/ci.md's Devin
  row), and ENV, ARG or run.env names matching key, token, secret, password or credential.

## Checks, on Linux runners and a Mac Studio (never this Mac)

Scripts in [scripts/](scripts/), their outputs in [evidence/](evidence/), each run through
`scripts/remote.sh SCRIPT OUT PAYLOAD [kiln run flags]` (`kiln run --no-sync -i`, the script and its
payload as one stream; a payload is the files the script reads, `name base64` lines made from this
commit's files, and `/dev/null` for shards-build.sh and cleanup.sh). kiln's sync of the worktree
(600.8 MB, 16,535 files) timed out ("did not answer the sync in 60s"), hence `--no-sync`. remote.sh's
start marker, and its retry when a placement goes away before the run starts, came after the Mac
Studio's run; shards-build.sh ran under its first form, which sent the script alone.

1. **A check-only shards from Shards main ac87190d3** (`scripts/shards-build.sh`,
   [e_shards_build.txt](evidence/e_shards_build.txt)): on adacachy2, `cargo build -p shards`
   (dev profile, 34 s) with `SHARDS_HELPERS=skip` and a placeholder file for its two guests, since a
   check boots none. 1da9132b7 is an ancestor of ac87190d3 (`git merge-base --is-ancestor`, on the
   Mac's clone).
2. **Shards' own check of the Agentfile and context** (`scripts/shards-check.sh`,
   [e_shards_check.txt](evidence/e_shards_check.txt)): the context made as `kiln agentfiles context
   devin` will make it (both bundles held to pin.json's size and SHA-256, unpacked
   into devin-amd64/ and devin-arm64/ beside harness.json: 235 files and 17 directories each, no other
   entry).

   | Check | Exit | Answer |
   |---|---|---|
   | kiln's static check (`--target kiln:static-check`, as `check.rs` asks) | 1 | no warnings; only `target stage "kiln:static-check" could not be found`, which kiln reads as passed |
   | whole check, `--platform linux/amd64` | 0 | no warnings, no error (the base resolved by digest) |
   | whole check, `--platform linux/arm64` | 0 | no warnings, no error |
   | mutant: `CONNECT … ON nowhere` | 1 | refused at line 35: `CONNECT ... ON names network "nowhere", which no NETWORK of this stage's lineage declares` |
   | mutant: `ENV WINDSURF_API_KEY=x` appended | 1 | `SecretsUsedInArgOrEnv` at line 44 |
   | mutant: harness.json with an unknown field | 0 | passed: the check doesn't open it |
   | mutant: devin-amd64/ missing | 0 | passed: the check doesn't open it |

   **Found:** `shards build --call=check` opens no harness directory, the whole check included, so it
   vouches for the Agentfile, not for harness.json or the context. kiln's own check of a person's
   Agentfile (`check.rs`, the static check) reaches no further. Check 3 holds harness.json to Shards'
   reader instead.
3. **Shards' own OSI reader on harness.json** (`scripts/osi-check.sh`,
   [e_osi_check.txt](evidence/e_osi_check.txt)): a scratch example in the runner's Shards checkout
   (removed after; the checkout left clean) calling `shards_image::osi::Config::parse`. The real file
   is accepted: name "devin", version "3000.12.43", command ["devin", "acp"], the two XDG entries. Each
   mutant is refused in Shards' words: an unknown field (`unknown field update, expected one of
   schemaVersion, name, version, description, platform, run, skills, mcp, asks`), run.env setting
   SHARDS_DOMAIN, schema version 2, an empty command.
4. **kiln's tests** (`scripts/kiln-tests.sh`): kiln at c09d23207 fetched at depth 1, this branch's
   diff applied and every changed file held to its SHA-256 (all nine OK, before and after), then the
   agentfiles module's unit tests, `tests/agentfiles.rs`, clippy on crates/kiln with warnings denied,
   and three mutations of the guarded files, each restored after.

   On a Mac Studio (adas-mac-studio.local-2, Darwin 25.4.0 arm64, rustc 1.99.1, 12 CPUs and 32 GiB
   asked; [e_kiln_tests_macos.txt](evidence/e_kiln_tests_macos.txt)):

   | Step | Result |
   |---|---|
   | `cargo test -p kiln --lib agentfiles::` | 24 passed, 0 failed (393 s, the build included); the six in builtin.rs: every_builtin_is_a_valid_agentfile_once, devins_pin_is_cognitions_two_linux_bundles, devins_harness_json_starts_it_as_kiln_does, devins_agentfile_is_the_layouts, no_builtin_carries_a_credential, the_server_image_copies_every_path_a_row_includes |
   | `cargo test -p kiln --test agentfiles` | 1 passed (step30_agentfiles_on_every_surface) |
   | `cargo clippy -p kiln --all-targets -- -D warnings` | exit 0 |
   | mutation: Devin's Kind `Acp(&["acp", "--stdio"])` in kinds.rs | killed: devins_harness_json_starts_it_as_kiln_does FAILED, `left: ["devin", "acp"]`, `right: ["devin", "acp", "--stdio"]` |
   | mutation: the Dockerfile's `COPY .kiln/images/ .kiln/images/` removed | killed: the_server_image_copies_every_path_a_row_includes FAILED, naming each of the three included paths |
   | mutation: `ENV WINDSURF_API_KEY=x` in the Agentfile | killed: no_builtin_carries_a_credential FAILED, `ENV WINDSURF_API_KEY is named like a credential` and the name itself |

   The build's warnings are a dependency's (focal-memory's deprecated `fetch_update`) and macOS's
   linker's (`__eh_frame section too large`), none this change's.

   On Linux amd64: **not run.** kiln's queue held it behind every Linux runner's strain cordon
   (the headroom guard's, which kiln's own gate yields to as well). Its first placement went away
   before it started ([e_kiln_tests.lost-placement.txt](evidence/e_kiln_tests.lost-placement.txt)).
   Asked again for 12 CPUs and 32 GiB, kiln said "expected within 673 min"
   ([e_kiln_tests.queued-12cpu.txt](evidence/e_kiln_tests.queued-12cpu.txt)); for 6 CPUs and 24 GiB,
   "expected within 790 min" ([e_kiln_tests.queued-6cpu.txt](evidence/e_kiln_tests.queued-6cpu.txt)).
   I stopped it rather than hold a Linux queue's place for 11 to 13 hours or sidestep the cordon with
   an unsized run. Step 6 runs this module's tests and clippy on a Linux runner at the landed
   revision. The change has no platform-specific path, and checks 2, 3 and 5 ran on Linux.

5. **The server image's context, by Docker** (`scripts/docker-context.sh`,
   [e_docker_context.txt](evidence/e_docker_context.txt)): on adacachy2 (Docker 29.9.3, whose missing
   buildx component left the legacy builder), a context of kiln's `.dockerignore` and
   `.kiln/images/harness-devin/` plus a decoy `x.log` there, built onto scratch with the build stage's
   own line, `COPY .kiln/images/ .kiln/images/`, then exported and removed. All five files landed byte
   for byte; the decoy was dropped by `**/*.log`; no image of its name was left.

## After main moved: Goose landed first

While this step waited on Linux, kiln main reached 172176b23: step 23 landed the table's first row,
harness-goose. It brought `Builtin {name, harness, text, pin, files}`, the shape this branch had,
and the Dockerfile's `COPY .kiln/images/ .kiln/images/` under its own comment. It also brought
`step23_kilns_dockerfile_copies_what_builtins_embed` (the test this branch had added),
`step23_a_real_shards_checks_every_builtin`, and Goose's tests in their own file,
`builtin/goose_tests.rs`, named `step23_…`. kiln's AGENTS.md asks that test names cite the ID they
serve, which this branch's names hadn't.

The merge (f3a10164b) takes main's side and adds Devin. `builtin.rs` is main's, with Devin's row after
Goose's and `mod devin_tests`. Devin's four tests move, unchanged in substance, to
`builtin/devin_tests.rs` as `step25_…`, the credential scan held to Devin's row alone. This branch's
`every_builtin_is_a_valid_agentfile_once` was main's word for word, and its Dockerfile test repeated
main's step23 one, so both go; the .dockerignore half of the latter goes with it, and check 5 keeps its
evidence. The Dockerfile is main's. tests/agentfiles.rs keeps this branch's comparison to
`builtin::views()`.

Smoke: Goose's pin carries one (`"smoke": {program, args, stdout}`), checked by Goose's own tests;
no shared test asks a pin for one. Devin's is step 5's, in pin.json through pin.sh, which writes that
file.

After the merge, on the same Mac Studio, `scripts/kiln-tests.sh` ran with BASE 172176b23 and
SHARDS_REV ac87190d3 ([e_kiln_tests_merged_macos.txt](evidence/e_kiln_tests_merged_macos.txt)). That
built a check-only shards first, as shards-build.sh does, and named it by `KILN_TEST_SHARDS`, with its
own SHARDS_HOME and an empty DOCKER_CONFIG.

| Step | Result |
|---|---|
| `cargo test -p kiln --lib agentfiles::` | 30 passed, 0 failed: Devin's four step25 tests, Goose's five, the table's three, and the module's others |
| `step23_a_real_shards_checks_every_builtin`, `--nocapture` | passed, and ran: kiln's own `check::run` with the real shards had nothing to say of Goose's Agentfile or Devin's |
| `cargo test -p kiln --test agentfiles` | 1 passed (both rows listed) |
| `cargo clippy -p kiln --all-targets -- -D warnings` | exit 0 |
| mutation: Devin's Kind `Acp(&["acp", "--stdio"])` | killed: step25_devins_harness_json_starts_it_as_kiln_does FAILED |
| mutation: the Dockerfile's COPY of .kiln/images/ removed | killed: main's step23_kilns_dockerfile_copies_what_builtins_embed FAILED |
| mutation: `ENV WINDSURF_API_KEY=x` in the Agentfile | killed: step25_devins_files_carry_no_credential FAILED |

**Main's own integration test fails as main stands (R09).** `scripts/main-repro.sh` ran
[e_main_repro_macos.txt](evidence/e_main_repro_macos.txt) on main 172176b23's tree exactly (`git status`
empty) with the same target. `step30_agentfiles_on_every_surface` FAILED, exit 101: line 111 asserts
`listed.get("builtin") == Some(&json!([]))`, and the listing's `builtin` holds
`{"name":"harness-goose", …}`. The merged branch passes it with tests/agentfiles.rs comparing to
`builtin::views()` instead, so main is red there until this branch, or that line alone, lands.

## R01-R16

| Rule | Proof, or the gap |
|---|---|
| R01 | The production change is data: two `&'static` fields and one const row, no fallible step. The tests use `?`, `ok_or_else` and asserts; no unwrap, expect or indexing. Clippy on crates/kiln, all targets, warnings denied (kiln's denied lints included): clean on macOS arm64; not run on Linux amd64 (check 4: kiln's queue; step 6 runs it at the landed revision). |
| R02 | Every line rests on research 01a1ce705e4d70089c04cee721525c8f (f3, f5, f9, f10, f12, f13, f14, f25, f26) and step 19's shared layout. The row's shape was compared with the s22, s23 and s24 branches' (s23 adds a `smoke` field, s22 puts its smoke in pin.json); harness.json's fields and env order were read in Shards' source at ac87190d3, then held to Shards' own reader. |
| R03 | No numeric literal in production. The version, URLs and digests are pin.sh's output (step 1); the tests' expected lines are the research's (f12, f14) and the plan's; the mutants' values (`--stdio`, `x`) exist only to differ. |
| R04 | No state machine and no error path added: data only. |
| R05 | No trait, cache or wait added; two fields on step 30's struct, as the shared layout decided (one table). |
| R06 | Each test helper does one check; the longest, the pin's, is a list of independent conditions. |
| R07 | The Agentfile's comments give reasons with their findings; the code's cite Shards' source and the research. |
| R08 | Not applicable to a data row. Image sizes and build times are step 4's single observations; comparisons are ROOT's. |
| R09 | f25's break (a row including from `.kiln/` while the build stage copies no `.kiln/`) is guarded: the test fails with the COPY line removed (mutation dockerfile-copy), and Docker shows the line copies the files with kiln's `.dockerignore` (check 5). kiln's whole server image was not built here (heavy: ROOT's, at merge). Main's integration-test failure was reproduced live on main's own tree before this branch's fix counts for it (red on main 172176b23, green on f3a10164b). |
| R10 | This directory: each script, its output, and the commands. |
| R11 | kiln's tests and clippy ran on macOS arm64 (a Mac Studio); not on Linux amd64 (check 4: kiln's queue, 11 to 13 hours; step 6 runs them there at the landed revision). Windows clippy did not run: kiln's full gate waits for the person's word, and the change has no platform code. Devin's linux/arm64 build was checked by Shards' whole check, run on amd64; arm64 images and smokes are step 4's. |
| R12 | One table (step 30's), with Goose's row before Devin's: the shape Goose landed, main's Dockerfile line, main's table tests (the Dockerfile and the real Shards), and Devin's own tests in their own file as Goose's are; no second fetcher (the context here was made by hand, with the same size and digest checks). The OSI test mirrors Shards' field names, since kiln doesn't vendor Shards yet (step 16). |
| R13 | Data plus the tests the plan names; nothing added for a reader that doesn't exist yet (no smoke field). |
| R14 | None here: profiling and benchmarks are ROOT's. |
| R15 | Implementer, light tests only; no push; the SHA and counts go to the coordinator. |
| R16 | `scripts/cleanup.sh` removes my `~/.cache/s25-step3` once a runner's last run is done: on the Mac Studio, 31 GB (30 GB of it kiln's target; [e_cleanup_macos.txt](evidence/e_cleanup_macos.txt)); on adacachy2, 5.0 GB (3.5 GB of it the check-only shards' target; [e_cleanup_adacachy2.txt](evidence/e_cleanup_adacachy2.txt)); on the Mac Studio after the merge's run, 37 GB ([e_cleanup_macos_merged.txt](evidence/e_cleanup_macos_merged.txt)). |

## Not done here

- `kiln agentfiles context devin` (step 19's fetcher) and tar.gz unpacking (step 2, branch
  s25-unpack) aren't on this branch, so the checks' context was made by hand, with the same size and
  SHA-256 checks and an unpack that adds nothing.
- Building both platforms, the version and ACP smokes, and the image's secrets scan: step 4. The CI leg
  and image()'s fallback: step 5. kiln's docs: step 6.
- kiln's server image (`docker build` of the Dockerfile): ROOT's, at merge.
- The agentfiles module's tests and clippy on Linux amd64 (check 4): step 6 runs them at the landed
  revision.
- Devin's bundles carry no publisher signature (step 1's record): the pin's SHA-256s and TLS are all
  that hold them.
