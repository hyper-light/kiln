# Plan step 26, step 3: agy's pinned archive unpacked in the fetcher

kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26; its plan 01a1ce705e55753182217afdfe607de9, step 3
("Unpack the pinned archive in the fetcher"). Branch `s26-unpack`.

## Identity

- Agent 01a1d07b3ba77708bb20d33bf184c5da, the step's **implementer**, started by kiln for step 26's
  coordinator 01a1ce6f7e4873619536f4adf51b5548. Machine adas-macbook-pro-2.local (macOS 26.4.1, arm64);
  nothing built or tested on it (the plan's rule): builds and tests on kiln's Linux runners and Mac
  Studios through kiln `run`.
- POLICY_READ, 2026-11-12T16:1xZ, read in full before any edit:
  - Shards `docs/development-rules.md` SHA-256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`,
    `AGENTS.md` `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`,
    `docs/development-evidence.md` `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`, at
    Shards origin/main 7ad28fab1. R01-R16 bind.
  - kiln `AGENTS.md` SHA-256 `16b05ef80db024a7b54b0e703d42c8cf5c1f330a626d6bd85ae4525609447a4c` and
    `docs/ci.md` §0 (file `be867453ea3527c50e5a030c94f931ef9bffb80f8f58a868e98b0cb7b47a4baa`), at kiln
    origin/main ce63fc099. Both equal the plan's recorded hashes.
- Owned paths: `crates/kiln/src/agentfiles/unpack.rs` and `unpack/tests.rs` (the member's `alone`);
  `context/unpack_tests.rs`'s `step26_*` tests; `cli.rs`'s `step26_*` live test;
  `.kiln/images/harness-antigravity/{pin.sh,pin_test.sh,pin.json}` and `builtin/antigravity_tests.rs`'s
  unpack check (the coordinator's files, changed for `alone` and said so); this folder.

## What was there, and what wasn't

- **The fetcher** is step 19's `kiln agentfiles context`, on kiln main (00198eee7, merged by ce63fc099).
  It refused every pinned archive (`Error::Unpack { url }`). A second fetcher (s19-fetcher 0b9063445,
  written by step 28's implementer) is on no main; step 20's step 3 builds on it (s20-unpack).
- **The hook** from main's fetcher into the unpacker is step 28's step 3 (agent
  01a1cf3fe10275eabd008cc17428a634), committed as 73fc55df9 on s28-unpack-hook (main ce63fc099 plus
  s27-unpack 4d1f96a5d). It reads each file's `unpack` through `unpack::Fields` and `Spec`, fetches the
  archive with step 19's streaming size and SHA-256 check into the context's folder, and hands it to
  `unpack::tar_gz` on the blocking pool (`crate::rt::blocking`, off hyper-rt's threads), mapping its
  refusal to `Error::Unpack(unpack::Unpack)`: URL, entry, typed `Why`. This step writes no hook and no
  fetcher: it merges 73fc55df9 (322ab8c26).
- **The unpacker** is step 25's, in the form steps 26 and 27 agreed (4d1f96a5d): `Spec::Member { member,
  sha256, size }` or `Spec::Tree`. Its member is streamed into the per-arch directory the fetcher is
  making, held to its size before a byte is written and to its SHA-256 as it streams, given its pinned
  mode, and the per-arch directory is renamed into place whole: no partial bin/agy is ever at its path,
  and a refusal removes what was written. Each `Why` names expected and found values where there are two.
- **The gap:** the step says agy's archive may hold one entry, the regular file named its member, and
  every other entry refuses it. The agreed member passes over every entry but the member, unchecked,
  as pi's ripgrep and fd archives need (they hold READMEs, completions, licences: step 28's measured
  needs in step 27's record, `docs/research/s27-cursor-agentfile/step2/evidence/s25-step2-brief.md`).
  Step 27's unpack implementer had told step 26 so, and offered "a pin field rather than the rule"
  (its message 01a1cf3f… to step 26's coordinator, 2026-11-12, still in that agent's queue when this
  step began).

## Decision (R02, R03, R05, R12, R13)

| Option | Result |
|---|---|
| Every member strict | Refuses pi's pinned archives: breaks step 28. Refused. |
| A tree's `entries` count on a member | The agreed contract's own test holds `{member…, entries}` to be `Both`. Refused. |
| An optional member field `alone: bool`, default false | pi's pins unchanged; agy's pin says what its pin.sh already checks. **Chosen.** |

- `alone: true` is pinned data, not a chosen constant (R03): pin.sh accepts an archive only when its
  one entry is the regular file `antigravity`, and now writes that fact into the pin it makes.
- One unpacker still (R12): the field is the member form's, in `unpack.rs`; the fetcher is unchanged.
- The pass is one explicit state (R04): `Pass::Nothing`, `Pass::Beside(first)`, `Pass::Picked`.
  - Beside the member, a hostile entry (absolute or `..` path, link, device or FIFO, another kind,
    set-ID or sticky bit) is refused at once as what it is; any other is `Why::Extra { member }`, at the
    member (before a byte of it is written) for one before it, at the entry for one after it.
  - The member twice is `Duplicate`; no member is `Missing`, whatever else the archive holds.
  - On any refusal the member's file, then the directories made for it, are removed (`one`).

## Commits (branch s26-unpack)

- 09e663836, 338f78473's f7308328a and 91c13a756's b9ca3bbfa merge the coordinator's s26-agy-agentfile:
  agy's pin, Agentfile, harness.json and built-in entry, the pin's smoke in its tests (76bd5ee27), kiln
  main 80fd748cb (338f78473), and the same cmp_owned fix as db510972c (91c13a756).
- 322ab8c26 and f71a1f341 merge step 28's hook, 73fc55df9 and 8e571d7d8.
- 1dd6ccf94: `alone` in the one unpacker, with its unit tests.
- bdec7ebc9 and 36cb87650: agy's archive through the fetcher from kiln's own HTTP server in a test, and
  the live test (36cb87650 makes the boxed checks Send and `|w: &Why|`, found reading the code).
- f867dd0ca: agy's pin says `"alone": true` (pin.sh writes it, pin_test.sh expects it, pin.json is
  pin.sh's output, antigravity_tests.rs holds the pin to it).
- 423169787: this record's first form and its harnesses.
- db510972c: clippy's cmp_owned in a line of 76bd5ee27's antigravity_tests.rs.

s26-unpack-core (8e571d7d8 plus 10a6ffe1f, 0911f08ee, f845ece8c: step 28's hook and this step's code,
without agy's entry) let kiln's tests run while 4955005d4's antigravity_tests.rs didn't compile.

## Results

Every run below is from a committed, clean tree (`git status --porcelain` empty), fetched on the runner
from a git bundle, with the files' SHA-256 printed. Each output is in `evidence/`, extracted byte-exact
from this session's transcript. Times are single observations, not claims (R08, R14).

| Run | Where | Revision | What | Result |
|---|---|---|---|---|
| r9 | adacachy2, Linux x86_64 | pin.sh 8c5bcd84 | pin_test.sh, then pin.sh against Google's updater | 12 of 12 cases; agy 1.5.7 pinned, the same archives and members as before, `alone: true` (e_pin.txt) |
| r11 | Mac Studio M3 Ultra, macOS arm64, rustc 1.99.1 | 0911f08ee's unpack.rs, tests.rs | the unpacker as a scratch crate: tests, clippy -D warnings, mutate.py | 18 of 18; clippy clean; 10 of 10 mutants killed (e_scratch_macstudio.txt) |
| r12 | adacachy2, Linux x86_64, rustc 1.98.1 | the same bytes | the same | 18 of 18; clippy clean; 10 of 10 mutants killed (e_scratch_linux.txt) |
| r15 | Mac Studio, macOS arm64 | s26-unpack-core f845ece8c | kiln's agentfiles tests | 63 of 63; stopped by this agent during clippy (e_core_macstudio_tests.txt) |
| r17 | Mac Studio, macOS arm64 | s26-unpack 423169787 | kiln's agentfiles tests, the live context in the test binary, clippy | 70 of 70; live MATCH 2 of 2; clippy refused one line of 76bd5ee27 (e_full_macstudio.txt) |
| r19 | Mac Studio, macOS arm64 | s26-unpack db510972c (b9ca3bbfa's tree) | the same | 70 of 70; live MATCH 2 of 2, nothing left; clippy exit 0 (e_final_macstudio.txt) |

The mutants (mutate.py), each killed by its test on both platforms: the hostile check beside the member
removed; an extra after the member taken; an extra before it taken; the member twice taken;
`alone` passed over; the pass stopping after the member; the written member kept on a refusal; a missing
member told as an extra; `alone` ignored in the parse; `alone` not a member's field.

The live context (r17, r19): `kiln agentfiles context antigravity` for linux/amd64 and linux/arm64, from
Google's storage, run through the CLI's own `run` in the test binary: the Agentfile and the two harness
directories alone, each with the embedded harness.json and bin/agy at 0755, hashing to the pin's member
SHA-256: amd64 bd643ae267d521b8febe5cd9fbce6533a1c7c0831aa0b28325401cb3e3b2035d, arm64
a96c0ecd1edabfb7e9736f25404601dc8002aea5d627dafe5c34e1aff37a442b. Nothing left in the folder after it.

## Against the step's words

- "read the archive with kiln's tar and flate2 crates, as blocking work off hyper-rt": `unpack::tar_gz`
  reads it with `tar::Archive<flate2::read::GzDecoder>`; step 28's hook calls it inside
  `crate::rt::blocking`, the blocking pool, never on a runtime shard.
- "accept only one entry: a regular file named member. Refuse links, devices, absolute or .. paths,
  other entries, and a member larger than its pinned size": `alone` (above); the member's size is held
  to the pin's before a byte of it is written.
- "stream the member into a temporary file in the target directory, checking its SHA-256 and size; set
  its mode and rename it into place": the member streams, through SHA-256, into the per-arch directory
  the fetcher makes under a temporary name in DIR (`.antigravity-<arch>.<id>`); its mode is set; the
  directory is renamed to `antigravity-<arch>` only once whole. bin/agy never shows in part at its path,
  and the one rename is the fetcher's for every harness (R12, R13): no second rename for a member.
- "Errors are typed (member missing, extra entry, entry not a regular file, member digest or size), each
  naming the URL, the member and the expected and observed values": `Error::Unpack(unpack::Unpack {url,
  entry, why})` with `Why::Missing`, `Why::Extra { member }`, `Why::Link`/`Device`/`Kind(typeflag)`,
  `Why::Digest { expected, got }`, `Why::Size { expected, got }`, and `Why::Escapes` for a `..` path;
  the tests match each variant and its values, and the message starts with the URL and names the entry.
- "Nothing is left behind on any error": the unpacker removes the archive and what it wrote for the
  member; the fetcher discards its staging and every harness directory made before the failure; the
  tests read the context's folder empty after each refusal, made before or not.
- "No panics, no new dependency": no unwrap, expect, indexing or panic in the code (kiln's deny-lints,
  clippy clean); Cargo.toml and Cargo.lock unchanged.

## R01-R16

- R01 Panics: none in production code; clippy -D warnings with kiln's workspace deny-lints passes on
  macOS arm64 (r19) and, for the module, on Linux x86_64 (r12). Tests alone assert.
- R02 Research: the agreed contract (step 27's commit 4d1f96a5d and its record), step 28's measured
  needs, step 27's offer of a pin field; three options weighed (Decision).
- R03 Constants: none added. `alone` is pinned data, pin.sh's own check at pin time.
- R04 States and typed errors: `Pass` (Nothing, Beside, Picked) is the pass's one state; refusals are
  `Why` variants the tests match.
- R05 Abstractions, waits, caches: none added; one field on an existing form; no wait, poll or cache.
- R06 Complexity: `one` keeps its checks and cleanup; `pick` is one loop over one explicit state;
  `hostile` names the tree's refusals for an entry beside the member.
- R07 Comments: say the contract and why (pi's pass-over, agy's pin), not the code.
- R08, R14 Performance: no claim. The times above are single observations.
- R09 Reproduce: the gap is a contract's, not a defect: the agreed member passes over every other entry
  by design (a_member_not_held_alone_passes_over_what_is_beside_it shows it, and pi needs it).
- R10 Evidence: this record, its harnesses, the logs; mutants each killed (mutate.py).
- R11 Platforms: Linux x86_64 (r9, r12, adacachy at the landed tree) and macOS arm64 (r11, r15, r17,
  r19, r1 at the landed tree) run; Linux arm64 and Windows unrun, so unverified (the code has no platform
  branch beside the module's existing Unix-only modes).
- R12 Shared: one fetcher (step 19's), one hook (step 28's), one unpacker (step 25's, as step 27 put it);
  this step adds a field to it and writes no second anything.
- R13 Simple: one optional field, one Why, one state type, one helper.
- R15 Role: implementer; light tests and the step's live test only. ROOT's landing batch merged this
  branch at 51af39c6d into kiln main as 6e48ee58b (2026-11-12T20:05Z), the code and its tests; this
  record's later commits land by fast-forward.
- R16 Space: every run made one ~/.cache directory and removed it (each log's END line); the targets
  (17-18 GB on the Mac Studio) went with them.

## Landing (ROOT's word, 2026-11-12)

On kiln main: 6e48ee58b, ROOT's landing batch's merge of s26-unpack at 51af39c6d (the unpacker's
`alone`, step 28's hook, agy's pin with `alone`, the tests). The runs below test the branch with main
ed0c9d3bd merged in, the tree this record's last commits sit on.

ROOT's goal for plan step 26 (on ROOT's stack, agent 01a148884a4770fdaf0111f3f792a248): "put the shared
unpacker on kiln main (your s26-unpack-core / s26-unpack: agentfiles unpack, the context hook and their
tests). It's THE one unpacker, and steps 20, 22, 23, 25, 27 and 28 wait on it." With its rules: test this
change only, merge origin/main in, push to kiln main (never force), report the SHA, what ran and what's
owed, and end; a platform without room is owed. Step 25's goal names this branch the one unpacker,
replacing step 25's own (s25-unpack).

- 95fada35b merges kiln main ed0c9d3bd (step 27's Cursor row beside Antigravity's) into s26-unpack:
  `git merge-tree` gave tree 1ba53cf7688dee9ed59c8777c3e34d17ed71ad13 and the merge has it. Cursor's pin
  unpacks as a tree ({format, strip, unpacked_bytes, entries}), the agreed form.
- r1 after kiln's restart, at 95fada35b, on the Mac Studio (macOS arm64): kiln's agentfiles tests 75 of
  75 (Cursor's five among them, with the unpacker present); clippy -D warnings on kiln --all-targets exit
  0 (evidence/e_land_macstudio.txt).
- adacachy (Linux x86_64, rustc 1.99.1) over ssh, as ROOT's rules allow when kiln's runners are busy, at
  95fada35b: kiln's agentfiles tests 75 of 75; the live context of Google's two archives through the
  CLI's own `run` in the test binary, MATCH 2 of 2 (each bin/agy the pin's member at 0755, harness.json
  the embedded one, nothing left) and `antigravity-amd64/bin/agy --version` printing `1.5.7`, the pin's
  version, as its first line; clippy -D warnings on kiln --all-targets exit 0
  (evidence/e_land_adacachy.txt, copied while kiln itself was still being built for the literal CLI
  run, which is extra to the step's tests).
- The merge commit was signed through the runner's harness socket: the restarted session had lost
  `KILN_HARNESS_SOCK` (kiln-git-sign: "not in a kiln shell"); its signature verifies against kiln's
  agent CA (O=hyper-light), as this step's earlier commits do.

## The step's live test, word for word (kiln main edba77bdb)

kiln built itself, then `kiln agentfiles context antigravity DIR --platform linux/amd64,linux/arm64`
(scripts/cli-run.sh), at kiln main edba77bdb, whose agy pin, unpack.rs and context.rs are the bytes
tested above:

| Where | Result |
|---|---|
| adacachy, Linux x86_64 (evidence/e_cli_adacachy.txt) | exit 0; DIR holds the Agentfile (the embedded one), antigravity-amd64/ and antigravity-arm64/ alone; each harness.json the embedded one; each bin/agy 0755 with the pin's member SHA-256 and size (2 of 2, sha256sum); `antigravity-amd64/bin/agy --version` printed `1.5.7` (31 2e 35 2e 37 0a), the pin's version |
| Mac Studio, macOS arm64 (evidence/e_cli_macstudio.txt) | the same, 2 of 2; --version not run (macOS runs no Linux binary) |

The context command took 10 s on adacachy and 105 s on the Mac Studio, single observations of Google's
storage, not claims.

## Open

- Owed, platforms without a run: Windows (no run of this branch there), Linux arm64. The step's own
  tests passed on Linux x86_64 and macOS arm64 at the landed tree.
- ROOT's, by the rules: image builds, smokes and real-VM runs of harness-antigravity.
- Step 28's hook (73fc55df9, 8e571d7d8) lands with this branch, as ROOT asked; step 28's record commit
  (a753c91f7, refs/s28-hold/unpack-hook-on-s27) is step 28's to land.
- Step 20's s20-unpack 40af1af26 is a second hook, on the withdrawn s19-fetcher: it conflicts with main's
  fetcher, and is step 20's to drop now that this one is on main.
