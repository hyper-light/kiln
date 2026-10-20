# KilnDB: every runner a node, every session a Raft group

KilnDB is kiln's conversation store (`docs/conversations.md`) grown into a distributed database:
every runner is a node, every session is replicated by a Raft group of its own across machines, and
kiln's events move on an event bus. It is measured against clustered MySQL (Vitess) on kiln's own
workload, latency and throughput, and has to beat it.

## Where it starts

- **Every machine already keeps its own store.** Each runner keeps its sessions in its own Turso file.
  The runner sends the server what it wrote after the number the server has (log shipping, not
  Raft). The server keeps every session, and one runner keeps a copy of the server's store (a mirror).
- **What that lacks:**
  - Durability rests on the server's disk and one mirror. It doesn't come from a quorum.
  - Nothing is sharded: the server holds everything, and every search runs there.
  - A write waits up to `COMMIT_MS` (50 ms) to share a commit: good for throughput, poor for latency.
  - Runners only talk to the server; even reach's runner-to-runner links are relayed through it.

## The design

Focal's ledger is the model (`~/Projects/focal/docs/archictecutre/04`, `24`, `27`): its unit of
replication is a session, and many sessions' groups share one log on each node.

- **A node per runner.** Every runner (and the server) runs a KilnDB node. A node hosts replicas of
  sessions: their rows in its Turso file, their Raft logs in one hyper-log shared by every group it
  hosts (hyper-durable's shell, as Focal's `shell_node` and mantle's range replicas run it).
- **A session is a group.**
  - **Voters.** Each session's rows are replicated by its own Raft group (hyper-raft core) with
    three voters on three machines (2f + 1, f = 1), chosen by placement.
  - **Leader.** The session's home runner, where its agent runs, is its preferred leader, so a write
    commits on its own disk and one more. When a session moves to another machine (`docs/agents.md`,
    moves), leadership moves with it: a leader transfer, not a copy.
  - **Quiet groups.** A group with nothing to say quiesces: no heartbeats for ended sessions.
- **Sharding is placement.**
  - **What a node holds.** The sessions' groups are spread over the nodes: a node holds the sessions
    it leads or follows, not everything.
  - **The directory.** Partitions over `hash(session)` (Focal's `NamespaceRange` over a blake3 key),
    each a Raft group of its own, record where each session's voters are. They split above a few
    thousand sessions and merge when small (Focal's 2,048 and 256).
  - **The root.** A root group names the partitions; the server and two stable nodes vote in it.
- **Placement.**
  - **Choosing.** Voters go on distinct machines, ordered by stability (how long a machine stays
    connected: a laptop that sleeps ranks low, as mirrors are picked today), then free disk, then load.
  - **Healing.** A new voter joins as a learner, is promoted once it has caught up, and voters change
    one at a time. A machine away longer than a hold (twice the election timeout, then policy) has
    its seats healed elsewhere; when it comes back, it is a learner again.
- **Writes, local first.**
  - **Committing.** A session's updates reach its leader and commit at a quorum. With the leader on
    the session's own machine, that means its disk plus one follower's.
  - **Group commit.** It is adaptive: a commit goes at once when the log is idle and batches under
    load (hyper-log's pipeline frames), never a fixed 50 ms wait.
  - **Without a quorum.** A runner that can't reach a quorum (a laptop on a plane) keeps writing to
    its local outbox, today's own store, and proposes from it when the quorum is back. Nothing an
    agent says is lost or refused for want of a network.
- **Reads.**
  - **A session.** Read from the local replica when the node holds one: a follower read after
    ReadIndex, or stale-ok for the UI. Otherwise from the leader.
  - **Search.** Search fans out to one replica of each partition's sessions and merges newest first,
    a slice at a time as today.
- **Transport.**
  - **Direct links.** Nodes talk directly over QUIC (hyper-transport), with mutual TLS on the
    runners' certificates.
  - **Relay.** The server's relay is the fallback for nodes that can't reach each other (NAT, no
    tailnet).
  - **Appends.** Appends travel ordered per group and are reordered on receipt, as in Focal's
    `RaftOrdered`.
- **The event bus.**
  - **Taking it in.** Focal's bus (`focal-bus`) is taken into kiln: pub/sub over ordered streams,
    lossless backpressure, replay from what memory holds.
  - **What moves on it.** Session updates go from harness to store and UI on it, as do CI events
    and the UI's live feed.
  - **Durability.** Its replicated class is Focal's step 3, not yet built. KilnDB's durability comes
    from its Raft groups; the bus moves events.
  - **Prerequisite.** kiln moves its hyper-raft pin from 767627f to at least a8f5941, which the bus
    needs (`Classes::lossless`, `Endpoint::hold_lane`).

## Benchmarks

- **Where.** On fedora (32 cores, Samsung 9100 PRO NVMe), never the machine production runs on.
- **The harness.** `bench/kilndb`, outside the workspace, so its MySQL client never enters kiln's
  build. It runs one workload with a driver per database:
  - preload every session's history;
  - writers appending events durably while readers read sessions' latest pages;
  - search across every session.

  Each run prints throughput and latency percentiles.
- **Vitess.** `bench/kilndb/vitess/cluster/up.sh`: one host, every tablet its own mysqld, keyspace
  `convo` sharded by session (xxhash). There are four shards of three tablets, durability
  `semi_sync` (a commit waits for a replica, as a Raft quorum of three does), and every commit is
  fsynced (`innodb_flush_log_at_trx_commit = 1`, `sync_binlog = 1`).
- **Against what.**
  - KilnDB as it is (one store, in process): its engine's ceiling and today's commit latency.
  - Then the cluster: three nodes on the same host as Vitess, then across machines for both.
- **The bar.** Lower p50 and p99 write latency and higher write throughput than Vitess at the same
  durability, history reads at least as fast, and search faster.

## Milestones

| | What | Done when |
|---|---|---|
| K0 | The harness, Vitess clustered, and baselines | numbers for Vitess (semi-sync, fsync) and today's store on the same workload and host |
| K1 | The bus in kiln; hyper-raft moved to the bus's revision | session updates flow harness → bus → store; the UI's live feed on the bus |
| K2 | A session group on one node: hyper-durable shell, shared hyper-log, Turso state machine, adaptive group commit | single-node write latency below Vitess's; kill -9 loses nothing |
| K3 | Three nodes: placement, the directory, direct QUIC with the relay as fallback, local-first outbox | a session's write commits at a quorum across machines; kill -9 of any node loses nothing; a laptop offline keeps writing |
| K4 | Reads and search across the cluster; the UI and API on KilnDB's client | the UI reads sessions and searches through the cluster |
| K5 | Healing and movement: learners, promotion, leader transfer on session moves, partition split and merge | a machine lost for good is healed around; a moved session's leader follows it |
| K6 | Performance against Vitess, and the figure (System page) drawn from the cluster | the bar met on the same host and across machines; the figure shows nodes, groups, leaders, and the bus live |
| K7 | Migration from the feed and the mirror; deployed | production on KilnDB, the old feed retired |

## Status

| Part | State |
|---|---|
| K0 | first numbers (2026-10-19, fedora; fedora was loaded by other builds near the end of the KilnDB run, so both are to be run again quiet). 1,000 sessions of 100 events, then 30 s of 32 durable writers (one event a write) and 4 readers, then searches. **Vitess** (4 shards × 3 tablets, semi-sync, fsync): preload 49.5k events/s; writes 1,214/s, p50 24 ms, p99 74 ms, p99.9 202 ms; history reads p50 2.0 ms, p99 6.5 ms; search p50 147 ms. **KilnDB today** (one store, in process): preload 11.4k events/s; writes 195/s, p50 159 ms, p99 269 ms; reads p50 159 ms, p99 279 ms; search p50 23 ms. The gap is the store's one writer thread, which every read waits behind, and its 50 ms commit; search is 6× faster already |
