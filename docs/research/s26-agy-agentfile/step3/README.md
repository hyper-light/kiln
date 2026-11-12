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

## Commits

- 09e663836 merges the coordinator's s26-agy-agentfile 4955005d4 (agy's pin, Agentfile, harness.json,
  built-in entry).
- 322ab8c26 merges step 28's hook 73fc55df9.
- 1dd6ccf94: `alone` in the one unpacker, with its unit tests.
- bdec7ebc9: agy's archive through the fetcher from kiln's own HTTP server in a test, and the live test.

## Results

(Filled in from the runs below, each from a committed tree.)
