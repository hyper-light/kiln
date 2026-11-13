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
  its image's own (default deny), narrowed no further, and the microVM sized as its boundary sizes
  one (the person's sizes reach it through its placement, step 32).
- **Its stdio are the member's streams.** The seam's run is the `Duplex` its session drives
  (`crate::proc::duplex_of_run`, `sandbox::held`): what the adapter writes reaches the member's stdin in
  order, the end of its stdin is the member's, what the member writes fills its stdout and stderr, and
  its end is the session's exit. Its stdout and stderr hold as many chunks unread as a process's pipes
  do (`proc::STDOUT_CHUNKS`, `STDERR_CHUNKS`, one bound for both): no number of its own, and the same
  backpressure on the member as on a process. The adapters are the ones host sessions use, unchanged.
- **It's no process of the machine's**: its session has no pid (`Duplex::host_pid` is none), nothing
  of it is in the session's group of processes, and it is never handed across an in-place upgrade
  (it stops, and starts again in the next image, its conversation going on).

What a session does to its harness maps onto the member:

| The session | On the host | In a microVM |
|---|---|---|
| start | a process, its pipes | the image's member, its streams |
| steer (a message, an answer, a mode) | the harness's protocol on stdin | the same bytes on the member's stdin |
| interrupt | the protocol's own (Claude's `interrupt` control request, Codex's `turn/interrupt`, ACP's `session/cancel`) | the same |
| resume | the harness's own arguments or protocol (`--resume`, `thread/resume`, `session/load`) | the same, past its image's command |
| stop | SIGTERM to its group, SIGKILL after its grace | `Run::signal(Terminate)` where the boundary delivers it, else the end of its input; its kill (the microVM's stop) after its grace |
| kill | SIGKILL | the microVM's stop |

## Refused, rather than done another way

- **A machine without microVMs** refuses an isolated agent before anything starts
  (`sandbox::NO_MICROVM`), never running it as a process; its runner says so in its facts
  (`isolation.why_not`, and `virt.microvm` false), where placement (step 32) reads it.
- **A move to another machine** (`sandbox::isolated::NO_MOVE`): its microVM and what its harness keeps
  there are its machine's. Refused as it's asked, before anything of it stops, so its member goes on.
  A move to the machine's newer runner process (an in-place upgrade) isn't refused: there it starts
  again in a microVM of the newer process, its conversation going on.
- **A switch to another harness** (`NO_SWITCH`): its microVM runs its own harness's image. Refused as
  it's asked, as a switch kiln can't make is.
- **kiln's own tools** aren't given it: kiln's MCP server, its skills and its hooks are reached through
  the machine's runner's socket, which no microVM reaches (step 18 brings them in). An isolated ACP
  agent is offered no MCP server.
- **What its boundary doesn't hold** is refused by the boundary, typed (`Refusal::Unsupported`), and the
  session's start fails in its words.

## Facts

A runner says whether it holds isolated agents (`RunnerFacts.isolation`: `available`, `hypervisor`,
`why_not`), and `virt.microvm` is that same `available`, one answer. `harness::lineage::isolation_on`
refuses an isolated start on a runner that says no, in the runner's own words.

## What the microVMs are, and what's owed

The machine's microVMs are a `Sandbox` implementation of their own (`sandbox::microvm`): Shards' image
boot (its D231) of the member, through Hex, its stdio carried on Shards' own member streams
(`shards-api`'s image channel with member stdin, version 6, Shards e7825cb3) until the agent-control
endpoint (step 11) carries them over hyper-transport. Until this kiln carries that runtime (step 16
vendors Shards and Hex), `sandbox::microvm` is none, every machine says it holds no isolated agent,
and the launch is held by the conformance suite and a stand-in alone (below).

Owed, each with whose it is:

- **The Shards implementation** of the seam (step 17's next slice, after step 16 lands with a Shards
  revision at e7825cb3 or later, the first with member stdin): the image, built and pinned as above,
  booted through Hex with the member's stdin asked for; the one real launch, red then green, and a real
  `shards build` of each image kiln ships, on each platform.
- **A member's run-time arguments** (Shards: steps 5 and 7). A member runs its image's command alone:
  `MemberSpec` takes its environment, files, working directory and stdin, no arguments. Codex and the
  ACP harnesses take everything a session asks over their protocols, so their launches carry no
  arguments; Claude Code, Antigravity and pi take theirs as flags (`--resume`, `--session-id`,
  `--model`, `--permission-mode`), which the Shards implementation refuses, typed, until Shards takes
  them.
- **The project at its host path as the harness starts** (Shards: step 8). D235 plugs a project into a
  member that's already running, and a member's working directory must be there as it starts.
- **A member's signals** (steps 11 and 12): until the endpoint's control lane, a stop is the end of the
  member's input, then the microVM's stop.
- **The microVM's group, charged** (step 32): placed and charged through the balancer.
- **Its configuration on a volume kiln reads on the host** (step 18): transcripts, resume and rewind
  from the host's side.

## Tests

- `cargo test -p kiln-seams sandbox`: the suite, the simulator holding everything and holding nothing,
  and a sandbox that takes what it says it doesn't hold failing exactly those clauses (signals among
  them, SANDBOX-11).
- `cargo test -p kiln --lib sandbox::`: the host's sandbox passing the suite (Unix); the held run's
  input in order, its end, terminate's signal or input's end then kill, a run let go of killed, and
  its `Duplex` with no pid, nothing to hand over and its stop its boundary's; an isolated session's
  image and launch; a machine without microVMs saying why in its facts.
- `cargo test -p kiln --lib sandbox::image`: each image's build planned from its content (platform,
  its harness's build, its tag), with a stand-in `shards`: a built-in built once and pinned by digest,
  found in the store without a build after, a changed Agentfile built again, starts at once waiting
  for one build, a refused build in Shards' words, nothing of a build left behind.
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
