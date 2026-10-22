# Goal stacks

A goal stack is an agent's goals, one after another: each goes to the agent once the one before
it is met, as if you gave it `/goal` again and again yourself. Any harness kiln runs can have
one; how each goal goes depends on what the harness has:

| Harness | A goal goes as | It's met when |
|---|---|---|
| Claude Code | `/goal <condition>` (its own: a Stop hook checks the condition each time it would stop, and it keeps working until it's met) | Claude Code's verdict says met (read from its transcript) |
| Codex | its thread goal (`thread/goal/set`, as Codex's own `/goal`) | Codex says the goal is complete (`thread/goal/updated`) |
| the rest (Gemini CLI, opencode, Goose, Amp …) | a message: "Your goal: …; work on it until it's met. When it is, end your reply with this line: GOAL MET" | the agent's reply ends with `GOAL MET` |

A goal is one line: a condition to meet ("the parser tests pass") or an objective. Whatever the
harness, the agent can also say a goal is met itself (`goals_done`, with what shows it: "Agents
work their own stack", below).

## In the console

An agent's **Plan** tab has its **Goal stack** (`/goals` opens it), in a panel of its own below its plans: what's met, under way and next,
each with how it came out; a field to add goals (a line each); and:

Each goal is a link of a chain, drawn as the agent's conversation draws its work: under way while
the agent is at work, hyperlight's mark with its light turning and the goal's words in the
holographic shimmer; met, the loop closed; not met, broken; skipped, dashed; to come (or under way
while the agent is idle, or paused), only sketched, still. A goal's parts are smaller links before
it, and a step's own parts (its acceptance criteria and tests) smaller still; a goal turned into a
plan links to it, and a part waiting for a plan's run to the run.

- **Start**: the first goal goes to the agent at once, whatever it's doing (behind a turn under
  way, its harness queues it: "One message at a time", below); each next one once it's free (idle,
  nothing waiting on you, nothing queued).
- **Pause**: the agent isn't held to the goal under way any more: its harness's own goal is
  cleared there (so Claude Code's or Codex's goal stops driving it), nothing is sent again, and no
  next goal starts. The goal keeps its place, shown as paused.
- **Resume**: the goal under way goes to the agent again at once, agents it started at work or
  not, and the stack carries on.
- **Skip this one**: leave the goal under way (its harness's own goal cleared) and go on: the next
  goes at once.
- **Edit**: the goals not yet met, one text box, a goal a line, the goal under way first. Put
  another before it and it goes back among them (cleared in the agent) while that one goes next;
  give it other words and it goes to the agent again as they say; take its line out and it's
  skipped. Reordering, rewording, removing and adding the goals to come is the same box.
- **Try it again** or **Go on to the next**, after a goal that ended without being met, with none
  under way (stacks from before goals were kept to the end); with a goal under way, **Resume**.
- **Clear**: empty the stack; a goal still set in the harness is cleared there too. The goals are
  kept: **Bring back** puts them back as they were (after any goals done since, before any to come)
  and runs the stack, the goal that was under way going to the agent again at once. Only the last
  clear's goals are kept.

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

How a goal came out is taken as soon as the agent's harness says so (or, sent as a message, its
reply says GOAL MET), whatever else the agent is doing, so the stack never shows under way a goal
the agent was told is met or cleared. Reminders, and the next goal, wait while agents it started
are at work (it ended its turn to let them, and is told when each settles: docs/agents.md, "Told,
not made to wait"); a goal you start, resume or skip to doesn't. They wait 30 seconds after the
agent stops, then twice as long each time one brings no real work (up to 15 minutes); once the
agent has worked on the goal for two minutes, they start over.
The goal under way shows how many times it was sent again, and when. While the agent's machine is
away, the stack waits for it and goes on when it's back. Only you (or the agent itself, below)
stop it: **Pause**, **Skip this one** or **Clear**; the stack pauses by itself only when the
agent is gone (closed).

### One message at a time

What the stack sends an agent behind a turn under way (a goal, the goal again, a clear of its
harness's own goal) waits in the agent's queue, and the stack keeps one message there at a time.
Sent while its last one still waits, the next takes that one's place: kiln edits it to say the new
word (a clear followed by the next goal is that goal), and doesn't send a word the waiting one says
already (the stack resumed again and again). A goal that ends or goes back among the others before
the agent took it is taken out of the queue (what's next goes in a message of its own, or, when
its harness's own goal is cleared first, as an edit of the clear). So an agent that spends hours in
one turn, its stack changing meanwhile, finds the stack's latest word when the turn ends, once,
rather than every goal and clear sent since, in the order they went. kiln sees the queue as the
agent's machine last reported it: an edit that reaches the machine just after the turn ended, the
message gone as it was, does nothing there, so what the edit said goes again, in a message of its
own (and again at the next look, should it not reach the agent either). The stack's messages say
so in their ids (`goal-…`): a server that started since (an upgrade) takes the one waiting for the
stack's last word.

A goal set in the agent itself (`/goal …` in its composer) joins its stack as the goal under way,
so it's kept the same way, across restarts too; a goal the stack had under way goes back to the
front of what's next. Clearing it in the agent (`/goal clear`) ends it as skipped.

A plan runs as goals too: **Run it** (or, in the agent's Plan tab, **Run here**) **as Goals**, each
step one, after the agent's others (a new agent is started for them when you pick one). A step
with parts goes as one line: its title, what to do, examples, "Done when:" its acceptance
criteria (each a sentence: "p99.9 latency is at most 200 ms") and "Tests:" its tests. The stack
shows its title, the rest under it. Each such goal says the step it was made from (`step`), so
the agent can split it into the step's acceptance criteria and tests (below); a plan run with an
agent per step does the same for each step's agent, or puts a step on the main agent's own stack
(docs/plans.md, "One agent per step").

## Agents work their own stack

An agent keeps its own stack (and those of the agents it started) through kiln's MCP tools, and
doesn't need a person for any of it:

- **Saying a goal is met: `goals_done`**, with `evidence`, what shows it ("212 tests pass on
  Linux and macOS"). The goal under way (or the one `id` names) ends as met, with those words, its
  harness's own goal is cleared, and the next goes to the agent at once. It's how any agent says
  so, whatever its harness, alongside its harness's own verdict (Claude Code's `/goal` check,
  Codex's goal, `GOAL MET`); a goal met along the way, or one skipped earlier that turned out
  met, is said met by its `id`. Skipping (`goals_control` `skip`) is for leaving a goal unmet, and
  says so. The stack shows who said a goal met: "met · the agent said so: 212 tests pass".
- **Splitting a goal: `goals_split`.** Its parts (`goals`, a goal each) go right before it and are
  pursued first, each in turn; then the goal itself comes back for its own check. The goal under
  way split waits after its parts, and the first part goes now. A goal made from a plan's step
  splits, with no parts given, into the step's acceptance criteria and tests, a goal each
  ("\"Split the scanner\" (step 2 of the plan \"Lexer\"), acceptance criterion 1 of 2:
  scan_number exists"). A goal said met takes its parts still to go with it, and theirs. Parts
  the agent gives bring the goal back for its own check after them (its breakdown may miss
  something); parts kiln makes (a step's acceptance criteria and tests, a plan's steps) don't:
  once they're all met, so is the goal, at once ("Met: each of its acceptance criteria and tests is
  met.").
- **Turning a goal into a plan: `goals_plan`.** The goal under way (or the one `id` names) becomes
  a plan in its planning phase, with research of its own (docs/plans.md, "Planning a goal"): its
  first part is planning it (research inside and outside, its steps written, said ready with
  `plans_ready`). Ready, the plan's steps become its parts (or it runs with an agent per step, a
  part waiting for the run), and once every one is met, so is the goal: "Met with its plan: its 4
  steps are met."
- **Changing one goal: `goals_update`** by its `id`, without giving the whole list: other words
  (`text`: the goal under way goes again as they say), moved (`next`, right after the goal under
  way; `before` or `after` another goal still to go: put before the goal under way, it goes next
  and that one waits), or taken out (`remove`: the goal under way ends as skipped, saying who took
  it out; one to come goes, with its parts). `goals_add` takes `next`, `before` and `after` too.
- **Pausing for a while: `goals_control`** `{action: "pause", why, resume_in_s}` (or `resume_at`,
  unix ms or a time like `2026-10-21T15:00:00Z`; at most 30 days). Paused, the agent isn't held
  to its goal and nothing is sent; when the time is up the stack resumes by itself and the goal
  under way goes to the agent at once, as when you resume it. The stack says who paused it and
  why ("Paused by the agent: waiting for the deploy") and when it resumes. Without a time it waits
  for someone to resume it (`resume`, or `start`). Kiln doesn't keep an agent going (below) while
  its stack is paused for a while: it waits on purpose.

The server keeps the stacks and drives them, so they go on with no console open and across
restarts. A stack holds at most 500 goals, those met or under way included, so a program of a
hundred goals and more fits, its met goals kept for the record.

## Held or paused for usage

An agent of a program (its coordinator, or a track's worker, as assigned:
[programs.md](programs.md)) is held to the program's usage rule by the driver. Above 80% of a
weekly window of the program's accounts no new goal starts; the goal under way goes on. Above
95% its stack pauses ("Paused for usage: …") as a pause of yours does: its goal in the agent
cleared, what waits in its queue for its goals withdrawn. It goes on again by itself once the window
is at 80% or under, as when you resume it, its goal under way sent again (the clear the pause sent
doesn't end it). A stack you paused stays paused. kiln doesn't prod an agent paused for usage, at a
limit or at its threshold: it isn't kept going, and it hears of its children once it goes on (what
others send it still reaches it).

## A goal under way on another agent

A program's coordinator can hand a goal of its stack to a track's worker
([programs.md](programs.md), "Workers"). The stack shows it under way on that agent, and the driver
doesn't start it here; while it's out, the stack isn't done. It's met, or comes back to the stack,
when the coordinator says so.

## Keep going

An agent with no goal stack under way is still kept at its work: when a turn stops part-way,
kiln tells it to go on, whatever its harness (`crates/kiln/src/goals/keep.rs`). A turn stopped
part-way is one that:

- its harness cut short: Claude Code's stop sequence (its "No response requested.") or output
  limit, an error part-way; a Codex turn that failed; an ACP agent out of tokens;
- ended on a failed tool, with nothing said after it;
- said nothing about the work;
- ended saying it's going on ("Next, I'll wire the UI.", "Let me check the logs:") and didn't:
  last words that wait on you ("I'll wait for your go-ahead", "Let me know…") or ask something
  don't count;
- left to-dos open, its last words asking you nothing;
- its harness started again under (a crash, a resume): kiln saw it at work, and it's back idle
  with that turn never having ended. Kiln's clock decides this, so a machine whose clock is off
  can't make a turn that ended look cut short; and only for an agent whose runner reports how its
  turns end (one from before keep-going would look started again after every turn).

A turn kiln itself cut short is taken up by the runner, not by keep-going: its runner upgrading in
place, the system stopping the runner, or the runner crashing. Each session's record on its
machine says whether a turn of it is open (set as one starts, cleared as one ends; its harness
ending doesn't clear it, a force stop does). Started again from that record, a session whose turn
was open gets one message first in its queue, shown as kiln's: "kiln restarted on this machine
part-way through your last turn, which stopped it and anything it was running. Pick up where you
left off." Agents that were between turns hear nothing, and it isn't said twice if the runner
restarts again before it goes out. What the agent started that still runs (a dev server, a test
run, a script it left going) is named in that message, so it uses it rather than starting it
again; see "What an agent left running" in [agents.md](agents.md). The runner does this whether or not keep-going is on for the
agent, since the turn was kiln's to have stopped.

Every harness kiln runs as an agent reports how its turns end: Claude Code (its result's stop
reason), Codex (`turn/completed`: completed, interrupted, failed), the ACP agents, Gemini CLI,
opencode, Goose and Amp (the prompt's `stopReason`: end_turn, max_tokens, max_turn_requests,
refusal, cancelled), and the Antigravity CLI (its `result`: SUCCESS, or ERROR with what went wrong;
"interrupted" is a stop). Their to-dos are Claude Code's TodoWrite, Codex's plan and ACP's plan. A
turn of the Antigravity CLI that ended on something it wasn't allowed to do (it can't ask, so it
was denied) ends as "blocked" and is left alone: going on would only be denied again.

Kiln waits four seconds (a message of yours goes first), then sends one line, shown as kiln's in
the timeline: "Your last turn stopped part-way (stop_sequence). Pick up where you left off and
carry on with the work. If it's all done, say so; if you need something from me, ask."

It doesn't when you stopped the turn, the agent waits on you (an approval, a question, last words
that ask something), a message is queued, a background task of its runs or an agent it started is
at work (it's told when that agent settles: docs/agents.md, "Told, not made to wait"), a usage
limit paused it, a goal stack drives it, or its goal stack is paused for a while. It keeps an agent going at most five turns in a row since a person last
said something, and stops after two of its own turns call no tool.

Each turn's end is in the agent's state as `last_turn` (`{at, how: "done" | "cut" | "stopped" |
"limit" | "refused", why?, tools, after_error?, asks?, kept?}`), and `kept` counts kiln's turns in
a row. Switch it off for an agent in its ⋯ menu (**Keep it going when it stops part-way**), or
with the `keep_going` control (`{"type": "control", "id": "keep_going", "value": false}`); it
holds across restarts.

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/sessions/:id/goals` | the stack: `{agent, state: "running" \| "paused" \| "done", reason?, items: [{id, text, status: "pending" \| "active" \| "met" \| "unmet" \| "skipped", reason?, how?, started_at?, ended_at?, reminders?, reminded_at?, part_of?, part_kind?, step?: {plan, version, n, run?}, planned_as?, waits_on?}], updated_at, cleared?, resume_at?}` (`part_kind`: who made it a part: `own` (the agent), or kiln: `checks`, `plan`, `planning`, `run`; `planned_as`: the plan the goal became; `waits_on`: the plan run a part waits for; `reminders`: times the goal under way was sent again in a row without real work; `reminded_at`: when it was last sent, unix ms, none since it was resumed; `part_of`: the goal it's a part of; `step`: the plan step it was made from; `cleared`: how many goals the last clear kept, to bring back; `resume_at`: when a stack paused for a while resumes, unix ms) |
| POST | `/api/harness/sessions/:id/goals` | `{goals: [text], start?, next?, before?, after?}`: goals after the others, or right after the goal under way (`next`), or before or after a goal still to go (by id) (a stack that was done, given more, is paused until started) |
| POST | `/api/harness/sessions/:id/goals/done` | `{id?, evidence?}`: the goal under way (or `id`) is met |
| POST | `/api/harness/sessions/:id/goals/split` | `{id?, goals?}`: the goal under way (or `id`) split into parts pursued first (none given: a plan step's acceptance criteria and tests) |
| POST | `/api/harness/sessions/:id/goals/plan` | `{id?, title?}`: the goal under way (or `id`) turned into a plan in its planning phase: `{plan, research, goal, part, next}` |
| PATCH | `/api/harness/sessions/:id/goals/:goal` | `{text?, next?, before?, after?, remove?}`: one goal still to go changed, moved or taken out |
| PUT | `/api/harness/sessions/:id/goals` | `{goals: [{id?, text}]}`: the goals still to go, as they should be (reordered, edited, removed, added; one without `id` is the goal still to go with its words, if one wasn't named, else a new one). The goal under way may be among them: first, it stays (with other words, it's sent again as they say); after others, it goes back among them and the first goes next; left out, it stays under way. Goals done, given by id, are left as they are, not added again |
| POST | `/api/harness/sessions/:id/goals/{start,resume,pause,skip,done,clear,restore}` | as above (`resume`: `start`; `restore`: **Bring back**); `pause` takes `{why?, resume_in_s?, resume_at?}` |

Agents use the same through kiln's MCP tools (`goals_get`, `goals_add`, `goals_done`,
`goals_split`, `goals_plan`, `goals_update`, `goals_set`, `goals_control`) or `kiln.goals.*` in code mode: an agent can line goals up for itself and for
the agents it started (and theirs), and change or clear only theirs (docs/agents.md, "No more than
its parent"). Without `agent`, they're about the agent calling: its own stack. `plans_run` takes
`mode: "goals"` (on the agent calling, when it gives neither `session` nor `new`); on a new agent,
that agent is the caller's child. Every agent kiln runs is told this (and its id) by kiln's MCP
server, and Claude Code gets the kiln-plans skill, which walks through turning a document or a list
into a goal stack it pursues: a goal per item, then ending its turn, since the first goal comes
once it's free.

How the harness's own goal stands is in the agent's state as `objective` (`{text, status:
"active" | "met" | "unmet" | "paused" | "cleared", reason?, at}`), for Claude Code and Codex.
