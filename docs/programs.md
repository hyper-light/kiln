# Programs

A program is work too big for one goal stack: a master plan, run as goals on a coordinating agent,
and tracks, each a plan of its own, each worked by an agent assigned to it. kiln's critical path
(master plan `01a157fddb8276a2996246ea931c4808`, ten tracks) is one. The Programs view follows them:
Mission control's **Programs** (`/agents/programs`), the critical path first.

## What a program is, to kiln

- **Its master plan** is tagged `master`, and with the program's own tag (`critical-path`).
- **Its tracks** are the plans that share the program's tag, each with a `t<n>` tag (`t7`): track
  `T7`, named from its plan's title (`Critical path T7: Hex, …` is `Hex`).
- **Its coordinator** is the agent the master plan's latest goals run went to (Plans: Run as
  goals). Its goal stack is the program's list of record: a goal is a track's by its id, the first
  word of its text (`CP-T7.1 Hex set up; …` is `T7`'s).
- **Its weights** are kept in kiln, with its slots (6 unless the person sets them). They're seeded
  once, the first time kiln reads the program, from the master plan's tracks table: the column
  headed `Weight`, rows by track (`| T7 Hex | … | 4 | … |`). From then on they're only the
  person's: an edit of the plan changes none. A track with no weight weighs 1.

## Workers

Each track's **worker** is assigned, and the assignment is kept in kiln. Any agent that works for
the same person as the coordinator can be one (with no coordinator yet, as the person assigning
it), whether the coordinator started it or not (T1's Amphora isn't its child). The coordinator
assigns workers, or the person does, in the view.

The **goal under way** on a track is one of:

- a goal of the coordinator's stack, handed to the worker (`programs.assign` with `goal`). The
  coordinator's stack shows it as under way on that agent, and the goal driver doesn't start it
  on the coordinator. While one is handed out, the stack isn't done. A track has one goal handed
  out at a time; handing it another brings the first back to the stack;
- else the worker's own goal of that track, when the worker has a stack;
- else the track's next goal in the coordinator's stack.

The coordinator marks a handed-out goal met (`met`), or brings it back (`goal: null`), when its
worker is done. Taking a worker off brings its goal back too.

A track **stands as its worker's session does**: at work, or paused. A pause is at a usage limit
(its window, and when that window starts over) or at the worker's threshold. A worker kiln no longer
knows (its session closed; a machine that's away still has its sessions listed) holds no slot: the
row says whom the track is assigned to, and its goal stays out until the track is assigned again.

## The view

Each program is a section of its own:

- **Its head:** its coordinator and how it stands (or its id, when kiln doesn't list its session
  any more), its tracks at work out of its slots, its goals met out of all, the coordinator's goal
  under way, and its usage with the rule it brings.
- **One row per track:**
  - its worker and how it stands: its status, and a pause with its window and when it starts over;
    its threshold (and kiln's rule for it, when a person set another); and a choice of worker;
  - the goal under way (and on which agent), or the next;
  - its goals met out of all;
  - its change requests: its worker's, and the coordinator's or another worker's on its
    `cp/t<n>-` branch (kiln keeps one per agent, so only the program's agents' are read). Each
    says how far it went, `built` (open for review), `merged` (landed) or `closed`, and how its
    checks went. kiln doesn't know when a change is live, so the view doesn't say it;
  - its pass, its weight, and its worker's usage of its weekly window;
  - its slot.

On a phone a row is the same parts one under another; nothing in the page is wider than the
screen.

**Workers, slots and weights** are changed in the view, with **Save settings**. They're kept in
kiln. When the slots or weights change, the coordinator is told what changed, in a message queued
for its session, and it follows them from its next goal boundary: `programs.get` gives each
track's slot by them. Under the settings the view says whether kiln queued the message for the
coordinator's session (the session, or its machine, took it), or why not yet. kiln tries again
every 15 s until it does. Slots and weights are only the
person's: an agent can read them and can't change them, whether it calls as its session or signed
in as an agent.

When a read of the programs fails, the view says why. A refresh that fails says so above what was
read before.

## Its turns: stride scheduling

Each goal a track meets adds 1/weight to its **pass**. A track at work keeps its slot, and so does
one whose worker paused mid-goal (none is stopped mid-goal). The free slots go to the tracks not
done, lowest pass first, then the heavier, then the lower number. So over many goals each track's
share follows its weight: weights 5, 3 and 2 give 30 goals as 15, 9 and 6.

A track's **slot** is one of:
- `at work`: its worker is on a goal of its;
- `next`: a free slot is its;
- `waiting`;
- `held for usage`: it would be next, but no new goal starts;
- `paused for usage`: its worker's session is paused, at a usage limit or its threshold, mid-goal;
- `done`: none of its goals is left to go (each met, or ended unmet or skipped).

## Usage

A program's usage is the fullest weekly window (any harness's `7d` limit) of its agents' accounts:
its coordinator's and its workers'. Its rules are the critical path's (its master plan, §5), held by
kiln's per-session thresholds (`pause_at`: a session pauses once a window is that full) and by the
goal driver:

- **Every worker pauses at 95%.** kiln sets each worker's threshold to 95.
- **The lowest-weight track pauses at 80%.** Its worker's threshold is 80: the lowest-weight track
  at work, or paused mid-goal (of tracks as light, the one furthest ahead).
- **Above 80% of the weekly window, no new goal starts.** The driver doesn't start the
  coordinator's next goal, and `programs.assign` refuses to hand a worker a new one (409). The goals
  under way go on.
- **Above 95%, the program's goal stacks pause** (the coordinator's: workers have none). As a
  pause of yours does, it clears the agent's own goal (Claude Code's `/goal`), so the agent doesn't
  keep pursuing it, and withdraws what still waits in its queue for its goals. The stack says
  "Paused for usage: …". It goes on again by itself once the window is at 80% or under, as when you
  resume it: the goal under way goes to the agent again at once, and the clear the pause sent
  doesn't end it. Between 80% and 95% it stays paused, so a window near 95% doesn't pause and
  resume it by turns.
- **A stack you paused stays paused**, whatever the usage.
- **A threshold a person set on a worker stays theirs.** kiln only sets one when the worker has
  none, or still has the one kiln set last. It takes its own back from an agent that's no longer a
  worker. The thresholds are held every 15 s.
- **kiln doesn't prod a paused agent.** An agent paused at a limit or a threshold, or whose stack
  is paused for usage, isn't kept going when it stops part-way, and isn't told of its children
  while it's paused: it hears what they did once it goes on. What others send it still reaches it
  (a person's message, another agent's, news of its change requests and CI).

The view says so: its usage line, each worker's threshold and pause, and each track held or paused.

## API

| Method | Path | |
| --- | --- | --- |
| GET | `/api/programs` | `{programs}`: every program, the critical path first |
| GET | `/api/programs/:id` | one, by its master plan's id |
| PUT | `/api/programs/:id/settings` | `{slots?, weights?: {"T7": 4}}`: slots 1 to 32, weights 1 to 100. A person's only (an agent: 403); the coordinator is told |
| PUT | `/api/programs/:id/tracks/:track` | `{agent?, goal?, met?}`: a track's worker (an agent's id; `null` to take it off), the coordinator's goal handed to it (`null`: none), or that goal met. The coordinator's or the person's (another agent: 403); a worker of another person's: 403; a new goal above 80%: 409 |

The operations are `programs.list`, `programs.get`, `programs.set` and `programs.assign`, in code
mode too (`kiln.programs.get({program})`). They need `agents.run`.

## Tests

- **Rust:**
  - `programs::tests` checks how a program is found, matched and shown, on a scratch hub: workers
    by assignment (the coordinator's child or not), a goal handed to a worker and shown so in the
    coordinator's stack, a worker's own goal, a pause at a session limit, the thresholds (kiln's
    set, a person's left), the person's settings with the coordinator told (and told again when it
    didn't get there), the weights seeded once, the hold above 80%, a track whose goals were skipped
    done, a worker of another person's refused with no coordinator too, and an agent refused. It also checks the rotation and the lab:
    tracks weighted 5, 3 and 2, over 30 goals, each within one goal of its share, with one slot and
    with two.
  - `goals::stack_tests::a_program_s_usage_holds_new_goals_above_80_and_pauses_its_stacks_above_95`
    checks the driver's usage rules, and that a goal under way goes on when a pause lifts.
    `goals::tests::keep_going_leaves_an_agent_paused_for_usage_alone` checks keep-going leaves
    one alone.
  - `goals::stack_tests::a_goal_is_handed_to_a_track_s_worker_one_a_track_and_comes_back` and
    `goals::tests::a_goal_under_way_on_another_agent_isn_t_started_and_keeps_the_stack_open` check
    handed-out goals.
  - `harness::told::tests` checks that a parent paused for usage is told once it goes on.
- **The UI:** `ui/tests/programs.test.ts` checks the view's words (pauses, thresholds, the
  notice), and the form as edited.
- **The browser:** `ui/e2e/programs.spec.ts` runs on the production build, served by the mock
  (`npm run build && npm run e2e`):
  - the first load of 10 tracks and 120 goals takes at most 300 ms at the p95 of 20 loads. The same
    test runs against a real server: `scripts/programs_load.sh <folder>` (after the UI's production
    build) makes a release build serving it, starts it as a scratch server of its own, seeds it
    through its API with a program of 10 tracks and 120 goals (`scripts/programs_seed.py`), and
    takes the p95 of 20 cold loads, with the commit and the machine's load in `<folder>/summary.txt`;
  - on a phone and a desktop: one row per track, nothing scrolling sideways, and screenshots;
  - a track whose worker is paused, the usage rule, a held track and the thresholds say so;
  - a worker taken off in the view, and the person's settings, are kept, and the coordinator told.
