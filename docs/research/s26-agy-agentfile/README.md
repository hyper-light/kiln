# Antigravity's CLI as kiln's built-in Agentfile harness-antigravity

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26: Google's Antigravity CLI, agy, embedded in
kiln as step 19's layout has every harness. Its own plan is 01a1ce705e55753182217afdfe607de9, and
its research is 01a1ce705e557531867501c310184e8d. A person reads about it in
[docs/agents.md](../../agents.md), under "Notes per harness": Antigravity CLI (agy) in a Shards
microVM.

This is the step's evidence record (its plan's step 6). It lists every record, what stayed
unverified, and how each of Shards' rules R01 to R16 applies.

## POLICY_READ

- Shards origin/main a7dbf1446, read 2026-11-12. These are the same files as at 7ad28fab1 and
  8f3cd5b74, where the step began, and at fe44bc8ca:
  - docs/development-rules.md `df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`
    (R01-R16);
  - AGENTS.md `f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41`;
  - docs/development-evidence.md
    `61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63`.
- kiln origin/main 32a99c8dc: AGENTS.md
  `16b05ef80db024a7b54b0e703d42c8cf5c1f330a626d6bd85ae4525609447a4c`, the same as at 94639db7b,
  7d0db0768 and 80fd748cb. CLAUDE.md is `@AGENTS.md`.
- Role: implementer. Shards ROOT is the coordinator. This branch, s26-agy-agentfile, holds the
  plan's steps 1, 2, 4 and 6. Kiln started subagents of its own for steps 3 and 5.
- Owned paths:
  - `.kiln/images/harness-antigravity/`;
  - the antigravity entry in `crates/kiln/src/agentfiles/builtin.rs`, and
    `builtin/antigravity_tests.rs`;
  - `crates/kiln/tests/agentfiles.rs`, main's fix, the same blob as step 25's (d4deb53a7);
  - the Antigravity note in docs/agents.md;
  - this directory, except `step3/` and `step5/`, which are steps 3's and 5's.

## The plan's steps

| Step | What | State |
|---|---|---|
| 1 | pin.sh, pin_test.sh, pin.json and pin.log | Met: agy 1.5.7 pinned. |
| 2 | The Agentfile, harness.json, the built-in entry and its tests | Met. |
| 3 | The shared fetcher unpacks agy's archive | Kiln's subagent's, on branch s26-unpack. Not in this branch. |
| 4 | The microVM build, smokes, probe, control and other bases | Met, for 1.5.6 and again for 1.5.7. |
| 5 | CI's prebuild and the local fallback | Held: Shards has no v0.8.0 tag yet, and step 19's workflow and image() aren't on kiln main (32a99c8dc). |
| 6 | The docs, this record and the landing | This commit. |

## Records

- `.kiln/images/harness-antigravity/pin.log` is pin.sh's run on kiln-a094a12d at
  2026-11-12T16:06:21Z, from commit 2130dad30:
  - pin_test.sh passed all 12 of its cases;
  - pin.sh pinned 1.5.7, writing a pin.json with SHA-256 `6102386d…`;
  - asked for 1.5.6, pin.sh exited 2, since the updater serves no manifest for it.
- [evidence/e_step2_tests.txt](evidence/e_step2_tests.txt) and
  [evidence/e_step2_tests_on_main.txt](evidence/e_step2_tests_on_main.txt) are step 2's tests,
  Shards' check and clippy, each run on adacachy2 from step 2's committed tree.
- [evidence/e_smoke_bytes.txt](evidence/e_smoke_bytes.txt) holds the bytes of 1.5.6's smoke.
- [evidence/step4/](evidence/step4/README.md) is step 4 for 1.5.7, on both platforms. 1.5.6's
  records are that directory at 3423c8df.
- [evidence/e_step6_tests.txt](evidence/e_step6_tests.txt) is the tests at 24d65d633. Its built-in
  Agentfiles, their tests and the integration test are the ones that land: what kiln main brought
  after it changes none of them.
  [evidence/e_step6_tests_red.txt](evidence/e_step6_tests_red.txt) is the first such run, at
  338f78473, where clippy refused one line of the tests (91c13a756 fixes it).
- [scripts/](scripts/) holds what made each record. Among them is
  [verify-commit-sig.sh](scripts/verify-commit-sig.sh), which checks a commit's signature against
  kiln's CA.

## HOME and the launcher

The plan said to start agy through `env HOME=/home/antigravity agy …` until kiln's shards had
Shards' run.env fix, then to move HOME into run.env. That fix, Shards main 997f4e47e (2026-11-12
00:05 -08:00), was on Shards' main before step 2's first commit (f2990f9c3, 06:04 -08:00). So
harness.json sets `HOME=/home/antigravity` in run.env, as it has since that first commit, and no
launcher was ever committed.

- No release before v0.8.0 needs one: v0.7.0 starts no harness from its harness.json at all. That
  came with 1da9132b7, which v0.7.0 doesn't contain. ROOT is cutting v0.8.0 as the first release
  with both.
- 997f4e47e is the revision that let the launcher go. Step 4 ran on Shards main 8f3cd5b7, which
  contains it. There the probe found HOME /home/antigravity and uid and gid 200000, and its writes
  landed in the home volume.
- Step 2's tests on kiln main's table (e_step2_tests_on_main.txt) and step 4's smokes ran with
  HOME in run.env.

## Single observations

These are from step 4 for 1.5.7, one run each, with shards' test-release builds. They aren't
measurements of shards.

| | kiln-a094a12d (x86_64, KVM) | adas-mac-studio.local-2 (M3 Ultra) |
|---|---|---|
| Building both platforms | 23.1 s | 337.8 s |
| harness-antigravity:s26, disk / content | 1.12 GB / 477 MB | 1.13 GB / 477 MB |
| The base, disk / content | 684 MB / 293 MB | 710 MB / 293 MB |
| The first smoke, downloading the guest kernel | 2.1 s, linux/amd64 | 23.2 s, linux/arm64 |

Each image's manifests are 241,442,079 bytes (linux/amd64) and 235,427,336 bytes (linux/arm64) on
Linux, and 241,442,052 and 235,427,325 bytes on the Mac.

The probe's image on other bases, linux/amd64, on Linux:

| Base | Disk | Content | Build | agy's TLS |
|---|---|---|---|---|
| alpine:3.24.4 | 363 MB | 102 MB | 2.8 s | Passes. |
| debian:trixie-slim | 467 MB | 128 MB | 3.8 s | x509: certificate signed by unknown authority |
| busybox:1.37 | 357 MB | 101 MB | 2.7 s | The same. |

1.5.6's builds took 23.3 s and 370.9 s (3423c8df).

## What stayed unverified

- **A signed-in turn.** No sign-in reaches any microVM here, so every turn ended "authentication
  failed or timed out". Step 33 brings the sign-in.
- **agy's embedded ripgrep.** For searches, agy extracts a ripgrep it carries and runs it, unless
  `ANTIGRAVITY_DISABLE_EMBEDDED_RIPGREP` is true (research f29). Where it extracts it, and whether it
  runs from the harness's home volume, stayed unverified: a search needs a signed-in session.
- **The fetcher on Google's own archives.** Step 3's live test covers it, on step 3's branch.
  Here, step4.sh laid the contexts out from the same pin, holding each bin/agy to the pin's member
  SHA-256.
- **Step 5:** CI's prebuild, the registered digest, image() and its fallback. Step 19's own steps 6
  (image()) and 7 (the CI workflow), which it plugs into, aren't on kiln main.
- **ROOT's end-to-end runs (master step 34).** They're ROOT's, and none ran here.
- **Reproducible images.** The Linux and Mac builds of one context differ: indexes cbab89ce… and
  a6f44701…, whose manifests differ by a few bytes each.
- **IPv6.** Connects over IPv6 fail on Shards 8f3cd5b7, with the grants or without.
- **Other bases on linux/arm64.** Only linux/amd64 ran them.
- **Windows hosts (WHP) and Linux arm64 hosts (KVM).** Neither ran. No kiln runner is an arm64
  Linux machine with KVM, and no WHP run was in this step.
- **A Shards release.** None has the fix yet. The microVM runs used Shards main 8f3cd5b7. Shards'
  check in the tests used v0.7.0 (step 2) and 8f3cd5b7 (step 6).

## R01 to R16

Each rule, the proof that applies, and what's left.

- **R01, production never panics.** Production gains data alone: the Agentfile, harness.json and
  pin.json, embedded by `include_str!`, and one `Builtin` entry. No new code path runs in kiln.
  The tests assert, as tests may. Clippy with `-D warnings` is clean at 24d65d633
  (e_step6_tests.txt). Left: none.
- **R02, research.** Each decision rests on primary sources and live runs:
  - the pin checks what Google's own installer checks
    (`https://antigravity.google/cli/install.sh`): the SHA-512 in the updater's manifest;
  - the base: three others ran (alpine, trixie-slim and busybox), and those without CA
    certificates fail;
  - HOME rests on Shards' source (997f4e47e) and on the probe;
  - the grants rest on the probe and the control: HTTPS by name and by IPv4 address, TCP 80
    shut, and nothing without them.

  The research record cites each source. Left: none. The plan named agy, so no other CLI was
  weighed.
- **R03, constants.** Every literal in the data comes from a source:
  - the version, execution id, URLs, digests and sizes are what Google's updater served, read by
    pin.sh;
  - the base digest is step 19's pin;
  - 443/tcp: agy speaks HTTPS alone, as measured;
  - /workspace is the plan's, and /home/antigravity follows Claude's /home/claude;
  - /tmp/.cache lies on the harness's scratch.

  pin.sh is a maintainer's tool, not production. Its `curl --retry 3` decides only how often a
  maintainer's run retries a failed fetch before it stops, never what it pins. Left: none in
  production.
- **R04, explicit states.** No state machine or wire type is added. The unpacker's typed errors
  are step 3's.
- **R05, deliberate abstractions.** No interface, cache or wait is added: one row in the existing
  table, which the existing views and fetcher read. step4.sh's `sleep 600` is in the evidence
  harness. It only keeps a run's microVM up while the probe is read.
- **R06, complexity.** Each check in antigravity_tests.rs is a short function with one subject:
  the pin, the Agentfile, harness.json, the smoke, or sign-ins.
- **R07, comments.** The Agentfile's comments give each line's reason and source: the sign-in,
  the base, the grants, the volumes, HOME, the Shards floor, the updater and the cache. The table
  entry names the plan step.
- **R08, measured results.** Doesn't apply: there's no performance claim. The sizes and times
  above are single observations.
- **R09, live reproduction.** Each issue was seen live before anything changed:
  - init's HOME=/tmp, which ROOT reproduced and fixed (997f4e47e);
  - TLS on bases without a CA bundle, in agy's own log (step 4);
  - pin.sh unable to pin 1.5.6 once 1.5.7 was out, in pin.log's exit 2;
  - IPv6, in the probe's failed connects. That one isn't fixed here, and ROOT has the report;
  - main's tests/agentfiles.rs expecting no built-in, failing on main's tree since Goose's entry
    (6fa954363), as 7860f73bd says.

  Left: that last red run was observed, but not kept as a file.
- **R10, durable evidence.** The records above, with the scripts that made them. Each test feeds
  changed copies of the real files, and each copy must fail: the commit messages and the tests
  list them. Before and after for the one fix this step made in its own tests: clippy 1.99.1's
  cmp_owned refused `other_release != pin.to_string()` at 338f78473 (e_step6_tests_red.txt).
  Clippy's suggested `other_release != pin` would have made that check always pass, so 91c13a756
  compares the parsed pin instead (e_step6_tests.txt). Left: none for this step's changes.
- **R11, platforms.** agy publishes Linux builds alone, static musl, for amd64 and arm64. Both
  went into one index, and both smoked: linux/amd64 under KVM, and linux/arm64 under
  Hypervisor.framework. On amd64, a glibc base (trixie-scm) and a musl base (alpine) both ran agy.
  Unverified: WHP, Linux arm64 under KVM, and the other bases on arm64.
- **R12, shared implementations.** This uses step 19's fetcher and table, and step 25's unpacker
  through step 3. There is no second fetcher or unpacker.
- **R13, simple.** Three data files, one table row, one maintainer script and its test, and one
  docs note.
- **R14, profiling.** Doesn't apply: no performance path changes. ROOT owns profiling.
- **R15, roles and merges.** As implementer, this step ran light tests alone:
  - the module's tests and the integration test;
  - Shards' checks and clippy;
  - step 4's microVM build and smoke-level runs, which the plan's step 4 asks for.

  Every commit is signed with the session's gits, on named paths, and each signature verifies
  against kiln's CA (SHA-256 fingerprint C8:15:D0:46:…:47:49). A one-byte change to each is
  refused. Nothing was force-pushed. The landing on kiln's main follows the person's word of
  2026-11-12: subagents "merge their work to main and end". As a current explicit instruction,
  that overrides R15's "they do not merge themselves".
- **R16, runner space.** Removed:
  - each step 4 run's Shards home and contexts, once its daemon had stopped and no process was
    left in them. 1.5.6's homes were 12 GB on Linux and 5.6 GB on the Mac, beside their
    contexts. 1.5.7's homes and contexts together were 9.6 GB and 7 GB;
  - this step's 29 GB target directory on adacachy2, when its disk reached 99%.

  This Mac has 118 GiB free. Left: adas-mac-studio.local was 100% full (145 MiB free) when last
  seen. Nothing on it is this step's, and ROOT has the report. The test target on kiln-a094a12d
  (`~/.cache/s26-agy-step6/target`) is removed after the landing.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-antigravity/Agentfile` grants these 13 hosts by name (Shards D236), on TCP 443, HTTPS's port
(RFC 9110 §4.2.2, and the port Antigravity's allowlist names), and nothing else. Antigravity is Google's. Each host is
cited to Google's own documentation, saved as Google publishes it in `evidence/hosts/` (`SOURCES.tsv`: URL, fetch date
2026-11-13, SHA-256). Each is also cited to agy's own pinned build (1.5.7), quoted in `evidence/hosts/agy-build.txt`
with the build's URL and SHA-256, matched against pin.json's as it downloaded. The build embeds Google's installed-app
OAuth client secrets; the excerpt redacts them.

**Which sign-ins the image serves** is ROOT's call of 2026-11-13: all four that Antigravity documents for agy, each run
narrowed at launch to its own (`docs/research/s18-host-in-vm/evidence/part4/root-2026-11-13-agy-signins.txt`). The
reason is the person's bar of zero breaks against today. Kiln runs agy on the host today, where an enterprise sign-in
works, and an image naming fewer would break those users in a microVM. A run may drop an image's destinations but
never add one (D236). So kiln's launch (goal 60, plan step 17) keeps only the destinations of the session's own
sign-in.

| Host | For | Google's page | agy's build |
|---|---|---|---|
| `cloudcode-pa.googleapis.com` | a Google account's model service (sign-ins 1 and 3, 4) | `enterprise.md`, "Network allowlists": "Cloud Code Telemetry & Client Services" `cloudcode-pa.googleapis.com`; and "the client connects to the global endpoints (`businessaicode.googleapis.com` and `cloudcode-pa.googleapis.com`) during sign-in" | `https://cloudcode-pa.googleapis.com` |
| `oauth2.googleapis.com` | Google's token endpoint: a sign-in's code exchange and refresh (1, 3, 4) | "OAuth 2.0 for iOS & Desktop Apps", `google-oauth-native-app.html`: `https://oauth2.googleapis.com/token` | `https://oauth2.googleapis.com/token` |
| `generativelanguage.googleapis.com` | the Gemini API, signed in with a key (2) | `install.md`, "Using a Gemini API key": "Model requests go directly to the Gemini API"; the Gemini API reference, `gemini-api-reference.md`: `https://generativelanguage.googleapis.com` | `https://generativelanguage.googleapis.com` |
| `businessaicode.googleapis.com`, `businessaicode.us.rep.googleapis.com`, `businessaicode.eu.rep.googleapis.com` | AI Developer Tools orchestration, global, US and EU (3, 4) | `enterprise.md`, "Network allowlists", each by name | `businessaicode.googleapis.com`, 28 times (its Business AI Code API) |
| `aiplatform.googleapis.com`, `aiplatform.us.rep.googleapis.com`, `aiplatform.eu.rep.googleapis.com` | Vertex AI / Gemini Enterprise API, global, US and EU (3, 4) | the same table, each by name | `aiplatform.googleapis.com`, 316 times (its Vertex AI client) |
| `discoveryengine.googleapis.com`, `us-discoveryengine.googleapis.com`, `eu-discoveryengine.googleapis.com` | Discovery Engine, global, US and EU (3, 4) | the same table, each by name | its API's package (`google/cloud/discoveryengine`), not the host: the documented allowlist alone |
| `sts.googleapis.com` | Workforce Identity Federation's token exchange and refresh (3, its BYOID/WIF sign-in; 4 with an external-account ADC file) | "Obtain short-lived credentials for workforce identity federation", `google-workforce-short-lived-credentials.html`: `"token_url": "https://sts.googleapis.com/v1/oauthtoken"` | `https://sts.googleapis.com/v1/oauthtoken` |

**The sign-ins and their hosts**, from `install.md`'s "Authenticate" and `enterprise.md`'s "Sign in and license
selection". This is the rule kiln's launch narrows by.
1. **A personal Google account** (the browser sign-in; Free, Pro or Ultra): `cloudcode-pa`, `oauth2`.
2. **A Gemini API key** (`modelProvider: "gemini"` in `~/.gemini/antigravity-cli/settings.json` with `GEMINI_API_KEY`
   set): `generativelanguage` alone. A `GOOGLE_GEMINI_BASE_URL` is a host the person gives, and isn't granted.
3. **Gemini Enterprise through Google Cloud SSO, or BYOID/WIF** (the license's project and location are saved under
   `gcp` in that settings file): `cloudcode-pa`, `oauth2` and `businessaicode.googleapis.com`, plus the location's
   hosts. For `global`, that's `aiplatform.googleapis.com` and `discoveryengine.googleapis.com`. For `us` or `eu`, it's
   that location's `businessaicode`, `aiplatform` and `discoveryengine` twins. WIF adds `sts`.
4. **ADC** (`AGY_ADC_AUTH=true`, or `enableAdc`; the location is `GOOGLE_CLOUD_LOCATION`, `global` by default): sign-in
   3's hosts for that location. An authorized-user ADC file refreshes at `oauth2`; an external-account one exchanges at
   `sts`.

**Not granted:**
- **The mTLS twins** (`businessaicode.mtls.googleapis.com`, `aiplatform.mtls.googleapis.com` and the three
  `discoveryengine` `*.mtls.googleapis.com`). Google's own rule for its clients is to use the mTLS endpoint only "if a
  device certificate is available and the default regular endpoint otherwise". The certificate comes from the
  platform's credential store through EndpointVerification (AIP-4114, `aip-4114-device-certificate-authentication.md`),
  and no microVM holds one. An organization whose policy requires certificate-based access can't sign in to agy from a
  microVM: docs/agents.md says so.
- **Logging and updates:** `play.googleapis.com`, and the updater's `storage.googleapis.com`, which is off.
- **The person's browser:** `accounts.google.com` and `auth.cloud.google`, the sign-in pages.

**Changed from part 2** (2 hosts):
- Added sign-in 2's host and sign-ins 3 and 4's ten: the nine documented non-mTLS enterprise hosts, and `sts`.
- `oauth2` and `cloudcode-pa` were already there.
- Before applying the call, part 4 told ROOT that sign-ins 3 and 4 also take `oauth2`, and WIF takes `sts` once the
  build and Google's docs showed it. ROOT raised no objection.
