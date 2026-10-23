# kiln without a server

> **A design, for approval.** Nothing here is built yet. When it's approved, it is built in the
> phases at the end, each tested and live before the next.

Today one process is special: the **server**. It serves the API and the UI, keeps the store
(runs, jobs, repositories, people, settings, deployments), takes the conversation feed from the
runners, polls GitHub, schedules jobs, drives deployments and rollouts, votes in every synced
repo's group, relays runner-to-runner links, and holds the CA. Runners only run things. When the
server is down, kiln is down, and installing kiln means installing a server and then runners.

The end state has no server role. **Every runner is a node, and every node is a whole kiln**: the
API, the UI, the stores, the scheduler, the runner. A person opens any node's address and sees
and does everything. State lives in replicated groups whose voters are spread over the steadiest
machines. Work that must happen once (polling a repository, a deployment's sync) is held by a
lease that another node takes over when its holder goes away. kiln works while a quorum of voters
is up, and a node cut off from the rest (a laptop on a plane) keeps working on its own sessions,
folders and jobs, and catches up when it's back.

This builds on what's designed already: KilnDB for conversations ([kilndb.md](kilndb.md)), Mantle
for bytes ([mantle.md](mantle.md)), sync's voters on machines ([sync.md](sync.md), phase 4),
`kiln join` and discovery ([architecture.md](architecture.md), "Home network"), reach's links
between runners ([reach.md](reach.md)) and every runner's front door ([front-door.md](front-door.md)).

## A node

One binary, one process per machine (`kiln node`, which `kiln agent` and `kiln server` become):

| Part | Today | A node |
| --- | --- | --- |
| API and UI | the server's | every node's, from its own replicas |
| The store | the server's SQLite | the **cluster group**, a replica on every node |
| Conversations | runner feeds, the server's copy | KilnDB: a group per session |
| Runs, jobs, logs | the server's store and disk | run partitions; logs on the node that ran them, copied once |
| Bytes (artifacts, bundles, images) | the server's content store | Mantle: chunks on nodes, metadata replicated |
| Polling, scheduling, deployments, rollouts | the server's loops | **leases**: one holder each, any node |
| Terminals, agents' harnesses | the runner's | the node's (unchanged) |
| Synced repos | the server votes | voters on nodes |
| Links | through the server | direct between nodes; relayed by any node both reach |
| The CA, join codes | the server's key | the cluster group's, sealed |

## State, by kind

- **The cluster group.** People, accounts, tokens, roles, settings, registered repositories,
  deployments and environments, secrets (sealed, below), goals and plans, and the fleet itself:
  its nodes, their certificates, addresses and roles. Small and read everywhere, so it is one
  hyper-raft group with a replica on every node: three voters (five past ten nodes), the rest
  learners. A node reads its own replica; a write goes to the group's leader over the node links
  and returns once committed. Its state machine is the store kiln has (SQLite, one file per node).
- **Run partitions.** Runs, jobs and steps: written often, read mostly recent. A group per
  partition (by repository), three voters, placed as KilnDB places sessions. A job's logs stay
  on the node that ran it, as files, with one more copy on another node; the UI reads them from
  either, through the node links.
- **Conversations.** KilnDB as designed: a group per session, its preferred leader the node its
  agent runs on, so a write commits on that machine and one more.
- **Bytes.** Mantle: artifacts, images, bundles, backups, chunked over nodes' disks, with its
  metadata replicated on three nodes.
- **Per node.** Metrics and Observe's samples stay on the node that took them; a chart over many
  nodes asks each (and rollups are kept in the run partitions' fashion). Terminals, harness
  processes and caches are the node's own.

## Work that happens once: leases

A lease is a row in the cluster group: what it's for, the node holding it, until when, and its
epoch. The holder renews it; when it lapses (the holder gone for twice the renewal), any node may
take it, with the next epoch. Every write a lease's work makes carries its epoch, and the group
refuses a write with an older one, so two holders never both act.

| Lease | One per | Was |
| --- | --- | --- |
| `poll` | repository | the server's GitOps poller |
| `schedule` | run partition | the server's scheduler |
| `deploy` | deployment app | the CD controller |
| `rollout` | the fleet | update rollouts |
| `notify` | the fleet | notifications and webhooks out |
| `mirror` | external store | backups, Tailscale and DNS upkeep |

A webhook can arrive at any node: it is written to the cluster group, and the `poll` lease holder
acts on it. Leases prefer steady nodes with what the work needs (a Kubernetes context for
`deploy`, the network GitHub is on for `poll`).

## Jobs

A run's jobs wait in its partition's queue by their `runs-on` labels. A node with free slots that
matches claims a job (a compare-and-set through the partition's leader), runs it, and heartbeats
its claim; a claim that lapses goes back on the queue, as a lost agent's jobs do today. Kubernetes
jobs are claimed by a node that holds a context for the cluster, which starts the job's pod.

## The API and the UI on every node

- **Reads** come from the node's own replicas: a list may be a moment behind (a follower read), a
  page that must be current waits for the leader's index (ReadIndex) first.
- **Writes** go to the leader of the group they touch, over the node links.
- **Live updates** come from the node's replicas as they apply entries, on the event bus
  (focal-bus, as KilnDB takes it in).
- **Node-local things** (a terminal, an agent's harness, a job's live log) are reached through the
  node that has them, over the links, as reach does now.
- **Signing in** works on every node: sessions and tokens are in the cluster group, and cookies
  are signed with a key it keeps.
- **One address:** the front door answers on any node. With a domain (Tailscale's, a custom one),
  its name resolves to every node that serves it; a node that goes away drops out.

## Trust

- **The CA** is the cluster group's: its key sealed with a key the voters each hold a share of
  (two of three open it). A new node's certificate is a request committed to the group and signed
  by the leader once the shares open the key.
- **Joining** is `kiln join` as today (a code, or a cluster found on the network): the new node
  gets its certificate, joins the cluster group as a learner, and fills its replica.
- **Secrets** (tokens to GitHub, registries, clusters) are sealed the same way and opened only on a
  node that needs them for a lease's work.

## Links

Nodes talk directly over QUIC (hyper-transport, mutual TLS on their certificates), found through
the cluster group's address book, the local network (mDNS) and Tailscale. Two nodes that can't
reach each other relay through any node both can (the steadiest first), not a fixed server.

## Installing

`kiln install` installs a whole node, on the target the person picks, and leaves it running:

```
kiln install                                 # Kubernetes, the current context (the default)
kiln install --on kubernetes --context lab   # another context
kiln install --on docker                     # a container on this machine
kiln install --on machine                    # this machine itself, as a service
kiln install --join <code>                   # join a kiln, rather than start one
```

- **Kubernetes:** a StatefulSet of three nodes (`--nodes`), each with its own volume, a Service,
  and the front door. The chart in `deploy/` becomes the node's.
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

| Phase | What | Done when |
| --- | --- | --- |
| P1 | The cluster group on the server alone: the store's tables as its state machine, seeded from today's SQLite | the API reads and writes through the group; nothing else changes; a restart replays it |
| P2 | Runners as learners: each holds the group's replica, serves the UI and API from it, and forwards writes | the UI works through any runner's front door, reads served by that runner |
| P3 | Voters on runners: two steady runners join the server as voters | the server stopped, kiln still serves and writes |
| P4 | Leases: polling, scheduling, deployments, rollouts, notifications | with the server stopped, a push still builds and deploys |
| P5 | Links without the server; relays on any node; trust (sealed CA, joining) in the group | two runners link directly; a new node joins with the server gone |
| P6 | `kiln install` on Kubernetes (default), Docker and a machine, as whole nodes | each target installs a working node from nothing, in one command, checked on each |
| P7 | KilnDB (K2–K7) and Mantle (CP-T10) under the nodes | conversations and bytes no longer go through one machine |
| P8 | The server role retired: production runs as nodes | no code path names a server; production's minikube runs three nodes |

## For you to decide

1. **Voters:** the three steadiest machines by default (today: adacachy, fedora, jupiter), laptops
   as learners? Or name them yourself?
2. **Kubernetes default:** three nodes in the current context, or one (with runners joining it)?
3. **Sealing:** shares among the voters (two of three open the CA and secrets), or one key on each
   voter's disk (simpler, weaker)?
4. **Order:** P1–P6 first (no server role, installs whole), then KilnDB and Mantle under it (P7)?
   Or KilnDB first, as its milestones planned?
