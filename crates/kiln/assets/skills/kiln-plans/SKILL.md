---
name: kiln-plans
description: Turn a document, a list or a request into a kiln goal stack you pursue yourself (saying each goal met, splitting one into parts, pausing for a while), turn a goal into a researched plan (research inside and outside, each finding graded by kiln from its sources, then steps that rest on them), or into a plan kiln keeps and runs (as one message, a step a turn, as goals, or with an agent of its own for each step, steps that don't wait for each other side by side, its work coming back as a stack of changes). Use when asked to make a goal stack or a plan, to plan a goal, to research something and present what you found with its sources and how far to trust it, to work through a list item by item until each is done, to turn a plan step's acceptance criteria and tests into goals, or to hand each step of a plan to its own agent.
---

# Goal stacks and plans

kiln keeps two things that carry work on past one turn:

- **Your goal stack**: goals, one after another. kiln gives you each in turn and holds you to it
  until it's met, then gives you the next, with no person in between. Use it to work through a
  list yourself, item by item.
- **Plans**: a plan kept in kiln, for the person to read, change, share and run again. Running
  one gives it to an agent: as one message, a step a turn, each step a goal, or each step to an
  agent of its own.

The tools are `goals_get`, `goals_add`, `goals_done`, `goals_split`, `goals_plan`,
`goals_update`, `goals_set`, `goals_control`; `plans_create`, `plans_list`, `plans_get`,
`plans_update`, `plans_ready`, `plans_run`, `plans_runs_list`, `plans_runs_get`,
`plans_runs_stop`, `plans_runs_retry`; and `research_start`, `research_add`, `research_get`,
`research_list`, `conversations_search` (in code mode `kiln.goals.*`, `kiln.plans.*`,
`kiln.research.*`).

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
criterion 1 of 3: p99 latency is at most 200 ms"), and once they're all met so is the step, with
no check of its own. Said met, a goal takes its parts still to go with it.

### Turning a goal into a plan: `goals_plan`

A goal that takes several steps of work, or needs thinking through before it's done, becomes a
plan kiln keeps before you start on it: `goals_plan` (the goal under way, or `id`) makes a plan in
its planning phase, with research of its own, and puts one part before the goal: planning it (a
person can ask for the same with the goal's **Make a plan**). Then, in this order:

1. **Research it** (below): inside and outside, recording what you find as you go.
2. **Write its steps** with `plans_update` (`items`): each with what to do, its acceptance
   criteria and tests, the steps it waits for (`needs`) and the findings it rests on (`basis`:
   `["f1", "f3"]`).
3. **Say it's ready**: `plans_ready` `{id, then}`. It refuses with no steps, no sources inside or
   outside (`skipped: {external: "why"}` when one truly can't apply), or a `basis` naming a finding
   that isn't there. It answers with the research (`research`, its markdown) and warnings (steps
   resting on nothing, or on findings graded D or F): **show the person the research in your
   reply, as it is** (every claim with its grade, score and sources), and the warnings. `then`:
   `goals` (the default: its steps become this goal's parts, here), `review` (the same, the stack
   paused for the person to look first), `agents` (it runs with an agent per step; the goal waits
   for the run).

Once every step is met, so is the goal: you don't say it's met yourself.

## Research: sources and grades

Research is what you found out, kept in kiln with where you found it. `research_start`
`{question}` begins it (a plan made with `goals_plan` has one already: its `research`), and
`research_add` records sources and findings as you go:

```js
// research_add: each finding a claim, each citation the source, the exact words, where they are
{ "id": "<research>", "findings": [
  { "claim": "kiln's conversation store already searches every agent's events",
    "cites": [{ "source": { "type": "code", "title": "store.rs", "locator": "crates/kiln/src/convo/store.rs:396" },
                "quote": "pub fn search(&self, q: Query)" }] },
  { "claim": "SQLite's trigram tokenizer finds substrings",
    "cites": [{ "source": { "type": "docs", "title": "FTS5", "locator": "https://sqlite.org/fts5.html#the_trigram_tokenizer" },
                "quote": "The experimental trigram tokenizer extends FTS5 to support substring matching" }],
    "against": [{ "claim": "trigram indexes grow three to four times larger", "cites": [{ "source": "s3" }] }] }
] }
```

- **Look inside and outside.** Inside: the files on your machine and others (code, docs, data,
  pictures; frames of a video with ffmpeg), commands you run (a `measurement`: the command as its
  locator, what it printed as the quote), and kiln's own records: `conversations_search` for what
  agents tried, decided or found before (cite a hit as `record`, locator `agent:<id>`), plans, CI
  runs, deployments. Outside: the web, standards and official docs, papers (arXiv, Semantic
  Scholar, Crossref, PubMed), books, news, blogs, forums.
- **Quote exactly**, and say where: kiln looks for each quote where you cite it (a file or page
  read from your machine, a measurement in your own tool output) and a quote it can't find there
  counts for little. Say what kind each source is (`type`): code, docs, a paper, a blog and a forum
  are taken at their word to different degrees.
- **Record counterclaims** you find, with their sources: they weigh against the claim. Mark a
  claim you infer (`inferred: true`) rather than read.
- **Never grade it yourself.** kiln grades each finding A to F (out of 100) the same way every
  time, from its sources' kinds, how far its citations hold up, how many independent sources agree
  and what's against it.
- **Present it with its sources and grades, always.** When you tell a person what you found, in a
  plan or in conversation, give every claim with its grade, score and sources, as
  `research_get`'s markdown has them: never a claim without where it comes from and how far it's
  to be trusted.

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

Give steps `needs` when some don't depend on others: a step starts once each step it needs is
met, and steps that don't wait for each other run side by side (with an agent per step, or on
subagents of the agent it runs on). Leave `needs` out of every step for steps run one after
another. A step can't wait for itself or for a step that waits for it.

`plans_run` runs a kept plan (`id`) in one of four `mode`s:

| mode | what happens |
|---|---|
| `message` | the whole plan in one message (with `needs`: the order its steps go in, those side by side on subagents of the agent's own) |
| `steps` | each step its own message, one a turn (with `needs`: the steps side by side in one message, a subagent each) |
| `goals` | each step a goal in the agent's goal stack, with its acceptance criteria and tests in the goal (`split`: each step a goal stack of its own, its criteria and tests a goal each, before it); with `needs`, the steps side by side go to subagents kiln starts for the agent |
| `agents` | each step an agent of its own, held to the step as a goal; the next starts once the one before is met |

`only` (step numbers, from 1) runs just some of a plan's steps, each keeping its number: the steps
they wait for outside them are taken as done (the run says which).

**On you**: leave out `session` and `new` (modes `message`, `steps`, `goals`). As with your own
goals, end your turn after `plans_run` in `goals` mode: the first step comes when you're free.
Each step's goal says the step it was made from: `goals_split` turns the one under way into its
acceptance criteria and tests, a goal each, before the step itself is checked.

**Steps side by side on your stack** (a plan whose steps say what they wait for, run as goals or
planned from your goal): where more than one step can be under way at once, kiln starts a subagent
of yours for each ready step (named "Step 3: …", held to the step on its own stack; in your chat,
on your machine, in your folder, with your harness and settings), as many at once as your
Subagents setting lets, and gives you the steps there's no room for. Don't do
a step a subagent has (`goals_get` shows its `subagent`): kiln marks it met when its subagent meets
it, or gives it back to you if it doesn't. When you're told to run steps side by side yourself (one
message, or a step a turn), give each a subagent of your own (your Agent or Task tool; without one,
`agents_start`), at most the number it says at once, and check what each did before going on.

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
