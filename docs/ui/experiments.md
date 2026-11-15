# Experiments: a live split of an agent's sessions between deployments, and Replay (design)

Serves kiln plan 01a1cd8e862e7169a074db3d5fa33f3d's build plan P7.1 and P7.2, from the kiln × Hex × Shards design,
section G, as the person defined it (2026-11-14):

> "you deploy an A|B experiment with the given A|B|etc. groups at the specified proportion/percentage. Each time
> a session starts, that session will attach to/spawn one of the variants/groups with respect to its percentage."

Replay is an addition. Decision 4 holds: variants are deployments, measured on their own measures, with no model
tokens. Decision 8 holds: footprint is peak and steady, and a host variant is allowed.

Status: design for the person's approval. Nothing here is built. The mockup is
[mockups/experiments.html](mockups/experiments.html).
- It opens on a live split of 96 sessions, and a new one arrives every couple of seconds.
- Its intervals are computed from the sessions it makes, so they narrow as you watch.
- Move a weight, pause, resume, promote, or open Replay.
- New experiment starts a draft.

## Where it lives

- **Experiments**, `/agents/experiments`, under Agents beside Definitions and Agentfiles (crumbs `05 / Agents /
  Experiments`).
- Today's code experiments (W20-R11, an agent's question's options built in worktrees) stay with their question.
- One surface: the list of yours, then the open experiment, with two tabs in the agent page's rail style (`.pt`):
  **Live split** (its session count) and **Replay**.
- A session given a variant says so on its own page: "in experiment reviewer-size · variant B".

## An experiment

- **Target**, a `Segmented` control: **An agent** (one of your definitions), or **A harness in an Agentfile**.
  - "The sessions of it you start (Mission control, Run, a plan's step) are split. Others' aren't."
  - The target is fixed once it starts.
- **Variants**, two to four cards, keyed by letter on its colour (A, B, C, D). Each holds:
  - **Agentfile**;
  - **Engine**, `Segmented` `microvm | host`: a host variant is allowed, and runs on a runner whose engine is
    host;
  - **vCPUs** and **Memory**, for microvm;
  - **Weight**, a slider with its percent.
- The variant that's what the target runs on now is marked **current**. Add a variant copies the last, at 0%.
- **Weights always add up to 100**:
  - moving one moves the others in proportion;
  - the line under the cards reads "Weights A 50% · B 50%, 100% in all: moving one moves the others, in
    proportion, so they always add up to 100. A variant at 0% takes no new sessions.";
  - weights stay movable while it's live, and apply to sessions that start from then on.
- A variant's deployment is fixed once it starts: a different one is a new experiment, so its sessions are never
  mixed.

## States and controls

- **Draft**: everything editable. **Start the split**: "From then on, each agent reviewer session that starts
  gets a variant by these weights."
- **Live**: **Pause**, and "Live since 2 hours ago. A weight changed now applies to sessions that start from now
  on."
- **Paused**:
  - new sessions start on the current variant and aren't counted;
  - sessions already given a variant keep it;
  - **Resume** goes back to the split.
- **Promoted**: "Promoted B: agent reviewer starts on rust-dev · microvm · 4 vCPU · 8 GiB from now on. The split
  has ended; its sessions and results are kept."
- A session keeps its variant for its life, a resume included.

## Live assignment

- A table per variant: weight, sessions, the share it actually got, running now.
- Under it, the newest sessions: key, agent · project, then "warm start 219 ms · adas-mac-studio-3.local" or, in
  the fail tone, "failed to start: OOM-killed at 4 GiB", and when.

## Results, from those sessions

- The line on top: "Clear on 3 of 8 measures: 2 for A, 1 for B. The intervals narrow as sessions come."
- **Show as a table** gives the same numbers as a `data-table`.
- One block per measure that has sessions:
  - its name, unit and better direction, and at the right its verdict: "✓ A 0.74 GiB less" (ok tone) where its
    95% interval clears every other variant's, else "no clear difference yet (n = 20, 16)";
  - a row per variant: letter key, a thin interval bar with a median tick on a shared axis, the numbers as text
    ("219 (212–224)"), and its n;
  - a variant with no sessions of that kind says so.

| Measure | From each session | Better |
|---|---|---|
| Start: cold | boot to ready, ms | lower |
| Start: from its template | restore to ready, ms | lower |
| Start: warm | a warm host taken, ms | lower |
| Start: as a process (host) | a host variant's start, ms | lower |
| Footprint: peak | the microVM's (or process tree's) peak memory, GiB | lower |
| Footprint: steady | its memory once it settles, GiB | lower |
| Host CPU | CPU·s per session minute | lower |
| Failures | % of sessions that failed to start or ended in error | lower |
| Session outcome | % that ended done | higher |

- Intervals:
  - times, footprints and CPU: 95% intervals of the median, from the sessions' order statistics (kiln's
    bootstrap gives the same or near it);
  - rates: Wilson intervals;
  - failed starts count as failures and nowhere else.
- **Decide**:
  - **Promote A / B / …** (not the current one) asks in place: "Promote B? agent reviewer starts on rust-dev ·
    microvm · 4 vCPU · 8 GiB from now on, as its definition's next version, and the split ends. Sessions running
    on the other variants go on as they are.";
  - its actions are "Promote B" and "Keep the split";
  - nothing changes until that press.

## Replay (offline)

- "Run a recorded session's tool calls again, in order, in each variant's microVM, against the project at the
  session's commit. The model isn't asked: what each call returned that the next one used is played back as
  recorded. It runs offline, beside the live split, and moves no one's sessions."
- **Recorded session**: the target's sessions, each with its tool call counts ("214 tool calls (Read 120, Bash
  61, Edit 33)").
- **Against**: the variants, as checkboxes. **Times each**.
- **Replay** shows "10 replays, interleaved; each boots its variant from cold", then a progress bar.
- Results, as intervals with the same verdict rule:
  - replay time;
  - host CPU;
  - footprint peak;
  - calls that went otherwise (a call whose result differs from the recording).
- Each closes with one line in words: "B replays the session in two thirds of A's time".

## Colour and charts

- Variants take kiln's chart series: blue, rose, sand, teal.
- The dataviz validator, against `#0a0c0f`: contrast passes, but the categorical lightness, chroma and
  normal-vision checks don't. So every mark carries its letter, and every value is written beside its bar.
- The weight sliders take the variant's colour as their accent.

## What it needs from the server

- `experiments.*` for splits:
  - `target: {agent} | {harness, agentfile}`;
  - `variants: [{agentfile, engine, cpus, memory, weight}]` (weights sum to 100, checked);
  - `state: draft | live | paused | promoted`;
  - `start`, `pause`, `resume`, `weights` (a new set, from now on), `promote {variant}`.
- At session start (placement), the split's assignment: weighted, recorded on the session (`experiment`,
  `variant`), and kept across resumes.
- Per session: start kind and time, peak and steady footprint, host CPU, failure, outcome. Per variant and
  measure: median, interval, n and the verdict, served live (`experiment` events).
- Replay: recorded sessions' tool-call logs, `replay {session, variants, n}`, its runs and results.

## Tests (UI, Chromium and WebKit, desktop and phone)

- P7.2 AC1:
  - set a target and two variants at 70/30, then start;
  - sessions started for the target are assigned in proportion (the share column tracks the weights within
    their binomial interval over enough starts);
  - each session's page names its variant;
  - the results show both variants' intervals.
- Weights:
  - moving one keeps the sum at 100;
  - a variant at 0% gets no new sessions;
  - a change applies only to later sessions.
- Pause: new sessions go to the current variant and aren't counted; Resume splits again.
- P7.2 AC2: Promote B, then confirm. The target starts on B from then on (the definition's next version), and the
  list says "promoted B".
- P7.1:
  - variants differing only in size show the size's effect on footprint (peak and steady), with intervals;
  - failures are counted, not dropped;
  - no token count appears on the page or in its API.
- Replay: replaying a recorded session against two variants yields both variants' intervals, and moves no live
  session.

## Open questions for the person

1. **Whose sessions are split.** The mockup splits the sessions you start of your target. For a shared agent (a
   project's, a team's), should everyone's sessions of it be split?
2. **A session that resumes elsewhere.** A session keeps its variant for its life. If its runner can't hold
   that variant any more (the runner left, or switched engine), should it go on another runner on the same
   variant (the mockup's assumption), or fall back to the current one and leave the results?
