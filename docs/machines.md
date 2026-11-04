# Machines by id

> **Built** (2026-10-28), on Ada's word: "The name should just be for user benefit, machine id is
> what we use in all systems." The decisions it took are under [Decisions](#decisions). A runner
> that enrolled before moves to its machine id as it starts
> ([Runners from before](#runners-from-before)).

kiln knows a runner by one name: its certificate's, the key its connections, processes, agent
sessions, rows, routes and tokens are all filed under. That name used to be the machine's host
name when it joined, so two machines with one host name were one machine to kiln.

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
- **A machine is known by its id.** One that sends its id as it joins (`kiln join`), enrolls with
  the join token (`kiln install --agent --token`) or enrolls as an outpost gets a certificate whose
  name is its id. Since kiln knows a runner by its certificate's name, everything is keyed by the
  id: its connections and processes ("one runner a machine" is one per id), its agent sessions,
  its member in the cluster group, its synced folders and the refs kiln keeps for it in people's
  repositories, its links, its rows, routes and tokens. A runner process beside it
  (`upgrade::install_alongside`) gets its certificate from the machine's, id included. The same
  machine joining again is the same runner.
- **Its name is a label, for people.** The name it asked for becomes its label
  (`agents.label`), which the UI, the CLI (`kiln runners`, `kiln join`), the metrics and the
  server's own sentences (its errors, wait reasons, findings and notifications) show; a runner
  without one is shown by its name. A label is the machine's own: one another runner has,
  as its name or its label, gets `-2`, `-3`, … It's changed under Runners (the runner's details) or
  by `POST /api/agents/:name/label`, and nothing kiln keys by the id moves.
- **Names are resolved where people type them:** `kiln run --on`, `kiln spawn --on` and MCP's `on`
  find a runner by its name, its label or its host name; `runs-on: <name>` names a runner by its
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
  its label.
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

Each runner also reports its id with its facts (`RunnerFacts.machine_id`, W13-R01), and the server
keeps it with the runner (`agents.machine_id`): for a runner from before, that's what tells
whether a machine joining under its name is the same machine.

## Runners from before

A runner that enrolled before this is known by its name: it sent no id when it joined. As it
starts, before it connects or holds anything, it asks to be known by its machine id
(`POST /api/agents/rekey`), proving itself with its certificate and key as a second runner process
of a machine does (`Pki::sign_moved`). So does a runner process an upgrade started (`--instance`),
for its machine, then gets its own certificate for the id. An upgrade in place (one process taking
over from another) moves nothing, as the process it takes over from is connected: the runner
moves the next time it starts with none of it connected (a restart, a reboot). The server moves what it kept under the name to the id
(`Hub::move_runner`), and the name becomes its label:

- its agent row, the jobs it ran and the retries placed on it or away from it, its attempts, its
  insights and preview removals, its reap policy, the grants on it (`runner:<name>`), and what's
  set for it: its front door, its agent limit, its upgrade policy;
- its agent sessions (and so the conversation store's rows of them), its synced copies and places;
- its member in the cluster group, which it joins again as it is now.

The runner keeps its new certificate in place of the old one. It's refused while a process of it
is connected, for an id other than the one its facts said, for an id a connected runner is known
by, while another record of the machine is connected (below), and for a runner the server doesn't
know (one that never connected, or one forgotten); it then goes on by its name and asks again as
it starts next. One known by a machine id already never moves to another (a VM copied with its
runner's state is caught as a copy instead).

### Duplicates fold into the machine's id

A machine can have more than one record from before: it joined by one name, then again by another
(its host name changed, as `fedora.attlocal.net` became `kiln-a094a12d.attlocal.net`), and each
record's runner said the same machine id (`agents.machine_id`). Only one of them can move to the id;
the others fold into it (`Hub::fold_duplicates`):

- **When.** As a runner moves to its machine id, as a runner known by its id connects, and as one
  reports facts saying that id (`POST /api/agents/:name/probe` asks it to).
- **What folds.** Every other record whose runner said it is that machine and that isn't
  connected. Never a connected one: it stays (the log says so), and a runner moving to the id is
  refused while one is, saying which. Never one known by another machine id (that's its own
  machine, or a copy caught as one), and never one that only shares a host name, since macOS
  renumbers host names.
- **How.** What kiln kept under the duplicate's name moves to the id as a runner from before's does
  (`Hub::move_runner`, `Store::move_runner`), and merges with what the id has: the id's row, label,
  settings (front door, agent limit, upgrade policy), reap policy and enrollment win; what only the
  duplicate had moves; the insights both kept for one hour add up (the legs' rollups merged, the
  runner's connected, slot and busy seconds summed); a grant both had is kept once. Its jobs,
  attempts, agent sessions, synced copies and places move as well. Then its record is gone,
  audited as `agent.merged` ("fedora.attlocal.net was machine <id>: folded into
  kiln-a094a12d.attlocal.net"). A runner moving to an id kiln knows already, not connected, merges
  into it the same way: the same machine's second record.

### Finished job pods are forgotten

A Kubernetes job's pod runs an ephemeral runner (`kiln agent --ephemeral`), named after the pod
(`kiln-<job>-<attempt>`), each with a machine id of its own. Its record was forgotten when it
disconnected, but not when the server stopped first (a deploy, a crash): those records stayed,
offline, for good. Once a minute the server forgets every pod's record that isn't connected, is
past its reconnect grace (and the server past its own, so pods connected before it started can come
back), and whose jobs have all ended (`Hub::forget_finished_pods`). Its jobs, attempts and insights
keep its name, so a job's page still says which pod ran it. There's no switch: these are kiln's own
records of its own pods, not a person's machines (AGENTS.md, opt-in automation).

### Records that are stale

A record that never said its machine id (known by a name, `agents.machine_id` empty) and isn't
connected can't be proven to be any machine: kiln neither folds nor forgets it. The runners' API
says `stale: true` of it (`GET /api/agents`, MCP's `runners.list`), and the Runners page shows it as
stale, with what it means, beside its Forget. Forgetting it is the person's call. A runner of it
that connects again says its machine id and is a record like any other.

Its certificate for the id is one of kiln's identities (docs/ci.md W27-R01), recorded before
anything moves, and who enrolled it moves with it: forgetting the runner revokes its key, and so
its certificate for its old name too. A revoked key doesn't move. Its old name isn't revoked, so a machine on an
older kiln that shares it keeps working.

The server's own runner (`[local_agent]`) is known by its machine's id too
(`Config::local_agent_key`), shown by its configured name; what was kept under that name moves the
first time the server starts.

## Compatibility

- **The wire is unchanged.** A runner's id is its certificate's name, so `Hello` carries it as it
  carries a name. The join request, the enrollment and their answers gain optional JSON fields
  (`machine_id`, `label`): an older server ignores them and names a new machine as before; an older
  runner sends none and is named as before.
- **The store gained two nullable columns** older builds never name (`agents.machine_id`,
  `agents.label`): no new epoch.
- **A runner's local sockets keep their paths.** `kiln run`'s and `kiln spawn`'s sockets are named
  by the name the runner was started with, never by its id, so a moved runner serves them where its
  sessions were told they are (`KILN_REACH_SOCK`); an id wouldn't fit in a socket's path in a Mac's
  temporary folder anyway.

## What's next

Nothing planned. A runner on a kiln from before moves once it's upgraded; a machine's other records
fold into its id as it does.

## Decisions

Taken on 2026-10-27:

1. **VMs, containers and pods** have ids of their own ([The machine id](#the-machine-id)), and a
   copy is caught when it asks to join.
2. **History shows each machine's name now**, after a rename too: kiln keeps the id and shows the
   label it has now.
3. **Metrics carry both labels**, the machine's name and its id, so they group by either.
4. **Every way in is named by id:** joining, the join token and outposts alike.

Taken on 2026-11-04 ("everything runners related should be de-duplicated and identified throughout
the system via machine id; we use machine names in the UI by default"):

5. **A machine's records are one:** every record that said the machine's id folds into it, unless
   it's connected ([Duplicates fold into the machine's id](#duplicates-fold-into-the-machines-id)).
6. **Job pods' records go** once they've disconnected and their jobs ended
   ([Finished job pods are forgotten](#finished-job-pods-are-forgotten)).
7. **Mark stale, you forget:** a record that never said its machine id is shown as stale, never
   merged or forgotten by kiln ([Records that are stale](#records-that-are-stale)).
