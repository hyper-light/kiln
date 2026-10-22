# Plans

A plan is what an agent will do, kept apart from any one session: written in kiln or saved from an
agent's plan, changed (each change a version), shared by a link, and run at will on any agent of
any harness kiln runs (Claude Code, Codex, Gemini CLI, opencode, Goose, Amp, ...).

## Keeping one

- **In an agent's Plan tab**, under **Your plans**: **New plan**, then its steps one at a time,
  each with its own **description**, **examples**, **acceptance criteria** and **tests**; an
  optional overview for what every step should know. Save it, change it (**Edit**), and run it on
  that agent, without leaving it.

  A step's **acceptance criteria** are as many as it needs, each **in words** or **a measure**: a
  target ("p99.9 latency"), how it compares (is at most, less than, at least, more than, exactly,
  not, between two values, within a tolerance of one; or rises or falls by at least, or by no more
  than, a percentage compared with a baseline you name), the threshold and its unit, and,
  optionally, what it's measured by. A measure reads as one sentence, shown under it as you write
  it and sent to the agent as written: "p99.9 latency is at most 200 ms, measured by the soak
  test". Its **tests** are as many as it needs too: each a scenario and the criteria it passes by
  (in words or measures, the same way).

  A step **runs after** the steps it waits for (tick them under **Runs after**): run with an
  agent per step, it starts once each of them is met, and steps that don't wait for each other run
  side by side. Moving or removing a step keeps what the others wait for. A step can't wait for
  itself, or for a step that waits for it.
- **Agents → Plans → New plan**: the same, with tags, and the plan's text in markdown as well.
- **Save plan** under a plan an agent proposes: a plan-mode plan waiting for approval, the plan an
  agent laid out, or its todo list. The plan keeps where it came from (agent, harness, machine,
  folder); naming only the agent (`source.session`) is enough, kiln fills in the rest.
- **Agents** keep theirs with kiln's MCP tools: `plans_create` (and `plans_list`, `plans_get`,
  `plans_update`, `plans_versions`, `plans_run`, `plans_delete`), or `kiln.plans.*` in code mode.
  An agent on a runner has `plans_create`, `plans_list`, `plans_get`, `plans_update`, `plans_run`
  and the `plans_runs_*` tools as tools of its own (the rest in code mode). An agent that runs a
  plan without `session` or `new` runs it itself; with one agent per step (`mode: "agents"`), the
  steps' agents start in its harness, on its machine and in its folder unless it says otherwise.
  The kiln-plans skill says how.

A plan's **steps** are the ones written with their parts; a plan written only as text has the
items of its top-level lists (`1.`, `-`, `*`, `- [ ]`) as its steps (lines indented under an
item belong to it, and lists in code blocks don't count). The editor shows them as you type.

## Changing one

Saving a new title, text or steps makes a new version; the old ones stay, readable, and any one
can be made current again (as a new version, so nothing is lost). Tags change without a new
version.

## Running one

From the plan: **Run it**, on an agent that runs or on a new one (machine, harness, folder):

- **One message**: the whole plan, asking the agent to carry it out.
- **Step by step**: each step its own message (with its parts), waiting in the agent's queue until
  the turn before it ends, so the agent does one step a turn.
- **Goals**: each step a goal in the agent's goal stack (docs/goals.md): the next goes once the one
  before is met, as its harness's own `/goal` where it has one. A step's goal is one line: its
  title, what to do, its examples, "Done when:" its acceptance criteria and "Tests:" its tests, so
  `/goal` both works toward it and checks it. Each goal says the step it was made from, so the
  agent can split it into a goal stack of its own (`goals_split`): its acceptance criteria and
  tests, a goal each, pursued first, then the step itself for its check.
- **One agent per step**: below.

From an agent: its **Plan** tab shows the plan it proposed (with **Save plan**), **Your plans** (write
one, pick one, **Run here** any of the three ways) and its **Goal stack**.

A run sends the saved version and is listed under the plan with the agent it went to.

### One agent per step

Each step is held to as a goal (as with **Goals**) by an agent of its own, started for it on the
machine, harness and folder chosen for the run, or for the step; the run has a page of its own
(each step with its agent, where it stands, what it waits for and what it changed) and Mission
Control groups its agents under it.

The run's page draws its steps as their graph, in rounds (each step after the latest step it waits
for, a round's steps side by side), with a bar of where each stands, the way an agent's
conversation draws its work: a step at work with hyperlight's mark turning and its title in the
holographic shimmer; one needing a person with the optic held in amber; met, its loop closed; not
met, broken; skipped, dashed; stopped, faint; waiting, sketched and still. Only steps at work move.
A plan's page draws its steps the same way, without where they stand, so **Runs after** reads as a
graph while it's written.

- **Which steps run when.** A step starts once each step it waits for is met (**Runs after**;
  `needs` in the API). Steps that don't wait for each other run side by side: the run's page says
  which. In a plan whose steps say nothing of it, each step waits for the one before, or, run
  **side by side** (`parallel`), none waits at all. `max_parallel` caps how many are at work at
  once; an agent that runs a plan also has at most `[agents] max_children` agents of its own at
  once, and further steps wait for room.
- **One folder.** Steps side by side in one folder share it, and the run says so: brief each
  step to keep to its own files, or give steps folders or machines of their own (`steps`). Until
  agents have workspaces of their own (docs/agentic.md, W: held with Slates), nothing keeps them
  apart there.
- **A step for the main agent.** A step can go to an agent that's there already rather than to
  one started for it: `steps` `[{agent: "main"}]` gives it to the run's main agent (the agent
  that ran the plan, or the one `session` names), and an agent's id gives it to that agent (one
  you may steer). The step's goal goes on that agent's own goal stack, after its other goals, and
  the run goes on once it's met there; no agent is started for it.
- **Splitting a step.** The agent holding a step (its own, or the main agent) splits the step's
  goal into a goal stack of its own with `goals_split`: one goal per acceptance criterion and per
  test, pursued first, then the step's goal comes back for its check (docs/goals.md, "Agents work
  their own stack"). An agent says a goal is met with `goals_done`, and what shows it, which the
  step keeps as how it was met.
- **When a step isn't met.** It holds up the steps that wait for it; the others go on. Once
  nothing is at work and nothing more can start, the run stops, saying which steps hold it up:
  **Retry with a new agent** (a step given to an agent: its goal on that agent's stack again) or
  **Take it over** (kiln stops holding the agent to it; **It's met: go on** when it is).
  **Stop the run** lets every step under way go: their own agents interrupted, and a step's goal
  on an agent it was given to taken out of that agent's stack.
- **What comes out.** In a git repository, each step's work goes up for review as it's met, as
  one commit on the commit of the step met before it: the run ends as a stack with one diff per
  step, in the order they were met (a step always after the steps it waits for).

## Planning a goal

An agent turns a goal of its stack into a plan kiln keeps with `goals_plan` (docs/goals.md): a plan
in its **planning phase**, the goal's words its brief, with research of its own (docs/research.md).
The goal gets one part first, planning it; the plan isn't run until it's ready.

1. **Research.** The agent looks inside (its machine's files and others': code, documents, data,
   pictures, frames of video; kiln's records: `conversations_search`, plans, CI runs, deployments)
   and outside (the web, standards and official docs, papers, books, blogs, forums), recording each
   finding with `research_add`: the claim, each source and the words quoted from it, and any
   counterclaims. kiln looks for each quote where it's cited and grades each finding (A to F, out
   of 100) by its sources' kinds, how far its citations hold up, how many independent sources
   agree and what's against it.
2. **Steps.** It writes the plan's steps (`plans_update`): each with what to do, its acceptance
   criteria and tests, the steps it waits for (`needs`), and the findings it rests on (`basis`).
3. **Presented.** It presents the research as `research_get`'s markdown gives it: every claim with
   its sources and grade. The plan's page shows it too (**Being planned**, then **Research** under
   its steps), live while it's planned.
4. **Ready.** `plans_ready` ends the planning phase. It refuses while the plan has no steps, its
   research has no sources inside or outside (unless the agent says why: `skipped`), or a step
   rests on a finding that isn't there; and it answers with what's weak (a step resting on nothing,
   or on findings graded D or F; quotes still being checked), which the agent says. Then, as
   `then` says:
   - `goals` (the default): the plan's steps become the goal's parts on the same agent, each a
     goal saying the step it's made from (`goals_split` turns one into its acceptance criteria and
     tests);
   - `review`: the same, and the goal stack pauses for a person to look at the plan first; resuming
     it carries on;
   - `agents`: the plan runs with an agent per step, as its graph says; the goal waits for the run
     (a part held for it, nothing sent to its agent for it).

Once every step is met (or the run is done), the goal is met too, at once, with no check of its
own: "Met with its plan: its 4 steps are met."

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

A plan's written steps are `items` in the API: `[{title, description, examples, acceptance, testing, needs?, basis?}]` (up to 100; `needs`: the steps it waits for, by number from 1; `basis`: the findings of its research it rests on, by id). A plan says its `phase` (`planning` or `ready`), its `research` (an id), and in `source.goal` the goal it was made from (`{agent, id, text}`). `acceptance` is a list of criteria: `{kind: "qualitative", text}` or `{kind: "quantitative", target, op, value, value2?, unit?, baseline?, measured?}`, `op` one of `lt`, `le`, `eq`, `ne`, `ge`, `gt`, `between` (`value2` the upper bound), `within` (`value2` the tolerance), `rises_at_least`, `falls_at_least`, `rises_at_most`, `falls_at_most` (`value` a percentage against `baseline`). `testing` is a list of `{scenario, criteria}`. A threshold is a number. `steps` is what runs, one string a step, the criteria and tests as sentences.


| Method | Path | |
| --- | --- | --- |
| GET | `/api/plans` | every plan, newest change first |
| POST | `/api/plans` | `{title?, body, tags?, source?}` |
| GET / PATCH / DELETE | `/api/plans/:id` | one plan; `{title?, body?, items?, tags?}` (a title given empty is taken from the text, else the first step, as for a new plan) |
| GET | `/api/plans/:id/versions` | every version, newest first |
| POST | `/api/plans/:id/restore` | `{version}`: made current again, as a new version |
| POST | `/api/plans/:id/ready` | `{then?: goals \| review \| agents, skipped?: {internal?, external?}}`: its planning phase ended (a plan made from a goal goes on as `then` says); answers `{plan, warnings, then?, goal?, run?, stack?}` |
| POST | `/api/plans/:id/run` | `{session}` or `{new: {target, kind, cwd?}}`, `mode`: `message`, `steps`, `goals` or `agents`; `agents` takes `new` `{on, kind, cwd, model?, effort?, mode?}` for every step, `steps` `[{on?, kind?, cwd?, model?, effort?, mode?, agent?}]` for each (`agent`: `main` or an agent's id), `session` (the main agent), `parallel` and `max_parallel`, and answers `{run, steps, note}` |
| GET | `/api/plans/:id/runs` | runs, newest first |
| GET | `/api/plans/runs` | runs of one agent per step (`?active=1`: those under way) |
| GET | `/api/plans/runs/:run` | a run: each step's `{n, title, goal, stand, needs, given, agent, agent_id, reason, tries}`, its `state`, `reason`, `note` and stack |
| POST | `/api/plans/runs/:run/stop` | stop it |
| POST | `/api/plans/runs/:run/steps/:n/{retry,take,met}` | retry a step, take it over, say it's met |
| POST | `/api/plans/:id/share` | `{on}` |
| GET | `/api/shared/plans/:token` | **open**: a shared plan's title, text and steps |
