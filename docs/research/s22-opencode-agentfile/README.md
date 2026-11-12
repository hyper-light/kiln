# opencode's built-in Agentfile: evidence record

Step 22 of kiln plan 01a1cd8e862e7169a074db3d5fa33f3d ("Every kiln harness in Shards microVMs"),
planned as kiln plan 01a1ce705e2f7773af54b299f1de42f5 on step 19's shared layout (plan
01a1cdf3cc7a70e4b3ca2ec610352383). Research: 01a1ce705e307593bd11f26bf9f7b9a9, 35 findings
(26 A, 8 B, 1 C), every quote found where cited; its markdown, scripts and logs are in
[planning/](planning/). Each step of the plan adds its part below.

## Identity

| | |
|---|---|
| Policy | Shards origin/main 2464f27b: `docs/development-rules.md` sha256 `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`, `AGENTS.md` `f66d304c…`, `docs/development-evidence.md` `61613a24…`. kiln origin/main efb40ea0f: `AGENTS.md` sha256 `16b05ef80db024a7b54b0e703d42c8cf5c1f330a626d6bd85ae4525609447a4c`, `docs/ci.md` §0.3. |
| Role | Implementer of plan step 22; Shards ROOT coordinates. |
| Branch | `s22-opencode-agentfile`, from kiln origin/main efb40ea0f. |
| Owned paths | `.kiln/images/harness-opencode/`, this directory; later steps add the archive unpack in `crates/kiln/src/agentfiles/`, opencode's entries in the built-ins table and `.kiln/harness-images.yaml`, and its part of `docs/agents.md`. |

## Step 1: the pin

`.kiln/images/harness-opencode/pin.sh` pins OpenCode 2's official Linux builds into
[pin.json](../../../.kiln/images/harness-opencode/pin.json), and `pin.sh --check` checks a pin
against what it names. `pin_test.sh` holds each check to what it must refuse.

**Choices and their sources** (research findings; R02, R03):

| In pin.sh | Value | Why |
|---|---|---|
| Which opencode | OpenCode 2; 1.x refused | kiln's ACP adapter asks for `opencode/child-session-updates`, which only OpenCode 2 implements (f5, f6). |
| Version source | `https://opencode.ai/update/api/latest/cli/npm` | OpenCode 2's installer reads it; GitHub's "latest" release is OpenCode 1 (f7). |
| Archives | `opencode-linux-{x64-baseline,arm64}[-musl].tar.gz` of the release | The official Linux builds, the same bytes npm and opencode.ai serve (f8, f10); amd64 takes the baseline build, made for CPUs without AVX2 and identical to x64 in 2.2.7 (f11). |
| Verification | SHA256SUMS, GitHub's asset digest, `gh attestation verify --repo anomalyco/opencode --signer-workflow anomalyco/opencode/.github/workflows/release.yml` at the endpoint's commit and run | The release workflow attests the Linux archives' SLSA provenance with Sigstore (f9). |
| Member | exactly one entry, a regular file `opencode`, read as a stream | Each archive holds the one file (f8); nothing is unpacked to disk. |
| LICENSE | `raw.githubusercontent.com/anomalyco/opencode/<tag commit>/LICENSE`, checked to be MIT | The archives carry no license notice (f29). |
| Base | `buildpack-deps:trixie-scm@sha256:4f34e1d6…` | Steps 19–28's shared default (step 19's research f29). |

**State (R04):** a run is one fixed sequence: endpoint, version, release, SHA256SUMS, then per
archive download, SHA256SUMS line, GitHub digest, provenance, member; then LICENSE and the write.
Each failure is named (`FAILED download|endpoint|version|release|sha256sums|github-digest|
attestation|member|license|write|check`) and ends the run before anything is written; pin.json is
replaced by one rename after every check passed. **Waits and caches (R05):** none; each run
downloads into a new directory under `~/.cache/kiln-pins` and removes it.

**Runs** (this Mac: macOS 26.4.1 arm64, bash 3.2.57, Python 3.9.6, gh 2.98.0 signed in; gh is
not on the Linux runners, so the pin is made where it is):

| Run | Command | Result | Log |
|---|---|---|---|
| The pin, 2026-11-12T08:20Z | `pin.sh latest --keep` | OpenCode 2.2.7 from the endpoint (commit 5f76acb1, run 42778325289, refs/heads/dev); all four archives passed SHA256SUMS, GitHub's digest, provenance signed by release.yml at that commit and run, and the one-entry check; LICENSE MIT at the tag's commit fedc6f20; x64 = x64-baseline for gnu and musl; pin.json written | [step1-pin.log](step1-pin.log) |
| Check the pin | `pin.sh --check` (downloads again) | CHECKED 4 of 4 builds: each archive's and member's SHA-256 and size equal the pin's, each provenance at the pinned commit and run | [step1-check.log](step1-check.log) |
| Tests | `pin_test.sh <the kept archives>` | 25 passed, 0 failed: the checks accept the four real archives and refuse 1.x and non-X.Y.Z versions, a missing asset, a flipped byte, another signer workflow, repository, commit or run, two entries, another name, `./opencode`, `../opencode`, a symlink, a directory, a non-tarball and a truncated archive; whole runs that fail (version, a missing archive, a flipped byte) write no pin | [step1-test.log](step1-test.log) |

A crafted archive with a second entry or another name never reaches the one-entry check in a
whole run, because its digest fails SHA256SUMS first; that check is held by the crafted-archive
tests, which call it directly.

**Guard-removal mutants (R10)**, [scripts/step1-mutants.sh](scripts/step1-mutants.sh) →
[step1-mutants.log](step1-mutants.log): each disables one guard in a scratch copy of pin.sh
(sha256 `63ba92a5…`) and runs pin_test.sh against it. Every one is caught by the test that holds
its guard:

| Guard removed | Caught by |
|---|---|
| SHA256SUMS comparison | "one byte flipped: SHA256SUMS: it passed"; the whole run then fails at GitHub's digest instead |
| exactly one entry | "a second entry: it passed" |
| provenance from the endpoint's commit | "another commit: it passed" |
| 1.x refused | "refuses OpenCode 1.19.5", the run going on to 1.19.5's release |

**A transient failure, reproduced (R09):** in the SHA256SUMS mutant's run, a real archive failed
its provenance check ("a run missing an archive" failed at `attestation`, which that mutation
cannot touch). The four mutant suites ran at once. Reproduced with
[scripts/step1-gh-parallel.sh](scripts/step1-gh-parallel.sh): sixteen concurrent
`gh attestation verify` calls on one real archive failed 2 and then 1 times in 16, each with
`failed to choose verifier based on provided bundle issuer: public good verifier is not available
(initialization may have failed)` ([step1-gh-parallel.txt](step1-gh-parallel.txt)). Sixteen
sequential calls, as pin.sh makes them, failed none (6 to 11 s each,
[step1-gh-sequential.txt](step1-gh-sequential.txt)). So gh's Sigstore verifier can fail to start
when gh runs alongside itself; pin.sh verifies one archive at a time, and when it does fail it
fails closed, naming the attestation check and writing no pin. pin.sh does not retry: a person
reruns it.

**Unrun in step 1:** pin.sh on Linux (gh is not on the Linux runners; the script is bash 3.2 and
POSIX tools, so nothing in it is macOS's own); a release whose x64 and baseline builds differ
(none yet: pin.json records whether they do).
