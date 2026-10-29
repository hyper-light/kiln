# Train notes: a goal's or plan step's subagent is its agent's

The fix for goals and plan steps side by side starting full agents, not subagents that stay in the
chat of the agent they work for (docs/goals.md, "A plan's steps side by side"; docs/agentic.md's
G2): what it delivers, what deploying it does and how to watch it, its risks, its rollback, and
what's still open.

**Where it stands (2026-10-29 01:30Z, when written):**
- **Merged:** de28dd0d17 fast-forwarded main from 432773c142 on 2026-10-27 at 02:10Z, on Ada's
  word ("Merge the fix and push to main"). It merges branch `step-subagents`: 980e93c80 (subagents
  on their agent's machine, with its settings, in its chat and on its row) and 9ac0dc3c7 (they go
  along when it moves, get its harness's controls, and find searches their threads).
- **Deployed:** kiln:de28dd0d17 synced on 2026-10-27 at 02:53Z, on Ada's "Deploy it"; KilnCD's
  guard said it stayed healthy for 10m at 03:03Z. Production has run later builds of main since,
  each carrying it: kiln:b62672456d since 2026-10-29 01:06Z (healthy at 01:16Z).
- **Runners:** all seven connected runners run b62672456d, so each one opens a subagent with its
  agent's environment and program.
- **Gated** at de28dd0d17 on fedora (kiln-a094a12d) through ci/gate.sh's steps:
  - clippy for Linux (the workspace, all targets), Windows and macOS: clean;
  - kiln's lib tests: 1,922 passed, none failed;
  - every other test of the workspace passed but two, which fail only inside the gate's unit and
    pass alone on the same build, and fail on main without this change too:
    - `w13_ac01_facts`: the probe reports the unit's MemoryMax as the machine's memory;
    - `w14_ac05_github_actions_on_a_native_repository`: actions/checkout under the gate's
      GIT_CONFIG_GLOBAL;
  - the gate's guard stopped it twice on fedora's system I/O pressure, so these ran on their own
    with the gate's environment (exit 0): the tests from `pools` on, the other crates' tests and
    the doc tests;
  - the UI: typecheck, lint, 167 tests, the build.

  Ada said to push past the two.
- **Checked** by its tests:
  - `harness::lineage::tests::a_subagent_goes_where_and_as_its_agent_does`: its agent's machine,
    harness, model, effort, speed, mode and Subagents setting, whatever kiln could pick; what a
    plan run says for a step stays;
  - `harness::lineage::tests::a_subagent_is_given_its_agent_s_session_settings_alone`;
  - `harness::host::tests::a_subagent_opens_with_its_parent_s_environment_and_program`;
  - `goals::subagents::hub_tests::p2_steps_side_by_side_go_to_subagents_within_the_agent_s_setting_and_come_back`:
    each step's subagent is on its agent's machine, in its folder, harness and mode, with its
    Subagents setting; moving one by itself is refused;
  - `plans::agents::tests::side_by_side_runs_at_most_max_parallel_steps_at_once`: a person's run
    starts agents of their own;
  - `plans::agents::tests::p2_side_by_side_waits_for_room_within_the_parent_s_subagents_setting`:
    an agent's run starts its subagents;
  - ui/tests/mission-rows.test.ts: "the subagents kiln starts for an agent's goals or plan steps
    are on its row, not rows of their own".

## What it delivers

- **Subagents, not agents of their own.** kiln starts these through `lineage::start_subagent`:
  - a stack's goals or plan steps side by side (goals/subagents.rs);
  - the steps of a plan run that an agent runs (plans/agents.rs).

  Each is marked `subagent` (on `Spec` and `Info`). Only kiln sets it: `POST
  /api/harness/sessions` drops it. A subagent runs where and as its agent does:
  - on its agent's machine (a plan run may still place a step elsewhere), in its folder;
  - in its harness, with its model, effort, speed and mode (at most its own);
  - with its Subagents setting, its pause point and its way of ending a pause;
  - with its harness's other session settings (its controls with values). Each is given once the
    subagent's harness lists it, for up to ten minutes. `lineage::settle_settings` does this on
    each goal-driver tick, keeping what's still to give in kv under `agents/like/<subagent>`;
  - with its environment and program, which the runner copies as it opens
    (`Host::like_its_parent`), so it has the same account and configuration (`CLAUDE_CONFIG_DIR`,
    `CODEX_HOME`).
- **It goes where its agent goes.** Moving an agent takes its subagents on that machine along:
  a move, a usage limit, or recovering a machine that's gone. A subagent isn't moved by itself
  while its agent is there.
- **In its agent's chat:**
  - Mission Control lists none of them. The agent's row works through them and carries what one
    asks you, answered as that one's. One whose agent has gone is a row of its own.
  - The agent's chat lists them with its harness's own subagents: the line by the composer,
    Tools → Subagents, and the head.
  - Each opens its thread in place: what it asks is answered there, the composer writes to it, and
    find searches its conversation.
  - A subagent's own address opens in its agent's chat (`?own=1` keeps its own page).
- A person's plan run, and `agents.start`, still start agents of their own.

## Deploying it

Nothing to set: no config and no migration. The server carries all of it except the environment
and program, which the runner copies as a subagent opens; a runner from before de28dd0d17 opens
one with the machine's own. The wire grows by one optional JSON field, `subagent` on `Spec` and
`Info` (left out when false). No postcard variant or capability bit is added.

Watch:
- the audit log: a subagent's start is `agent.open` "1 <harness> subagent on agent:<machine> for
  <agent> (<id>)", with `goals.subagent` "<agent>: Step N: … to <subagent> (side by side with …)";
- `agents.list`: an agent's subagents have `subagent: true` and its `target`, `kind` and `cwd`;
- Mission Control: an agent running steps side by side stands as working through them, and has no
  "Step N" rows beside it.

## Risks

- **A subagent's own address hangs or crashes its page** (known since 2026-10-28). `/agents/<subagent>`
  redirects to its agent's chat through `lib/router`'s `Navigate`. That address is where
  notifications and other pages' links go, and where Mission Control's Answer and Review on its
  request went. `Navigate` navigates again whenever its props are new, and they're new on every
  render, so the redirect never lands.

  The work on Mission Control's subagent requests found this and fixed it on branch
  `mc-subagent-asks`, not on main when this was written:
  - `Navigate` keeps its element while `to` is unchanged;
  - Mission Control's links go straight to `/agents/<agent>?task=<subagent>`.

  Until that lands, `/agents/<subagent>?own=1` opens the subagent's page.
- **Its agent's machine or none.** A subagent starts only on its agent's machine, unless a plan run
  places its step. With that machine away, kiln starts none, and the goal stays the agent's to do
  itself.
- **Moving one alone is refused** while its agent is there ("it goes where its agent goes"): move
  the agent.
- **Settings it never lists** (a control its agent's harness offers and its own doesn't) are let go
  after ten minutes. Never copied: actions, a prompt's controls, and kiln's rename, reach, keep
  going, account and reload config.

## Rollback

To a build before de28dd0d17, nothing has to be undone first:
- An older server and runner ignore `subagent`, since `Spec` and `Info` don't deny unknown fields.
  The subagents it started then show as agents of their own (rows in Mission Control) and keep
  running.
- The `agents/like/*` entries left in kv are never read.
- Goals and plan steps side by side start full agents again, as before this fix.

## Open

- The redirect fix above, on branch `mc-subagent-asks` with Mission Control's handling of a
  subagent's requests, waits on Ada.
- ci/gate.sh can't say PASS on main while `w13_ac01_facts` and `w14_ac05` fail under its unit. The
  gate or the tests need a change, outside this train.
- Ada's call: a plan run an agent runs gives its steps to that agent's subagents. Limiting that to
  goal stacks alone is a small change, if wanted.
