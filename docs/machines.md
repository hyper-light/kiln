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
of a machine does (`Pki::sign_moved`). The server moves what it kept under the name to the id
(`Hub::move_runner`), and the name becomes its label:

- its agent row, the jobs it ran and the retries placed on it or away from it, its attempts, its
  insights and preview removals, its reap policy, the grants on it (`runner:<name>`), and what's
  set for it: its front door, its agent limit, its upgrade policy;
- its agent sessions (and so the conversation store's rows of them), its synced copies and places;
- its member in the cluster group, which it joins again as it is now.

The runner keeps its new certificate in place of the old one. It's refused while a process of it
is connected, for an id other than the one its facts said, for an id another runner is known by,
and for a runner the server doesn't know (one that never connected, or one forgotten); it then goes
on by its name and asks again as it starts next. Its old name isn't revoked, so a machine on an
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

## What's next

Nothing planned. A runner on a kiln from before moves once it's upgraded.

## Decisions

Taken on 2026-10-27:

1. **VMs, containers and pods** have ids of their own ([The machine id](#the-machine-id)), and a
   copy is caught when it asks to join.
2. **History shows each machine's name now**, after a rename too: kiln keeps the id and shows the
   label it has now.
3. **Metrics carry both labels**, the machine's name and its id, so they group by either.
4. **Every way in is named by id:** joining, the join token and outposts alike.
