# Machines by id

> **Built** (2026-10-28), on Ada's word: "The name should just be for user benefit, machine id is
> what we use in all systems." Since 2026-11-04 ("We should register and manage machines/runners
> by machine id. Period. The machine name is just something we show on the surface."), a machine
> is registered by its id or not at all, and a runner is keyed by its machine id from the moment
> it connects, whatever name its certificate carries ([One record a
> machine](#one-record-a-machine)). The decisions are under [Decisions](#decisions).

kiln knows a runner by one key: its machine id, which its connections, processes, agent sessions,
rows, routes and tokens are all filed under. That key used to be the machine's host name when it
joined, so two machines with one host name were one machine to kiln.

They happen without anyone choosing it. macOS names a Mac after its owner ("Ada's MacBook Pro" is
`Adas-MacBook-Pro.local`) and renames one only once it notices another on the network. On
2026-10-26 a second MacBook Pro joined while it still had the first one's host name and was
approved under the first one's runner name. From then on:

- **Their connections pushed each other off.** `instances::join` takes a connection with the same
  name and the same instance id for a stale one of the same process. Every machine's usual runner
  has no instance id, so each laptop's connection closed the other's, about every 17 minutes.
- **Their agent sessions erased each other.** A machine's inventory (`harness::registry::inventory`)
  marks gone every session at `Machine(name)` that the reporting machine didn't list.
- **They were one member of the cluster group,** whose members are a hash of the name
  (`store::group::member_of`).

## How it works

- **Every machine has a machine id** ([The machine id](#the-machine-id)).
- **A machine is registered by its id, or not at all.** Joining (`kiln join`), enrolling with the
  join token (`kiln install --agent --token`), enrolling as an outpost and a second runner
  process's enrollment (`/api/agents/instance`) all need the machine's id; one that can't say it is
  refused, told how to get one (`systemd-machine-id-setup` on Linux, its own `/etc/machine-id` for a
  VM or container, sysprep for a Windows copy, a volume for a container's data directory, or
  `KILN_MACHINE_ID`). Its certificate's name is its id, and everything is keyed by the id from the
  moment its runner connects (a certificate from before included, [One record a
  machine](#one-record-a-machine)): its connections and processes ("one runner a machine" is one per id), its agent sessions,
  its member in the cluster group, its synced folders and the refs kiln keeps for it in people's
  repositories, its links, its rows, routes and tokens. A runner process beside it
  (`upgrade::install_alongside`) gets its certificate from the machine's, id included. The same
  machine joining again is the same runner.
- **Its name is a label, for people.** The name it asked for becomes its label
  (`agents.label`), which the UI, the CLI (`kiln runners`, `kiln join`), the metrics and the
  server's own sentences (its errors, wait reasons, findings and notifications) show; only a runner
  without one is shown by its id. A label is the machine's own: one another runner has,
  as its name or its label, gets `-2`, `-3`, … It's changed under Runners (the runner's details) or
  by `POST /api/agents/:name/label`, and nothing kiln keys by the id moves.
- **Names are resolved where people type them:** `kiln run --on`, `kiln spawn --on` and MCP's `on`
  find a runner by its id, its label, a name its certificate carried before or its host name; `runs-on: <name>` names a runner by its
  label as by its name (its one label, as GitHub's runner names do); `[conversations] mirror_on`,
  an agent's `agent:<name>` target, where a session moves to, the runner a retry asks for and
  `kiln reach forget` take labels too. A grant on `runner:<label>` is kept as the grant on the
  runner's id, so it holds whatever the runner is renamed to.
- **History shows the name it has now.** What kiln keeps names a runner by its id, so a list of
  runs, jobs or sessions shows its label as it is now, after a rename too; so do the notes a
  runner writes into a session's timeline ("Moved here from …"), which only have the ids. Words
  kiln already wrote in a job's log keep the name they said then.
- **What's signed names it by its id.** A runner's certificate names it by its id, and so does
  what kiln verifies from one (docs/ci.md W27): an identity's description ("on runner <id>
  (instance main)"), `kiln verify`'s links ("build leg … on <id>"), where a custody record came
  from, and the timelines named for the machine ("runner role of <id>"). What kiln says in its own
  words about a hand-over (whom a session was resumed from, which side went unsigned) names it by
  its label. A process still holding a certificate for an old name is addressed by that name in the
  hand-over words kiln signs for it, the name it checks them against, until it starts again with
  its certificate for the id.
- **Metrics carry both:** every series of a machine (`kiln_runner_*`, `kiln_agent_*`, sync's copies
  and blobs, the conversation stores') has `machine_id` and `machine_name` beside its own labels,
  so they group by either.
- **A copy is caught.** A machine asking to be known by an id that a connected runner has is a copy
  of it (a VM cloned with its `/etc/machine-id`): it's refused, and told how to get an id of its
  own (`systemd-machine-id-setup` on Linux, sysprep on Windows). The same machine joining again
  stops its runner first, so it isn't one.

### The machine id

`os::machine_id`: a hash of the id the OS keeps for the machine, so the OS's id itself never leaves
it.

| Machine | Its id |
| --- | --- |
| A Mac, or a Mac VM | its `IOPlatformUUID` (a VM's comes from its hypervisor) |
| A Linux machine or VM | its `/etc/machine-id`: made at its first boot, from the hypervisor's own UUID for the VM where it gives one (systemd does), so each VM has its own. One that says `uninitialized` is none |
| A Windows machine or VM | its `MachineGuid`, made when Windows was installed (sysprep makes a new one for a copy) |
| A container (Docker's, Podman's, a Kubernetes pod's) | one kept in `machine-id` in its data directory, on its volume where it has one, made the first time from its pod's UID (`KILN_POD_UID`, from the Downward API: kiln's pods, its chart and `kiln install --on kubernetes` give it) or at random. The same volume is the same machine; a container made again with none is a new one, as it keeps nothing |
| Any, said outright | `KILN_MACHINE_ID` in its environment: a machine of its own whatever the OS says, as each runner a test starts on one machine is, or a second runner someone runs on one machine on purpose |

Each runner also says its id as it connects (`AgentMsg::Machine`) and with its facts
(`RunnerFacts.machine_id`, W13-R01), and the server keeps it with the runner (`agents.machine_id`):
for a certificate that carries a name from before, that's the machine it is.

## One record a machine

Every machine is one record, keyed by its id, from the moment its runner connects. A name is only
what people see: its label.

### Names its certificates carried

Before machine ids, kiln named a machine's certificate after its host name, and some runners still
hold such a certificate. kiln keeps each such name with the machine it is (`machine_names`), and
resolves it wherever a runner names itself (`Hub::machine_of`), so nothing inside the server keys a
runner by one:

- **At the connection.** A runner says its machine id before its `Instance` and `Hello`
  (`AgentMsg::Machine`, docs/wire.md). The server keys the connection by that id; for a runner that
  says none (an older build) by the id kiln knows for its certificate's name (the name's, or what its
  record's runner reported, `agents.machine_id`). A name kiln sees with an id for the first time
  moves to it right then (`Hub::machine_of_connection`): what was kept under it moves to the id
  (below), the name becomes the id's label if the id has none, and the connection is the id's. No
  restart, no refusal because it's connected: it's that process connecting. One that says another
  id than the one kiln knows for its name (a copy, or a machine whose id changed) is refused, saying
  so. One whose runner never said an id stays by its name, stale once it's away
  ([below](#records-that-are-stale)).
- **Its certificate for the id** comes as a refresh: as the runner starts it asks for one
  (`POST /api/agents/rekey`, proving itself with its certificate and key, `Pki::sign_moved`), and
  for a name kiln already maps to the id nothing moves and its being connected doesn't matter. A
  second runner process of it gets its certificate for the id too (`/api/agents/instance`). Until
  it starts again, a process holding the old certificate is keyed by the id all the same; what it
  says of itself by that name is resolved: its sync requests' `?machine=`, the cluster copy it asks
  for, and the hand-over words kiln signs for it, which name it as it knows itself (`peer_certs`).
- **Where people type a name** (`--on`, `runs-on`, MCP's `on`, `agent:<name>`, a grant's
  `runner:<name>`, a path under `/api/agents/` or `/api/runners/`), it resolves to the id: by the
  id, a name its certificate carried, or its label (`Hub::machine_named`; reach's directory carries
  the old names, `RunnerInfo.was`).
- **Forgotten**, a machine's old names go with it: their certificates' keys are revoked with the
  id's, and the names refused at the connection.
- **Its certificate for the id is one of kiln's identities** (docs/ci.md W27-R01), recorded as
  it's issued; a revoked key gets none.
- **The server's own runner** (`[local_agent]`) is known by its machine's id
  (`Config::local_agent_key`), shown by its configured name; what was kept under that name moves the
  first time the server starts.

### Records fold into the machine's id

What kiln kept under a name moves to the machine's id (`Hub::move_runner`, `Store::move_runner`):
its agent row, the jobs it ran and the retries placed on it or away from it, its attempts, its
insights and preview removals, its reap policy, the grants on it, who enrolled it, what's set for
it (front door, agent limit, upgrade policy), its agent sessions (and the conversation store's rows
of them), its synced copies and places, and its member in the cluster group. When the id has a
record already the two merge, one per (machine, key):

- the id's row, label, settings, reap policy and enrollment win, and what only the name had moves;
- the insights both kept for one hour add up (the legs' rollups merged, `Rollup::merge`; the
  runner's connected, slot and busy seconds summed); a grant both had is kept once;
- a synced repository's copies in one folder of the machine are one copy: the one furthest along
  the log as the server last heard, else the one the id had, else the newest. Copies in two
  folders are two;
- its places (the repositories it last said it holds) are the newest word of the two;
- its agent sessions are the machine's. Onto a machine connected now, the ones its runner holds
  are asked for their snapshots at once (they read as they are, not disconnected) and the ones it
  doesn't hold are gone, as its next inventory would say (`Hub::reconcile_moved`).

It happens as the server starts, before any runner connects, for every record kept under a name
whose runner said its id (`Hub::one_record_a_machine`); as a runner connects (above); and as one
known by its id says so again (its `Hello`, its facts: `Hub::fold_duplicates`), for any other record
not connected that said that id. A record folds only by the id its runner said: never by host name,
which macOS renumbers. Each fold is audited (`agent.moved`, or `agent.merged` when the id had a
record: "fedora.attlocal.net was machine <id>: folded into kiln-a094a12d.attlocal.net").

### Finished job pods are forgotten

A Kubernetes job's pod runs an ephemeral runner (`kiln agent --ephemeral`), keyed by its pod
(`kiln-<job>-<attempt>`): it's kiln's own, made for one job, with an id of its own from its pod's
UID. Once a minute the server forgets every pod's record that isn't connected, is past its
reconnect grace (and the server past its own, so pods connected before it started can come back),
and whose jobs have all ended (`Hub::forget_finished_pods`). Its jobs, attempts and insights keep
its name, so a job's page still says which pod ran it. There's no switch: these are kiln's own
records of its own pods, not a person's machines (AGENTS.md, opt-in automation).

### Records that are stale

A record whose runner never said its machine id and that isn't connected can't be proven to be any
machine: kiln neither folds nor forgets it. The runners' API says `stale: true` of it
(`GET /api/agents`, MCP's `runners.list`), `kiln runners` lists it as stale, and the Runners page
shows it as stale, with what it means, beside its Forget. Forgetting it is a person's call. A
runner of it that connects again and says its id is that machine's from then on.

## Compatibility

- **The wire grew one message.** `AgentMsg::Machine { id }`, appended last, said before
  `Instance` and `Hello`: an older server skips it (as it skips `Caps`), and the server takes a
  runner that doesn't send it by the id it knows for its certificate's name. The join request, the
  enrollment and their answers carry optional JSON fields (`machine_id`, `label`); this server
  refuses to register a machine without `machine_id`. Reach's directory gains optional `was` and
  `stale`; `GET /api/agents` optional `stale`.
- **The store gained two nullable columns and a table** older builds never name
  (`agents.machine_id`, `agents.label`, `machine_names`): no new epoch.
- **A runner's local sockets keep their paths.** `kiln run`'s and `kiln spawn`'s sockets are named
  by the name the runner was started with, never by its id, so a moved runner serves them where its
  sessions were told they are (`KILN_REACH_SOCK`); an id wouldn't fit in a socket's path in a Mac's
  temporary folder anyway.

## What's next

Nothing planned. A process still holding a certificate for its old name takes its certificate for
the id the next time it starts.

## Decisions

Taken on 2026-10-27:

1. **VMs, containers and pods** have ids of their own ([The machine id](#the-machine-id)), and a
   copy is caught when it asks to join.
2. **History shows each machine's name now**, after a rename too: kiln keeps the id and shows the
   label it has now.
3. **Metrics carry both labels**, the machine's name and its id, so they group by either.
4. **Every way in is named by id:** joining, the join token and outposts alike.

Taken on 2026-11-04 ("everything runners related should be de-duplicated and identified throughout
the system via machine id; we use machine names in the UI by default", then "We should register and
manage machines/runners by machine id. Period. The machine name is just something we show on the
surface."):

5. **Registered and managed by machine id, period:** a runner is keyed by its id from the moment
   it connects, whatever its certificate says; registration needs an id; names are labels,
   resolved where people type them ([One record a machine](#one-record-a-machine)).
6. **A machine's records are one:** every record that said the machine's id folds into it, one per
   (machine, key), its sessions reconciled
   ([Records fold into the machine's id](#records-fold-into-the-machines-id)).
7. **Job pods' records go** once they've disconnected and their jobs ended
   ([Finished job pods are forgotten](#finished-job-pods-are-forgotten)).
8. **Mark stale, you forget:** a record that never said its machine id is shown as stale, never
   merged or forgotten by kiln ([Records that are stale](#records-that-are-stale)).
