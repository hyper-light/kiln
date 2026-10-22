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
  compressed too, to a taker that says it takes them (`zip` in its answer), and a compressed line
  goes as it is kept to a taker that says it takes lines so (`kept`): it is never decompressed on
  one side and compressed again on the other. A store from before says neither, and gets text.
- **An ended agent's transcript goes after `keep_closed_days`** (30 unless set). Its events stay,
  where search finds them; only the harness's record of it is let go, as nothing reads that once
  the agent has ended. Each store lets go by its own clock, a few thousand lines at a time while
  it has nothing else to do, and writes no drop, so no feed carries it: the server's store keeps
  them as long as `[conversations] keep_closed_days` says, runners and mirrors the 30 days.
  Claude Code keeps its own transcripts 30 days by default too. On the server, a transcript is also
  archived in kiln's store of bytes once its agent ends (docs/blobs.md, `transcripts/`), and the
  store lets no record go before that: it holds back every session that ended since the oldest
  close not archived yet.
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
  2. The server answers with the number it has (0 for a store it hasn't seen), and how it takes
     batches (`zip`, `kept`).
  3. The runner sends batches from there, two ahead of what the server has acknowledged. A batch
     larger than a frame goes in parts, so a transcript line of any size arrives whole. Reading a
     batch reads no more of the store than the batch takes: a page of rows stops at its bytes.
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
- **Restoring the server's store from a mirror.** A mirror sends its copy back the way it got it,
  over the same feed, and the server takes it as a fill: a session the server has nothing of
  comes back as it was, listed away on the machine it ran on; a session the server has anything
  of from elsewhere (its own machine, still there, sent it again) keeps what it has, and one whose
  machine comes back while the copy is being taken goes on with what its machine says. What the
  copy dropped stays dropped. The copy goes through what it held when it began, then the mirror
  says so (`Restored`), and the sessions it brought back are listed at once, no restart needed.
  What the copy brought isn't sent back to that mirror: the server notes the numbers it wrote it
  under, and the mirror's feed goes through them without sending them.
  - **By itself:** a server that starts with an empty store while `mirrors.json` still names its
    mirrors (the store was lost, the rest wasn't) asks the first of them to connect.
  - **On demand:** `POST /api/conversations/restore {"from": "<machine>"}` asks that runner;
    `GET /api/conversations/restore` says where it stands (`idle`, `asked`, `copying`, `done` with
    how many sessions came back, or `failed` and why). A runner that keeps no copy says so.
  - **By hand,** as before: stop the server, copy the mirror's `<state>/mirror.db` with its `-wal`
    to `<data_dir>/harness/conversations.db`, start the server.

## Filtering and search

- `GET /api/conversations/search?q=&kind=&tool=&status=&agent=&since=&until=&limit=` returns
  matching events across every agent, newest first: `{agent, name, machine, event}`. The server
  answers from its store. Words match anywhere, in any case, all of them.
  - It looks at events newest first by an index (across agents by time, in one agent's by number),
    so nothing is sorted and it stops once it has as many as asked: a word many events say is
    found in milliseconds.
  - It looks a slice at a time, each slice holding the store for about 50 ms (how many events a
    slice holds follows how quick the disk is), and the store's other reads and writes go between
    slices. A word few events say means looking at all of them: on a store of 1M events about
    5 s, the store answering everything else meanwhile in tens of milliseconds. Past 8 s it
    answers with the newest matches it found. (It once sorted every event on the store's writer:
    23 s for such a word on 1M events, everything else waiting.)
- `GET /api/harness/sessions/:id/events?before=&limit=` returns a session's history beyond what the
  timeline keeps in memory (`MAX_EVENTS`), page by page, from the store.
- **In the console:**
  - **Mission Control's filter box** also searches conversations, from three characters typed. Under
    the agents, *In conversations* lists the matching events, newest first: which agent, which
    machine, who said it, when, and the words around the match, marked. A match opens its agent
    with the find bar already looking for the same words. An agent that ended and is forgotten has
    its matches listed, but not as links.
  - **A long conversation** opens on its latest 30 turns. Earlier ones join as you scroll up, without
    moving what you're reading, and back at the end it holds the latest again. A long turn shows its
    latest 60 steps, with *N earlier steps* to show the rest.
  - **An agent's find bar** finds what its timeline holds, and opens the turn of the match it shows. When the timeline holds only the latest
    events (`MAX_EVENTS`), the matches before them, from its history, are listed under the bar
    (*N earlier, in its history*) to read in place.
  - The UI mock answers the search too: `MOCK_DROPPED=n` gives its long conversation an earlier
    history.

## Telemetry

What every store is doing, live: on the System page (*02 Conversations*), on `/metrics`, and from
the API. Nothing of it asks a store's writer for anything.

- **What each store counts** (`convo/telemetry.rs`), as its writer and its readers go:
  - commits and the rows in them, reads run, and how long the writer was held (by commits, reads,
    and looking after the file);
  - updates dropped while the writer couldn't keep up, reads that got no answer in time, and how
    long reads waited, by power of two of milliseconds.
- **What it sees of its file.** Every 2 s, between whatever else it does, the writer looks at its
  file. That takes the header's numbers and the log's size, nothing more. It notes:
  - the file's size, its free part and its log, and its last write number (and when that last
    changed);
  - its sessions, counted every 30 s;
  - what it does to the file while idle: compressing older lines, letting old transcripts go, or
    making it compact (which phase, which table, and how much of it is copied);
  - when it last made the file compact, and when it last let old transcripts go.
- **Feeds.** The sending side counts batches, rows and bytes, acknowledgements, hellos and quiet
  spells (`ShipStats`). The taking side counts, for each store that feeds it:
  - batches, the rows taken and those left out, and bytes;
  - gaps and errors;
  - through where it has the store, and how long the last batch held the store (`PeerView`).
- **Runners report.** Every 5 s while linked, with its host sample, a runner sends the server what
  its stores are doing (`Msg::Stats` on the feed's lane): its own store and its feed, and its mirror
  copy if it keeps one. Versions mix safely:
  - a server from before takes the report for nothing;
  - a runner from before says nothing, and is shown as *feeding*: its batches still count, but how
    far behind it is isn't known.
- **The board** (`convo/board.rs`, `harness/stores.rs`): the server puts it together. It holds:
  - the server's own store;
  - each runner's store, from what the runner said, what its feed brought, and the store's `peers`
    table (that table covers those not heard from since the server started);
  - the machines that keep none;
  - each mirror's copy.

  Each store is in one state: *in sync*, *catching up*, *behind* (and nothing moving), *offline*,
  *feeding* or *keeps no store*. *Rows to come* is, for a runner's store, its last write less what
  the server has of it; for a mirror's copy, the server's last write less what the mirror
  acknowledged.
- **Series and notes.** Every 5 s the server samples the board.
  - Each store's series takes a point: an hour at 5 s and a day at a minute, kept in memory like the
    machines' samples.
  - What changed since the last sample is noted: a store that came or went, fell behind or caught
    up, a gap, a batch that couldn't be taken, a quiet spell, updates dropped, reads that timed
    out, a compaction begun or done, old transcripts let go, a restore.
  - The replication log takes a line for what each feed moved since the sample before (batches,
    their rows and bytes on the wire, and the row it reached), into the server's store from each
    runner's and out of it to each mirror's copy, and a line for each note. It keeps its last
    5,000 lines. A feed's counters count from when kiln started: one seen first is only counted,
    and counters that went down (a restart) count from nothing.
- **API** (`runners.read`):
  - `GET /api/conversations/status` returns `{at, on, mirrors_wanted, restore, stores, notes}`;
  - `GET /api/conversations/series?key=&window=` returns one store's series, a column each, rates
    per second;
  - `GET /api/conversations/log?since=&q=&limit=` returns `{at, total, matched, lines}`: the
    replication log's lines after `since` (unix ms) with `q` in their words or their store's key
    (any case), newest first, at most `limit` (1,000; up to 5,000).
- **Prometheus.** `/metrics` labels each line by `store`, `role` and `machine`:
  - `kiln_convo_store_up`, `_bytes`, `_free_bytes`, `_wal_bytes`, `_rev`, `_sessions` and
    `_behind_rows`;
  - `kiln_convo_store_commits_total`, `_busy_milliseconds_total`, `_dropped_total` and
    `_read_timeouts_total`;
  - `kiln_convo_feed_batches_total` and `kiln_convo_feed_bytes_total`.
- **On the System page,** the figure draws every store as a database:
  - runners' stores on the left, the server's in the middle, the mirror's copy on the right (below
    it on a phone);
  - a light runs on a cable about once a batch while batches move on it, rows to come wait as marks
    at its start, and a cable whose store is away is faint; idle cables stay still;
  - picking a store says what it is doing and dims the other runners' cables; under the picture,
    the stores in sync, the rows still to come and the mirror's copy.

  Below it, the replication log: read again every 4 s, searched on the server as you type, and
  downloaded whole (every line that matches). Then every store in a table. Picking one (in the
  table or in the figure) shows its dashboard: writes, the file, the feed (rows to come, batches),
  and the writer (busy, reads' waits at p50 and p99), over 1 h, 6 h or 24 h, with what happened to
  it lately. The UI mock has stores too: they write, feed, fall behind and catch up, the server's
  file is made compact every ten minutes, and its log is sampled every 5 s as the server's is.

## Settings

- `KILN_CONVERSATIONS`, the same on every machine: `off` turns the store off. Replication to the
  server needs no setting: a runner with a store sends it.
- The server's `kiln.toml`:

```toml
[conversations]
mirrors = 1          # runners that keep a copy of the server's store; 0: none
mirror_on = []       # runners to pick first, by name
keep_closed_days = 30  # days an ended agent's transcript is kept (its events stay); 0: for good
keep_archived_days = 365  # days its archive in kiln's store of bytes is kept after its last close; 0: for good
compact = true       # make the store's file compact once a third of it is free (runners and mirrors too)
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
  MB. They are kept compressed, and ended agents' transcripts go after `keep_closed_days`. What
  that frees is used again for what comes next, and once a third of the file (and 256 MiB) is
  free, the store makes it compact while it has nothing else to do (`Engine::compact_some`):
  1. It copies itself into `<file>.compact.tmp` a step at a time: the writer reads 4 MiB of rows,
     and a thread of its own writes them into the copy, a transaction of their own there, under
     the same numbers, the copy's log written into it each time. Two batches ahead at most.
  2. Then what was written meanwhile, by its numbers, in the order it was written: what a drop
     took away is taken away from the copy too, and a session changed without a number of its
     own (where it runs) is copied again.
  3. Caught up, the copy goes to disk and is counted against the store, table by table.
  4. With nothing else writing, the rest is copied, the copy counted again, and it takes the
     file's place.

  A step holds the store for a few tens of milliseconds (it only reads; the copy's disk is the
  copier's to wait on). Counting the store before the swap, and the swap, hold it for up to a
  second or two on a busy disk. The memory it takes is a few steps'. A copy written whole before a crash
  takes the file's place at the next start; one written in part is removed. It needs room beside
  the file for what the file holds, and waits until there is. `compact = false` turns it off. It
  once copied the whole file in one go (`VACUUM INTO`, one transaction), which took more memory
  than the first server had: it was killed for it every minute until rolled back (2026-10-18).
- **A runner's first feed** sends its whole store, everything it kept before the feed.

## Status

| Part | State |
|---|---|
| C1 store | live (1a59fad, 2026-10-18): the Turso engine, each runner's store of its sessions, the server's store of every session, the writer, restore from the store, search and paging API |
| C2 replication | live (dfbbd1a, 2026-10-18): revisions, the feed on its lane, the harness's record followed, epochs, resets that keep history. Lab with a real Claude Code: the server's store matches the runner's and the transcript line for line; a server killed mid-turn catches up; a runner killed comes back and goes on |
| C3 resume elsewhere | live (dfbbd1a, 2026-10-18): Move falls back to the server's store when the machine is away; the transcript lands where Claude Code looks; the copy left behind lets go; away agents listed after a server restart. Lab with two runners and a real Claude Code: the conversation goes on with its context. Built since (e1bb7f2): every harness (ACP agents and Antigravity with their conversation in words), and a gone machine's agents moved on at once (Mission Control's Move them on). Lab (`~/.cache/kiln-mcp-lab/recover_e2e.py`, 12 checks): an ACP agent's runner killed outright, its agent moved on to the other runner, which answers from the conversation it was given |
| C4 search UI | live (dfbbd1a, 2026-10-18): Mission Control's filter box finds what was said across every agent; an agent's find bar lists the matches in its earlier history. Checked against the UI mock on desktop Chromium, and on phone-sized Chromium and WebKit. Quick on big stores (4be95ec): newest first by an index, a slice at a time; on 1M events a common word in a few ms, a word none says in about 5 s, other reads meanwhile waiting at most 75 ms (they waited the whole 10 s and failed before) |
| C5 mirrors | live (dfbbd1a, 2026-10-18): the server picks runners to keep a copy of its store and feeds them; a copy restores the server's store. Lab with two runners and a real Claude Code: the source runner killed and the server's store deleted, the store restored from the mirror, the agent listed away on its machine, moved on, its conversation going on with its context |
| Compression | live (2540397, deployed with 2bd78af, 2026-10-18): transcript lines of 256 bytes or more kept brotli-compressed, older ones compressed in the background under the same numbers; feed batches compressed to a taker that says it takes them. Labs C2, C3 and C5 pass on compressed stores |
| Pruning and compaction | pruning live (902b999, deployed with 5360484, 2026-10-19): an ended agent's transcript goes after `keep_closed_days`. Compaction went live with c98ae28 as one copy of the whole file in one transaction (`VACUUM INTO`) and took the first server down: out of memory on its store of several GB, every minute, until rolled back. The fix (9b7558f, live with 5360484) took it out, and a server answers nothing rather than "from 0" when it can't read where a runner's store stands. Built again (6ca8494): a copy a step at a time, written on a thread of its own, under the same numbers, caught up by them, counted, then swapped. On a 4 GB store (1M events, 76k compressed transcript lines), capped at 2 GiB with a runner feeding it meanwhile: made 2.8 GB in 3 to 5 minutes, identical after (integrity ok), memory under 140 MB, reads 2 ms median and 0.15 to 0.46 s at p99, none failed; the swap held the store 0.3 to 1.4 s |
| Restore from a mirror | live (5360484, 2026-10-19): a server that starts with an empty store asks a mirror named in `mirrors.json` for its copy, or `POST /api/conversations/restore` asks one; the copy fills in what the store lacks and never overwrites what a machine's own feed brought. Lab C6 (12 checks): the store deleted and the agent's machine killed, the server restores by itself, the agent is listed away and moved on with its context; a second restore changes nothing. At scale (a 4 GB store: 40 sessions, 75,764 transcript lines kept compressed; the server and the mirror each capped at 2 GiB like the pod): restored by itself in 18 seconds, identical row for row, the server at 107 MB and the mirror at 93 MB at most, no OOM (122 seconds before the feed read only what a batch takes, sent lines as kept, and stopped sending the copy back). Mixed versions (an old server with new runners, and the other way) pass labs C2 and C6 |
| Telemetry | built 2026-10-19, not deployed: each store's counters and file, the feeds' positions, runners' reports, the server's board with each store's series and notes, `kiln_convo_*` on `/metrics`, and the System page's figure and dashboards. Lab (`board_e2e.py`): a server with an empty store and a runner whose 150 MB store feeds it. The board shows the runner's store catching up from 22,885 rows to in sync, the runner picked as the mirror and its copy caught up, series and notes, 38 `kiln_convo_*` lines on `/metrics`, and no errors in either log. Checked on the UI mock on desktop Chromium and phone WebKit |
