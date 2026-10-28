# Machines by id

> **Proposed** (2026-10-27), for Ada's review: nothing below past phase 1 is built. Phase 1 is
> branch `runner-machine-ids` (machine ids reported and kept, and used when a machine joins). The
> decisions this needs are under [Decisions](#decisions).

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

Where the OS keeps no usable id, the machine still gets one:

| Machine | Its id |
| --- | --- |
| A container without `/etc/machine-id` | a hash of its enrollment key's public key: a new key is a new machine |
| A job's pod (`[kubernetes]` executors) | its pod's own (ephemeral, forgotten when it goes, as now) |
| The server's local agent | the server machine's: it is that machine's runner |
| A VM cloned with its image's `/etc/machine-id` | the id it says, until enrollment finds it held: [Decisions](#decisions) 1 |

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
| Sync | `repos.json`'s `CopyRef.machine`, blob tokens and `?machine=`, `places.json`, `tasks.json`, `Step.by` | by id; `Step.by` and conflict copies keep the name, for people |
| Reach and the front door | the relay's links, `AgentMsg::Reach.to`, `ServerMsg::Reach.from`, each runner's `pins.json`, `front_door/<name>` | by id; pins by id with the name beside it |
| Conversations | `sessions.machine`, `peers.peer = <machine>/<store>`, `Row::Machine`, `mirrors.json` | by id |
| Fleet, updates, doctor | upgrade jobs' `target`, `updates/policy/<name>`, the fleet's tries | by id |
| CI | the scheduler's `RunnerIn.id`, `jobs.agent`, `jobs.placement`, `ci_attempts.runner`, `ci_rollups`, `ci_runner_time`, `ci_preview_removals`, `work_reap_policies`, the W24 ledger's runner, `grants` `runner:<name>`, `on: runner:<name>` attachments | by id; `runs-on: <name>` resolves to an id when a job is routed, as GitHub's labels mean |
| The store | `agents.name` the primary key, the rest bare text (no foreign keys) | `agents.machine_id` the key; history keeps the name it had, beside the id |
| Metrics | `{agent=}`, `{runner=}`, `{machine=}` labels | the name stays the label people read; `machine_id` added where series must survive a rename |
| API, UI, CLI | `/api/agents/:name/*`, `/api/runners/:name/*`, `?agent=`, `?runner=`, `/terminal?open=agent:<name>`, `kiln runners facts <name>`, `kiln reach forget <runner>`, `kiln updates … <runners>` | routes by id, names accepted and resolved; the UI routes by id and shows names |
| Outside kiln | git refs in people's repositories (`refs/remotes/kiln/<machine>/…`, `refs/kiln/wip/<machine>/…`), conflict copies' names, webhook payloads and links, Prometheus series | names, for people: a rename moves the refs; webhooks carry both |

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
  reported, or refuses one another key holds ([Decisions](#decisions) 1).
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

1. **Two machines report one id** (VMs cloned from one image without a new `/etc/machine-id`).
   Proposed: enrollment refuses an id another key holds that was connected in the last 30 days,
   saying to regenerate the machine id (`systemd-machine-id-setup`), unless an admin says it's
   that machine, replaced. The other way, a new key is a new machine.
2. **What history says after a rename:** runs, legs and audit rows show the name the runner had
   then, or its name now. Proposed: its name now, with the old one in the audit trail.
3. **Metrics:** keep `runner=<name>` as the label (series change on a rename), or label by id and
   add the name. Proposed: the name, plus `machine_id` on the runner series only.
4. **Git refs in people's repositories:** keep names (people read them) and move them on a rename,
   or use ids. Proposed: names.
5. **Join-token enrollments** (`kiln install --agent --token`) take the name asked, even another
   machine's. Proposed: from phase 2, by id as joins are.
