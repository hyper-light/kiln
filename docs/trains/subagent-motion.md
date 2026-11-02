# Train notes: subagents in motion, answered in Mission control, and removed

An agent's subagents, wherever their tree is (Mission control's, the composer's): one at work turns
its ring, a question is answered right under its row in Mission control, and one that ended is
removed as kiln's Remove removes an agent (docs/agents.md, "What a person does with an agent's
subagents").

**Where it stands (2026-11-02, when written):**
- **Merged:** not yet. Branch `subagent-motion` (off origin/main bbcd667358) lands as a merge into
  main, main its first parent, on Ada's word.
- **Deployed:** no.
- **Runners:** removing one of a harness's own subagents needs its runner upgraded (below).
- **Checked:**
  - clippy for the kiln crate, all targets: exit 0;
  - kiln's `harness::core` and `wire::` lib tests: 129 passed, including
    `removing_a_subagent_that_ended_takes_it_and_its_ended_ones_and_nothing_still_going` and the TS
    bindings;
  - the UI: typecheck, lint, the unit tests and the build, each exit 0;
  - the subagent e2e specs (subagent-motion new) in Chromium and WebKit at desktop and phone
    widths.

## What it delivers

- **Motion.** One at work turns its open ring through the prism once every 1.4 s, as Ship's
  running glyph does, every ring on the page's clock; its word carries the prism, as Mission
  control's working rows' do. Nothing else in the tree moves; nothing turns where motion is reduced
  or kiln's motion is paused. The path runs on unbroken from row to row: a row's piece spans the
  whole of it (what it asks and its Stop's question too) and the divider above it.
- **Answering in Mission control.** Allow, Deny and Approve answered in place already; a question's
  Answer now opens it under its row (or its subagent's, in the tree), drawn as the chat draws it
  (`Choices` `headless`: options, Other, a note, Decline and Submit), and answers it there.
- **Remove.** One that ended (Done, Failed, Stopped) has a trash where its pause was, swipes away by
  touch, and can be picked by the composer, where Remove removes those picked that ended. One kiln
  started is its session removed (`DELETE /api/harness/sessions/:id`, as Mission control's Remove);
  one of its harness's own leaves its agent's session (`Command::RemoveTask`, `remove_task`).

## Deploying it

- The server and UI: as any build of main. `remove_task` is a new JSON command
  (`ServerMsg::HarnessCmd` carries JSON); no postcard variant or capability bit is added.
- Runners: a session advertises `remove_task` in `meta.features` once its runner runs this build.
  Until then, by the composer, the trash on one of that runner's harness subagents is off. Mission
  control's tree doesn't know a session's features: its Remove of one reaches a runner that can't
  read it (which says so, as docs/wire.md has it for a new command), and the row, gone from the tree
  at once, is back on the next load.

## Risks

- A harness's own subagent removed is gone from its session's tasks for good; the call that started
  it stays in the conversation. One kiln started is its session removed, as Remove does an agent.
- The ring's motion: one CSS animation per working row (a transform only), still under reduced
  motion and kiln's motion toggle.

## Rollback

- Revert the merge. Sessions that took `remove_task` keep the tasks they lost (removed for good).

## Open

- Runner upgrades, Ada's: each runner takes `remove_task` once upgraded.
