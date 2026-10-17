# Goal stacks

A goal stack is an agent's goals, one after another: each goes to the agent once the one before
it is met, as if you gave it `/goal` again and again yourself. Any harness kiln runs can have
one; how each goal goes depends on what the harness has:

| Harness | A goal goes as | It's met when |
|---|---|---|
| Claude Code | `/goal <condition>` (its own: a Stop hook checks the condition each time it would stop, and it keeps working until it's met) | Claude Code's verdict says so (read from its transcript: met, can't be met, or it stopped after its checks kept finding it unmet) |
| Codex | its thread goal (`thread/goal/set`, as Codex's own `/goal`) | Codex says the goal is complete (`thread/goal/updated`); blocked, usage-limited or out of its token budget is not met |
| the rest (Gemini CLI, opencode, Goose, Amp …) | a message: "Your goal: …; work on it until it's done" | the turn it started ends |

A goal is one line: a condition to meet ("the parser tests pass") or an objective.

## In the console

An agent's **Plan** tab has its **Goal stack** (`/goals` opens it), in a panel of its own below its plans: what's met, under way and next,
each with how it came out; a field to add goals (a line each); and:

- **Start** (or **Resume**): goals go to the agent, the next once it's free (idle, nothing waiting
  on you, nothing queued).
- **Pause**: no more goals go; the one under way carries on in the agent.
- **Skip this one**: leave the goal under way (its harness's own goal cleared) and go on.
- **Edit**: the goals not yet started, one text box, a goal a line.
- **Try it again** (after one that wasn't met) or **Go on to the next**.
- **Clear**: empty the stack; a goal still set in the harness is cleared there too.

A goal that isn't met pauses the stack, and says why (Claude Code's or Codex's own words). The
stack also pauses when the agent stops, goes away, or doesn't take a goal up.

A plan runs as goals too: **Run it** (or, in the agent's Plan tab, **Run here**) **as Goals**, each
step one, after the agent's others (a new agent is started for them when you pick one). A step
with parts goes as one line: its title, what to do, examples, "Done when:" its acceptance
criteria (each a sentence: "p99.9 latency is at most 200 ms") and "Tests:" its tests. The stack
shows its title, the rest under it.

The server keeps the stacks and drives them, so they go on with no console open and across
restarts.

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/sessions/:id/goals` | the stack: `{agent, state: "running" \| "paused" \| "done", reason?, items: [{id, text, status: "pending" \| "active" \| "met" \| "unmet" \| "skipped", reason?, how?, started_at?, ended_at?}], updated_at}` |
| POST | `/api/harness/sessions/:id/goals` | `{goals: [text], start?}`: goals after the others |
| PUT | `/api/harness/sessions/:id/goals` | `{goals: [{id?, text}]}`: the goals not yet started, as they should be (reordered, edited, removed, added) |
| POST | `/api/harness/sessions/:id/goals/{start,pause,skip,clear}` | as above |

Agents use the same through kiln's MCP tools (`goals_get`, `goals_add`, `goals_set`,
`goals_control`) or `kiln.goals.*` in code mode: an agent can line goals up for itself or for
another agent. `plans_run` takes `mode: "goals"`.

How the harness's own goal stands is in the agent's state as `objective` (`{text, status:
"active" | "met" | "unmet" | "paused" | "cleared", reason?, at}`), for Claude Code and Codex.
