# Isolated agents

An agent whose start says `isolation` (`spec.isolation: {kind: "microvm", image}`, docs/agents.md "From
an Agentfile, in a microVM of its own") runs its harness in **a microVM of its own**, a member of an
Agentfile's image, never as a process on its machine. This page is kiln plan
01a1cd8e862e7169a074db3d5fa33f3d's step 17: the launch seam for harnesses in microVMs. It carries on
CP-T4.1's isolated agents (track T4, branch `cp/t4-isolation`), whose seam, host implementation,
facts and refusals hold; its microVM path (`shards run vm -i`, the harness as the run's command, on
the machine's network) is replaced by an Agentfile image's members.

## How an isolated agent runs

- **Its image** is made from the Agentfile its start chose: one kiln ships by its name
  (`harness-claude`, for that harness alone), or one of a person's own Agentfiles by its id
  (`agentfile:<id>`); none chosen, kiln's own for its harness (`sandbox::isolated::source_of`). Anything
  else is refused, saying what it takes. A person's own is theirs alone: kiln's server reads its text
  from their Agentfiles as the agent starts, at its latest version, and refuses another person's, even
  to whoever reaches everyone's (`harness::lineage::kept_agentfile`; the text travels in the start's
  `Spec::agentfile`, which kiln sets and never takes from what's asked).
- **It boots pinned by its digest** (`NAME@sha256:…`, as Hex boots one: `sandbox::image`). The
  machine's carried `shards` builds it into the Shards home its microVMs boot from, for the machine's
  platform, tagged by the digest of everything its build takes (`NAME:<recipe>`, `agentfiles::context`):
  its Agentfile, and kiln's harness directory for its harness, files, pin and the build its own argument
  defaults name (`ARG CLAUDE_LIBC=gnu`). A person's own Agentfile is built beside kiln's harness
  directory, so a copy of kiln's finds it where kiln's does. One whose defaults name none of its
  harness's several builds reads none of kiln's directories, as no build given no argument could, and
  is built from its text alone: kiln keeps a person's Agentfile as its text, so whatever else its build
  takes comes from a source Shards fetches itself (a Git or http(s) URL, an image). That's how a
  stand-in with no vendor's binary is built: its HARNESS named as the harness it stands in for, from a
  Git URL at a commit, and no vendor's turn taken. Where the home's store holds an image under
  that tag, it boots without a build; else it's built, once, as the starts that ask for it at once wait
  for one build. A build Shards refuses is refused in Shards' own words; a build runs as long as it
  takes, and is ended, with all it started, when its start is given up.
- **Nothing in it is a picked number.** Its platform is the machine's own architecture, as OCI names
  it; the build of its harness is the one its Agentfile's own default names; its tag is its content's
  digest. It waits for no fixed time: a build ends by itself or with its start. What Shards says of a
  refused build is kept whole, as kiln keeps a program's output (`proc::output`); with
  `--progress=quiet`, that is Shards' error and its warnings. Its cold start is a machine's first
  isolated agent of each content: that start builds (its harness's files downloaded and checked
  against their pin, its base pulled), and the starts after it boot what it built. One whose image
  Shards' store collected builds it again.
- **Its harness is a member of the image**: the HARNESS its Agentfile declares by the harness's id, as
  each one kiln ships does. It runs by its image's command (`harness.json`'s `run.command`), which runs
  the harness as kiln's adapter drives it (`kinds::tests::s17_each_images_command_runs_its_harness_as_its_adapter_drives_it`
  holds each built-in's command to its adapter's base arguments). What the session adds (its model,
  its mode, the conversation it resumes) are the arguments past that command (`Kind::elsewhere`).
- **Its launch goes through the `Sandbox` seam** (docs/seams.md "Sandbox"): `Boundary::MicroVm`, its
  program the member, its folder the session's at its own path, its environment only the session's
  locale and time zone (POSIX's `LANG`, `LC_*`, `TZ`) and what kiln fixes for the harness, its grants
  its image's own (default deny) narrowed to what its session uses (below), and the microVM sized as
  its boundary sizes one (the person's sizes reach it through its placement, step 32).
- **What it reaches past its microVM** is its image's destinations, narrowed to its session's own
  provider's or sign-in's (`sandbox::narrow`, step 17 part 6; Shards D236: a run drops destinations,
  never adds one). Each of kiln's Agentfiles for a harness of several providers or sign-ins names
  every host its harness reaches for each (step 18 part 4, `agentfiles::builtin::destinations::BY_USE`,
  each held to its Agentfile by `grants_tests`), and a run keeps those of what its session uses alone,
  read as its harness reads it: its run's environment first, then the model its session names, then
  its configuration in its person's home on the machine (`harness::isolated::home`) or its project's
  own. Antigravity's by its sign-in (a Gemini API key, ADC at its location, an enterprise license's
  location, else a personal Google account), Goose's by `GOOSE_PROVIDER` (and TypeSafe's auto
  effort), opencode's by its `model`'s provider, with its catalog and ripgrep's hosts always, pi's by
  its `defaultProvider`, with `pi.dev` always.
  Where kiln can't tell what it uses (no person or no home kept on the machine, a file it can't read,
  a provider its image names no hosts for, nothing that says), the run keeps every destination its
  image names, and the session says so and why: its `runs.reach` (`{"as": "whole", "why": "no_home"}`,
  or with what it read, `{"as": "whole", "why": {"unread": {"file", "error"}}}`) and a notice in its
  conversation. That's the one fallback, never a silent widening. An image of one
  provider (Claude Code's, Codex's) keeps its own whole (`"why": "one_use"`), as does a person's own
  Agentfile (`"persons_own"`), whose grants are as its person wrote them; neither is a fallback.
  `runs.reach` of a narrowed run is `{"as": "narrowed", "uses": [...], "hosts": ["host:443", ...]}`.
- **Its sign-in is taken outside its microVM** (plan step 33, Shards' D242 step 8, Hex's C8), for
  Claude Code and Codex, whose CLIs Shards' injector takes sign-ins of: each launch grants its harness
  its person's sign-in (`secrets::signin::Provider::grant`, `sandbox::isolated`), named
  `<harness>/<person>` (`claude/owner`, `codex/user-<id>`) so kiln's broker reads its provider from it.
  Its token response is kept by the server's run-lease broker over kiln's credential store and its
  CLI given placeholders; its access token is filled in at its egress toward its provider's API alone
  (`api.anthropic.com`, `chatgpt.com`); its refreshes are the broker's. A later run is granted the
  placeholders its CLI's file in its person's volume holds (`.credentials.json`, `auth.json`), so they
  stand for the sign-in again. Every runner leases from that one broker: the server's own in its
  process, every other over its link (`secrets::lessor::Remote`), for its own isolated agents'
  people's sign-ins alone, of a server that names `secrets.leases` (docs/wire.md). What crosses the
  link is wiped once used, as Shards' injector wipes what it holds: each ask's and answer's JSON,
  made the size it comes to, and every copy kiln holds of it on its way (its frame in the net
  thread's outbox, the words a frame is read into and their tree); the transport's own buffers are
  hyper-transport's. A launch before its runner's server has said whether it answers (a session
  restored as its runner started) waits for that word, then is granted its sign-in as any launch is
  (`secrets::lessor::answering`). Where a runner's microVMs lease no credentials, or its server
  answers none, the harness signs in itself, as before, said in the runner's log. A harness switched
  into its running microVM (P6c.5) is granted its sign-in as a launch is, or the switch is refused,
  never left to sign in itself there: Hex's microVMs, which grant a sign-in only as a box is made,
  refuse it until their swap call carries one (`sandbox::Microvms::swap`). Other harnesses sign in
  themselves: Shards has no profile of their CLIs' sign-ins.
- **Its stdio are the member's streams.** The seam's run is the `Duplex` its session drives
  (`crate::proc::duplex_of_run`, `sandbox::held`): what the adapter writes reaches the member's stdin in
  order, the end of its stdin is the member's, what the member writes fills its stdout and stderr, and
  its end is the session's exit. Its stdout and stderr hold as many chunks unread as a process's pipes
  do (`proc::STDOUT_CHUNKS`, `STDERR_CHUNKS`, one bound for both): no number of its own, and the same
  backpressure on the member as on a process. The adapters are the ones host sessions use, unchanged
  but for the folder they name: Codex's requests and ACP's `session/new` and `session/load` name the
  project as its microVM sees it (`sandbox::isolated::Seen`, 2806f93b6: a Windows machine's `C:\src\app`
  at `/work/app`), which is the machine's own path for a Unix project and for every session on its host.
- **It's no process of the machine's**: its session has no pid (`Duplex::host_pid` is none), nothing
  of it is in the session's group of processes, and it is never handed across an in-place upgrade
  (it stops, and starts again in the next image, its conversation going on).
- **Its session says where it runs** (`GET /api/harness/sessions/:id`, the session's state): `runs`
  is `{"in": "host", "pid": N}` for a process of the machine's, and for a microVM's member `{"in":
  "microvm", "vm": <the microVM's own name, as its runtime gives it>, "image": "NAME@sha256:…",
  "host_pid": null, "control": {"controller": {"suite", "group"}, "endpoint": {"suite", "group"},
  "lanes": ["control", "stdout:0", "stderr:0", "stdin:0", "exec", "socket", "mcp:0" …]}}`.
  `host_pid` is read from its run's handle and always said, so a check reads it rather than an
  absence. `control` is the agent-control link its run is driven over into the microVM, as each end
  says its TLS 1.3 handshake negotiated (rustls's names, or the IANA code where rustls has none): the
  controller's on the machine and the endpoint's own report from inside (Shards' agent control, step
  11), and every lane its controller was granted on it, by Shards' names (`Linked::report`'s lanes,
  `Granted::name`), `mcp:<member>` among them for each member whose MCP bridge the run serves (D254).
  It's absent until that link is made (`Run::link`, awaited once, never polled). A Hex-held run's is
  Hex's controller's (`HexRun::link`, Hex's `Runtime::link`): Hex drives its members' signals and MCP
  lanes over it and takes their stdio from the image channel (`take_input`, `take_output`), asking its
  endpoint for no member stream (Shards' D252), so its lanes are `control`, `exec`, `log:run` and each
  `mcp:<member>`, none of its stdio.
- **kiln's record of what it started for it** (`starts`): each start of its harness, when (`at`),
  where (`runs`, as it was as it started) and how it ended (`ended`: when, and its exit code where it
  was seen), kept with the session and handed to kiln's next image with it. A harness that ran and
  ended between two looks at the machine's processes is still in it; a start taken over in an
  in-place upgrade is the same start. It grows one entry a start, as kiln's restart rule paces
  starts (`AGAIN_*`, `SOON_TRIES`): no number of its own.

What a session does to its harness maps onto the member:

| The session | On the host | In a microVM |
|---|---|---|
| start | a process, its pipes | the image's member, its streams |
| steer (a message, an answer, a mode) | the harness's protocol on stdin | the same bytes on the member's stdin |
| interrupt | the protocol's own (Claude's `interrupt` control request, Codex's `turn/interrupt`, ACP's `session/cancel`) | the same |
| resume | the harness's own arguments or protocol (`--resume`, `thread/resume`, `session/load`) | the same, past its image's command |
| stop | SIGTERM to its group, SIGKILL after its grace | `Run::signal(Terminate)`, TERM to its member alone over its run's agent-control endpoint (Hex's `Runtime::signal_member`, Shards' D238); its kill (the microVM's stop) after its grace |
| kill | SIGKILL | the microVM's stop |
| its CLI's own answers (its models, its version, an MCP server added or removed) | its CLI run on the host (`Effect::Run`) | never run on the host: its version is its image's pin where it boots kiln's image for its harness, anything else refused in words (`session::in_microvm_alone`; kiln goal 3cd82843, r734) |
| its kind's catalog for the start dialog (Devin's models) | asked of the Devin installed on the host (`acp::catalog::warm_models`) | not asked on a runner whose sessions may run in microVMs: unknown until a session, in its microVM, tells it (`acp::catalog::sessions_on_host`) |

## Refused, rather than done another way

- **A machine without microVMs** refuses an isolated agent before anything starts
  (`sandbox::NO_MICROVM`), never running it as a process; its runner says so in its facts
  (`isolation.why_not`, and `virt.microvm` false), where placement (step 32) reads it.
- **A project that would hand it the machine's own** (`sandbox::isolated::kept_out`; CP-T4.1's
  AC-T4.1.5, as T4's `cp/t4-isolation` held it, 6134fd63b): the home of the user kiln runs as or a
  folder holding it (where an agent started with no folder would work); a folder that is, holds or
  lies in `~/.ssh` or kiln's own state (the folder its microVMs are held beneath, `Microvms::state`:
  its runner's keys and records, its microVMs' store; its people's volumes, `Host::homes`; and the kiln
  server's data folder, `Host::server_data`: kiln.db, its secrets and tokens, the server's own
  `[server] data_dir` for its in-process runner and as the machine's kiln config names it for any
  other runner, which on a machine that runs both holds that runner's default state); or one holding another
  worktree of its repository, whose files are another agent's. A folder of kiln's not made yet (a
  person's volumes, a server's data before it starts) is compared as made ones are, through the
  folders it lies in (macOS's `/var` is `/private/var`). In a workspace of its
  own (a Slates volume kiln mounts beneath its state) it's the folder the workspace lies over that's
  judged, whose files the volume holds. Refused before anything starts, saying which; a project's own
  folder, or a worktree of its own, is shared.
- **A move to another machine** (`sandbox::isolated::NO_MOVE`): its microVM and what its harness keeps
  there are its machine's. Refused as it's asked, before anything of it stops, so its member goes on.
  A move to the machine's newer runner process (an in-place upgrade) isn't refused: there it starts
  again in a microVM of the newer process, its conversation going on.
- **A switch to another harness** where its machine's microVMs swap none in place (`NO_SWITCH`; Hex's,
  until its wire calls land, P6c.2): refused as it's asked, as a switch kiln can't make is. Where they swap
  one (`Holds::swaps`, `Run::swap`), the switch swaps its member for the other harness's in the same
  microVM (kiln plan 01a1dd2e…, P6c.5): `sandbox::isolated::ready_to_swap` pins the other's image first,
  then its running member is handed on whole and `isolated::launch` swaps it there, its conversation
  carried from kiln's timeline.
- **kiln's MCP** reaches it over its link's own MCP lane (Shards D254), never the network: its harness's
  config names its microVM's bridge, `/run/shards/mcp`, and its runner serves each connection to it in its
  own process (`sandbox::isolated::bridge`, over `reach::mcp::serve_lane`), knowing its session by the lane,
  with no token in the microVM. Hex carries the lane: its harness member asks its bridge (`ImageMember::mcp`,
  Shards' `MemberSpec::mcp`) and each connection is taken through Hex (`Runtime::mcp`). Where a machine's
  microVMs carry none (`Microvms::mcp`, `NO_LANES`: a sandbox held with no lane), its harness starts without
  kiln's MCP, said, and an ACP agent is offered no MCP server.
- **What its boundary doesn't hold** is refused by the boundary, typed (`Refusal::Unsupported`), and the
  session's start fails in its words.

## A runner's engine

kiln plan 01a1dd2e…, P2.3 and P2.4 (`harness::engine`). A runner's **engine** is how it runs new agent
sessions: `host`, each a process of its machine's with no microVM, or `microvm`, each in a Shards microVM of
its own through Hex (`sandbox::hexvm`).

- **Which one.** What its owner set on the server (`PUT /api/runners/:name/engine`, `runners.engine.set` over
  MCP and code mode; `runners.manage` on that runner), else `microvm` where it can hold microVMs now (the
  person's decision 3), else `host`, saying why. Whether it can is read in one place, `engine::capable`, from
  its isolation facts below, which carry Shards' own check of its host (`shards_api::host_check`).
  `microvm` set where it can't any more is held as `host`, saying why.
- **Refused.** `microvm` on a runner that can't hold microVMs, saying why (409).
- **Held where it's decided.** The server keeps what's set (`kv` `runner_engine/<runner>`) and writes the engine
  into the facts it has of the runner (`isolation.engine`; `virt.microvm` only on `microvm`), which placement
  and the runners API read. It sends it to the runner (`ServerMsg::RunnerEngine`, feature `runner.engine`),
  which keeps it for its restarts and writes it into its own facts, so its admission refuses a microVM ask
  while it runs sessions on its host, whoever owns the placement.
- **Each new session's boundary.** A start that asks nothing takes the engine of the runner it's placed on
  (a conversation taken up, `resume`, stays where its harness keeps it). One
  that asks `microvm`, names an Agentfile or has an isolated parent needs a `microvm` runner; one that asks
  `none` needs a `host` runner. Each is refused where nothing fits, the engine named. The server itself
  (`local`) runs sessions on its host. A `microvm` runner's Hex node must also list Hex's `shards-image`
  class (P2.2), as its runner said of it (`isolation.image_class`, below; `engine::node_images`): one whose
  node lists none is refused, named, in Hex's words, and one whose runner doesn't say (from before the
  class) is placed by its engine alone.
- **A switch** takes nothing away: the sessions of the engine it had finish there (`draining` in its view, each
  of them in `draining_sessions`, with the switch they drain for, `switch`: what was set before, what it was set
  to, and when), and new sessions take the new one. While they drain (ROOT's decision 8 of the design):
  `runners.engine.end_draining` ("End them now", `POST /api/runners/:name/engine/end`; destructive, so an
  agent's call waits for its person's click) ends each, after the agents it started; `runners.engine.cancel`
  (`POST /api/runners/:name/engine/cancel`) sets the engine back to what was set before the switch, so they're
  on its engine again, refused where it wasn't switched or nothing drains. Both need `runners.manage` on that
  runner. The switch is kept with the setting (`engine::Kept`), on the server alone: the runner is sent the
  setting. Each setting has a version (the one before's plus one, kept with it), and a
  start carries the version its boundary was decided under, taken before it waits to be placed
  (`Spec::engine_version`). The runner refuses one decided under another (`engine::stale`), naming its engine
  now, so a session placed just before a switch never starts on the engine the runner had. The push is ordered
  against starts on the runner's link (a barrier).
- **Its real-VM test** (`vm_harnesses` `engine::p2_3_…`) has the microVM session answer a turn in its microVM
  before the switch and another after it, as it drains, each waited for on its harness's liveness (step 35's
  waits), so a harness that never ran there fails at once in kiln's word on it. It couldn't pass on the Hex
  kiln vendored before (1969a423e, on Shards 9975d1f55): its image class gave a start share to the run's root alone,
  so a harness never started in its project (Shards D277: `Setup (entering its working directory): errno 2`), its
  run ended failed, and kiln said exit -1; the switch ended nothing, the session having ended before it. The Hex
  kiln vendors now (955e15c06, on Shards 04481e36b) gives a share its members (`hex_api::Mount`), and kiln maps
  `Share::members` in `sandbox::hexvm` (P1.4); its run on that Hex is owed.

## Live overrides

kiln plan 01a1dd2e…, P6b.4 (`harness::overrides`; docs/agents.md, "Live overrides"). A person publishes a
port into a running session's microVM, grants it egress, connects its members to networks and plugs in a
folder, each in place: the session's `Command::Override`, which its session takes into its record
(`State::overrides`), gives its microVM live through the seam's `Run::change` (`kiln_seams::sandbox::Change`:
`Publish`, `Unpublish`, `Grant`, `Revoke`, `Connect`, `Disconnect`, `Plug`, `Unplug`) and a folder to its
harness among its allowed ones. Each start of its harness gives them again: the new run is given each change once
it runs, and its harness its folders as it starts (`Spec::dirs`). Each launch declares what its microVM takes live
(Shards' `ImageSpec::plug_roots` and `egress_ceiling`, the seam's `Launch::plug_roots` and `egress_ceiling`): the
top-level directories of its project and its person's volumes and the session's volume root
(`sandbox::isolated::VOLUMES`, where a folder goes by default; none of its init's own, `/proc`, `/sys`, `/dev`,
`/tmp`, `/etc`, `/run`), and the most egress it may be granted live, `[agents] egress_ceiling` as kiln's server set
it at its start (`Spec::egress_ceiling`). A folder is plugged as Shards' `ImageGuest::plug` takes one (D271: a host
folder or a volume of its store, for every member or those named, strictly beneath a root, read-only or not), and
unplugged by where it is; one outside every root is refused until a member restarted can take it (D270); egress past
the ceiling is refused. Ports go as Shards' `ImageGuest::publish` and `unpublish` take them (D269): a guest port at a
host address and port, let go of by the address it's bound at. A microVM that takes no change live refuses it
in its own words, kept on the override (`took: refused`): Hex's microVMs do until its wire calls land (P6b.3,
`sandbox::hexvm::NO_LIVE`). A session on its machine's host refuses ports, egress and networks, naming its engine.

## An agent's volumes

kiln plan 01a1dd2e959772a1b17bc49026448e8a, P6.2 (`agentdefs::volumes`; Shards' D271, D281; Hex's P6.2). An agent
definition names what it keeps from one of its sessions to the next on a machine, its caches, toolchains and package
stores: each volume a `name` and the most `bytes` it may hold (`agentdefs.create`, `update`, `check`). Each session
started from it (`Ask::definition`: `agentdefs.run`, a UI agent a person's definition stands in for, a split's agent)
is given them, as the definition was at the version it started from. Its store's name is `kiln-agent-` and the
SHA-256 of its person, definition and name (`volumes::store_name`): a person's own, never another's. Volumes stay per
machine (the person, 2026-11-14): a session elsewhere has that machine's.

- **Given as its microVM starts.** Its launch carries them (the seam's `Launch::volumes`, `isolated::launch_of`):
  each by its store name, with its bound, at `/volumes/<name>`, to its harness, read-write. Hex puts them in its box's
  spec (`ShardsImage::volumes`) and plugs each as the run starts, before the boot answers and so before the session
  hands its harness a turn; Shards makes one where it isn't there, keeps it past the session, and holds its files to
  its bound (a write past it is refused ENOSPC). Its box is admitted with their bounds together as its disk
  (`disk_bytes`, `hexvm::disk_of`), which Hex draws each bound from, so each byte a volume may hold is charged once.
  Nothing is plugged after the start.
- **Refused, rather than started without them:** a session of such an agent on its machine's host, before it
  starts; a launch on microVMs that keep no volumes (`Holds::volumes`), before anything boots; a volume with no
  bound (from a server older than bounds); and a start Hex refuses for one of them, typed by its cause
  (`Refusal::Volume`): `Bound` where another session of the agent there holds it with another bound (it changes
  only once none does), `Clearing` while a clearing holds it. Its words say when to start it again; kiln doesn't
  start it again on its own.
- **Placed where its volumes fit.** Its placement asks a machine's disk for what its volumes may still grow by
  there: their bounds together (`Isolation::disk`) less what they hold already (`lineage::session_needs`). A
  machine's free disk counts a volume's files as used, so asking a volume's whole bound of it would count those
  bytes twice (ROOT's decision on Hex's P6.2 question, 2026-11-16; Hex keeps admitting each box's whole bounds,
  which never over-admits). What they hold is read from the machines it may go to as it's placed
  (`volumes::held_on`): on each, each volume's charge (what its files take by its bound's own measure, which its
  bound holds it to), at most its bound, together; the least of those over every machine it may go to, so none is
  asked less than its volumes may still take there. A machine that can't say (its kiln older, away, its store
  failing) holds none of them as far as placement knows. Each listing walks its machine's volumes' files, which a
  start of an agent with volumes waits on, at most as long as a person's command waits for its runner
  (`hub::COMMAND_WITHIN`); a start of an agent with none asks nothing.
- **Seen and cleared by its person.** `agentdefs.volumes` (`GET /api/agentdefs/{id}/volumes`) shows the caller's
  volumes of a definition of theirs, or of one of kiln's, on each machine that keeps any, for every name any of its
  versions declared: each one's bytes, its charge against its bound, and when it was made. The server asks each
  machine's runner (`ServerMsg::Volumes`, named feature `agents.volumes`) for those of the store names it asks
  alone, so no other person's volume leaves its machine; the runner reads its microVMs' store (`Sandbox::volumes`,
  Hex's `Runtime::volumes` through `shards_api::volumes`). `agentdefs.volumes.clear` removes one on a machine with its
  files (`Sandbox::clear_volume`), refused at once while a session holds it (Shards' holder locks: 409, its files
  kept) and where that machine has none of that name (404). Whose a volume is is its store name's: an admin who
  reaches everyone's definitions sees and clears their own volumes alone.

Owed: a switch of harness (P6c.5) giving the swapped-in harness its agent's volumes, unchecked on a real microVM (Hex
swaps none yet).

## Warm pools

kiln plan 01a1dd2e…, P3.2 (`sandbox::pools`, `kiln_ci::pools`). kiln names the warm pools a runner's microVMs keep,
and their node sizes them: how many hosts a pool keeps warm is Shards' demand rule's (D26, D253: the largest burst of
its starts within one refill window, within the node's room), never kiln's. A pool is a microVM: its image pinned by
its digest and its size (vCPUs and MiB, as the session's isolation gives them), as Hex keys Shards' pools (P3.1).

- **Kept from starts.** The first start of a microVM on a runner asks its microVMs to keep its pool (the seam's
  `Sandbox::keep`), once; each later start of it is its latest, and one whose microVM started on a warm host
  (Hex's `StartedFrom::Warm`) is a hit. A swap into the microVM a session runs in starts none, and counts as none.
- **Let go after its keep window.** Once no start of it came for its keep window, the runner lets it go
  (`Sandbox::let_go`), its warm hosts ended and their room given back; a later start begins a new pool. The window is
  the node's own keep-alive (Shards' `demand::KEEP`, 10 minutes, as AWS keeps an idle function: Shahrad et al.,
  "Serverless in the Wild", USENIX ATC 2020), so kiln names a pool for as long as its node keeps an idle pool's hosts.
  One waiter on the runner sleeps until the earliest window's end while any pool is named, and none otherwise.
- **Refused, held.** A microVM runtime that keeps no pools refuses one in its own words: Hex's microVMs do until its
  image class takes Shards' `ImagePools` (`sandbox::hexvm::NO_POOLS`, P3.1). The refusal is said once a pool, and
  asked again only as a new pool of it begins; a pool never kept is never let go.
- **Shown on the runners page.** A runner's details on the Runners page list its warm pools under "Warm pools", from
  `GET /api/runners/:name/pools` (docs/api.md): each pool the same rule names over the starts the server keeps, with
  its agent, its deployment experiment's variant (P7.1: a variant booting another image or size is a pool of its own
  on the node, sized by its own starts), its size, its starts and hits, and when it's let go; one whose window has
  passed leaves the list then. Its agent is named as the person looking may know it, as agent definitions are: the
  definition its sessions started from where they reach it (their own, everyone's for the server's owner and whoever
  has `agentdefs.manage`, and kiln's own), the harness of sessions started from none, and nothing of another person's
  agent, whose pool still shows. Its node's warm hosts aren't reported to the server yet (`warm_hosts` null).

## Facts

A runner says whether it holds isolated agents (`RunnerFacts.isolation`: `available`, `hypervisor`,
`why_not`), and `virt.microvm` is that same `available`, one answer. One holding microVMs with the carried
Shards says what Shards' own check of its host says at each probe (`sandbox::facts_of`; Shards' D262,
`shards_api::host_check`: the check its VM process makes before it boots, made without booting one, by one
launch of the carried `shards`): `available` where the host runs them, its backend the `hypervisor`; else
`why_not` in Shards' words (`/dev/kvm` missing or not this user's to open, a platform Shards has no backend
for, a VM given no nested virtualization), or that Shards couldn't say, and why (`sandbox::UNCHECKED`).
`harness::lineage::isolation_on` refuses an isolated start on a runner that says no, in the runner's own words.
One holding them through Hex also says whether its node lists the class its image VMs run as (`image_class`:
`{"is": "listed"}`, or `{"is": "unlisted", "why": ...}` in Hex's words), read from its node at each probe:
`hex_node::images::ImageRuntime`'s classes, and where it lists none, its image class's refusal (Shards' check
of the host, asked once as the node starts, or an image boot unconfigured; Hex's `ImageClass::serves`).

## What the microVMs are, and what's owed

The machine's microVMs are a `Sandbox` implementation of their own (`sandbox::microvm`): Shards' image
boot (its D231) of the member, through Hex, its stdio carried on Shards' own member streams
(`shards-api`'s image channel with member stdin, version 6, Shards e7825cb3) until the agent-control
endpoint (step 11) carries them over hyper-transport. A runner whose kiln carries Shards (`shards::carried`)
holds them as it starts (`sandbox::hold`): the carried `shards` written out beneath kiln's state, Hex's image runtime
over it (`hex_node::images::ImageRuntime`, its records in `<state>/hex-records`, Shards' home and cache beneath the
state), and `sandbox::hexvm::HexVms` as the runner's sandbox; its facts then say `virt.microvm` where Shards' check
says its host runs them (Facts, above). Each launch's box is
the one placement holds for its session (`Holder::box_of`), else the next of placement's own numbering of boxes that
reserve nothing (`kiln-z…`, shared with its holding), so no two share a name. Its charge is kept in placement's records as it
ends (`hexvm::USE`), and placement reads its use from its VM's meter once it's launched (`Group::Hex`). A launch adds kiln's variables
(`isolated::env::member_env`) and kiln's MCP through the link's bridge (`isolated::mcp`): its harness member asked for its
bridge (`ImageMember::mcp`), each connection taken through Hex (`Runtime::mcp`) for the runner's `isolated::bridge`; and refuses a project built
with its host's own tools (`isolated::host_tools`). A kiln that carries no Shards holds no isolated agent, and the
launch is held by the conformance suite and a stand-in alone (below).

Built, each where it is:

- **The Shards implementation** of the seam (step 17): the image built and pinned as above, booted through
  Hex's image class with the member's stdin, stdout and stderr (`sandbox::hexvm`).
- **A member's run-time arguments** (Shards' D256, `MemberSpec::args`; Hex's `ImageMember::args`): its
  command line is its image's command, then the session's flags (`--resume`, `--model`).
- **The project at its host path as the harness starts**: its folder shared at its own path as its box is
  made (Hex's `ShardsImage::shares`), and its working directory there. Each start share is given to its harness
  member as it starts (Shards' D277, Hex's `Mount::members`, `hexvm::mount_of`), so its harness works in its
  project from its first instruction; one given to a member a launch doesn't drive is refused, typed.
- **A member's signals** (Shards' D238, Hex's `Runtime::signal_member`): interrupt and stop reach the member
  alone on its own lane of its run's agent-control endpoint; its kill is its box's delete.
- **kiln's MCP from inside** (Shards' D254, Hex's `ImageMember::mcp` and `Runtime::mcp`): above.
- **The microVM's group, charged** (step 32): placed and charged through the balancer (`hexvm::USE`,
  `reach::place::Group::Hex`). As its run ends, it's charged what its VM cost its host over the run, which Hex
  answers as the box is deleted: read just before the delete stops a run that still runs, or, where the run
  ended on its own first, what its VM told of itself as it went down, which Hex keeps with that end until the
  delete (Shards' D260). Where Hex has none of it (a VM whose process was killed tells none), the box goes by a
  plain delete and is charged the use last read while it ran.
- **Its configuration on a volume kiln reads on the host** (step 18): its shares are its project at its own
  path, then each of its person's volumes for its harness at its path in the microVM, as kiln's Agentfile or
  the person's own lays them out, made before it starts; and beside them kiln notes where its harness keeps
  its conversations, where a history finds them.

Owed, each with whose it is:

- **The real launches** (ROOT's runs, `crates/kiln/tests/vm_e2e/README.md`): `vm_harnesses`' `step35::rest`,
  `step35::mcp` and `narrowed` in microVMs on macOS (HVF), Linux (KVM) and Windows (WHP), and a real
  `shards build` of each image kiln ships on each; their results recorded in `vm_e2e/gaps.json`.
- **Limits Hex's boxes don't hold** (`hexvm::HOLDS`): memory and process limits (Hex reads no OOM kill or
  refused fork of an image VM's guest), and weights; each refused `Unsupported` as a launch asks it (Hex's).
- **The member's stdio over hyper-transport** (step 11's stdio lanes): Shards' (an orchestrator's run
  serving its members' streams on its endpoint), then Hex's (`member_image` taking them through
  `shards_api::control::Controller`); kiln's `HexRun` takes Hex's `Proc::streams` as it is.
- **What T4 held that isn't carried yet**: its repository's git folder shared read-only over its project
  (kiln's own git runs in the project on the machine, `harness::gitstate`, reading the configuration and
  hooks its microVM can write), owed until a real VM shows Shards mounting a read-only start share
  beneath a writable one, in order (no Shards test holds it). T4's refusal of kiln's state and its
  header's "in a microVM" (5e66bbeaf: the agent page's eyebrow, its image on hover) are carried.

### Part 7's clauses, where they're met

Step 17 part 7 (#73): sessions launch through Hex's member call (`Call::Member`) and image class,
their `Duplex` the member's stdio; interrupt, stop and kill mapped to the member's signals and Hex's
stop. Each clause, as kiln main has it (paths under `crates/kiln/src`):

| Clause | Where | Held by |
|---|---|---|
| Launch through the image class | `sandbox/hexvm.rs` (`HexVms::launch_as`): the box's spec is `ClassSpec::ShardsImage`, its image pinned, its harness an `ImageMember` given the launch's environment, folder and arguments | `hexvm::tests::s17_p7_a_members_run_echoes_its_stdin_ends_and_its_box_goes` |
| Launch through the member call | `sandbox/hexvm.rs` (`boot`): create, start, then `Runtime::member`, the call Hex's node serves for `Call::Member` (`hex-client` `calls.rs:2998`, `hex-node` `serve.rs:2105`); kiln holds Hex's image runtime in process (`sandbox/mod.rs:71-76`, `ImageRuntime::new`) | the same test, over Hex's `SimRuntime` |
| Its `Duplex` the member's stdio | `sandbox/hexvm.rs` (`HexRun::start`: the member's `Proc::streams`: stdin, stdout, stderr, all three or refused); `sandbox/isolated.rs:115,168` → `proc.rs:39` (`duplex_of_run`) → `sandbox/held.rs:43-62` (`pump`: the `Duplex`'s pipes the run's) → `os/proc_unix.rs:436`, `os/proc_windows.rs:520` (`Duplex::held`) | `hexvm::tests::s17_p7_a_members_run_…` (stdin in order, back on stdout); `held::tests::s17_a_held_run_takes_its_input_in_order_and_ends_at_its_end`, `s17_a_held_duplex_is_no_process_…` |
| Interrupt → the member's signal | Every harness interrupts in its own protocol on the member's stdin, as on the host (`harness/claude.rs`, `codex.rs`, `acp.rs`, `pi.rs`, `antigravity.rs`). The seam's `Signal::Interrupt` is INT to the harness member alone (`sandbox/hexvm.rs`, `HexRun::signal`: Hex's `Runtime::signal_member`, on the member's own lane of its run's agent-control endpoint, Shards' D238; Hex's `guest/image.rs`, `signal_member_image`); `HOLDS.signals` is true | `hexvm::tests::s17_p7_interrupt_and_stop_reach_the_member_and_its_box_goes`; `harness::host::tests::s17_` (the control request at its member) |
| Stop → the member's signal, then Hex's stop | `harness/session.rs:4953,5029,5115,5120,5135` call `Duplex::terminate` → `os/proc_unix.rs:417`, `os/proc_windows.rs:511` → `sandbox/held.rs:110-124` (`Held::terminate`): `Signal::Terminate`, TERM to the harness member alone (Hex's `Runtime::signal_member`, as above; the end of its input only where a boundary holds no signals), then `Run::kill` once its grace passes | `held::tests::s17_terminate_signals_where_it_can_else_ends_its_input_then_kills`; `hexvm::tests::s17_p7_interrupt_and_stop_…` |
| Kill → Hex's stop | `sandbox/hexvm.rs` (`HexRun`'s `Run::kill` and its drop) → `finish`: Hex's `delete_with_host_use`, which stops the box's VM and removes it (Hex's `guest/image.rs`, `delete_image_used`, its final use read "just before its delete's stop"; its `MemberProc`: "the box's stop (`signal(KILL)`, `delete`)"); its end `Exit::Killed`, said once its box is gone, its charge kept first | `hexvm::tests::s17_p7_a_kill_and_a_drop_delete_the_box`, `s17_p7_s3_the_final_use_is_kept_before_its_end_is_said`, `s17_p7_s3_a_retried_delete_finding_its_box_gone_…` |

A held run is never stopped by a process id: its `Duplex` has none (`host_pid`, and `detach` answers none
at `os/proc_unix.rs:384`), so the handover paths that stop a harness by its pid (`harness/host.rs`
`stop_process`, `agent.rs:3161,3273`) never take it, and the session terminates it through its boundary
instead (`harness/session.rs:4953`). Hex delivers a member's own signal (`Runtime::signal_member`,
`Call::MemberSignal`, over Shards' agent-control endpoint) since its `414aa30d`, which kiln vendors.

## Tests

- `cargo test -p kiln-seams sandbox`: the suite, the simulator holding everything and holding nothing,
  and a sandbox that takes what it says it doesn't hold failing exactly those clauses (signals among
  them, SANDBOX-11).
- `cargo test -p kiln --lib sandbox::`: the host's sandbox passing the suite (Unix); the held run's
  input in order, its end, terminate's signal or input's end then kill, a run let go of killed, and
  its `Duplex` with no pid, nothing to hand over and its stop its boundary's; an isolated session's
  image and launch; a machine without microVMs saying why in its facts.
- `cargo test -p kiln --lib sandbox::hexvm`: the seam over Hex's simulator: a member's run (its stdin
  in order, back on its stdout, its end, its box gone), interrupt and stop reaching the member alone
  (`s17_p7_interrupt_and_stop_reach_the_member_and_its_box_goes`), kill and drop deleting its box, its
  charge kept before its end is said, a run that ended on its own with no final use Hex kept deleted plainly and
  charged its use last read
  (`p7_a_run_ended_with_no_final_use_told_goes_by_a_plain_delete_charged_its_last_read`), credentials granted by
  reference with their placeholders, and its harness member's MCP bridge asked and its connections taken through
  Hex (`s35_mcp_a_launch_asks_its_members_bridge_and_its_lane_takes_its_connections`).
- The real launch, on a machine whose kiln carries Shards (`ci/shards/pack.sh`) and has a hypervisor, the
  stand-in in each harness's place (`crates/kiln/tests/vm_e2e/README.md`, "Without a vendor's turn"):
  `VM_E2E=1 VM_E2E_IMAGE_<KIND>=stub cargo test -p kiln --test vm_harnesses step35::rest::` and
  `step35::mcp::`, and `VM_E2E=1 cargo test -p kiln --test vm_harnesses narrowed::`: a session launched
  through Hex into a Shards microVM, its stdio, kiln's MCP from inside, interrupt, stop and kill, and its
  box gone after.
- `VM_E2E=1 cargo test -p kiln --test vm_harnesses ended:: -- --test-threads 1` (Unix), on such a machine: a
  session's microVM whose VM process was killed, which told no final use, deleted and charged its use last read.
- `cargo test -p kiln --lib sandbox::image`: each image's build planned from its content (platform,
  its harness's build, its tag), with a stand-in `shards`: a built-in built once and pinned by digest,
  found in the store without a build after, a changed Agentfile built again, starts at once waiting
  for one build, a refused build in Shards' words, nothing of a build left behind.
- `cargo test -p kiln --lib sandbox::isolated::tests::s17_t4_`: a home, a folder holding it, `~/.ssh`, a
  folder that is, holds or lies in kiln's state (its people's volumes too, made or not yet) and a folder
  holding another worktree of its repository refused as a project, before anything starts; a project's
  own folder and a worktree of its own taken; in a workspace of its own, the folder it lies over judged
  (a project's taken, the home's refused), and another agent's workspace refused.
- `cargo test -p kiln --lib harness::lineage::tests::s17_`: a runner's own reason in a refusal; a
  person's own Agentfile read for them alone, at its latest version.
- `cargo test -p kiln --lib harness::kinds::tests::s17_`: every launch beginning with its base, and each
  built-in image's command carrying it.
- `cargo test -p kiln --lib harness::host::tests::s17_`: an isolated Claude Code session in the
  machine's microVMs' stand-in (`sandbox::fake`): it starts as the image's member with no process of
  the machine's, takes a message and answers, is interrupted (the control request at its member),
  is stopped by force and starts again on the same conversation (`--resume` past its image's command),
  is refused a switch and a move with its member going on, and ends with nothing of it running; and a
  machine without microVMs refusing one.
