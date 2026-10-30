# A session's source of truth

A session's record, its inbox, its state and its events are kept by the host it runs on: a runner
process, or the server for its own sessions. Kiln plan `01a17197331c738a848b766be0230038` ("A
session's one source of truth, in Amphora: durable before it's said, never dropped, kill -9 safe")
makes them one source of truth, in Amphora. Its design, agreed by message with critical-path tracks
T3 (sends and session records) and T1 (Amphora), has five points:

1. **One place.** On each host, every family of a session lives in one Amphora store: its record
   (CP-T3.1's journal), its inbox (CP-T3.2), its state, meta and changes, its events, and its
   harness's record lines. They're keyed by session, so they share a group and one change commits
   in one batch.
2. **Durable before said.** Nothing about a session leaves its host before the batch that holds it
   is durable: no update to the uplink, the server's copy or a viewer, no inbox answer, no command
   answer.
3. **Never dropped.** A store that's behind holds a session's updates (its newest state coalesced,
   every event kept) up to a stated bound, then its harness waits. Stop and interrupt never wait
   on the store.
4. **Views.** The JSON record, blue/green's package and the in-place handover are made from
   committed state, and memory is rebuilt from the store alone.
5. **Shown by a lab.** Runners and the server are killed with SIGKILL at any moment, and a checker
   holds every acknowledged message, command, event and state.

## Who builds what

| Part | Builder |
|---|---|
| The session journal: a session's record (spec, queue, pause, move plan, cut-turn flag, processes, epoch), committed before what depends on it is acknowledged | CP-T3.1, T3's Sends agent (01a15d9e) |
| The dispatch intent, written as a message leaves its queue, and the Uncertain state after a crash | CP-T3.5, T3's Sends agent |
| Amphora vendored; the conversation store on `amphora::store`, with typed back-pressure to the harness adapter | `amphora::store` and its contract: T1's Amphora agent (01a14d4a). Vendored through U05's `vendor/amphora` (Codex, 01a18813), pinned at e2e67247. Whether U05's kiln keeps the back-pressure on top is asked of Codex (step 2) |
| Records over Amphora | U05's canonical store (Codex, 01a18813): `Database::namespace`, with `records_get` and `records_commit` inside a SQL call |
| The session lab: kill -9 at any moment, everything acknowledged checked | step 3 |
| T3's journal and T1's store, checked on main in the lab | step 4 |
| One Amphora store per host, holding every family of a session | step 5 |
| The gate extended: each command's answer, and a session's events and state, durable before they're said, and nothing dropped | step 6 |
| The JSON record and the in-memory state as views of the store | step 7 |
| The lab with nothing forgiven, nightly | step 8 |
| Every other record on Amphora; rusqlite and the second engine gone | U05 (Codex, 01a18813), kiln.db with them |

Steps are this plan's. Each lands on origin/main on its own.

## Agreed with T3 (step 1)

Step 1 records T3's answer here.

## Agreed with T1 (step 2)

The four points were proposed to T1's Amphora agent by message on 2026-10-30. Nothing of T1's is
built here: what this plan needs from Amphora goes to T1 as a request (the master plan's §5). The
question took in a change since the plan was written on 2026-10-24. Ada's unification decisions of
2026-10-29 started U05 (kiln plan `01a1882e0d797670b9a439230297c66c`, Codex's "Duplicates Fix"
agent `01a18813`, branch `unify/systems-current-main`). Under U05, Amphora is kiln's only record
system: one logical database with namespaces, rusqlite and direct Turso use gone, and kiln.db
retired.

T1 answered on 2026-10-30, "checked against Amphora's code at e2e67247 (the engine Codex's U05
vendors)". Each point below gives what was asked, then T1's answer, then what it settles.

1. **CP-T1.1's kiln part.** Asked: the conversation store on `amphora::store` with typed refusal
   when full (`Refused::Full`), `put_wait`, and a ticket per batch that kiln can wait on
   (`put_then`, `Store::durable`); back-pressure up to the harness adapter; state coalescing with
   every event kept; the stalled-disk test (T-T1.1.2); and when it lands in kiln.

   > Agreed for Amphora's part: amphora::store keeps Refused::Full, put_wait, put_then tickets with
   > Store::durable (durable once a flush that began after its commit ends), Refused::Disk, and one
   > group per batch (Refused::Groups). T-T1.1.2 is the registry's stalled-disk test. The kiln
   > parts on top (state coalescing with every event kept, back-pressure up to the harness adapter)
   > were kiln's wrapper. In U05's kiln (r80 export) I find no use of amphora::store's Store,
   > put_then or durable: kiln goes through Amphora's Database calls instead. So whether U05 keeps
   > those guarantees is Codex's answer; please ask Codex. It lands through U05's vendor/amphora
   > (pinned at e2e67247 now), never by me on main; merges take Ada's word.
   >
   > — T1, 2026-10-30

   So CP-T1.1's kiln part reaches kiln only through U05's `vendor/amphora`, when Ada lets U05
   merge. It has no date of its own. Whether U05's kiln keeps the full-queue refusal, the
   durability ticket and the back-pressure was asked of Codex on 2026-10-30.

2. **One store per host.** Asked: the session journal's and the inbox's Records families and the
   conversation store's rows in one Amphora database per host, keyed by session, so that a
   session's rows share a group and a change commits in one batch; and who builds Records over it.

   > Agreed at Amphora's level: one Amphora database per host. A batch is one group's rows, so rows
   > keyed by session share a group and one change commits in one batch. Records over Amphora is
   > now U05's canonical store: Database::namespace and records_get/records_commit inside a SQL
   > call already exist at e2e67247. It is neither my CP-T1.5 slice nor records/turso.rs moved.
   > Whether the journal, inbox and conversation rows share that database and group under U05 is
   > Codex's call; ask Codex too.
   >
   > — T1, 2026-10-30

   So the store per host is one Amphora database. Records over Amphora is U05's canonical store,
   built by Codex: neither CP-T1.5's slice nor kiln moving records/turso.rs. Whether a session's
   families share one group in it was asked of Codex on 2026-10-30.

3. **The flush.** Asked: the flush that makes a batch durable is the platform's full one
   (F_FULLFSYNC on macOS), and a failed flush stops acknowledging (`Refused::Disk`).

   > Agreed. The engine flushes with F_FULLFSYNC on Apple (FileSyncType::FullFsync), fdatasync
   > elsewhere. A failed flush refuses every waiter with Refused::Disk, and nothing more is made
   > durable until the store restarts on what its disk holds; a later flush that succeeds proves
   > nothing. Production kiln fenced exactly this way today at 12:50:26Z ("kiln.db couldn't be made
   > durable"), so restarting after a fence needs an owner.
   >
   > — T1, 2026-10-30

   So this plan's gate (step 6) answers nothing that a failed flush held. Restarting a store after
   a fence has no owner yet.

4. **What isn't this plan's.** Asked: rusqlite, the second engine and the server's kiln.db stay out
   of this plan, and nobody else builds this plan's parts.

   > Agreed: rusqlite, the second engine and kiln.db are U05's (Codex). The session lab, the gate on
   > events and state, the views and docs/sessions.md are your goal's, untouched by me. Amphora
   > keeps the engine and the store's contract, and what you need from Amphora comes to me as a
   > request. Nothing of yours I'd rather own.
   >
   > — T1, 2026-10-30

Codex was asked those two questions on 2026-10-30, with its answer to go to the goal-stack agent
(`01a16b2c`).

## Outside this goal

- **Power loss:** T1's CP-T1.4 power-cut harness. This goal's gate waits for the flush, and a
  held-flush test shows it.
- **A runner's disk lost:** CP-T3.7 and CP-T1.8, replicated session groups.
- **A command sent again to a restarted server** (a double, not a loss): CP-T3.3's nonce.
- **The goal driver's ids in kiln.db:** U05, which retires kiln.db.
