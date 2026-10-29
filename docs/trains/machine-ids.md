# Train notes: machines by id

docs/machines.md: kiln knows every runner by its machine id, and shows it by its name. What it
delivers, what deploying it does and how to watch it, its risks, its rollback, and what's still
open.

**Where it stands (2026-10-28, when written):**
- **Merged:** with this file, onto main at 2e0fb8fd (Ship's commit pages and Mission control's
  composer merged in: Ship's replanting runners and the composer's machine are named by their
  labels too). The landing commit's message has the gate's evidence.
- **Deployed:** not yet. Deploys go through Ada, and so do runner upgrades: a runner moves to its
  machine id only once it runs this build and starts.
- **Gated** on adacachy (2026-10-28, 23:19 to 23:53Z) on 0c9be472, the tree this push carries but
  for these notes: clippy across the workspace with all targets, clean, for Linux, Windows
  (`x86_64-pc-windows-gnu`) and macOS (`aarch64-apple-darwin`, both through cargo-zigbuild); every
  workspace test run, with 4 test threads (3,230 passed). What failed, none of it this train's:
  - adacachy has a `/usr/bin/claude`, so the tests that expect no Claude Code on `/usr/bin:/bin`
    find one: `couldnt_start`'s two and `harness_path`'s one (all three pass on macOS).
  - The gate's own git config breaks `actions/checkout`'s token step: `hosted_actions`' `w14_ac05`,
    on main too with the same git config.
  - The gate's memory cap is what a runner reports: `facts`' `w13_ac01`.
  - Under load: `harness::builds`' `w24_r24` (passes alone), and `resume`'s
    `a_runner_that_starts_again_…`: its restarted runner connects again under its machine id at
    once, and the session's turn sometimes doesn't go on in time; alone on adacachy it passed 7 of
    8 times, and on macOS every time.

  The gate before the rebase (82155673, onto a946ea5f) found `runner_path` asking for its runner
  by name: fixed in this train, passing on Linux and macOS. Its other failures were Docker's address
  pools used up on adacachy (since freed), and main's `w28_ac77` and `wire::ts` (since fixed on
  main). Its kiln lib tests aborted with a stack overflow, on main too; run on Linux with
  `RUST_MIN_STACK=16777216`, their failures were only those. Five tries stopped themselves as their
  machines' services' I/O pressure rose (adacachy three times, kiln-a094a12d twice), and one lost
  its link to this Mac's runner mid-run: this train's own problem, two runners sharing a name.
- **Checked on macOS,** on the rebased tree: clippy on every crate and target; kiln-ci's tests
  (560); kiln's lib tests with `RUST_MIN_STACK=16777216` (2,088 passed; the 4 failing need Linux or
  a quiet machine: two ACP catalog deadlines, an ELF check of a Mach-O binary, a 2 ms p99); the
  runners' integration tests and `tests/machines.rs`; the UI's typecheck, lint, tests (333) and
  build, and its browser tests in Chromium and WebKit at desktop and phone widths, main's composer
  with the longest name a runner can have among them.
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
  cluster group. Their names become their labels. The server's own runner too, the first time the
  server starts.
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
