# kiln without a server

> **Approved** (2026-10-22), with three decisions taken: three nodes by default on Kubernetes, the
> CA and secrets sealed in shares two of three voters open, and the phases without a server first,
> Amphora and Mantle under them after. How voters are chosen and elections held is
> [Voters and elections](#voters-and-elections), researched as asked. It is built in the phases at
> the end, each tested and live before the next.

Today one process is special: the **server**. It serves the API and the UI, keeps the store
(runs, jobs, repositories, people, settings, deployments), takes the conversation feed from the
runners, polls GitHub, schedules jobs, drives deployments and rollouts, votes in every synced
repo's group, relays runner-to-runner links, and holds the CA. Runners only run things. When the
server is down, kiln is down, and installing kiln means installing a server and then runners.

The end state has no server role. **Every runner is a node, and every node is a whole kiln**: the
API, the UI, the stores, the scheduler, the runner. A person opens any node's address and sees
and does everything. kiln's records are replicated in groups whose voters are the steadiest
machines: first the **cluster group**, kiln's own, then **Amphora**'s groups under it; its bytes go
to **Mantle**. Work that must happen once (polling a
repository, a deployment's sync) is held by a lease that another node takes over when its holder
goes away. kiln works while a quorum of voters is up, and a node cut off from the rest (a laptop on
a plane) keeps working on its own sessions, folders and jobs, and catches up when it's back.

This builds on what's designed already: Amphora, the store for every record kiln keeps (vendored at
`vendor/amphora`, reached through kiln-seams' `Records`; its replication and placement are kiln
plan T1's CP-T1.8 and CP-T1.9), Mantle for bytes ([mantle.md](mantle.md)), sync's voters on
machines ([sync.md](sync.md), phase 4), `kiln join` and discovery ([architecture.md](architecture.md),
"Home network"), reach's links between runners ([reach.md](reach.md)) and every runner's front
door ([front-door.md](front-door.md)).

## A node

One binary, one process per machine (`kiln node`, which `kiln agent` and `kiln server` become):

| Part | Today | A node |
| --- | --- | --- |
| API and UI | the server's | every node's, from its own replicas |
| The fleet's records, runs, jobs, deployments | the server's SQLite | the **cluster group**, a replica on every node; Amphora's groups at P7 |
| Conversations | runner feeds, the server's copy | today's feed and mirror; Amphora's session groups at P7 |
| Logs, artifacts, bundles, images | the server's disk and content store | Mantle: chunks on nodes, metadata replicated |
| Polling, scheduling, deployments, rollouts | the server's loops | **leases**: one holder each, any node |
| Terminals, agents' harnesses | the runner's | the node's (unchanged) |
| Synced repos | the server votes | voters on nodes |
| Links | through the server | direct between nodes; relayed by any node both reach |
| The CA, join codes | the server's key | the cluster group's, sealed |

## State, by kind

- **The cluster group.** Everything the server's store keeps (people, accounts, tokens, roles,
  settings, registered repositories, runs and jobs, deployments, secrets sealed as below, goals and
  plans) and the fleet itself: its nodes, their certificates, addresses and roles, and the leases.
  One hyper-raft group, as each synced repo's is (`crate::sync::group`), with the store kiln has as
  its state machine (SQLite, a file on each node), so kiln's records move whole, not table by
  table. Every node holds a replica: voters as [chosen below](#voters-and-elections), the rest
  learners. A node reads its own replica; a write goes to the group's leader over the node links and
  returns once committed.
- **Amphora's groups (P7).** Amphora takes the cluster group's records over as its root group, and
  splits off a group per session (conversations), per CI repository (runs, jobs, steps) and per
  deploy environment, with directory partitions for search, as its design has them (kiln plan T1).
  Each places its voters by the same rules, its leader where its work is: a session's on the node
  its agent runs on, so a write commits on that machine and one more.
- **Bytes.** Mantle: logs, artifacts, images, bundles and backups, chunked over nodes' disks, with
  its metadata replicated on three nodes. A job's log is written where the job runs and goes to
  Mantle as it's written.
- **Per node.** Metrics and Observe's samples stay on the node that took them; a chart over many
  nodes asks each. Terminals, harness processes and caches are the node's own.

## Voters and elections

kiln's fleet is not a rack of servers: it has desktops and servers that stay up for weeks, laptops
that sleep and roam, Kubernetes pods that are rescheduled with new addresses, and machines that
share one host (minikube's pods on adacachy). Raft (hyper-raft) is safe in all of it. What it leaves
to its owner is **who votes**, **who leads**, and **when to replace a voter**, and that's where a
fleet like this one goes wrong.

### What goes wrong, and how others answer it

| Shortcoming | Answer elsewhere |
| --- | --- |
| A voter that sleeps or flaps costs the group its margin, and a member that comes back can disrupt it with elections | Pre-vote and check-quorum ([Kafka KIP-996](https://cwiki.apache.org/confluence/display/KAFKA/KIP-996:+Pre-Vote)); a vote refused within the minimum election timeout of hearing a leader (Ongaro's dissertation, §4.2.3). hyper-raft has all three, and elections by suspicion (`docs/timing.md` §2.9) |
| A member promoted before it's caught up stalls commits | Non-voting learners promoted once caught up ([etcd's learners](https://etcd.io/docs/v3.3/learning/learner/)); a stabilization time before a new server votes ([Consul Autopilot](https://developer.hashicorp.com/consul/docs/manage/scale/autopilot), 10 s by default). hyper-raft catches a learner up in rounds (R13) |
| Voters on one machine or one site fail together | Replicas spread by failure-domain labels and an isolation level ([TiKV/PD](https://docs.pingcap.com/tidb/dev/schedule-replicas-by-topology-labels/)); redundancy zones with a non-voter promoted in the same zone ([Consul](https://developer.hashicorp.com/consul/docs/manage/scale/autopilot)) |
| A voter gone for good leaves the group one failure from stopping | Its replicas re-made elsewhere after a deadline: five minutes ([CockroachDB](https://www.cockroachlabs.com/docs/v21.1/remove-nodes), `time_until_store_dead`), thirty ([TiKV](https://docs.pingcap.com/tidb-in-kubernetes/dev/use-auto-failover), `max-store-down-time`); a dead server removed once its replacement is healthy ([Consul](https://developer.hashicorp.com/consul/docs/manage/scale/autopilot)) |
| Changing voters trips the group | One voter at a time, adding before removing ([Kafka KIP-853](https://developers.redhat.com/articles/2024/11/27/dynamic-kafka-controller-quorum), [MongoDB](https://www.mongodb.com/docs/manual/tutorial/adjust-replica-set-member-priority/)); joint consensus, and the election rule across changes hyper-raft settled (`docs/research/reconfiguration.md` in hyper-raft) |
| The leader lands on a slow or unsteady member | Election priority, a member of priority 0 never a candidate yet still a voter ([MongoDB](https://www.mongodb.com/docs/manual/tutorial/adjust-replica-set-member-priority/)); leaders that follow locality (Amphora's design). hyper-raft has priority and leader transfer |
| Too few steady machines for a quorum | Witnesses: voters that hold the log but not the data, never lead and serve no reads ([Spanner](https://docs.cloud.google.com/spanner/docs/replication)); smaller replication quorums where election quorums still meet them ([Flexible Paxos](https://arxiv.org/abs/1608.06696)) |
| Thousands of groups each watching each other | Failure detection between machines, shared by every group on them ([CockroachDB's store liveness](https://www.cockroachlabs.com/docs/stable/architecture/replication-layer), [the paper](https://emptysqua.re/blog/review-scalable-leader-leases-for-multi-consensus-groups-in-cockroachdb/)); a suspicion level that adapts to each link, not a fixed timeout (the φ accrual detector, Hayashibara et al., 2004) |
| Voters named by address break when pods move | Stable ids, never hostnames, in the voter set (a reported KRaft failure, KAFKA-20427) |

kiln today picks its conversation mirrors by how long a runner's connection has lasted, then free
disk, and replaces one after a day away (`crate::convo::mirror`). A desktop that just restarted
ranks below a laptop that hasn't slept yet: the measure is too short.

### What kiln does

1. **Every node is a member; voters are few.** Every node holds a learner replica of the cluster
   group: it reads locally and votes on nothing. Three of them vote (five past ten eligible nodes,
   at most seven). A fleet of one runs every group with one voter, as Amphora's one code path has
   it.
2. **Who may vote (candidacy).** A node is eligible when it is all of:
   - **steady:** up at least 99% of the last seven days, measured by the cluster group from the
     nodes' own liveness, not by a connection's age, with a planned restart (an upgrade, a move)
     not counted against it;
   - **not portable:** a machine on battery power or that has slept in the last week is never a
     voter on its own (it can be a witness, below);
   - **caught up:** a learner caught up in rounds (hyper-raft's R13), then stable for ten minutes
     (Consul's stabilization time, longer, as kiln's machines are);
   - **not pinned out:** a person can pin a node as a voter or keep it from voting (Settings ·
     Fleet), as MongoDB's `votes` and kiln's `mirror_on` do. Pins win; nothing else needs choosing.
3. **Which eligible nodes vote.** Spread first: no two voters on one host (minikube's pods on
   adacachy share adacachy's host label, as do VMs that say so), then across sites (networks,
   Kubernetes zones); then the steadiest; then the fewest suspicions in the last week; then the
   most free disk and the lowest latency to the others.
4. **Who leads.** Election priority follows the same order, so the steadiest, nearest voter leads,
   and a leader that comes back after an outage gets leadership back by a transfer, not an
   election. Amphora's other groups lead where their work is (a session's on its agent's node).
5. **Replacing a voter.** A voter the failure detectors suspect for **five minutes** is replaced:
   the best eligible learner is added as a voter first, then the lost one removed, one change at a
   time through joint consensus. A planned restart doesn't start the clock; a node that announces
   it's going to sleep or shutting down hands its seat over before it goes. A replaced node comes
   back as a learner, and isn't made a voter again for an hour, so a flapping machine can't make
   the group churn.
6. **Too few steady machines.** With fewer than three eligible nodes, the third seat goes to a
   **witness**: a voter that keeps the log, never leads, holds no state and serves no reads. Any
   machine can be one (a laptop, a small pod), as it only needs to be up to vote and to count
   toward commits while a full voter is away. Two witnesses never sit beside one full voter.
7. **One detector per pair of machines.** The failure detectors run between machines, adapting
   to each link's own delays, and every group on those machines takes their verdict: a session
   group doesn't watch its peers itself.
8. **Ids, not addresses.** A voter is its node id (its certificate's key), whatever its address:
   a pod rescheduled with a new address keeps its seat.

hyper-raft has what this needs but the witness role and detectors shared between groups; both are
added to it (with its own suites and simulations). kiln's cluster group keeps the rules first;
Amphora's placement (CP-T1.9) carries them for its groups after. kiln shows them on the System page:
each node's role, eligibility and why, and the groups' voters and leaders.

## Work that happens once: leases

A lease is a row in the cluster group: what it's for, the node holding it, until when, and its
epoch. The holder renews it; when it lapses (the holder gone for twice the renewal), any node may
take it, with the next epoch. Every write a lease's work makes carries its epoch, and the group
refuses a write with an older one, so two holders never both act.

| Lease | One per | Was |
| --- | --- | --- |
| `poll` | repository | the server's GitOps poller |
| `schedule` | CI repository | the server's scheduler |
| `deploy` | deployment app | the CD controller |
| `rollout` | the fleet | update rollouts |
| `notify` | the fleet | notifications and webhooks out |
| `mirror` | external store | backups, Tailscale and DNS upkeep |

A webhook can arrive at any node: it is written to the cluster group, and the `poll` lease holder
acts on it. Leases prefer steady nodes with what the work needs (a Kubernetes context for
`deploy`, the network GitHub is on for `poll`).

## Jobs

A run's jobs wait in the cluster group's queue by their `runs-on` labels. A node with free slots
that matches claims a job (a compare-and-set through the group's leader), runs it, and
heartbeats its claim; a claim that lapses goes back on the queue, as a lost agent's jobs do today.
Kubernetes jobs are claimed by a node that holds a context for the cluster, which starts the job's
pod.

## The API and the UI on every node

- **Reads** come from the node's own replicas: a list may be a moment behind (a follower read), a
  page that must be current waits for the leader's index (ReadIndex) first.
- **Writes** go to the leader of the group they touch, over the node links.
- **Live updates** come from the node's replicas as they apply entries.
- **Node-local things** (a terminal, an agent's harness, a job's live log) are reached through the
  node that has them, over the links, as reach does now.
- **Signing in** works on every node: sessions and tokens are in the cluster group, and cookies are
  signed with a key it keeps.
- **One address:** the front door answers on any node. With a domain (Tailscale's, a custom one),
  its name resolves to every node that serves it; a node that goes away drops out.

## Trust

- **The CA** is the cluster group's: its key sealed in shares, one with each voter, any two of three
  (three of five) opening it. A new node's certificate is a request committed to the group and
  signed by the leader once the shares open the key. When voters change, the shares are dealt
  again before the old ones are dropped.
- **Joining** is `kiln join` as today (a code, or a cluster found on the network): the new node
  gets its certificate, joins the cluster group as a learner, and fills its replica.
- **Secrets** (tokens to GitHub, registries, clusters) are sealed the same way and opened only on a
  node that needs them for a lease's work.

## Links

Nodes talk directly over QUIC (hyper-transport, mutual TLS on their certificates), found through
the cluster group's address book, the local network (mDNS) and Tailscale. Two nodes that can't reach
each other relay through any node both can (the steadiest first), not a fixed server.

## Installing

`kiln install` installs a whole node, on the target the person picks, and leaves it running:

```
kiln install                                 # Kubernetes, the current context (the default; without one, this machine)
kiln install --on kubernetes --context lab   # another context
kiln install --on docker                     # a container on this machine
kiln install --on machine                    # this machine itself, as a service
kiln install --join <code>                   # join a kiln, rather than start one
```

- **Kubernetes:** a StatefulSet of three nodes (`--nodes`), each with its own volume, spread over
  the cluster's nodes (anti-affinity, the zone label as their site), a Service, and the front door.
  The chart in `deploy/` becomes the node's.
- **Docker:** one node in a container (`--restart unless-stopped`, a volume for its data, its ports
  or the host's network).
- **Machine:** a user or system service (systemd, launchd, a Windows service), as the runner is
  installed now, with the UI and stores in the same process.
- **First or joining:** the first install starts a cluster; later ones join one (`--join`, or one
  it finds on the network, which it asks to confirm).
- **Done means it works:** install waits until the node answers on its address, the UI loads, and
  the node is in the cluster, and says where to open it. It never ends half installed: a failure
  says what failed and leaves nothing running.

**Built ahead of the phases** (`crates/kiln/src/install_targets.rs`, today's server as the node):
`--on kubernetes` (the default where a kubeconfig names a cluster, else `--on machine`; one
Deployment until P3 makes it three, a NodePort service, its admin token a secret, an image built
for the nodes' CPU, from the installing binary or the static Linux build its kiln server has, and
loaded into minikube, kind or k3d, shared with Docker Desktop's and OrbStack's, or pushed to
`--registry` or the registry the cluster names), `--on docker` and `--on machine`. Each ends once the kiln answers,
its UI's page and script load and its runner is connected (`/healthz/server`), and otherwise takes
back what it made. `--join` comes with P5.

## Getting there from today

Production keeps working at every step; each phase ships, is checked live, then the next starts.
The phases without a server come first (P1–P6); Amphora and Mantle go under them after (P7), as
their own tracks land.

| Phase | What | Done when |
| --- | --- | --- |
| P1 | The cluster group on the server alone: the store as its state machine, seeded from today's | the API reads and writes through the group; nothing else changes; a restart replays it |
| P2 | Runners as learners: each holds the group's replica, serves the UI and API from it, and forwards writes | the UI works through any runner's front door, its reads served by that runner |
| P3 | Voters on runners, chosen and replaced as [above](#voters-and-elections), with the witness and the shared detectors in hyper-raft | the server stopped, kiln still serves and writes; a voter lost for good is replaced in minutes |
| P4 | Leases: polling, scheduling, deployments, rollouts, notifications | with the server stopped, a push still builds and deploys |
| P5 | Links without the server; relays on any node; trust (sealed CA, joining) in the cluster group | two runners link directly; a new node joins with the server gone |
| P6 | `kiln install` on Kubernetes (default), Docker and a machine, as whole nodes | each target installs a working node from nothing, in one command, checked on each |
| P7 | Amphora under the nodes (its root group takes the cluster group's records; session, repository and environment groups; kiln plan T1) and Mantle for bytes (T10) | conversations, runs and bytes no longer go through one machine |
| P8 | The server role retired: production runs as nodes | no code path names a server; production's minikube runs three nodes |

### P1, as built

kiln.db's writes go through the cluster group: one hyper-raft group, its log at
`<data>/cluster/cluster.log`, with the store as its state machine (`crates/kiln/src/store/group.rs`).
On the server alone the group has one voter, which commits alone.

- **A write** (`Store::call`):
  - It runs as it always has, in a transaction of the store's connection (`BEGIN IMMEDIATE`, so what
    it reads is what it writes over), while SQLite's pre-update hook notes the rows it touches
    (`store/capture.rs`).
  - When it's done, those rows are read as it left them: what it commits, with the parts it took
    back, or that failed half way, left out.
  - It commits with its number (`seq`, in `cluster_state`), and its net change goes to the group as
    an entry. The log writes the entries that came meanwhile together, one flush for all of them.
  - The caller's answer comes once kiln.db holds the write, as before the group, and is as durable:
    kiln.db commits without waiting for the disk. Measured on adacachy's NVMe, a small write took
    104 µs at the median and 305 µs at p95 through the group, against 48 µs and 120 µs without it;
    the log caught up 18 ms after the last write. A read is only a read.
- **A restart replays** the log's writes kiln.db doesn't hold: those whose commit the disk lost while
  the log's flush kept them. An entry applies when its `seq` is past kiln.db's, and is skipped
  otherwise. Its rows are written whole by key, so applying one twice leaves what once does.
  - The group starts with a barrier, an entry of its own, so the store serves only once that's done.
  - The other way, kiln.db holds writes the log didn't take (a stop before they reached it). kiln.db
    is whole and the log goes on from it; a learner takes a copy of the database (P2).
  - On first start, today's database is the group's state at index 0, write 0.
- **Rows keep their rowids.** A change carries each row whole with its rowid, so a replica is the same
  database, not just an equal one. kiln uses rowids as identifiers: a run's is its GitHub run id, and
  a job's orders its legs. SQLite's sessions were the other way, and they renumber rows.
- **The machine applies** on a connection of its own, foreign keys off, since a write's cascades are
  among its rows already.
- **Transactions nest.** A write that needs its statements together opens a `store::tx` (a savepoint)
  rather than rusqlite's transactions, which can't open inside the write's own.
- **This node's own tables** (its log search index, built from its own logs) are written past the
  group with `Store::call_local`; a write through the group that touches them is refused.
- **Images:** on one node the image is kiln.db itself, so the log compacts behind what's applied and
  durable, every 8,192 writes or 128 MiB. A learner's first copy of the database is P2's. Writes
  forwarded by learners (P2), and a group with voters (P3), answer once committed.
- **Seen live:** `/healthz/server`'s `store` says whether writes go through the group, kiln.db's last
  write, and the log's last entry.
- **Off:** `[cluster] group = false` (or `KILN_CLUSTER_GROUP=0`) writes straight to kiln.db as before.
  The database is whole either way. `KILN_STORE_GROUP=1` runs the tests' stores through the group.

### P2, as built

Each runner holds a learner replica of the cluster group, and its front door answers the owner's
reads from it (`crates/kiln/src/store/learner.rs`, `crates/kiln/src/frontdoor/replica.rs`,
`crates/kiln/src/reads.rs`).

- **Joining.** A runner that can hold one says so (`CAP_CLUSTER`). The server adds it as a learner,
  one change at a time through the group, and tells its newest process which member it is (an id
  from its machine's name) and the token its copies are fetched with. Other processes of the
  machine let go of theirs, so one process at a time holds the replica's files. A newest process
  whose older one holds them yet (finishing its last jobs before it retires) tries again until it
  lets go: 15 s apart at first, then each minute, with the server's newest word to join.
- **A copy, then the log.** A learner starts from a copy of kiln.db, `GET /api/cluster/copy`, made
  with `VACUUM INTO` at the group's last applied entry. The server's own log index stays behind. Any
  copy made later stands in for that point: an entry its `seq` holds already is skipped, as a restart
  skips them. So a learner the log has moved past (a runner away while the log compacted) is sent an
  image that only says where the group stands, and fetches a fresh copy. It takes that copy in whole,
  with SQLite's backup, while its readers go on. One that meets a write numbered past the next (a
  write the log lacks: the server stopped before it got there) takes a fresh copy before it applies
  anything after it. A replica kept by a build from before that, which went on past such a gap
  without the write, is copied again as its learner starts: `cluster_mark` says where this build
  last left it, and a replica standing anywhere else is copied. A learner touches its replica only
  once no other process has it open: a machine's older runner process lets its learner go as the
  newest takes it ("One runner a machine", `docs/releases.md`). While it has learners, the server
  keeps up to 64 MiB of the log behind what's applied for one a little behind, and beats once a
  second.
- **Kept following.** Each minute the runner checks its replica: one whose group stopped, or that
  stays behind the server's last write for five checks running, is stopped and started again,
  saying why (nothing else would start it again, and its front door would leave every read to the
  server).
- **Messages.** The group's messages go over each runner's link on the sync lane
  (`ServerMsg::Cluster`, `AgentMsg::Cluster`), in parts past 512 KiB. A learner whose runner is away
  misses them, and the log sends what it needs once it's back.
- **Reads.** While a runner holds a replica, its front door serves requests one by one instead of
  piping each connection to the server. The owner's `GET` of the runs, a run, a job and the
  repositories, and the CI pages' own (`/api/ci/runs`, a run with its legs, its graph), is answered
  from the replica, saying so with `kiln-read: <runner> <seq>`. Before
  answering, the runner asks the server for kiln.db's last write and waits until its replica holds
  it (2 s at most, else the server answers). So a read there is as current as the server's: a run
  the page is told of has arrived. Everything else goes to the server as it came, WebSockets
  included: anyone's but the owner's (the server holds the route table), writes, and what lives
  in the server's memory. Whether credentials are the owner's, the server says (`/api/cluster/owner`),
  and the runner keeps its word for 30 s. The server's routes answer from the same code
  (`crate::reads`), so an answer is the same wherever it's read.
- **Seen live:** `/healthz/server`'s `store` counts the learners and gives kiln.db's size.
- **Off:** `[cluster] learners = false` (or `KILN_CLUSTER_LEARNERS=0`) adds none, and each runner,
  as it reconnects, is told to let its replica go: its front door pipes to the server again. The
  learners the group has stay in it, and lag. A replica that didn't catch up within its 2 s leaves
  every read to the server for the next 30 s.
