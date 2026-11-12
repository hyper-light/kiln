# Step 27 [kiln]: Cursor's CLI agent as a built-in Agentfile

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27; its plan 01a1ce705e5c7504b88732f443d99288 (v7), research
01a1ce705e5c7504bdcac9721fa699e3 (f1-f27, 24 graded A, 3 B). Scope, the person's rule as ROOT's goal for this step
gives it (2026-11-12): implement the change and test only it, unit tests and light integration tests of the code
it touches; merge to kiln main and end. Image builds, smoke runs, harness turns and KVM, HVF or WHP runs are
ROOT's. Unpacking is step 25's alone.

## What is on kiln main

MERGED

- `.kiln/images/harness-cursor/`: the Agentfile (glibc-and-bash base by digest, no RUN, the bring-up's three
  grants, `/workspace` and the whole home `/home/cursor` as harness volumes), harness.json (OSI, `cursor-agent
  --disable-auto-update acp`, `HOME=/home/cursor`, `XDG_CACHE_HOME=/tmp/.cache`, `NO_OPEN_BROWSER=1`), pin.sh and
  the pin.json it wrote: Cursor 2026.11.11-b912975 from its installer, each platform's archive held to the
  `.sha256` Cursor publishes beside it (unsigned, same origin), with the tree's counts the unpacker holds it to.
- `crates/kiln/src/agentfiles/builtin.rs`: `harness-cursor` in `ALL`; `builtin/cursor_tests.rs`, its tests.
- `crates/kiln/tests/agentfiles.rs`: step 20's fix of the surface test (a690a02c5, step 25's blob too), which held
  the listing of built-ins to `[]` and failed on main since Goose's row.

## Steps

| Step | State | Record |
|---|---|---|
| 1. Pin Cursor's Linux builds | met: b75ff50fc, b3edce7e2 | `step1/record.md` |
| 2. Cursor's archives through the one unpacker | ended (the person's rule: step 25's); what waits is said there | `step2/record.md`, `step2/premerge/` |
| 3. Agentfile, harness.json, the table's row | met: c20f50b55 | `step3/record.md` |
| 4. Both platforms built and smoked | ROOT's; its harness prepared, unrun | `step4/README.md` |
| 5. Served by digest, the CI leg | out of scope (as step 26's step 5); step 19's image() and workflow aren't on main | here |
| 6. Docs, evidence, landing | landed here; docs owed: step 19's section of docs/agents.md isn't on main yet | here |

## Tests of the merged tree

PLATFORMS

## Owed

- Cursor's context and image once the one unpacker lands (`step2/record.md`); its image builds and smokes, both
  platforms, ROOT's (`step4/`).
- Cursor's `smoke` in pin.json, as Goose's and Claude Code's pins carry theirs, written by pin.sh: with step 19's
  image(), which runs it.
- Cursor's entry in docs/agents.md, once step 19's section on built-in Agentfiles is there.
- Not run anywhere: Windows hosts, Linux arm64 with KVM, a real Cursor sign-in into the HOME volume, Cursor's
  sandbox inside a harness domain, and ROOT's E2E (step 34).

## Shards' rules (docs/development-rules.md, sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3)

- R01: no production code but the table's row; the tests return errors, with no unwrap, expect or indexing.
- R02, R10: research 01a1ce705e5c7504bdcac9721fa699e3; every run's script and log committed beside its record,
  each log's SHA-256 checked as it came back from its runner.
- R03: the unpacker's bounds are the pin's counts, measured from the archives at pin time; no limit chosen. The
  first test run's memory cap was the measured 16.7 GiB and was killed there (17.7 GB resident, 3.3 GB swapped),
  so the run that counted went uncapped and the measure is recorded (`step3/record.md`).
- R04, R05, R06, R13: no state machine or abstraction added; one table, no second; flat checks.
- R07: the Agentfile's comments say why: its base, its grants, its volumes, the shards it needs.
- R08, R14: no performance claim and no hot path.
- R09: the surface test was shown red as main has it before its fix was merged (`step3/record.md`, INTEG_RED).
- R11: the table is platform-free; Cursor's builds are linux/amd64 and linux/arm64; clippy on each platform with
  room is above, and any without room is owed.
- R12: one unpacker (step 27 lands none), one table, step 20's fix merged as its own commit, no third copy.
- R15: step 27's coordinator did steps 3 and 6 itself (ROOT's word); six mutants each failed the tests holding
  them (`step3/record.md`); the merge is the person's rule.
- R16: runner space: `~/.cache/s27s3` on kiln-a094a12d (a reflinked 58G target, the clone) is removed after.

## The planning evidence

`POLICY_READ.json`; `evidence/` (Cursor's `.sha256` sidecars and the response headers of them and of the
installer, the CDN's `__cf_bm` cookie values redacted before commit; the static and live inspections of both
packages; step 26's messages; the revisions read); `scripts/`, which made them.

Cursor's own text and code that the research quotes stay on the planning machine, untracked, as they are
Cursor's to publish: its installer, `evidence/installer/install.sh` (sha256
e8c7a37d74b27a4beedf2dc5fc3b4c83627b2dc5c05a51d34f95ffec3cc0a247, pin.json's `version_read`); the excerpts of its
package's code, `evidence/code-excerpts.txt` (6d4a97c1031cd685c5ebb4d887c176a007333e43ba628f6abab93c5c1ea37ff3,
made by `scripts/ctx.py` from the pinned archive); and its docs pages as fetched, `evidence/docs/*.md`
(`scripts/fetch_docs.sh` names each page's URL):

| Page | sha256 |
|---|---|
| agent_security_run-modes.md | 3047e773f2f179e85214eaa4f058bc9db6b8e54ac040a55371d7b95766095192 |
| cli_acp.md | 65f46c603203646915b98f3ab52db1f0d670c221b46521a2b4e71d70d3c9181b |
| cli_changelog.md | de106fcb00a92e0ff8320923c8b2c704366592980b2123ffeab695abb5623e35 |
| cli_installation.md | 50435bbdfe5f3640484df508440b1c6274d8c57f85c2f91ac78ee15caa5a1d3c |
| cli_reference_authentication.md | c578539ee1ae3d574a75937230b22cea360f28e17da37529ddfceeedd04d2a1e |
| cli_reference_configuration.md | aab79ccc8949c23f30c45e0928ea80cefb5ee4ee6ff1723a3864ac61851f2a87 |
| cli_reference_parameters.md | da76369d0deeda7ebe45206ef5705ba89a8c83a6baa5bd91aba2736220de1ae7 |
| enterprise_network-configuration.md | 1899324badbf5381754d1ca518258f7738d7ec7804ba79b892dab558094ae030 |
