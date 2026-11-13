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
| kiln's MCP and its plugin dir are on the machine | MCP over the link kiln drives the harness by, as its own `mcp` lane (Shards D254), never a network grant; the plugin dirs plugged read-only at their host paths | owed with the lane (below); the plugs are step 17's |
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
their work there); `KILN_REACH_ON` and `KILN_SESSION` are left out, as kiln's MCP in a microVM knows
the session by the link it rides, the session's own. Nothing of the machine's own environment goes.

### kiln's MCP: over the agent-control link, never the network

Built first as `harness::isolated::mcp`: each harness given `http://host.docker.internal:<port>/api/mcp`
with the session's own token, through a destination its image granted (D236). ROOT withdrew that on
2026-11-13, as the person didn't take it. kiln's MCP from inside a microVM rides the link kiln drives
the harness by, as that link's own `mcp` lane (Shards D254, goal 58's), so default deny holds with
no grant at all. The module, its typed refusals (`McpNotGranted`, `McpUnserved`), `Kit::for_microvm`,
the MCP token in the member's environment and every built-in's `host.docker.internal:7070` grant
came out before landing. What the lane needs on kiln's side is this goal's next part (Owed, below).

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
5. Give the member `env::member_env(session, kind)`, its image's run.env applied by Shards under it.
   kiln's MCP comes over the run's `mcp` lane (D254), owed with it.
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
- **kiln's MCP** rides the run's own `mcp` lane (D254), whose link says which session asks: no
  token goes into the microVM for it.

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
| M6 | kiln's MCP only through the image's grant (withdrawn with that design, before landing) | `isolated::mcp::tests::step18_mcp_goes_only_where_the_image_grants_it` | `host.docker.internal:7071` granted for 7070 |
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
| R03 | No chosen numbers. The statuses are HTTP's. The test's 240,673 and 246,515 are the record's own. The built-ins' one port is 443, HTTPS's (RFC 9110 §4.2.2): no port of kiln's is in any image, as kiln's MCP is no network grant. The tests' bent images name kiln's own port from `config::Server::default()`, no literal of their own. Nothing is estimated, so there's no cold start. |
| R04 | Refusals are typed (`isolated::Refusal`, `trouble::TroubleKind`), matched by callers; words are presentation. |
| R05 | No new cache, wait or thread; one owner each (the layout, the session's state). |
| R06, R07, R13 | Small pure functions at existing seams; comments say why, not what. |
| R08, R14 | No performance claim: nothing on a hot path but one match per stream line. |
| R09 | H6 is the bring-up's own live record (Shards harness-bringup-v1); the tests replay it. |
| R10 | Each guard has a test that fails without it (part 1's mutation record below). |
| R11, R12 | Pure code for every platform; Linux's places in a microVM whatever the host (`Dirs::microvm`); Windows' ACL inheritance named. Clippy on Linux and macOS below; Windows' build, clippy and tests are CI's on main (ROOT's word of 2026-11-13). The withdrawn `isolated::mcp` called the Unix-only `reach::mcp`, which Windows couldn't build; with it gone, nothing of step 18 calls Unix-only code outside a `cfg(unix)` twin. |
| R15 | Light tests only; real-VM runs are ROOT's (owed). |
| R16 | Builds in `~/g121-tmp/s18` on adacachy and `~/.cache/s18-gate` on the gates' runners, removed after. |

## Owed

- Wiring into step 17's launch seam (above), when ROOT says it has landed.
- Steps 6 and 11's terminal lane and the microVM's key: sign-in, git signing, credential helpers
  (design above).
- kiln's MCP over the `mcp` lane (D254, ROOT's word of 2026-11-13), this goal's next part: each
  harness's MCP config pointed at the lane's endpoint inside the microVM, its form agreed with goal
  58 from each harness's documented transports; the runner bridging the lane to kiln's MCP handler
  in its own process, with no Unix socket, so Windows gets kiln's MCP too; and kiln's in-process
  Agentfile check refusing an MCP network grant. The runner's loopback `/api/mcp` (plan step 35's
  part 3) was the withdrawn design's.
- Real-VM runs (ROOT's): an isolated session's transcript read from its volume and resumed; kiln's
  MCP over the lane; H6's notices from a live microVM.

## Part 2: the built-ins' grants and HOME (ROOT's routed findings)

- **Destinations by name** (Shards D236), every built-in: its harness's own hosts on TCP 443, and
  nothing for kiln's MCP (above); no `--dns`, no port grant. The rule: the hosts a harness reaches on its own to sign
  in, keep its sign-in fresh and work, as its vendor documents them, else as kiln saw it reach them,
  else as its own pinned build names them; none for a browser's page, an update (off in each
  image), telemetry, a registry, the editor's features, the web, or a provider whose host its
  account, region or person gives. A harness that takes any provider (Goose, opencode, Pi) gets
  each hosted provider its pinned build knows at a fixed public host, with their sign-ins, so the
  image holds what any configured provider needs; a run can drop destinations, never add them
  (D236), so narrowing to the provider in use is the launch's to do (owed with step 17).
  `crates/kiln/src/agentfiles/builtin/grants_tests.rs` holds every list, its sources beside it.
  It holds each built-in's hosts alone. Its bent texts include kiln's MCP as a destination on the
  machine, and the boundary opened at kiln's port.
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
  It says kiln's MCP comes over the link kiln drives the harness by, never the network, and that no
  Agentfile grants anything for it.
- **The plan's evidence** (`evidence/plan/`): ROOT's goal text, verbatim, and numbered excerpts of
  Shards' D235, D236 and D240 and of `plan.rs` at Shards origin/main 1145bd3f2, each headed by its
  whole file's SHA-256. kiln research 01a1d2bd5014705d944b487c232fc750 (plan
  01a1d2bd5014705d918ad1f2ce1154f2) cites them.
- **Owed (ROOT)**: each built-in run in a real microVM with these grants, a signed-in turn of each
  harness reaching its provider and nothing else (no vendor turn was run here).

### No grant for kiln's MCP, read as Shards reads it (ROOT's routed findings, 2026-11-13)

Step 34's checks found that on Windows a harness could reach kiln's MCP only from a microVM, since
`harness::kinds::local_tools` (`kiln reach mcp` over the runner's socket) is Unix-only, and that
`isolated::mcp` didn't build there. It called `reach::mcp`, which only Unix builds have. ROOT first
decided on plain HTTP to the machine's loopback, then withdrew that within the hour: kiln's MCP
rides the agent-control link's `mcp` lane (D254). This landing therefore takes the network design
out (above) rather than porting it.

`grants_tests`' `step18_shards_grants_each_builtin_its_hosts_alone_and_nothing_for_kilns_mcp` reads
every built-in through Shards' own parser and `agentfile::grants` in kiln's process, the same
reading Shards' daemon holds a run to (D109). Each built-in's destinations are its harness's hosts
alone. None names the machine itself (`host.docker.internal`) or a remote MCP server, none opens a
port to any address, and none grants `--dns`. Bent images are caught too: one granting kiln's MCP
as a destination on the machine reads as an MCP grant, and ones granting kiln's port or TCP 443 to
any address, or any name, read as wider.

### What ran where (part 2, and both parts' landing)

One host, as ROOT's word of 2026-11-13 has it: the focused tests of the code touched, and clippy for
its crate, on Linux; CI on main runs the rest of the matrix.

| Where | What | Result |
|---|---|---|
| Linux x86_64, adacachy, at eaf76bee4 (part 2 on part 1's landing tree 5fe18df48) | `cargo test -p kiln --lib` with `light.sh`'s filters, `KILN_TEST_SHARDS` = shards e82b9403 (D236 and run.env's HOME) | 227 passed, among them `grants_tests::step18_every_builtin_names_its_hosts_and_keeps_its_home_on_its_volume` and `step23_a_real_shards_checks_every_builtin` (a real shards checking the ten new texts); 1 failed, main's own `harness::moving` version test (`evidence/part2/p2-light.log`) |
| the same | the grants guard's red and green (for the MCP grant withdrawn since): kiln's default `http_addr` moved to `0.0.0.0:7071` in `config.rs`, every image still granting 7070 | red: the grants test failed at harness-goose, its NETWORK line naming `host.docker.internal:7070` where `host.docker.internal:7071` was due (`evidence/part2/p2-mutant.log`); restored, green (`p2-green.log`). It ran before ROOT's word against mutation runs reached the step, and none ran after. |
| the same | `cargo clippy --locked -p kiln -p kiln-ci --all-targets -- -D warnings` | passed (`evidence/part2/p2-clippy.log`) |
| Linux x86_64, adacachy, at f1d7f28e8 (both parts, part 1's record, and kiln main fe9c17c7d, whose commits changed api.rs, harness/model.rs, moving.rs and session.rs) | the same light tests, then `cargo clippy --locked -p kiln --all-targets -- -D warnings` | 227 passed; 1 failed, main's own `harness::moving` version test, its line moved to 386 by main; clippy passed (`evidence/landing/f1d7f28e8-*`) |
| Linux x86_64, adacachy, at 51de8f6c0 (the same, and kiln main cd224213c: resume in place, step 35's ACP capabilities) | the same | didn't run: the test binary failed to link, because adacachy's stable toolchain was updated under the build and its std rlibs were gone (`evidence/landing/51de8f6c0-adacachy-link-failure.log`). Its clippy was stopped there, and the lane (`~/g121-tmp/s18`, 94 GB) removed, on ROOT's word that kiln's checks leave adacachy, whose disk is saturated. |

### Landing (2026-11-13): one host, macOS arm64

ROOT's word of 2026-11-13: one `cargo test -p kiln --lib` with step 18's filters and one clippy, on
one Mac, then a fast-forward. Windows, Linux and the rest of the matrix run in CI on main.

| Where | What | Result |
|---|---|---|
| macOS 26.4 arm64, adas-mac-studio.local-2 (Apple M3 Ultra, 32 CPUs), rustc 1.99.1, `CARGO_INCREMENTAL=0`, at e951b2968 (both parts, the MCP grant taken out, kiln main eb91cc8f8) | `cargo test --locked -p kiln --lib -- step18 step23 agentfiles:: wire::ts harness::homes harness::whose harness::moving harness::trouble harness::isolated harness::lineage mcp:: reach::mcp` | 228 passed, 0 failed. Main's own `harness::moving` version test passes now. |
| the same | `cargo clippy --locked -p kiln -p kiln-ci --all-targets -- -D warnings` | passed (3 min 29 s; a dependency's warnings only) |
| the same Mac, at 0c64ad0e8: the same, merged with kiln main 522f14c31 (step 17's launch seam and images, conflicts in api.rs, session.rs and docs/agents.md resolved for both sides, and main's two `Isolation` test literals given part 1's `person`) | the same test | 230 passed, 0 failed, among them main's new `s17_*` tests in the filters |
| the same | the same clippy | passed (3 min 4 s) |
| adalundhe-macbook-pro (at a5ce21085), then adas-mac-studio-3.local (at e951b2968) | the same | stopped before a verdict, on ROOT's word: both Macs were thrashing (swap near full) under other lanes' kiln builds |

## Part 3: kiln's MCP over the agent-control lane (Shards D254), first slice

The decisions, verbatim in `evidence/part3/`:
- ROOT, 2026-11-13: kiln's MCP from inside a microVM rides the agent-control link as its own `mcp`
  lane (D254), never a network grant. No Agentfile grants anything for it, and kiln's in-process
  Agentfile check refuses a grant that reaches kiln itself. Third parties' remote MCP servers stay
  grantable (ROOT's ruling (a), widened to all of kiln itself).
- Goal 58, D254's API (Shards main e1995061f): `shards_remote::control::Execs::mcp(member)` hands
  over each connection the member's harness makes to its bridge. The bridge is `/run/shards/mcp`
  inside the microVM (shards-init's own binary: no arguments, environment or token), and each
  connection carries newline-delimited JSON-RPC as `kiln reach mcp` does.
- Goal 65, each harness's documented MCP forms. agy reads servers only from its shared
  `mcp_config.json`, and a stdio server named there inherits agy's environment
  (`docs/research/s35-every-harness/agy-mcp`).

Built here:
- `harness::isolated::mcp` names the bridge as a stdio server in each harness's own form, with no
  token: Claude Code's `--mcp-config`, Codex's `-c mcp_servers.kiln.*` (its `tool_timeout_sec` as
  on its machine), ACP's `mcpServers`, and one fixed entry in agy's shared config (`agy_config`,
  keeping the person's other servers and a kiln entry they turned off). Pi is refused, typed.
- Codex's timeout and `CODE_WALL_MAX` now live in `reach` itself with one owner, as Windows builds
  `isolated::mcp` and reaches kiln's MCP only this way.
- `agentfiles::check::Kiln` covers what is kiln itself: the machine its runner is
  (`host.docker.internal`; loopback, link-local and unspecified literals, D124's host network; a
  network's declared gateway, D31's `host-gateway`) and kiln's server (the hosts its configured URLs,
  advertised and listening addresses and certificate names give).
  - Every save and lint refuses, typed (`ReachesKiln`) and at its line, a destination or a remote MCP
    server that is kiln itself, with ROOT's reason, "kiln's MCP rides the agent-control lane (D254)".
  - The check reads the text with Shards' own parser, after Shards passed it, and resolves no name.
  - A name that only resolves to the runner, or a literal of the runner's own interfaces, is the
    run's to see on its machine, as goal 58 notes: owed with the launch.

R03: no number of the rule's own. The address classes are RFCs' (loopback, RFC 3927's and RFC
4291's link-local) and the names are kiln's configuration. Codex's timeout is main's
`CODE_WALL_MAX` plus main's own allowance, moved, not copied.

| Where | What | Result |
|---|---|---|
| macOS 26.4 arm64, adas-mac-studio.local-2 (M3 Ultra), rustc 1.99.1, `CARGO_INCREMENTAL=0`, at 053e79233 (part 3 on parts 1 and 2) | `cargo test --locked -p kiln --lib -- step18 agentfiles:: harness::isolated mcp:: reach::mcp wire::ts` | 199 passed, 0 failed |
| the same | `cargo clippy --locked -p kiln -p kiln-ci --all-targets -- -D warnings` | 2 errors, `field_reassign_with_default` in two test helpers: fixed (943294258) |
| the same Mac, at 027d1aacf (parts 1, 2 and 3 merged with kiln main d1f5b7f69) | step 18's whole filter set (`step18 step23 agentfiles:: wire::ts harness::homes harness::whose harness::moving harness::trouble harness::isolated harness::lineage mcp:: reach::mcp`) | 239 passed, 0 failed |
| the same | the same clippy | passed (3 min 25 s) |

Owed, this goal's part 3b:
- The runner's end of the lane: kiln's MCP served in the runner's own process over each
  `McpConnection`, with no Unix socket, on every OS. `reach::mcp`, `reach::local`'s dispatch and
  `reach::told`'s teller are Unix-only today, behind the reach socket.
- Kiln's vendor/shards at or after e1995061f, with shards-remote linked (its owner's).
- The wiring at the launch (`sandbox::microvm()`, step 17's next slice).
- Real-VM MCP round trips on Linux, macOS and Windows hosts (ROOT's).
