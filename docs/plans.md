# Plans

A plan is what an agent will do, kept with the agent session it belongs to: written in its Plan
tab, made by the agent or from one of its goals, or saved from the plan it proposed; changed (each
change a version), shared by a link, and run on that agent or any other of any harness kiln runs
(Claude Code, Codex, Gemini CLI, opencode, Goose, Amp, ...). Each agent's Plan tab lists its own
plans (**Your plans**); there's no list of every plan apart from them. A plan's page, its runs and
its research lead back to the agent it's kept with.

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
  side by side; run on an agent, the steps side by side go to subagents of its own (below). Moving
  or removing a step keeps what the others wait for. A step can't wait for itself, or for a step
  that waits for it.
- **Kept by kiln**: the plan an agent proposes in plan mode (Claude Code's plan, waiting for approval
  or approved) is kept as one of the plans by itself: one an agent, a new version each time it
  proposes another, its title from its words (`source.proposed`; a plan longer than 16 KB keeps its
  head and tail). **Save plan** keeps another: a plan waiting for approval in the conversation, or
  an agent's to-do list. A plan keeps where it came from (agent, harness, machine, folder); naming
  only the agent (`source.session`) is enough, kiln fills in the rest.
- **Agents** keep theirs with kiln's MCP tools, each with its own session unless it names another
  (`source.session`): `plans_create` (and `plans_list`, `plans_get`,
  `plans_update`, `plans_versions`, `plans_run`, `plans_delete`), or `kiln.plans.*` in code mode.
  An agent on a runner has `plans_create`, `plans_list`, `plans_get`, `plans_update`, `plans_run`
  and the `plans_runs_*` tools as tools of its own (the rest in code mode). An agent that runs a
  plan without `session` or `new` runs it itself; with one agent per step (`mode: "agents"`), the
  steps' agents start in its harness, on its machine and in its folder unless it says otherwise.
  The kiln-plans skill says how.

A plan's **steps** are the ones written with their parts; a plan written only as text has the
items of its top-level lists (`1.`, `-`, `*`, `- [ ]`) as its steps (lines indented under an
item belong to it, and lists in code blocks don't count). The editor shows them as you type.

## Reading one

A plan opens as you read it: its title, its overview, then its steps, as their graph (when they
say what they wait for) and as cards, each its number and title, what it waits for, and what it is
(what to do, examples, acceptance criteria, tests), folded until asked. **Edit** opens the editor
(its words and its steps); **Done** goes back. Pick steps on their cards to run just those, or
press a step's own **Run** to run it alone: **Run it**, below, takes them.

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

A plan whose steps say what they wait for runs on an agent as its graph says (docs/agentic.md P2),
the steps that don't wait for each other side by side on subagents of the agent's own, as many at
once as its Subagents setting lets (**Subagents → At once**, one pool for all its subagents at
work; none set, no limit):

- **One message** says the order its steps go in ("1, then 2, 3 and 4 side by side, then 5") and
  asks the agent to give each step side by side a subagent of its own (its Agent or Task tool;
  without one, agents it starts), at most its setting at once.
- **Step by step** sends the steps that don't wait for each other in one message, in rounds: each
  round's steps side by side, a subagent each, the next round's message waiting for that turn.
- **Goals** go as docs/goals.md, "A plan's steps side by side", says: kiln starts a subagent for
  each step that can be under way beside the others, follows it, and takes its step as met when the
  subagent meets it; the agent takes the steps there's no room for.

A plan whose steps say nothing of waiting runs one step after another, as before.

With steps picked, only those run, each keeping its number; the steps they wait for outside them
are taken as done (the run says which), and one message isn't offered. As goals, **Each step as a
goal stack of its own** (on by default for steps with acceptance criteria or tests) puts each
step's criteria and tests on the stack at once, a goal each, before the step: the step is met
once they are.

From an agent: its **Plan** tab shows **Its plan**: the plan it proposed, as kiln keeps it, its steps
as cards to pick or run (**Run** opens the plan's page with them picked, **Run it** set to start
there), its words folded under them (**Its words**); and its to-do list, each item as a step stands
(done met, the one under way working, in the prism's shimmer, the rest waiting). Then **Your
plans** (write one, pick one, **Run here** any of the three ways) and its **Goal stack**. A plan's
page opened with `?pick=2,3` has those steps picked, its **Run it** in view, on the agent the plan
came from while it runs.

A run sends the saved version and is listed under the plan with the agent it went to.

### One agent per step

Each step is held to as a goal (as with **Goals**) by an agent of its own, started for it on the
machine, harness and folder chosen for the run, or for the step; the run has a page of its own
(each step with its agent, where it stands, what it waits for and what it changed) and Mission
Control groups its agents under it.

The run's page draws its steps as their graph, in rounds (each step after the latest step it waits
for), with a bar of where each stands, the way an agent's
conversation draws its work: a step at work with hyperlight's mark turning and its title in the
holographic shimmer; one needing a person with the optic held in amber; met, its loop closed; not
met, broken; skipped, dashed; stopped, faint; waiting, sketched and still. Only steps at work move.
A plan's page draws its steps the same way, without where they stand, so **Runs after** reads as a
graph while it's written. It runs down, a step a row at every width, a round's steps one under
another. A line that would run behind another step isn't drawn: the step it leads to says what it
waits for instead (**after 2, 3**).

- **Which steps run when.** A step starts once each step it waits for is met (**Runs after**;
  `needs` in the API). Steps that don't wait for each other run side by side: the run's page says
  which. In a plan whose steps say nothing of it, each step waits for the one before, or, run
  **side by side** (`parallel`), none waits at all. `max_parallel` caps how many are at work at
  once; an agent that runs a plan also has at most as many subagents at work at once as its
  Subagents setting says (one pool, whatever each does; none set, no limit), and further steps
  wait for room.
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

An agent turns a goal of its stack into a plan kiln keeps with `goals_plan` (docs/goals.md), as a
person does with **Make a plan**, and kiln does by itself for a big goal about to start (docs/goals.md,
"Big goals, planned first"): a plan in its **planning phase**, the goal's words its brief, with
research of its own (docs/research.md). The goal gets one part first, planning it; the plan isn't
run until it's ready.

1. **Research.** The agent looks inside (its machine's files and others': code, documents, data,
   pictures, frames of video; kiln's records: `conversations_search`, plans, CI runs, deployments)
   and outside (the web, standards and official docs, papers, books, blogs, forums), recording each
   finding with `research_add`: the claim, each source and the words quoted from it, and any
   counterclaims. kiln looks for each quote where it's cited and grades each finding (A to F, out
   of 100) by its sources' kinds, how far its citations hold up, how many independent sources
   agree and what's against it.
2. **Steps.** It writes the plan's steps (`plans_update`): each with what to do, its acceptance
   criteria and tests, the steps it waits for (`needs`), and the findings it rests on (`basis`).
3. **Ready.** `plans_ready` ends the planning phase. It refuses while the plan has no steps, its
   research has no sources inside or outside (unless the agent says why: `skipped`), or a step
   rests on a finding that isn't there. It answers with the research, as `research_get`'s markdown
   gives it, and with what's weak (a step resting on nothing, or on findings graded D or F; quotes
   still being checked): the agent shows the person the research in its reply, as it is, every
   claim with its sources and grade, and says what's weak. The plan's page shows the research too
   (**Being planned**, then **Research** under its steps), live while it's planned. Then, as `then`
   says:
   - `goals` (the default): the plan's steps become the goal's parts on the same agent, each a
     goal saying the step it's made from (`goals_split` turns one into its acceptance criteria and
     tests), the steps that don't wait for each other side by side on subagents of the agent's own
     (docs/goals.md, "A plan's steps side by side");
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

A plan's written steps are `items` in the API: `[{title, description, examples, acceptance, testing, needs?, basis?}]` (up to 100; `needs`: the steps it waits for, by number from 1; `basis`: the findings of its research it rests on, by id). A plan says its `phase` (`planning` or `ready`), its `research` (an id), and in `source.goal` the goal it was made from (`{agent, id, text}`). `acceptance` is a list of criteria: `{kind: "qualitative", text}` or `{kind: "quantitative", target, op, value, value2?, unit?, baseline?, measured?}`, `op` one of `lt`, `le`, `eq`, `ne`, `ge`, `gt`, `between` (`value2` the upper bound), `within` (`value2` the tolerance), `rises_at_least`, `falls_at_least`, `rises_at_most`, `falls_at_most` (`value` a percentage against `baseline`). `testing` is a list of `{scenario, criteria}`. A threshold is a number (`0`, or `"0"`). What's wrong with a step says which it is (`step 2: criterion 1: unknown variant …`). `steps` is what runs, one string a step, the criteria and tests as sentences.


| Method | Path | |
| --- | --- | --- |
| GET | `/api/plans` | every plan, newest change first; `session`: an agent's (made in its conversation), `proposed=1`: the plans agents proposed (plan mode), kept by kiln |
| POST | `/api/plans` | `{title?, body, tags?, source?}` |
| GET / PATCH / DELETE | `/api/plans/:id` | one plan; `{title?, body?, items?, tags?}` (a title given empty is taken from the text, else the first step, as for a new plan) |
| GET | `/api/plans/:id/versions` | every version, newest first |
| POST | `/api/plans/:id/restore` | `{version}`: made current again, as a new version |
| POST | `/api/plans/:id/ready` | `{then?: goals \| review \| agents, skipped?: {internal?, external?}}`: its planning phase ended (a plan made from a goal goes on as `then` says); answers `{plan, warnings, research?, say?, then?, goal?, run?, stack?}` (`research`: its research's markdown, for the agent to show) |
| POST | `/api/plans/:id/run` | `{session}` or `{new: {target, kind, cwd?}}`, `mode`: `message`, `steps`, `goals` or `agents`; `agents` takes `new` `{on, kind, cwd, model?, effort?, mode?}` for every step, `steps` `[{on?, kind?, cwd?, model?, effort?, mode?, agent?}]` for each (`agent`: `main` or an agent's id), `session` (the main agent), `parallel` and `max_parallel`, and answers `{run, steps, note}`; any mode but `message` takes `only` (step numbers from 1: just those, each keeping its number, the steps they wait for outside them taken as done), and `goals` takes `split` (each step a goal stack of its acceptance criteria and tests; answers `split`: how many were) |
| GET | `/api/plans/:id/runs` | runs, newest first |
| GET | `/api/plans/runs` | runs of one agent per step (`?active=1`: those under way) |
| GET | `/api/plans/runs/:run` | a run: each step's `{n, title, goal, stand, needs, given, agent, agent_id, reason, tries}`, its `state`, `reason`, `note` and stack |
| POST | `/api/plans/runs/:run/stop` | stop it |
| POST | `/api/plans/runs/:run/steps/:n/{retry,take,met}` | retry a step, take it over, say it's met |
| POST | `/api/plans/:id/share` | `{on}` |
| GET | `/api/shared/plans/:token` | **open**: a shared plan's title, text and steps |
