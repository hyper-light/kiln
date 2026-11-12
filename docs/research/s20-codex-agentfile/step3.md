# Plan step 20 [kiln], step 3: the shared unpack, Codex's rules (evidence)

Plan 01a1ce705d7076648c1d9f9ee26214d3 (step 20 [kiln], the Codex Agentfile), step 3 of 6; research
01a1ce705d71770686ed0e7e45e6bcdc (f3, f4, f8, f9, f11, f21, f23). This records what step 3 built, what it
ran, what changed from the step's text and why, and what is still owed. Step 6 joins it into the step's
evidence record.

## Identity

- Role: implementer, step 3's agent (01a1d00e7c147708a8eb91b143a5e6e3), under step 20's coordinator
  (01a1ce6eb22f76988397e3264f4b3864).
- Policy readback: `evidence/POLICY_READ-step3.json` (2026-11-12T14:33:01Z):
  - Shards origin/main 8f3cd5b74: development-rules.md df2acf47…, AGENTS.md f66d304c…,
    development-evidence.md 61613a24…, the hashes the plan pins. g121's checkout lacks them, so they
    were read whole from origin/main.
  - kiln origin/main 6fa954363: AGENTS.md 16b05ef8…, unchanged. docs/ci.md changed as a whole
    (be867453…); its §0 (SHA-256 1e464890…, the prefix step 1 recorded) was read in full.
- Branch: s20-unpack, from s20-codex-agentfile's ee2f681d3 (step 1's pin), in its own worktree
  (~/Projects/kiln/.claude/worktrees/s20-unpack), so it never touches the worktree step 2's agent edits.

## Where things stood when step 3 started (2026-11-12T14:00Z)

- **No fetcher on any branch.** s19-claude-agentfile still had no commits of its own. Step 19's agent,
  restarted by ROOT, re-planned (plan 01a1cf48e8157494ac9a51f7a564f526). Its step 3 gives
  `kiln agentfiles context` to step 28's step-3 implementer (01a1cf3fe10275eabd008cc17428a634), on a
  branch s19-fetcher. That step's terms hold this step's first Codex rule, whole-directory assembly: the
  per-arch directory is built "under a temporary name in dir … rename the directory into place only
  when it is complete; on any failure, leave nothing". It also hands `unpack` to the shared module's
  `unpack::Spec` as written. So this step writes no fetcher of its own.
- **The shared unpack is step 25's module, converted by step 27.** Step 21 dropped the
  {format, strip, to, only?} shape this step's text names, and moved to step 25's (step 1's record says
  why). Step 27's implementer (01a1cf3fbf0c72f38531c455cc4647f2) is converting step 25's module to the
  contract steps 25 to 27 agreed, and ROOT ruled that it lands on kiln main on its own. In it:
  - Spec is Member {member, sha256, size} or Tree {strip, unpacked_bytes, entries}, with unknown fields
    refused;
  - Exists refuses any path written twice;
  - Missing refuses a member the archive lacks;
  - Link, Device, Kind, Escapes, SetId, Sticky, Duplicate, Prefix, Nameless and HarnessFile refuse
    entries, each naming its URL and entry.
- **Step 1's pin.json doesn't parse under it.** Its trees lack `unpacked_bytes` and `entries`, and
  `skip` is unknown.

## What step 3 adds: a tree's `skip`

The step's text has Codex's second write to one path refused (Collision) unless the later file says
`replace: true`, and an absent `only` path refused (Missing). In the shared shape:
- Collision is the module's `Exists`;
- the arm64 rg swap is the tree's `skip` that step 1's pin already carries (the coordinator's option (A),
  sent to step 25's coordinator at 13:45Z with (B) `replace` as the alternative);
- Missing covers both a member and a skipped path the archive lacks.

Why (A), not (B):
- **Nothing is ever overwritten.** That is the module's invariant (`create_new`), and (A) keeps it: the
  tree leaves the path for the member.
- **The rule stays inside one tree.** (B) needs the fetcher to know which earlier file wrote which path,
  and to tell a file from a directory there.
- **The pin says what's left out.** The tree's own entry names the file it doesn't write, and its totals
  stay the archive's.
- **Codex needs the exact path.** Codex runs codex-path/rg whenever it is a file (step 1's record,
  `rg_command`), so a member at another path won't do.

The change to `crates/kiln/src/agentfiles/unpack.rs`, on step 27's conversion (commit 4d1f96a5d on
s27-unpack, merged into s20-unpack as b41d1f018; its unpack.rs and tests.rs are byte for byte the working
copy this was first written on, sha256 c839318f… and c0f93809…):
- `Tree.skip: Vec<String>` and `Fields.skip: Option<Vec<String>>`. `skip` is a tree's field: beside a
  member's fields it is `Both`, and alone it still needs `strip`, `unpacked_bytes` and `entries`. Tree
  loses `Copy`.
- `Skip::of` checks the list before staging is made or the archive is read. A path that is absolute,
  climbs with `..` or names nothing is `Escapes`; a path listed twice (after `.` is cleaned away) is
  `Duplicate`. Each error names the pin's words for the path.
- In `fill`, a regular file at a skipped path is counted in `entries` and `unpacked_bytes`, which stay the
  archive's own totals, as `tar -t` and `tar -tv` list them. It is checked as every entry is, and never
  written:
  - Link, Device, SetId, Escapes and Duplicate still refuse it;
  - a directory at a skipped path is `Kind('5')`, as a member that is a directory is.
- At the end, a skipped path the archive never held is `Missing`, naming the path.

## Tests

Five tests are added to `unpack/tests.rs`, besides step 27's thirteen (whose four `Tree` literals gain
`skip: Vec::new()`):

| test | shows |
|---|---|
| codex_arm64s_tree_skips_its_rg_and_ripgreps_static_rg_takes_its_place | the package at strip 0 skips its glibc rg; ripgrep's member is written at codex-path/rg 0755; the tree's totals (8 entries, 57 bytes) count the skipped rg; directories 0755, files 0755/0644; both archives and staging gone |
| without_its_skip_ripgreps_rg_collides_with_the_packages_and_the_first_stays | with no skip, ripgrep's member is `Exists("codex-path/rg")`, naming ripgrep's URL and member; the package's rg is left as it was |
| a_skipped_path_is_one_regular_file_the_archive_holds | Missing (absent), Kind('5') (a directory), Link (a symlink), SetId (4755), Escapes (`/abs`, `../`, empty, `.`), Duplicate (listed twice): each names the path and leaves no per-arch directory, staging or archive |
| a_skipped_file_counts_in_the_trees_totals | totals that leave the skipped rg out are `Entries {7, 8}` / `Bytes {43, 57}` at the entry past them, leaving nothing |
| a_trees_skip_is_a_trees_field | skip parses with a tree; beside a member's fields it is `Both`; alone it `Lacks("strip")` |
| every_builtins_pinned_unpack_is_a_spec | every `unpack` in every built-in's pin.json parses as a `Spec` (Goose's two members, Codex's three); Codex's are two trees at strip 0 with their totals, one skipping codex-path/rg, and one member |

### Runs (scratch crate on adacachy2)

The module and its tests alone, in a scratch crate with kiln's dependency versions and kiln's workspace
lints (`scripts/step3-scratch-Cargo.toml`, `scripts/step3-run-scratch.sh`). The files go over as one
base64 bundle on stdin, with `kiln run --no-sync -i`, so no worktree is synced. Linux 7.3.3-1-cachyos
x86_64, rustc 1.98.1. The base is step 27's working copy as of 2026-11-12T14:22:40Z (unpack.rs sha256
c839318f…, tests.rs c0f93809…, uncommitted on s27-unpack at 94639db7b).

- `evidence/e_step3_scratch_run1.txt`: unpack.rs 674161d0…, tests.rs d68fbebb…. 18 of 18 tests passed;
  `cargo clippy --all-targets -- -D warnings` exited 0. The GNU tar warnings at its head are about macOS
  xattrs in the bundle, nothing else.
- `evidence/e_step3_mutants_run1.txt` (`scripts/step3-mutants.py`): each of seven mutants removes or
  bends one guard, applied alone and exactly once, and each fails the test written for it.
  - M1, a skipped file written: the codex test fails (ripgrep's member hits Exists).
  - M2, no Missing: the skipped-path test fails.
  - M3, skipped files left out of the totals: the totals test and the codex test fail.
  - M4, a directory at a skip path taken: the skipped-path test fails.
  - M5, skip paths not held inside: the skipped-path test fails.
  - M6, a path listed twice taken: the skipped-path test fails.
  - M7, skip not a tree's field when parsed: the parse test fails.
  The control and the restored file pass.

### Run (the kiln crate on adacachy2)

`evidence/e_step3_kiln_run1.txt` (`scripts/step3-start-kiln-tests.sh`): the same files in the kiln crate
at ee2f681d3 (kiln main c09d23207 plus step 1's pin), synced with `kiln run` to adacachy2 and run as a
`systemd-run --user` unit, since the kiln server drops long runs.
- `cargo test -p kiln --lib agentfiles::unpack`: 18 passed, 0 failed (a cold build of 9 min 3 s).
- `cargo clippy -p kiln --all-targets -- -D warnings`: exit 0.

Both started 2026-11-12T14:44:59Z.

`evidence/e_step3_kiln_run2.txt`: 5e296cac2 (main 263cad6b4 plus steps 1 and 2) with this step's files.
`cargo test -p kiln --lib agentfiles::` passed 49 of 49, among them the coordinator's codex_tests on the
re-pinned pin and the new every_builtins_pinned_unpack_is_a_spec. Clippy exited 0.

### macOS and Windows (scratch crate)

The committed unpack.rs (674161d0…) and its tests, the built-in test aside (it needs the kiln crate),
through `scripts/step3-run-scratch-portable.sh`:
- `evidence/e_step3_scratch_macos.txt`: adas-mac-studio.local-2, Darwin 25.4.0 arm64, rustc 1.99.1. 18
  passed; clippy --all-targets -D warnings exit 0.
- `evidence/e_step3_scratch_windows.txt`: jupiter, MINGW64 on Windows 10.0.26200 x86_64, rustc 1.99.0.
  16 passed, step 27's two `cfg(unix)` tests compiled out; clippy exit 0.

### The final run, on what is committed

`evidence/e_step3_kiln_final.txt` (`scripts/step3-start-kiln-final.sh`), on adacachy2 at
2026-11-12T15:27:55Z. The tree is HEAD b41d1f018 (ce8ad3845, the coordinator's totals, merged with step
27's 4d1f96a5d) plus this step's unpack.rs (674161d0…) and tests.rs (38d41298…), the tree this step
commits:
- `cargo test -p kiln --lib agentfiles::`: 49 passed, 0 failed.
- `cargo clippy -p kiln --all-targets -- -D warnings`: exit 0.
- M8: pin.json's amd64 tree without `entries`. every_builtins_pinned_unpack_is_a_spec fails, 101, with
  "harness-codex: {…"unpacked_bytes":540223046}: no entries, which its form needs". With the pin
  restored, it passes.

## The pin's totals (an independent count)

The shared Tree requires `unpacked_bytes` and `entries`, and step 1's pin had neither. Before the
coordinator's ce8ad3845 was seen, this step changed pin.sh on its own:
- `inspect` counted every member tarfile yields, `./` included, and the regular files' bytes;
- the package trees' unpack carried both;
- pin-mutations.sh's control required them;
- check-pin.sh recounted them from the fresh download with GNU tar's listing, a second reader.

The coordinator's harness, `run-step2-pin.sh … rust-v0.167.2`, ran with these as a `systemd-run` unit on
adacachy2 (`evidence/e_step3_pin.txt`, 2026-11-12T15:15:16Z to 15:17:20Z):
- fetch, verify, the mutations and the fresh downloads all exited 0;
- MUTATIONS passed=21 failed=0, the control holding the totals;
- `FRESH TREE MATCH` for both packages: GNU tar lists 52 entries, with 540223046 bytes on amd64 and
  481289300 on arm64, as pinned. That is 44 files and 8 directories each; neither archive has a `./`
  entry;
- FRESH archives=3 matching=3, members=1 matching=1, trees=2 matching=2.

That pin.json equals the coordinator's ce8ad3845 pin.json in every field but `read_at`. So two separate
changes to pin.sh, and GNU tar, count the same totals. This step's pin edits were dropped for the
coordinator's.

## R01 to R16, for this step's change

| rule | proof, or the gap |
|---|---|
| R01 no panics | No unwrap, expect, indexing or panic in the new production code: `Skip` uses `contains_key` and `get_mut`. The workspace's deny lints passed (`clippy -p kiln --all-targets -D warnings`, e_step3_kiln_final). |
| R02 research | (A) skip against (B) replace, and step 21's {strip, to, only}, weighed above. Sources: Codex's `rg_command` at rust-v0.167.2 (step 1's record, its saved page lines 151-172); the shared module's contract (4d1f96a5d's message, step 27's record); plan f11, f23. |
| R03 no arbitrary constants | No new literal. The totals are the pinned archive's own, counted by pin.sh and recounted by GNU tar. |
| R04 typed errors | Each new refusal is a typed `Why` the module already had (Missing, Kind, Escapes, Duplicate), or Exists when nothing is skipped. Each names the URL and the entry or skip path. |
| R05 deliberate abstraction | One private type, `Skip`, at the module's existing seam. No trait, no new layer, no cache, no wait. |
| R06 complexity | `Skip` has four short methods, and `fill` gains one check per arm. |
| R07 comments | The doc comments give why codex-path/rg (Codex runs it whenever it's a file) and what the totals count. They don't narrate the code. |
| R08 measured results | Not applicable: a correctness rule, with no performance claim made. |
| R09 reproduce first | Both defects are reproduced by tests: without skip, ripgrep's member is Exists (`without_its_skip…`); a tree without totals doesn't parse (M8). |
| R10 durable evidence | Logs in `evidence/e_step3_*`. Eight mutants (M1-M7 in the scratch crate, M8 in the kiln crate), each failing for its reason. |
| R11 platforms | The module and its tests: Linux x86_64 (adacachy2, the kiln crate and the scratch crate), macOS arm64 (adas-mac-studio.local-2, Darwin 25.4.0, rustc 1.99.1: 18 of 18) and Windows x86_64 (jupiter, MINGW64 on Windows 10.0.26200, rustc 1.99.0: 16 of 16, step 27's two `cfg(unix)` tests compiled out), with clippy -D warnings clean on each. Unverified: the kiln crate's own build on macOS and Windows, and Linux arm64. |
| R12 share implementations | Extends the one shared unpacker. No second unpacker and no second fetcher. Duplicate pin edits were dropped for the coordinator's. |
| R13 simple | The smallest change that lets the arm64 rg member be written: one tree field. |
| R14 profiling | Not done: ROOT's. No speed or allocation claim. |
| R15 roles | Implementer, light tests only. No merge or push; the coordinator and the steps named are told. |
| R16 runner space | adacachy2 `~/.cache/s20-step3/` holds a build target, regenerable, kept for the fetcher's runs and removed when the step ends. Evidence lives only in this worktree. |

## Owed, in order

1. ~~Rebase onto step 27's commit, test again, commit.~~ Done: step 27 committed 4d1f96a5d at 15:24Z, it
   was merged (b41d1f018), and the final run above passed. Step 25's own unpack agent
   (01a1ceb7914e702aa9059e6dfdd3c79a) has been still since its 10:35Z cut-off. Step 27's module was
   converted from that agent's 02:35 working copy and is the one ROOT ruled lands on main on its own.
2. ~~Codex's pin with each tree's `unpacked_bytes` and `entries`.~~ Done by the coordinator in
   ce8ad3845 (15:20Z), while this step's request to it was still refused. This step's own re-pin, with an
   independent change to pin.sh, agreed with it exactly (see "The pin's totals" above). s20-unpack now
   sits on ce8ad3845, with this step's pin edits dropped for the coordinator's.
3. ~~Once the fetcher exists, two checks through it~~ (done on kiln main: "Step 20's finish on kiln main"
   below):
   - kiln's HTTP server in a test serving the hostile fixtures (`..`, /abs, symlink, hard link, device,
     truncated gzip, a missing member or skip path, a collision without skip). Each must return its
     typed error and leave 0 files, counted by a directory walk;
   - the live `kiln agentfiles context codex DIR --platform linux/amd64,linux/arm64` on a Linux runner,
     with every file's SHA-256 against the pinned archives' members and arm64's rg against ripgrep's
     (`scripts/step3-live-check.py`). Step 2's harness-codex row is in (ff7857ae4), so only the fetcher
     is missing.
4. Tell steps 19 and 21 (and 25, 27, 28) the commit.

## Messages

kiln's mail refused most sends with `backpressure` (pending 3, authority 2). Those still unsent are
retried from `~/.cache/s20-step3/mail/` every 300 s:
- to step 20's coordinator: this status;
- to step 25's coordinator: (A) built.

Sent:
- to step 2's agent, about the totals. It answered that pin.sh is the coordinator's;
- to step 27's implementer: `skip`, to go on top of its commit, and a no-sync way to test.

Dropped: the relay to step 28's implementer of step 19's acceptance, which it took up itself at 14:42Z
(s19-fetcher's POLICY_READ, 92fa514d4).

## Step 20's finish on kiln main (2026-11-12, 22:00Z on)

Step 3's agent stalled with the skip on s20-unpack-v3, its last commits rebased there at 19:34Z (f195624f6),
a branch that conflicted with kiln main. ROOT (session g121-a0, kiln agent 01a148884a4770fdaf0111f3f792a248) gave the finish to an implementer
subagent: work out what step 20's and step 26's unpack branches hold that main lacks, port what's needed onto
main's unpacker, show Codex's context through the fetcher for both platforms, and land it.

### Identity

- Role: implementer, under ROOT (coordinator). Branches p/s20-finish-skip-v1 (this landing) and
  p/s20-finish-v1 (the fetcher slice after it), from kiln origin/main 3a4b09005, in their own worktrees
  (.claude/worktrees/s20-skip and s20-finish).
- Policy readback, sent to ROOT before any edit: kiln origin/main 3a4b09005, AGENTS.md (blob bff8c147e,
  sha256 16b05ef8…, unchanged from step 3's), CLAUDE.md (`@AGENTS.md`), docs/ci.md §0 read whole (§0.3
  binds); and Shards origin/main d13253e4b, AGENTS.md f66d304c…, development-rules.md df2acf47…,
  development-evidence.md 61613a24…, the hashes step 3 recorded.

### What each old branch held that main lacked

`git cherry` against origin/main, then each branch, and each of its merges, trial-merged into main with
`git merge-tree`: the branches have several merge bases each, so no merge-base diffstat was trusted.
Nothing was deleted; every branch below is left as it was.

| branch, commit | fate |
|---|---|
| s26-unpack-core 10a6ffe1f, 0911f08ee, f845ece8c | On main as 1dd6ccf94, bdec7ebc9 and 36cb87650, the same patches (patch-ids f89e65f9…, 0765f863…, 786490f6…), landed by 6e48ee58b (Merge s26-unpack, step 26). The branch merged into main 3a4b09005 makes main's own tree (3eec541a0): nothing is left. |
| s20-unpack-v3 324add5ab (the tree's skip) | Landed here as 3ec2deab4, cherry-picked onto main's unpacker, whose unpack.rs and unpack/tests.rs were byte for byte its parent 1c6e1b91d's. Its every-built-in test is as it was written (f31f5c31e put it back after 3ec2deab4 had narrowed it to Codex while Devin's pin wasn't a tree). |
| s20-unpack-v3 f195624f6 (macOS and Windows) | Landed here as 7d4254af8, as it was. |
| s20-unpack-v3 d4fb2de6f, 1c6e1b91d (merges) | d4fb2de6f merged into main is main's tree. 1c6e1b91d's inputs are on main (b5cceafb0 by b1a822568); merged into main it differs only in builtin.rs's table, where main's rows stand. Nothing of their own. |
| s20-unpack-v2 e2dd18a8f, 59b954ad9 | The same patches as s20-unpack's c2f5e6e5e and a9d29d11a (patch-ids d3c4b3cf… and 0ea9f6a1…): c2f5e6e5e is the skip before the member's `alone`, superseded by 324add5ab (3ec2deab4); a9d29d11a is f195624f6's (7d4254af8). |
| s20-unpack-v2 d45285626 (the fetcher's hook, Codex's fetcher tests, modes) | The hook is main's own since 73fc55df9 (step 28: the fetcher unpacks pinned archives through the one unpacker, trees first) and 8e571d7d8 (a failed context discarded as the unpacker discards). Its Codex-shaped fetcher tests are ported here (8d519acd5), less the hostile entries steps 26 and 28 already test through the same fetcher. Its modes said whatever the umask are the fetcher slice's (p/s20-finish-v1). |
| s20-unpack-v2 8a29779b6, 8bfa3c805 (merges) | Their inputs are on main: b5cceafb0 (by b1a822568) and 4d1f96a5d (by 6e48ee58b). |
| s20-unpack b41d1f018 (merge of s27-unpack) | 4d1f96a5d is on main (6e48ee58b). |
| s20-unpack 92fa514d4, 0b9063445, 704d9696d | Step 19's fetcher as step 28's implementer first wrote it, merged in: main took its own, 00198eee7. Superseded. |
| s20-unpack 40af1af26 (the hook on that fetcher) | Superseded by main's 73fc55df9. |
| s20-unpack b9da4675d (Codex's live context; the context directory 0755) | On that fetcher's `open_dir`: superseded. The live check is this landing's step20_a_live_codex_context_holds_the_pinned_builds and the CLI run below; the directory's mode is the fetcher slice's. |
| s20-unpack 457e5c81a (that branch's record of its messages) | This record. |
| s20-step2-agentfile a97340b79, s20-codex-agentfile-pre-rebase bed0057d3 | Step 2's earlier branches, superseded by s20-codex-agentfile (b5cceafb0, landed by b1a822568). s20-codex-agentfile and s20-surface-fix merged into main make main's own tree. |

### R09: main refused Codex's arm64 pin

Main's unpacker reads a pin's `unpack` as `unpack::Fields`, which refuses an unknown field, and had no
`skip`; Codex's linux/arm64 pin (b1a822568) carries `"skip": ["codex-path/rg"]`. codex_tests.rs passed,
since it reads the pin's JSON itself. So `kiln agentfiles context codex` refused that platform before
fetching anything:
- ROOT, live, at kiln e3f137930 on adas-mac-studio.local-2 (macOS arm64), `kiln agentfiles context codex
  <dir> --platform linux/arm64`: "ERROR harness-codex: its pin: .'s unpack: unknown field `skip`,
  expected one of `format`, `member`, `sha256`, `size`, `alone`, `strip`, `unpacked_bytes`, `entries`".
- M0 below: main's own unpack.rs and unpack/tests.rs under this landing's fetcher tests, the same words.

### What lands

- 3ec2deab4 and 7d4254af8: 324add5ab and f195624f6, cherry-picked (`-x`).
- 8d519acd5: Codex's context through the fetcher, served by kiln's own HTTP server in the test
  (step20_a_codex_shaped_context_holds_ripgreps_rg_in_place_of_arm64s, step20_a_codex_skip_that_fails_
  leaves_nothing), and the live test step20_a_live_codex_context_holds_the_pinned_builds; the runs'
  scripts (step3-finish-linux.sh, step3-finish-macos.sh).
- 1d9f44d11: step3-finish-mutants.sh (M0, M9 to M11).
- f31f5c31e: every_builtins_pinned_unpack_is_a_spec as 324add5ab wrote it: every built-in's every
  `unpack` read through unpack::Spec, so a pin the unpacker refuses can't land unseen (ROOT's word). It
  waited for Devin's tree pin (a14191747, on main with 3bd05755d), until which Devin's {"format": "tar.gz"}
  alone was neither form.

### Runs

- macOS arm64, adas-mac-studio.local-2 (Darwin 25.4.0, rustc 1.99.1), kiln run r308, the tree of 1d9f44d11
  (40bdea8a…) unpacked from a git archive and held to the commit's tree: `cargo test -p kiln --lib
  agentfiles::` 96 passed; `cargo clippy -p kiln --all-targets -- -D warnings` exit 0; then the mutants,
  each applied alone, each failing for its reason ([evidence/e_step3_finish_macos.txt](evidence/e_step3_finish_macos.txt)):
  - M0, main's unpack.rs and unpack/tests.rs (3a4b09005): the two Codex fetcher tests fail with "its pin:
    .'s unpack: unknown field `skip` …" (88 passed, 2 failed);
  - M9, a skipped file written anyway: codex_arm64s_tree_skips… and the Codex-shaped context fail;
  - M10, no Missing for a skip path the archive lacks: a_skipped_path_is_one_regular_file… and the Codex
    skip test fail;
  - M11, a directory at a skip path taken: the same two fail;
  - restored byte for byte (RESTORED_SAME), and the control 96 passed.
- Linux x86_64 and the live context through `kiln agentfiles context codex`: run on the landing's merge
  with kiln main, recorded below once they're in.

### Two things this landing found

- kiln-a094a12d.attlocal.net's /home went read-only during this step's first Linux run there: its btrfs
  had 1 MiB unallocated, metadata at 122.28 of 127.50 GiB and its global reserve at 475.6 of 512 MiB, and
  the run had seeded its cargo target with `cp -a --reflink=always` of another step's 58 GB target. A
  reflink adds metadata for every extent it shares, so that copy is the likely trigger. ROOT confirmed
  the state and took it to the person: the fix needs root. This step's Linux runs moved to adacachy with
  a cold target, and no run seeds a target by reflink again.
- Devin's pin on main was neither of the unpacker's forms until a14191747; the every-built-in test is what
  shows such a pin, and now does on every built-in.

