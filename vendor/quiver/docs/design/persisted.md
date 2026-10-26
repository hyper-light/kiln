# The persisted class on Focal's replicated part (kiln's CP-T2.2, step 4)

Status: 2026-10-25, decided for CP-T2.2 step 4 (kiln plan `01a16c5b7e8c75acbdfafd51315c3885`).
How Quiver serves topics of the persisted class on Focal's replicated part, what it changed in
Focal's copy, and what its tests hold. CLAUDE.md §5: each decision with its sources, its evidence
and what would falsify it. The code is `crates/quiver/src/persisted/`.

## What the class promises

Focal's doc 33 (§4.2): a persisted publish is acknowledged once a majority of its group's nodes
have synced it to disk. Focal's replicated part (doc 34, `focal_bus::member`) gives exactly that
for a group that states no fates: its commit counts a majority's durable logs, hyper-durable's
`GroupStore` over hyper-log on the node's device, the full flush (the write-ahead log mandatory,
`docs/design/README.md`). Quiver's node checks, as it opens each member, that the member's class
is `Durability::Persisted`, and refuses to open one that would acknowledge anything less.

## Each topic a part

A persisted topic is a part of its own: a Raft group of every node of the cluster, each node's
member of it an endpoint and a log of its own. Three nodes run every topic's part on all three;
one node runs every topic's part as a group of one, durable on its disk (kiln on one server).

- **Why a part a topic, not a stream a topic in one part.** Focal's part holds many streams, and
  one part for every topic would be one log, one leader and one disk's stall for all of them:
  Kafka's partitions and Focal's own design (a node serves a part, doc 32) put each log under a
  leader of its own so that a slow or failed group holds up only its own topics. Partitions of a
  topic (several parts each) and their placement are CP-T2.6's.
- **What it costs.** Each part a node serves is two UDP sockets (its member's and its
  publisher's) and a log of about twice the part's memory (`hyper_log::Config::derive`). The
  cluster bounds its topics (`PersistedConfig::topics`).

## The catalog

Every node must open the same parts for the same topics, so the topics a cluster has are decided
once, in order, by a part of their own that every node replicates: the catalog. Its records are a
topic created (with its settings), a topic deleted, a node serving a topic's part at a port, and a
node's start. Every node applies them in the order the catalog's group committed them, so what a
create or a delete did is decided by what came before it at every node alike (`catalog.rs`, a
pure core). A topic's incarnation is the offset of the record that created it: deleted and
created again, it's a new part whose offsets start at 0 (QUIVER-17). Kafka's KRaft keeps its
metadata the same way, as a log replicated by a quorum that every broker applies (KIP-500,
KIP-631).

- **Ports.** A topic's member listens on the node's host at a port the operating system picks
  as the part is first bound, kept in the part's directory and bound again at every start. Each
  node records its port in the catalog, and a node opens its member once every node's port is
  recorded.
- **A topic a node doesn't hold yet.** A create is answered by the node that asked once it has
  applied it; another node may not have yet, and a node back from a restart has applied only what
  its catalog's log held. So a node asked for a topic it doesn't hold makes a mark through the
  catalog and looks again once it has applied it, everything committed before it: it refuses
  `NoTopic` only then, and `NoQuorum` where the catalog has no leader. A node makes a mark as it
  starts too; its figures say when it has applied it (`PersistedStats::settled`). (Without the
  mark, a publish through the second node right after a create through the first was refused
  `NoTopic`: the concurrent-publishers test, on fedora.)
- **A bound.** The catalog's part keeps what its window holds, as any part (Focal's doc 34 P10):
  its records are tens of bytes, so a catalog of the least part memory holds some hundreds of
  thousands of them. A node that finds its catalog let go of records it never applied stops,
  saying so. Snapshotting the catalog is open (STATUS.md); the catalog proper, its schemas and
  access rules, is CP-T2.5's, in Amphora.

## Publishing

Each node holds a publisher for each part: a Focal client on an endpoint of its own, a session of
its own (the node above, its run below: no two nodes or runs share one) with a sequence for each
batch, one Quiver record a batch (`ephemeral.rs`'s framing).

- **Redirects.** A node that doesn't lead the part answers a publish with Focal's typed redirect,
  naming the leader it knows and its term; the publisher takes a leader only from a term past the
  one it knew (KIP-951's rule). A node that answers nothing for 500 ms while batches are owed is
  left for the next: a leader killed -9 says nothing, and its connection ends only at the
  transport's idle timeout, two minutes.
- **Once, in order.** A batch once sent is sent again, in order, until its part has committed it:
  as the publisher's leader changes, its connection comes up, the part says the session waits for
  an earlier batch, or a resend period passes. The part's sessions recognise a batch they took
  and answer it again from their state, never appending it twice (doc 34 P6, Kafka's idempotent
  producer, KIP-98). A batch is never dropped once sent, so a session never has a gap.
- **No quorum.** A publish no leader could be found for within the node's quorum wait is refused
  `Refusal::NoQuorum`, never having been sent: nothing of it is kept.
- **The answer.** A publish is answered once the node's own member has applied it, at the offset
  the part gave it: committed, so on a majority's disks, and found by a read on the node that
  published it.

## Reading what's committed

Every member applies what its group committed to its part, leader and followers alike. Each node
keeps a view of each topic: the ephemeral class's log (`ephemeral.rs`), fed only what the node's
member applied, in order, from the oldest its part keeps. Reads, waits, bounds and subscriptions
are served from it as for the ephemeral class, so the contract's retention, lag notices and stamps
hold alike: a stamp is the batch's, or the topic's last if that's later, the same at every node. A
node whose part was given its leader's image past what it had applied tells its readers what they
missed. A view holds what the node's memory holds; after a restart it holds what the part's window
kept.

## Connections between members

A member dials every other as it opens: members open as each node learns its peers' ports, and a
node back from a restart is dialed by no one, its peers' connections to it having ended with its
process unknown to them. (Focal's rule, the lesser dials, made a fresh topic's first election take
1.5 to 11 s on a Mac Studio, against 0.6 s.) A peer the member's liveness stream suspects, having
heard from it, has its connections closed and is dialed again: the transport replaces a
connection only once it carried nothing for its serve period (two minutes), and a node goes on
sending on the connections of a peer that was killed, both of an identity's places
(`CONNECTIONS_PER_CLIENT`) taken where each side dialed the other. Without it a peer back from a
kill was refused every connection it dialed (`Refusal::Busy`, seen in the kill drills below) and
never caught up.

## What was changed in Focal's copy

One change, recorded in `vendor/focal-bus/UPSTREAM.md` and offered to Focal: a member's liveness
stream takes room for one peer where its group has none, so a group of one opens (hyper-liveness
refuses a stream of no peers; hyper-raft elects a sole voter on its own vote).

## Evidence

- quiver-api's conformance suite, QUIVER-1 to 19, passes for the persisted class on a node that is
  a cluster of one and on three nodes in one process (`crates/quiver/tests/persisted.rs`); each
  node's concurrent publishes are answered at their own offsets.
- Kill drills, three nodes in one process (`tests/persisted_kill.rs`) and three processes killed
  with SIGKILL (`tests/persisted_processes.rs`): the leader, a follower and the publisher's own
  node, each killed at every frame its logs write in a run of publishes, then started again; every
  acknowledged publish is at its offset in every node's log, nothing appended twice, the three
  logs alike.

**What would falsify it.** An acknowledged publish missing from, or twice in, a node's log after
any kill; a node's publish answered with an offset its payload didn't take; a read of a topic the
cluster has refused `NoTopic` by a node that caught up; the suite failing on one node or three.
