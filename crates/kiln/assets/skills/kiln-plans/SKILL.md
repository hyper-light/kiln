---
name: kiln-plans
description: Turn a document, a list or a request into a kiln goal stack you pursue yourself, or into a plan kiln keeps and runs (as one message, a step a turn, as goals, or with an agent of its own for each step, its work coming back as a stack of changes). Use when asked to make a goal stack or a plan, to work through a list item by item until each is done, or to hand each step of a plan to its own agent.
---

# Goal stacks and plans

kiln keeps two things that carry work on past one turn:

- **Your goal stack**: goals, one after another. kiln gives you each in turn and holds you to it
  until it's met, then gives you the next, with no person in between. Use it to work through a
  list yourself, item by item.
- **Plans**: a plan kept in kiln, for the person to read, change, share and run again. Running
  one gives it to an agent: as one message, a step a turn, each step a goal, or each step to an
  agent of its own.

The tools are `goals_get`, `goals_add`, `goals_set`, `goals_control`, and `plans_create`,
`plans_list`, `plans_get`, `plans_update`, `plans_run`, `plans_runs_list`, `plans_runs_get`,
`plans_runs_stop`, `plans_runs_retry` (in code mode `kiln.goals.*` and `kiln.plans.*`).

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
signal. If one can't be met (it needs a person, a decision, something missing), say so plainly;
the person pauses or skips it.

- **When asked to pursue all of something** ("all the P items"), add every item. Don't trim the
  list, merge items, or add only the first few.
- **Where it stands**: `goals_get` gives each goal's status (pending, active, met, unmet,
  skipped) and whether the stack runs, is paused or is done.
- **Changing it**: `goals_set` with the goals not yet started as they should be (by `id`,
  reordered, edited, removed, added); `goals_control` with `pause`, `start` (resume), `skip`
  (leave the one under way) or `clear`.
- **Another agent's stack**: name it with `agent`. You line up and change goals only for
  yourself and the agents you started (and theirs).

## Plans

`plans_create` keeps a plan: a `title`, and its `body` in Markdown (the items of its top-level
lists are its steps) or its `items`, each step with `title`, `description`, `examples`,
`acceptance` (criteria: `{kind: "qualitative", text}` or a measure, `{kind: "quantitative",
target, op, value, unit}`) and `testing` (`{scenario, criteria}`). Keep a plan when the person
should see, change or reuse it; for work only you will do, a goal stack is enough.

`plans_run` runs a kept plan (`id`) in one of four `mode`s:

| mode | what happens |
|---|---|
| `message` | the whole plan in one message |
| `steps` | each step its own message, one a turn |
| `goals` | each step a goal in the agent's goal stack, with its acceptance criteria and tests in the goal |
| `agents` | each step an agent of its own, held to the step as a goal; the next starts once the one before is met |

**On you**: leave out `session` and `new` (modes `message`, `steps`, `goals`). As with your own
goals, end your turn after `plans_run` in `goals` mode: the first step comes when you're free.

**On an agent you started**: `session` is its id. **On a new agent**: `new` `{on, kind, cwd}`;
it's your child.

## One agent per step

`plans_run` with `mode: "agents"` starts an agent for the first step, holds it to the step as a
goal, and starts the next step's agent once it's met. Each is your child: no more permissions
than you, the same rules (the kiln-agents skill). By default each runs in your harness, on your
machine and in your folder; `new` `{on, kind, cwd, model, effort, mode}` changes that for every
step, and `steps` (a list in step order) for one step. `parallel: true` starts the steps
together, though in one plain folder they still run one at a time (the run says why).

```js
// code_run: run a kept plan with an agent per step, wait on the step under way, say where it stands
const { run } = await kiln.plans.run({ id: input.plan, mode: "agents" });
const now = (await kiln.plans.runs.get({ run })).steps.find(s => s.stand === "working");
if (now?.agent_id) await kiln.agents.wait({ agents: [now.agent_id], timeout_s: 600 });
const r = await kiln.plans.runs.get({ run });
return { run, state: r.state, met: `${r.met}/${r.total}`, steps: r.steps.map(s => ({ n: s.n, title: s.title, stand: s.stand })) };
```

`plans_runs_get` gives each step's goal, its agent (`agent_id`) and where it stands (`stand`:
waiting, working, needs_you, yours, met, unmet, stopped), what it changed, and the run's `state`
(running, stopped, done) and why. `plans_runs_stop` stops the
run (no next step; the steps under way are interrupted); `plans_runs_retry` `{run, step}` tries
a step again with a new agent, and the run goes on from there.

In a git repository, each step's work goes up for review as it's met: one commit per step on
the step before's, as a stack of changes (one diff per step on a repository kiln hosts, a pull
request per step onto the one before elsewhere). What reviewers and CI say about a step reaches
its agent when the run ends.

## What a person decides

Approvals, questions and plans that an agent of a run asks a person are the person's to answer.
A step whose `stand` is `needs_you` waits on a person: tell them which step and what it asks.
