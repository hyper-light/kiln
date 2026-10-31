# Resolved transactions through hyper-durable (U15, CP-T1.8)

Kiln's unification decision on 2026-10-29 calls for one logical Amphora database, with namespaces
rather than application-managed databases. A namespace is not a replication group. Atomic SQL and
Records state must not be split across independent groups.

A client SQL session can make adaptive decisions, invoke triggers and choose random/time defaults.
Replaying its SQL on followers would repeat those decisions and can produce different rows. The
leader therefore executes the session inside an unpublished engine transaction, captures its dirty
page images (including spilled pages), and rolls back. A Raft entry contains only the resolved
pages and the index/term of the state the session observed. An entry with a stale base is refused;
its index still advances durably, without applying its pages. The owner serializes preparations
until their proposals resolve. User answers held beside those proposals must only leave after the
matching applied result succeeds.

`replication::Machine` implements the existing hyper-durable StateMachine. It inserts imported
pages without commit markers, writes the applied index/term/membership in the same engine
transaction, commits and invokes the engine's real WAL flusher. Only then does apply return an
answer to hyper-durable. All entries act at restart, so hyper-durable's durable-commit fence covers
them. The Raft log is hyper-log through hyper-durable GroupStore; no Raft algorithm is implemented
in Amphora. A speculative preparation never publishes a local commit, so a leader that loses its
term cannot leave that preparation visible after restart.

The replicated owner admits an application request with an unknown leader into its existing
bounded waiting queue, retaining its original absolute deadline and admission charge. It dispatches
only after discovering a leader. If the term or leader changes before application evaluation,
the same untouched operation returns to the front of that queue and must pass a new context and
ReadIndex. This deliberately replaces immediate unknown-leader refusal with bounded waiting;
expiry returns Timeout, or UnknownOutcome if the client's existing receive deadline wins.

The retry boundary ends before `prepare_db` begins and before a SQL session's `FnOnce` is taken.
An evaluated session is never replayed, including one that fails, captures no pages or cannot
propose. Proposed changes and bootstrap seeds retain their UnknownOutcome semantics on leadership
loss, displacement or deadline. A stale read completion cannot turn a proposal into a new operation.
Queue/core-read admission can still refuse work at its existing bounds; no deadline is extended.
Application admission keeps one maximum wire record's bytes free within the existing shared byte
budget, so waiting calls alone cannot prevent replication traffic from entering the same actor.
Already admitted wire traffic can still fill that bounded budget and cause an honest refusal.

The implementation currently retains the engine WAL for atomic application of committed Raft
pages. It does not yet eliminate that apply journal in favor of the Raft log alone. Transactions
are limited to 32 MiB of resolved pages, snapshots to 256 MiB. Larger snapshots are explicitly
refused until a streaming snapshot protocol exists. Snapshot installation atomically imports the
image and applied marker, flushing before hyper-durable may move the log's start.

The host must still wire this machine to the hyper-durable Replica, GroupStore and a bounded
transport owner, driving quorum ReadIndex before serving reads. `Database` currently remains the
local-durable API; its writer must be replaced by that owner before replicated operation is
advertised. No application writer may use the same database alongside a Replica-owned Machine.
Membership changes and recovery use hyper-durable; the host must not synthesize committed indices
or infer quorum durability from a local flush. Missing owner/transport integration is not an
optional degraded mode and is not U15 completion.

Evidence: the existing hyper-durable contract states the durable commit fence, configuration and
snapshot ordering in its [StateMachine and Replica implementations](https://github.com/hyper-light/hyper-raft/tree/4d72e19b9533851e19602eee57053a5a7337b2cd/crates/hyper-durable/src).
Raft's state-machine safety and log compaction rules are described in
[Ongaro's dissertation, chapters 3 and 5](https://raft.github.io/raft.pdf).
The engine's current transaction dirty-page set, spill completion and raw WAL insertion form the
physical capture/apply boundary; `db::Db` preserves that boundary instead of replaying SQL.

Falsified by: any unproposed captured change surviving restart; two replicas applying one entry
with different random/default/trigger output; an acknowledged applied index reopening without its
rows or membership; a stale preparation changing state; or snapshot installation making a new
index durable before every image page. The focused persistence tests exercise these boundaries;
quorum, leader-loss and host integration remain separate required gates.
