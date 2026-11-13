# The drive's waits: the harness's liveness and progress, never a clock

Steps 34 and 35's drive (`crates/kiln/tests/vm_e2e/mod.rs`: `Pace`, `Watch`, `Asked`, `Stuck`, `Stall`)
and step 35's MCP drive (`mcp.rs`). kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 35; ROOT's (a) and
(b) of 2026-11-13.

## Why

- Every turn wait had a picked bound: 600 s (`turn_within`, `VM_E2E_TURN_SECS`) and 60 s for a stop
  (`STOP_WITHIN`). Picked numbers are forbidden in tests too (R03).
- In run r47 (adas-mac-studio.local, macOS arm64, kiln fa0cb01b2 on f963e0000),
  `step35::host::cursor`'s resumed session was certified at 15:39:52. The runner's log then said nothing
  of it for 600 s, and the failure said only `status "idle", exit null; it said []`
  (`~/.cache/e2e34-keep/run8-studio.log` on that host). Its first turn had taken about 11 s under that
  load: the session was certified at 15:39:28, and the agent it started at 15:39:39.5.

## What a refused wait says

`Refused` is typed (`Stall`). Its text gives the session's fields that say where it stands (status,
queue, what it does, its exit and starts, room and pause holds, admission, `last_activity`). It also
gives each of its messages' delivery and its queue, read from the state kiln sends its viewers. Last
comes its timeline, an event a line: order, kind, message id, tool and status or notice level, and the
first 80 characters (ROOT's ask), then the runner's log.

## The rule

The person's ruling, through ROOT and goal 77's agent (2026-11-13): "No record seeds. The cold start,
and every wait after a run-out, follows the party's liveness and progress. After that the bound is the
slowest answer seen."

- Each drive learns one value: its harness's slowest answer, where an answer is one whole wait from
  the drive's ask to its answer (`Pace`). Nothing is picked, seeded or carried over from another run.
- Before the first answer a wait has no time bound. It ends on its answer, or at once on kiln's own
  word that none can come (below).
- After that, a wait refuses (`Stall::Quiet`) only when nothing of the session moves for a whole
  window as long as the slowest answer. Any movement starts the window again.
- Progress is any change in kiln's word on the session or in its timeline. kiln's word counts as its
  status, queue, `doing`, `activity`, turn start and end, exit, `runs`, starts, approvals, tasks,
  subagents, pause, freeze, `room_wait`, admission credit, harness session and mode. In the timeline, a
  new event, growing words, or a tool's status count. `last_activity` doesn't count, since kiln moves it
  on every state it is sent, the same one again included.
- A message kiln holds in its custody is held, not quiet (`Verdict::Held`). R03: a host's hold is said as
  held, and the wait goes on. As its window runs out, the wait reads the session's state: where the
  message's delivery is `queued` (or it waits in kiln's queue), the wait says so with what kiln says
  holds it (its admission, room, pause, freeze), and the window starts again. ROOT's runs of 2026-11-13
  found cursor's, gemini's and goose's resumed prompts held so: idle, queued 1, admission
  `{learning: bootstrap, target 2, admitted 1, depth 2}`, delivery `queued`. A message kiln neither
  delivers nor holds for a whole window is refused (`Stall::Quiet`).
- A turn the drive asked to be long (the steered turn, `Asked::Steered`) waits as a first one does: no
  earlier answer bounds a turn asked to take longer. Bound by the slowest answer, the stand-in's steered
  turn would be refused, since it is silent for `POLL * 8` between lines by the drive's own ask. That
  silence is longer than a quick first answer (Pi's, with no MCP, on an idle host).
- A stop is judged by progress too. The stand-in's turn to stop holds once it says what it heard
  (`HOLD`), so only a stop ends it. Idle means stopped. A line of it after the stop was taken means it
  wasn't (`Stuck::NotStopped`), and otherwise the window decides.

## Refused at once, from kiln's own state

| `Stuck` | kiln's word | Source in kiln |
|---|---|---|
| `Crashed` | `status: crashed`: no runner holds it; only a person's resume takes it up | `harness/model.rs`, `Status::Crashed` |
| `Refused` | `exited` with `runs` still set: its startup was refused while its process lives | `harness/acp.rs`, `startup_failed` (CP-T3.2); `harness/session.rs`, `ended_run` clears `runs` before `exited` |
| `Ended` | `exited`, `exit.again_at` unset, and it never started (no start recorded) or kiln's next word after the exit set no restart | `harness/session.rs`: "Its process ending never ends the agent … It starts again by itself" (`again_at`); `began` records every start; the exit and kiln's decision on it are sent one after the other |
| `Answered` | idle; the message delivered, its delivery `completed` or `cancelled`, no other message's under way, nothing queued, nothing under way or held for room, and its last turn not `cut` or `limit` | every adapter marks `Delivery::Completed`/`Cancelled` as the message's own turn ends. A message Claude Code queued itself stays `sent` while it is idle, which is why the timeline alone doesn't decide this |
| `Asks` | approvals pending: a person's answer, which a drive never gives | `Info::approvals` |
| `NotStopped` | the stand-in's held turn said more after its stop was taken | the stand-in's `HOLD` |

None of these refuse:

- `exited` with `again_at` set: kiln starts it again by itself.
- An exit kiln hasn't decided on yet.
- Idle with a message queued, held for room, or paused.

These follow progress and the window.

## The fixture's setup waits

Before any harness has answered, the fixture waits on its own parties: the runner connecting (`up`),
its headroom turned off, its facts on microVMs (`isolation_ready`), and each session's start (`open`).
These waits were 120 s, 60 s, 60 s and 90 s. Each is now a cold start (`setup`): it follows its
parties with no time bound, looking each `POLL`, and refuses at once, typed (`SetupStuck`), where nothing
can progress.

- **A process of the fixture's own ended** (`Exited`). The server's and the runner's processes are the
  fixture's children (`Child::try_wait`, on every OS). ROOT's runner (`VM_E2E_RUNNER_PID`) is looked at
  by its pid where kiln can tell one runs (`os::pid_alive`, Unix; on Windows it says nothing of a pid).
- **The server's final word** (`Refused`): a client error (4xx). A start refused while the runner was
  listed connected before and after it is final. Refused while the runner was between connections, it's
  asked again (kiln answers 400 to both). The server's own trouble (5xx), and a look that couldn't reach
  it, are looked past while its process lives.
- **Facts on microVMs**: a runner's facts always say `virt.microvm` (`kiln_ci::facts::Virt`), so the
  wait follows the runner until its facts come, then takes their yes or no. The 60 s took facts that
  hadn't come yet for a runner that says nothing of microVMs.

## What a wait wakes on

No wait looks on a period of its own (the 250 ms `POLL` it had): between looks it waits for kiln's own
next change notification (`Wake`, R05).

- **A session's waits** (`Pace::until`) read the session's own socket,
  `GET /api/harness/sessions/{id}/ws`. That is the stream kiln's UI reads a session from, and every
  update of the server's copy of the session (its events, state and meta, each frame carrying the
  copy's revision: docs/wire.md, "Revisions") comes down it as it's made. Its snapshot (up to
  `synced`) is what's there, not a change, and is read past as the socket opens.
- **Waits on kiln's own work and the setup waits** (`Pace::unlisted`, `setup`) read kiln's events socket,
  `GET /api/events`: the hub's events as they come (docs/wire.md, "The events socket"). A session's
  end comes down it (`HarnessClosed`), and so do a runner's record on its connect and its loss
  (`Hub::emit_agent`). Every connected runner's periodic host samples come too, so a change kiln
  announces nowhere (a runner's facts, its headroom applied) is looked at by the next one.
- A frame wakes the wait, and the frames that follow within as long as a look took are read with it:
  looks never outrun kiln's answers. A read that waits is bounded only by what's left of the wait's
  window (`Watch::left`), and has no bound before a first answer.
- The socket is opened as it's first waited on, and again after it ended. That wait returns at once,
  so a change made while it opened is looked at. An opening that fails pauses as long as it took.
- What a look reads (kiln's word and the timeline over the API), the typed refusals and the evidence
  are as before.

## Alternatives weighed

- **A picked bound** (the 600 s and 60 s this replaces). R03 forbids it.
- **A cited bound for the cold start** (a harness's documented request timeout, the stand-in's declared
  delays). The person ruled out seeds of any kind.
- **Learning per kind of ask**. One sample each: a new answer is slower than a single earlier sample
  about half the time, so it refuses working harnesses.
- **The stand-in showing progress in its silences**. A tick of its own would be another picked number.

## Owed

- The stand-in's steered gap (`STEERED_GAP`, 2 s, what `POLL * 8` was) is its own pace, which no
  wait reads: to be derived from a measured round trip through kiln.
- ROOT's runs: five of `cargo test -p kiln --test vm_harnesses -- step35::host:: host::` under load on
  a Mac Studio, and the microVM drives.
