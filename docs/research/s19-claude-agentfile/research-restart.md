**Research:** Plan step 19 [kiln]: the Claude Code Agentfile is embedded in kiln: pinned official Linux build; HARNESS plus harness.json; DNS and egress grants; the project volume and a per-user volume that is the whole HOME, with HOME set in run.env (needs Shards at or after 997f4e47e) and no volume under /tmp; amd64 and arm64; no secrets; one smoke run prints its version. Full step: kiln plan 01a1cd8e862e7169a074db3d5fa33f3d. RESTARTED by ROOT: the first agent's inbox jammed and its plan run was lost in kiln's restart. Your plan exists: kiln plan 01a1cdf3cc7a70e4b3ca2ec610352383 version 3. Don't re-plan; if kiln asks for a plan, say that one ready (plans_ready). Do its steps 1-7 from step 1. Worktree ~/Projects/kiln/.claude/worktrees/s19-claude-agentfile; evidence in ~/.cache/s19-claude-research; the predecessor's transcript is /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards--claude-worktrees-g121/01a1cdda-1d8f-7318-9039-0bff9a08656d.jsonl (read its end first). Steps 20-28 wait on your fetcher (kiln agentfiles context) and the first built-in row; step 25's unpacker (a04cffd0 on s25-unpack) is for your step 3. The first built-in row adds COPY .kiln/images/ to kiln's Dockerfile, with a test. Rules: the relevant unit and light integration tests, land on kiln's main as kiln's rules allow, report and end; research and unblock yourself before asking the person.

49 findings (A 24, B 23, C 2) from 37 sources (20 internal, 17 external)

- **A · 93 (high)** Step 19 asks for a Claude Code Agentfile embedded in kiln: the official Linux build pinned by version and digest, HARNESS with harness.json, DNS and egress grants, the project volume and a per-user volume, layering over any base image, built for linux/amd64 and linux/arm64, prebuilt by kiln CI and served by digest with a local build as fallback, no credentials, and one smoke run in a microVM that prints the version. ROOT's restart of 2026-11-12 makes the per-user volume the whole HOME with HOME set in run.env, requires Shards at or after 997f4e47e, allows no volume under /tmp, and says steps 20-28 wait on this step's fetcher (kiln agentfiles context) and first built-in row, that step 25's unpacker is for this step's step 3, and that the first built-in row adds COPY .kiln/images/ to kiln's Dockerfile with a test. [1][2]
  _2 independent sources (kiln's records, logs); 7 of 7 quotes found where cited_
  - [1] "the harness's official Linux build, pinned by version and digest (find its source; if there is no Linux build, say so and stop);" (step 19, description) · quote found
  - [1] "It layers over any base image, so a project's own toolchain image works." (step 19, requirements) · quote found
  - [1] "It builds for linux/amd64 and linux/arm64, and kiln CI prebuilds it and serves it by digest, with a local build as fallback." (step 19, requirements) · quote found
  - [1] "One smoke run in a microVM prints the harness's version." (step 19, requirements) · quote found
  - [2] "the project volume and a per-user volume that is the whole HOME, with HOME set in run.env (needs Shards at or after 997f4e47e) and no volume under /tmp" (the goal, line 3) · quote found
  - [2] "Steps 20-28 wait on your fetcher (kiln agentfiles context) and the first built-in row; step 25's unpacker (a04cffd0 on s25-unpack) is for your step 3." (the goal, line 3) · quote found
  - [2] "The first built-in row adds COPY .kiln/images/ to kiln's Dockerfile, with a test." (the goal, line 3) · quote found
- **B · 74 (good)** The restart keeps the first agent's plan: plan 01a1cdf3cc7a70e4b3ca2ec610352383 version 3 (seven steps, research 01a1cdf3cc7b73dd9c618e2a0c72e604, the layout shared with steps 20-28) is to be carried out from its step 1, not planned again; so this plan carries its steps, changed only where the restart's goal or what has changed since (Shards 997f4e47e, step 30's table on kiln main, step 25's unpacker, the siblings' pins, step 28's offer) requires. [2][35]
  _2 independent sources (logs, kiln's records); 2 of 2 quotes found where cited; inferred from them_
  - [2] "Your plan exists: kiln plan 01a1cdf3cc7a70e4b3ca2ec610352383 version 3. Don't re-plan; if kiln asks for a plan, say that one ready (plans_ready). Do its steps 1-7 from step 1." (the goal, line 3) · quote found
  - [35] "Steps 20 to 28 follow this step's goal, so the layout below is the shared one, merged with step 21's planner's proposal on 2026-11-12 and sent to steps 20 to 28's planners" (opening paragraph) · quote found
- **A · 100 (high)** Step 19 works under plan step 16's rules and the person's rule of 2026-11-12: read kiln's AGENTS.md and CLAUDE.md and Shards' policy and record the readback; work in its own kiln worktree from origin/main; build only on Linux runners or the Mac Studio runner, never on this Mac; commit with gits; run the relevant unit and light integration tests, then land on kiln's main as kiln's own rules allow (the person's word for every merge and push), fast-forward only, report and end. [13][17][8][2]
  _4 independent sources (logs, a document); 5 of 5 quotes found where cited_
  - [13] "Rules: read kiln's AGENTS.md and CLAUDE.md (origin/main) plus Shards' policy, and record the readback. Work in your own kiln worktree from origin/main under ~/Projects/kiln/.claude/worktrees/. Build only on Linux runners or the Mac Studio r…" (goal 6d9264239fcb8c48b20c5da1685cd36e) · quote found
  - [17] "kiln-repo agents land on kiln's main as kiln's own rules allow." (How to apply) · quote found
  - [17] "fast-forward only. Retry the merge if main moved. Never force." (How to apply, item 3) · quote found
  - [8] "is needed for every merge to main, every push and every deploy" (s_kiln_agents, AGENTS.md line 62) · quote found
  - [2] "Rules: the relevant unit and light integration tests, land on kiln's main as kiln's rules allow, report and end; research and unblock yourself before asking the person." (the goal, line 3) · quote found
- **B · 80 (good)** Shards' policy fixes the roles: scoped implementers do implementation and targeted unit and light integration tests, the main coordinator owns audits, profiling, benchmarking and cross-lane integration validation, and every production literal needs a documented source or derivation (R03). [8]
  _1 independent source (logs); 3 of 3 quotes found where cited_
  - [8] "Use scoped subagents for parallel implementation and targeted unit/light" (s_policy, development-rules.md line 147) · quote found
  - [8] "The main coordinator owns all audits, profiling, benchmarking, cross-lane" (s_policy, development-rules.md line 156) · quote found
  - [8] "Use online learned/data-derived values, or exceptionally well-researched invariants" (s_policy, development-rules.md line 42) · quote found
- **A · 99 (high)** Steps 20 to 28 are written as step 19's goal says, and their Agentfiles already follow the shared layout: ARG BASE defaulting to buildpack-deps:trixie-scm by digest, ARG TARGETARCH, HARNESS <id> FROM ./<id>-${TARGETARCH}, the three provider grant lines, VOLUME --target-kind=harness /workspace, and a per-user volume, which amp and cursor put at the whole HOME (/home/<id>), cursor setting HOME=/home/cursor in run.env; step 30 lists the built-ins read-only beside a person's own Agentfiles. [13][1][6]
  _3 independent sources (logs, kiln's records); 7 of 7 quotes found where cited_
  - [13] "Plan step 20 [kiln]: the Codex Agentfile is embedded in kiln, as step 19's goal says. Rules as plan step 16's goal." (goal 306e318bf3d68d8ab9526b6946ecafa9) · quote found
  - [1] "Built-in (embedded) Agentfiles are listed read-only beside the user's own and the project's .kiln/images/<name>/Agentfile." (step 30, description) · quote found
  - [6] "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10" (s24-amp-agentfile Agentfile) · quote found
  - [6] "HARNESS amp FROM ./amp-${TARGETARCH}" (s24-amp-agentfile Agentfile) · quote found
  - [6] "VOLUME --target-kind=harness /home/amp FOR amp" (s24-amp-agentfile Agentfile) · quote found
  - [6] "VOLUME --target-kind=harness /home/cursor FOR cursor" (s27-cursor-agentfile Agentfile) · quote found
  - [6] ""HOME=/home/cursor"" (s27-cursor-agentfile harness.json) · quote found
- **B · 80 (good)** Three siblings (pi, devin, antigravity) have, uncommitted in their worktrees, the same two fields added to step 30's Builtin, pin (the text of pin.json) and files (name and text pairs, harness.json among them), and a row each built with include_str! from .kiln/images/harness-<id>/; so the first built-in row adds exactly these two fields, and the fetcher reads a built-in through them. [6]
  _1 independent source (logs); 4 of 4 quotes found where cited_
  - [6] "+    pub pin: &'static str," (Uncommitted Builtin fields, agentfile-pi-v1) · quote found
  - [6] "+    pub files: &'static [(&'static str, &'static str)]," (Uncommitted Builtin fields, agentfile-pi-v1) · quote found
  - [6] "files: &[("harness.json", include_str!("../../../../.kiln/images/harness-devin/harness.json"))]," (Uncommitted Builtin fields, s25-devin-row) · quote found
  - [6] "files: &[("harness.json", include_str!("../../../../.kiln/images/harness-antigravity/harness.json"))]," (Uncommitted Builtin fields, s26-agy-agentfile) · quote found
- **C · 64 (moderate)** The siblings' committed pins agree only on builds[] of {platform, variant (or null), files[] of {url, sha256, size, path, mode, unpack}}; their other fields differ by publisher (base a string in goose's, amp's and opencode's, an object in pi's, devin's and gemini's; verified a string, an object or an array), agy's file entries also carry sha512, and unpack takes several forms: a whole archive (path ".", optionally with strip 1) or one member at a path with a mode, some with the member's own sha256 and size. So the one fetcher reads only the shared build fields, ignores the rest, and hands unpack to step 25's unpacker as it is. [6]
  _1 independent source (logs); 6 of 6 quotes found where cited; inferred from them_
  - [6] "{"keys":["base","builds","channel","read_at","verified","version"],"base_type":"string","verified_type":"object"}" (s23-goose-agentfile pin.json) · quote found
  - [6] "{"keys":["base","builds","harness","manifest","read_at","verified","version"],"base_type":"object","verified_type":"string"}" (s25-devin-row pin.json) · quote found
  - [6] ""path":".","mode":null,"unpack":{"format":"tar.gz","strip":1}" (b89452884 (pi) pin.json) · quote found
  - [6] ""path":"bin/rg","mode":"0755","unpack":{"format":"tar.gz","member":"ripgrep-15.2.0-x86_64-unknown-linux-musl/rg"}" (b89452884 (pi) pin.json) · quote found
  - [6] "{"keys":"mode,path,sha256,sha512,size,unpack,url","path":"bin/agy","mode":"0755","unpack":{"format":"tar.gz","member":"antigravity"" (s26-agy-agentfile pin.json) · quote found
  - [6] "{"platform":"linux/amd64","variant":"musl","files":[{"keys":"mode,path,sha256,size,unpack,url","path":"bin/opencode"" (s22-opencode-agentfile pin.json) · quote found
- **B · 80 (good)** Step 25's unpacker is committed at a04cffd0 (branch s25-unpack, not on kiln main): agentfiles::unpack::tar_gz unpacks a tar.gz whose size and SHA-256 matched its pin, taking only directories and regular files at relative paths, refusing links, devices, FIFOs, escapes and set-ID bits with a typed Unpack error naming the URL and entry, leaving nothing behind on failure; its hook in the fetcher, its HTTP light-integration test and its live context wait on step 19's fetcher. Its form taking a spec, a root, a path and a mode (strip and member) is so far uncommitted in the s25-unpack worktree. [6]
  _1 independent source (logs); 5 of 5 quotes found where cited_
  - [6] "agentfiles::unpack::tar_gz unpacks a gzip tar whose size and SHA-256 have matched its pin into a" (git log -1 a04cffd0) · quote found
  - [6] "Unpack { url, entry, why: Link | Device | Escapes | SetId | Kind | Io }. tar 0.4.46's own unpack_in" (git log -1 a04cffd0) · quote found
  - [6] "Step 19's fetcher isn't committed yet, so its hook, the HTTP light-integration test and the live" (git log -1 a04cffd0) · quote found
  - [6] "pub fn tar_gz(archive: &Path, url: &str, into: &Path) -> Result<(), Unpack> {" (its tar_gz at a04cffd0) · quote found
  - [6] "pub fn tar_gz(archive: &Path, url: &str, spec: &Spec, root: &Path, path: &Path, mode: Option<u32>) -> Result<(), Unpack> {" (the s25-unpack worktree, uncommitted) · quote found
- **B · 80 (good)** Step 28's implementer, waiting on the fetcher, offered on 2026-11-12 to write materialize, recipe and kiln agentfiles context to this plan's step-3 text on a branch from step 19's, tested with kiln's own HTTP server and pi's pin, for step 19 to review and merge, Claude's live check staying step 19's; it asks that a failed file (status, size, digest or unpack) leave no per-arch directory, the directory made under a temporary name and renamed into place once complete, and that the pin's mode go to the unpacker only for a member. [2]
  _1 independent source (logs); 4 of 4 quotes found where cited_
  - [2] "If it helps, I can write materialize, recipe and `kiln agentfiles context` to your plan's step-3 text, on a branch from yours, while you do your steps 1 and 2:" (step 28's message, Offer) · quote found
  - [2] "when any file fails (status, size, digest or unpack), leave no per-arch directory behind. Make it under a temporary name and rename it into place once it's complete, or remove it." (step 28's message, item 3) · quote found
  - [2] "Pass the pin's mode only for a member: a whole tree keeps its archived modes." (step 28's message, item 2) · quote found
  - [2] "Answer yes or no. Until you do, I wait and write nothing of the fetcher." (step 28's message) · quote found
- **A · 97 (high)** Anthropic publishes official Linux builds of Claude Code for x64 and arm64, each for glibc and musl (linux-x64, linux-arm64, linux-x64-musl, linux-arm64-musl), under downloads.claude.ai/claude-code-releases/<version>/<platform>/claude; its npm package installs the same native binary. [20][27][9]
  _2 independent sources (official docs, a vendor, logs); 5 of 5 quotes found where cited_
  - [20] "Supported npm install platforms are `darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`, `linux-x64-musl`, `linux-arm64-musl`, `win32-x64`, and `win32-arm64`." (Install with npm) · quote found
  - [20] "The npm package installs the same native binary as the standalone installer." (Install with npm) · quote found
  - [27] "DOWNLOAD_BASE_URL="https://downloads.claude.ai/claude-code-releases"" (download section) · quote found
  - [9] "PLATFORM 2.1.311 linux-arm64 checksum=2e2496c9a8065df93acb0fca2d341f59a6b8777649b2e025ab8c9bcd5eef61f3 size=293658808" (2.1.311 manifest) · quote found
  - [9] "PLATFORM 2.1.311 linux-x64 checksum=86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada size=293776752" (2.1.311 manifest) · quote found
- **A · 100 (high)** A release is pinned through its signed manifest: manifest.json lists every platform binary's SHA-256 and, from 2.1.89 on, is signed by Anthropic's release key (primary fingerprint 31DD DE24 DDFA B679 F42D 7BD2 BAA9 29FF 1A7E CACE); the manifests of 2.1.311 and 2.1.322 verified with VALIDSIG by that key, and each downloaded binary matched its listed SHA-256. [20][9][12][8]
  _4 independent sources (official docs, logs); 5 of 5 quotes found where cited_
  - [20] "The manifest is signed with an Anthropic GPG key, so verifying the signature on the manifest transitively verifies every binary it lists." (Binary integrity and code signing) · quote found
  - [20] "Manifest signatures are available for releases from `2.1.89` onward." (Binary integrity and code signing) · quote found
  - [9] "[GNUPG:] VALIDSIG 31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE 2026-10-26 1793042338 0 4 0 1 10 00 31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE" (2.1.311 verify) · quote found
  - [12] "VERIFIED 2.1.311-linux-x64 sha256=86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada" (fetch check) · quote found
  - [8] "GnuPG `VALIDSIG` of `manifest.json.sig` by the documented primary key" (s_bringup, README.md line 34) · quote found
- **A · 99 (high)** Anthropic's stable channel serves a version about a week old that skips releases with major regressions; stable was 2.1.311 (built 2026-10-26) and latest 2.1.322 at 06:27Z on 2026-11-12, and still at 13:52Z, so the pin is read from the stable channel when step 1 runs. [20][9][7]
  _3 independent sources (official docs, logs); 4 of 4 quotes found where cited_
  - [20] "use a version that is typically about one week old, skipping releases with major regressions" (Configure release channel) · quote found
  - [9] "CHANNEL stable=2.1.311 latest=2.1.322" (channels) · quote found
  - [9] "MANIFEST version=2.1.311 commit=e7d3b8365a0ffbe2f5dd032feb28e9fd9c7de025 buildDate=2026-10-26T19:03:46Z" (2.1.311 manifest) · quote found
  - [7] "CHANNEL stable=2.1.311 latest=2.1.322 read_at=2026-11-12T13:52:22Z" (line 2) · quote found
- **B · 80 (good)** The glibc builds link only glibc's own libraries (librt, libc, the loader, libpthread, libdl, libm) at symbol versions no newer than GLIBC_2.17, on x86_64 and aarch64, and 2.1.311's x86_64 build ran on a glibc host as is; so a glibc base needs no package installed (no RUN). Anthropic supports glibc distributions from Debian 10 and Ubuntu 20.04, the support boundary the plan states. [10][12][20]
  _3 independent sources (logs, official docs); 5 of 5 quotes found where cited; inferred from them_
  - [10] "NEEDED librt.so.1 libc.so.6 ld-linux-x86-64.so.2 libpthread.so.0 libdl.so.2 libm.so.6" (2.1.311 linux-x64) · quote found
  - [10] "VERNEED libc.so.6 max GLIBC_2.17 n 14" (2.1.311 linux-x64) · quote found
  - [10] "NEEDED librt.so.1 libc.so.6 ld-linux-aarch64.so.1 libpthread.so.0 libdl.so.2 libm.so.6" (2.1.311 linux-arm64) · quote found
  - [12] "VERSION_OUT 2.1.311: 2.1.311 (Claude Code)" (version) · quote found
  - [20] "Debian 10+" (System requirements) · quote found
- **A · 100 (high)** The musl builds need musl's loader (/lib/ld-musl-x86_64.so.1, /lib/ld-musl-aarch64.so.1), so on a glibc base they do not start (exit 127, No such file or directory); on Alpine and other musl distributions Anthropic requires libgcc, libstdc++ and ripgrep at run time with USE_BUILTIN_RIPGREP=0, which makes Claude Code use the system's rg; the bring-up's Alpine image added libgcc and libstdc++. [10][12][20][22][8]
  _4 independent sources (logs, official docs); 7 of 7 quotes found where cited_
  - [10] "INTERP /lib/ld-musl-x86_64.so.1" (linux-x64-musl) · quote found
  - [10] "INTERP /lib/ld-musl-aarch64.so.1" (linux-arm64-musl) · quote found
  - [12] "No such file or directory exit=127" (musl build on the glibc host) · quote found
  - [20] "Installing Claude Code on Alpine and other musl/uClibc-based distributions requires `bash` and `curl` for the install command, and `libgcc`, `libstdc++`, and `ripgrep` at runtime." (Alpine Linux and musl-based distributions) · quote found
  - [20] "Install these packages using your distribution's package manager, then set `USE_BUILTIN_RIPGREP=0`." (Alpine Linux and musl-based distributions) · quote found
  - [22] "| `USE_BUILTIN_RIPGREP` | Set to `0` to use system-installed `rg` instead of `rg` included with Claude Code |" (USE_BUILTIN_RIPGREP) · quote found
  - [8] "RUN apk add --no-cache libgcc libstdc++" (s_example, Agentfile line 13) · quote found
- **A · 97 (high)** claude --version prints the version followed by (Claude Code), for example 2.1.311 (Claude Code), with no credential needed. [26][20][12]
  _2 independent sources (official docs, logs); 4 of 4 quotes found where cited_
  - [26] "Output the version number" (CLI flags, --version) · quote found
  - [20] "To confirm which version installed, run `claude --version`: the command prints the exact version you passed, such as `2.1.89 (Claude Code)`." (Install a specific version) · quote found
  - [12] "VERSION_OUT 2.1.311: 2.1.311 (Claude Code)" (version) · quote found
  - [12] "VERSION_OUT 2.1.322: 2.1.322 (Claude Code)" (version) · quote found
- **A · 96 (high)** Without a credential Claude Code refuses locally and makes no API request (duration_api_ms 0, exit 1), so a run carrying no secret can be smoked safely; with a key its 401 retries take about 250 s. [8][12]
  _2 independent sources (logs); 4 of 4 quotes found where cited_
  - [8] "locally (`Not logged in · Please run /login`, `duration_api_ms: 0`, no request), so its reach" (s_bringup, README.md line 290) · quote found
  - [8] "is shown with a placeholder key. With a key it retries a 401 ten times with backoff" (s_bringup, README.md line 291) · quote found
  - [12] "PRINT_EXIT 1" (-p with no credential) · quote found
  - [12] "PRINT_FIELDS {"duration_api_ms":0,"is_error":true,"terminal_reason":"api_error"}" (-p with no credential) · quote found
- **B · 80 (good)** Three official routes put Claude Code in a Linux image: the per-platform binary pinned through its signed manifest; Anthropic's signed apt, dnf and apk repositories; and the npm package (the same native binary), which T4 used. The pinned binary needs no package manager, no Node and, on a glibc base, no RUN step, and pins an exact digest; the repositories and npm each need a RUN with network at build time and a base with that package manager. [20][8][10]
  _3 independent sources (official docs, logs); 4 of 4 quotes found where cited; inferred from them_
  - [20] "Claude Code publishes signed apt, dnf, and apk repositories." (Install with Linux package managers) · quote found
  - [20] "The npm package installs the same native binary as the standalone installer." (Install with npm) · quote found
  - [8] "RUN npm install -g "@anthropic-ai/claude-code@${VERSION}" && npm cache clean --force" (s_t4_af, claude.Agentfile line 14) · quote found
  - [10] "NEEDED librt.so.1 libc.so.6 ld-linux-x86-64.so.2 libpthread.so.0 libdl.so.2 libm.so.6" (2.1.311 linux-x64) · quote found
- **B · 78 (good)** Claude Code's documented network needs are HTTPS hosts (api.anthropic.com among them), which TCP 443 with DNS covers, and its feature flags come from api.anthropic.com; turning off non-essential traffic or telemetry also turns off feature-flag fetching (Remote Control and other features become unavailable) and availability checks such as fast mode's. DISABLE_UPDATES blocks every update, manual ones included, is documented for distributing Claude Code through one's own channels, and the 2.1.311 binary knows it. So a pinned image sets DISABLE_UPDATES=1 and leaves telemetry and non-essential traffic as on the host. [24][25][22][20][12]
  _2 independent sources (official docs, logs); 8 of 8 quotes found where cited; inferred from them_
  - [24] "Claude Code requires access to the following URLs. Allowlist these in your proxy configuration and firewall rules, especially in containerized or restricted network environments." (Network access requirements) · quote found
  - [24] "| `api.anthropic.com` | Claude API requests, including the WebFetch [domain safety check](https://code.claude.com/docs/en/data-usage.md#webfetch-domain-safety-check), feature flag fetches, and telemetry event logging |" (Network access requirements table) · quote found
  - [25] "Setting `DISABLE_TELEMETRY` or `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` also disables feature-flag evaluation" (Telemetry services) · quote found
  - [22] "availability checks such as the [fast mode](https://code.claude.com/docs/en/fast-mode.md#use-fast-mode-behind-proxies-and-llm-gateways) check" (CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) · quote found
  - [22] "Also disables feature-flag fetching, which makes [Remote Control](https://code.claude.com/docs/en/remote-control.md#requirements) and the other" (CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) · quote found
  - [22] "| `DISABLE_UPDATES` | Set to `1` to block all updates including manual `claude update` and `claude install`. Stricter than `DISABLE_AUTOUPDATER`." (DISABLE_UPDATES) · quote found
  - [20] "Use this when you distribute Claude Code through your own channels and need users to stay on the version you provide." (Disable auto-updates) · quote found
  - [12] "STRING 2.1.311 DISABLE_UPDATES count=8" (strings in the pinned binary) · quote found
- **B · 80 (good)** At Shards origin/main 8f3cd5b74, which carries 997f4e47e, init gives a harness PATH=<its directory>:<its directory>/bin:Docker's default PATH, HOME=/tmp and SHARDS_DOMAIN, then applies its OSI config's run.env by name, the last given winning, so a run.env HOME (or PATH) replaces init's; SHARDS_DOMAIN in run.env is refused at build (osi::Config::check) and at run; and a bare program name such as claude is looked up on that PATH, the harness's own bin first. Before 997f4e47e, run.env was appended after init's entries and programs read the first, so no harness.json could set HOME. [4]
  _1 independent source (logs); 9 of 9 quotes found where cited_
  - [4] "/// A supplied domain's environment: init's `PATH` (its own directory first), `HOME=/tmp` and" (crates/init/src/domains.rs 519) · quote found
  - [4] "/// `SHARDS_DOMAIN`, then its config's `run.env`, each name once and the last given winning, so a" (crates/init/src/domains.rs 520) · quote found
  - [4] "Some(kept) => kept.clone_from(e)," (crates/init/src/domains.rs 543) · quote found
  - [4] "let path = format!("{dir}:{dir}/bin:{PATH}");" (crates/init/src/domains.rs 587) · quote found
  - [4] "// Its program is found on the PATH it runs with: init's, unless run.env gave its own." (crates/init/src/domains.rs 590) · quote found
  - [4] "init gave a supplied domain PATH, HOME=/tmp and SHARDS_DOMAIN, then appended run.env without" (git log -1 997f4e47e) · quote found
  - [4] "removing duplicates. Go and glibc's getenv read the first, so no harness.json could set HOME or" (git log -1 997f4e47e) · quote found
  - [4] "an OSI config's run.env sets SHARDS_DOMAIN, which the runtime sets: a domain's own name" (crates/image/src/osi.rs 222) · quote found
  - [4] "997f4e47e IN origin/main 8f3cd5b7411dfeea47956a5143eec2c3466b058f" (merge-base check) · quote found
- **A · 99 (high)** A harness's environment reaches it only through its image's OSI config until step 7 lands: a run's -e reaches the run's command only, and a member gets no image ENV. harness.json's simple form (entrypoint, options) cannot set environment; its full OSI form can (schemaVersion, name, version, run.command, run.env, run.workdir), refusing unknown fields, and the bring-up gave Codex CODEX_HOME through it. [1][4][8]
  _3 independent sources (kiln's records, logs); 8 of 8 quotes found where cited_
  - [1] "Today a member gets HOME=/tmp, PATH and SHARDS_DOMAIN only: no image ENV, no run-time env, and a read-only working directory." (step 7, description) · quote found
  - [4] "Reaches the run's command only." (harness-bringup-v1/README.md 224) · quote found
  - [8] "2. The simple `harness.json` cannot point a harness at its credential by environment" (s_bringup, README.md line 234) · quote found
  - [8] "its full OSI form can, and a `--settings`-style" (s_bringup, README.md line 235) · quote found
  - [8] "pub env: Vec<String>," (s_osi, osi.rs line 84) · quote found
  - [4] "#[serde(deny_unknown_fields)]" (crates/image/src/osi.rs 80) · quote found
  - [4] "pub version: String," (crates/image/src/osi.rs 130) · quote found
  - [8] ""env":["CODEX_HOME=/run/codex-home"]" (s_codex_full, codex-login-volume/harness.json) · quote found
- **A · 99 (high)** No Shards release carries 997f4e47e: the latest release is v0.7.0 (2026-11-05), which lacks harness.json startup itself (c3b7bde8b), and no tag contains 997f4e47e; a shards built from main still reports 0.7.0, so its version cannot tell a fixed build from v0.7.0. ROOT has said v0.8.0 will be the first release with 997f4e47e and 1da9132b7, so kiln's floor is v0.8.0, and until it is cut the smokes use a build from Shards main at or after 997f4e47e with its commit recorded. [4][14][18]
  _3 independent sources (logs, a document); 7 of 7 quotes found where cited_
  - [4] "tags containing 997f4e47e: []" (tags check) · quote found
  - [4] "c3b7bde8b (startup.rs) NOT in v0.7.0" (merge-base check) · quote found
  - [4] "1da9132b7 IN origin/main 8f3cd5b7411dfeea47956a5143eec2c3466b058f" (merge-base check) · quote found
  - [14] ""at":"2026-11-05T22:48:00Z","tag":"v0.7.0"" (gh release view) · quote found
  - [18] "a shards binary can't show whether it has the fix. Main still says version 0.7.0, the same as v0.7.0." (Version gate) · quote found
  - [18] "**v0.8.0** will be the first release with 997f4e47e and 1da9132b7" (ROOT's answer) · quote found
  - [18] "kiln's floor is v0.8.0: CI pins it, the fallback refuses anything below 0.8.0, and smokes until then use a main build at or after 997f4e47e with its commit recorded." (ROOT's answer) · quote found
- **A · 98 (high)** A harness domain sees the image's system read-only (nosuid, nodev) and writes only its volumes and a scratch tmpfs of its own at /tmp that is lost when it ends; so with HOME outside /tmp, anything written under HOME that is not a volume fails (step 26 found ~/.gitconfig and ~/.npm would hit EROFS), and a writable, lasting HOME has to be a volume, as master step 7 asks (a writable private HOME on a volume its owner names). [4][18][1]
  _3 independent sources (logs, a document, kiln's records); 6 of 6 quotes found where cited_
  - [4] "sees the image's system as built, read-only, `nosuid` and `nodev` (workloads inherit" (crates/init/src/domains.rs 11) · quote found
  - [4] "own and an `mqueue` of its IPC namespace's; and a scratch tmpfs of its own at" (crates/init/src/domains.rs 17-18) · quote found
  - [4] "// The microVM's system, read-only, nosuid and nodev (struct mount_attr)." (crates/init/src/domains.rs 2350) · quote found
  - [4] "// Its volumes, written as it pleases where they are not read-only (D109)." (crates/init/src/domains.rs 2685) · quote found
  - [18] "With HOME set outside /tmp, only volume paths are writable, so `~/.gitconfig` and `~/.npm` would hit EROFS." (Read-only image) · quote found
  - [1] "a writable private HOME, on a volume its owner names or a tmpfs otherwise;" (step 7, description) · quote found
- **B · 80 (good)** Init makes each domain's /tmp a fresh scratch tmpfs of its own and attaches it before the domain's volumes, whose mount points are looked up after it, so a VOLUME at or under /tmp would land inside that scratch rather than the image's /tmp (how it then behaves is unmeasured); ROOT requires no volume there and is not changing the scratch. A mount over a directory hides what the directory held while it stays mounted. [4][18][2][34]
  _4 independent sources (logs, a document, official docs); 6 of 6 quotes found where cited; inferred from them_
  - [4] "/// A domain's scratch, `/tmp` as it sees it, where the image has a `/tmp` (D139): a tmpfs of" (crates/init/src/domains.rs 878) · quote found
  - [4] "// Its own scratch: /tmp, init's mount of it, which its server instance holds too" (crates/init/src/domains.rs 2418) · quote found
  - [4] "// Its volumes, at their mount points." (crates/init/src/domains.rs 2524) · quote found
  - [18] "A VOLUME under `/tmp` doesn't help: each domain's scratch tmpfs covers it, and ROOT is not changing that (noted on master step 7)." (Why) · quote found
  - [2] "and no volume under /tmp" (the goal, line 3) · quote found
  - [34] "The previous contents (if any) and owner and" (DESCRIPTION) · quote found
- **A · 99 (high)** A harness gets a volume only through VOLUME --target-kind=harness <path> FOR <harness>; at run time -v NAME:<path> or -v HOSTDIR:<path> stands in for it, the share that harness's alone, its host-made files shown owned by the harness's uid with the host's modes kept; a volume the Agentfile itself names is one named volume kept from run to run, so a per-user volume is the run's -v at a mount point the Agentfile declares, never a name written in the Agentfile. [8][4][14]
  _3 independent sources (logs); 6 of 6 quotes found where cited_
  - [8] "`--target-kind=<agent|harness>` (added 2026-10-01): whether the names after `FOR`" (s_af_arch, AGENTFILE_ARCH.md line 182) · quote found
  - [8] "A host directory given at run time to a volume declared for the harness alone" (s_bringup, README.md line 225) · quote found
  - [4] "**Works.** The share is the harness's alone" (harness-bringup-v1/README.md 225) · quote found
  - [4] "its host-made files shown owned by the harness's uid, the host's modes kept" (harness-bringup-v1/README.md 225) · quote found
  - [14] "A run's own `-v` or `--mount` of a volume or a directory at one of them stands in for it," (architecture.md D109, The daemon) · quote found
  - [14] "- A volume the Agentfile names is that named volume, made where it is missing, kept from" (architecture.md D109, line 17916) · quote found
- **A · 99 (high)** kiln reads a session's diffs, HEAD and checkpoints at its host path, so a member must see the project at that same path (step 8, run-time plugging, not landed); until then the project reaches the harness only through a VOLUME the Agentfile declares at a fixed path (/workspace), given its host directory with -v. T4 shared the folder at its own path instead. [1][8][6]
  _3 independent sources (kiln's records, logs); 3 of 3 quotes found where cited_
  - [1] "so a member must see the project at that same absolute path, writable, with its writes landing on the host." (step 8, description) · quote found
  - [8] "is shared with it writable at its own path, so what the harness says of its files" (s_t4, docs/isolation.md line 59) · quote found
  - [6] "VOLUME --target-kind=harness /workspace FOR amp" (s24-amp-agentfile Agentfile) · quote found
- **B · 78 (good)** HARNESS takes a git URL, an http(s) URL, a build-host path or an OCI reference; a path is copied from the build context as COPY copies, and an http(s) source is downloaded by the build with no checksum given (BuildKit's own ADD --checksum, which would check one, is not what HARNESS uses); so a binary pinned by digest is fetched and checked by kiln before the build and handed to it as a build-context path. [8][37]
  _2 independent sources (logs, official docs); 4 of 4 quotes found where cited; inferred from them_
  - [8] "| path on the build host | copies it, as `COPY` copies |" (s_af_arch, AGENTFILE_ARCH.md line 273) · quote found
  - [8] "let st = self.graph.source(url, attrs, None, custom_name(shown.clone()));" (s_planrs, plan.rs line 4544) · quote found
  - [8] "crate::agentfile::Source::Path(p) => {" (s_planrs, plan.rs line 4549) · quote found
  - [37] "flag lets you verify the checksum of a remote Git or HTTP" (ADD --checksum) · quote found
- **B · 78 (good)** A HARNESS (or AGENT) source expands build arguments where it is laid out, and TARGETARCH is set from each target platform; BuildKit defines it in the global scope, so a stage redeclares ARG TARGETARCH to use it. One Agentfile can thus take ./claude-${TARGETARCH}-<libc> for each platform of a multi-platform build (step 4's build checks it). [8][28]
  _2 independent sources (logs, official docs); 5 of 5 quotes found where cited; inferred from them_
  - [8] "// Expanded where it is laid out too." (s_buildtest, build.rs line 9887) · quote found
  - [8] "AGENT main FROM $SRC" (s_buildtest, build.rs line 9890) · quote found
  - [8] "(b"TARGETARCH", target.architecture.clone())," (s_planrs, plan.rs line 1633) · quote found
  - [28] "These arguments are defined in the global scope so are not automatically" (Automatic platform ARGs in the global scope) · quote found
  - [28] "- `TARGETARCH` - architecture component of TARGETPLATFORM" (Automatic platform ARGs in the global scope) · quote found
- **B · 79 (good)** An ARG declared before the first FROM can pick a stage (FROM base-${CLAUDE_LIBC}), and BuildKit builds only the stages the target depends on, so musl's package step runs only in a musl build and a glibc build runs no RUN at all; shards holds a Dockerfile's meaning byte for byte to BuildKit's. [28][29][19]
  _3 independent sources (official docs, a document); 3 of 3 quotes found where cited; inferred from them_
  - [28] "`FROM` instructions support variables that are declared by any `ARG`" (Understand how ARG and FROM interact) · quote found
  - [29] "only builds the stages that the target stage" (Differences between legacy builder and BuildKit) · quote found
  - [19] "A Dockerfile's meaning is held byte for byte to BuildKit's by `tests/oracle.rs`" (Where things are, crates/agentfile) · quote found
- **A · 100 (high)** shards builds several platforms in one command (--platform linux/amd64,linux/arm64), running another CPU's RUN steps emulated at 4.6 to 5.5 times the cycles; a multi-platform build is an index of its platforms, and shards push sends the index with every platform's manifest, the OCI image index being the manifest for one or more platforms. [8][14][32]
  _3 independent sources (logs, a standard); 5 of 5 quotes found where cited_
  - [8] "build for both (`--platform linux/amd64,linux/arm64`, D87) runs each platform's RUN steps." (s_arch, architecture.md line 10961) · quote found
  - [8] "From the host's side, an emulated run costs 4.6 and 5.5 times the cycles of a native one" (s_arch, architecture.md line 10982) · quote found
  - [14] "`a_builds_platform_defaults_to_the_default_platforms` (an index of two platforms: the" (architecture.md line 11114) · quote found
  - [14] "- `shards push image` sends the index together with every platform's manifest." (architecture.md D97, line 19115) · quote found
  - [32] "The image index is a higher-level manifest which points to specific [image manifests](manifest.md), ideal for one or more platforms." (opening paragraph) · quote found
- **A · 93 (high)** Today a harness's provider reach is three directives, proven by the bring-up: NETWORK --dns --egress=443/tcp provider FOR claude, EXPOSE --harnesses=claude 443/tcp AS egress FOR provider, and CONNECT claude WITH claude ON provider (joined, no flow to any member). The egress names a port, not destinations (H2); step 9 adds grants of providers by name. [8][1]
  _2 independent sources (logs, kiln's records); 6 of 6 quotes found where cited_
  - [8] "NETWORK --dns --egress=443/tcp provider FOR claude" (s_example, Agentfile line 22) · quote found
  - [8] "EXPOSE --harnesses=claude 443/tcp AS egress FOR provider" (s_example, Agentfile line 23) · quote found
  - [8] "CONNECT claude WITH claude ON provider" (s_example, Agentfile line 24) · quote found
  - [8] "(`CONNECT a WITH a ON n`) attaches it to the networks and grants no flow." (s_af_arch, AGENTFILE_ARCH.md line 236) · quote found
  - [8] "**H2: egress names ports, not destinations.** `--egress=443/tcp` with `EXPOSE … AS egress`" (s_bringup, README.md line 268) · quote found
  - [1] "its providers by name (api.anthropic.com:443, api.openai.com:443, …) where an Agentfile asks for names rather than ports." (step 9, description) · quote found
- **A · 93 (high)** The bring-up started Claude Code (musl, x86_64) as a Shards harness from HARNESS and harness.json alone on Alpine, read its output and kept every credential out of the image; it was verified on Linux x86_64 with KVM only, and its fetch.sh fetches x86_64 builds only. [8][1]
  _2 independent sources (logs, kiln's records); 4 of 4 quotes found where cited_
  - [8] "| `a_real_claude_code_starts_as_a_harness_and_refuses_without_a_login` (`claude`) | pass | 0.78 s, End | the example as published: Claude Code exec'd as declared, its local refusal |" (s_bringup, README.md line 105) · quote found
  - [8] "no credential enters an image." (s_bringup, README.md line 8) · quote found
  - [8] "Verified: Linux x86_64 on KVM only." (s_bringup, README.md line 310) · quote found
  - [1] "fetch.sh takes x86_64 only so far; arm64 is owed." (step 19, description) · quote found
- **B · 77 (good)** D115 confines a run's own command by a PID namespace of its own and by refusing the flags and IDs that would reach a domain; the hide list that covers other domains' directories is built for each domain, and nothing read hides /harness/<name> (where the build lays a harness out) from the run's command, but no run has yet shown that the command can exec it; and no command reads a member's output (H3). So the version smoke tries /harness/claude/bin/claude --version as the run's command first, and a harness-domain probe that writes its version into the project volume is the fallback. [4][8]
  _2 independent sources (logs); 5 of 5 quotes found where cited; inferred from them_
  - [4] "### D115. A run's command reaches none of its image's agents and harnesses" (docs/design/architecture.md 17106) · quote found
  - [4] "**The run's own command is confined from the domains too** (D115). The workload a run's" (docs/architecture/AGENTFILE_ARCH.md 640) · quote found
  - [4] ".filter(|x| **x != d.dir)" (crates/init/src/domains.rs 1822-1824) · quote found
  - [4] "- A harness is a file artifact. The build unpacks it to `/harness/<name>`, or to" (docs/architecture/AGENTFILE_ARCH.md 276) · quote found
  - [8] "**H3: no command reads a member's output.** It goes only to the owner endpoint" (s_bringup, README.md line 276) · quote found
- **B · 80 (good)** Without CLAUDE_CONFIG_DIR, Claude Code keeps its settings, session history and plugins under ~/.claude, its own state file at ~/.claude.json and, on Linux, its sign-in at ~/.claude/.credentials.json; so with HOME=/home/claude on a per-user volume mounted at /home/claude, all of a user's Claude Code state lies in that volume, laid out as in a host user's home (FHS keeps users' application configuration as dot files in their home directory), and kiln resolves the config dir the same way (CLAUDE_CONFIG_DIR, else ~/.claude). CLAUDE_CONFIG_DIR then adds nothing. [22][21][23][33][5]
  _3 independent sources (official docs, a standard, logs); 5 of 5 quotes found where cited; inferred from them_
  - [22] "| `CLAUDE_CONFIG_DIR` | Override the configuration directory (default: `~/.claude`). All settings, session history, and plugins are stored under this path." (CLAUDE_CONFIG_DIR) · quote found
  - [21] "Claude Code also keeps a fifth file, [`~/.claude.json`](https://code.claude.com/docs/en/claude-directory#ce-claude-json), that it writes for itself; you don't need to edit it." (Find or create your settings files) · quote found
  - [23] "On Linux, credentials are stored in `~/.claude/.credentials.json` with file mode `0600`." (Credential management) · quote found
  - [33] "User specific configuration files for applications are stored in the" (3.8.2 Requirements) · quote found
  - [5] "/// session's environment or the machine's, else `~/.claude`." (crates/kiln/src/harness/kinds.rs 499) · quote found
- **A · 97 (high)** With CLAUDE_CONFIG_DIR set, everything Claude Code wrote in the measured runs (2.1.311 --version, -p with no credential, mcp add --scope user) landed in that directory, .claude.json and session transcripts under projects/ among them, and HOME stayed empty; on Linux its sign-in (.credentials.json) goes there too. So whichever way the config directory is named, it holds all of a user's Claude Code state, sign-in included. [12][21][23]
  _2 independent sources (logs, official docs); 6 of 6 quotes found where cited_
  - [12] "PROBE_IN_CONFIG_DIR_CLAUDE_JSON yes" (after mcp add --scope user) · quote found
  - [12] "FILES_IN_HOME_COUNT 0" (HOME after both runs) · quote found
  - [12] "./projects/-home-adalundhe--cache-s19-claude-research-bin-20261112T050222Z" (FILES_IN_CONFIG_DIR) · quote found
  - [21] "To keep the home-directory files somewhere else, set [`CLAUDE_CONFIG_DIR`](https://code.claude.com/docs/en/env-vars); Claude Code then stores your settings, session history, and plugins there instead." (Find or create your settings files) · quote found
  - [23] "If you've set the `CLAUDE_CONFIG_DIR` environment variable, Claude Code keeps the `.credentials.json` file under that directory instead" (Credential management) · quote found
  - [21] "It holds your account details, [MCP server](https://code.claude.com/docs/en/mcp.md) configurations, per-project state such as trust decisions" (Find or create your settings files) · quote found
- **A · 99 (high)** Every flag kiln's Claude adapter passes is accepted by Claude Code 2.1.311 and 2.1.322: each is in --help except --resume-session-at, which the binary carries unlisted; kiln's adapter was written against Claude Code 2.1.296. [8][15][12]
  _3 independent sources (logs); 6 of 6 quotes found where cited_
  - [8] ""--forward-subagent-text"," (s_claude_rs, claude.rs line 57) · quote found
  - [8] "a.extend(["--resume-session-at".to_owned(), at.to_owned()]);" (s_claude_rs, claude.rs line 81) · quote found
  - [15] "//! Message shapes were taken from Claude Code 2.1.296" (claude.rs line 8) · quote found
  - [12] "FLAG 2.1.311 --forward-subagent-text help=1 binary_quoted=2" (flags) · quote found
  - [12] "FLAG 2.1.311 --resume-session-at help=0 binary_quoted=3" (flags) · quote found
  - [12] "FLAG 2.1.311 --permission-prompt-tool help=1 binary_quoted=4" (flags) · quote found
- **B · 78 (good)** kiln starts Claude Code as the program claude with claude::args: eleven fixed arguments (-p, --input-format stream-json, --output-format stream-json, --verbose, --include-partial-messages, --replay-user-messages, --forward-subagent-text, --permission-prompt-tool stdio), then a session's own (--model, --permission-mode, --effort, --resume or --session-id); it adds --mcp-config and --plugin-dir for each session, and it always sets CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=1, which a bare claude -p needs for file rewinds (the SDK sets it itself, the CLI never does). So the image's harness.json carries the fixed arguments and that variable, and a session's own words come at launch (steps 7 and 17). [5][36]
  _2 independent sources (logs, official docs); 9 of 9 quotes found where cited; inferred from them_
  - [5] "Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson" (crates/kiln/src/harness/kinds.rs 51) · quote found
  - [5] "p.args(super::claude::args(spec, session_id.as_deref()));" (crates/kiln/src/harness/kinds.rs 121) · quote found
  - [5] "p.arg("--mcp-config").arg(crate::reach::mcp::claude_config(kiln, sock, *prefer, session));" (crates/kiln/src/harness/kinds.rs 129) · quote found
  - [5] "p.arg("--plugin-dir").arg(dir);" (crates/kiln/src/harness/kinds.rs 139) · quote found
  - [5] "// File snapshots before edits, so a rewind can restore them." (crates/kiln/src/harness/kinds.rs 146) · quote found
  - [5] "p.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");" (crates/kiln/src/harness/kinds.rs 147) · quote found
  - [5] ""--permission-prompt-tool"," (crates/kiln/src/harness/claude.rs 58) · quote found
  - [36] "The SDK enables checkpointing for you, but when you run `claude -p` directly you must set the `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` environment variable:" (Rewind from the CLI) · quote found
  - [36] "the bare CLI never sets it." (Troubleshooting) · quote found
- **A · 93 (high)** kiln resolves Claude Code's config dir as Claude Code does (CLAUDE_CONFIG_DIR from the session's or the machine's environment, else ~/.claude) and reads from it a session's transcript and its skill roots, not .claude.json; step 18 will read that directory on the host from the per-user volume, for transcripts and resume. [5][1]
  _2 independent sources (logs, kiln's records); 4 of 4 quotes found where cited_
  - [5] "/// Where Claude Code keeps its configuration for this session: `CLAUDE_CONFIG_DIR` from the" (crates/kiln/src/harness/kinds.rs 498) · quote found
  - [5] "roots.push(std::path::PathBuf::from(&self.config_dir));" (crates/kiln/src/harness/claude.rs 1495) · quote found
  - [5] "core.effects.push(Effect::TranscriptTail { job, config: self.config_dir.clone(), session, bytes: 512 * 1024 });" (crates/kiln/src/harness/claude.rs 3156) · quote found
  - [1] "Config dir (CLAUDE_CONFIG_DIR, CODEX_HOME, …): on a kiln-managed per-user volume that kiln reads on the host for transcripts and resume." (step 18, description) · quote found
- **B · 74 (good)** Step 30's table of shipped Agentfiles is on kiln main (94639db7b): crates/kiln/src/agentfiles/builtin.rs's Builtin {name, harness, text}, ALL still empty, each harness's own step adding its row with include_str!, a test that every built-in is a valid Agentfile under a unique name of a known harness, and the built-ins listed read-only beside a person's own. Step 19's row is the first; a second table would be a parallel owner, so pin and files join this struct. [5][1]
  _2 independent sources (logs, kiln's records); 5 of 5 quotes found where cited; inferred from them_
  - [5] "//! one binary, listed read-only beside a person's own, and copied to make one theirs. None yet: each" (crates/kiln/src/agentfiles/builtin.rs 3) · quote found
  - [5] "pub const ALL: &[Builtin] = &[];" (crates/kiln/src/agentfiles/builtin.rs 18) · quote found
  - [5] "pub harness: &'static str," (crates/kiln/src/agentfiles/builtin.rs 13) · quote found
  - [5] "fn every_builtin_is_a_valid_agentfile_once() {" (crates/kiln/src/agentfiles/builtin.rs, tests) · quote found
  - [1] "Built-in (embedded) Agentfiles are listed read-only beside the user's own and the project's .kiln/images/<name>/Agentfile." (step 30, description) · quote found
- **B · 78 (good)** kiln's Dockerfile copies Cargo's files, .cargo/, crates/ and the vendored workspaces into its build stage before its cargo build, and not .kiln/, while its .dockerignore leaves .kiln/ in the context; a stage sees only what COPY brings it from the build context, and every include_str! in crates/kiln/src that reaches outside crates/ today is inside a test. So the first row's include_str! of .kiln/images/harness-claude/ files would break kiln's Docker build unless the build stage copies .kiln/images/, which a test reading the Dockerfile can hold. [5][37]
  _2 independent sources (logs, official docs); 7 of 7 quotes found where cited; inferred from them_
  - [5] "COPY crates/ crates/" (Dockerfile 45) · quote found
  - [5] "COPY vendor/turso/ vendor/turso/" (Dockerfile 47) · quote found
  - [5] "cargo build --locked --release --target "$triple" -p kiln;" (Dockerfile 75) · quote found
  - [5] "# Build context for the top-level Dockerfile: only Cargo workspace, crates/, ui/, vendor/ and deploy/docker/." (.dockerignore 1) · quote found
  - [5] "fn docs_sends_says_what_the_code_does() {" (crates/kiln/src/harness/inbox/mod.rs 1475-1477) · quote found
  - [5] "fn wire_registry_holds_every_bit_and_variant_once() -> anyhow::Result<()> {" (crates/kiln/src/proto.rs 2200-2202) · quote found
  - [37] "can be copied from the build context, build stage, named context, or" (COPY) · quote found
- **C · 64 (moderate)** kiln's subcommands are clap Args structs in a module's own cli.rs (a subcommand enum and an async run), each a variant of entry.rs's Cmd dispatched to that run; kiln has no agentfiles subcommand yet, so the fetcher's kiln agentfiles context adds crates/kiln/src/agentfiles/cli.rs and one Cmd variant. [5]
  _1 independent source (logs); 5 of 5 quotes found where cited; inferred from them_
  - [5] "pub struct BlobsArgs {" (crates/kiln/src/blobs/cli.rs 12-13) · quote found
  - [5] "pub async fn run(args: BlobsArgs, cfg: &Config) -> Result<()> {" (crates/kiln/src/blobs/cli.rs 26) · quote found
  - [5] "Blobs(kiln::blobs::cli::BlobsArgs)," (crates/kiln/src/entry.rs 279) · quote found
  - [5] "Cmd::Blobs(a) => {" (crates/kiln/src/entry.rs 1210) · quote found
  - [5] "## the CLI's agentfiles words (git grep)" (the CLI's agentfiles words (git grep)) · quote found
- **B · 77 (good)** kiln keeps the image definitions its CI builds at .kiln/images/<name>/Agentfile (or Dockerfile), kiln/build-image@v1 takes .kiln/images/<name> as its context by default, and every sibling keeps its harness at .kiln/images/harness-<id>/; so Claude's lives at .kiln/images/harness-claude/. [5][6]
  _2 independent sources (logs); 3 of 3 quotes found where cited; inferred from them_
  - [5] "(`.kiln/images/<name>/Dockerfile` or `.kiln/images/<name>/Agentfile`) or are made in the UI" (docs/ci.md 4415, W13-R11) · quote found
  - [5] "let context = get("context").unwrap_or_else(|| format!(".kiln/images/{name}"));" (crates/kiln/src/runner/runtime.rs 3946) · quote found
  - [6] "### s23-goose-agentfile .kiln/images/harness-goose/Agentfile" (Agentfile instructions) · quote found
- **A · 96 (high)** kiln already builds Agentfiles in CI: kiln/build-image@v1 runs shards build (with --platform when given) and shards push into kiln's own registry at <registry>/<repo>/images/<name>:<tag>, and fails where shards is not on the runner's PATH; kiln's registry is OCI Distribution v1.1 and stores indexes, and kiln registers a built image only as its registry holds it, the platforms read from the stored manifest. No kiln CI workflow under .kiln/ names shards yet; runners report shards --version as a tool fact. [8][5]
  _2 independent sources (logs); 9 of 9 quotes found where cited_
  - [8] "an Agentfile through `shards build` and `shards push` (shards' own, never" (s_runtime, runtime.rs line 3938) · quote found
  - [8] "let base = format!("{own}/{}/images/{name}", self.a.repo.to_ascii_lowercase());" (s_runtime, runtime.rs line 3964) · quote found
  - [8] "args.extend(["--platform".into(), platforms.join(",")]);" (s_runtime, runtime.rs line 3981) · quote found
  - [8] "is built by {prog}, which isn't on this runner" (s_runtime, runtime.rs line 3960) · quote found
  - [8] "//! kiln's own OCI registry (docs/ci.md W12-R01): the OCI Distribution Specification v1.1 at" (s_registry, registry/mod.rs line 1) · quote found
  - [8] "const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";" (s_registry, registry/mod.rs line 38) · quote found
  - [5] "digest one of its manifests there, the platforms read from that manifest (an index's" (crates/kiln/src/ci/images.rs 6) · quote found
  - [5] "("shards", "shards", &["--version"])," (crates/kiln/src/runner/probe.rs 84) · quote found
  - [5] "kiln CI workflows that name shards: 0" (kiln CI workflows under .kiln/ that name shards) · quote found
- **B · 77 (good)** kiln/build-image@v1 pushes each run's image under the same <repo>/images/<name>:<tag> (and :latest), so two legs of a harness-by-arch matrix would each push a one-platform image to one tag, the second replacing the first; one shards build of both platforms per harness pushes one index instead. [5][8]
  _2 independent sources (logs); 3 of 3 quotes found where cited; inferred from them_
  - [5] "let tags = if tag == "latest" { vec![format!("{base}:latest")] } else { vec![format!("{base}:{tag}"), format!("{base}:latest")] };" (crates/kiln/src/runner/runtime.rs 3965) · quote found
  - [5] "runs.extend(tags.iter().map(|t| vec!["push".to_owned(), t.clone()]));" (crates/kiln/src/runner/runtime.rs 3987) · quote found
  - [8] "build for both (`--platform linux/amd64,linux/arm64`, D87) runs each platform's RUN steps." (s_arch, architecture.md line 10961) · quote found
- **B · 80 (good)** kiln's CI builds kiln for x86_64 musl once per commit with one step text shared by its release and image jobs (W28-R78), so a CI job can run that commit's kiln, and with it the commit's embedded Agentfiles and fetcher, from the same step. [8]
  _1 independent source (logs); 2 of 2 quotes found where cited_
  - [8] "# One x86_64-unknown-linux-musl build per commit for release and the image (W28-R78): the" (s_image_yaml, .kiln/image.yaml line 33) · quote found
  - [8] "run: KILN_COMMIT=$KILN_SHA nice -n 10 ci/release/build.sh x86_64-unknown-linux-musl" (s_image_yaml, .kiln/image.yaml line 37) · quote found
- **B · 80 (good)** kiln's unmerged T4 work embedded one Agentfile per harness with include_str!, built it with Shards on first need under a tag carrying the recipe's digest, on node:24-bookworm-slim with Claude Code from npm at the host's version, and named its base by tag, which it recorded as a reproducibility gap. [8]
  _1 independent source (logs); 4 of 4 quotes found where cited_
  - [8] ""claude" => Some(include_str!("agentfiles/claude.Agentfile"))," (s_t4_iso, isolated.rs line 67) · quote found
  - [8] "Its tag, `kiln-harness/<harness>:<version>-<recipe>`, carries a digest of the" (s_t4, docs/isolation.md line 46) · quote found
  - [8] "FROM node:24-bookworm-slim" (s_t4_af, claude.Agentfile line 9) · quote found
  - [8] "**The images' reproducibility**: kiln's Agentfiles name their base by tag" (s_t4, docs/isolation.md line 631) · quote found
- **A · 87 (high)** For a default glibc base, buildpack-deps:trixie-scm (a Docker Official Image for both arches, index sha256:4f34e1d6...) already carries git, openssh-client and procps over trixie-curl's ca-certificates, curl and gnupg, about 143 MB compressed per arch, with no RUN needed; debian:trixie-slim is about 30 MB but would need an apt-get RUN, emulated for the other CPU and resolved at build time, to add git and certificates. [11][30][31]
  _2 independent sources (logs, a vendor); 7 of 7 quotes found where cited_
  - [11] "== buildpack-deps:trixie-scm index digest: sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10" (index) · quote found
  - [11] "amd64 sha256:712f969c9d81a35f9f00cce701faae521891d42a20b592b61bd0e6c5328fad7c layers=3 compressed_bytes=142865592" (amd64) · quote found
  - [11] "arm64/v8 sha256:4256ac3dbf04b630b9082d15c426b1982ceed0b071b2abe76a40ad028d323636 layers=3 compressed_bytes=142449890" (arm64) · quote found
  - [11] "amd64 sha256:7378260c0a098d27ac206225db5e7d92b8c7c84ea7afccc86eeb513756bbd392 layers=1 compressed_bytes=29856132" (debian:trixie-slim amd64) · quote found
  - [30] "FROM buildpack-deps:trixie-curl" (Dockerfile) · quote found
  - [30] "procps is very common in build systems, and is a reasonably small package" (Dockerfile comment) · quote found
  - [31] "FROM debian:trixie" (Dockerfile) · quote found
- **B · 75 (good)** Linux arm64 microVMs run on Apple silicon Macs through shards (kiln's Mac Studio runners), and no arm64 Linux host here has KVM, so the arm64 build and smoke run on a Mac Studio runner and amd64's on a Linux runner with KVM. [5][19]
  _2 independent sources (logs, a document); 2 of 2 quotes found where cited; inferred from them_
  - [5] "silicon Macs Linux arm64 microVMs; QEMU emulation as a declared fact (W04-R13)." (docs/ci.md 4428, W13-R12) · quote found
  - [19] "No arm64 Linux host here has KVM" (Commands and gotchas) · quote found
- **A · 99 (high)** Checks against credentials exist on both sides: shards' build checks include BuildKit's SecretsUsedInArgOrEnv; the bring-up kept every credential out of its images; kiln never prints or commits a secret; step 33 keeps credential values out of images entirely. [14][8][1]
  _3 independent sources (logs, kiln's records); 4 of 4 quotes found where cited_
  - [14] "name: "SecretsUsedInArgOrEnv"," (lint.rs line 132) · quote found
  - [8] "no credential enters an image." (s_bringup, README.md line 8) · quote found
  - [8] "- **Never print or commit a secret,** kiln's admin token included." (s_kiln_agents, AGENTS.md line 61) · quote found
  - [1] "Values never appear in images, telemetry or logs." (step 33, description) · quote found
- **A · 96 (high)** kiln's rules shape the code: async on hyper-rt with kiln's own HTTP (no tokio, reqwest), no panics, only what the task asks, the touched crate's tests and clippy before handing over and the full gate only when the person asks; kiln's HTTP client already streams a response body chunk by chunk for large downloads. [8][5]
  _2 independent sources (logs); 4 of 4 quotes found where cited_
  - [8] "Async code runs on hyper-raft's `hyper-rt`; HTTP is kiln's own" (s_kiln_agents, AGENTS.md line 14) · quote found
  - [8] "Don't add tokio, hyper, h2, axum, reqwest, kube-rs or" (s_kiln_agents, AGENTS.md line 15) · quote found
  - [8] "what you touched first (its tests, clippy on its crate), then, when the person asks for the full run, your change's gate (above)" (s_kiln_agents, AGENTS.md line 103) · quote found
  - [5] "/// Stream the body chunk by chunk (`None` at the end), e.g. for large downloads." (crates/kiln/src/http/client.rs 347) · quote found

**Sources**

1. Parent plan 01a1cd8e862e7169a074db3d5fa33f3d (v11): steps 7, 8, 9, 18, 19, 30, 33 (kiln's records, internal) — plan:01a1cd8e862e7169a074db3d5fa33f3d
2. Evidence: step 19's restart goal from ROOT and step 28's implementer's offer, byte-exact from this session (2026-11-12) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_restart.txt
3. Evidence: POLICY_READ, Shards R01-R16 and evidence checklist (origin/main 8f3cd5b74) and kiln AGENTS.md (origin/main 94639db7b), with SHA-256s (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_policy.txt
4. Evidence: Shards origin/main 8f3cd5b74 excerpts (997f4e47e, tags and releases, domains.rs, osi.rs, D115, AGENTFILE_ARCH, bring-up rows, the harness-claude example) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_shards.txt
5. Evidence: kiln origin/main 94639db7b excerpts (built-ins table, Claude adapter and launch, Dockerfile, .dockerignore, embeds outside crates/, build-image@v1, images, registry, image.yaml, probe, HTTP client, ci.md, CLI wiring, workflows naming shards) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_kiln.txt
6. Evidence: steps 20-28's kiln branches and worktrees (Agentfiles, harness.json, pins, uncommitted Builtin fields) and step 25's unpacker, read 2026-11-12T13:48Z (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_siblings.txt
7. Evidence: Claude Code's stable and latest channels, 2026-11-12T13:52Z (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_channels.txt
8. Evidence: the first research's git quotes found again at Shards 8f3cd5b74, kiln 94639db7b and T4 0aeb1d58 (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/r2_regrep.txt
9. Evidence: Claude Code channels and GPG-verified manifests of 2.1.311 and 2.1.322 (2026-11-12T06:27Z) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_manifest.txt
10. Evidence: ELF interpreter, NEEDED and GLIBC versions of the eight Linux builds (2.1.311, 2.1.322) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_elf.txt
11. Evidence: Docker Hub base images, platforms and compressed sizes (2026-11-12T06:34Z) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_bases.txt
12. Evidence: Claude Code 2.1.311 and 2.1.322 run on adacachy2 (Linux x86_64, glibc 2.44): version, flags, musl on glibc, CLAUDE_CONFIG_DIR (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_bin_adacachy2.txt
13. Evidence: Shards ROOT's goals for plan steps 16, 19 and 20 (kiln goals_get, 2026-11-12) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_goals.txt
14. Evidence: Shards origin/main c4ae59ee excerpts (D109, lint.rs, startup.rs against v0.7.0, the v0.7.0 release) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_src_shards.txt
15. Evidence: kiln origin/main 1a5a5f00d excerpts (claude.rs, probe.rs, http/client.rs, ci/images.rs) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_src_kiln.txt
16. Evidence: kiln's image-definition home and build-image@v1's context and tags (1a5a5f00d) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_layout.txt
17. The person's rule of 2026-11-12: subagents test, merge their own work and end (a document, internal) — /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards/memory/feedback-subagents-merge-and-end.md
18. Step 26's finding and ROOT's answer: run.env HOME (997f4e47e), the read-only image, the v0.8.0 floor (a document, internal) — /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards/memory/project-harness-env-last-wins.md
19. Shards CLAUDE.md (repository mechanics) (a document, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/g121/CLAUDE.md
20. Claude Code: Set up (markdown) (official docs, external) — https://code.claude.com/docs/en/setup.md
21. Claude Code: settings (markdown) (official docs, external) — https://code.claude.com/docs/en/settings.md
22. Claude Code: environment variables (markdown) (official docs, external) — https://code.claude.com/docs/en/env-vars.md
23. Claude Code: authentication, credential management (markdown) (official docs, external) — https://code.claude.com/docs/en/authentication.md
24. Claude Code: network access requirements (markdown) (official docs, external) — https://code.claude.com/docs/en/network-config.md
25. Claude Code: data usage, telemetry services (markdown) (official docs, external) — https://code.claude.com/docs/en/data-usage.md
26. Claude Code: CLI reference (markdown) (official docs, external) — https://code.claude.com/docs/en/cli-reference.md
27. Anthropic's installer (bootstrap.sh) (a vendor, external) — https://downloads.claude.ai/claude-code-releases/bootstrap.sh
28. BuildKit Dockerfile reference: ARG and FROM, automatic platform ARGs (official docs, external) — https://raw.githubusercontent.com/moby/buildkit/master/frontend/dockerfile/docs/reference.md
29. Docker docs: multi-stage builds, legacy builder and BuildKit (official docs, external) — https://docs.docker.com/build/building/multi-stage/#differences-between-legacy-builder-and-buildkit
30. Docker Official Image buildpack-deps:trixie-scm Dockerfile (a vendor, external) — https://raw.githubusercontent.com/docker-library/buildpack-deps/master/debian/trixie/scm/Dockerfile
31. Docker Official Image buildpack-deps:trixie-curl Dockerfile (a vendor, external) — https://raw.githubusercontent.com/docker-library/buildpack-deps/master/debian/trixie/curl/Dockerfile
32. OCI Image Format Specification: image index (a standard, external) — https://raw.githubusercontent.com/opencontainers/image-spec/main/image-index.md
33. Filesystem Hierarchy Standard 3.0, 3.8 /home (a standard, external) — https://refspecs.linuxfoundation.org/FHS_3.0/fhs/ch03s08.html
34. mount(8), Linux man-pages (util-linux) (official docs, external) — https://man7.org/linux/man-pages/man8/mount.8.html
35. The first agent's plan for step 19, version 3 (kiln's records, internal) — plan:01a1cdf3cc7a70e4b3ca2ec610352383
36. Claude Agent SDK: file checkpointing (markdown) (official docs, external) — https://code.claude.com/docs/en/agent-sdk/file-checkpointing.md
37. Dockerfile reference (COPY; ADD --checksum) (official docs, external) — https://docs.docker.com/reference/dockerfile/

