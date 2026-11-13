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

## Alternatives weighed

- **A picked bound** (the 600 s and 60 s this replaces). R03 forbids it.
- **A cited bound for the cold start** (a harness's documented request timeout, the stand-in's declared
  delays). The person ruled out seeds of any kind.
- **Learning per kind of ask**. One sample each: a new answer is slower than a single earlier sample
  about half the time, so it refuses working harnesses.
- **The stand-in showing progress in its silences**. A tick of its own would be another picked number.

## Owed

- `POLL` (250 ms) is a probe's period, which bounds nothing. R05 prefers kiln's own change
  notifications, the session's event stream, to periodic looks.
- The fixture's setup waits keep picked numbers: `open` 90 s, `up` 120 s, `isolation_ready` 60 s and
  `headroom_off` 60 s.
- ROOT's runs: five of `cargo test -p kiln --test vm_harnesses -- step35::host:: host::` under load on
  a Mac Studio, and the microVM drives.
