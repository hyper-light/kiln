# Plan step 21, step 1: Gemini CLI and its Node.js, pinned (evidence)

Plan 01a1ce705e2374dfbac201fe6ba98184 (step 21 [kiln] of 01a1cd8e862e7169a074db3d5fa33f3d, the Gemini
CLI Agentfile), step 1 of 6; research 01a1ce705e2472df9f3dd81be0531c9f (f5-f16). This records what
step 1 built, why, and what it ran; step 6 joins it into the step's evidence record.

## Identity

- Role: implementer, agent 01a1ce6ef65e723aad4151e03f227874; ROOT 01a148884a4770fdaf0111f3f792a248.
- POLICY_READ: `POLICY_READ.json` beside this file (Shards ac87190d3, docs/development-rules.md sha256
  df2acf47…, R01-R16 read in full; kiln 096f860e7, AGENTS.md 16b05ef8…, docs/ci.md §0 be867453…).
- Owned paths: `.kiln/images/harness-gemini/` and `docs/research/s21-gemini-agentfile/`.
- Base: branch `s21-gemini-agentfile`, fast-forwarded to kiln main 096f860e7. Step 1 needs nothing of
  step 19's or step 30's code (neither is on main).

## What was built

`.kiln/images/harness-gemini/pin.sh [latest | <dist-tag> | <x.y.z>]` writes `pin.json` beside it, only
when every check passed (a temporary file renamed into place). It is bash around a Python program
(stdlib only), fetching with curl whose redirects are held to HTTPS.

1. **Gemini CLI** (f5-f8): the version from npm's dist-tags (or the version given); its registry
   record, whose name, version, single sha512 integrity and tarball URL (under the registry) are
   checked; every `dist.signatures` entry verified with openssl as ECDSA P-256 over
   `<name>@<version>:<integrity>` with the unexpired registry key of its keyid (npm's documented
   method); the tarball's SHA-512 equal to the integrity. The tarball is then read: every entry a
   regular file or directory under `package/`, no link, device or set-ID bit, no path leaving it (what
   step 25's unpack refuses, refused at pin time), and its `package.json` naming the package, the
   version and `bin.gemini = bundle/gemini.js`.
2. **Node.js, derived** (R03; f9-f11): the node-version matrix of the 'Test (Linux)' job in Gemini's
   `ci.yml` at the pinned tag; nodejs/Release's `schedule.json` (newest tested major with
   start <= today < end); that major's newest release in nodejs.org's index, which must list linux-x64
   and linux-arm64; `SHASUMS256.txt.asc` verified by `gpgv` against nodejs/release-keys'
   `gpg-only-active-keys/pubring.kbx` at a commit (main's, read from GitHub, or `PIN_RELEASE_KEYS_COMMIT`),
   its own `--homedir`; each `.tar.gz` (f14) checked against the signed SHA-256 and found to hold a
   regular `bin/node` and `LICENSE`.
3. **pin.json**: the version, channel and time read; how each part was verified (registry keyids and
   integrity; Node's derivation inputs, signer fingerprint, keyring commit and SHA-256, and the
   SHA-256 of `ci.yml`, `schedule.json` and `SHASUMS256.txt.asc`); the base; the version smoke as data
   (`bin/gemini --version` prints the version); four builds.

### The unpack shape: step 25's, not the plan's first words

The plan's step 1 wrote the Node archive as one `{strip, to, only}` unpack. Before this step ran,
step 25 (Devin) had committed the shared unpack, `crates/kiln/src/agentfiles/unpack.rs` (a04cffd0c on
`s25-unpack`, from step 30's branch), and step 28 (pi) uses it: an `unpack` is `{format: "tar.gz",
strip?}` for a whole tree at the file's `path`, or `{format: "tar.gz", member}` for one regular file
written at `path` with `mode`; links, devices, set-ID bits, escaping paths and a name already there
are typed refusals. The plan says to reuse whichever unpack lands first, so pin.json takes that shape:

- the npm tarball, `{strip: 1}` at `path: "."`: its 449 regular files under `package/` land at the
  harness's root (`bundle/`, `package.json`, `LICENSE`, `README.md`), none clashing with
  `harness.json`, `bin/`, `etc/` or `node/`;
- Node's archive twice, as members: `bin/node` at `node/bin/node` (0755) and `LICENSE` at
  `node/LICENSE` (0644). A whole-tree unpack would refuse its three symlinks (`bin/npm`, `bin/npx`,
  `bin/corepack`; f15).

Consequences for later steps: step 2's launchers start `/harness/gemini/node/bin/node
/harness/gemini/bundle/gemini.js` (gnu) and `node /harness/gemini/bundle/gemini.js` (musl). Step 3
reuses step 25's unpack rather than writing one. The fetcher (step 19's, not yet written) should fetch
each distinct URL once per context, since Node's archive is named twice per platform.

### Literals and their sources (R03)

| Literal | Source |
|---|---|
| `@google/gemini-cli`, `bundle/gemini.js` | npm's record and the package's `bin` (f5), checked at each pin |
| `x64`/`arm64` to `linux/amd64`/`linux/arm64` | Node's file names (index.json) and OCI's platform names |
| `node/bin/node` 0755, `node/LICENSE` 0644 | the archive's executable and its license text; step 25's member mode takes a pinned mode |
| `'Test (Linux)'` job, `node-version` | Gemini CLI v0.68.0's `.github/workflows/ci.yml` (f9) |
| start <= today < end | nodejs/Release's schedule fields (f9, f10) |
| `sha512-` + 86 base64 + `==` | a SHA-512 integrity (64 bytes) as npm writes it (f7) |
| `ecdsa-sha2-nistp256` | npm's registry keys and its signature docs (f7) |
| `gpg-only-active-keys/pubring.kbx` | Node's README, release keys (f11) |
| the base `buildpack-deps:trixie-scm@sha256:4f34e1d6…` | the shared default of every built-in harness (step 19, f13) |

## What ran

On adacachy2 (Linux 7.3.3-1-cachyos x86_64; curl 8.23.0, gpgv 2.4.9, OpenSSL 3.6.5, Python 3.14.9),
2026-11-12T10:27:08Z, through `kiln run --no-sync --on adacachy2 --needs linux/amd64 -i` with the two
scripts sent base64-encoded and their SHA-256 checked on arrival (pin.sh 4845cb8b…, pin-tests.sh
54949615…). The first try, placed by kiln on another runner, was refused there before starting ("has no
room here now: it isn't started") and ran nothing. Logs: `evidence/pin-run-20261112T102708Z.log` (the
run) and `evidence/pin-tests-20261112T102708Z.log` (pin-tests.sh's own log).

- **A, the pin**: Gemini CLI 0.68.0 (npm's latest) on Node.js v24.22.3: tested majors 20, 22 and 24;
  24 ends 2028-04-30; SHASUMS256 signed by 5BE8A3F6C8A5C01D106C0AD820B1A390B168D356; keyring at
  nodejs/release-keys 481637f8. pin.json sha256 7cf3b6bf…, copied back byte for byte.
- **A, every pinned file downloaded again**: 3 of 3 distinct files match pin.json's SHA-256 and size
  (Node x64 061ec3e3…, 58,330,843 bytes; Node arm64 261af2af…, 58,051,304 bytes; the npm tarball
  325099dd…, 20,975,501 bytes).
- **B, a local fixture of the same records and files**: 9 of 9 cases as expected. The control pins
  the same builds, files and signer as A. Each change is refused, names its check, and writes no
  pin.json:
  - a byte of `dist.integrity` flipped: the registry signature does not verify;
  - another version's signatures (0.67.0-nightly.20261104): the registry signature does not verify;
  - a byte of the npm tarball flipped: its SHA-512 is not the signed integrity;
  - a hash in `SHASUMS256.txt.asc` edited: gpgv, BAD signature;
  - an empty keyring: gpgv, no public key;
  - a byte of Node's arm64 archive flipped: its SHA-256 is not the signed one;
  - index.json without linux-arm64: no linux-arm64 build;
  - `ci.yml` testing only 16, 18 and 20: no tested major still supported.
- No pin.sh work directory left behind (0).

## R01-R16

| Rule | Step 1 |
|---|---|
| R01 | No Rust; the script refuses every malformed input with a message and exit 1, and writes nothing. |
| R02 | Research f5-f16 (npm, Node, Alpine, kiln's crates); step 25's unpack reused (above). |
| R03 | The Node version derived from Gemini's CI and Node's schedule; literals in the table above. |
| R04 | One terminal outcome each: pin.json written whole, or a named refusal and none. |
| R05 | No cache; the work directory is temporary and removed on exit. |
| R06, R07, R13 | One pass, top to bottom; comments say why (sources), not what. |
| R08, R14 | No performance claim. |
| R09 | No defect reported. |
| R10 | Both logs kept; every refusal exercised by a mutation that changes one thing. |
| R11 | Pins linux/amd64 and linux/arm64, gnu and musl; pin.sh itself ran on Linux x86_64 only. |
| R12 | Step 25's unpack shape; step 28's pin.sh conventions (PIN_* overrides, PIN_BASE, PIN_OUT). |
| R15 | Implementer scope: the script and its light tests; no real-VM run here (step 4). |
| R16 | Nothing reclaimed. |

## Unverified

- pin.sh on macOS or Windows (not needed: pins are made on Linux runners).
- The fetcher that consumes pin.json (step 19's) is not written; its use of these entries is step 3's.
- arm64's Node archive was checked by digest and listing only; its bin/node runs in step 4.
