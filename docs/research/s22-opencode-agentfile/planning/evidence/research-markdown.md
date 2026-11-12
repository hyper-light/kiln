**Research:** Plan step 22 [kiln]: the opencode Agentfile is embedded in kiln, as step 19's goal says. Rules as plan step 16's goal. Restarted at the person's word after ROOT stopped this step's first agent: read its transcript /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards--claude-worktrees-g121/01a1cdf2-73fe-7771-8d4a-693f546b9527.jsonl first and continue from its findings; don't redo what it verified. Run the relevant unit tests and light integration tests, land on kiln's main as kiln's rules allow, report and end. Before asking the person for anything, research and find a way to unblock yourself (the person's rule).

35 findings (A 26, B 8, C 1) from 56 sources (45 internal, 11 external)

- **C · 65 (moderate)** Step 22 is step 19 for opencode (program opencode, ACP through its acp command), accepted as step 19 is: built for both arches from pinned sources, no secrets, granting only what it lists, starting and printing its version in a microVM, and embedded in kiln with its pin. [1]
  _1 independent source (kiln's records); 2 of 2 quotes found where cited_
  - [1] "As step 19, for opencode: program opencode, ACP via acp." (step 22, description) · quote found
  - [1] "builds for both arches from pinned sources, carries no secrets, grants only what it lists, starts and prints its version in a microVM, and is embedded in kiln with its pin" (step 19, acceptance) · quote found
- **A · 99 (high)** The step works under plan step 16's rules (its own kiln worktree from origin/main under ~/Projects/kiln/.claude/worktrees/, builds only on Linux runners or a Mac Studio runner, commits with gits) and the person's rule of 2026-11-12 (run the relevant unit and light integration tests, merge to main, end), landing on kiln's main as kiln's own rules allow: the person's word for every merge to main and every push, each push after the exit codes of the tests relevant to its change, rebases only onto a fixed SHA. [2][7][8]
  _3 independent sources (logs, a document); 5 of 5 quotes found where cited_
  - [2] "Work in your own kiln worktree from origin/main under ~/Projects/kiln/.claude/worktrees/. Build only on Linux runners or the Mac Studio runner, never on this Mac. Commit with gits; no push." (goal 6d9264239fcb8c48b20c5da1685cd36e (step 16) as printed in its tool output) · quote found
  - [7] "Once complete, they should merge their work to main and end." (the person's words) · quote found
  - [7] "kiln-repo agents land on kiln's main as kiln's own rules allow." (How to apply) · quote found
  - [8] "AGENTS.md:62: - **The person's word** is needed for every merge to main, every push and every deploy, and to" (AGENTS.md:62) · quote found
  - [8] "AGENTS.md:64: - **Git:** never a bare `git stash`; rebase and squash only onto a fixed SHA; every push waits on" (AGENTS.md:64-65) · quote found
- **A · 93 (high)** Steps 19 to 28 share one layout (step 19's plan v3, merged with step 21's planner's proposal and sent to steps 20 to 28): each harness's directory .kiln/images/harness-<id>/ (Agentfile, OSI full-form harness.json and any variants, pin.json, pin.sh; nothing fetched committed), W13-R11's image home and build-image@v1's default context; one table, step 30's builtin.rs ALL, whose Builtin gains pin and files; pin.sh verifies as the publisher documents and writes per build its files with url, sha256, size, path, mode and unpack for an archive; `kiln agentfiles context` is the one fetcher for CI and kiln's local fallback, step 19 writing it and leaving archives to the steps that need them; the Agentfile's base ARG, TARGETARCH, HARNESS from ./<id>-${TARGETARCH}[-<variant>], the bring-up's grants, the project and per-user VOLUMEs, no credential and no self-update; and CI as one multi-platform build per harness in .kiln/harness-images.yaml with smoke legs per arch. [4][8]
  _2 independent sources (kiln's records, logs); 7 of 7 quotes found where cited_
  - [4] "1. `.kiln/images/harness-<id>/`: Agentfile, harness.json (full OSI form) and any variants, pin.json, pin.sh; nothing fetched is committed." · quote found
  - [4] "each adds its own `.kiln/images/harness-<id>/`, its table entry and its pin script, and code only where its publisher's fetch differs (an archive to unpack)." · quote found
  - [4] "a step whose files are archives adds `unpack` to it if step 19's hasn't." · quote found
  - [4] "6. CI: one multi-platform build per harness in `.kiln/harness-images.yaml`; smoke legs per arch (amd64 on Linux with KVM, arm64 on a Mac Studio)." · quote found
  - [8] "docs/ci.md:4414:   (`.kiln/images/<name>/Dockerfile` or `.kiln/images/<name>/Agentfile`) or are made in the UI" (W13-R11) · quote found
  - [8] "let context = get("context").unwrap_or_else(|| format!(".kiln/images/{name}"));" (runtime.rs:3946) · quote found
  - [8] "pub const ALL: &[Builtin] = &[];" (step 30 branch, builtin.rs:18) · quote found
- **B · 80 (good)** kiln starts opencode as the program opencode with the single argument acp (protocol Acp, not yet verified against a real install), in the session's folder, with no other fixed argument. [8]
  _1 independent source (logs); 3 of 3 quotes found where cited_
  - [8] "Kind { id: "opencode", name: "opencode", program: "opencode", verified: false, protocol: Protocol::Acp(&["acp"]), screen: &["opencode"], screen_takes_command: false }," (kinds.rs:55) · quote found
  - [8] "Protocol::Acp(args) => {" (kinds.rs:168) · quote found
  - [8] "p.args(args);" (kinds.rs:169) · quote found
- **A · 97 (high)** kiln's ACP adapter asks opencode for its subagents in initialize, through clientCapabilities._meta["opencode/child-session-updates"], ACP's place for extensions. [8][49]
  _2 independent sources (logs, official docs); 3 of 3 quotes found where cited_
  - [8] "const OPENCODE_CHILDREN: &str = "opencode/child-session-updates";" (acp.rs:251) · quote found
  - [8] ""_meta": {OPENCODE_CHILDREN: true}}" (acp.rs:472) · quote found
  - [49] "can use it to attach custom information" (The _meta field) · quote found
- **A · 100 (high)** Only OpenCode 2 implements opencode/child-session-updates: v2.2.7's ACP code reads it and its answer to kiln's initialize echoes it, while OpenCode 1.19.5's source never names it. Both lines install the opencode command and cannot sit side by side, and OpenCode 1's installer installs nothing else, so the pin is OpenCode 2. [22][12][52][44][47]
  _4 independent sources (code, logs, official docs, a vendor); 6 of 6 quotes found where cited_
  - [22] "export const ChildSessionUpdates = "opencode/child-session-updates"" · quote found
  - [12] ""_meta": {"opencode/child-session-updates": true}}" (INIT, loopback only) · quote found
  - [52] "OPENCODE1 v1.19.5 810c0eba1a7348036f8bdf61e6632d0c36c6876c files naming child-session-updates: 0" · quote found
  - [52] "files naming child-session-updates under packages/cli/src: packages/cli/src/acp/capabilities.ts" · quote found
  - [44] "OpenCode 1 and OpenCode 2 both use the `opencode` command and cannot be installed side by side under their default command." · quote found
  - [47] "# This installer only installs OpenCode 1." · quote found
- **A · 97 (high)** OpenCode 2's current release is 2.2.7 (2026-11-10): the publisher's update endpoint, which OpenCode 2's installer reads, names it with its commit 5f76acb1 and release run 42778325289, and npm's @opencode/cli latest is 2.2.7; GitHub's latest-release marker still points at OpenCode 1 (v1.19.5), so a pin script reads the version from the update endpoint, never from GitHub's latest. [14][46][16][47]
  _3 independent sources (logs, a vendor); 5 of 5 quotes found where cited_
  - [14] "UPDATE_API {"channel":"latest","name":"cli","distribution":"npm","version":"2.2.7"" · quote found
  - [46] "metadata=$(curl -fsSL https://opencode.ai/update/api/latest/cli/npm || true)" · quote found
  - [16] "GITHUB_LATEST v1.19.5 published 2026-10-28T06:17:49Z" · quote found
  - [16] "NPM_CLI_LATEST 2.2.7 2026-11-10T10:33:01.652Z" · quote found
  - [47] "GitHub's Latest release may be OpenCode 2, so it's never used." · quote found
- **A · 99 (high)** OpenCode 2 publishes official Linux builds for glibc and musl on arm64 and x64 (and x64 baseline) as .tar.gz archives, each holding the one file opencode, and its docs list them for standalone download. [40][10][11]
  _3 independent sources (official docs, logs); 4 of 4 quotes found where cited_
  - [40] "Download a standalone CLI binary for your platform." · quote found
  - [10] "- **Linux (musl):** [ARM64](https://opencode.ai/update/api/latest/cli/opencode/opencode-linux-arm64-musl.tar.gz)" (services/site/src/docs/content/index.mdx:38) · quote found
  - [11] "== opencode-linux-arm64.tar.gz members: opencode" · quote found
  - [11] "== opencode-linux-x64-musl.tar.gz members: opencode" · quote found
- **A · 100 (high)** v2.2.7's Linux archives verify three ways: each matches SHA256SUMS and GitHub's own asset digest, and each carries a Sigstore-signed SLSA v1 build provenance from anomalyco/opencode's release.yml (refs/heads/dev, workflow_dispatch, commit 5f76acb1, run 42778325289) that gh attestation verify accepts for that signer workflow; the release workflow attests the npm tarballs and Linux archives, and GitHub documents gh attestation verify for binaries. Two runs (06:52Z and 07:14Z) agreed. [11][37][48]
  _3 independent sources (logs, code, official docs); 6 of 6 quotes found where cited_
  - [11] "ARCHIVE opencode-linux-x64.tar.gz size=121783361 sha256=89a6cee227b54bc011179c528a5e63176032462b1a5e21723b616801cddc2ca7 sums=match api=match" · quote found
  - [11] "VERIFY opencode-linux-arm64-musl.tar.gz exit=0" · quote found
  - [11] "cert.subjectAlternativeName https://github.com/anomalyco/opencode/.github/workflows/release.yml@refs/heads/dev" · quote found
  - [11] "cert.sourceRepositoryDigest 5f76acb12572932297cbce81bd0026e59b16e7ac" · quote found
  - [37] "# Build provenance for the npm tarballs and Linux archives. No secrets; id-token only for Sigstore." · quote found
  - [48] "gh attestation verify PATH/TO/YOUR/BUILD/ARTIFACT-BINARY -R ORGANIZATION_NAME/REPOSITORY_NAME" · quote found
- **A · 96 (high)** The same bytes come from every official channel: GitHub's release archive, the publisher's own host that the docs' download link redirects to, and npm's @opencode/cli platform package hold identical binaries, so the channel chosen changes nothing that runs. [14][13]
  _2 independent sources (logs); 4 of 4 quotes found where cited_
  - [14] "SITE opencode.ai/files/bin/2.2.7/opencode-linux-x64.tar.gz sha256=89a6cee227b54bc011179c528a5e63176032462b1a5e21723b616801cddc2ca7" · quote found
  - [14] "GITHUB opencode-linux-x64.tar.gz sha256=89a6cee227b54bc011179c528a5e63176032462b1a5e21723b616801cddc2ca7" · quote found
  - [13] "NPM_BINARY cli-linux-x64 npmx-cli-linux-x64/package/bin/opencode sha256=589253d36810f21217edd51545cd435b328cdc3cf4f451fbe0cce6f88afcf5e1" · quote found
  - [13] "GITHUB_BINARY x64 sha256=589253d36810f21217edd51545cd435b328cdc3cf4f451fbe0cce6f88afcf5e1" · quote found
- **A · 99 (high)** In 2.2.7 the x64 and x64-baseline archives are byte-identical for glibc and for musl; OpenCode's build defines its baseline targets as those without AVX2, and OpenCode 2's installer picks baseline on a CPU without avx2. [11][36][46]
  _3 independent sources (logs, code, a vendor); 4 of 4 quotes found where cited_
  - [11] "ARCHIVE opencode-linux-x64-baseline.tar.gz size=121783361 sha256=89a6cee227b54bc011179c528a5e63176032462b1a5e21723b616801cddc2ca7 sums=match api=match" · quote found
  - [11] "ARCHIVE opencode-linux-x64-baseline-musl.tar.gz size=121562329 sha256=c1801a36bf61e03eafac8d7f4521a200e4d3cd76cf43b36621cb78b460b851ee sums=match api=match" · quote found
  - [36] "{ os: "linux", arch: "x64", avx2: false }," · quote found
  - [46] "if ! grep -qwi avx2 /proc/cpuinfo 2>/dev/null; then" · quote found
- **A · 99 (high)** The glibc builds link only glibc's own libraries (libc, the loader, libpthread, libdl, libm) at symbol versions no newer than GLIBC_2.17, on x86_64 and aarch64, and the x86_64 build runs as is on a glibc host, so a glibc base with glibc 2.17 or later, the shared default included (buildpack-deps:trixie-scm by digest, no RUN), needs nothing added for them. [11][12][4]
  _3 independent sources (logs, kiln's records); 6 of 6 quotes found where cited_
  - [11] "NEEDED x64 libc.so.6 ld-linux-x86-64.so.2 libpthread.so.0 libdl.so.2 libm.so.6" · quote found
  - [11] "MAX_GLIBC x64 GLIBC_2.17" · quote found
  - [11] "NEEDED arm64 libc.so.6 ld-linux-aarch64.so.1 libpthread.so.0 libdl.so.2 libm.so.6" · quote found
  - [11] "MAX_GLIBC arm64 GLIBC_2.17" · quote found
  - [12] "opencode v2.2.7" · quote found
  - [4] "global `ARG BASE` defaulting to buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10 (Docker Official Image, both arches, git, ssh client, curl, CA certificates, no RUN; f29)" · quote found
- **A · 99 (high)** The musl builds need musl's loader and libstdc++.so.6: on Alpine without libstdc++ the x86_64 build fails to load (exit 127); apk add libstdc++ installs libstdc++ and libgcc (libstdc++ depends on libgcc_s.so.1), after which the binary loads and its runtime starts; each build fails on the other libc for want of its loader (exit 127). [11][15][50]
  _3 independent sources (logs, official docs); 7 of 7 quotes found where cited_
  - [11] "NEEDED x64-musl libstdc++.so.6 libc.musl-x86_64.so.1" · quote found
  - [11] "NEEDED arm64-musl libstdc++.so.6 libc.musl-aarch64.so.1" · quote found
  - [15] "Error loading shared library libstdc++.so.6: No such file or directory (needed by /oc/opencode-musl)" · quote found
  - [15] "(1/2) Installing libgcc (14.2.0-r6)" · quote found
  - [15] "so:libgcc_s.so.1" · quote found
  - [15] "env: can't execute '/oc/opencode-gnu': No such file or directory" · quote found
  - [50] "so:libgcc_s.so.1" · quote found
- **B · 80 (good)** Each Linux archive is about 120 MB and its binary 254 to 261 MB unpacked (single observations of 2.2.7), so a harness directory adds about a quarter of a gigabyte to an image. [11]
  _1 independent source (logs); 3 of 3 quotes found where cited_
  - [11] "BINARY x64 sha256=589253d36810f21217edd51545cd435b328cdc3cf4f451fbe0cce6f88afcf5e1 size=258848944 mode=755" · quote found
  - [11] "BINARY arm64-musl sha256=b4203e9861d0472e711493a34e8d843fb249cf9bf499b69cfdc64416085d87c6 size=253697128 mode=755" · quote found
  - [11] "ARCHIVE opencode-linux-arm64-musl.tar.gz size=119600531" · quote found
- **A · 98 (high)** opencode acp needs a working loopback: it starts its own binary again as serve --stdio --port 0, which listens on 127.0.0.1 by default, and bridges ACP on stdio to it over HTTP. Measured on Linux x86_64 with kiln's own initialize and session/new: with only loopback up, initialize was answered in 188 ms and session/new in 452 ms over loopback connections alone; with loopback down, initialize was answered but session/new failed with ServerUnavailable. [24][25][23][12]
  _2 independent sources (code, logs); 7 of 7 quotes found where cited_
  - [24] "return ChildProcess.make(executable, [...args, "--stdio", "--port", "0"], {" · quote found
  - [25] "const hostname = options.hostname ?? config.hostname ?? "127.0.0.1"" · quote found
  - [23] "const endpoint = yield* Standalone.start()" · quote found
  - [12] "CHILDREN 1573855 /home/adalundhe/.cache/s22-opencode-research/dl/gnu/opencode serve --stdio --port 0" · quote found
  - [12] "INIT_MS 188" · quote found
  - [12] "SESSION_MS 452" · quote found
  - [12] ""message": "Internal error: OpenCode server is unavailable", "data": {"errorName": "ServerUnavailable"}" · quote found
- **B · 80 (good)** A Shards harness domain gets what opencode needs to start: its own loopback, brought up; a scratch tmpfs of its own at /tmp; HOME=/tmp; PATH of its own directory, its bin and Docker's default, so a bare command is found there; harness.json's run.env appended to that; and the image's system read-only. [9]
  _1 independent source (logs); 6 of 6 quotes found where cited_
  - [9] "//! - has only its own loopback, brought up;" (crates/init/src/domains.rs:19) · quote found
  - [9] "//!   own and an `mqueue` of its IPC namespace's; and a scratch tmpfs of its own at" (domains.rs:17) · quote found
  - [9] "//! - sees the image's system as built, read-only, `nosuid` and `nodev` (workloads inherit" (domains.rs:11) · quote found
  - [9] "let path = format!("{dir}:{dir}/bin:{PATH}");" (domains.rs:490) · quote found
  - [9] "cstr("HOME=/tmp")?," (domains.rs:493) · quote found
  - [9] "for e in run.get("env").and_then(Value::strings).unwrap_or_default() {" (domains.rs:496) · quote found
- **A · 100 (high)** opencode keeps everything a user owns under its four XDG roots: config, data, state and cache (each else under HOME), its database of sessions and saved credentials at data/opencode.db, and only its tmp under the system temporary directory. Measured with the four variables set on a scratch HOME, every path but home and tmp, the database included, landed under them and nothing was written to HOME. [28][30][10][41][12]
  _4 independent sources (code, logs, official docs); 7 of 7 quotes found where cited_
  - [28] "const data = process.env.XDG_DATA_HOME || (home ? path.join(home, ".local", "share") : undefined)" · quote found
  - [28] "const config = process.env.XDG_CONFIG_HOME || (home ? path.join(home, ".config") : undefined)" · quote found
  - [30] "return filename === ":memory:" ? filename : path.resolve(data, filename)" · quote found
  - [10] "services/site/src/docs/content/cli/providers.mdx:136: The usual release path is `~/.local/share/opencode/opencode.db`; `XDG_DATA_HOME`, the release channel, and `OPENCODE_DB`" · quote found
  - [41] "Do not edit the database to manage credentials" · quote found
  - [12] "/home/adalundhe/.cache/s22-opencode-research/vol/.local/share/opencode/opencode.db" · quote found
  - [12] "0	/home/adalundhe/.cache/s22-opencode-research/scratch" (sizes) · quote found
- **A · 98 (high)** Every opencode command, --version included, first creates its data, config, state, log, bin, repos and tmp directories (and a log file), so it needs writable XDG roots and a writable temporary directory wherever it runs. [29][12][28]
  _2 independent sources (code, logs); 3 of 3 quotes found where cited_
  - [29] "(directory) => fs.promises.mkdir(directory, { recursive: true })," · quote found
  - [12] "/home/adalundhe/.cache/s22-opencode-research/vol/.local/share/opencode/log/opencode.log" (files after --version) · quote found
  - [28] "tmp: path.join(os.tmpdir(), app)," · quote found
- **A · 97 (high)** opencode's saved API keys and OAuth tokens live in its server's SQLite database under the data root (V2 imports the legacy auth.json there); over ACP it uses the credentials it already has, offering only its terminal login as an auth method, so an image needs none and a user's sign-in belongs on their per-user volume. [41][42][12]
  _2 independent sources (official docs, logs); 3 of 3 quotes found where cited_
  - [41] "V2 imports supported credentials from the legacy auth.json in the OpenCode data directory during its database" · quote found
  - [42] "OpenCode uses the provider credentials it already has." · quote found
  - [12] ""authMethods": [{"description": "Run `opencode auth login` in the terminal", "name": "Login with opencode", "id": "opencode-login"}]" · quote found
- **A · 98 (high)** OpenCode 2's update check is skipped when OPENCODE_DISABLE_AUTOUPDATE is 1 or true, and only its TUI's default command runs the updater (opencode acp does not); OpenCode 1's docs describe the variable as disabling automatic update checks. A pinned image sets it, for the harness and any TUI started in the microVM. [26][27][10][45]
  _2 independent sources (code, logs, official docs); 4 of 4 quotes found where cited_
  - [26] "if (OPENCODE_LOCAL || ["1", "true"].includes(process.env.OPENCODE_DISABLE_AUTOUPDATE?.toLowerCase() ?? "")) {" · quote found
  - [27] ".run((version) => {" · quote found
  - [10] "packages/cli/src/commands/handlers/default.ts:62:     const updater = yield* Updater.Service" · quote found
  - [45] "Disable automatic update checks" · quote found
- **A · 98 (high)** At run time opencode may fetch, all over HTTPS: ripgrep 15.1.0 from GitHub when no rg is on PATH or in its cache's bin (checked against SHA-256s pinned in its source from ripgrep's published .sha256 files, and unpacked with the system's tar); provider SDK packages from npm for a configured provider not built in; and its model catalog from models.dev unless OPENCODE_DISABLE_MODELS_FETCH is set. With only loopback, session/new still listed models from its bundled catalog; with the host's network its server held one HTTPS connection on port 443. DNS and TCP 443 out cover all of it. [31][33][25][12]
  _2 independent sources (code, logs); 7 of 7 quotes found where cited_
  - [31] "const url = `https://github.com/BurntSushi/ripgrep/releases/download/${VERSION}/${filename}`" · quote found
  - [31] "// SHA-256 of each release asset, from the `<asset>.sha256` files published with ripgrep 15.1.0. Release assets" · quote found
  - [31] ": run("tar", ["-xzf", archive, "-C", dir], dir)" · quote found
  - [33] ": yield* npm.add(installSpec(packageName)).pipe(" · quote found
  - [25] "fetch: !truthy(process.env.OPENCODE_DISABLE_MODELS_FETCH)," · quote found
  - [12] ""currentValue": "opencode/big-pickle"" (SESSION, loopback only) · quote found
  - [12] "104.18.10.66:443" (ESTAB, host network) · quote found
- **A · 98 (high)** opencode pins ripgrep's aarch64-unknown-linux-gnu build (glibc) for arm64 Linux and its static x86_64-unknown-linux-musl build for x64, and ripgrep 15.1.0 publishes aarch64 Linux builds for gnu only; on Linux opencode searches a git repository with its native finder and anything else with ripgrep. So on a musl base an arm64 opencode has a working ripgrep only from the distribution. [31][53][32]
  _2 independent sources (code, logs); 5 of 5 quotes found where cited_
  - [31] "platform: "aarch64-unknown-linux-gnu"," · quote found
  - [31] "platform: "x86_64-unknown-linux-musl"," · quote found
  - [53] "ASSET ripgrep-15.1.0-aarch64-unknown-linux-gnu.tar.gz sha256:2b661c6ef508e902f388e9098d9c4c5aca72c87b55922d94abdba830b4dc885e" · quote found
  - [53] "ASSET ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz sha256:1c9297be4a084eea7ecaedf93eb03d058d6faae29bbc57ecdaf5063921491599" · quote found
  - [32] "return location.vcs && !Protected.isHome(location.directory) ? fffLayer : ripgrepLayer" · quote found
- **A · 90 (high)** opencode's PTY helper ships inside its binary and is written, after a SHA-256 check, to its cache's bin on first use, so the cache root too must be writable. [35][29]
  _1 independent source (code); 2 of 2 quotes found where cited_
  - [35] "if (sha256(bytes) !== input.sha256) throw new Error("Embedded opencode-pty checksum mismatch")" · quote found
  - [29] "bin: path.join(cache, "bin")," · quote found
- **B · 78 (good)** The only telemetry hook in OpenCode 2's CLI entry is OpenTelemetry export to an endpoint and headers the user sets; a search of its CLI, core and server sources found no other, so the Agentfile has nothing to turn off. [38][10]
  _2 independent sources (code, logs); 2 of 2 quotes found where cited; inferred from them_
  - [38] "endpoint: process.env.OTEL_EXPORTER_OTLP_ENDPOINT," · quote found
  - [10] "packages/cli/src/index.ts:153:       headers: process.env.OTEL_EXPORTER_OTLP_HEADERS," · quote found
- **B · 80 (good)** kiln can unpack opencode's .tar.gz archives with what it already has: tar 0.4.46 and flate2 1.1.10 are dependencies of its crate, and its runner and deployer code already read gzip tarballs with them. [8]
  _1 independent source (logs); 4 of 4 quotes found where cited_
  - [8] "crates/kiln/Cargo.toml:113: tar = "0.4.46"" · quote found
  - [8] "crates/kiln/Cargo.toml:133: flate2 = "1.1.10"" · quote found
  - [8] "tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice())).unpack(&d)?;" (runtime.rs:3767) · quote found
  - [8] "let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));" (aws_static.rs:135) · quote found
- **B · 80 (good)** Shards' Agentfile gives what the layout uses: HARNESS from a build-host path copies it as COPY does (an http(s) source is downloaded, with no checksum); VOLUME --target-kind=harness scopes a volume to harnesses; the bring-up's provider reach is three lines (NETWORK --dns --egress=443/tcp, EXPOSE ... 443/tcp AS egress, CONNECT); and a full OSI harness.json sets the harness's environment through run.env, as the bring-up's Codex did. [9]
  _1 independent source (logs); 7 of 7 quotes found where cited_
  - [9] "| path on the build host | copies it, as `COPY` copies |" · quote found
  - [9] "| http(s) URL | downloads it, respecting HTTPS |" · quote found
  - [9] "`--target-kind=<agent|harness>` (added 2026-10-01): whether the names after `FOR`" · quote found
  - [9] "NETWORK --dns --egress=443/tcp provider FOR claude" · quote found
  - [9] "EXPOSE --harnesses=claude 443/tcp AS egress FOR provider" · quote found
  - [9] "CONNECT claude WITH claude ON provider" · quote found
  - [9] ""env":["CODEX_HOME=/run/codex-home"]" · quote found
- **B · 80 (good)** Building the image needs a shards with harness.json startup (c3b7bde8b, 2026-11-09), which release v0.7.0 lacks and Shards' main has had since the bring-up's merge 1da9132b7. [9]
  _1 independent source (logs); 2 of 2 quotes found where cited_
  - [9] "startup.rs (c3b7bde8b) NOT in v0.7.0" · quote found
  - [9] "1da9132b7 IN origin/main 08cf6ec5d72a6d48a9c36232f27f801932907140" · quote found
- **A · 93 (high)** This kiln has no Linux arm64 runner (its arm64 machines are Macs), so the arm64 smoke runs in a Linux arm64 microVM on a Mac Studio runner and the amd64 smoke on a Linux runner with KVM, as step 19 plans. [54][4]
  _2 independent sources (logs, kiln's records); 2 of 2 quotes found where cited_
  - [54] "No runner suits os == linux, arch == aarch64." · quote found
  - [4] "Linux arm64 with KVM (none here: arm64 runs on a Mac Studio)" · quote found
- **A · 98 (high)** opencode is MIT-licensed, and its Linux archives hold the binary alone, without the license text whose notice MIT asks copies to carry. [39][11]
  _2 independent sources (code, logs); 3 of 3 quotes found where cited_
  - [39] "MIT License" · quote found
  - [39] "The above copyright notice and this permission notice shall be included in all" · quote found
  - [11] "== opencode-linux-x64.tar.gz members: opencode" · quote found
- **A · 100 (high)** On 2026-11-12 a disk-full sweep of ~/.cache on this Mac removed research directories, step 19's evidence and this research's first downloads and logs among them, though R16 keeps evidence even under a cache directory; this research's evidence now lives in ~/.local/share/s22-opencode-research, and the implementation commits its evidence to the kiln repository. [6][16][20]
  _3 independent sources (a measurement, logs); 4 of 4 quotes found where cited_
  - [6] "Preserve user data, attachments, experiments, unmerged source/worktrees, credentials and evidence, including when someone placed them in a directory named cache/tmp." · quote found
  - [16] "S19_EVIDENCE_DIR ls: /Users/adalundhe/.cache/s19-claude-research: No such file or directory" · quote found
  - [16] "S22_FIRST_RUN_EVIDENCE ls: /Users/adalundhe/.cache/s22-opencode-research/evidence: No such file or directory" · quote found
  - [20] "rm /Users/adalundhe/.cache/kiln-research-work" · quote found
- **A · 93 (high)** This step's first agent was stopped only to cut the running agents from 19 to 10 (ROOT folded the ten Agentfile goals into step 19's agent), not for a fault in its work; its findings (OpenCode 1 and 2, v2.2.7's digests and attestations, the musl builds' needs) were re-verified here after the sweep removed its downloads. [3][2]
  _2 independent sources (kiln's records, logs); 2 of 2 quotes found where cited_
  - [3] "I'm folding the ten Agentfile agents into one and stopping the other nine." · quote found
  - [2] "x64-musl" · quote found
- **B · 80 (good)** opencode --version prints opencode v<version> and needs no network at all: it ran in a network namespace whose loopback was down. [12]
  _1 independent source (logs); 3 of 3 quotes found where cited_
  - [12] "--- version, no network (new netns, lo down)" · quote found
  - [12] "opencode v2.2.7" · quote found
  - [12] "VERSION_EXIT 0" · quote found
- **A · 99 (high)** opencode debug paths prints each of its paths (home, data, cache, config, state, tmp, bin, log, repos, db) without starting a server or opening the database, so a probe can show the XDG variables took effect inside the microVM. [43][12][10]
  _3 independent sources (official docs, logs); 3 of 3 quotes found where cited_
  - [43] "This command does not start a server or open the database." · quote found
  - [12] "PATHS_EXIT 0" · quote found
  - [10] "services/site/src/docs/content/cli/index.mdx:80: The optional selector accepts `db`, `home`, `data`, `config`, `cache`, `state`, `tmp`, `bin`, `log`, or `repos` and prints" · quote found
- **A · 93 (high)** A Shards harness domain's stdin is /dev/null and its output goes to init's relay, so a full ACP session through a harness member waits on parent step 5 (a member's stdio as streams its owner holds); this step's smoke is the version, the started process and a probe, not an ACP turn. [9][1]
  _2 independent sources (logs, kiln's records); 2 of 2 quotes found where cited_
  - [9] "//! - holds no descriptor of init's but its stdio: stdin `/dev/null`, stdout and stderr one" · quote found
  - [1] "[Shards] A member's stdin, stdout and stderr as streams its owner holds" (step 5, title) · quote found
- **A · 99 (high)** On Alpine 3.22 the x86_64 musl build fails to load without libstdc++ (exit 127) and prints opencode v2.2.7 once apk's libstdc++ (with libgcc) is in and the root has a /proc and a /tmp of its own, as a Shards domain has; given HOME=/tmp and the four XDG roots on /home/opencode, as this plan's harness.json sets them, opencode puts data, config, state, cache, bin, log, repos and its database under /home/opencode and only home and tmp under /tmp. Without /proc mounted the same root failed at realpath of /tmp/opencode. [55][56][9]
  _3 independent sources (logs); 7 of 7 quotes found where cited_
  - [55] "Error loading shared library libstdc++.so.6: No such file or directory (needed by /oc/opencode)" · quote found
  - [55] "(2/2) Installing libstdc++ (14.2.0-r6)" · quote found
  - [55] "--- musl build on Alpine with libstdc++, /proc and /tmp
opencode v2.2.7
EXIT 0" · quote found
  - [55] "db         /home/opencode/.local/share/opencode/opencode.db" · quote found
  - [55] "home       /tmp" · quote found
  - [56] "Error: ENOENT: no such file or directory, realpath '/tmp/opencode'" · quote found
  - [9] "//!   and grants hidden under an empty tmpfs no one may read; `/sys` hidden; a `/proc` of" (crates/init/src/domains.rs:15) · quote found

**Sources**

1. Parent plan 01a1cd8e: steps 16, 19 and 22 (kiln's records, internal) — plan:01a1cd8e862e7169a074db3d5fa33f3d
2. Step 22's first agent's transcript (stopped by ROOT; restarted at the person's word) (logs, internal) — /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards--claude-worktrees-g121/01a1cdf2-73fe-7771-8d4a-693f546b9527.jsonl
3. Shards ROOT's conversation: why it stopped steps 20-28's agents (kiln's records, internal) — agent:01a148884a4770fdaf0111f3f792a248
4. Step 19's plan v2 (Claude Code): the shared layout (kiln's records, internal) — plan:01a1cdf3cc7a70e4b3ca2ec610352383
5. Step 19's message to steps 20-28: the merged layout (agreed with step 21) (kiln's records, internal) — agent:01a1cdda1d3f759c8aa4ea4ff46f09da
6. Shards binding rules R01-R16 (origin/main, sha256 df2acf47..., unchanged at 08cf6ec5) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/development-rules.md
7. The person's rule of 2026-11-12: subagents test, merge their own work, end (a document, internal) — /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards/memory/feedback-subagents-merge-and-end.md
8. Evidence: kiln origin/main efb40ea0f excerpts (kinds.rs, acp.rs, Cargo.toml, aws_static.rs, runtime.rs, docs/ci.md W13-R11, AGENTS.md hazards) and step 30's builtin.rs at d8b829869 (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/e_src_kiln.txt
9. Evidence: Shards origin/main 08cf6ec5 excerpts (init domains.rs, AGENTFILE_ARCH.md, the bring-up's Agentfile and harness.json, startup vs v0.7.0) (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/e_src_shards.txt
10. Evidence: opencode v2.2.7 (fedc6f20) source and docs excerpts with file:line (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/e_src_opencode.txt
11. M1-M3 (this Mac, 2026-11-12T07:11Z): v2.2.7 Linux archives vs SHA256SUMS and GitHub digests; gh attestation verify; ELF needs (llvm-readelf) (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m1-m3.txt
12. M4 (adacachy2, Linux 7.3.3, glibc 2.44): --version and debug paths without network; opencode acp with kiln's initialize and session/new, loopback only, loopback down, host network (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m4-linux-x64.txt
13. M5: npm's @opencode/cli-linux-* 2.2.7 binaries vs GitHub's archives (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m5-npm-vs-github.txt
14. M6: the docs' download host (opencode.ai/files) vs GitHub; the publisher's update API (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m6-site-vs-github.txt
15. M8 (adacachy2): Alpine 3.22 minirootfs, the musl build without and with libstdc++; each build on the other libc (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m8-linux-musl.txt
16. M9: GitHub's latest-release marker, npm's latest, the swept evidence directories (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m9-latest-and-sweep.txt
17. M7: child-session-updates in OpenCode 1.19.5 vs 2.2.7; the installers' baseline and version lines; shards v0.7.0 (a measurement, internal) — git -C ~/.cache/g121-s22/oc1 grep -c child-session-updates; git -C ~/.cache/g121-s22/oc2 grep -c child-session-updates -- packages/cli/src; grep -n avx2 ~/.cache/g121-s22/dl/install-v2.sh
18. ripgrep 15.1.0's Linux release assets (a measurement, internal) — gh api repos/BurntSushi/ripgrep/releases/tags/15.1.0 --jq '.assets[] | select(.name|test("linux"))'
19. kiln runners able to run linux/amd64 (every other runner is a Mac or Windows) (a measurement, internal) — kiln MCP runners want=linux/amd64
20. The ~/.cache sweep's log on this Mac (2026-11-12T05:29Z run) (logs, internal) — /Users/adalundhe/.cache-clear-1794463763.log
21. The agent whose run r18 cleared ~/.cache on this Mac (ended 2026-11-12T07:10Z) (kiln's records, internal) — agent:01a1ce2f26f376b49a27735c42b27faf
22. opencode v2.2.7: packages/cli/src/acp/capabilities.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/acp/capabilities.ts · 2026-11-10
23. opencode v2.2.7: packages/cli/src/commands/handlers/acp.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/commands/handlers/acp.ts
24. opencode v2.2.7: packages/cli/src/services/standalone.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/services/standalone.ts
25. opencode v2.2.7: packages/cli/src/server-process.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/server-process.ts
26. opencode v2.2.7: packages/cli/src/services/updater.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/services/updater.ts
27. opencode v2.2.7: packages/cli/src/commands/handlers/default.ts (the TUI's command) (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/commands/handlers/default.ts
28. opencode v2.2.7: packages/util/src/global-roots.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/util/src/global-roots.ts
29. opencode v2.2.7: packages/util/src/global.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/util/src/global.ts
30. opencode v2.2.7: packages/cli/src/database-path.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/database-path.ts
31. opencode v2.2.7: packages/core/src/ripgrep/binary.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/core/src/ripgrep/binary.ts
32. opencode v2.2.7: packages/core/src/filesystem/search.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/core/src/filesystem/search.ts
33. opencode v2.2.7: packages/core/src/plugin/provider/sdk-factory.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/core/src/plugin/provider/sdk-factory.ts
34. opencode v2.2.7: packages/core/src/provider.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/core/src/provider.ts
35. opencode v2.2.7: packages/core/src/persistent-pty/binary.bun.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/core/src/persistent-pty/binary.bun.ts
36. opencode v2.2.7: packages/cli/script/build.ts (its release targets) (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/script/build.ts
37. opencode v2.2.7: .github/workflows/release.yml (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/.github/workflows/release.yml
38. opencode v2.2.7: packages/cli/src/index.ts (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/cli/src/index.ts
39. opencode v2.2.7: LICENSE (code, internal) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/LICENSE
40. OpenCode 2 docs: Intro (install, standalone CLI downloads) (official docs, external) — https://opencode.ai/docs/
41. OpenCode 2 docs: CLI providers, Storage (official docs, external) — https://opencode.ai/docs/cli/providers/#storage
42. OpenCode 2 docs: CLI, ACP (official docs, external) — https://opencode.ai/docs/cli/acp/
43. OpenCode 2 docs: CLI, Paths (official docs, external) — https://opencode.ai/docs/cli/#paths
44. OpenCode 1 docs source at v2.2.7: Intro (OpenCode 1 and 2) (official docs, external) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/web/src/content/docs/index.mdx
45. OpenCode 1 docs source at v2.2.7: CLI environment variables (official docs, external) — https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/packages/web/src/content/docs/cli.mdx
46. OpenCode 2's official installer (a vendor, external) — https://opencode.ai/v2/install
47. OpenCode 1's official installer (a vendor, external) — https://opencode.ai/install
48. GitHub Docs: using artifact attestations (verifying with the GitHub CLI) (official docs, external) — https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations
49. Agent Client Protocol: Extensibility (_meta) (official docs, external) — https://agentclientprotocol.com/protocol/extensibility
50. Alpine Linux packages: libstdc++ (v3.22, x86_64) (official docs, external) — https://pkgs.alpinelinux.org/package/v3.22/main/x86_64/libstdc++
51. Step 19's merged layout for steps 19-28, as it reached this agent (agreed with step 21) (kiln's records, internal) — agent:01a1ce6f23217458ac7c98b0e2afa51d
52. M7: child-session-updates in OpenCode 1.19.5 vs 2.2.7 sources; OpenCode 2's installer lines (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m7-opencode1-vs-2.txt
53. M10: ripgrep 15.1.0's Linux release assets and digests (GitHub API) (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m10-ripgrep-assets.txt
54. M11: kiln runners for linux/arm64 (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m11-runners-arm64.txt
55. M8D (adacachy2): Alpine 3.22.2 minirootfs (digest checked) with its own /proc and /tmp: the x86_64 musl build without and with libstdc++; debug paths with HOME=/tmp and the XDG roots on /home/opencode (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m8d-linux-musl-alpine.txt
56. M8B (adacachy2): the same Alpine root with /tmp present but no /proc mounted (logs, internal) — /Users/adalundhe/.local/share/s22-opencode-research/evidence/m8b-linux-musl-tmp.txt
