# Agents you manage: Mission's dropdown, the Agents tab, chat

An agent as you define it (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.1, P5.2, P5.4): one of kiln's ten harnesses,
its model, its Agentfile, instructions, tools, skills and limits, versioned; the sessions started from it; and its
experiment (see [experiments.md](experiments.md)). Rules: [principles.md](principles.md).

## Where

- **Mission control**: a session started from one of your definitions gets the row's ⫷ dropdown (kiln's subagents
  toggle), even with no subagents. Opened, in this order, one line each until opened, in the tree's own columns:
  - SUBAGENTS: the existing tree, unchanged (its head only when the groups below follow);
  - DEFINITION: name and version, where it runs and how fast it starts (engine glyph, warm hosts of the target as
    dots, start p50 by kind), its model, Run, ⋯ (History, New experiment, Duplicate, Delete…; kiln's: Copy as yours).
    Opened: its values, each edited in place, a save a new version;
  - SESSIONS: a stand dot per session, "n · m at work". Opened: a plain row per session (stand, version, warn tone
    when behind, its variant's swatch and name, its sandbox line, compact CPU and memory bars, pause, stop, open);
  - EXPERIMENT: the split as swatches, its weights, "n · splitting", pause.
  - `?open=ID,…` opens rows' dropdowns.
- **Every Mission row** says where it runs: engine glyph, runner, start glyph and ms (a session not yet listed: what
  its start chose, Host or microVM; never its machine's).
- **The Agents tab** (agent page): this session's agent first (name, harness, Agentfile, where it runs; its version,
  "Restart on vN" when behind; its variant's swatch, name and weight; its values), then Yours / kiln's: a row per
  definition (name, stand dots, at work, where it runs, Run, ⋯) whose chevron opens the same groups.
- **The head**: "reviewer v3 →v4" and the variant's swatch, a crumb that opens the Agents tab.
- **Chat**: a kiln MCP call about a definition (`agentdefs_*`) shows the same row, live while on screen, "as of" its
  time further back, ↻ to go live.
- kiln's definitions (one per harness, per CI skill, and the UI's) are listed under kiln's, read only.

Code: `ui/src/views/harness/definitions.tsx` (groups, rows, card), `definition-panel.tsx` (this agent, the crumb),
`agents-tab.tsx`, `variants.ts`; data `ui/src/api/agentdefs.ts`.

## What the server answers (the mock, `ui/mock/agentdefs.ts`)

- `GET /api/agentdefs`, `GET /api/agentdefs/:id`: each definition with `runs` (`engine`, `warm`, `target`,
  `p50_ms` by kind), `sessions` (id, version, variant id) and its `experiment`.
- Not in the mock yet (the UI changes them locally, with toasts): saves, versions and restore, Run, the experiment's
  edits. The MCP tools exist as `agentdefs.{list,get,create,update,versions,restore,delete,check,run}`.

## Open

- A save carrying its base version, and a save in between shown as a diff on the row (principle 5).
- One expanded row per list, sticky, keys, Back (principle 1); peeks (principle 2); ⌘K entries (principle 4).
