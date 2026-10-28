# Machines by id

> **Proposed** (2026-10-27), for Ada's review, with its [decisions](#decisions) taken: nothing
> below past phase 1 is built. Phase 1 is branch `runner-machine-ids` (machine ids reported and
> kept, and used when a machine joins).

Today a runner's **name** is its identity everywhere. The name comes from the machine's host name
when it joins, and kiln keys its certificate, its connections, its processes, its agent sessions,
its replica of kiln's records, its synced folders, its links, its rows and its routes by it. Two
machines with one name are one machine to kiln.

They happen without anyone choosing it. macOS names a Mac after its owner ("Ada's MacBook Pro" is
`Adas-MacBook-Pro.local`) and renames one only once it notices another on the network. On
2026-10-26 a second MacBook Pro joined while it still had the first one's host name and was
approved under the first one's runner name. From then on:

- **Their connections pushed each other off.** `instances::join` takes a connection with the same
  name and the same instance id for a stale one of the same process. Every machine's usual runner
  has no instance id, so each laptop's connection closed the other's (`hub.rs`, the stale peers'
  `Disconnect`), about every 17 minutes, all night.
- **Their agent sessions erased each other.** A machine's inventory (`harness::registry::inventory`)
  marks gone every session at `Machine(name)` that the reporting machine didn't list.
- **They were one member of the cluster group.** A runner's member is a hash of its machine's name
  (`store::group::member_of`), so the two laptops took turns holding one replica.
- Hand-overs (`handovers.rs`) would move a session to "the newest process of the machine", the
  other laptop.

## The end state

- **Every machine has a machine id, and kiln keys everything by it**: its certificate, its
  connections and processes, its sessions, its member in the cluster group, its synced folders,
  its links, its rows, its routes, its tokens.
- **The name is for people.** The UI shows it, people type it, and it can change at any time
  without breaking anything. No two machines have the same name, so what people type means one
  machine; kiln resolves it to an id where it's used and keeps the id.
- **One runner a machine** means one per machine id (`docs/releases.md`): a machine's processes
  share its id and are told apart by their instance id, as now.

### The machine id

Phase 1's `os::machine_id`: a hash of the id the OS keeps for the machine (macOS's
`IOPlatformUUID`, Linux's `/etc/machine-id`, Windows' `MachineGuid`). It survives a reinstall of
kiln, a new key, a new host name and, on a Mac, a reinstall of macOS. The OS's id itself never
leaves the machine.

Today the id is something a runner *says* (its facts), after its connection is already grouped by
name. In the end state it is something the runner *proves*: the server writes it into the
machine's certificate when it enrolls the machine, as a second subject alternative name
(`urn:kiln:machine:<id>`) beside the name, and a connection's id is its certificate's. A process
beside the usual one (`upgrade::install_alongside`) gets its certificate from the machine's, id
included, as it gets the name now.

Every kind of machine gets one of its own:

| Machine | Its id |
| --- | --- |
| A Mac, or a Mac VM | its `IOPlatformUUID` (a VM's comes from its hypervisor) |
| A Linux machine or VM | its `/etc/machine-id`: made at its first boot, from the hypervisor's own UUID for the VM where it gives one (systemd does), so each VM has its own |
| A Windows machine or VM | its `MachineGuid`, made when Windows was installed (sysprep makes a new one for a copy) |
| A container | one kiln makes the first time it starts there, kept on the container's data volume with the rest of its state: the same volume is the same machine, a new volume a new machine. A container has no hardware of its own, its `/etc/machine-id` is missing or its image's (the same in every container of that image), and its runtime's own id (Docker's, a pod's UID) changes each time it's made again, as an upgrade does |
| A job's pod (`[kubernetes]` executors) | its pod's UID: it lives for one job and is forgotten when it goes, as now |
| The server's local agent | the server machine's: it is that machine's runner |

A VM copied after it first booted carries the original's id. Enrollment catches it: an id that a
machine with another key already holds and that was connected in the last 30 days is refused, and
kiln says how to give the copy its own (`systemd-machine-id-setup` on Linux, sysprep on Windows),
unless an admin says it is that machine, replaced. Nothing is knocked offline on the way.

### Names

- **Unique among machines.** A machine that joins asking for a name another machine has gets
  `<name>-2` (phase 1 decides "another machine" by id already). A rename to a name another machine
  has is refused.
- **Renamed in one place** (Runners, the machine's row): the server keeps the new name with the id
  and reissues the certificate's name lazily, at its next renewal. Nothing keyed by the id moves.
- **Resolved where it's typed:** `kiln run --on`, `kiln spawn --on`, MCP `on`, `runs-on: <name>`,
  `[conversations] mirror_on`, a plan's `Place.on`, URL segments, `grants` scopes. Each resolves
  the name (or the host name, as `--on` does now) to an id when it's used; what kiln stores
  is the id. One that matches more than one machine is refused, saying which.

## What changes, part by part

| Part | Keyed by name today | By id |
| --- | --- | --- |
| Certificates and connections | the certificate's only name (`pki.rs`), `net::ServerBook`, `Hello.name` checked against it, revocation as a set of names | id in the certificate; connections, revocation (by id and serial) and `Hello` checked by it |
| The hub | `agents`, `peers`, `terms`, `away_peers`, `ai_sessions`, `history`, `declined`, `lost_instances`, the reach directory, the wire link's sequence state | the same maps by id; `AgentLive` carries the name |
| Processes | `instances::join` groups by name; stale = same name and instance id | same id and instance id; another machine with the name is another machine |
| Hand-overs | `handovers.rs`, `MoveTo { to: <name> }` | by id; a session moves between processes of one id only |
| Agent sessions | `Location::Machine(name)`, the `agent:<name>` target (Rust, the UI, authorization, session specs: ~100 places), inventories, `holders.json` | `Machine(id)` and `agent:<id>`; holders by id |
| The cluster group | member = hash of the name, persisted in `cluster_conf`; the copy token is a MAC of the name | member = hash of the id: one membership change per runner; tokens of the id |
| Sync | `repos.json`'s `CopyRef.machine`, blob tokens and `?machine=`, `places.json`, `tasks.json`, `Step.by`, the refs kiln keeps in people's repositories (`refs/remotes/kiln/<machine>/…`, `refs/kiln/wip/<machine>/…`) | by id |
| Reach and the front door | the relay's links, `AgentMsg::Reach.to`, `ServerMsg::Reach.from`, each runner's `pins.json`, `front_door/<name>` | by id; pins by id with the name beside it |
| Conversations | `sessions.machine`, `peers.peer = <machine>/<store>`, `Row::Machine`, `mirrors.json` | by id |
| Fleet, updates, doctor | upgrade jobs' `target`, `updates/policy/<name>`, the fleet's tries | by id |
| CI | the scheduler's `RunnerIn.id`, `jobs.agent`, `jobs.placement`, `ci_attempts.runner`, `ci_rollups`, `ci_runner_time`, `ci_preview_removals`, `work_reap_policies`, the W24 ledger's runner, `grants` `runner:<name>`, `on: runner:<name>` attachments | by id; `runs-on: <name>` resolves to an id when a job is routed, as GitHub's labels mean |
| The store | `agents.name` the primary key, the rest bare text (no foreign keys) | `agents.machine_id` the key; history shows each machine's name now |
| Metrics | `{agent=}`, `{runner=}`, `{machine=}` labels: the name | two labels on every series of a machine, its name and its id, so metrics group by either |
| API, UI, CLI | `/api/agents/:name/*`, `/api/runners/:name/*`, `?agent=`, `?runner=`, `/terminal?open=agent:<name>`, `kiln runners facts <name>`, `kiln reach forget <runner>`, `kiln updates … <runners>` | routes by id, names accepted and resolved; the UI routes by id and shows names |
| Webhooks | the `runner.offline` payload and link | the id, with the name beside it |

## Compatibility

- **The wire only grows** (`proto.rs` is postcard by position). `Hello` stays as it is: a runner
  with an id in its certificate needs no new field, since the server reads the id there. Messages
  that name a runner (`Reach`, `MoveTo`) get new variants carrying ids, sent when both sides
  announce a new capability bit (`CAP_MACHINE_ID`). JSON gains optional `machine_id` fields
  beside `name`.
- **Runners from before** have certificates without an id. Until they upgrade, the server knows
  their id from phase 1's `agents.machine_id` (what their facts said), and treats them by name as
  now. On their first start on a new build they ask for a certificate with their id, proving their
  key as an upgrade's new process does (`/api/agents/instance`); the server writes the id they
  reported, or refuses one another key holds, as enrollment does ([The machine id](#the-machine-id)).
- **The store changes by migrations that older builds can live with** (`store/migrations.rs`):
  nullable `machine_id` columns beside the name columns, filled from `agents.machine_id`, read by
  the new build; a table re-keyed by id is a new table, with `-- kiln:epoch N`, once every
  release that reads the old one is past.

## Phases

Each ships, is checked live, then the next starts.

| Phase | What | Done when |
| --- | --- | --- |
| 1 (built) | Machine ids reported in facts and kept (`agents.machine_id`); a machine joining or an outpost enrolling never gets another machine's name; a server isn't installed beside a runner | two Macs with one host name join as two runners |
| 2 | The id in certificates (new enrollments, renewals, and runners from before as they upgrade); revocation by id and serial; the server knows each connection's id before `Hello` | every connected runner's certificate carries its id |
| 3 | Processes, hand-overs, the hub and agent sessions by id | two machines that report one name both stay connected, each with its own sessions and terminals |
| 4 | The store by id: new columns filled, readers switched, then the tables re-keyed | no query keys a runner by name |
| 5 | The cluster group, sync, reach and conversations by id: one membership change per runner, tokens reissued | a runner renamed keeps its replica, its synced folders, its links and its feed |
| 6 | The API, the UI and the CLI by id; names resolved where typed; renaming in Runners | renaming a runner changes only what people see |
| 7 | The name-keyed paths removed, a release after phase 6 | no code path keys a runner by name |

## Decisions

Taken on 2026-10-27:

1. **VMs and containers** have ids of their own ([The machine id](#the-machine-id)): a VM its
   OS's, made from its hypervisor's UUID; a container one kiln keeps on its data volume. A copied
   VM is caught when it enrolls.
2. **History shows each machine's name now**, after a rename too.
3. **Metrics carry both labels**, the machine's name and its id, so they group by either.
4. **Every way in is named by id:** a machine enrolling with a join token
   (`kiln install --agent --token`), which today takes whatever name it asks for, is named as a
   joining machine is.
