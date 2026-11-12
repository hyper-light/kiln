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
| Tests | `pin_test.sh` (downloading the archives afresh) | 27 passed, 0 failed: the checks accept the four real archives and refuse 1.x and non-X.Y.Z versions, a missing asset, a flipped byte, one byte of SHA256SUMS changed, another release's SHA256SUMS, another signer workflow, repository, commit or run, two entries, another name, `./opencode`, `../opencode`, a symlink, a directory, a non-tarball and a truncated archive; whole runs that fail (version, a missing archive, a flipped byte) write no pin | [step1-test.log](step1-test.log) |

The plan's tests for this step, against what ran:

| The plan's test | Ran as | Outcome |
|---|---|---|
| Run pin.sh latest; then download each pinned archive and hash it and its member (4 builds) | `pin.sh latest`, then `pin.sh --check` | CHECKED 4 of 4 builds |
| One byte flipped in a downloaded archive | "one byte flipped: SHA256SUMS", "one byte flipped: GitHub's digest", "a run with one byte flipped" | refused (sha256sums, github-digest); no pin written |
| --signer-workflow naming another workflow | "another signer workflow" (and "another repository", "another commit", "another run") | refused (attestation) |
| Version 1.19.5 | "refuses OpenCode 1.19.5" (and "refuses 2.2") | refused (version); no pin written |
| An asset name that does not exist | "a missing asset fails to download", "GitHub lists no such asset", "a run missing an archive" | refused (download, github-digest); no pin written |
| A crafted tar.gz with two members | "a second entry" (and the other crafted archives) | refused (member) |

Step 19's tests flip a byte of its signed manifest and swap its key. opencode's release has no
signed manifest and no key of its own: SHA256SUMS lists the digests (one byte of it changed, and
2.2.6's list given as 2.2.7's, are both refused), and the Sigstore certificate stands where a key
would (another repository's and another workflow's are refused).

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

## Step 2: the Agentfile, harness.json and the table

[Agentfile](../../../.kiln/images/harness-opencode/Agentfile) and
[harness.json](../../../.kiln/images/harness-opencode/harness.json) beside the pin, and opencode's
entry in step 30's table, `crates/kiln/src/agentfiles/builtin.rs` `ALL` (branch
`p/agentfile-store-v1`, merged into this one at bba4ced87, so this branch lands after it). `Builtin`
gains the two fields the shared layout agreed: `pin` (pin.json) and `files` (harness.json).

**Choices and their sources:**

| | Value | Why |
|---|---|---|
| Base | `ARG BASE` = pin.json's base, by digest | Steps 19–28's default; opencode's glibc builds need glibc 2.17 at most, so no RUN (f12). |
| Variants | `ARG OPENCODE_LIBC=gnu`; `base-musl` adds `libstdc++ ripgrep` | The musl builds need libstdc++, which brings libgcc (f13, f35); on arm64 the ripgrep opencode fetches is a glibc build (f22). BuildKit builds only the stage asked for. |
| Harness | `HARNESS opencode FROM ./opencode-${TARGETARCH}-${OPENCODE_LIBC}` | The directories `kiln agentfiles context` writes, every file checked against the pin (f26). |
| Reach | the bring-up's three lines, for opencode | DNS and TCP 443 out cover its providers, models.dev, npm and GitHub (f21); its ACP server is on the domain's own loopback, which needs no grant (f15, f16). Step 9's by-name grants replace them. |
| Volumes | `/workspace` and `/home/opencode`, the harness's alone | The project, and the user's opencode (settings, sessions and sign-in in opencode.db, cache) under the four XDG roots (f17, f18, f19). |
| harness.json | OSI full form: `["opencode", "acp"]`, the four XDG roots under /home/opencode, `OPENCODE_DISABLE_AUTOUPDATE=1` | kiln's program and ACP arguments (f4); opencode reads its XDG roots before HOME (f17); its updater skips its check with the variable set (f20). Only fields Shards' OSI config accepts (it refuses unknown ones). |
| Smoke | pin.json `smoke`: `bin/opencode --version` with `HOME=/tmp` prints `opencode v<version>` | In the pin, which owns the version, for whatever runs the smoke (step 19's image lookup, CI); step 19's code has no form of its own yet. Every opencode command creates its directories first, hence HOME (f18, f32). |
| Server image | the Dockerfile's build stage copies `.kiln/images/` | Its build context held only `Cargo.*`, `.cargo/`, `crates/` and `vendor/`: the `include_str!`s of `.kiln/images/` would fail to compile there. Every other build of kiln is from a whole checkout. |

**harness.json and Shards' init (2026-11-12):** Shards' init gives a harness `PATH`, `HOME=/tmp` and
`SHARDS_DOMAIN` before run.env, and until its fix (branch `p/harness-env-last-wins`) a run.env
`HOME` or `PATH` would be shadowed. opencode's run.env names neither: only variables init doesn't
set, which hold today.

**Runs:**

| Run | Command | Result | Log |
|---|---|---|---|
| The pin again, with its smoke | `pin.sh latest` (this Mac, 2026-11-12T08:51Z) | the four archives passed every check again; pin.json gains `smoke`: `bin/opencode --version` with `HOME=/tmp`, printing `opencode v2.2.7` | [step2-pin.log](step2-pin.log) |
| The table's tests | `cargo test --locked -p kiln --lib agentfiles::` on a Mac Studio (macOS arm64, rustc 1.99.1) | 24 passed, 0 failed: step 30's 18 and the table's 6 (`every_builtin_is_a_valid_agentfile_once`, `opencodes_pin_is_opencode_2s_four_builds`, `opencodes_harness_json_starts_it_as_kiln_does`, `opencodes_agentfile_is_the_layouts`, `no_builtin_carries_a_credential`, `the_server_image_builds_with_the_builtin_agentfiles`) | [step2-tests.log](step2-tests.log) |
| Clippy | `cargo clippy --locked -p kiln --all-targets -- -D warnings`, there | clean once a needless lifetime in a test helper was elided (its first run named it) | [step2-tests.log](step2-tests.log) |
| Guard-removal mutants (R10) | [scripts/step2-mutants.sh](scripts/step2-mutants.sh), there | each caught by the test that holds its guard: kinds.rs's opencode ACP arguments changed (the harness.json test), the Dockerfile's COPY removed (the server image test), an `ENV OPENAI_API_KEY` added to the Agentfile (the credential test, and the layout test), a `HOME` added to harness.json's env (the harness.json test); restored, the six pass | [step2-tests.log](step2-tests.log) |

The tests also hold each check to what it must refuse, inside the test itself: a 63-character
digest, linux/arm64 missing, an archive not unpacked and OpenCode 1 in the pin; an ENV and a
credential-named env in the files.

**How the run reached a machine:** a kiln sync of the whole worktree from this laptop timed out
("did not answer the sync in 60s"), as other agents found tonight, and the Linux runners' queue was
hours long. The run went as a git bundle of the branch from a commit a full clone on the Mac Studio
holds (e5dc43f44; a first try from a shallow clone lacked the bundle's prerequisites), plus a patch
of the uncommitted work, applied in a clone of its own there.

**Unrun in step 2:** the tests and clippy on Linux and clippy on Windows (the branch's gate, at
landing); a `docker build` of the server image with `.kiln/images/` copied (CI's image job, after
landing).

**Told:** step 19 and ROOT could not be reached (their inboxes refused messages, 429). For step 19:
the Dockerfile's COPY (a production `include_str!` of `.kiln/images/` fails `docker build` without
it), the two fields, and the smoke's form in pin.json. To send when they take messages.
