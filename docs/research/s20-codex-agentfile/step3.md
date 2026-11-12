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
| R11 platforms | Linux x86_64 (adacachy2) only so far. macOS arm64 and Windows are unrun for this change: the module's mode code is step 27's `cfg(unix)`, and this change adds none. Listed as unverified. |
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
3. Once the fetcher exists, two checks through it:
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
