# Plans

A plan is what an agent will do, kept apart from any one session: written in kiln or saved from an
agent's plan, changed (each change a version), shared by a link, and run at will on any agent of
any harness kiln runs (Claude Code, Codex, Gemini CLI, opencode, Goose, Amp, ...).

## Keeping one

- **Agents → Plans → New plan**: a title and the plan in markdown, with tags.
- **Save plan** under a plan an agent proposes: a plan-mode plan waiting for approval, the plan an
  agent laid out, or its todo list. The plan keeps where it came from (agent, harness, machine,
  folder); naming only the agent (`source.session`) is enough, kiln fills in the rest.
- **Agents** keep theirs with kiln's MCP tools: `plans_create` (and `plans_list`, `plans_get`,
  `plans_update`, `plans_versions`, `plans_run`, `plans_delete`), or `kiln.plans.*` in code mode.

A plan's **steps** are the items of its top-level lists (`1.`, `-`, `*`, `- [ ]`); lines indented
under an item belong to it, and lists in code blocks don't count. The editor shows them as you
type.

## Changing one

Saving a new title or text makes a new version; the old ones stay, readable, and any one can be
made current again (as a new version, so nothing is lost). Tags change without a new version.

## Running one

From the plan: **Run it**, on an agent that runs or on a new one (machine, harness, folder):

- **One message**: the whole plan, asking the agent to carry it out.
- **Step by step**: each step its own message, waiting in the agent's queue until the turn before it
  ends, so the agent does one step a turn.
- **Goals**: each step a goal in the agent's goal stack (docs/goals.md): the next goes once the one
  before is met, as its harness's own `/goal` where it has one.

From an agent: its **Plan** tab shows the plan it proposed (with **Save plan**) and **Run a saved
plan**, which runs any kept plan on that agent the same two ways.

A run sends the saved version and is listed under the plan with the agent it went to.

## Sharing one

**Anyone with the link can read it**: a read-only page at `/shared/plans/<token>` that needs no
sign-in and shows the plan's words only, never where it came from. It opens for whoever can reach
the address in the link (pick it under **Link through**: the one you're on, the server's front
doors, its public URL). kiln has no internet-facing address unless you give it one, so that is
usually someone on your network or tailnet; for a front door's `https://` name, on a device that
trusts kiln's certificate. Turning it off kills the link;
turning it on again makes a new one. **Export as markdown** downloads it; the shared page has **Copy
the plan**, for keeping it in another kiln.

## API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/plans` | every plan, newest change first |
| POST | `/api/plans` | `{title?, body, tags?, source?}` |
| GET / PATCH / DELETE | `/api/plans/:id` | one plan; `{title?, body?, tags?}` |
| GET | `/api/plans/:id/versions` | every version, newest first |
| POST | `/api/plans/:id/restore` | `{version}`: made current again, as a new version |
| POST | `/api/plans/:id/run` | `{session}` or `{new: {target, kind, cwd?}}`, `mode`: `message` or `steps` |
| GET | `/api/plans/:id/runs` | runs, newest first |
| POST | `/api/plans/:id/share` | `{on}` |
| GET | `/api/shared/plans/:token` | **open**: a shared plan's title, text and steps |
