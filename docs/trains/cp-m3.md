# Train notes: CP-M3, the Programs view, and CP-M2's counters

CP-M3's part of its merge train (the critical path's master plan, §6): what it delivers,
what deploying it does and how to watch it, its risks and its rollback, and what's still open.
CP-M2's counters landed on top of it and ride the same image, so they're here too.

**Where it stands (2026-10-23):**
- **Merged** on main: e49ab73 (CP-M3), then e969ec4 (the counters). Kiln Distro pushed them on
  2026-10-22 at 13:29Z as one fast-forward of f4acd56, on Ada's word ("Fix minor points, then
  land"; "Yes, land them").
- **Deployed:** production carried them by 17:17Z on 2026-10-22, in kiln:b683d91, with goal-to-plan
  (7828491 and before it) and parent news (f4acd56). It has run kiln:08bf166 since 22:51Z (healthy
  at 23:02Z). Before them it ran kiln:6adf2ba. KilnCD's history no longer reaches back to the
  first deployment that carried them.
- **Live, at 00:30Z on 2026-10-23:**
  - the view showed the critical path, with Kiln Distro as its coordinator, and one goal of 99 met;
  - six workers were assigned (T1 Amphora, T2 Quiver, T3, T4, T6 and T7);
  - the program was paused for usage. Account `claude:claude-max:e040abd213` was at 97% of its
    weekly window, which starts over on 2026-10-26 at 08:00Z;
  - production's `/metrics` had the counters.
- **Gated** on fedora, each on its commit exactly, in a fresh clone, with every step passing:
  - e49ab73, from 12:00 to 12:18Z:
    - clippy on Linux, macOS and Windows;
    - the tests: 1,598 passed, none failed;
    - the stride lab, by name;
    - the vendor checks;
    - the UI's checks (npm ci, typecheck, lint, 109 unit tests, the build);
    - Playwright on the production build: 7 passed (3 desktop-only ones skipped on the phone),
      and a first-load p95 of 108 ms.
  - e969ec4, from 12:19 to 12:30Z, alone: the same steps, with 1,604 tests passed, none failed,
    and a p95 of 138 ms. Its first run, beside e49ab73's, passed clippy, then was killed by
    fedora's OOM killer while it compiled the tests.
- **Verified adversarially (§3)** by two fresh subagents. The second, resumed, found every
  requirement met at 8cebe78. e49ab73 is that work with the verifier's minor points fixed, merged
  since with goal-to-plan and parent news and gated again.
- The landing commits' messages have the change request's evidence.

## What it delivers

- **The Programs view:** Mission control's **Programs** (`/agents/programs`), the critical path
  first (docs/programs.md). It has one row per track: its worker and how it stands, the goal under
  way and on which agent, its goals met of all, its change requests (built, merged or closed), its
  pass, slot, threshold and usage. The slots, the weights and the workers are changed there.
- **Workers by assignment** (`programs.assign`). A goal of the coordinator's stack handed to a
  worker is under way on it (`goal_items.delegated_to`), and the driver doesn't start it on the
  coordinator.
- **Turns by stride scheduling,** over the person's slots (6) and weights.
- **§5's usage rules, held by kiln:**
  - per-session thresholds: 95 for every worker, and 80 for the lowest-weight track at work;
  - above 80% of a weekly window, no new goal starts;
  - above 95%, the coordinator's stack pauses. It goes on again at 80% or under.
- **The counters** (docs/agents.md, "Metrics"):
  - `kiln_goals_sent_total{kind}`;
  - `kiln_goals_ended_total{status}`;
  - `kiln_harness_polls_total`;
  - `kiln_harness_records_written_total`.

## Deploying it

**Config: nothing to set.** Neither change adds a setting. Production's `kiln.toml` (KilnCD's diff
of `default/kiln-production`, read 2026-10-22) has `[gitops]`, `[kubernetes]`, `[local_agent]`,
`[retention]`, `[server]`, `[terminal]` and `[[repos]]`. Its `[server]` has `metrics_public = true`,
so the counters are on its public `/metrics`.

**At its first start:**
- **The migration** `20261021-1500_goal_items_delegated` adds one column that may be empty,
  `goal_items.delegated_to`. It writes no rows.
- **The view finds the critical path** as production has it today:
  - master plan `01a157fddb8276a2996246ea931c4808`, tagged `critical-path` and `master`;
  - its ten track plans, tagged `t1` to `t10`;
  - its coordinator: Kiln Distro (`01a147adf48a719e8013eed528dc8f39`), the agent its latest goals
    run went to (2026-10-20, 14:08Z). Its stack holds 107 goals, 99 of them the tracks'
    (`CP-T…`), one of those met.
- **The weights are seeded once,** the first time kiln reads the program, from the master plan's
  tracks table: T1 5, T2 3, T3 5, T4 4, T5 2, T6 3, T7 4, T8 3, T9 3 and T10 3. The slots are 6.
  From then on they change only in the view, and only by Ada.
- **No workers yet:** no track has a worker or holds a slot until Kiln Distro or Ada assigns one.
  kiln sets an assigned worker's threshold within 15 s.
- **The usage rules hold at once, for the coordinator too.**
  - A program's usage is the fullest weekly window of its agents' accounts: the coordinator's and
    its workers'.
  - On 2026-10-22 at 13:40Z, before the deploy, Kiln Distro's account
    (`claude:claude-max:37c8b361a4`) was at 73% of its weekly window, which starts over on
    2026-10-24 at 20:00Z.
  - Past 80%, Kiln Distro's stack starts no new goal, and `programs.assign` refuses to hand a
    worker a new one (409).
  - Past 95%, its stack pauses, and its own `/goal` is cleared.
  - Assigning a worker whose account is fuller raises the program's usage to that account's.
- **The first loads after a start wait for compression.** The server compresses its UI's assets
  (brotli, quality 11) in the background as it starts, and until that ends, a load waits for the
  asset it needs:
  - one 91 KB asset took 577 ms;
  - loads begun seconds after a start had a p95 of 311 ms, at load 23 on fedora;
  - a minute in, a release build serving 10 tracks and 120 goals had a p95 of 130 ms.

## Watching it

- **The view:** `/agents/programs` shows the critical path first, and its head names Kiln Distro
  as its coordinator. Its first load's p95 stays at most 300 ms, as CP-M3's step asks.
- **The coordinator's notice:** after Ada saves slots or weights, the view says kiln queued the
  message for the coordinator's session. Until it does, kiln tries again every 15 s.
- **Production's server log:**
  - `program worker's usage threshold set` (info, with the agent, the program and the
    threshold): kiln set a worker's threshold.
  - `goal stack paused for usage`, then `goal stack resumed: its program's usage is at 80% or
    under again` (info): the 95% rule, then its lift.
  - Warnings that should stay rare, since repeated ones mean the rules aren't held:
    - `program usage rule: not read`;
    - `programs: thresholds and notices not held this time`;
    - `goal stacks paused for usage: not read`;
    - `goal stack paused for usage: not resumed`;
    - `program: an agent's change record isn't one kiln can read`.
- **Production's `/metrics`:**
  - `kiln_harness_polls_total` and `kiln_harness_records_written_total` stay at 0 on the server's
    `/metrics`. They count the server's own harness host, and production's hosts no harness
    (`[local_agent]` is off). Ada's agents run under the runners' hosts, which count in their own
    processes. A host's watch starts with its first session, then looks every 10 s.
  - Observe reads each runner's in-process registry as it reads the server's, tagged with the
    runner's host, so it doesn't need `/metrics`. Once the observe branch merges, the runners'
    harness counts reach it by host (`sum:kiln_harness_polls_total{*} by {host}`), though no
    Prometheus scrapes them. The goal counters move only on the server's host, since the goal
    driver runs only there.
  - `kiln_goals_sent_total{kind="start"}` moves as goals start. A growing share of `reminder`
    means agents stop short of their goals.
  - `kiln_goals_ended_total{status}` moves as goals end, met or skipped.
  - Every series is there from the start, at 0, and counts from the process's start.

## Metric names awaiting Kiln Metrics

None.
- The counters' names and labels are the ones Kiln Metrics (agent
  `01a14bf7c0ce7451a5fd7c6f0a9764d0`) agreed on 2026-10-20.
- It was told on 2026-10-22, once they were on main, with `kiln_goals_ended_total`'s wider help
  text: every way a goal ends is counted where it ends.
- On 2026-10-23 it confirmed that all four fit Observe's rules, with no name changes: counters end
  in `_total`, the labels are bounded, there's no `host` label (Observe adds one per machine), and
  nothing is under `kiln_agent_`, which Observe's registry scrape drops.
- The runners' harness counts need no follow-up: Observe reads each runner's registry itself
  (under "Watching it").
- Kiln Metrics puts the "kiln agents" rows on its observe branch: goals sent by kind, goals ended
  by status, and harness polls and records written, each by host. The agents' message sends go
  beside them.
- CP-M3 adds no metric.

## Risks

- **No capability gate.** §6 puts every migration behind a capability, with read fallback for one
  release. This one has none: it only adds a column that may be empty, and the release before runs
  on it (below). The coordinator and Ada may want one regardless.
- **The usage rules act by themselves:**
  - near 80%, the program's new goals hold;
  - at 95%, the coordinator's stack pauses, its `/goal` cleared and its queued goal messages
    withdrawn;
  - they go on at 80% or under;
  - a stack Ada paused stays paused, and a threshold Ada set on a worker stays hers.
- **A goal out on a worker keeps the coordinator's stack open:** the stack isn't done while one is
  handed out. An assignee kiln no longer knows holds no slot. Its goal stays out until the track is
  assigned again, or the goal is brought back (`goal: null`).
- **A clear that lands late.** The pause clears the agent's `/goal`. If that clear reaches the
  agent only after the lift, it can still be read as the agent's own, and end the goal under way
  as skipped. A person's pause and resume have the same window (the verifier's minor point, left as
  it is).
- **The first loads after a start** wait for the assets' compression (under "Deploying it").

## Rolling back

- **To take back CP-M3 and the counters:** revert e969ec4 and e49ab73 on main, then build and sync
  that. Several trains have deployed on top of them since. A KilnCD rollback
  (`kiln cd rollback default/kiln-production <id>`) to a release without them, such as
  kiln:6adf2ba, would take all of those back too, with goal-to-plan and parent news.
- **First, bring back every handed-out goal** (`programs.assign` with `goal: null`), for the
  reason under "What an older kiln does" below.
- **No config to take out:** neither change adds a setting.
- **The data stays, and an older kiln runs on it:** 6adf2ba, or main with the two reverted.
  - Its migration runner applies only the migrations it lists that aren't recorded yet, so it
    skips this one (crates/kiln/src/store/migrations.rs).
  - It names the columns it reads and writes in `goal_items` (6adf2ba, and f4acd56 just before
    CP-M3), so it leaves the new one alone. Rows it writes have it empty.
- **What an older kiln does with what this release kept:**
  - **A goal handed to a worker goes back to the coordinator.** The older kiln doesn't know
    `delegated_to`, so the goal is a pending goal of the coordinator's again. Its driver sends it
    to the coordinator when the stack gets there.
  - **Workers' thresholds stay.** It keeps each session's threshold (`pause_at`) and pauses the
    session there, but nothing sets or takes back kiln's 95 and 80 any more. To lift them, change
    each worker's threshold on its page.
  - **A stack paused for usage stays paused** ("Paused for usage: …"), since nothing lifts it.
    Resume it by hand.
  - **The programs' keys stay,** read by nothing: `programs/<id>/settings`, `assignments` and
    `thresholds`.
  - **The counters are gone:** Observe sees their series stop.
- **Coming back to this release:** it reads the settings and assignments as they were left, and
  holds the thresholds again within 15 s.
- **Still to do (§6):** drill the rollback in the lab with the real binaries: an older kiln on a
  store e969ec4 wrote, then e969ec4 again.

## Ada's open decisions

1. **The capability gate** for this migration (under "Risks"): accept it without one, now that
   it's deployed, or ask for one in a later train.
2. **Compressing the UI's assets when the image is built,** not when the server starts (under
   "Deploying it"): whether, and when.
3. **Slots and weights:** 6 slots and the master plan's weights, seeded at the first read. They're
   Ada's to change in the view.

Ada has decided:
- landing CP-M3 ("Fix minor points, then land");
- landing the counters ("Yes, land them").

## Open with others

- **Kiln Distro** checks production for §6 (the train's SLOs over 24 hours) and says whether
  CP-M3 is verified live. It has assigned six workers with `programs.assign`.
- **Kiln Metrics** puts the "kiln agents" rows on its observe branch. Once it merges, the runners'
  harness counts reach Observe by host.
- **kiln's own CI** (`.kiln/ci.yaml`) doesn't run the UI's unit tests; GitHub's CI does.
