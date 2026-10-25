# Sends: each message once

A message sent to an agent reaches it once, whatever sent it again: the page that reconnected, the
agent that retried, the plan run, the goal driver or the network. This is the critical path's
CP-T3.2 (track T3, "sends never lost or doubled"); CP-T3.1, CP-T3.3 to CP-T3.5 build on it (what's
open, below).

## One id, made once

Every message carries an id made once by whoever sends it, and every hop and every retry carries
that same id: to the server, to the runner, into the session's queue, its timeline (a user event's
`client_id`) and its harness (Claude Code's `uuid`).

| Who sends | The id |
|---|---|
| A conversation's composer | a UUIDv7 made as you send it, kept with a message held for an agent that's away until it goes |
| Mission Control's message box (`/api/harness/broadcast`) | each agent's made from the message's key, its caller and the agent; the panel sends a message again under the key it first had when the first try didn't come back |
| `agents.send` (MCP `agents_send`, `kiln.agents.send`, its HTTP route) | made from its `key` (the `Idempotency-Key` header over HTTP), the caller and the agent; without a key, new on each call. The caller is an agent by its id (its name says the machine it's on), so its retry after it moves is the same message |
| `POST /api/harness/sessions/:id/command` | the `send`'s own `id`; with an `Idempotency-Key` header, made from that, the caller and the session |
| `POST /api/harness/command` | each agent's made from `key` (or the header, else the command's `id`), the caller and the agent |
| A plan run | the run's id is made first (from `plans.run`'s `key` and its caller when it has one, in every mode: asked for again, it's the same run; asked again under the key for other steps or in another mode, refused), and each of its messages' ids from it (a step's, or a round's of steps side by side). The plan's page and an agent's Plan tab make a key for each run asked for, and ask again under it. A step tried again (`plans.runs.retry`) names the try it replaces: asked again once the step was tried since, nothing more starts |
| Goals added (`goals.add`: the goals panel's Add, MCP `goals_add`, its HTTP route) | not a message itself: each goal is made under an id from its `key` (or the `Idempotency-Key` header), the caller, the agent, its place and its words, so asked again the goals it added are found and none is added twice (each goal's message is then the goal driver's); without a key, new goals each time. The goals panel makes a key for each goal it adds. A goal split again into the same parts (`goals.split` asked again) adds nothing |
| The goal stack's controls (`goals.control`, `goals.done`) | not messages themselves, but each can send the next goal. Started or resumed while it runs, nothing is sent again. Done or skip naming the goal (the goals panel names it), asked again once it has ended, ends nothing more. Without a name, the caller's own end of a moment ago asked again (its same words, within two minutes, whatever others ended since; an agent asking still in the turn that made it, told by its harness's turn then and now on its own machine's clock, never against the server's; a person or another agent only when it said what shows it) is answered with the stack as it is. Each control and end can carry a `key` (or the `Idempotency-Key` header): asked again under it, it's answered with the stack as it is and does nothing more, whatever changed since (paused again, a goal ended); a stack keeps its newest 64 asked under a key. The goals panel, Mission Control and the held card make a key for each one asked for. A keyed `goals.add` asked again is answered with the stack as it is too: paused since, it stays paused |
| The goal driver | kept on the goal before it's sent, and kept until it gets there: a send refused (its session on its way elsewhere), or to a machine that's away, goes again under it, and so does one after a restart in between, so one that got there is answered as its repeat. Let go of once it's delivered; a goal reworded is another message, with an id of its own. Bringing back cleared goals keeps it, on the goals still in the stack and on those brought back: a send never heard answered goes again under it, since a clear can't withdraw it; a goal that was under way when it was cleared goes to the agent again at once, a send of its own. A goal that goes as an edit of the stack's word waiting in the agent's queue goes under that word's id, kept on the goal before the edit goes: sent again after a restart, it's that word's repeat. The word, edited and then taken, is said again only when the agent's runner says its harness was handed the words from before the edit (in the state that shows its queue, `State::given`, kept across the runner coming back; a runner from before this release: its timeline, a word gone from its queue waiting 5 s for it), never when the edit applied and was taken between two looks |
| News of the agents an agent started | made once for its one message of news, which is edited as more comes; made again only when the last one never reached the agent's machine, or its inbox refused it. What reached the agent is what its runner says its harness was handed, whatever kiln had the message say since (an edit too late, or one the agent's own read of a child's result made): an edit's lines go again only when it was handed the words from before the edit, as for the goal driver's word (a runner from before this release: its news gone from its queue, its handing not heard of yet, waiting 5 s for it) |
| A program's notice | made from the notice: the agent it's for, when it came and its words |
| A goal planned now by a subagent (`goals.plan` asked by someone other than its agent) | made from the plan and when it was last sent: asked for twice at once, one message; again within half an hour, it isn't sent again |
| `kiln agents send --key` (a kiln shell's socket) | made from the key and the agent: the key is the machine's user's (only they reach its socket); an agent's goes to the server as `agents.send` |
| A session's first prompt | made from the session's id |

A skill run (the console's palette and tools, a `/name` that's a skill in Mission Control) is
delivered like a message, and goes through the inbox the same way: the console makes its id, and a
key makes it as it does a send's. A message from another agent is its words: kiln's note of who sent
it (its name and machine) isn't part of what the inbox compares, so a retry under the same key after
its sender was renamed or moved is the same message.

## The inbox

Each session's inbox is on the host it runs on: the server for its own sessions, else its runner.
It keeps its records in the host's `records.db`, beside its record of its sessions, through
kiln-seams' `Records` (docs/seams.md): kiln's embedded Turso until Amphora's (CP-T1.5), each commit
synced before it's answered. What's sent to a session goes down its lane, in the order it came; a
message goes through its inbox before the session gets it:

- **The first time,** the inbox takes it (a synced commit) and the session gets it.
- **Every time after,** the sender gets the first time's answer, marked `repeat`, and the session
  gets nothing. Once its session went on elsewhere, the answer is what the machine it's on keeps of
  it: the time and epoch its id came there at, and its place there.
- **Another message under an id already used** is refused `reused`, never taken as its repeat. An
  id a session brought from its timeline (moved, or resumed from the server's store) isn't known
  here by its words, which its timeline may keep otherwise than they came: whatever comes under it
  is its repeat.

**Fenced by epoch.** A session's epoch counts the times it went on on another machine
(docs/conversations.md, "Fencing"). The server stamps each message with the epoch it last heard the
session is at, and the inbox refuses, naming both epochs:

| Refused | When | The server |
|---|---|---|
| `stale_epoch` | sent for an epoch the session has left | sends it once more, for the epoch named |
| `behind` | sent for an epoch this copy hasn't reached: it's a copy left behind | answers the sender |
| `moved` | the session went on elsewhere, or is on its way (a move began) | answers the sender: send it again |
| `reused` | another message under an id already used | answers the sender |
| `no_session` | the session isn't on that host | answers the sender |
| `full` | the session's lane holds 1,024 waiting | answers the sender |
| `unavailable` | its records can't be read or written now | answers the sender |
| `invalid` | an id that can't be kept (empty, over 128 bytes, a control character) | answers the sender |

**Moves.** As a move begins, a session takes no more messages: those it was sent and hadn't taken go
with it at the end of its queue, and those after are refused `moved` before its inbox takes them, so
sent again they go where it went. Its inbox is fenced first, before its conversation goes to the
machine it moves to. Then, once what its lane holds is done (a message its inbox took before the
fence, and the session then refused, is taken back by then), the newest ids its inbox took (its last
4,096) go with it, each with its digest; and what it holds is copied after it stopped taking
messages. So no id goes without its message, which a retry would find taken there and never
deliver. Its inbox can't be read, or its lane isn't done within 10 s, it doesn't go: the fence is
lifted, the move fails, and it takes messages again where it is. The fence is lifted, too, if that
machine refuses it. All of it gone and no
answer come (the link closed, or that machine took too long), it may run there now: its inbox stays
fenced, and it takes nothing here while it asks that machine again (after 5 s, then twice as long
each time, up to a minute) until it says. A machine remembers each hand-over it took (the newest
4,096, in its runner's state folder, where a blue/green upgrade's new build finds them too; one it
couldn't write down, the process that took it still knows, its newest 4,096 too) and answers one
sent again as taken, whatever became of the session since (it ended there, or went on elsewhere):
the move is done. Refusing it, it says it didn't take it, and the session stays and takes messages
again; refused by a machine from before this release after a try with no answer, it keeps waiting.
There, its inbox takes those ids as its own (another message under one is refused `reused` there
too), with the ids in its queue and its timeline (each once: the newest are kept), in the order they
came (those taken in in one commit keep it among themselves, so past the bound the oldest go first);
ids it can't take in yet (a write failed) wait, and it refuses messages (`unavailable`) until
they're in, each message trying them first. Its record keeps those waiting from the moment it's
here, so a restart before they're in brings them again (an upgrade in place too, its harness still
running), and they go with it if it moves on. A session resumed on another machine from the server's
store (its own machine gone, and its inbox with it) brings the ids of the last 4,096 messages the
server's copy of its timeline shows. A session that went on elsewhere and is restored on its old
machine (a stale copy) takes nothing there.

**Blue/green.** A session handed to the new build of its runner (another runner process of the same
machine, at the same epoch) brings its inbox's newest ids the same way, and refuses messages while
it's handed over. The new build takes no messages until it's promoted (each refused `moved`, and
sent again): if it never is, the runner it upgrades takes its sessions back once the new build is
seen gone (stopped by its supervisor, else killed: one that can't be stopped keeps what it took
over, and takes messages again a while later, never beside the runner it upgrades), and their
inboxes take messages again, what they took before still counting; so does that runner if it's
started again with them before then (a handover's fence, unlike a move's, is lifted). Its inbox
unreadable as a session is handed over, the session takes the ids it holds itself: its queue's, its
timeline's, and those it brought and hasn't taken in.

**One runner a machine.** A session an older runner process hands to its machine's newest
(docs/releases.md, "One runner a machine") goes as a move does: it takes no messages as it's handed
over, its inbox's newest ids go with it (the newest process keeps an inbox of its own, in its own
folder), and its inbox in the older one is fenced before it goes. How each try at handing it over
went is decided once for the machine, by whichever of its processes comes first, and kept in its
state folder (`handovers-decided.json`, beside its note of the hand-overs it took, under one lock,
synced before it's said or acted on): a newer process writes that it took the try over before
anything of it runs there, or that it didn't before it says so, and the older one, done waiting,
takes the try back (writes that it wasn't taken) unless one took it first. The try goes again under
the same name, so whichever process it reaches answers it as decided: one taken over where that
process doesn't run it is answered as taken over on the machine (the server doesn't take that
process for its holder), and one that process can't read or write the record for is answered that
it can't say, which isn't a refusal. So the server's word on a try, which may be an earlier try's,
come late, says only when to look. Refused, or not passed on (no newer process, or not all of it
went), the older process takes the try back unless it was taken over, and goes on with it, the
fence lifted; taken, it lets go. With no word (the link dropped, nothing came in 10 minutes, or the
newest went away once its whole package went to it), or the server's own refusal after a try that
went without one, it looks: while the try isn't decided, the newest may run it now, so it waits, its
harness down, its queue held, its inbox fenced, and its package goes again, 5 s on and twice as long
each time up to a minute, until it's decided. A process from before this release names no try, and
its word alone says, as before (the newest answers a package it took before as taken): with no word,
or a word that can't say, it goes on with the session, so while this release rolls out a session
handed over by a process from before it can still reach two harnesses, as before this release. A
newest from before it (a rollback) doesn't read the record, so the older one's taking a try back
doesn't bind it.
Whatever a package names, a machine's processes take one over at a time, under the same lock: two
given one package at once (a try sent again to a newer process still, or a move's sent again after a
blue/green upgrade) take it over once, the second finding the first's note. While a new build of the
machine's runner takes it over, the server asks for no hand-over and passes none on (its newest is
the process leaving then), and a process leaving answers one that reached it anyway as not taken,
written down for the try. With no newer process of the machine to take one (the newest went away,
and none came since), the server's word says so (`HandedOver`'s `none_newer`): the older process
takes the try back unless one took it before, as on a refusal, and goes on with the session; a
hand-over dropped as its older process went away is answered so once that process says it holds
the session again.

**Files.** A file uploaded for a message is kept after the message carries it, until it expires
(30 minutes; short of room, those messages carried already go first): a message sent again
carries it too, and reads as the same message, its repeat; and the server's sending once more for
the epoch named carries it.

**Bounds.** A session's inbox keeps the ids of its last 4,096 messages; past 4,096 and 256 more, its
oldest go, down to its newest 4,096, once the take that passed the bound is answered (not on the
answer's way), and a repeat of one older than that is taken as new. A lane holds 1,024 commands; the
server waits 10 s for an answer (then says the message is on its way), for 10,000 messages at once;
a host keeps 4,096 sessions' heads in memory (past that, one goes: read again as it's needed), and
counts a session's ids once a process, as it tidies, never on a take's way. A tidy or a move reads
at most 8,704 of a session's ids at once: one past that (its tidies failing) is tidied a read at a
time (the oldest of those read go, down to the newest 4,096 of them: none is among its newest), and
doesn't move until it's back under (its carry refused `unavailable`).

**Off.** `KILN_INBOX=off` runs a host without inboxes: messages go on as they come, unchecked, as
before CP-T3.2 (counted: `kiln_sends_unchecked_total`). A host whose records can't be opened runs so
too, saying why. Its answer says so (`unchecked`): sent again, a message there is another.

**On disk.** A take is answered once it's synced: on Linux an `fsync`; on a Mac a full sync past the
drive's cache (`F_FULLFSYNC`), which costs more there. Its check seeks the message's id by its key
in the records store, so it costs the same however many ids the host's sessions keep (a test reads
the store's query plans: `records/turso.rs`).

## Answers

The inbox answers every message it's handed (`Update::Inbox`, through the runner's link), and the
sender gets that answer:

| Sender | Taken | A repeat | Refused |
|---|---|---|---|
| `agents.send` | `{sent, message, accepted: {session, message, epoch, at, seq}}` | the same, `accepted.repeat: true` | 409 (an epoch, a move, a reused id), 404, 429, 503 |
| `POST /api/harness/sessions/:id/command` | 200, the same | the same | the same |
| The WebSocket | nothing more | nothing more | an `error` frame with the refusal, when it comes: the socket waits for no answer (live updates, and a stop sent next, go on) |
| A broadcast | `sent` | `sent` | `failed`, saying why |

`seq` is the revision its take was committed at: a later message to the session has a later one.

## On the wire

To a runner announcing `CAP_INBOX` (past `Build`'s 32 bits, in its `AgentMsg::Caps`, beside
`CAP_WIRE`: docs/wire.md), the server sends a message as `HarnessSend { session, epoch, json }`
(appended last, after the wire's and the cluster group's: postcard variants); the runner's process
that holds the session answers it. A runner from before inboxes gets `HarnessCmd` as before: its
messages go on unchecked, and no answer is waited for.

## Metrics

| Name | What |
|---|---|
| `kiln_sends_accepted_total` | messages a session's inbox took, the first time (the server's count, every machine) |
| `kiln_sends_repeated_total` | messages an inbox had taken already: answered, never delivered again |
| `kiln_sends_refused_total{reason}` | messages an inbox refused, by reason (the table above) |
| `kiln_sends_unanswered_total` | messages the server stopped waiting for an answer to |
| `kiln_sends_unchecked_total` | messages a host without inboxes handed on (`KILN_INBOX` off, or its records couldn't be opened): not checked for repeats |
| `kiln_sends_take_seconds` | how long an inbox took to take a message, on the host that took it (its buckets close near the drive's sync floor: 2, 3, 4, 5, 7.5 ms) |

The "Sends" row of the Kiln overview dashboard (deploy/grafana/dashboards/kiln-overview.json) shows
messages taken, repeated, unanswered and handed on unchecked, refusals by reason, and the inbox's
take time at p50 and p99 (Prometheus scrapes the server: its own host's). Observe reads every
host's.

## Evidence

- **The simulation** (`harness/inbox/sim.rs`, kiln-sim's way): originators that send again under
  the same id, a server that stamps epochs and waits, runners with the real inbox over the records
  simulator (kept across their restarts), and the network nemesis (frames dropped, duplicated,
  delayed out of order, links cut), with runner and server restarts, moves under way a while (the
  session refusing messages), and copies left behind by a runner restarted before its record of a
  session's leaving was written. It runs kiln's own code where a send is decided: the host's take
  of a message, a session's arrival and carry, and the registry's sending once more for the epoch
  named; and it injects faults into it: a session that can't take a message it's handed (its inbox
  takes it back), an inbox that can't be opened as its session arrives (it's opened as a message
  comes), one that can't be read as its session leaves (the move fails), and one that can't take
  in the ids its session brought (they wait, kept in its record), and a move's answer lost after all
  of it went (the machine it left keeps its inbox fenced, taking nothing, until it hears that it
  was taken over). 1,000 seeds in CI (`KILN_SIM_SEEDS` for more), and 10,000 and 100,000
  measured: no message reached a harness twice, and every one reached one. Bugs planted on purpose
  are found within 20,000 seeds, a message reaching a harness twice (the last, a move that leaves
  the ids its session brought behind, first at seed 12,470: a run of fewer seeds can't show that
  one, the twentieth adversarial check's): without inboxes, with moves
  that don't fence, that carry no ids, that go on without ids they can't read, or that go on where
  they were when their answer was lost, with a session that takes messages before the ids it
  brought are in, with a record that doesn't keep those waiting (a restart forgets them), and with
  a move that leaves them behind. A seed replays exactly. It's kiln-sim's way, not kiln-sim itself:
  CP-T6.1's kiln-sim reached main just before this did, and this simulation moves onto it next, now
  that both are there (agreed with T6). What it doesn't model, tested on its own: blue/green and its rollback, the hand-over to a
  machine's newest runner process (the host's tests, and two newer processes given one try), the
  lanes' timing, a session resumed from the server's store, the 4,096-id bound, each of kiln's
  senders' own rule for trying again (its originators always send again under the same id; the
  goal driver's, the composer's and the keyed operations' are each tested), the registry's waiters
  (one per id there), and sessions' queues (a message a session is handed counts as delivered, so
  a copy left behind that replays a queue it restored isn't seen there: its inbox is what's
  checked).
- **`bench send_path`** (`cargo bench -p kiln --bench send_path -- --dir <a folder on the disk>`):
  an inbox's take and a plain synced commit of one record (what a send costs once its session's
  record is journaled, CP-T3.1), message by message, the take first on one and second on the next,
  so the two meet the same disk and load. What the inbox adds is, run by run, the p50 and p99 of its
  takes less those of the plain commits (a difference of percentiles, which the criterion reads: the
  median run), with each message's own take less its plain commit (the paired difference) beside it,
  and every run. The criterion reads eight cases: one session at a time; a session that keeps its
  most ids, whose oldest go once a take's answer has gone (the next take waits for that, and its
  time counts it); the same on a busy host, eight sessions at their most ids, the others taking
  messages all along; on a session with a long history, 25,600 ids let go before, which its scans
  read through until a compaction; 32 sessions at once, their takes and their plain commits in whole
  phases apart, the order alternating, so a take's check never rides in a plain commit's shared
  write; the same with each at its most ids, tidying as the host does; takes across a compaction of
  the store's history; and a first take, its session's head not in memory. It reads p99, p99.9
  beside it, the median of 5 runs. Beside them: a session's opening, its ids counted (once a
  process, on its lane: a take waits on it only if it comes that moment), timed apart; a session's
  arrival with its most ids, taken in (its first message waits for it); a repeat, and the disk's own
  sync floor; it names the drive (its model, write cache and FUA), and refuses to record at a load
  average of 2 or more. Not in it: a head read again after another take came between (a conflict),
  the round trip to a session's runner; a burst to one session (its messages are taken one at a
  time, a synced commit each, so the k-th waits for the k before it: kiln's own senders send a
  session one at a time, each waiting for its answer); and a Mac (the bench can't read a Mac's load,
  so it refuses there; a Mac's commits ask for `F_FULLFSYNC`, a take's and a plain commit alike).
- **`bench send_e2e`** (`cargo bench -p kiln --bench send_e2e -- --dir <a folder on the disk>`): a
  send end to end on a host, from `Host::command` until its session hands the message to its
  harness, one session from its first message (one at its most ids is send_path's case). It uses
  only what main had before inboxes, so it builds on main too and measures a branch and main alike;
  it records the machine, the drive and the load as send_path does, and refuses the same way. Not in
  it: the round trip between the server and a session's runner. A send over the network adds it on
  main too, but now waits on it for the inbox's answer, where main answered once the message was on
  its way.
- **Recorded** on fedora with the change that lands CP-T3.2: its commit message says what it
  recorded (the machine, the drive and its write cache, the load before and after, what the inbox
  adds at p99 in each case and each run, the paired difference and p99.9 beside it, the flush floor,
  and send_e2e against main on the same machine), and at which commit. The figures go there rather
  than here so they're never older than the code they measure.
- **Tests:** `agents.send` twice with one key (one prompt reaches the harness, both calls get its
  answer), on the server's own sessions and through a runner of its own (`tests/sends.rs`); a
  message for a stale epoch refused naming both epochs, and sent again by the server for the one
  named; what a host took still taken after it restarts; a move's gathering, and a moving session
  refusing before its inbox takes; blue/green carrying the newest ids, the new build taking no
  messages till it's promoted, and a rollback taking messages again; a handover ending when its
  lanes wait on a sink nobody drains; a session back higher whose inbox is still fenced taking
  messages; a carry holding the newest ids, and an import keeping the newest distinct ones; a skill
  run's id from a key; a fellow agent's message as its words; a keyed send, and a keyed plan run,
  asked for again after their agent moved to another machine (the same message, the same run); a key
  given to another plan run refused; a plan run asked for again under its key in every mode, and one
  that failed before it did anything tried again; each send under an id told its own answer; an
  inbox's answer from another machine leaving the session where it runs; the goal driver's ids
  across a restart; kiln's registry passing every HarnessPort clause (what reached a session read
  from what its inbox took: the ids it was handed); a hand-over sent again answered as taken once
  its session left the machine it went to, after that machine's blue/green upgrade, and when that
  machine couldn't write it down, and a refusal saying it wasn't taken (`harness/inbox/faults.rs`);
  an upgrade in place keeping the ids a session brought and hadn't taken in; the UI's plan run sent
  again under its key (`ui/tests/tries.test.ts`); a blue/green handover whose inbox can't be read
  keeping the ids its session brought; goals added again under their key added once (`goals/ops.rs`,
  and over HTTP in `tests/sends.rs`), and a goal split again into the same parts split once; every
  plan run and goal the UI asks for carrying its key (`ui/tests/keyed-requests.test.ts`); goals
  added under their key with a start (asked again after a pause too), a resume, a done (asked again
  after another's end too) and a skip, each asked again, sending the agent nothing more and ending
  one goal (`goals/ops.rs`); an agent's end asked again matched to its turn by its own machine's
  clock, ahead of the server's or set back; a control or an end asked again under its key doing
  nothing more, the stack changed since; the stack's word, edited and taken between two looks, said
  once through kiln's real registry however its runner goes on (as it says, coming back, its
  resume's state before its events: `goals`), what its runner says was handed kept across an Open
  and a state that says less; a session past what's read at once tidied a read at a time and carried
  once it's back under; the hand-overs a process couldn't write down kept to their newest bound;
  every keyed operation telling its caller to pass its key (`ops.rs`); a step tried again for the
  try it replaced starting nothing more; the Turso store passing every Records clause, each of its
  reads planned on the index it needs (a row's versions by its key, a scan's on its keys, a family's
  from a revision on by revision).

## What's open

- **CP-T3.1, the session journal.** A session's queue is still kept in its host's JSON record,
  written half a second after it changes: a message taken and then lost to a crash before that stays
  taken, so its retry is answered, not delivered. And the other way: a message handed to its harness
  and then a crash before the record is written again comes back in the queue the record had, and is
  handed again (a second prompt). The journal commits the queue with the take, and the queue without
  the message once it's handed; a crash in the moment between the two is CP-T3.5's.
- **CP-T3.3.** Acknowledged end to end, and parked by the server for a runner that's away (today a
  message for a machine that's away is refused at once).
- **CP-T3.4.** The composer keeps a message only until its socket takes it.
- **CP-T3.5.** A crash between a message reaching the harness and the harness's own record of it;
  and, for a plan run of one agent per step, a crash between a step's agent starting and the run
  recording it (the step starts another).
- **Starting agents.** `agents.start` takes no key: a start retried is another agent, with its own
  first prompt.
- **An answer sent again after a server restart.** A runner keeps each session's updates until the
  server has them (docs/wire.md), an inbox's answers among them. A server that restarted before it
  had one gets it again: no one waits for it then, unless a retry of that very message does, and
  that retry is told the first time's answer, not marked a repeat (the message reaches its agent
  once either way). CP-T3.3 gives each send a nonce its answer carries.
- **Ending a goal without naming it, asked again late.** `goals.done` or a skip that names no goal
  and has no key, asked again more than two minutes after the caller's own end with the same words
  (or by its agent from a later turn), ends the goal under way then; a person's or another agent's
  that says nothing of what shows it ends the goal under way then too; and a person who means to end
  the next goal too with the same words, within two minutes, is answered as asking again: name the
  goal (its `id`) to end that one alone, as the goals panel does, or give it a key. A step tried
  again without its try (an older client, or a caller that leaves it out: the tool says to pass it),
  asked again, starts another agent; a send, a plan run or goals added without their key, asked
  again, are new ones (each operation's key says so, and to pass it).
- **A runner's first moment.** A runner says it has inboxes (`CAP_INBOX`) in the `Caps` it sends
  right after its `Build`: a message sent to it between the two goes as to a runner from before. Its
  inbox still takes it once, but for no epoch named (the fence can't refuse it for one the session
  has left), and the server waits for no answer (counted `unchecked`).
- **A session's lane.** Everything a session is told goes down its lane in order, a stop as well as
  a message: after a burst of messages, a stop waits for their takes, a synced commit each. A
  session that just arrived takes its first message once the ids it brought are in (up to 4,352: a
  synced commit each 400, timed beside the bench's cases).
- **Goals added again after they were cleared.** `goals.add` under its key finds the goals it added
  by the ids its key made: taken out of the stack since (cleared), they're added again.
- **Lost, not doubled.** A message its inbox took that's handed to its session's lane just as an
  upgrade in place freezes the session, or a move's ids read once its wait for the lane to settle
  (10 s) ran out with a message still on its way, is taken but never reaches the session: sent
  again, it's answered as its repeat. CP-T3.1's journal and CP-T3.5's reconciliation take these. So
  does a goal that went as an edit of the stack's word after the agent had taken that word (the edit
  too late), its server restarting before it noted it went: sent again under the word's id, it's
  that word's repeat, and its words reach the agent with the next reminder. And a goal whose first
  word was sent just before a restart, never heard answered, while that word still waited in the
  agent's queue: a goal put before it then goes as an edit of the word, under its id, so the word
  says the new goal, and the first, sent under the same id, is answered as its repeat, its words
  reaching the agent with its next reminder. And a session a machine's newest runner process wrote
  it took over from an older one, that then stopped before the session's record was written (or
  failed to take it over after, and couldn't write that down): the older process lets go of it, and
  it runs nowhere until it's resumed from the server's store.
- **Unsending a message its harness holds.** The conversation offers Unsend on a message it shows
  queued, which its harness holds (Claude Code's own queue) or the harness's adapter does (an ACP or
  Codex adapter's waiting list); its `queue_remove` takes back only what kiln's own queue holds, so
  that message still goes, once, though the conversation says it was unsent. Undo shows it again
  and sends nothing: sent again under an id of its own, it was a second prompt (the twentieth
  adversarial check's; `ui/src/views/harness/unsend.ts`). Taking such a message back, where its
  harness allows, is to come; an unsend that does take one back must let its id go for Undo to send
  it again.
- **A goal's word withdrawn while its harness starts.** The goal driver takes a goal's word back
  (the goal ended, cleared or put back) with `queue_remove` too, which can't take back a word a
  starting Codex or ACP harness's adapter holds: that word still reaches the agent, once, and the
  goal, brought back, is said again, as one put back always is (the twenty-first adversarial
  check's).
- **Goals given as a harness's own control.** A harness whose goal is a control of its own (Codex's
  `thread/goal/set`) is given it as a command, with no message id: started again after a server
  restart between giving it and noting it, the same goal is set again.
- **A blue/green rollback and a session's queue.** What a session's queue held as it was handed to
  the new build goes with it; if the new build hands one on to its harness and the upgrade is then
  rolled back, the runner it upgrades sends it again (CP-T3.1's journal keeps the queue with what's
  handed on).
- **A crash just after a blue/green handover.** A runner that crashes in the moment after it handed
  its sessions to the new build, before its record says so, and starts again takes them back
  (fenced, and its harnesses stopped) while the new build, not promoted, takes messages again after
  a while: both hold them. CP-T3.1's journal, which commits a session's leaving before it's done,
  closes the moment.
- **A crash mid-move.** A runner that crashes after fencing a moving session's inbox and before the
  machine it goes to takes it has the session back fenced: it refuses messages (`moved`) until it's
  moved again (the server's taking it back by itself is to come).
- **A copy left behind and its queue.** A runner restarted just after one of its sessions moved
  (before its record is written again), or while a move's answer is awaited, restores a copy of it
  whose inbox refuses messages; but what that copy's queue held went with the move too, and the copy
  hands it to its harness unless the server tells it to let go first: a second prompt for those.
  So does an older runner process restarted while it hands a session to its machine's newest, which
  may run it: the try it was on is forgotten, and a word that it was refused, come then, lifts its
  inbox's fence unless the machine's record says a try of it was taken over (one still undecided,
  its package on its way, can't be told). CP-T3.1's journal, its records committed as they change,
  narrows the moment; a copy that's let go before it starts, or reconciled, is CP-T3.5's.
- **A move that's never answered.** A session whose move got no answer waits, fenced, on the machine
  it left until the machine it went to answers: one that never comes back, or one from before this
  release that refuses it then, leaves it waiting, taking no messages (the server's settling it by
  itself is to come). So does a try at handing a session to a machine's newest runner process that's
  never decided while that process still runs (its package lost on the way, and no word comes back;
  with no newer process left, the server says so and the older one takes the try back), and one
  whose machine's record of its hand-overs can't be read.
- **A server's power loss.** The goal driver's message ids and plans.run's key claims are in the
  server's main store, which answers once kiln.db holds a write, before the disk does; its cluster
  group's log, flushed a moment later, keeps the rest (docs/p2p.md, "P1"). A power loss (not a
  restart) in that moment can take the last ones: a goal sent again then goes under a new id.
- **CP-T3.7.** A session resumed from the server's store brings the ids its timeline there shows: a
  message its gone machine took that never reached the server's copy is taken again if it's sent
  again.
