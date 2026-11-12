# Step 1: Devin's pin

Kiln plan `01a1ce705e4d7008993f5c7291ef4476` (step 25 of `01a1cd8e862e7169a074db3d5fa33f3d`), step 1 of 6,
run `01a1ceb67ae9706692dc15dd4e4534a1`; research `01a1ce705e4d70089c04cee721525c8f` (basis f2, f3, f4, f6, f7,
f24). Implementer under step 25's coordinator `01a1ce6f55c3749ca87f3ef3393ec8b0`, on branch
`s25-devin-agentfile` from `1a5a5f00d`. The first commit was `ea04490d9`; this revision makes the coordinator's two
review changes on top of it. The policy read is in [../POLICY_READ.json](../POLICY_READ.json) (Shards
`docs/development-rules.md` sha256 `df2acf47…`, kiln `AGENTS.md` sha256 `16b05ef8…`).

Step 2 has since re-pinned 3000.12.43 in the form kiln's one unpacker takes, a tree with its totals, by the
pin.sh it changed: [../step2/README.md](../step2/README.md). This file records step 1's run.

## Files

| File | What |
|---|---|
| `.kiln/images/harness-devin/pin.sh` | Pins Cognition's Linux builds of Devin for Terminal into `pin.json` beside it. **Needs Linux with GNU tar**, plus bash, curl and sha256sum |
| `.kiln/images/harness-devin/pin.json` | Devin 3000.12.43, written by `pin.sh current` on adacachy2, read at 2026-11-12T08:48:32Z |
| `.kiln/images/harness-devin/pin-tests.sh` | Fixture tests over HTTPS, check removals, and `verify PIN`. Needs Linux with openssl and python3 as well |
| [pin-run.txt](pin-run.txt) | The pin's record: `pin.sh current`'s output, then `verify` |
| [pin-tests.txt](pin-tests.txt) | The test record |

Each record's header names its command, runner and the SHA-256 of every file it ran. Both runs went through
`kiln run --needs linux/amd64 --on adacachy2 --no-sync -i`, with the scripts sent over stdin. Syncing the worktree
failed twice for `ea04490d9`: runner `8fadc1ec…` did not answer the 592 MB sync in 60 s, and adacachy2's sync was
interrupted.

## Results

| Test | Result |
|---|---|
| `pin.sh current` (the plan's testing scenario 1) | Exit 0. current/ names 3000.12.43 and equals `3000.12.43/manifest.json` byte for byte (sha256 `093e9492…`). All 13 signature and checksum URLs answer 403 or 404: 13 answered 403. x86_64: `6a19997b…`, 71424183 bytes. aarch64: `1eaa07d2…`, 67806876 bytes. Each bundle holds 234 regular files and 16 directories, `bin/devin` `-rwxr-xr-x`. Both files' `unpack` is `{"format": "tar.gz"}` |
| `pin-tests.sh verify` (fresh downloads, `sha256sum` and `stat`, manifest read with Python's `json`) | 2 of 2 builds match the manifest and pin.json, each with `unpack` `{"format":"tar.gz"}` |
| `pin-tests.sh` (the plan's testing scenario 2) | 81 passed, 0 failed. Breakdown: 7 pins that must succeed; 24 refusals, each run beside an existing pin.json (left byte-identical) and with none (none written); 24 runs of pin.sh with that refusal's check taken out, each caught; and 2 more mutations, both caught |

The seven pins that must succeed: the default, `current`, a version current/ names, an older version, indented
JSON, `./`-prefixed names, and every signature URL answering 404 (it reports "0 answered 403, 13 answered 404").
Each holds `unpack` as `{"format": "tar.gz"}`. A string-valued `unpack` fails the test that holds every pin to the
object form.

The fixtures are served over HTTPS from 127.0.0.1, using a certificate the test makes and trusts through
`CURL_CA_BUNDLE`. The server answers 200 for a file and 403 for a missing one, as static.devin.ai does, unless a
scenario asks for 404 or 500. So every test runs pin.sh's own HTTPS path, and `DEVIN_PIN_BASE` takes `https://` only.

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

The review added:
- a signature URL answering 200 (`signature`: its verification must be added before pinning);
- one answering 500 (`signature-unknown`).

Added for checks of my own:
- an `http://` or `file://` base (`base`);
- a version from current/ that is not dotted numbers (`version-format`);
- a NUL byte in both manifests (`manifest-text`);
- a duplicated x86_64 entry (`platform`);
- an uppercase sha256 (`sha256-form`).

The two extra mutations:
- 404 no longer taken as absent: the 404 pin is then refused as `signature-unknown`;
- `unpack` written as the string `"tar.gz"`: the object-form test catches it.

## Review outcome

1. **`unpack`** is the object `{"format": "tar.gz"}`, step 28's committed form (pi's pin.json at `b89452884`).
   Devin needs neither `strip` nor `member`, so both are omitted.
2. **Signature URLs.** A 403 or 404 is taken as absent. A 200 refuses (`signature`): its verification must be added
   before pinning. Anything else refuses as unknown (`signature-unknown`): another status, or no answer at all (a
   failed connection or a timeout). `verified` says "answer 403 or 404".
3. **Accepted as they were:** dotted versions and the older-version wording in `verified`; printable-ASCII
   manifests (bash drops NUL bytes, so a NUL could hide inside a key); duplicate keys refused; GNU tar
   (`--absolute-names` lists names as stored, and escape quoting keeps each entry on one line).

## R01–R16, the implementer's part (the coordinator's review is owed)

- **R01:** No Rust is involved. The script fails closed: `set -euo pipefail`, every refusal is a named exit 1, and
  pin.json is written by rename only after every check passes.
- **R02:** Rests on the coordinator's research (f2, f3, f4, f6, f7, f24) and on today's measurements on adacachy2:
  the manifests, the 403s and both bundles ([pin-run.txt](pin-run.txt)).
- **R03:** No numeric literals beyond HTTP's own status codes. The constants are Cognition's URL base and platform
  names (from its manifest), step 19's base digest, and f6's signature file names.
- **R04:** Each refusal has a check name, tested one by one. There are no states.
- **R05, R06, R07, R13:** One script and its tests, with no new abstraction. The test-only `file://` path is gone
  from pin.sh. Comments say why.
- **R08, R09, R14:** Not applicable: no performance claim and no defect.
- **R10:** The records are here, with a mutation for every check. One branch has no test of its own: refusing as
  unknown when curl gets no answer. url-host's check-removal run reaches it (`curl: (6) Could not resolve host`),
  but the record keeps only that first line of stderr.
- **R11, R12:** The pin covers linux/amd64 and linux/arm64. **pin.sh needs Linux with GNU tar**, and ran only on
  adacachy2 (x86_64 CachyOS). Unverified: arm64 Linux (no such runner is listed). macOS and Windows hosts can't
  run it as written.
- **R15:** Implementer, light tests only. Commits are signed, not pushed and not merged; step 6 lands them.
- **R16:** Every run removed its work directories under `~/.cache`: 0 were left (the last lines of each record).
