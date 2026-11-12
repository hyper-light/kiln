**Research:** Plan step 19 [kiln]: the Claude Code Agentfile is embedded in kiln: - pinned official Linux build; - HARNESS plus harness.json; - DNS and egress grants; - the project volume and a per-user config volume; - amd64 and arm64; - no secrets; - one smoke run prints its version. Rules as plan step 16's goal. Full step: kiln plan 01a1cd8e862e7169a074db3d5fa33f3d.

38 findings (A 30, B 5, C 2, F 1) from 62 sources (46 internal, 16 external)

- **A · 93 (high)** Step 19 asks for a Claude Code Agentfile embedded in kiln: the official Linux build pinned by version and digest, HARNESS with harness.json, DNS and egress grants, a project volume and a per-user config volume, layering over any base image, built for linux/amd64 and linux/arm64, prebuilt by kiln CI and served by digest with a local build as fallback, no credentials, and one smoke run in a microVM that prints the version. [1][54]
  _2 independent sources (kiln's records, logs); 5 of 5 quotes found where cited_
  - [1] "the harness's official Linux build, pinned by version and digest (find its source; if there is no Linux build, say so and stop);" (step 19, description) · quote found
  - [1] "It layers over any base image, so a project's own toolchain image works." (step 19, requirements) · quote found
  - [1] "It builds for linux/amd64 and linux/arm64, and kiln CI prebuilds it and serves it by digest, with a local build as fallback." (step 19, requirements) · quote found
  - [1] "One smoke run in a microVM prints the harness's version." (step 19, requirements) · quote found
  - [54] "the project volume and a per-user config volume; - amd64 and arm64; - no secrets; - one smoke run prints its version." (goal d4d312f663f88b09b218463a81241214) · quote found
- **A · 100 (high)** Step 19 works under plan step 16's rules: read kiln's AGENTS.md and CLAUDE.md (origin/main) plus Shards' policy and record the readback; work in its own kiln worktree from origin/main under ~/Projects/kiln/.claude/worktrees/; build only on Linux runners or the Mac Studio runner, never on this Mac; commit with gits. Since 2026-11-12 the person's rule is that subagents run their relevant unit and light integration tests, merge fast-forward only and end, and kiln-repo agents land as kiln's own rules allow, which need the person's word for every merge and push. [54][5][4]
  _3 independent sources (logs, a document, a measurement); 4 of 4 quotes found where cited_
  - [54] "Rules: read kiln's AGENTS.md and CLAUDE.md (origin/main) plus Shards' policy, and record the readback. Work in your own kiln worktree from origin/main under ~/Projects/kiln/.claude/worktrees/. Build only on Linux runners or the Mac Studio r…" (goal 6d9264239fcb8c48b20c5da1685cd36e) · quote found
  - [5] "kiln-repo agents land on kiln's main as kiln's own rules allow." (How to apply) · quote found
  - [5] "fast-forward only. Retry the merge if main moved. Never force." (How to apply, item 3) · quote found
  - [4] "is needed for every merge to main, every push and every deploy" (Hazards) · quote found
- **A · 93 (high)** Steps 20 to 28 are written as step 19's goal says, so step 19's layout (where built-in Agentfiles live in kiln, how each is pinned, how CI builds them and how kiln falls back) is the pattern for nine more harnesses; step 30 lists the built-ins read-only beside users' own Agentfiles. [54][1]
  _2 independent sources (logs, kiln's records); 2 of 2 quotes found where cited_
  - [54] "Plan step 20 [kiln]: the Codex Agentfile is embedded in kiln, as step 19's goal says. Rules as plan step 16's goal." (goal 306e318bf3d68d8ab9526b6946ecafa9) · quote found
  - [1] "Built-in (embedded) Agentfiles are listed read-only beside the user's own and the project's .kiln/images/<name>/Agentfile." (step 30, description) · quote found
- **A · 98 (high)** Anthropic publishes official Linux builds of Claude Code for x64 and arm64, each for glibc and musl (linux-x64, linux-arm64, linux-x64-musl, linux-arm64-musl), under downloads.claude.ai/claude-code-releases/<version>/<platform>/claude; its npm package installs the same native binary. [56][33][39][48]
  _3 independent sources (official docs, a vendor, logs); 5 of 5 quotes found where cited_
  - [56] "Supported npm install platforms are `darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`, `linux-x64-musl`, `linux-arm64-musl`, `win32-x64`, and `win32-arm64`." (Install with npm) · quote found
  - [33] "The npm package installs the same native binary as the standalone installer." (Install with npm) · quote found
  - [39] "DOWNLOAD_BASE_URL="https://downloads.claude.ai/claude-code-releases"" (download section) · quote found
  - [48] "PLATFORM 2.1.311 linux-arm64 checksum=2e2496c9a8065df93acb0fca2d341f59a6b8777649b2e025ab8c9bcd5eef61f3 size=293658808" (2.1.311 manifest) · quote found
  - [48] "PLATFORM 2.1.311 linux-x64 checksum=86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada size=293776752" (2.1.311 manifest) · quote found
- **A · 100 (high)** A release is pinned through its signed manifest: manifest.json lists every platform binary's SHA-256 and is signed by Anthropic's release key (primary fingerprint 31DD DE24 DDFA B679 F42D 7BD2 BAA9 29FF 1A7E CACE, published at downloads.claude.ai/keys/claude-code.asc), from 2.1.89 on; the manifests of 2.1.311 and 2.1.322 verified with VALIDSIG by that key, and each downloaded binary matched its listed SHA-256. [33][48][60][6]
  _4 independent sources (official docs, logs, a measurement); 5 of 5 quotes found where cited_
  - [33] "The manifest is signed with an Anthropic GPG key, so verifying the signature on the manifest transitively verifies every binary it lists." (Binary integrity and code signing) · quote found
  - [33] "Manifest signatures are available for releases from 2.1.89 onward." (Binary integrity and code signing) · quote found
  - [48] "[GNUPG:] VALIDSIG 31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE 2026-10-26 1793042338 0 4 0 1 10 00 31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE" (2.1.311 verify) · quote found
  - [60] "VERIFIED 2.1.311-linux-x64 sha256=86dc1fcc69498159eaf15f446613e847010f9036bfbbdffd4c49350be8489ada" (fetch check) · quote found
  - [6] "GnuPG `VALIDSIG` of `manifest.json.sig` by the documented primary key" (The publishers' binaries, Publisher's check) · quote found
- **A · 97 (high)** Anthropic's stable channel is about a week old and skips releases with major regressions; on 2026-11-12 stable was 2.1.311 (built 2026-10-26) and latest 2.1.322 (built 2026-11-10). [33][48]
  _2 independent sources (official docs, logs); 3 of 3 quotes found where cited_
  - [33] "use a version that is typically about one week old, skipping releases with major regressions" (Configure release channel) · quote found
  - [48] "CHANNEL stable=2.1.311 latest=2.1.322" (channels) · quote found
  - [48] "MANIFEST version=2.1.311 commit=e7d3b8365a0ffbe2f5dd032feb28e9fd9c7de025 buildDate=2026-10-26T19:03:46Z" (2.1.311 manifest) · quote found
- **A · 99 (high)** The glibc builds link only glibc's own libraries (librt, libc, the loader, libpthread, libdl, libm) at symbol versions no newer than GLIBC_2.17, on x86_64 and aarch64, in both 2.1.311 and 2.1.322, and 2.1.311's x86_64 build ran on a glibc host as is; Anthropic supports glibc distributions from Debian 10 and Ubuntu 20.04. [49][60][33]
  _3 independent sources (logs, official docs); 5 of 5 quotes found where cited_
  - [49] "NEEDED librt.so.1 libc.so.6 ld-linux-x86-64.so.2 libpthread.so.0 libdl.so.2 libm.so.6" (2.1.311 linux-x64) · quote found
  - [49] "VERNEED libc.so.6 max GLIBC_2.17 n 14" (2.1.311 linux-x64) · quote found
  - [49] "NEEDED librt.so.1 libc.so.6 ld-linux-aarch64.so.1 libpthread.so.0 libdl.so.2 libm.so.6" (2.1.311 linux-arm64) · quote found
  - [60] "VERSION_OUT 2.1.311: 2.1.311 (Claude Code)" (version) · quote found
  - [33] "Debian 10+" (System requirements) · quote found
- **A · 100 (high)** The musl builds need musl's loader (/lib/ld-musl-x86_64.so.1, /lib/ld-musl-aarch64.so.1), so on a glibc base they do not start (exit 127, No such file or directory); on Alpine and other musl distributions Anthropic requires libgcc, libstdc++ and ripgrep at run time with USE_BUILTIN_RIPGREP=0, and the bring-up's Alpine image added libgcc and libstdc++. [49][60][56][33][7]
  _4 independent sources (logs, official docs, a measurement); 8 of 8 quotes found where cited_
  - [49] "INTERP /lib/ld-musl-x86_64.so.1" (linux-x64-musl) · quote found
  - [49] "INTERP /lib/ld-musl-aarch64.so.1" (linux-arm64-musl) · quote found
  - [60] "MUSL_LOADER_ON_HOST ls: cannot access '/lib/ld-musl-x86_64.so.1': No such file or directory" (host check) · quote found
  - [60] "No such file or directory exit=127" (musl build on the glibc host) · quote found
  - [56] "Installing Claude Code on Alpine and other musl/uClibc-based distributions requires `bash` and `curl` for the install command, and `libgcc`, `libstdc++`, and `ripgrep` at runtime." (Alpine Linux and musl-based distributions) · quote found
  - [33] "Install these packages using your distribution's package manager, then set USE_BUILTIN_RIPGREP=0." (Alpine Linux and musl-based distributions) · quote found
  - [7] "RUN apk add --no-cache libgcc libstdc++" (Agentfile line 13) · quote found
  - [49] "NEEDED libc.musl-x86_64.so.1" (linux-x64-musl) · quote found
- **A · 97 (high)** claude --version prints the version followed by (Claude Code), for example 2.1.311 (Claude Code), and needs no credential. [38][56][60]
  _2 independent sources (official docs, logs); 4 of 4 quotes found where cited_
  - [38] "Output the version number" (CLI flags, --version) · quote found
  - [56] "To confirm which version installed, run `claude --version`: the command prints the exact version you passed, such as `2.1.89 (Claude Code)`." (Install a specific version) · quote found
  - [60] "VERSION_OUT 2.1.311: 2.1.311 (Claude Code)" (version) · quote found
  - [60] "VERSION_OUT 2.1.322: 2.1.322 (Claude Code)" (version) · quote found
- **A · 100 (high)** Every flag kiln's Claude adapter passes is accepted by 2.1.311 and 2.1.322: each is in --help except --resume-session-at, which the binary carries unlisted; kiln's adapter was written against Claude Code 2.1.296. [20][53][60]
  _3 independent sources (a measurement, logs); 6 of 6 quotes found where cited_
  - [20] ""--forward-subagent-text"," (args, lines 46-60) · quote found
  - [20] "a.extend(["--resume-session-at".to_owned(), at.to_owned()]);" (args, line 81) · quote found
  - [53] "//! Message shapes were taken from Claude Code 2.1.296" (claude.rs line 8) · quote found
  - [60] "FLAG 2.1.311 --forward-subagent-text help=1 binary_quoted=2" (flags) · quote found
  - [60] "FLAG 2.1.311 --resume-session-at help=0 binary_quoted=3" (flags) · quote found
  - [60] "FLAG 2.1.311 --permission-prompt-tool help=1 binary_quoted=4" (flags) · quote found
- **A · 100 (high)** kiln starts Claude Code as the program claude with the stream-json arguments claude::args builds (fixed: -p, --input-format stream-json, --output-format stream-json, --verbose, --include-partial-messages, --replay-user-messages, --forward-subagent-text, --permission-prompt-tool stdio; then per-session ones), adds --mcp-config and --plugin-dir, sets CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=1, drives Claude Code's speed, and takes the config dir from the session's CLAUDE_CONFIG_DIR, else ~/.claude. [19][20]
  _2 independent sources (a measurement); 6 of 6 quotes found where cited_
  - [19] "Kind { id: "claude", name: "Claude Code", program: "claude", verified: true, protocol: Protocol::ClaudeStreamJson" (line 48) · quote found
  - [19] "p.args(super::claude::args(spec, session_id.as_deref()));" (line 112) · quote found
  - [19] "p.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");" (line 138) · quote found
  - [19] ".with_speed(spec.speed.clone())" (line 85) · quote found
  - [19] ".find(|(k, _)| k == "CLAUDE_CONFIG_DIR")" (lines 482-491) · quote found
  - [20] ""--permission-prompt-tool"," (args) · quote found
- **A · 97 (high)** With CLAUDE_CONFIG_DIR set, everything Claude Code writes lands in that directory: .claude.json (account, MCP servers, per-project state), session transcripts under projects/, plugins, backups and sessions, while HOME stays empty; on Linux its sign-in credentials (.credentials.json) go there too. So one per-user volume at CLAUDE_CONFIG_DIR holds all of a user's Claude Code state, sign-in included. [60][57][35][36]
  _2 independent sources (logs, official docs); 6 of 6 quotes found where cited_
  - [60] "PROBE_IN_CONFIG_DIR_CLAUDE_JSON yes" (after mcp add --scope user) · quote found
  - [60] "FILES_IN_HOME_COUNT 0" (HOME after both runs) · quote found
  - [60] "./projects/-home-adalundhe--cache-s19-claude-research-bin-20261112T050222Z" (FILES_IN_CONFIG_DIR) · quote found
  - [57] "To keep the home-directory files somewhere else, set [`CLAUDE_CONFIG_DIR`](https://code.claude.com/docs/en/env-vars); Claude Code then stores your settings, session history, and plugins there instead." (Find or create your settings files) · quote found
  - [35] "If you've set the CLAUDE_CONFIG_DIR environment variable, Claude Code keeps the .credentials.json file under that directory instead" (Credential management) · quote found
  - [36] "It holds your account details, MCP server configurations, per-project state such as trust decisions" (Find or create your settings files) · quote found
- **A · 99 (high)** Without a credential Claude Code refuses locally and makes no API request (duration_api_ms 0, exit 1), so a run carrying no secret can be smoked safely; with a key its 401 retries take about 250 s. [6][60]
  _2 independent sources (a measurement, logs); 4 of 4 quotes found where cited_
  - [6] "locally (`Not logged in · Please run /login`, `duration_api_ms: 0`, no request), so its reach" (Findings, H6) · quote found
  - [6] "is shown with a placeholder key. With a key it retries a 401 ten times with backoff" (Findings, H6) · quote found
  - [60] "PRINT_EXIT 1" (-p with no credential) · quote found
  - [60] "PRINT_FIELDS {"duration_api_ms":0,"is_error":true,"terminal_reason":"api_error"}" (-p with no credential) · quote found
- **A · 100 (high)** harness.json's simple form (entrypoint, options) cannot set environment; its full OSI form can (run.command, run.env, run.workdir), and init appends run.env after PATH, HOME=/tmp and SHARDS_DOMAIN, joining run.workdir under the harness's own directory; until step 7 lands a member gets no image ENV and no run-time env. The bring-up gave Codex CODEX_HOME through the full form's run.env. [6][11][51][8][1]
  _5 independent sources (a measurement, logs, kiln's records); 8 of 8 quotes found where cited_
  - [6] "2. The simple `harness.json` cannot point a harness at its credential by environment" (What is missing, item 2) · quote found
  - [6] "its full OSI form can, and a `--settings`-style" (What is missing, item 2) · quote found
  - [11] "pub env: Vec<String>," (struct Run) · quote found
  - [51] "cstr("HOME=/tmp")?," (domains.rs line 493) · quote found
  - [51] "for e in run.get("env").and_then(Value::strings).unwrap_or_default() {" (domains.rs line 496) · quote found
  - [51] "let workdir = cstr(&format!("{dir}/{workdir}"))?;" (domains.rs line 522) · quote found
  - [8] ""env":["CODEX_HOME=/run/codex-home"]" (harness.json) · quote found
  - [1] "Today a member gets HOME=/tmp, PATH and SHARDS_DOMAIN only: no image ENV, no run-time env, and a read-only working directory." (step 7, description) · quote found
- **A · 100 (high)** A harness gets a volume only through VOLUME --target-kind=harness <path> FOR <harness>; at run time -v NAME:<path> or -v HOSTDIR:<path> stands in for it and is given as the Agentfile scopes it, the share that harness's alone; a volume the Agentfile itself names is one named volume kept from run to run. [9][6][51]
  _3 independent sources (a measurement, logs); 5 of 5 quotes found where cited_
  - [9] "`--target-kind=<agent|harness>` (added 2026-10-01): whether the names after `FOR`" (4.5 VOLUME) · quote found
  - [6] "A host directory given at run time to a volume declared for the harness alone" (Credentials table) · quote found
  - [51] "A run's own `-v` or `--mount` of a volume or a directory at one of them stands in for it," (architecture.md D109, The daemon) · quote found
  - [51] "- A volume the Agentfile names is that named volume, made where it is missing, kept from" (architecture.md D109, line 17916) · quote found
  - [51] "// A volume given one agent or harness alone, as a `VOLUME … FOR` scopes it, or the" (domains.rs line 1479) · quote found
- **B · 80 (good)** The per-user config volume belongs at a home-relative path mirroring Claude Code's own default (~/.claude): /home/claude/.claude with CLAUDE_CONFIG_DIR pointing at it, not under /var/lib, which FHS keeps for state users never edit to configure a package; kiln reads this volume on the host as the session's config dir (step 18). [57][46][1]
  _3 independent sources (official docs, a standard, kiln's records); 3 of 3 quotes found where cited; inferred from them_
  - [57] "To keep the home-directory files somewhere else, set [`CLAUDE_CONFIG_DIR`](https://code.claude.com/docs/en/env-vars); Claude Code then stores your settings, session history, and plugins there instead." (Find or create your settings files) · quote found
  - [46] "Users must never need to modify files in /var/lib to configure a package's operation" (5.8 /var/lib) · quote found
  - [1] "Config dir (CLAUDE_CONFIG_DIR, CODEX_HOME, …): on a kiln-managed per-user volume that kiln reads on the host for transcripts and resume." (step 18, description) · quote found
- **A · 98 (high)** kiln reads a session's diffs, HEAD and checkpoints at its host path, so a member must see the project at that same path (step 8, run-time plugging, not landed); until then the project reaches the harness only through a VOLUME the Agentfile declares at a fixed path, given its host directory with -v. T4 bound the folder at its own path for the run's command instead. [1][26]
  _2 independent sources (kiln's records, a measurement); 2 of 2 quotes found where cited_
  - [1] "so a member must see the project at that same absolute path, writable, with its writes landing on the host." (step 8, description) · quote found
  - [26] "is shared with it writable at its own path, so what the harness says of its files" (What an isolated agent is) · quote found
- **A · 100 (high)** HARNESS takes a git URL, an http(s) URL, a build-host path or an OCI reference; an http(s) source is downloaded with no checksum and unpacked, while a path is copied from the build context. A binary pinned by digest is therefore fetched and checked by kiln before the build and handed to it as a build-context path. [9][14]
  _2 independent sources (a measurement); 3 of 3 quotes found where cited_
  - [9] "| path on the build host | copies it, as `COPY` copies |" (4.8 HARNESS) · quote found
  - [14] "let st = self.graph.source(url, attrs, None, custom_name(shown.clone()));" (content_of, Http) · quote found
  - [14] "crate::agentfile::Source::Path(p) => {" (content_of, Path) · quote found
- **A · 100 (high)** A HARNESS (or AGENT) source expands build arguments where it is laid out, and TARGETARCH is set from each target platform; BuildKit defines it in the global scope, so a stage redeclares ARG TARGETARCH to use it. One Agentfile can thus take ./claude-${TARGETARCH}-<libc> for each platform of a multi-platform build. [13][14][40]
  _3 independent sources (a measurement, official docs); 5 of 5 quotes found where cited_
  - [13] "// Expanded where it is laid out too." (q19 test) · quote found
  - [13] "AGENT main FROM $SRC" (q19 test) · quote found
  - [14] "(b"TARGETARCH", target.architecture.clone())," (line 1633) · quote found
  - [40] "These arguments are defined in the global scope so are not automatically" (Automatic platform ARGs in the global scope) · quote found
  - [40] "- `TARGETARCH` - architecture component of TARGETPLATFORM" (Automatic platform ARGs in the global scope) · quote found
- **B · 79 (good)** An ARG declared before the first FROM can pick a stage (FROM base-${CLAUDE_LIBC}), and BuildKit builds only the stages the target depends on, so musl's package step runs only in a musl build and a glibc build runs no RUN at all; shards holds a Dockerfile's meaning byte for byte to BuildKit's. [40][42][16]
  _3 independent sources (official docs, a document); 3 of 3 quotes found where cited; inferred from them_
  - [40] "`FROM` instructions support variables that are declared by any `ARG`" (Understand how ARG and FROM interact) · quote found
  - [42] "only builds the stages that the target stage depends on." (Differences between legacy builder and BuildKit) · quote found
  - [16] "A Dockerfile's meaning is held byte for byte to BuildKit's by `tests/oracle.rs`" (Where things are, crates/agentfile) · quote found
- **A · 100 (high)** shards builds several platforms in one command (--platform linux/amd64,linux/arm64), running another CPU's RUN steps under a pinned qemu-user in the builder microVM at 4.6 to 5.5 times the cycles; a multi-platform build is an index of its platforms, and shards push sends the index with every platform's manifest, the OCI index being the manifest for one or more platforms. [10][51][45]
  _3 independent sources (a measurement, logs, a standard); 5 of 5 quotes found where cited_
  - [10] "build for both (`--platform linux/amd64,linux/arm64`, D87) runs each platform's RUN steps." (D179) · quote found
  - [10] "From the host's side, an emulated run costs 4.6 and 5.5 times the cycles of a native one" (D179, Measured (PM M241)) · quote found
  - [51] "`a_builds_platform_defaults_to_the_default_platforms` (an index of two platforms: the" (architecture.md line 11114) · quote found
  - [51] "- `shards push image` sends the index together with every platform's manifest." (architecture.md D97, line 19115) · quote found
  - [45] "The image index is a higher-level manifest which points to specific [image manifests](manifest.md), ideal for one or more platforms." (opening paragraph) · quote found
- **A · 100 (high)** Today a harness's provider reach is three directives, proven by the bring-up: NETWORK --dns --egress=443/tcp provider FOR claude, EXPOSE --harnesses=claude 443/tcp AS egress FOR provider, and CONNECT claude WITH claude ON provider (joined, no flow to any member). The egress names a port, not destinations (H2); step 9 adds grants of providers by name. [7][9][6][1]
  _4 independent sources (a measurement, kiln's records); 6 of 6 quotes found where cited_
  - [7] "NETWORK --dns --egress=443/tcp provider FOR claude" (Agentfile line 22) · quote found
  - [7] "EXPOSE --harnesses=claude 443/tcp AS egress FOR provider" (Agentfile line 23) · quote found
  - [7] "CONNECT claude WITH claude ON provider" (Agentfile line 24) · quote found
  - [9] "(`CONNECT a WITH a ON n`) attaches it to the networks and grants no flow." (4.7 CONNECT) · quote found
  - [6] "**H2: egress names ports, not destinations.** `--egress=443/tcp` with `EXPOSE … AS egress`" (Findings, H2) · quote found
  - [1] "its providers by name (api.anthropic.com:443, api.openai.com:443, …) where an Agentfile asks for names rather than ports." (step 9, description) · quote found
- **A · 97 (high)** Claude Code's documented network needs are HTTPS hosts (api.anthropic.com, claude.ai, platform.claude.com, downloads.claude.ai and others), which TCP 443 with DNS covers; its feature flags come from api.anthropic.com itself. Turning off non-essential traffic (or telemetry) also turns off feature-flag evaluation, which can make Remote Control and other flag-gated features unavailable, and stops the fast-mode availability check; DISABLE_UPDATES blocks every update path, is documented for distributing Claude Code through one's own channels, and is known to the 2.1.311 binary. So a pinned image sets DISABLE_UPDATES=1 and leaves telemetry and non-essential traffic as on the host. [37][59][58][34][56][33][60]
  _2 independent sources (official docs, logs); 8 of 8 quotes found where cited_
  - [37] "Claude Code requires access to the following URLs. Allowlist these in your proxy configuration and firewall rules, especially in containerized or restricted network environments." (Network access requirements) · quote found
  - [59] "| `api.anthropic.com` | Claude API requests, including the WebFetch [domain safety check](https://code.claude.com/docs/en/data-usage.md#webfetch-domain-safety-check), feature flag fetches, and telemetry event logging |" (Network access requirements table) · quote found
  - [58] "Setting `DISABLE_TELEMETRY` or `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` also disables feature-flag evaluation, which can make [Remote Control](https://code.claude.com/docs/en/remote-control.md#requirements) unavailable; `DISABLE_ERROR_REP…" (Telemetry services) · quote found
  - [34] "Also disables feature-flag fetching, which makes Remote Control and the other features that need feature-flag fetching unavailable." (CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) · quote found
  - [34] "availability checks such as the fast mode check" (CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC) · quote found
  - [56] "Use this when you distribute Claude Code through your own channels and need users to stay on the version you provide." (Disable auto-updates) · quote found
  - [33] "To block all update paths, including manual updates" (Disable auto-updates) · quote found
  - [60] "STRING 2.1.311 DISABLE_UPDATES count=8" (strings in the pinned binary) · quote found
- **A · 98 (high)** The bring-up started Claude Code 2.1.322 (musl, x86_64) as a Shards harness from HARNESS and harness.json alone on Alpine, read its stream-json, and kept every credential out of the image; it was verified on Linux x86_64 with KVM only, and its fetch.sh fetches x86_64 builds only. [6][1]
  _2 independent sources (a measurement, kiln's records); 4 of 4 quotes found where cited_
  - [6] "| `a_real_claude_code_starts_as_a_harness_and_refuses_without_a_login` (`claude`) | pass | 0.78 s, End | the example as published: Claude Code exec'd as declared, its local refusal |" (Runs table) · quote found
  - [6] "no credential enters an image." (opening paragraph) · quote found
  - [6] "Verified: Linux x86_64 on KVM only." (R01-R16, R11) · quote found
  - [1] "fetch.sh takes x86_64 only so far; arm64 is owed." (step 19, description) · quote found
- **A · 100 (high)** kiln already builds Agentfiles in CI: kiln/build-image@v1 runs shards build (with --platform when given) and shards push into kiln's own registry at <registry>/<repo>/images/<name>:<tag>, reports the pushed digest and platforms, and fails where shards is not on the runner's PATH; kiln's registry is OCI Distribution v1.1 and stores indexes, and kiln registers a built image only as its registry holds it, the platforms read from the stored manifest. No kiln CI workflow names shards yet; runners report shards --version as a tool fact. [21][22][53]
  _3 independent sources (a measurement, logs); 9 of 9 quotes found where cited_
  - [21] "/// the builder can), an Agentfile through `shards build` and `shards push` (shards' own, never" (build_image doc, line 3938) · quote found
  - [21] "let base = format!("{own}/{}/images/{name}", self.a.repo.to_ascii_lowercase());" (line 3964) · quote found
  - [21] "args.extend(["--platform".into(), platforms.join(",")]);" (line 3981) · quote found
  - [21] "is built by {prog}, which isn't on this runner" (line 3960) · quote found
  - [22] "//! kiln's own OCI registry (docs/ci.md W12-R01): the OCI Distribution Specification v1.1 at" (line 1) · quote found
  - [22] "const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";" (line 38) · quote found
  - [53] "digest one of its manifests there, the platforms read from that manifest (an index's" (ci/images.rs line 6) · quote found
  - [53] "("shards", "shards", &["--version"])," (runner/probe.rs line 84) · quote found
  - [53] "## kiln CI workflows that name shards: 0" (git grep of .kiln) · quote found
- **B · 80 (good)** shards' latest release, v0.7.0 (2026-11-05, Linux and macOS binaries with SHA256SUMS), predates harness.json startup (startup.rs, c3b7bde8b, 2026-11-09), so building this Agentfile needs a shards from main at or after the bring-up's merge 1da9132b7, which is in origin/main, until a release or step 16's vendored revision carries it. [51]
  _1 independent source (logs); 5 of 5 quotes found where cited_
  - [51] "startup.rs added: c3b7bde8b 2026-11-09 Start supplied agents and harnesses from their root startup descriptors" (git log) · quote found
  - [51] "startup.rs NOT in v0.7.0" (git merge-base) · quote found
  - [51] "1da9132b7 IN origin/main" (git merge-base) · quote found
  - [51] ""at":"2026-11-05T22:48:00Z","tag":"v0.7.0"" (gh release view) · quote found
  - [51] ""shards-linux-amd64"" (gh release view, assets) · quote found
- **A · 95 (high)** kiln's CI builds kiln for x86_64 musl with one step text shared by its release and image jobs (W28-R78), so a job can run that commit's kiln, and with it the commit's embedded Agentfiles, from the same step. [24]
  _1 independent source (a measurement); 2 of 2 quotes found where cited_
  - [24] "# One x86_64-unknown-linux-musl build per commit for release and the image (W28-R78): the" (image.yaml, x86_64 musl step) · quote found
  - [24] "run: KILN_COMMIT=$KILN_SHA nice -n 10 ci/release/build.sh x86_64-unknown-linux-musl" (image.yaml, x86_64 musl step) · quote found
- **A · 100 (high)** kiln's unmerged T4 work embedded one Agentfile per harness with include_str!, built it with Shards on first need under a tag carrying the recipe's digest, on node:24-bookworm-slim with Claude Code from npm at the host's version, and named its base by tag, which it recorded as a reproducibility gap. [28][26][27]
  _3 independent sources (a measurement); 5 of 5 quotes found where cited_
  - [28] ""claude" => Some(include_str!("agentfiles/claude.Agentfile"))," (isolated.rs line 67) · quote found
  - [26] "Its tag, `kiln-harness/<harness>:<version>-<recipe>`, carries a digest of the" (What an isolated agent is) · quote found
  - [27] "FROM node:24-bookworm-slim" (line 9) · quote found
  - [27] "RUN npm install -g "@anthropic-ai/claude-code@${VERSION}" && npm cache clean --force" (line 14) · quote found
  - [26] "**The images' reproducibility**: kiln's Agentfiles name their base by tag" (Open) · quote found
- **A · 98 (high)** For a default glibc base, buildpack-deps:trixie-scm (a Docker Official Image for both arches, index sha256:4f34e1d6...) already carries git, openssh-client and procps over trixie-curl's ca-certificates, curl and gnupg, about 143 MB compressed per arch, with no RUN needed; debian:trixie-slim is about 30 MB but would need an apt-get RUN, emulated for the other CPU and resolved at build time, to add git and certificates. [50][43][44]
  _2 independent sources (logs, code); 7 of 7 quotes found where cited_
  - [50] "== buildpack-deps:trixie-scm index digest: sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10" (index) · quote found
  - [50] "amd64 sha256:712f969c9d81a35f9f00cce701faae521891d42a20b592b61bd0e6c5328fad7c layers=3 compressed_bytes=142865592" (amd64) · quote found
  - [50] "arm64/v8 sha256:4256ac3dbf04b630b9082d15c426b1982ceed0b071b2abe76a40ad028d323636 layers=3 compressed_bytes=142449890" (arm64) · quote found
  - [50] "amd64 sha256:7378260c0a098d27ac206225db5e7d92b8c7c84ea7afccc86eeb513756bbd392 layers=1 compressed_bytes=29856132" (debian:trixie-slim amd64) · quote found
  - [43] "FROM buildpack-deps:trixie-curl" (Dockerfile) · quote found
  - [43] "procps is very common in build systems, and is a reasonably small package" (Dockerfile comment) · quote found
  - [44] "FROM debian:trixie" (Dockerfile) · quote found
- **A · 99 (high)** Linux arm64 microVMs run on Apple silicon Macs through shards (kiln's Mac Studio runners), and no arm64 Linux host here has KVM, so the arm64 build and smoke run on a Mac Studio runner and amd64's on a Linux runner with KVM. [23][16]
  _2 independent sources (a measurement, a document); 2 of 2 quotes found where cited_
  - [23] "silicon Macs Linux arm64 microVMs; QEMU emulation as a declared fact (W04-R13)." (W13-R12) · quote found
  - [16] "No arm64 Linux host here has KVM" (Commands and gotchas) · quote found
- **A · 100 (high)** Checks against credentials exist on both sides: shards' build checks include BuildKit's SecretsUsedInArgOrEnv; the bring-up kept every credential out of its images; kiln never prints or commits a secret; step 33 keeps credential values out of images entirely. [51][6][4][1]
  _4 independent sources (logs, a measurement, kiln's records); 4 of 4 quotes found where cited_
  - [51] "name: "SecretsUsedInArgOrEnv"," (lint.rs line 132) · quote found
  - [6] "no credential enters an image." (opening paragraph) · quote found
  - [4] "- **Never print or commit a secret,** kiln's admin token included." (Hazards) · quote found
  - [1] "Values never appear in images, telemetry or logs." (step 33, description) · quote found
- **A · 99 (high)** kiln's rules shape the code: async on hyper-rt with kiln's own HTTP (no tokio, reqwest), no panics, only what the task asks, the touched crate's tests and clippy before handing over and the full gate only when the person asks; kiln's HTTP client already streams a response body chunk by chunk for large downloads. [4][53]
  _2 independent sources (a measurement, logs); 4 of 4 quotes found where cited_
  - [4] "Async code runs on hyper-raft's `hyper-rt`; HTTP is kiln's own" (The rules) · quote found
  - [4] "Don't add tokio, hyper, h2, axum, reqwest, kube-rs or" (The rules) · quote found
  - [4] "what you touched first (its tests, clippy on its crate), then, when the person asks for the full run, your change's gate (above)" (Checks before handing over) · quote found
  - [53] "/// Stream the body chunk by chunk (`None` at the end), e.g. for large downloads." (http/client.rs line 347) · quote found
- **F · 29 (very low)** Inside the microVM, init covers every other domain's directory with an empty tmpfs in each domain's own mount namespace, while the run's own command is confined from the domains by D115 (its own PID namespace, IDs and capabilities); none of the sources read says whether that command can read a harness's directory, and no CLI command reads a member's output (H3). So the plan's version smoke tries /harness/claude/bin/claude --version as the run's command first and has a harness-domain smoke that writes its version into the project volume as the fallback. [51][52][6]
  _3 independent sources (logs, a measurement); 3 of 3 quotes found where cited; inferred from them; 1 counterclaim, the strongest strong_
  - [51] "// Every other domain, and /sys, hidden." (domains.rs line 2272) · quote found
  - [52] "**The run's own command is confined from the domains too** (D115)." (9.3, line 602) · quote found
  - [6] "**H3: no command reads a member's output.** It goes only to the owner endpoint" (Findings, H3) · quote found
  - Against: D115's title says a run's command reaches none of its image's agents and harnesses, which may cover their files, so the command-path smoke may be refused. [51] (weighs 80%)
- **A · 95 (high)** Shards' policy fixes the roles: scoped implementers do implementation and targeted light tests, the main coordinator owns audits, profiling, benchmarking and integration validation, and every production literal needs a documented source or derivation (R03). [3]
  _1 independent source (a measurement); 3 of 3 quotes found where cited_
  - [3] "Use scoped subagents for parallel implementation and targeted unit/light" (R15) · quote found
  - [3] "The main coordinator owns all audits, profiling, benchmarking, cross-lane" (R15) · quote found
  - [3] "Use online learned/data-derived values, or exceptionally well-researched invariants" (R03) · quote found
- **B · 80 (good)** Three official routes put Claude Code in a Linux image: the per-platform binary pinned through its signed manifest; Anthropic's signed apt, dnf and apk repositories (stable and latest channels); and the npm package (the same native binary, needing Node.js 22 or later), which T4 used. The pinned binary needs no package manager, no Node and, on a glibc base, no RUN step, and pins an exact digest; the repositories and npm each need a RUN with network at build time and a base with that package manager. [56][33][27][49]
  _3 independent sources (official docs, a measurement, logs); 4 of 4 quotes found where cited; inferred from them_
  - [56] "Claude Code publishes signed apt, dnf, and apk repositories." (Install with Linux package managers) · quote found
  - [33] "The npm package installs the same native binary as the standalone installer." (Install with npm) · quote found
  - [27] "RUN npm install -g "@anthropic-ai/claude-code@${VERSION}" && npm cache clean --force" (line 14) · quote found
  - [49] "NEEDED librt.so.1 libc.so.6 ld-linux-x86-64.so.2 libpthread.so.0 libdl.so.2 libm.so.6" (2.1.311 linux-x64) · quote found
- **C · 64 (moderate)** kiln keeps the image definitions its CI builds at .kiln/images/<name>/Agentfile (or Dockerfile), the UI's image builder proposes them there, and kiln/build-image@v1 takes .kiln/images/<name> as its context by default; so the built-in harness Agentfiles belong at .kiln/images/harness-<id>/, beside kiln's other images, rather than in a source directory of their own. [61]
  _1 independent source (logs); 2 of 2 quotes found where cited; inferred from them_
  - [61] "(`.kiln/images/<name>/Dockerfile` or `.kiln/images/<name>/Agentfile`) or are made in the UI" (docs/ci.md line 4414, W13-R11) · quote found
  - [61] "let context = get("context").unwrap_or_else(|| format!(".kiln/images/{name}"));" (runtime.rs line 3946) · quote found
- **C · 64 (moderate)** Step 30 already has kiln's one table of shipped Agentfiles, crates/kiln/src/agentfiles/builtin.rs on branch p/agentfile-store-v1 (not yet on main): Builtin {name, harness, text}, ALL empty, each harness's own step adding its Agentfile there with include_str!. A second table for harness files and pins would be a parallel owner, so the fetcher's pin and harness files are added to this struct. [61]
  _1 independent source (logs); 3 of 3 quotes found where cited; inferred from them_
  - [61] "//! one binary, listed read-only beside a person's own, and copied to make one theirs. None yet: each" (builtin.rs line 3) · quote found
  - [61] "pub const ALL: &[Builtin] = &[];" (builtin.rs line 18) · quote found
  - [61] "pub harness: &'static str," (builtin.rs line 13) · quote found
- **B · 79 (good)** kiln/build-image@v1 pushes each run's image under the same <repo>/images/<name>:<tag> (and :latest), so two legs of a harness x arch matrix would each push a one-platform image to one tag, the second replacing the first; one shards build of both platforms per harness pushes one index instead. [61][10]
  _2 independent sources (logs, a measurement); 3 of 3 quotes found where cited; inferred from them_
  - [61] "let tags = if tag == "latest" { vec![format!("{base}:latest")] } else { vec![format!("{base}:{tag}"), format!("{base}:latest")] };" (runtime.rs line 3965) · quote found
  - [61] "runs.extend(tags.iter().map(|t| vec!["push".to_owned(), t.clone()]));" (runtime.rs line 3987) · quote found
  - [10] "build for both (`--platform linux/amd64,linux/arm64`, D87) runs each platform's RUN steps." (D179) · quote found

**Sources**

1. Parent plan: step 19 and its neighbours (7, 8, 9, 18, 30, 33) (kiln's records, internal) — plan:01a1cd8e862e7169a074db3d5fa33f3d
2. Shards ROOT's goal stack: plan step 16's rules and steps 20-28's goals (kiln's records, internal) — agent:01a148884a4770fdaf0111f3f792a248
3. Shards binding rules R01-R16 (origin/main 04abb68d, sha256 df2acf47...) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/development-rules.md
4. kiln AGENTS.md (origin/main b2dfa3f2, sha256 16b05ef8...) (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:AGENTS.md
5. The person's rule of 2026-11-12: subagents merge their own work and end (a document, internal) — /Users/adalundhe/.claude/projects/-Users-adalundhe-Projects-shards/memory/feedback-subagents-merge-and-end.md
6. Harness bring-up v1 record (real Codex and Claude Code as Shards harnesses) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/research/measurements/harness-bringup-v1/README.md
7. docs/examples/harness-claude: the bring-up's Claude Code Agentfile and harness.json (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/examples/harness-claude/Agentfile origin/main:docs/examples/harness-claude/claude/harness.json
8. Bring-up record: Codex's full OSI harness.json with run.env (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/research/measurements/harness-bringup-v1/records/codex-login-volume/harness.json
9. AGENTFILE_ARCH.md: EXPOSE, VOLUME, NETWORK, CONNECT, HARNESS, isolation (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/architecture/AGENTFILE_ARCH.md
10. Shards architecture.md: D54, D97, D109, D115, D179 (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:docs/design/architecture.md
11. OSI artifact config schema (crates/image/src/osi.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:crates/image/src/osi.rs
12. shards-init's domain start (crates/init/src/domains.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:crates/init/src/domains.rs
13. shards build E2E test: an ARG expanded in a domain's source (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:crates/shards/tests/build.rs
14. Agentfile planner (crates/agentfile/src/plan.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:crates/agentfile/src/plan.rs
15. shards build checks (crates/agentfile/src/lint.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/shards/.claude/worktrees/g121 show origin/main:crates/agentfile/src/lint.rs
16. Shards CLAUDE.md (repository mechanics) (a document, internal) — /Users/adalundhe/Projects/shards/.claude/worktrees/g121/CLAUDE.md
17. When harness.json startup landed, against shards v0.7.0 (a measurement, internal) — git log origin/main --diff-filter=A -- crates/agentfile/src/startup.rs; git merge-base --is-ancestor c3b7bde8b v0.7.0
18. shards' latest release and its assets (a measurement, internal) — gh release view -R hyper-light/shards --json tagName,publishedAt,assets
19. kiln harness kinds and Claude Code's launch (crates/kiln/src/harness/kinds.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:crates/kiln/src/harness/kinds.rs
20. kiln's Claude Code adapter arguments (crates/kiln/src/harness/claude.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:crates/kiln/src/harness/claude.rs
21. kiln/build-image@v1: Agentfiles through shards build and shards push (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:crates/kiln/src/runner/runtime.rs
22. kiln's own OCI registry (crates/kiln/src/ci/registry/mod.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:crates/kiln/src/ci/registry/mod.rs
23. kiln docs/ci.md W13: shards executor, runner images, platforms (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:docs/ci.md
24. kiln .kiln/image.yaml: the memoized x86_64 musl build shared by jobs (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:.kiln/image.yaml
25. kiln runner probes (crates/kiln/src/runner/probe.rs) (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln grep -n shards origin/main -- .kiln crates/kiln/src/runner/probe.rs
26. kiln T4 isolation design (unmerged cp/t4-isolation) (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show remotes/adacachy/cp/t4-isolation:docs/isolation.md
27. T4's embedded Claude Code Agentfile (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show remotes/adacachy/cp/t4-isolation:crates/kiln/src/sandbox/agentfiles/claude.Agentfile
28. T4's isolated.rs: Agentfiles embedded with include_str! (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show remotes/adacachy/cp/t4-isolation:crates/kiln/src/sandbox/isolated.rs
29. Claude Code stable and latest manifests fetched and GPG-verified (2026-11-12T04:51Z) (a measurement, internal) — curl downloads.claude.ai/claude-code-releases/{stable,latest,<v>/manifest.json,<v>/manifest.json.sig}; gpg --verify with downloads.claude.ai/keys/claude-code.asc (~/.cache/s19-claude-research/manifest-20261112T045108Z)
30. ELF interpreter, NEEDED and GLIBC versions of the eight Linux builds (2.1.311, 2.1.322) (a measurement, internal) — python3 -I ~/.cache/s19-claude-research/scripts/elfdeps_remote.py https://downloads.claude.ai/claude-code-releases/<v>/<platform>/claude
31. Claude Code 2.1.311 and 2.1.322 run on Linux x86_64: version, flags, musl on glibc, CLAUDE_CONFIG_DIR (a measurement, internal) — kiln run on adacachy2: ~/.cache/s19-claude-research/bin-20261112T050222Z (fetch verified against the signed manifest; --version; --help; -p with no credential; mcp add --scope user)
32. Docker Hub: buildpack-deps:trixie-scm and debian:trixie-slim platforms and sizes (a measurement, internal) — registry-1.docker.io/v2/library/{buildpack-deps,debian}/manifests/{trixie-scm,trixie-slim} (~/.cache/s19-claude-research/registry-*)
33. Claude Code: Set up (system requirements, Alpine/musl, binary integrity, channels, updates) (official docs, external) — https://code.claude.com/docs/en/setup
34. Claude Code: environment variables (official docs, external) — https://code.claude.com/docs/en/env-vars
35. Claude Code: authentication, credential storage (official docs, external) — https://code.claude.com/docs/en/authentication
36. Claude Code: settings files (official docs, external) — https://code.claude.com/docs/en/settings
37. Claude Code: network access requirements (official docs, external) — https://code.claude.com/docs/en/network-config
38. Claude Code: CLI reference (official docs, external) — https://code.claude.com/docs/en/cli-reference
39. Anthropic's installer (claude.ai/install.sh redirects to bootstrap.sh) (a vendor, external) — https://downloads.claude.ai/claude-code-releases/bootstrap.sh
40. BuildKit Dockerfile reference: automatic platform ARGs (official docs, external) — https://raw.githubusercontent.com/moby/buildkit/master/frontend/dockerfile/docs/reference.md
41. Dockerfile reference: how ARG and FROM interact (official docs, external) — https://docs.docker.com/reference/dockerfile/#understand-how-arg-and-from-interact
42. Multi-stage builds: legacy builder and BuildKit (official docs, external) — https://docs.docker.com/build/building/multi-stage/#differences-between-legacy-builder-and-buildkit
43. Docker Official Image buildpack-deps:trixie-scm Dockerfile (code, internal) — https://raw.githubusercontent.com/docker-library/buildpack-deps/master/debian/trixie/scm/Dockerfile
44. Docker Official Image buildpack-deps:trixie-curl Dockerfile (code, internal) — https://raw.githubusercontent.com/docker-library/buildpack-deps/master/debian/trixie/curl/Dockerfile
45. OCI Image Format Specification: image index (a standard, external) — https://raw.githubusercontent.com/opencontainers/image-spec/main/image-index.md
46. Filesystem Hierarchy Standard 3.0, /var/lib (a standard, external) — https://refspecs.linuxfoundation.org/FHS_3.0/fhs/ch05s08.html
47. kiln's HTTP client: streaming a body by chunk (a measurement, internal) — git -C /Users/adalundhe/Projects/kiln show origin/main:crates/kiln/src/http/client.rs
48. Evidence: Claude Code channels and GPG-verified manifests (2026-11-12T06:27Z) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_manifest.txt
49. Evidence: ELF interpreter, NEEDED and GLIBC versions of the eight Linux builds (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_elf.txt
50. Evidence: Docker Hub base images, platforms and compressed sizes (2026-11-12T06:34Z) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_bases.txt
51. Evidence: Shards origin/main c4ae59ee excerpts (domains.rs, architecture.md, lint.rs, startup.rs vs v0.7.0, release) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_src_shards.txt
52. Evidence: AGENTFILE_ARCH.md excerpt at Shards origin/main c4ae59ee (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_src_af.txt
53. Evidence: kiln origin/main 1a5a5f00d excerpts (claude.rs, probe.rs, http/client.rs, ci/images.rs, workflows naming shards) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_src_kiln.txt
54. Evidence: Shards ROOT's goals for plan steps 16, 19 and 20 (kiln goals_get) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_goals.txt
55. Evidence: Claude Code 2.1.311 and 2.1.322 run on adacachy2 (Linux x86_64, glibc 2.44) (logs, internal) — /home/adalundhe/.cache/s19-claude-research/bin-20261112T050222Z/evidence.txt
56. Claude Code: Set up (markdown) (official docs, external) — https://code.claude.com/docs/en/setup.md
57. Claude Code: settings (markdown) (official docs, external) — https://code.claude.com/docs/en/settings.md
58. Claude Code: data usage, telemetry services (markdown) (official docs, external) — https://code.claude.com/docs/en/data-usage.md
59. Claude Code: network access requirements (markdown) (official docs, external) — https://code.claude.com/docs/en/network-config.md
60. Evidence: Claude Code 2.1.311 and 2.1.322 run on adacachy2 (Linux x86_64, glibc 2.44); byte-identical mirror of adacachy2's evidence.txt, sha256 f31c5579c4c574862b602752586b1fa9b774bcbf4133ccc9d3e2ff7f030b8de0 (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_bin_adacachy2.txt
61. Evidence: kiln's image-definition home, build-image@v1's context and tags (origin/main 1a5a5f00d), and step 30's built-ins table (p/agentfile-store-v1 03b2a9c77) (logs, internal) — /Users/adalundhe/Projects/kiln/.claude/worktrees/s19-claude-agentfile/docs/research/s19-claude-agentfile/evidence/e_layout.txt
62. Step 21 (Gemini CLI) planner's layout proposal to step 19, 2026-11-12 (kiln's records, internal) — agent:01a1cdf263f07272a8c55fd281bcdb31

