# Conversations: durable, replicated, searchable

> **Spec, phase 2 of usage limits and durability (2026-10-18).** The status table at the end
> says what is built. Phase 1, pausing at a usage limit and going on at the reset, is in
> [agents.md](agents.md) ("Usage limits: pause and resume").

An agent's conversation is kept per event, on disk, as it happens. It is replicated to the server,
and to other runners if you want, so it can be:

- **resumed** after any restart, exactly as it was, with every event's id, order and time;
- **resumed on another machine**, even when the one it ran on is gone;
- **filtered and searched**, per agent and across every agent on every machine.

## What is kept

For each agent session (kiln's id):

| Record | What | From |
|---|---|---|
| the session | its spec (what starts it again), info, meta, state (queue, pause, limits …), changeset | the session's updates (`Open`, `State`, `Meta`, `Changes`) |
| its events | every timeline event, upserted by id: messages, thoughts, tool calls with output, notices | `Events` and `Stream` updates (stream text folded into its event) |
| the harness's record | the harness's own transcript: Claude Code's JSONL lines; for Codex, its rollout's lines | read on the session's machine as it grows |

Large payloads stay as they are today: tool output is clipped to `TEXT_MAX` in the event, and a
task's full output is read on its machine. A `Reset` (a rewind) drops the session's events after
the rewind point, as the timeline does.

## The store: Turso, per machine

Each machine keeps its conversations in a [Turso](https://github.com/tursodatabase/turso) database
(`<state>/conversations.db`): the SQLite-compatible engine in Rust, embedded in kiln's binary.
The file is SQLite's format, so `sqlite3` reads it.

- **Per event.** Every update a session flushes also goes to the machine's store writer, which
  commits in batches. An update is durable within `COMMIT_MS` (50 ms) of its flush, or as soon as a
  batch reaches `COMMIT_ROWS`. One writer thread owns the database, and readers use their own
  connections.
- **Schema** (`convo/db.rs`, migrations by number):
  - `sessions(id PRIMARY KEY, kind, machine, cwd, name, created_at, updated_at, closed_at, spec, info, meta, state, changes)`
    holds JSON columns as the model serializes them.
  - `events(session, id, seq, at, parent, by, kind, level, tool, status, text, body, PRIMARY KEY(session, id))`.
    `text` holds what a search matches: a message's words, a tool's title and output, a notice's text.
    There are indexes on `(session, seq)`, `(kind, at)` and `(tool, status)`.
  - `records(session, n, line, PRIMARY KEY(session, n))` holds the harness's record, line by line.
- **Restarts.** A session restored after a restart (`host::restore`) takes its timeline and
  state from the store (`Core::adopt`), as a move does, instead of rebuilding them from the
  harness's transcript. The harness still resumes its own conversation.
- **Ended sessions** stay until removed (`DELETE /api/harness/sessions/:id` removes them from the
  store too, after `KEEP_CLOSED`, 30 days, unless pinned).

## Replication

**A first replica, from C1.** The server already receives every session's updates, from every machine, to show them. It keeps them in its own store as they come in, so it holds every conversation. A runner that was away re-announces its sessions in full when it reconnects, and the server's copy catches up. What C2 adds is guaranteed delivery, the harness's record, extra replicas and fencing.

Each session's conversation is one **Raft group**: hyper-raft's core in hyper-durable's shell, its
log on hyper-log. The machinery is repo sync's (`sync::group`), with its own log file on each
machine (`<state>/convo.log`, laid out for `GROUPS` groups).

- **Members.** The server is the voter, and so the leader. The session's machine is a learner
  that proposes through it. Other runners can hold a replica as learners
  (`[conversations] replicas = n`, picked by availability, never a sleeping laptop).
- **Entries.** Each entry is a batch of the session's records (updates and record lines), with the
  writer's epoch. The server applies committed entries to its own store, so it holds every
  conversation, and each learner applies them to its own.
- **Fencing.** Only the current writer's entries apply: a session that moves bumps the epoch,
  so the machine it left can't write into it any more, even if it comes back.
- **Offline.** A runner that can't reach the server keeps writing its own store and proposes
  what it holds once it's back. Applying is idempotent (events upsert by id, records by number).
- **No new ports.** The group's messages travel on the runner's link to the server, on a lane of
  their own (`ServerMsg::Convo` / `AgentMsg::Convo`), as repo sync's do. A runner without the
  capability (`CAP_CONVO`) keeps its own store and isn't replicated.

## Resuming on another machine

`POST /api/harness/sessions/:id/resume_on {machine, cwd?}`, or **Move** on a session whose machine
is gone. The server packs the session from its store, as `moving::pack` does: the spec, the
harness's record, events and state. It hands the package to the target runner, which installs it
(`moving::install`) and starts the session under the same id. Its epoch goes up first, so the old
machine's writes no longer count. A machine that comes back finds its copy of that session handed
on and stops it, leaving the files as they are.

## Filtering and search

- `GET /api/conversations/search?q=&kind=&tool=&status=&machine=&agent=&since=&until=&limit=` returns
  matching events across every agent: `{agent, name, machine, event, snippet}`, newest first.
  The server answers from its store.
- `GET /api/harness/sessions/:id/events?before=&limit=` returns a session's history beyond what
  the timeline keeps in memory (`MAX_EVENTS`), page by page, from the store.
- **In the console:**
  - Mission control's filter box also searches conversations. Its results open the agent at
    that event.
  - An agent's find bar (Ctrl F) searches its whole history, not only what is loaded.

## Settings

```toml
[conversations]
enabled = true        # keep conversations in the store (per machine)
replicate = true      # replicate to the server (runners) / hold every conversation (server)
replicas = 0          # extra runners holding each conversation, besides the server
keep_closed_days = 30
```

## Milestones

| | What | Done when |
|---|---|---|
| C1 | Turso embedded; each machine's store (a runner keeps its own sessions; the server keeps every session as its updates come in, so it is a first replica of all of them); the writer (per event, batched); restore from the store; `events?before=` paging | a session restarted keeps its whole timeline; 10k events written and read back in tests; musl, macOS and Windows builds |
| C2 | Replication: the per-session group, the lane, epochs, the server's store of every conversation; the harness's record replicated | killing a runner loses nothing the server acknowledged; a runner offline catches up; tests with two members over the real group code |
| C3 | Resuming on another machine from the server's store | a session whose machine is gone goes on elsewhere, its conversation and harness record intact; checked in the lab with a real Claude Code |
| C4 | Search: the API and the console's filter box and find bar | search across agents, newest first, by words, tool, status, machine and time |

## Risks

- **Turso is pre-1.0** (0.9.0-pre.1). The store is a copy, never the only record: the harness keeps
  its transcript, and the server holds a replica. The file is SQLite's, so it can be recovered with
  `sqlite3`. Turso is pinned by commit and vendored, with changes listed in its `UPSTREAM.md`.
- **Lock order.** The writer never takes a session's lock: updates come to it by channel from
  `flush`. See the lesson of c0ffda6 (agents.md).
- **Disk.** Conversations grow: `keep_closed_days`, and records bounded per session.

## Status

| Part | State |
|---|---|
| C1 store | in progress |
| C2 replication | not yet |
| C3 resume elsewhere | not yet |
| C4 search | not yet |
