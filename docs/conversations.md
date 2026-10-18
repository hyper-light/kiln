# Conversations: durable, replicated, searchable

> **Phase 2 of usage limits and durability (2026-10-18).** The status table at the end says what
> is built. Phase 1, pausing at a usage limit and going on at the reset, is in
> [agents.md](agents.md) ("Usage limits: pause and resume").

An agent's conversation is kept per event, on disk, as it happens. Each runner keeps its own
sessions, and sends them to the server, which keeps every session from every machine. So a
conversation can be:

- **resumed** after any restart, exactly as it was, with every event's id, order and time;
- **resumed on another machine**, even when the one it ran on is gone;
- **filtered and searched**, per agent and across every agent on every machine.

## What is kept

For each agent session (kiln's id):

| Record | What | From |
|---|---|---|
| the session | info, meta, state (queue, pause, limits …), changeset, its epoch | the session's updates (`Open`, `State`, `Meta`, `Changes`) |
| its events | every timeline event, upserted by id: messages, thoughts, tool calls with output, notices | `Events` updates (streamed text is kept once its event is whole) |
| the harness's record | the harness's own transcript, line by line: Claude Code's JSONL, Codex's rollouts | followed on the session's machine as its files grow (`convo/record.rs`) |

- **Large payloads** stay as they are today. Tool output is clipped to `TEXT_MAX` in the event, and
  a task's full output is read on its machine.
- **A reset** (`Update::Reset { from }`) means the timeline starts over after a restart, a move or a
  rewind. The store drops the session's events from number `from` on, then keeps the snapshot that
  follows. Events before `from` stay: the timeline no longer holds them in memory (it holds
  `MAX_EVENTS`, 4,000), but they are its history.
- **A clear** (`/clear`) is a reset too: the timeline starts over from the line saying so, and
  `State::cleared` holds that line's number. The events before it stay in the store, where search
  finds them, but a timeline taken up again from the store (after a restart, a move, or in the
  server's list of agents whose machine is away) leaves them out.
- **Transcripts are kept compressed.** They are nearly all of what a store holds: on the first
  server, 5.4 GB of 6.5 GB, in 15 transcripts of long-running agents. A line of 256 bytes or more
  is kept brotli-compressed (quality 5) when that saves a tenth or more, about 3× overall. Lines
  kept before that are compressed in the background, a few thousand at a time while the store has
  nothing else to do, under the same numbers, so no feed sends them again. Feed batches travel
  compressed too, to a taker that says it takes them (`zip` in its answer).
- **The harness's record** is numbered among each file's non-empty lines, from 0. A move carries
  those lines alone, so a copy of the file on another machine numbers them the same way. After a
  restart, following a file goes on where the store's copy ends. The file is first checked: one
  that was replaced since (shorter, or not ending with the line kept) starts over.

## The store: Turso, per machine

Each machine keeps its conversations in a [Turso](https://github.com/tursodatabase/turso) database:
the SQLite-compatible engine in Rust, embedded in kiln's binary and vendored in `vendor/turso`
(`UPSTREAM.md` lists the commit, the features kiln builds it with, and the changes). Rust only: no
C, no SIMD, no FTS.

- **Where:**
  - A runner's store is `<state>/conversations.db` and holds its own sessions. A runner beside
    another one of its machine (`<state>/instances/<id>/`) has a store of its own.
  - The server's store is `<data_dir>/harness/conversations.db` and holds every session, from every
    machine.
- **Reading it by hand:** the file is SQLite's format. kiln holds it open exclusively, so `sqlite3`
  reads it when kiln isn't running, or reads a copy of it together with its `-wal`.
- **Turning it off:** `KILN_CONVERSATIONS=off` turns the store off on a machine, which then runs as
  it did without one.
- **Per event.** Every update a session flushes also goes to the machine's store writer, which
  commits in batches. An update is durable within `COMMIT_MS` (50 ms) of its flush, or as soon as a
  batch reaches `COMMIT_ROWS`. One writer thread owns the database; sessions reach it by channel,
  never by lock.
- **Revisions.** Every write gets the store's next number (`rev`). That is what a feed sends: what
  came after a number (see Replication). Each reset of a session's events is a row of its own in
  `drops`, numbered like the writes, so a feed sends it in its place.
- **Schema** (`convo/sql.rs`, migrations by number; one that shipped is never edited):
  - `sessions(id, info, meta, state, changes, updated_at, closed_at, epoch, rev, machine)`. The JSON
    columns hold the model as it serializes. `machine` is the machine whose feed brought the
    session (the server's store).
  - `events(session, id, seq, at, parent, by, kind, level, tool, status, text, body, rev)`, keyed by
    `(session, id)`. `text` holds what a search matches: a message's words, a tool's title and
    output, a notice's text.
  - `records(session, source, n, line_end, line, rev)`: the harness's record. `source` is Claude
    Code's session id, or a Codex rollout's file name.
  - `drops(rev, session, what, source, n, at)`: what was dropped.
  - `peers(peer, rev, at)`: through where this store has each peer's feed.
- **Restarts.** A session restored after a restart (`host::restore`) takes its timeline and state
  from the store (`Core::adopt`), as a move does, instead of rebuilding them from the harness's
  transcript. The harness still resumes its own conversation.

## Replication: the feed

A runner's store sends the server's what it wrote after the number the server has, oldest first
(`convo/feed.rs`). The server takes each batch in one transaction, with that number kept beside
what it took, and says through where it has it.

- **The link.** The feed travels on the runner's link to the server: hyper-QUIC
  (hyper-transport on hyper-rt), on a lane of its own (`LANE_CONVO`, bulk frames). There are no new
  ports and no HTTP. The runner announces it with `CAP_CONVO`.
- **The exchange:**
  1. When the link comes up, the runner says hello with its store's id.
  2. The server answers with the number it has (0 for a store it hasn't seen).
  3. The runner sends batches from there, two ahead of what the server has acknowledged. A batch
     larger than a frame goes in parts, so a transcript line of any size arrives whole.
- **Nothing is lost.** A runner killed, a server killed mid-turn, or a runner away for days: the
  server's number says where to go on, and the runner's store still has everything after it.
  Applying is idempotent: events upsert by id, record lines by number.
- **What the server keeps from where.** A runner that sends its store is the one source of its
  sessions in the server's store. The server stops keeping them from their updates, which a
  reconnect sends again in full. A runner from before the feed is kept from its updates, as in C1.
- **Why not a Raft group per session.** The server is the one voter, so Raft would add a second log
  beside the store and whole-store images, without making anything safer. The feed gives the same
  guarantees: ordered delivery, catching up, and fencing. Raft (hyper-raft) is the plan for
  replicating the server itself.

## Fencing: epochs

A session's `epoch` (`State::epoch`, also in its `Info`) counts the times it went on on another
machine. A move raises it as the session is taken over, and so does a resume from the server's
store.

- **In the server's store:** a session's rows from a lower epoch than the one it has are left out.
  These are a former writer's late rows, after it went on elsewhere.
- **In the registry:** a machine that announces a copy of a session from a lower epoch (it was away
  when the session went on elsewhere) is told to let go of it (`HarnessRelease`). Its harness stops,
  and the session isn't closed. A runner from before letting go has its copy closed instead.
  Nothing a copy left behind says counts. Its updates, its inventory and its feed rows are left
  out, and it never takes the session's messages from the machine it runs on.

## Resuming on another machine

**Move** on an agent whose machine is away (`POST /api/harness/sessions/:id/move {to}`) goes on from
the server's store (`convo/resume.rs`):

1. The server packs the session as a move does (`harness::moving::Package`):
   - its settings as last chosen;
   - its state, queue included;
   - its timeline;
   - the harness's record: Claude Code's transcript lines, or for Codex the conversation as a new
     thread's history. For a harness whose own record kiln doesn't keep (Gemini CLI, opencode,
     Goose, Amp, Antigravity), the conversation in words from kiln's timeline
     (`handoff::as_words`): the harness there starts a new conversation and is given it, once, with
     the first message it gets. The timeline shows that message as the person wrote it.
2. It raises the session's epoch, here and in its store.
3. It sends the package to the target runner in pieces on the feed's lane (`HarnessPackage`).
4. That runner installs the record where its harness looks for it, and takes the session over under
   the same id, as it does a moved one.

The agent's folder is mapped as a move maps it: a copy of its repository kept in step there, else
the same place under that machine's home. Its files stay on the machine that's away. Its environment
stays there too: the machine it goes to runs it with its own.

**A machine that's gone.** Once a machine has been offline for two minutes with agents on it,
Mission Control says so above the list: if it's gone, **Move them on** moves every agent of it
that hasn't ended at once, each to the machine that suits it best (connected, its harness
installed, the same platform first, then the one running the fewest agents) or all to one machine
picked there (`POST /api/harness/machines/:machine/recover {"to"?}`; a machine that's connected is
refused, since its own runner hands its agents over). Each goes on as Move does, and the answer says
where each went or why it couldn't.

**After a server restart**, the agents of machines that are away (not ended, heard of in the last 30
days) are listed from the store, as away, with their recent timeline, so they can be moved on. A
machine that comes back says what it has, and that wins. Removing an away agent ends it in the store
too.

## Mirrors

Other runners keep a copy of the server's whole store (`convo/mirror.rs`). A conversation then
outlives the server's disk and its own machine both.

- **How.** The server sends each mirror its store the way a runner sends the server its own: the
  same feed, the other way, on the same lane. A mirror keeps the copy beside its own store, in
  `<state>/mirror.db`, with where each session runs.
- **Which runners.** `[conversations] mirrors` says how many (1 unless set; 0 for none). The server
  picks them among the connected runners that keep a store:
  1. those named in `mirror_on` (one named takes the place of one that wasn't);
  2. then the ones connected the longest (a machine that sleeps reconnects often);
  3. then the most free disk.

  A mirror away for a day is replaced; its copy stays where it is.
- **Seeing them.** `GET /api/conversations/mirrors` returns `{mirrors: [{machine, connected,
  chosen_at, last_seen}]}`.
- **Restoring the server's store from a mirror:**
  1. Stop the server.
  2. Copy the mirror's `<state>/mirror.db`, together with its `-wal`, to
     `<data_dir>/harness/conversations.db`. While kiln runs on the mirror, copy both files as they
     are.
  3. Start the server. The agents of machines that are away are listed again from the copy, on the
     machines they ran on, and can be moved on. Runners that are still there send their own stores
     again, in full.

## Filtering and search

- `GET /api/conversations/search?q=&kind=&tool=&status=&agent=&since=&until=&limit=` returns
  matching events across every agent, newest first: `{agent, name, machine, event}`. The server
  answers from its store. Words match anywhere, in any case, all of them.
- `GET /api/harness/sessions/:id/events?before=&limit=` returns a session's history beyond what the
  timeline keeps in memory (`MAX_EVENTS`), page by page, from the store.
- **In the console:**
  - **Mission Control's filter box** also searches conversations, from three characters typed. Under
    the agents, *In conversations* lists the matching events, newest first: which agent, which
    machine, who said it, when, and the words around the match, marked. A match opens its agent
    with the find bar already looking for the same words. An agent that ended and is forgotten has
    its matches listed, but not as links.
  - **An agent's find bar** finds what its timeline holds. When the timeline holds only the latest
    events (`MAX_EVENTS`), the matches before them, from its history, are listed under the bar
    (*N earlier, in its history*) to read in place.
  - The UI mock answers the search too: `MOCK_DROPPED=n` gives its long conversation an earlier
    history.

## Settings

- `KILN_CONVERSATIONS`, the same on every machine: `off` turns the store off. Replication to the
  server needs no setting: a runner with a store sends it.
- The server's `kiln.toml`:

```toml
[conversations]
mirrors = 1          # runners that keep a copy of the server's store; 0: none
mirror_on = []       # runners to pick first, by name
```

## Milestones

| | What | Done when |
|---|---|---|
| C1 | Turso embedded; each machine's store (a runner keeps its own sessions; the server keeps every session as its updates come in); the writer (per event, batched); restore from the store; `events?before=` paging | a session restarted keeps its whole timeline; 10k events written and read back in tests; musl, macOS and Windows builds |
| C2 | Replication: the feed, the lane, epochs; the harness's record followed and replicated | killing a runner loses nothing; a server killed mid-turn catches up; a runner offline catches up; checked with a real Claude Code |
| C3 | Resuming on another machine from the server's store; the copy left behind lets go | a session whose machine is gone goes on elsewhere, its conversation and harness record intact, also after a server restart; checked in the lab with a real Claude Code |
| C4 | Search in the console: the filter box and the find bar | search across agents, newest first, by words; a match opens its agent where the words are |
| C5 | Mirrors: other runners keep a copy of the server's store | the server's store lost and a runner gone too: nothing lost |

## Risks

- **Turso is pre-1.0** (0.9.0-pre.1). The store is a copy, never the only record: the harness keeps
  its transcript, and the server holds a replica. The file is SQLite's, so it can be recovered with
  `sqlite3`. Turso is pinned by commit and vendored, with changes listed in its `UPSTREAM.md`.
- **Lock order.** The writer never takes a session's lock: updates come to it by channel from
  `flush`. The recorder takes a session's locks one at a time (`Session::record_source`). See the
  lesson of c0ffda6 (agents.md).
- **Disk.** Conversations grow, and so do transcripts: a long-running agent's reaches hundreds of
  MB. They are kept compressed. A compressed store doesn't shrink its file by itself: the space
  freed is used again for what comes next (Turso's `VACUUM INTO` makes a compact copy). Pruning
  ended sessions' transcripts (`keep_closed_days`) is still to come.
- **A runner's first feed** sends its whole store, everything it kept before the feed.

## Status

| Part | State |
|---|---|
| C1 store | live (1a59fad, 2026-10-18): the Turso engine, each runner's store of its sessions, the server's store of every session, the writer, restore from the store, search and paging API |
| C2 replication | built: revisions, the feed on its lane, the harness's record followed, epochs, resets that keep history. Lab with a real Claude Code: the server's store matches the runner's and the transcript line for line; a server killed mid-turn catches up; a runner killed comes back and goes on |
| C3 resume elsewhere | built: Move falls back to the server's store when the machine is away; the transcript lands where Claude Code looks; the copy left behind lets go; away agents listed after a server restart. Lab with two runners and a real Claude Code: the conversation goes on with its context. Every harness now (ACP agents and Antigravity with their conversation in words), and a gone machine's agents moved on at once (Mission Control's Move them on). Lab (`~/.cache/kiln-mcp-lab/recover_e2e.py`, 12 checks): an ACP agent's runner killed outright, its agent moved on to the other runner, which answers from the conversation it was given |
| C4 search UI | built: Mission Control's filter box finds what was said across every agent; an agent's find bar lists the matches in its earlier history. Checked against the UI mock on desktop Chromium, and on phone-sized Chromium and WebKit |
| C5 mirrors | built: the server picks runners to keep a copy of its store and feeds them; a copy restores the server's store. Lab with two runners and a real Claude Code: the source runner killed and the server's store deleted, the store restored from the mirror, the agent listed away on its machine, moved on, its conversation going on with its context |
