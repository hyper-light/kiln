# Experiments: agents deployed different ways, measured against each other (design)

Serves kiln plan 01a1cd8e862e7169a074db3d5fa33f3d's build plan P7.1 and P7.2, from the kiln × Hex × Shards design,
section G. Decision 4 (the person, 2026-11-14) holds: an arm is a microVM deployment (an Agentfile, its harness
or agent, its engine and size), compared on the deployment's own measures, with no model-token budget.

Status: design for the person's approval. Nothing here is built. The mockup is
[mockups/experiments.html](mockups/experiments.html). Press Start experiment: its 20 reps run in a few seconds,
then the results come, and you decide.

## Where it lives

- **Experiments**, `/agents/experiments`, under Agents beside Definitions and Agentfiles (crumbs `05 / Agents /
  Experiments`).
- Today's code experiments (W20-R11, an agent's question's options built and measured in worktrees) stay where
  they are, with their question. This page is for deployments.
- One page, one primary surface: the list of yours, and under it the experiment that's open.
- That experiment is set up, runs, and reads its results in that one place: no wizard pages, no popups.

## Set up

- **Its name.**
- **Arms**: two to four cards, each keyed by a letter on its colour (A, B, C, D). A card holds:
  - **Agentfile**: yours, then kiln's;
  - **Runs**: one of your agent definitions, or a harness with no agent;
  - **Engine**: `Segmented` `microvm | host`;
  - **vCPUs** and **Memory**, for `microvm`. On `host`, the card says its footprint is the process tree's.
- Add an arm copies the last one, so arms start alike and differ only in what you change.
- One line under the cards names what differs: "The arms differ in vCPUs and memory alone: that is what the
  results measure." Arms that are alike say so in the warn tone, and Start waits.
- **Workload** (`Segmented`):
  - **Starts only**: each rep starts the arm's agent to ready and stops it, cold, from its template, and warm;
  - **A command**: a project at a fixed commit, and a command run in each arm's microVM. A unit of work is one
    run of it, and a non-zero exit fails it.
  - No model is asked anything in either.
- **Reps per arm (n)** and **Where**:
  - one runner for every rep, picked by kiln for the platform (the default, so arms are comparable);
  - or each rep where kiln places it, which says it's comparable only across like runners.
- **Order, interleaved**: the first reps drawn as letter squares, a balanced `ABBA BAAB …`, so neither arm
  always goes first.
- **Measures**, fixed, as chips: start (cold, template, warm), footprint, host CPU per unit of work,
  throughput, failures.
- **Start experiment**, with "20 reps, 10 on each arm; each runs the command once."

## While it runs

- A prism `progress` bar; then "Rep 14 of 20 on adas-mac-studio-3.local · arm B running the command" and
  **Stop**.
- The order squares fill in each arm's colour as reps finish. A failed rep is outlined in the fail tone.
- A small table per arm: done of n, failed, warm-start median, units a minute median.
- The reps' log, newest first: "B rep 7 · warm 219 ms · 2.06 units/min · peak 3.02 GiB", or "A rep 4 · failed:
  cargo exited 137, OOM-killed at 4 GiB".
- "A failed rep is counted, never dropped or run again." (P7.1's test.)

## Results

- A summary line in words, with what's clear and for whom: "B does 1.69× the work a minute and had no
  failures; A starts from its template faster and is 0.71 GiB smaller. Clear on 3 of 7 measures: 1 for B, 2
  for A."
- The arms' full descriptions, and **Show as a table** (the same numbers as a `data-table`, for reading and
  for screen readers).
- One block per measure:
  - its name, its unit and which way is better, and at the right its verdict;
  - the verdict reads "✓ A 9% faster" (ok tone) where the arms' 95% intervals don't overlap, else "no clear
    difference";
  - under that, a row per arm: its letter key, a thin interval bar with a median tick on a shared axis, and the
    numbers as text, "412 (398–431)";
  - failures read "2 of 10" against "0 of 10", with their Wilson intervals: at n = 10 they overlap, and it says
    so.
- "Intervals are 95% bootstrap intervals of each arm's median over its 10 reps (failures: Wilson intervals of
  the rate). A measure names a winner only where the intervals don't overlap." kiln's experiments already
  compute bootstrap intervals (W20-R11): the same engine, new measures.
- **Decide**:
  - Keep A or Keep B records the choice, "and changes nothing else until you say";
  - the record then offers "Start reviewer this way from now on" (it writes the kept arm's size into the
    definition, as a new version) and Undo;
  - opt-in automation: keeping an arm changes no agent by itself.

## Colour and charts

- The arms take kiln's own chart series (`components/charts/types.ts`): blue, rose, sand, teal.
- Checked with the dataviz validator against kiln's panel surface `#0a0c0f`:
  - all four have the contrast (≥ 3:1);
  - all four fall short of a categorical palette's lightness band and chroma floor;
  - the pair A and B separates for colour-blind readers (ΔE 9.1), but not enough for full colour vision (ΔE
    11.8 < 15).
- So no mark relies on colour: every interval row and order square carries its arm's letter, and every value
  is written beside its bar.
- Mentioned, not changed: kiln's series are too pale to tell categories apart by colour alone anywhere they're
  used that way.

## What it needs from the server

- `experiments.*` gains deployment experiments:
  - `arms: [{agentfile, runs: {agent | harness}, engine, cpus, memory}]`;
  - `workload: {starts} | {command, project, commit}`;
  - `n` and `where`.
- The run log, with each rep's arm, round, runner, start kind and times, peak footprint, host CPU, units and
  outcome, in order (P7.1: interleaved by the log's order).
- Results: per arm and measure, the median and its interval, and the verdict.
- `decide {arm}`, and an explicit apply to a definition.
- Events, so progress moves without a reload.

## Tests (UI, Chromium and WebKit, desktop and phone)

- P7.2 AC1:
  - set up two arms that differ in size, start, and see progress move;
  - the results show both arms' intervals for every measure, and a winner only where they don't overlap.
- P7.2 AC2: Keep B marks B kept, and the list says "kept B".
- P7.1:
  - arms differing only in VM size show the effect on footprint, with intervals;
  - the log's order is the interleave;
  - a failing arm's failures are counted, not dropped;
  - no token count appears anywhere on the page or in its API.

## Open questions for the person

1. **The fixed workload.** The mockup offers starts only, or a command run in each arm's microVM. Is that the
   workload you mean, or should an arm also replay a recorded session's tool calls (still no model asked)?
2. **Footprint.** The mockup shows peak memory of the microVM (RSS on the host). Is peak the measure, or a
   steady state after start, or both?
3. **What "keep" changes.** The mockup records the choice, then offers to apply the kept arm's size to the
   agent's definition. Should keeping apply it at once, or never (a record only)?
4. **Host as an arm.** The mockup allows an arm on `host`, to measure microVM against bare metal. Keep it, or
   microVM arms only?
