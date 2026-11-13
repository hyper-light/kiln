# Plan step 18 [kiln]: what kiln assumes of the host, inside the VM (evidence)

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18; ROOT's goal 7c8235c884a68fd987874e76afb718c0.
This records what the step built, why, what ran where, and what is still owed. The step's own text:
each host assumption kiln makes must hold for a harness in a microVM, or be refused with its reason,
and no credential leaves its person's volume.

## Identity

- Role: implementer. The step's first agent (aa7d3e3dc8e4b5ab2, a subagent of ROOT's session g121)
  was stopped by an interruption with its work uncommitted; its successor took the work over as it
  stood and finished it. ROOT is kiln agent 01a148884a4770fdaf0111f3f792a248.
- Policy readback (2026-11-12): kiln origin/main 1401629e8, AGENTS.md 16b05ef80db0…,
  CLAUDE.md 336cc4fbf19b… (`@AGENTS.md`), docs/ci.md §0.3 f301e1d40876…; Shards origin/main
  e82b9403b, AGENTS.md f66d304c033a…, docs/development-rules.md df2acf47cf92…,
  docs/development-evidence.md 61613a244106…. Mail to ROOT was refused (429, backpressure); the
  receipt is beside ROOT's brief, `/tmp/g121-shot/s18-POLICY_READ.txt`, and goes again with the
  report.
- Branch `p/s18-host-in-vm-v1`, worktree `~/Projects/kiln/.claude/worktrees/s18-host-in-vm`.
- ROOT's goal #61 (7c8235c884a68fd987874e76afb718c0) then went to a goal agent, which planned it with
  kiln (plan 01a1d2bd5014705d918ad1f2ce1154f2, research 01a1d2bd5014705d944b487c232fc750) as two
  steps: part 1 landed as committed, then part 2. The Agent tool refused its children (the session's
  20-subagent cap), so it worked both steps itself, per ROOT's brief. Its readback is kiln origin/main
  e86ec22c6 (AGENTS.md, CLAUDE.md and docs/ci.md §0.3 as above) and Shards origin/main 1145bd3f2
  (AGENTS.md, development-rules.md and development-evidence.md as above).
- The person's rule of 2026-11-12 (subagents test their own change, merge it to main themselves
  and end) is the current word over R15's older "do not merge themselves".

## Each assumption, and where it holds

| kiln assumes, of a harness on its machine | In a microVM of its own | Built |
|---|---|---|
| Its project is at its path, so kiln's diff, HEAD and checkpoint reads work there | Shards plugs the project in at its host path (D235); kiln's Agentfiles mount no `/workspace` (part 2) | the plug's call is step 17's launch |
| Its config dir (CLAUDE_CONFIG_DIR, CODEX_HOME …) is on the machine, read for transcripts and resume | its person's volume, kept on the machine per person and harness, read there through its host view | `harness::isolated::home` |
| kiln's MCP and its plugin dir are on the machine | MCP at `/api/mcp` on the machine through the image's grant (D236); the plugin dirs plugged read-only at their host paths | `harness::isolated::mcp`; the plugs are step 17's |
| `KILN_*` are set for it | each one decided: the session's mark given, the machine's sockets left out | `harness::isolated::env` |
| git signing and credential helpers know it by its process | by the microVM's key (step 11) | design below |
| Its sign-in runs on its terminal | on the microVM's terminal lane (steps 6 and 11) | design below |
| The host's own tools (Xcode, MSVC) are there | they can't be: a project that builds with them is refused isolation, saying so | `harness::isolated::host_tools` |
| A harness gives up on its own when it can't sign in or can't reach its provider | kiln says so at the harness's first retry (H6) | `harness::trouble` |

### The per-person volume and kiln's reads of it (`harness::isolated::home`)

- **Where.** `<state>/homes/<person>/<harness>/`, the person `owner` or `user-<id>` (from
  `user:<id>`), the harness kiln's kind. Each of the image's volumes that holds what its run
  environment names (`HOME`, `CODEX_HOME`, `XDG_*` …) is a directory beneath it at the volume's own
  path (`/home/claude` from `…/claude/home/claude`). A person's directory and each harness's in it
  are made 0700 (Windows: the runner's state directory's ACL, which they inherit).
- **Whose.** The session's `Isolation::person`, which kiln sets from whoever started it (its
  parent's, for an isolated agent's agents) and never takes from the request (`api.rs` `as_asked`,
  `lineage.rs`). No other person's session, and no other harness, is given it.
- **kiln's reads.** `Layout::host_env` is the session's environment as the machine sees the
  microVM's: `HOME` and every directory its run environment names, at their paths in the volume,
  and every variable kiln's own reads take (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GEMINI_CLI_HOME`,
  `XDG_*`), so no read of an isolated session's falls back to the machine's own environment. What
  lies in no volume (the microVM's scratch) maps under `.scratch/`, which kiln never makes: a read
  there finds nothing, never the machine's own file. `homes::Dirs::microvm` takes Linux's places
  whatever the machine (not Windows' `APPDATA`, not macOS's `Library/Caches`); `Dirs::of`,
  `Roots::of` and a move's pack and install use it for an isolated session, and read nothing when
  it has no host view.
- **Refused, typed** (`isolated::Refusal`): a person kiln keeps nothing for (anything but `owner`
  and `user:<id>` of POSIX's portable filename characters); a harness kiln doesn't run; a volume no
  machine can keep a directory for (not absolute and plain, a name outside the portable set, a name
  Windows reserves, two that differ only by case); an image that keeps its harness's configuration
  in no volume.

### `KILN_*` (`harness::isolated::env`)

Every variable kiln gives a harness on its machine has its fate decided in `env::HOST`, and a test
reads `kinds.rs` and `reach/mcp.rs` for each `"KILN_…"` they set: one added there without a fate
fails it. `KILN_AGENT` (the session's mark) is given; `KILN_REACH_SOCK`, `KILN_HARNESS_SOCK` and
`KILN_RUNNER_SOCK` are left out, as sockets of the machine a microVM can't reach (kiln's MCP does
their work there); `KILN_REACH_ON` and `KILN_SESSION` are left out, as kiln's MCP knows the session
by its own token. Nothing of the machine's own environment goes.

### kiln's MCP (`harness::isolated::mcp`)

- The harness reaches `http://host.docker.internal:<port>/api/mcp`, the machine itself as Shards
  names it (D236), at the one port its image grants. An image that grants no such destination gets
  no server (`Refusal::McpNotGranted`, naming the destination it needs); a run can only keep or drop
  an image's destinations, never add one (D236's `Narrowing.destinations`).
- The token is the session's own MCP token (`crate::mcp::session_token`), in each harness's form:
  Claude Code's `--mcp-config` as JSON (an `http` server, its token in its headers); Codex's `-c`
  settings of a streamable-HTTP server, its token in `KILN_MCP_TOKEN` (`bearer_token_env_var`:
  Codex takes no token in its configuration, codex-rs `config/src/mcp_types.rs`); ACP's `http`
  server. Antigravity reads servers only from its own config file, in its person's volume, where a
  session's own token can't go; Pi takes none from kiln.
- A runner's kiln serves kiln's MCP over its own socket alone, which a microVM can't reach:
  `Refusal::McpUnserved` until a runner serves `/api/mcp` on its loopback (owed).

### The host's own tools (`harness::isolated::host_tools`)

A project whose tracked files hold an Xcode project or workspace (`*.xcodeproj/project.pbxproj`,
`*.xcworkspace/contents.xcworkspacedata`) or a Visual C++ project (`*.vcxproj`) is refused
isolation, `Refusal::HostTools`, naming each tool and the one system it runs on. `xcodebuild` runs
on macOS alone (Apple's Xcode system requirements) and MSVC on Windows alone (Microsoft, "Use the
Microsoft C++ toolset from the command line"). A `.sln` alone says nothing: `dotnet build` builds
one on Linux.

### Failures said as they happen (`harness::trouble`, the bring-up's H6)

The bring-up measured Claude Code retrying a refused credential ten times over 246.5 s before it
gave up, and Codex without a network waiting as long as it ran. A harness says each retry as it
makes it, so kiln reads the first and says then, in the harness's own words, that it can't sign in
to its provider or can't reach it: a notice in the timeline, and `State::trouble` (its kind, the
HTTP status where one came, its words, since when, how many times), until its model answers again.
kiln stops nothing on its own.

- Claude Code (stream-json): a `system` `api_retry` with `error_status` 401 or 403, or an error
  category `authentication_failed`, `oauth_org_not_allowed` or `cloud_credential_error`
  (`SDKAssistantMessageError`, code.claude.com/docs/en/agent-sdk/typescript), is a credential
  refused; `error_status: null` is no answer at all. An `assistant` message its CLI made itself
  (`model: "<synthetic>"`) with such a category is one too ("Not logged in · Please run /login").
- Codex (app-server, rust-v0.167.2): an `error` notification's or a failed turn's `codexErrorInfo`,
  `unauthorized` or a status 401/403 being a credential refused, a connection with no status, its
  "waiting for network" and an unresolved name being no reach.
- HTTP's statuses: 401 (no valid credentials) and 403 (credentials not let do it), RFC 9110
  §15.5.2 and §15.5.4. Overloaded, rate-limited and server errors are the provider answering: none.

The tests run the bring-up's recorded Claude Code streams byte for byte (Shards
`docs/research/measurements/harness-bringup-v1`, `records/claude-key-volume` and `records/claude`)
and Codex notifications made from its source for the same runs through the real adapters. In the
recorded 401 run, kiln speaks at the first retry, with 240,673 ms of the turn's 246,515 still ahead.

## For step 17's launch, as it lands

What the launch does with these, in order, for an isolated session:
1. `host_tools::check(host_tools::tracked(project))`; refused, the start is refused with its words.
2. `home::Layout::new(<state>/homes, person, harness, image's harness volumes, image's run.env)`,
   then `prepare()`; give the member each of `mounts()` (host directory at its guest path).
3. Plug the project read-write at its host path (D235), and kiln's plugin directory
   (`mcp::Kit::plugin_dir`, `mcp::plugin_dir()`) and the session's skills folder
   (`skills::place::session_folder`) read-only at theirs, so Claude Code's `--plugin-dir`
   arguments name the same paths inside. On a Windows host the guest path is D235's WSL form
   (`C:\X` at `/mnt/c/X`).
4. Set the session's `spec.env` to `Layout::host_env()` for kiln's own reads (transcripts, resume,
   memories, whose a file is). The member never gets it.
5. Give the member `env::member_env(session, kind, mcp env)`, its image's run.env applied by Shards
   under it, and kiln's MCP from `mcp::on_this_machine(session)` and `mcp::for_harness`, once
   `mcp::granted(image destinations, port)` holds.
6. A path the harness reports (a tool call's file, a file a message mentions) is the microVM's: the
   project's is the host's own by the plug; one in its home is `Layout::host_of`'s; one on its
   scratch is nowhere here. Reading `/tmp/shot.png` on the machine for an isolated session would
   read the machine's own file: every such read goes through `host_of`.

## Waiting on steps 6 and 11: design

- **Sign-in.** Each harness's own sign-in (`claude /login`, `codex login`, `agy`'s Google sign-in,
  `cursor-agent login` …) runs inside the microVM on its terminal lane (D238, step 6's exec and
  TTY), so what it writes (its credential file, its keyring file) lands in the person's volume and
  nowhere else. The browser half of a device or PKCE flow runs in the person's own browser: the
  harness prints the address, and the code or redirect comes back over the same lane. kiln runs
  nothing of a sign-in on the machine.
- **git signing and credential helpers.** Inside the microVM, `git` signs with a helper that asks
  kiln over the microVM's control lane (step 11), not a socket of the machine: kiln knows the
  session by the microVM's key, pinned at its start, never by a peer process's credentials, which
  a microVM doesn't have on the machine. The helper is the same `kiln-git-sign` contract
  (`gpg.format x509`), its transport the lane. A credential helper for git hosts follows the same
  path: the lane, then kiln's own credential, never a token written into the volume.
- **kiln's MCP token.** Until step 11, the session's own MCP token goes in as above, revoked as
  the session ends. With step 11, the microVM's key says which session asks, and no token goes in
  at all.

## What ran where (part 1)

| Where | What | Result |
|---|---|---|
| Linux x86_64, adacachy (CachyOS, rustc stable) | `cargo check -p kiln --lib --tests` | clean, no warnings in kiln |
| Linux x86_64, adacachy | `cargo test -p kiln --lib -- step18 wire::ts harness::homes harness::whose harness::moving harness::trouble harness::isolated mcp:: reach::mcp` | 89 passed, 2 failed: `wire::ts::bindings_are_current` (regenerated with `KILN_WRITE_TS=1`, then 7 of 7) and `harness::moving::tests::a_harness_whose_record_kiln_doesnt_keep_moves_with_its_conversation_in_words` (moving.rs:380's version check, failing on main too: ROOT's list of main failures without an owner) |
| Linux x86_64, adacachy | `cargo clippy --locked -p kiln -p kiln-ci --all-targets -- -D warnings` (the gate's clippy step) | passed (7 min 58 s) |
| macOS arm64, adalundhe-macbook-pro, at 28b96170e | `ci/gate.sh -p kiln -p kiln-ci --cpus 8 --mem 16g clippy` | `GATE: PASS`, 766 s, resident peak 11.0 GiB |
| Windows x86_64, jupiter, at 28b96170e | the same | not run to its end |
| Linux x86_64, adacachy, landing at 5fe18df48 (part 1 merged with main 819e7f13e) | `cargo test -p kiln --lib` with the filters of `light.sh` (step18, step23's real-shards check with `KILN_TEST_SHARDS` = shards e82b9403, agentfiles::, wire::ts, harness::{homes,whose,moving,trouble,isolated,lineage}, mcp::, reach::mcp) | 226 passed, among them every step18 test and `step23_a_real_shards_checks_every_builtin` (not skipped); 1 failed, main's own `harness::moving` version test: main's package `VERSION` is 4 and the test holds `!takes(4)` (`evidence/part1/landing-light.log`, `landing-light.out`) |
| Linux x86_64, adacachy, at 5fe18df48 | `cargo clippy --locked -p kiln -p kiln-ci --all-targets -- -D warnings` | passed, 5 min 45 s (`evidence/part1/landing-clippy.log`) |
| macOS arm64, adalundhe-macbook-pro, at 5fe18df48 | `ci/gate.sh -p kiln -p kiln-ci --cpus 8 --mem 16g clippy` | `GATE: PASS`, 1,248 s, resident peak 10.4 GiB |
| Windows x86_64, jupiter, at 5fe18df48 | the same | stopped on ROOT's word (2026-11-13): tests and clippy on one host, CI on main for the rest |

### Guard-removal mutants (R10)

Each mutant takes one guard away; the test named beside it must fail, at the assertion that guard
holds. Applied together in one build on adacachy (`mutate.py`'s twelve, each replacement matching
once), then M12 again alone, as its test's first assertion leans on the 401 retry M8 takes away.

| Mutant | Guard taken away | Test | Failed at |
|---|---|---|---|
| M1 | the isolated agent's person replaced by the caller's (`as_asked`) | `api::tests::step18_an_isolated_agents_person_is_whoever_asks` | the forged person kept (api.rs, `Some("owner")`) |
| M2 | a person's id held to the portable characters | `isolated::home::tests::step18_each_persons_and_harness_volume_is_its_own` | `person_dir(bad)` accepted `user:../owner` and the rest |
| M3 | an image keeping its configuration in no volume refused | `isolated::home::tests::step18_volumes_every_machine_can_keep_or_a_refusal` | `/workspace` alone not refused `NoConfigVolume` |
| M4 | an isolated session's dirs from its volume's view alone (`Dirs::of`) | `isolated::tests::step18_an_isolated_session_without_its_volumes_view_reads_nothing_here` | the machine's own home read |
| M5 | `KILN_HARNESS_SOCK`'s fate decided | `isolated::env::tests::step18_every_kiln_variable_a_harness_gets_is_decided_for_a_microvm` | "no fate in a microVM for KILN_HARNESS_SOCK" |
| M6 | kiln's MCP only through the image's grant | `isolated::mcp::tests::step18_mcp_goes_only_where_the_image_grants_it` | `host.docker.internal:7071` granted for 7070 |
| M7 | the host-tools refusal | `isolated::host_tools::tests::step18_a_session_that_needs_host_tools_is_refused_isolation_saying_why` | Xcode and MSVC projects not refused |
| M8 | Claude Code's 401 retry as a credential refused | `trouble::tests::step18_h6_claude_says_its_401_at_its_first_retry` | "not said at its first retry" |
| M9 | Codex's 401 status as a credential refused | `trouble::tests::step18_h6_codex_says_its_401_at_its_first_retry` | "not said at its first retry" |
| M10 | the scratch read as nowhere on the machine (`host_of`) | `isolated::home::tests::step18_a_harness_home_is_its_persons_volume_and_read_here` | `/tmp/x` read at the machine's `/tmp/x` |
| M11 | the person of an agent kiln starts: its asker's, else its parent's (`with_person`) | `lineage::tests::step18_an_isolated_agents_person_is_its_askers_else_its_parents` | the parent's person kept over the asker's |
| M12 | its model answering ends a trouble | `trouble::tests::step18_h6_an_answer_ends_it_and_another_kind_is_said_anew` | alone: `core.state.trouble.is_none()` (an answer left the trouble standing) |

Every targeted test failed (exit 101) with its own guard taken away, at the assertion beside it, and
passes on the tree as committed. Run with `scripts/mutate.py <kiln checkout>` then
`scripts/mutrun.sh` (`MUTANTS=` a list like `evidence/part1/mutants.txt`); the logs are
`evidence/part1/mutrun.out` (all twelve together, 567f429a5) and `evidence/part1/mutrun12.out`
(M12 alone). The first light run's log is `evidence/part1/test1.log`, Linux clippy's verdict
`evidence/part1/clippy.out`.

## R01–R16

| Rule | How it's met, or why it doesn't apply |
|---|---|
| R01 | No panics: every fallible step returns its refusal or `None`; the tests use `anyhow::Result` and `ensure!` as the workspace's lints ask. |
| R02 | Each decision rests on the sources cited beside it (Shards D231, D235, D236, D240; Claude Code's and Codex's own sources and docs; RFC 9110; POSIX; Microsoft's and Apple's docs). |
| R03 | No chosen numbers. Codex's MCP tool timeout in a microVM is `reach::mcp::codex_tool_timeout_sec()`, the one its machine's config takes too (main's `CODE_WALL_MAX` and main's own allowance), not a copy of it. The statuses are HTTP's. The test's 240,673 and 246,515 are the record's own. The built-ins' one port, kiln's MCP at `host.docker.internal:7070`, is kiln's own default `http_addr` (`config::Server`). An image must name its port as written (Shards carries NETWORK unexpanded, plan.rs; a run can drop a destination but never add one, D236), so `grants_tests` derives the destination it holds every image to from `config::Server::default()`, with no literal of its own: a changed default fails it until every image follows. At start the running kiln's own port (`mcp::Kit::for_microvm`) is checked against the image's grant (`isolated::mcp::granted`), and a kiln on another port is refused `McpNotGranted`, naming the destination it needs. Nothing is estimated, so there's no cold start. |
| R04 | Refusals are typed (`isolated::Refusal`, `trouble::TroubleKind`), matched by callers; words are presentation. |
| R05 | No new cache, wait or thread; one owner each (the layout, the session's state). |
| R06, R07, R13 | Small pure functions at existing seams; comments say why, not what. |
| R08, R14 | No performance claim: nothing on a hot path but one match per stream line. |
| R09 | H6 is the bring-up's own live record (Shards harness-bringup-v1); the tests replay it. |
| R10 | Each guard has a test that fails without it (part 1's mutation record below). |
| R11, R12 | Pure code for every platform; Linux's places in a microVM whatever the host (`Dirs::microvm`); Windows' ACL inheritance named. Clippy on Linux, macOS and Windows below. |
| R15 | Light tests only; real-VM runs are ROOT's (owed). |
| R16 | Builds in `~/g121-tmp/s18` on adacachy and `~/.cache/s18-gate` on the gates' runners, removed after. |

## Owed

- Wiring into step 17's launch seam (above), when ROOT says it has landed.
- Steps 6 and 11's terminal lane and the microVM's key: sign-in, git signing, credential helpers,
  the MCP token (design above).
- `/api/mcp` on a runner's loopback, so an isolated session on a runner's machine gets kiln's MCP.
  Plan step 35 (ROOT's goal #65) builds it. Agreed with that step's agent: it changes only
  `isolated::mcp::on_this_machine`, so that a runner serving its loopback MCP gives an `Endpoint`
  where `McpUnserved` stands today. Its port must be the one the built-ins grant, or their grant must
  follow it.
- Real-VM runs (ROOT's): an isolated session's transcript read from its volume and resumed; kiln's
  MCP reached through `host.docker.internal`; H6's notices from a live microVM.

## Part 2: the built-ins' grants and HOME (ROOT's routed findings)

- **Destinations by name** (Shards D236), every built-in: its harness's own hosts on TCP 443, and
  kiln's MCP at `host.docker.internal:7070` (kiln's default `http_addr`, config.rs) for the eight
  that take it; no `--dns`, no port grant. The rule: the hosts a harness reaches on its own to sign
  in, keep its sign-in fresh and work, as its vendor documents them, else as kiln saw it reach them,
  else as its own pinned build names them; none for a browser's page, an update (off in each
  image), telemetry, a registry, the editor's features, the web, or a provider whose host its
  account, region or person gives. A harness that takes any provider (Goose, opencode, Pi) gets
  each hosted provider its pinned build knows at a fixed public host, with their sign-ins, so the
  image holds what any configured provider needs; a run can drop destinations, never add them
  (D236), so narrowing to the provider in use is the launch's to do (owed with step 17).
  `crates/kiln/src/agentfiles/builtin/grants_tests.rs` holds every list, its sources beside it.
  It holds each built-in's hosts alone. kiln's MCP joins a list from `config::Server::default()`'s
  `http_addr` exactly where `isolated::mcp::for_harness` gives that harness kiln's MCP, so the test
  names no port of its own (R03 above). Its bent texts include kiln's MCP at a port not kiln's, and
  the boundary not open at kiln's port.
- **The project** isn't a volume: the launch plugs it in at its host path (D235). A project under
  `/tmp` can't be (D235's known limit).
- **HOME, one rule**: the person's volume, whole, `HOME=/home/<harness>` in run.env and one
  `VOLUME --target-kind=harness /home/<harness>`. Each harness's own variables keep pointing beneath
  it; caches that were on the scratch stay there (`XDG_CACHE_HOME=/tmp/.cache` for Codex, Devin,
  Amp, Goose and Pi). Why: every harness's documented state lives under HOME (Claude Code's
  `~/.claude.json` and agy's `~/.gemini` have no variable at all), a domain sees its image
  read-only (Shards D59, D240: HOME is the deepest volume holding it, else the scratch), and the
  programs a harness runs write their own files under HOME. kiln's layout keeps a volume's directory
  at its guest path beneath the person's base, so a Codex volume made at `/home/codex/.codex` sits
  inside the new `/home/codex` one unmoved.
- **Sources** (each cited in its Agentfile): code.claude.com/docs/en/network-config.md
  (`evidence/claude-network-config`, sha256 f8772ba8…); cursor.com/docs/enterprise/
  network-configuration.md (`evidence/cursor-network-config`, 300a70aa…); openai/codex rust-v0.167.2
  `codex-rs/cli/src/doctor.rs` (b3c964ba…), `login/src/server.rs` (2f66d1c0…),
  `login/src/auth/manager.rs` (584348a4…); google-gemini/gemini-cli v0.68.0 (`evidence/gemini-cli-v0.68.0`),
  @google/genai 1.30.0, google-auth-library 10.9.0's npm build; Goose 1.59.0's and pi 1.5.2's
  provider sources; and the pinned builds of Amp, agy, Devin, Cursor, opencode and pi, each
  downloaded at its pin, its SHA-256 matched, and read for the hosts it names
  (`scripts/hostscan.sh`, `evidence/part2-hostscan.out`).
- **docs/agents.md** lists each host assumption and each feature for an isolated session, working
  or refused with its reason: "What kiln's Agentfiles give it", "What an isolated agent can't do"
  and "Each feature, in a microVM of its own". The last now includes kiln's `KILN_*` variables:
  `KILN_AGENT` set, and the machine's sockets and `kiln reach mcp`'s left out (`isolated::env`).
  It also says how to reach a kiln on another port: your own Agentfile granting that port.
- **The plan's evidence** (`evidence/plan/`): ROOT's goal text, verbatim, and numbered excerpts of
  Shards' D235, D236 and D240 and of `plan.rs` at Shards origin/main 1145bd3f2, each headed by its
  whole file's SHA-256. kiln research 01a1d2bd5014705d944b487c232fc750 (plan
  01a1d2bd5014705d918ad1f2ce1154f2) cites them.
- **Owed (ROOT)**: each built-in run in a real microVM with these grants, a signed-in turn of each
  harness reaching its provider and nothing else (no vendor turn was run here).
