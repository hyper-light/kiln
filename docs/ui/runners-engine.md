# Runners: where agents run, where CI runs, and a runner's microVMs

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2.3 (a runner's engine) and P3.2 (warm pools). Decision 3: a runner
that can run microVMs defaults to `microvm`, its owner opts out per runner. (Superseded by the person's ruling of
2026-11-21: every runner's engine is `host` until its owner sets `microvm`.) Decision 8: a switch drains, with an
explicit "End them now"; only `runners.manage` switches; the workspace default is in Settings → Runners, a
project's override shown. Rules: [principles.md](principles.md).

## The table

- **Agents in**: the engine agents' sessions run in here (box glyph microVM, server glyph host; ⊘ with Shards' why when
  it can't run microVMs; "→ host" while a switch drains). **CI in**: kiln's executors for jobs (host, docker, shards,
  k8s). Both titled with a tooltip, so the two "host"s aren't read as one.
- Labels keep to one line, faded at the cell's edge (all of them in its title), so no row grows as fonts arrive.

## A runner's details, top to bottom

1. Its name line, then **now**: CPU, memory and disk as live bars from its own samples, and on a microVM runner the
   vCPUs its Hex node holds (`RunnerNow`). The facts grid no longer repeats them; refused messages span it.
2. **Agents in**: the engine switch (microVM | host), Shards' checks as dots (microVM, isolation, host check, image
   class, each its words in a tooltip), the default and any project's override; a switch asks first ("Switch and
   drain"), then shows the drain with End now (asked) and Cancel the switch.
3. kiln's own: watched by, facts, agent sessions.
4. **MicroVMs**: starts by kind and warm hosts; a row per microVM (stand, its agent, linked to its Sandbox tab, image,
   start, uptime, CPU and memory as compact bars, pause (vCPUs) with Undo, restart, stop asked in place, telemetry
   folding out full-width plots); warm pools (size, warm hosts, idle, hit rate, Release asked in place); images
   (from, built, size, Build again, Remove asked in place, Shards' words on a failed build); Build image inline.
   Columns give way in order as the details narrow; the controls' column is fixed, so rows line up.

Code: `ui/src/views/runners.tsx` (table, details), `ui/src/views/runners-microvms.tsx` and `.css`.

## What the server answers (the mock, `ui/mock/microvms.ts`, `ui/mock/runners-ci.ts`)

- `GET /api/microvm/engines`, `GET /api/microvm/runners/:name`, `PUT /api/microvm/runners/:name/engine
  {engine | end | cancel}`; the microVMs' acts are the sandbox's (`POST /api/microvm/sessions/:id/:act`).
- The page's other panels: `GET /api/ci/pools`, `GET /api/ci/images`, `GET|PUT /api/runners/:name/headroom`.

## Loading

The page asks for the open runner's microVMs and the CI panels with the rest; with a runner open its placeholder holds
the details' place. Layout shift measured 0 at 1280, 900 and 390.
