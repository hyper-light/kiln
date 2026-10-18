# Goal stacks

A goal stack is an agent's goals, one after another: each goes to the agent once the one before
it is met, as if you gave it `/goal` again and again yourself. Any harness kiln runs can have
one; how each goal goes depends on what the harness has:

| Harness | A goal goes as | It's met when |
|---|---|---|
| Claude Code | `/goal <condition>` (its own: a Stop hook checks the condition each time it would stop, and it keeps working until it's met) | Claude Code's verdict says met (read from its transcript) |
| Codex | its thread goal (`thread/goal/set`, as Codex's own `/goal`) | Codex says the goal is complete (`thread/goal/updated`) |
| the rest (Gemini CLI, opencode, Goose, Amp …) | a message: "Your goal: …; work on it until it's met. When it is, end your reply with this line: GOAL MET" | the agent's reply ends with `GOAL MET` |

A goal is one line: a condition to meet ("the parser tests pass") or an objective.

## In the console

An agent's **Plan** tab has its **Goal stack** (`/goals` opens it), in a panel of its own below its plans: what's met, under way and next,
each with how it came out; a field to add goals (a line each); and:

- **Start**: goals go to the agent, the next once it's free (idle, nothing waiting on you, nothing
  queued).
- **Pause**: the agent isn't held to the goal under way any more: its harness's own goal is
  cleared there (so Claude Code's or Codex's goal stops driving it), nothing is sent again, and no
  next goal starts. The goal keeps its place, shown as paused.
- **Resume**: the goal under way goes to the agent again at once, and the stack carries on.
- **Skip this one**: leave the goal under way (its harness's own goal cleared) and go on.
- **Edit**: the goals not yet started, one text box, a goal a line.
- **Try it again** or **Go on to the next**, after a goal that ended without being met (stacks from
  before goals were kept to the end).
- **Clear**: empty the stack; a goal still set in the harness is cleared there too.

### Kept on the goal until it's met

The next goal starts only when the agent says the one under way is met. Until then kiln keeps the
agent on it: whenever the agent is free (idle or stopped, nothing waiting on you, nothing queued)
and hasn't said it's met, kiln sends the goal again: its harness's own `/goal` again (re-armed), or
for the rest the message "Your goal isn't met yet: …". That covers an agent that:

- stopped short: Claude Code's checks kept finding the goal unmet and it gave up for now, or
  Codex's goal was blocked, paused or out of budget;
- stopped with the goal still set, or never took it up;
- lost it: the agent started again (a runner update, a restart, a crash) into a session that no
  longer has it. The stack is kept by the server, so it knows the goal under way and sends it to
  the new session;
- stopped altogether: a message starts it again.

Reminders wait 30 seconds after the agent stops, then twice as long each time one brings no real
work (up to 15 minutes); once the agent has worked on the goal for two minutes, they start over.
The goal under way shows how many times it was sent again, and when. While the agent's machine is
away, the stack waits for it and goes on when it's back. Only you stop it: **Pause**, **Skip
this one** or **Clear**; the stack pauses by itself only when the agent is gone (closed).

A goal set in the agent itself (`/goal …` in its composer) joins its stack as the goal under way,
so it's kept the same way, across restarts too; a goal the stack had under way goes back to the
front of what's next. Clearing it in the agent (`/goal clear`) ends it as skipped.

A plan runs as goals too: **Run it** (or, in the agent's Plan tab, **Run here**) **as Goals**, each
step one, after the agent's others (a new agent is started for them when you pick one). A step
with parts goes as one line: its title, what to do, examples, "Done when:" its acceptance
criteria (each a sentence: "p99.9 latency is at most 200 ms") and "Tests:" its tests. The stack
shows its title, the rest under it.

The server keeps the stacks and drives them, so they go on with no console open and across
restarts.

## Keep going

An agent with no goal stack under way is still kept at its work: when a turn stops part-way,
kiln tells it to go on, whatever its harness (`crates/kiln/src/goals/keep.rs`). A turn stopped
part-way is one that:

- its harness cut short: Claude Code's stop sequence (its "No response requested.") or output
  limit, an error part-way; a Codex turn that failed; an ACP agent out of tokens;
- ended on a failed tool, with nothing said after it;
- said nothing about the work;
- left to-dos open, its last words asking you nothing;
- its harness started again under (a crash, a resume): kiln saw it at work, and it's back idle
  with that turn never having ended. Kiln's clock decides this, so a machine whose clock is off
  can't make a turn that ended look cut short.

Kiln waits four seconds (a message of yours goes first), then sends one line, shown as kiln's in
the timeline: "Your last turn stopped part-way (stop_sequence). Pick up where you left off and
carry on with the work. If it's all done, say so; if you need something from me, ask."

It doesn't when you stopped the turn, the agent waits on you (an approval, a question, last words
that ask something), a message is queued, a background task of its runs, a usage limit paused it,
or a goal stack drives it. It keeps an agent going at most five turns in a row since a person last
said something, and stops after two of its own turns call no tool.

Each turn's end is in the agent's state as `last_turn` (`{at, how: "done" | "cut" | "stopped" |
"limit" | "refused", why?, tools, after_error?, asks?, kept?}`), and `kept` counts kiln's turns in
a row. Switch it off for an agent in its ⋯ menu (**Keep it going when it stops part-way**), or
with the `keep_going` control (`{"type": "control", "id": "keep_going", "value": false}`); it
holds across restarts.

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/sessions/:id/goals` | the stack: `{agent, state: "running" \| "paused" \| "done", reason?, items: [{id, text, status: "pending" \| "active" \| "met" \| "unmet" \| "skipped", reason?, how?, started_at?, ended_at?, reminders?, reminded_at?}], updated_at}` (`reminders`: times the goal under way was sent again in a row without real work; `reminded_at`: when it was last sent, unix ms) |
| POST | `/api/harness/sessions/:id/goals` | `{goals: [text], start?}`: goals after the others |
| PUT | `/api/harness/sessions/:id/goals` | `{goals: [{id?, text}]}`: the goals not yet started, as they should be (reordered, edited, removed, added) |
| POST | `/api/harness/sessions/:id/goals/{start,pause,skip,clear}` | as above |

Agents use the same through kiln's MCP tools (`goals_get`, `goals_add`, `goals_set`,
`goals_control`) or `kiln.goals.*` in code mode: an agent can line goals up for itself or for
another agent. `plans_run` takes `mode: "goals"`.

How the harness's own goal stands is in the agent's state as `objective` (`{text, status:
"active" | "met" | "unmet" | "paused" | "cleared", reason?, at}`), for Claude Code and Codex.
