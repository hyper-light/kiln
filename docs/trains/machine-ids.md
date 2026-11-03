# Train notes: machines by id

docs/machines.md: kiln knows every runner by its machine id, and shows it by its name. What it
delivers, what deploying it does and how to watch it, its risks, its rollback, and what's still
open.

**Where it stands (2026-11-02):**
- **Merged:** with this file, onto main at 893c9a6a, with what main brought since this began:
  Ship's commit pages and Mission control's composer (their runners named by their labels too),
  kiln's identities (W27: a runner's move records its certificate for the id, moves who enrolled
  it, and refuses a revoked key), and W27's transfers: a runner process takes a hand-over's word
  for itself by its id, and its local sockets keep the paths named by the name it was started
  with (an id doesn't fit in one in a Mac's temporary folder). The landing commit's message has the
  gate's evidence.
- **Deployed:** not yet. Deploys go through Ada, and so do runner upgrades: a runner moves to its
  machine id only once it runs this build and starts.
- **Gated** on kiln-a094a12d (2026-11-02, 19:57 to 21:07Z) on 61a271e8, the tree this push carries
  but for these notes: clippy across the workspace with all targets, clean, for Linux, Windows
  (`x86_64-pc-windows-gnu`) and macOS (`aarch64-apple-darwin`, both through cargo-zigbuild); every
  workspace test run, with 4 test threads (4,418 passed, 119 ignored). What failed (20), none of
  it this train's:
  - Docker's address pools used up on kiln-a094a12d (32 networks there; "all predefined address
    pools have been fully subnetted" 32 times): the 18 tests that start containers, `runner::docker`'s
    five, `docker_sock`'s, `freeze`'s and `runtime`'s in kiln's lib, and `actions`' `w07_r05`,
    `deploy_containers`, `image_pool`, `leftovers`, `pools`, `post_steps`' two, `previews`,
    `retries`' `w05_r15` and `services`.
  - main's own, failing on main at 893c9a6a the same way (its durable Codex startup messages):
    `harness::session`'s `cp_t3_2_a_restart_recovers_a_codex_startup_refusal_…` and `switch_held`'s
    one.

  The gate before, on the merge of 893c9a6a alone (6ab40c6f), found main's W27 tests naming runners
  by their names, and with them a runner process refusing a hand-over's word for itself once it
  moved: fixed here. Gated before on adacachy (2026-10-28, on 0c9be472, 3,230 passed), its
  failures none of this train's either (a `/usr/bin/claude` there, the gate's own git config and
  memory cap, two tests under load).
- **Checked on macOS,** on 61a271e8: clippy across the workspace with all targets, clean; main's
  tests this train changed, on its build: `transfers` (7; the 3 that need a release build
  aside), `identities` (23), `store_handover` (3), `experiments` (2), `verify` (4), and
  `tests/machines.rs` (3); the UI's typecheck, lint, tests (421) and build. Failing here as on main
  at 893c9a6a, run the same way: `switch_held`'s one (the message held for Codex never shows
  queued) and four kiln lib tests (`cp_t3_2_a_restart_recovers_a_codex_startup_refusal_…`, "original
  prompt was lost or delivered twice"; `w27_r03_only_a_sessions_own_process_signs_as_its_agent`;
  `p_ac1_a_plan_s_steps_end_as_a_stack_of_diffs_…`; `a_detached_run_whose_asker_takes_none_of_…`).
  These found the two things this train fixed last: a moved runner on a Mac had no `kiln run` (its
  socket's path too long) and refused hand-overs for itself. On 2026-10-28, on the tree then:
  kiln-ci's tests (560); kiln's lib tests with `RUST_MIN_STACK=16777216` (2,088 passed); the
  runners' integration tests; the UI's browser tests in Chromium and WebKit at desktop and phone
  widths, main's composer with the longest name a runner can have among them.
- **Why:** on 2026-10-26 a second MacBook Pro joined under the first one's runner name
  (`adas-macbook-pro.local`, its host name then): the two laptops' connections pushed each other off
  about every 17 minutes, their agent sessions erased each other, and they were one member of the
  cluster group. docs/machines.md has the whole story.

## What it delivers

- **Every runner is known by its machine id** (a hash of the id its OS keeps: macOS's
  `IOPlatformUUID`, Linux's `/etc/machine-id`, Windows' `MachineGuid`; a container's is kept on its
  volume, made from its pod's UID). A machine that joins, enrolls with the join token or as an
  outpost gets a certificate for its id; everything kiln keys by a runner's name is keyed by the id.
- **Its name is a label, for people:** the UI, the CLI, the metrics (`machine_id` and
  `machine_name` on every machine's series) and the server's sentences show it. Runners are renamed
  in their details (Runners) or by `POST /api/agents/:name/label`.
- **Names people type find runners** by their labels: `--on`, `on`, `runs-on`, `mirror_on`,
  `agent:<name>` targets, session moves, retries, `kiln reach forget`, and grants
  (`runner:<label>` is kept as the grant on the id).
- **Runners from before move** to their ids as they start (`POST /api/agents/rekey`), what kiln kept
  under their names with them: rows, jobs, attempts, insights, reap policy, grants, front door,
  agent limit, upgrade policy, agent sessions, synced copies and places, their member in the
  cluster group, who enrolled them. Their names become their labels, and their certificates for
  their ids are kiln's identities: forgetting one revokes its key. The server's own runner too, the
  first time the server starts.
- **A copy is caught:** a machine asking to be known by the id of a connected runner (a cloned VM)
  is refused, and told how to give it an id of its own.

## Deploying it

1. The server first. As it starts, its own runner (`[local_agent]`) moves to its machine's id: its
   log says nothing more than before, and Runners shows it by its configured name. Runners on older
   builds go on by their names; new ones that join are known by their ids.
2. Then the runners, as Ada upgrades them. A runner moves the first time this build of it starts
   with no process of it connected: a restart or a reboot. An upgrade in place (blue/green) moves
   nothing while the process it takes over from is connected, and the runner moves when it next
   starts.
3. This Mac and the other MacBook Pro share a runner name. Once both run this build, each moves to
   an id of its own as it starts with the other not connected under the name: the first takes the
   name as its label, the second gets `adas-macbook-pro.local-2` (a rename puts it right). It can
   take a couple of starts, as each refuses to move while the other is connected under the name.
   Joining one of them again (`kiln uninstall --agent`, then `kiln join --server <url> --yes`) is
   the quick way.

## Watching it

- **The audit:** `agent.moved` says each move: "<name> is known by its machine id <id>, shown as
  <label>".
- **A runner's log:** "this runner is known by its machine id now" (moved), or "the server didn't
  move this runner to its machine id now; it goes on by its name" with the server's reason (it asks
  again as it starts next).
- **`/api/agents`:** each runner's `name` is its id once moved, its `label` the name it had.
- **Metrics:** `kiln_runner_*`, `kiln_agent_*`, sync's copies and the conversation stores' series
  carry `machine_id` and `machine_name`.

## Risks

- **A moved runner joins the cluster group again** as a new member (the old one is let go), so it
  takes a new copy of kiln.db, as a runner joining for the first time does. Runners restarting
  together after an upgrade take one each.
- **Reach pins** are kept by runner name: a moved runner presents its id, which the others pin on
  first use; the old pin stays, unused (`kiln reach forget <old name>` drops it).
- **The two laptops' history is one:** what kiln kept under their shared name (sessions, jobs) goes
  with whichever moves first. It can't be told apart after the fact.
- **The old name isn't revoked:** a machine still on an older kiln that shares it keeps working. A
  moved runner keeps its key; its certificate for the old name is replaced on its disk.
- **Labels in written words:** words kiln already wrote in a job's log keep the name they said then.

## Rolling back

- **The release before starts on this one's database:** the two new columns (`agents.machine_id`,
  `agents.label`) are nullable and named by nothing before; no epoch.
- **Moved runners stay moved:** their certificates name their ids, which an older server takes as
  their names. Runners shows them by their ids (an older UI knows no labels), and what moved stays
  under the ids. The server's own runner, on an older server, is known by its configured name again,
  and what moved to its id stays there.
- **An older runner** sends no machine id and never asks to move: it's known by its name, as before.

## Open

- Runners write some words naming other machines by their ids (a session's timeline: "Moved here
  from …"): the UI says them by labels, but the raw events (MCP, the API) carry the ids.
- `kiln users` and the grants' lists show `runner:<id>`; SSO's configured grants (`[sso.*]
  grants`) take ids, not labels.
- Two of the browser specs fail before this too: `programs.spec.ts` (`/agents/programs` is an
  agent's page now) and `subagents.spec.ts` (commands are sent wrapped as `{t: "cmd", id, c}`).
