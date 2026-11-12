# Step 1: Devin's pin

Kiln plan `01a1ce705e4d7008993f5c7291ef4476` (step 25 of `01a1cd8e862e7169a074db3d5fa33f3d`), step 1 of 6,
run `01a1ceb67ae9706692dc15dd4e4534a1`; research `01a1ce705e4d70089c04cee721525c8f` (basis f2, f3, f4, f6, f7,
f24). Implementer under step 25's coordinator `01a1ce6f55c3749ca87f3ef3393ec8b0`, on branch
`s25-devin-agentfile` from `1a5a5f00d`. The policy read is in [../POLICY_READ.json](../POLICY_READ.json) (Shards
`docs/development-rules.md` sha256 `df2acf47…`, kiln `AGENTS.md` sha256 `16b05ef8…`).

## Files

| File | What |
|---|---|
| `.kiln/images/harness-devin/pin.sh` | Pins Cognition's Linux builds of Devin for Terminal into `pin.json` beside it |
| `.kiln/images/harness-devin/pin.json` | Devin 3000.12.43, written by `pin.sh current` on adacachy2, read at 2026-11-12T08:30:27Z |
| `.kiln/images/harness-devin/pin-tests.sh` | Fixture tests, guard removals, and `verify PIN` |
| [pin-run.txt](pin-run.txt) | The pin's record: `pin.sh current`'s output, then `verify` |
| [pin-tests.txt](pin-tests.txt) | The final test run, then `verify` of the committed `pin.json` |

Each record's header names its command, runner and the SHA-256 of every file it ran. Both runs went through
`kiln run --needs linux/amd64 --on adacachy2 --no-sync -i`, the scripts sent over stdin. Syncing the worktree failed
twice: runner `8fadc1ec…` did not answer the 592 MB sync in 60 s, and adacachy2's sync was interrupted. Two
earlier tries of the pin ended before placement ("the kiln server did not answer in time").

## Results

| Test | Result |
|---|---|
| `pin.sh current` (the plan's testing scenario 1) | Exit 0. current/ names 3000.12.43 and equals `3000.12.43/manifest.json` byte for byte (sha256 `093e9492…`). All 13 signature and checksum URLs answer 403. x86_64: `6a19997b…`, 71424183 bytes. aarch64: `1eaa07d2…`, 67806876 bytes. Each bundle holds 234 regular files and 16 directories, `bin/devin` `-rwxr-xr-x` |
| `pin-tests.sh verify` (fresh downloads, `sha256sum` and `stat`, manifest read with Python's `json`) | 2 of 2 builds match the manifest and pin.json, twice: right after pinning (08:30Z) and against the committed file (08:32Z) |
| `pin-tests.sh` (the plan's testing scenario 2) | 72 passed, 0 failed. Breakdown: 6 pins that must succeed; 22 refusals, each run beside an existing pin.json (left byte-identical) and with none (none written); 22 runs of pin.sh with that refusal's guard removed, each caught |

The six pins that must succeed: the default, `current`, a version current/ names, an older version, indented
JSON, and `./`-prefixed names.

Refusals exit 1 with `pin.sh: <check>: …` on stderr.

The plan names these:
- a versioned manifest naming another version (`manifest-version`);
- current/ one byte different (`current`);
- the aarch64 entry removed (`platform`);
- a URL in another directory, or on another host (`url`);
- a bundle with one byte flipped (`digest`);
- a symlink, a hard link, a character device or a FIFO, the symlink in either bundle (`entry-type`);
- `/x` (`absolute`) and `../x` (`dotdot`);
- a setuid or a setgid file (`set-id`);
- no `bin/devin` (`bin-devin`).

Added for the checks below:
- an `http://` base (`base`);
- a version from current/ that is not dotted numbers (`version-format`);
- a NUL byte in both manifests (`manifest-text`);
- a duplicated x86_64 entry (`platform`);
- an uppercase sha256 (`sha256-form`);
- a `manifest.json.sig` present (`signature`).

## Choices beyond the plan's text, for review

1. **The `verified` claim is checked when pinning.** These 13 URLs must answer 403: `<v>/SHA256SUMS`, plus `.sig`,
   `.asc`, `.sha256` and `.sigstore.json` beside the manifest and both bundles. If one answers otherwise, pin.sh
   refuses, so that a check of what appeared gets written. Step 28's pi pin.sh does the same for attestations.
2. **Field names step 19 hasn't fixed.** pin.sh uses `manifest {url, sha256}`, `read_at` and `base {ref, digest}`,
   the names step 28's pi pin.sh uses (worktree `agentfile-pi-v1`, uncommitted). `unpack` is the string `"tar.gz"`,
   as this plan's steps 1 and 2 say. Pi's pin.sh writes an object `{format, strip, member}` instead, so the fetcher
   (step 2 here, step 19's code) must accept both forms, or the steps must agree on one.
3. **Versions are dotted numbers.** The version becomes part of every URL and of pin.json. Pinning a version that
   current/ doesn't name writes "current/ named <cv> when pinned" into `verified`, where a current/ pin has "equal to
   current/".
4. **Manifests must be printable ASCII.** Bash drops NUL bytes from what it reads, so a NUL could hide inside a
   key. With the guard removed, pin.sh pins that manifest.
5. **A key must appear once.** A manifest naming a platform twice is refused, because JSON parsers disagree on
   which copy wins.
6. **GNU tar.** `--absolute-names` lists names as stored, keeping a leading `/` and `..`. Escape quoting keeps each
   entry on one line.

## R01–R16, the implementer's part (the coordinator's review is owed)

- **R01:** No Rust is involved. The script fails closed: `set -euo pipefail`, every refusal is a named exit 1, and
  pin.json is written by rename only after every check passes.
- **R02:** Rests on the coordinator's research (f2, f3, f4, f6, f7, f24) and on today's measurements on adacachy2:
  the manifests, the 403s and both bundles ([pin-run.txt](pin-run.txt)).
- **R03:** No numeric literals. The constants are Cognition's URL base and platform names (from its manifest), step
  19's base digest, and f6's signature file names.
- **R04:** Each refusal has a check name, tested one by one. There are no states.
- **R05, R06, R07, R13:** One script and its tests, with no new abstraction. Comments say why.
- **R08, R09, R14:** Not applicable: no performance claim and no defect.
- **R10:** The records are here, with a guard-removal mutation for every check.
- **R11, R12:** The pin covers linux/amd64 and linux/arm64. pin.sh needs bash, curl, sha256sum and GNU tar, and ran
  only on adacachy2 (x86_64 CachyOS). Unverified: arm64 Linux (no such runner is listed), macOS and Windows.
- **R15:** Implementer, light tests only. The commit is signed, not pushed and not merged; step 6 lands it.
- **R16:** Every run removed its work directories under `~/.cache`: 0 were left (the last lines of each record).
