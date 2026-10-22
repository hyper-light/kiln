---
name: kiln-plans
description: Turn a document, a list or a request into a kiln goal stack you pursue yourself (saying each goal met, splitting one into parts, pausing for a while), or into a plan kiln keeps and runs (as one message, a step a turn, as goals, or with an agent of its own for each step, steps that don't wait for each other side by side, its work coming back as a stack of changes). Use when asked to make a goal stack or a plan, to work through a list item by item until each is done, to turn a plan step's acceptance criteria and tests into goals, or to hand each step of a plan to its own agent.
---

# Goal stacks and plans

kiln keeps two things that carry work on past one turn:

- **Your goal stack**: goals, one after another. kiln gives you each in turn and holds you to it
  until it's met, then gives you the next, with no person in between. Use it to work through a
  list yourself, item by item.
- **Plans**: a plan kept in kiln, for the person to read, change, share and run again. Running
  one gives it to an agent: as one message, a step a turn, each step a goal, or each step to an
  agent of its own.

The tools are `goals_get`, `goals_add`, `goals_done`, `goals_split`, `goals_update`,
`goals_set`, `goals_control`, and `plans_create`, `plans_list`, `plans_get`, `plans_update`,
`plans_run`, `plans_runs_list`, `plans_runs_get`, `plans_runs_stop`, `plans_runs_retry` (in
code mode `kiln.goals.*` and `kiln.plans.*`).

## Your own goal stack

Leave out `agent`: the goals are yours. Each goal is **one line** that says what done is: a
condition to meet ("`cargo test -p parser` passes on Linux and macOS") or an objective with its
end ("P3 is built: its tests pass and docs/plans.md describes it").

```js
// goals_add: one goal per item of the document, in its order
{ "goals": [
    "P1 is built: ... (what done is for P1)",
    "P2 is built: ...",
    "P3 is built: ..."
] }
```

Then **end your turn**. The first goal comes to you as soon as you're free (idle, nothing
queued, nothing waiting on a person), and each next one once the one before is met. How a goal
goes, and how it's judged met, depends on your harness:

| Harness | The goal comes as | It's met when |
|---|---|---|
| Claude Code | `/goal <condition>`: a check runs each time you'd stop, and you keep going until it holds | Claude Code's check says it's met |
| Codex | its thread goal (Codex's own `/goal`) | Codex says the goal is complete |
| the others | a message: "Your goal: …" | your reply ends with the line `GOAL MET` |

Until a goal is met, kiln sends it again whenever you stop short of it, also after a restart.
So don't stop to ask the person whether to go on to the next item: meeting the goal is the
signal. If one can't be met (it needs a person, a decision, something missing), say so plainly
and pause the stack, saying why (below); the person resumes or skips it.

- **When asked to pursue all of something** ("all the P items"), add every item. Don't trim the
  list, merge items, or add only the first few.
- **Where it stands**: `goals_get` gives each goal's `id` and status (pending, active, met,
  unmet, skipped), the goal it's a part of (`part_of`) and the plan step it was made from
  (`step`); and whether the stack runs, is paused (why, and `resume_at` when it resumes by
  itself) or is done.

### When a goal is met: `goals_done`

Once you've met the goal under way, **say so** with `goals_done`, and what shows it:

```js
// goals_done: the goal under way is met; the next comes to you at once
{ "evidence": "cargo test -p parser: 212 passed on Linux and macOS; docs/parser.md updated" }
```

It ends as met (your words kept with it for the person to see), your harness's own goal is
cleared, and the next goal comes at once. Your harness's check (Claude Code's `/goal`, Codex's
goal, the `GOAL MET` line) still counts too, but don't wait on it once the work is done and
shown. `id` says a goal other than the one under way is met (one you met along the way, or one
skipped earlier that turned out met). **Never skip a goal you met**: `skip` is for leaving a
goal unmet, and it's recorded that way.

### Splitting a goal: `goals_split`

A goal too big to pursue in one go becomes a goal stack of its own: its parts go right before
it, you pursue each in turn, then the goal itself comes back for its own check (often quickly
met: `goals_done`).

```js
// goals_split: the goal under way into parts, each saying what done is
{ "goals": ["the scanner reads numbers: 0x1F, 1e9 and 007 scan", "the scanner reads strings with escapes", "cargo test -p lexer passes"] }
```

A goal made from a plan's step (`step` in `goals_get`) needs no parts: `goals_split` alone
splits it into the step's acceptance criteria and its tests, a goal each ("…, acceptance
criterion 1 of 3: p99 latency is at most 200 ms"). Said met, a goal takes its parts still to go
with it.

### Changing one goal, pausing, resuming

- **One goal**: `goals_update` with its `id` and `text` (other words; the goal under way goes
  to you again as they say), `next`, `before` or `after` (another goal's id) to move it, or
  `remove` to take it out. `goals_add` takes `next`, `before` and `after` too, to add goals
  somewhere other than the end.
- **The whole list**: `goals_set` with the goals still to go as they should be (by `id`,
  reordered, edited, removed, added; the one under way may be among them: after others, it goes
  back among them and the first goes next).
- **Pausing**: `goals_control` `{action: "pause", why: "waiting for the deploy", resume_in_s:
  900}`. Paused, you aren't held to the goal and nothing is sent; with `resume_in_s` (or
  `resume_at`, a time) the stack resumes by itself then and the goal under way comes back to
  you, so pause rather than stop when you're waiting on something that takes a while. Without a
  time it waits until someone resumes it: `{action: "resume"}`. To wait on a person, ask with
  your harness's question tool: nothing is sent while a question or an approval waits on them.
- **The rest**: `goals_control` with `skip` (leave the one under way unmet), `clear`, or
  `restore` (bring back the goals cleared last).
- **Another agent's stack**: name it with `agent`. You line up and change goals only for
  yourself and the agents you started (and theirs).

## Plans

`plans_create` keeps a plan: a `title`, and its `body` in Markdown (the items of its top-level
lists are its steps) or its `items`, each step with `title`, `description`, `examples`,
`acceptance` (criteria: `{kind: "qualitative", text}` or a measure, `{kind: "quantitative",
target, op, value, unit}`), `testing` (`{scenario, criteria}`) and `needs` (the steps it waits
for, by number from 1). Keep a plan when the person should see, change or reuse it; for work
only you will do, a goal stack is enough.

Give steps `needs` when some don't depend on others: run with an agent per step, a step starts
once each step it needs is met, and steps that don't wait for each other run side by side.
Leave `needs` out of every step for steps run one after another. A step can't wait for itself
or for a step that waits for it.

`plans_run` runs a kept plan (`id`) in one of four `mode`s:

| mode | what happens |
|---|---|
| `message` | the whole plan in one message |
| `steps` | each step its own message, one a turn |
| `goals` | each step a goal in the agent's goal stack, with its acceptance criteria and tests in the goal |
| `agents` | each step an agent of its own, held to the step as a goal; the next starts once the one before is met |

**On you**: leave out `session` and `new` (modes `message`, `steps`, `goals`). As with your own
goals, end your turn after `plans_run` in `goals` mode: the first step comes when you're free.
Each step's goal says the step it was made from: `goals_split` turns the one under way into its
acceptance criteria and tests, a goal each, before the step itself is checked.

**On an agent you started**: `session` is its id. **On a new agent**: `new` `{on, kind, cwd}`;
it's your child.

## One agent per step

`plans_run` with `mode: "agents"` holds each step to its goal with an agent of its own, each
starting once the steps it needs are met: steps that don't wait for each other run side by
side (in a plan whose steps say no `needs`, each starts once the one before is met; `parallel:
true` runs them all side by side). `max_parallel` caps how many are at work at once. Each agent
is your child: no more permissions than you, the same rules (the kiln-agents skill). By default
each runs in your harness, on your machine and in your folder; `new` `{on, kind, cwd, model,
effort, mode}` changes that for every step, and `steps` (a list in step order) for one step.

Steps side by side in one folder share it: brief each to keep to its own files, or give them
folders or machines of their own with `steps` (the run's `note` says which run together).

**A step for you**: `steps` `[{agent: "main"}, …]` gives that step to you (the main agent: you,
or the agent `session` names) rather than starting an agent: its goal goes on your own goal
stack, after your others, and the run goes on once you've met it (`goals_done`). Split it into
its acceptance criteria and tests with `goals_split` like any step's goal. An agent's id there
gives the step to that agent (one you started).

```js
// code_run: a plan whose steps 2 and 3 both wait for step 1 (and run side by side), step 1 yours
const { id } = await kiln.plans.create({ title: "Search", items: [
  { title: "Schema", acceptance: [{ kind: "qualitative", text: "the migration applies" }] },
  { title: "API", needs: [1] },
  { title: "UI", needs: [1] },
] });
const { run, note } = await kiln.plans.run({ id, mode: "agents", steps: [{ agent: "main" }] });
return { run, note };
```

```js
// code_run: run a kept plan with an agent per step, wait on the step under way, say where it stands
const { run } = await kiln.plans.run({ id: input.plan, mode: "agents" });
const now = (await kiln.plans.runs.get({ run })).steps.find(s => s.stand === "working");
if (now?.agent_id) await kiln.agents.wait({ agents: [now.agent_id], timeout_s: 600 });
const r = await kiln.plans.runs.get({ run });
return { run, state: r.state, met: `${r.met}/${r.total}`, steps: r.steps.map(s => ({ n: s.n, title: s.title, stand: s.stand })) };
```

`plans_runs_get` gives each step's goal, its agent (`agent_id`), the steps it waits for
(`needs`), whether it was given to an agent that was there (`given`) and where it stands
(`stand`: waiting, working, needs_you, yours, met, unmet, stopped), what it changed, and the
run's `state` (running, stopped, done) and why. A step not met holds up the steps that wait for
it while the rest go on; once nothing is at work the run stops, saying which steps hold it up.
`plans_runs_stop` stops the run (no next step; the steps under way are interrupted, and a step's
goal on an agent it was given to is taken out of its stack); `plans_runs_retry` `{run, step}`
tries a step again with a new agent (a step given to an agent: its goal again on that agent's
stack), and the run goes on from there.

In a git repository, each step's work goes up for review as it's met: one commit per step on
the commit of the step met before it, as a stack of changes (one diff per step on a repository
kiln hosts, a pull request per step onto the one before elsewhere). What reviewers and CI say
about a step reaches its agent when the run ends.

## What a person decides

Approvals, questions and plans that an agent of a run asks a person are the person's to answer.
A step whose `stand` is `needs_you` waits on a person: tell them which step and what it asks.
