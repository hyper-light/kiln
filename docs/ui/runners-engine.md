# Runners: the engine, isolation facts, images and warm pools (design)

Serves kiln plan 01a1cd8e862e7169a074db3d5fa33f3d's build plan P2.3 (a runner's engine) and P3.2 (warm-pool
view), from the kiln × Hex × Shards design, sections A and E. Decision 3 (the person's, 2026-11-14) holds: a
runner that can run microVMs defaults to `microvm`, and its owner opts out per runner. Decision 8's rulings
hold too:
- a switch drains, waiting, with an explicit "End them now";
- only `runners.manage` switches; others see the control disabled, with why;
- the workspace default lives in Settings → Runners, and a project's override is shown.

Status: design for the person's approval. Nothing here is built. The mockup is
[mockups/runners.html](mockups/runners.html): open it in a browser, or as kiln showed it. It uses kiln's own
compiled stylesheet (`ui/src` at b2fc6cebb), so what it shows is kiln's type, colour and components.

## Where it lives

Runners (`/runners`, `ui/src/views/runners.tsx`). Nothing moves: one column joins the table, a facet joins the
filter, and two blocks join a runner's details, which open in place under its row as they do today.

## The table

- **Engine** column (new), after Labels, shown from `sm` up. The cell reads `microvm` or `host` in mono, with a
  faint line under it:
  - `microvm`: its hypervisor (`Hypervisor.framework`, `KVM`, `WHP`), from `isolation.hypervisor`;
  - `host` by its owner's choice: `your choice`;
  - `host` because it can't: `can't run microVMs`;
  - during a switch: `microvm → host` in the warn tone, and `draining N` under it.
- On a phone the column is hidden, and the runner's own mono line ends with ` · microvm` or ` · host`.
- **Executors** lists `shards` beside `host` for a runner on `microvm`.
- The filter's facets gain **Engine** (`host`, `microvm`) beside State, Labels and Platform.
- The page lead counts them: `… · 5 run sessions in microVMs, 2 on the host.`

## A runner's details

Two blocks join `AgentDetails`, each under a mono `.label`, between the actions row and the existing facts
(the engine is acted on where it's read), and after the agent sessions limit (what microVMs here hold).

### Engine

- A `Segmented` control, `host | microvm`, at the block's head.
  - Only those with `runners.manage` switch it.
  - Anyone else sees it disabled, with the words "Only those who manage runners (`runners.manage`) switch its
    engine: ask the runner's owner." (the mockup's "see it as someone without runners.manage" box).
  - The precedence, `workspace → runner → project → session`, sits at the block's right in faint mono.
- One line under it says what holds and why:
  - "Sessions here run in microVMs of their own (Shards, through Hex) on Hypervisor.framework. The default for
    a runner that can: you can opt out."
  - "Sessions here run on the host, as processes of the machine: set by you 3 days ago. It can run microVMs
    (KVM)."
  - "Sessions here run on the host: this runner can't run microVMs." followed, in the warn tone, by Shards'
    own words from `isolation.why_not`, and "Shards' host check, 4 min ago, without booting a VM. Check again".
- When the runner can't run microVMs, the `microvm` pill is disabled, with the same words as its title.
- **Switching** (a press on the other pill) asks in place, with kiln's `Confirm` under the control: no popup.
  - It names what happens: "Switch adas-mac-studio.local to host? New sessions start on the host, with no
    microVM, at once. Its 1 microvm session drains first: it goes on until it ends, and none is stopped."
  - It lists the sessions that drain (dot, name, harness, state, age).
  - Its actions are "Switch and drain" (or "Switch" when nothing runs) and "Keep microvm".
  - Pressed, the button says its request is on its way (W21-R15).
- **While it drains**, the control is disabled.
  - The line says "Switching to host. New sessions here start on the host already; the 2 microvm sessions below
    finish first, then the switch is done."
  - The sessions are listed with "microvm until it ends".
  - "Cancel the switch" ends it.
  - **End them now** (ghost-danger) asks in place, with a danger `Confirm`: "End them now? Their turns stop and
    the sessions end; their conversations are kept, to go on later on the host." Its actions are "End them now"
    and "Let them finish" (the singular "it" for one).
  - Ended, the switch completes at once.
- Under the line, the **workspace default** and any **project override**: "Workspace default: microvm, in
  Settings → Runners." followed, in the warn tone, by each project that asks for another engine here
  ("hyper-light/kiln asks for host: its sessions here run on the host."), or "No project overrides it here."
- A switch to `host` releases that runner's warm pools once it's done.

### MicroVMs here

- **Facts** (kiln's `Facts`), each with its source name under its label, so a person can tell what kiln
  read from where:
  - `virt.microvm`;
  - `isolation.available`;
  - `isolation.hypervisor`;
  - `isolation.why_not`;
  - Shards' `host_check` (usable, backend, when; tone pass or fail);
  - the Hex node's class (`lists shards-image` or `lists no shards-image`);
  - its room, from Hex's keeper (memory and vCPUs free).
- **Images here**: a `data-table` of the images built on this runner: image name and whose (`yours · v8`,
  `kiln's`), harnesses, digest, size, how it got there (`prebuilt 4 min ago`), last used. Its eyebrow says how
  many, and "built on this runner": images aren't shared until Reliquary (decision 2).
- **Warm pools** (P3.2): a `data-table`:

  | Column | What it shows |
  |---|---|
  | Agent | a link to its definition; under it, whose (yours, or kiln's) |
  | Image · size | e.g. `rust-dev v8 · 2 vCPU · 4 GiB` |
  | Warm | how many are warm now |
  | Hits, 24 h | `41 of 47`, with "starts warm" under it |
  | Kept | "while its starts come", or, in the warn tone, "no starts for 31 min · goes at 38" |
  | Release | a ghost button |

  - The eyebrow reads "named by kiln · sized by Shards".
  - One line explains the rule without a number of ours: "kiln keeps a pool for an agent whose starts are
    placed here. Its size is Shards' rule: the largest burst of starts within a refill window, inside this
    node's room, which pays for its memory. One with no starts for its keep window is released."
  - Nothing here sets a pool's size: sizes are learned (R03), so there's no field for one.

## What it needs from the server

- On `Agent` (`/api/agents`), all optional (append-only):
  - `engine: "host" | "microvm"`;
  - `engine_origin: "default" | "owner" | "cannot"` and `engine_set_at`;
  - `engine_switch: { to, draining: [session ids] } | null`;
  - `host_check: { usable, backend, why, at }`;
  - `hex_class: string[]`;
  - `room: { memory, vcpus }`.
- `overrides: [{project, engine}]` on `Agent`, and the workspace default (`GET/PUT /api/settings/runners`, its
  `engine` field), for Settings → Runners.
- New routes:
  - `PUT /api/agents/:name/engine {engine}`: refused, typed, when the runner can't run microVMs, with Shards'
    why, and for a caller without `runners.manage`;
  - `POST /api/agents/:name/engine/switch/end`: End them now;
  - `DELETE /api/agents/:name/engine/switch`: cancels a switch;
  - `POST /api/agents/:name/host-check`: Check again.
- `GET /api/agents/:name/images` and `GET /api/agents/:name/pools`, and
  `DELETE /api/agents/:name/pools/:key` (Release).
- Events: `engine` and `pool` on `/api/events`, so the cell and the pools follow without a reload.

## Tests (each acceptance criterion, through the UI, in Chromium and WebKit, desktop and phone)

- P2.3 AC1: the Engine column and the runners API show each runner's engine.
- P2.3 AC2, against the mock:
  - a runner with HVF and a usable host check shows `microvm`, origin default;
  - one without shows `host`, `can't run microVMs` and Shards' why.
- P2.3 AC3:
  - Switch, then Switch and drain, shows the draining state;
  - the next session started there runs on the host, with no microVM;
  - the switch completes when the drained session ends.
- P2.3 AC4: the details show the five isolation facts with their sources.
- A runner that can't is refused: the `microvm` pill is disabled, and the API's refusal is shown in its words.
- Someone without `runners.manage` sees the control disabled, with why, and the API refuses their switch.
- During a drain, End them now ends the sessions and completes the switch, and their conversations are kept.
- A project's override shows under the line.
- P3.2 AC2: after repeated starts of agent A on a runner, its pools table shows A, its size and a hit.
- P3.2 AC3: with no starts for a keep window, the pool leaves the table.

## Decided (decision 8, 2026-11-14)

- A switch waits for the other engine's sessions to end, and offers an explicit **End them now**.
- Only `runners.manage` switches; others see the control disabled, with why.
- The workspace default lives in Settings → Runners; a project's override shows in the runner's details.
