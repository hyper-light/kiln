# Runners: what agents can run in, and a runner's microVMs

The person's decision of 2026-11-23: the engine is each session's, chosen as it starts, Host or microVM, never a
runner's. A runner only says whether it can run microVMs. There is no container engine. Kiln plan
01a1cd8e862e7169a074db3d5fa33f3d, P3.2 (warm pools). Rules: [principles.md](principles.md).

## Where it's chosen: Mission control's composer

More options has an **Engine** row after Workspace: Host, first and as it starts out, and microVM.

- microVM is off where the chosen machine can't run microVMs, its title the runner's why
  (`facts.isolation.why_not`, else "It can't run microVMs"). kiln's server runs none itself.
- Kept on a machine that can't run them, a line under the row says "<machine> can't run microVMs: <why>." and Start
  stays off, with that line as its title (said under More options too while it's closed). Nothing falls back to Host.
- The Agentfile row shows only with microVM. The choice is kept per harness and project, as the Agentfile was: Host
  keeps none, microVM keeps its image; picked with none kept, kiln's for the harness.
- Beside the machine's name, a quiet "microVM" badge, struck through with the why where it can't.
- Closed, More options says "microVM: <Agentfile>"; Host says nothing.
- The start carries `isolation: {kind: "microvm", image}` for microVM, none for Host.

Code: `ui/src/views/harness/new-agent.tsx`, `ui/src/views/agentfiles/choose.ts` (`microvmsOn`).

## The table

- **Agents**: "host · microVM" where the runner can run microVMs (`facts.virt.microvm`), else "host" and ⊘ with the
  why as its title; "—" for a runner without facts. **CI in**: kiln's executors for jobs (host, docker, shards, k8s).
  Both are titled, so the two "host"s aren't read as one.
- Labels keep to one line, faded at the cell's edge (all of them in its title), so no row grows as fonts arrive.

## A runner's details, top to bottom

1. Its name line, then **now**: CPU, memory and disk as live bars from its own samples, and on a runner that runs
   microVMs the vCPUs its Hex node holds (`RunnerNow`).
2. **Agents**: the badges Host and microVM; microVM struck through, its why beside it, where it can't. Nothing to
   switch.
3. kiln's own: watched by, facts, agent sessions.
4. **MicroVMs**: starts by kind and warm hosts; a row per microVM (stand, its agent, linked to its Engine tab, image,
   start, uptime, CPU and memory as compact bars, pause (vCPUs) with Undo, restart, stop asked in place, telemetry
   folding out full-width plots); warm pools (size, warm hosts, idle, hit rate, Release asked in place); images
   (from, built, size, Build again, Remove asked in place, Shards' words on a failed build); Build image inline.
   Columns give way in order as the details narrow; the controls' column is fixed, so rows line up.

Code: `ui/src/views/runners.tsx` (table, details), `ui/src/views/runners-microvms.tsx` and `.css`,
`ui/src/views/sandbox-marks.tsx` (`EngineBadge`).

## What the server answers (the mock: `ui/mock/server.ts`, `ui/mock/microvms.ts`, `ui/mock/runners-ci.ts`)

- A runner's facts: `virt.microvm` (it can run microVMs now) and `isolation.{available, hypervisor, why_not}`; the UI
  never reads `isolation.engine`. `GET /api/agents?lean=1` lists `virt`, and in the mock `isolation` too: a proposal,
  so the composer can say why.
- `GET /api/microvm/runners/:name` (a proposal); the microVMs' acts are the sandbox's (`POST /api/microvm/sessions/:id/:act`).
- The page's other panels: `GET /api/ci/pools`, `GET /api/ci/images`, `GET|PUT /api/runners/:name/headroom`.

## Loading

The page asks for the open runner's microVMs and the CI panels with the rest; with a runner open its placeholder holds
the details' place.
