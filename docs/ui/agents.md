# Agents: definitions, their editor, versions, runs and history (design)

Serves kiln plan 01a1cd8e862e7169a074db3d5fa33f3d's build plan P5.1 and P5.2, and P5.4's conflict display, from
the kiln × Hex × Shards design, section B. Kiln's ten harnesses stay its fixed set (the person, 2026-11-14);
nothing here adds a harness kind.

Status: design for the person's approval. Nothing here is built. The mockup is
[mockups/agents.html](mockups/agents.html).

## Names

- The API is `agentdefs.{list,get,create,update,versions,restore,delete,check,run}` on `/api/agentdefs`, with MCP
  `agentdefs_*` and code mode `kiln.agentdefs.*`, and the capability `agentdefs.manage`. `agents.*` and
  `/api/agents` are sessions' and runners'.
- The page reads **Agent definitions** at `/agents/definitions`, under Agents beside Agentfiles, Plans and
  Research (crumbs `05 / Agents / Definitions`). Its command-palette entry is "Agent definitions: in Agents,
  agents as you define them". See open question 1.

## The list (`/agents/definitions`)

It's laid out exactly as the Agentfiles page (`views/agentfiles/agentfiles.tsx`, its `af-*` classes).

- **Yours**:
  - each row: the name (mono link to its editor), then `Claude Code · Opus 5.5 · in rust-dev · v4 · changed 3
    hours ago`, then **Run**;
  - **New agent** opens a form in place at the head of the list: name, harness (one of the ten), Agentfile.
    "Keep it" makes v1 and opens its editor.
- **kiln's**: one for each of kiln's seven CI skills (ci-diagnose, ci-review, ci-performance, ci-migrate,
  ci-author, ci-secrets, ci-runners), read only, with **Read** (its settings in place, as `.af-read`) and
  **Copy as yours**.
- **Run**, on a row or in the editor, opens a run form in place under it: no dialog.
  - The form: project, where (`kiln places it`, or one of your runners), and a first message.
  - Above Start, one mono line says where kiln would place it and why: "kiln places it on
    adas-mac-studio-3.local: a microvm runner with rust-dev v8 built for linux/arm64 and a warm reviewer
    waiting". kiln's load balancer gives that reason; the page makes up nothing.
  - Start says it's on its way, then: "Started reviewer on adas-mac-studio-3.local from a warm microVM in
    212 ms: Claude Code · Opus 5.5 · rust-dev v8." and "Open it in Mission control".
  - It runs through `agentdefs.run`.

## The editor (`/agents/definitions/:id`)

- The head:
  - crumbs, then the name as `af-title`, with **Run** at its right;
  - a line: `v4 · changed 3 hours ago · written as AGENT reviewer in rust-dev · in step with it`, the last part
    in the ok tone, or the warn tone when it differs.
- Three tabs in the agent page's rail style (`.pt`): **Settings**, **Versions** (its count), **History** (its
  count).

### Settings

Each section is an `af-section`, with a hairline above it.

- **Its name, Harness, Model.**
  - Harness is a native `.select` of the ten.
  - Model follows the harness's `KindInfo.models`.
  - A harness that lists none (it hasn't run on any of your machines) offers "Its default", and says so under it.
- **Its microVM.**
  - Agentfile: your Agentfiles, then kiln's per-harness ones.
  - The line under it says what saving writes: "Saving writes its `AGENT reviewer`, `ATTACH reviewer FOR
    claude`, `SKILL` and `MCP … FOR reviewer` lines into rust-dev, as a new version of it, and its runners
    build it again."
  - Choosing one of kiln's (read only) says the lines go into a copy of it, made yours when you save.
- **Instructions and Playbook.**
  - Two textareas.
  - The line under them names the file the harness reads them from: `CLAUDE.md` for claude, `.goosehints` for
    goose, `AGENTS.md` for the rest (kiln's `harness/homes.rs`).
- **Tools and MCP.**
  - kiln's own tool groups as `.check` boxes: CI runs and legs, Deployments, Agents, Plans and goals, Research,
    Machines, Code search. These reach the harness over the link kiln drives it by, never the network.
  - Then other MCP servers, one row each (name, source, "reaches api.githubcopilot.com:443 alone", Remove), and
    an add row.
  - Each is written as `MCP <name> FROM <source> FOR <agent>`: a grant to its origin alone.
- **Skills.** `.check` boxes, each marked kiln's or yours. Each is written `SKILL ./skills/<name> FOR <agent>`.
- **Limits.**
  - Time, tokens and cost, each per session.
  - Empty means none. The person sets their own, so no number of ours appears.
  - "One reached ends the session's turn and says which."
- **Save and Discard changes**, as the Agentfile editor has them (Save disabled until something changed).
  - What came of it is said under them: "Kept as v5. rust-dev's AGENT reviewer lines changed to match, as
    rust-dev v10; its runners are building it."

### The conflict (P5.4)

When the Agentfile's text changed the agent's lines after the editor opened, Save keeps nothing. In its place
the page shows a block with the warn edge (`.ag-conflict`):

- what happened: "rust-dev's text changed while you edited: v9, by you in the Agentfile editor, 4 min ago,
  rewrote AGENT reviewer. Nothing is kept until you choose.";
- a three-column table: Setting | Here | In rust-dev v9, with the differing cells in the warn tone;
- **Keep mine: rewrite its lines**, **Take the Agentfile's**, and **Open rust-dev**.

The mockup has a checkbox that makes the next save meet this.

### Versions

- Kept like the Agentfile's versions: `v4 · 3 hours ago · you · model Sonnet 5.5 → Opus 5.5 · current`, with
  Read and Make current.
- Each version says what changed and where from ("from rust-dev's text" when the Agentfile changed it).
- Making one current keeps it as a new version and rewrites the Agentfile's lines.

### History

A `data-table` of the definition's sessions:

| Column | Example |
|---|---|
| State | a dot |
| Started | 12 min ago |
| Project | hyper-light/kiln |
| Runner | adas-mac-studio-3.local |
| Engine | microvm |
| Version used | v4 |
| Start | `warm · 212 ms`, `template · 0.41 s`, `cold · 1.9 s` or `process · 0.3 s` |
| Outcome | reviewed 3 files · 2 findings |

Each row opens the session.

### Delete

Asked in place with a danger `Confirm`: "Delete reviewer and its 4 versions? Its AGENT reviewer lines leave
rust-dev as a new version of it; its sessions keep their history."

## What it needs from the server

- `agentdefs.*` as named above.
- `agentdefs.get` returns:
  - the definition;
  - its Agentfile's id and version;
  - `sync: "in_step" | "differs"`, with the differing fields and the Agentfile version that differs.
- `update` with a stale base answers 409, with the same differing fields, so the conflict block shows them.
- `run` returns the session id and how it started (warm, template, cold), and the placement's reason before it
  starts, from `agentdefs.check` or a dry run.
- History: the sessions started from a definition, with the version each used.

## Tests (UI, Chromium and WebKit, desktop and phone)

- P5.2 AC1: create an agent, edit it, see v2 in Versions, make v1 current, see v3.
- P5.2 AC2:
  - Run starts a session;
  - the session's harness, model and Agentfile match the definition's (its page and `agents.get`);
  - History lists it.
- P5.1 AC2: another person's definition URL shows "No agent definition here", as the Agentfile page does.
- P5.4:
  - editing the definition changes the Agentfile's text;
  - editing the Agentfile's text changes the definition;
  - a stale save shows the conflict block, and nothing is kept until a choice.

## Open questions for the person

1. **The page's name.** The nav's Agents is Mission control (running agents). ROOT says the page can read
   "Agents". The mockup calls it **Agent definitions**, at `/agents/definitions`, to keep the two apart. Keep
   that, or another name?
2. **kiln's own agents.** The mockup offers one per CI skill (seven). Should kiln ship those, others, or none
   (yours alone)?
3. **Run's "Where".** Should a person be able to pin a run to one runner, or only let kiln place it?
4. **What a version covers.** A version of the definition keeps its settings. Should Make current also put back
   the Agentfile version it ran in, or only the agent's own lines (the mockup)?
