# kiln without a server

> **Approved** (2026-10-22), with three decisions taken: three nodes by default on Kubernetes, the
> CA and secrets sealed in shares two of three voters open, and the phases without a server first.
> How voters are chosen and elections held is [Voters and elections](#voters-and-elections),
> researched as asked. It is built in the phases at the end, each tested and live before the next.

Today one process is special: the **server**. It serves the API and the UI, keeps the store
(runs, jobs, repositories, people, settings, deployments), takes the conversation feed from the
runners, polls GitHub, schedules jobs, drives deployments and rollouts, votes in every synced
repo's group, relays runner-to-runner links, and holds the CA. Runners only run things. When the
server is down, kiln is down, and installing kiln means installing a server and then runners.

The end state has no server role. **Every runner is a node, and every node is a whole kiln**: the
API, the UI, the stores, the scheduler, the runner. A person opens any node's address and sees
and does everything. kiln's records live in **Amphora**, replicated in groups whose voters are
the steadiest machines; its bytes in **Mantle**. Work that must happen once (polling a
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
| The fleet's records | the server's SQLite | Amphora's **root group**, a replica on every node |
| Conversations | runner feeds, the server's copy | Amphora: a group per session |
| Runs, jobs | the server's store | Amphora: a group per CI repository |
| Deployments | the server's store | Amphora: a group per deploy environment |
| Logs, artifacts, bundles, images | the server's disk and content store | Mantle: chunks on nodes, metadata replicated |
| Polling, scheduling, deployments, rollouts | the server's loops | **leases**: one holder each, any node |
| Terminals, agents' harnesses | the runner's | the node's (unchanged) |
| Synced repos | the server votes | voters on nodes |
| Links | through the server | direct between nodes; relayed by any node both reach |
| The CA, join codes | the server's key | the root group's, sealed |

## State, by kind

- **Amphora's root group.** People, accounts, tokens, roles, settings, registered repositories,
  secrets (sealed, below), goals and plans, and the fleet itself: its nodes, their certificates,
  addresses and roles, and the leases. Small and read everywhere, so every node holds a replica:
  voters as [chosen below](#voters-and-elections), the rest learners. A node reads its own replica;
  a write goes to the group's leader over the node links and returns once committed.
- **Amphora's other groups.** One per session (conversations), one per CI repository (runs, jobs,
  steps), one per deploy environment, and directory partitions for search, as Amphora's design
  has them. Each places its own voters by the same rules, its leader where its work is: a session's
  on the node its agent runs on, so a write commits on that machine and one more.
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

1. **Every node is a member; voters are few.** Every node holds a learner replica of the root
   group: it reads locally and votes on nothing. Three of them vote (five past ten eligible nodes,
   at most seven). A fleet of one runs every group with one voter, as Amphora's one code path has
   it.
2. **Who may vote (candidacy).** A node is eligible when it is all of:
   - **steady:** up at least 99% of the last seven days, measured by the root group from the
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
added to it (with its own suites and simulations). Amphora's placement (CP-T1.9) carries the
rules, for the root group and every other group; kiln shows them on the System page: each node's
role, eligibility and why, and the groups' voters and leaders.

## Work that happens once: leases

A lease is a row in the root group: what it's for, the node holding it, until when, and its
epoch. The holder renews it; when it lapses (the holder gone for twice the renewal), any node may
take it, with the next epoch. Every write a lease's work makes carries its epoch, and the group
refuses a write with an older one, so two holders never both act.

| Lease | One per | Was |
| --- | --- | --- |
| `poll` | repository | the server's GitOps poller |
| `schedule` | CI repository group | the server's scheduler |
| `deploy` | deployment app | the CD controller |
| `rollout` | the fleet | update rollouts |
| `notify` | the fleet | notifications and webhooks out |
| `mirror` | external store | backups, Tailscale and DNS upkeep |

A webhook can arrive at any node: it is written to the root group, and the `poll` lease holder
acts on it. Leases prefer steady nodes with what the work needs (a Kubernetes context for
`deploy`, the network GitHub is on for `poll`).

## Jobs

A run's jobs wait in its repository group's queue by their `runs-on` labels. A node with free
slots that matches claims a job (a compare-and-set through the group's leader), runs it, and
heartbeats its claim; a claim that lapses goes back on the queue, as a lost agent's jobs do today.
Kubernetes jobs are claimed by a node that holds a context for the cluster, which starts the job's
pod.

## The API and the UI on every node

- **Reads** come from the node's own replicas: a list may be a moment behind (a follower read), a
  page that must be current waits for the leader's index (ReadIndex) first.
- **Writes** go to the leader of the group they touch, over the node links.
- **Live updates** come from the node's replicas as they apply entries, on Amphora's change feeds.
- **Node-local things** (a terminal, an agent's harness, a job's live log) are reached through the
  node that has them, over the links, as reach does now.
- **Signing in** works on every node: sessions and tokens are in the root group, and cookies are
  signed with a key it keeps.
- **One address:** the front door answers on any node. With a domain (Tailscale's, a custom one),
  its name resolves to every node that serves it; a node that goes away drops out.

## Trust

- **The CA** is the root group's: its key sealed in shares, one with each voter, any two of three
  (three of five) opening it. A new node's certificate is a request committed to the group and
  signed by the leader once the shares open the key. When voters change, the shares are dealt
  again before the old ones are dropped.
- **Joining** is `kiln join` as today (a code, or a cluster found on the network): the new node
  gets its certificate, joins the root group as a learner, and fills its replica.
- **Secrets** (tokens to GitHub, registries, clusters) are sealed the same way and opened only on a
  node that needs them for a lease's work.

## Links

Nodes talk directly over QUIC (hyper-transport, mutual TLS on their certificates), found through
the root group's address book, the local network (mDNS) and Tailscale. Two nodes that can't reach
each other relay through any node both can (the steadiest first), not a fixed server.

## Installing

`kiln install` installs a whole node, on the target the person picks, and leaves it running:

```
kiln install                                 # Kubernetes, the current context (the default)
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

## Getting there from today

Production keeps working at every step; each phase ships, is checked live, then the next starts.
Amphora's own goals (CP-T1.8 replication, CP-T1.9 placement) are its track's; the phases below take
each as it lands.

| Phase | What | Done when |
| --- | --- | --- |
| P1 | kiln's records on Amphora, one voter, on the server: the store's tables through kiln-seams' `Records` | the API reads and writes through Amphora; a restart replays it; nothing else changes |
| P2 | Runners as learners: each holds the root group's replica, serves the UI and API from it, and forwards writes | the UI works through any runner's front door, its reads served by that runner |
| P3 | Voters on runners, chosen and replaced as [above](#voters-and-elections) (Amphora's replication, CP-T1.8; the witness and shared detectors in hyper-raft) | the server stopped, kiln still serves and writes; a voter lost for good is replaced in minutes |
| P4 | Leases: polling, scheduling, deployments, rollouts, notifications | with the server stopped, a push still builds and deploys |
| P5 | Links without the server; relays on any node; trust (sealed CA, joining) in the root group | two runners link directly; a new node joins with the server gone |
| P6 | `kiln install` on Kubernetes (default), Docker and a machine, as whole nodes | each target installs a working node from nothing, in one command, checked on each |
| P7 | Amphora's other groups (sessions, repositories, environments) and Mantle under the nodes | conversations, runs and bytes no longer go through one machine |
| P8 | The server role retired: production runs as nodes | no code path names a server; production's minikube runs three nodes |
