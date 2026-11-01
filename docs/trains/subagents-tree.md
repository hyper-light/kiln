# Train notes: what a person does with an agent's subagents

An agent's subagents as a tree a person reads, writes to, pauses, limits and branches (docs/agents.md,
"What a person does with an agent's subagents"; docs/headroom.md, "A person's pause"): what it
delivers, what deploying it does and how to watch it, its risks, its rollback, and what's still open.

**Where it stands (2026-11-01 09:10 PST, when written):**
- **Merged:** not yet. Branch `subagents-tree` (origin/main f9d50260a9 merged in at 8aa215de80) lands as a
  merge into main, main its first parent, on Ada's word.
- **Deployed:** no. Production runs an earlier build of main.
- **Runners:** none run it yet. Each runner carries its part when it's upgraded (below).
- **Gated** at 5f5f22be22 (origin/main 786549bfc7 merged in) through ci/gate.sh, run by kiln's MCP
  `run` on adacachy (12 CPUs, 40 GiB):
  - clippy for Linux (the workspace, all targets), Windows and macOS: clean;
  - kiln's lib tests: 2,568 passed, none failed;
  - the integration tests: 8 failed, each the environment's:
    - passed alone on the gate's build: actions `w07_r05`, agent_limit, resume, retries
      `w05_r15`, services `w13_ac03` and kiln-obs `store::tests::a_write_s_records_are_one_batch…`;
    - verdicts `w28_r59` fails the same way at main 786549bfc7: `ci.verdict.wait`'s first look
      finds the verdict not final yet, and its 30 s nap outlasts the test client's 30 s read;
    - pools `w13_ac04`: Docker's address pools were full ("all predefined address pools have
      been fully subnetted"), with other runs' leftovers;
  - the UI: typecheck, lint, 406 tests, the build; and every spec that opens an agent page (22)
    in Chromium and WebKit at desktop and phone widths: 254 passed, 6 skipped (desktop-only).

  After origin/main moved to f9d50260a9 (merged at 8aa215de80, conflicts only in the order of
  lists: `Command` takes `OwnEdits`, then `Pause`, `Perms` and `Branch`): the UI checks again,
  clippy for the kiln crate (all targets), kiln's harness, wire and headroom lib tests (719
  passed), and the e2e specs of this train and of main's agent page and Changes tab (14) in the
  four browsers: 162 passed, 2 skipped (phone-only).
- **Checked** by its tests:
  - `harness::claude::tests::a_paused_subagent_s_next_tool_call_waits_until_it_s_resumed_and_so_do_those_under_it`;
  - `harness::claude::tests::a_subagent_s_perms_hold_at_each_of_its_tool_calls`: a grant turned off
    refuses, Ask asks, a folder keeps file calls in it and asks for each command, and past its
    minutes or tokens it's told to finish;
  - `harness::model::tests::a_subagent_s_perms_never_let_it_do_more_than_what_it_s_under`;
  - `harness::lineage::tests::a_pause_holds_the_subagents_kiln_started_under_it_and_theirs_not_agents_it_started_beside_it`;
  - `harness::lineage::tests::a_session_s_perms_are_its_parent_s_at_most_and_narrow_the_subagents_under_it`;
  - `harness::subagent::tests::a_subagent_can_be_paused_while_it_goes_where_its_harness_holds_it_and_one_that_ended_keeps_no_pause`;
  - `harness::session::tests::a_list_says_what_each_subagent_does_and_which_subagent_asks`;
  - `harness::goback::tests::a_branch_is_a_variant_beside_it_or_an_agent_of_its_own` and
    `harness::goback::tests::a_subagents_thread_in_words`;
  - `headroom::tests::a_session_a_person_paused_is_held_all_the_time_whatever_the_engine_gave_it` and
    `headroom::tests::without_a_runner_pacing_its_work_nothing_is_paused_and_resuming_always_goes`;
  - ui/tests/subagent-tree.test.ts, and the e2e specs mission-subagent-tree, composer-subagent-tree,
    plan-step-agents, subagent-relays, subagent-thread, subagent-pause, subagent-perms,
    subagent-branch and subagent-needs-you, in Chromium and WebKit at desktop and phone widths.

## What it delivers

- **The tree.** Mission control opens an agent's subagents under its row, and the line by the
  composer opens the same tree in the agent's chat: kiln's subagents and its harness's own, each
  under the one that started it, in the order they started, each with what it's doing in a line.
  One that needs the person is tan along its path.
- **Writing to several.** Picks in the composer's tree send one message to each subagent picked
  (the To line says to whom). A message to a subagent shows in that subagent's thread, not in the
  agent's chat.
- **The Plan tab** shows each step's agent live, and a step that needs the person is tan on its
  path.
- **A subagent's thread** names its whole path (crumbs from the agent down), who wrote each message
  in it (the person, or the agent above it), and how it ended, with a link back to where it was
  started.
- **Pause.** A kiln subagent pauses whole: its machine's pacer holds its processes at share 0
  (cgroup.freeze on Linux, SIGSTOP on macOS, suspended threads on Windows), and the subagents kiln
  started under it with it. A Claude subagent (the harness's own) is held at its next tool call, and
  so are those under it. Resume lets them go.
- **What each may do.** Read, Edit, Run and Web, each Allow, Ask or Off; a folder; minutes and
  tokens. Never more than what it's under: the server narrows each to its parent's. A Claude session
  holds them at each tool call, its own and its subagents' (Off refuses, Ask asks the person, a
  folder keeps file calls in it and asks for each command, past a limit it's told to finish).
- **Branch.** A subagent branches into a Variant beside it or a New agent of its own, from its
  conversation so far and what the person says changes. A harness that can resume forks its own
  conversation; otherwise the new one is given it in words. Keep hands a finished variant's result
  to the agent it was a variant under.

## Deploying it

Nothing to set: no config and no migration.

The wire grows by three commands at the end of `Command` (`Pause`, `Perms`, `Branch`), sent to a
runner as JSON on the command stream, and by optional JSON fields (left out when empty):
`Task::pausable`, `paused_at` and `perms`; `State::frozen` and `perms`; `Info::frozen`, `perms` and
`variant_of`; `Spec::variant_of`; `SubagentBrief::last_tool`, `pausable`, `paused_at` and `perms`;
`ApprovalBrief::agent`. No postcard variant or capability bit is added.

What's where:
- **The server** carries the UI (the tree, the dock's picks and To line, the Plan tab's live steps,
  the crumbs and who-lines, the panels) and hands a session's pause and permissions on to the
  subagents kiln started under it.
- **Each runner** carries the rest for the sessions on it: the line of what each subagent is doing,
  the pause (its pacer), the permissions held at each tool call, and branching. Until its runner is
  upgraded, a session's tree shows no such lines, and Mission control can't say which of its
  harness's subagents asks; its pause and permissions are off; and a pause or branch sent to it
  says the runner can't read it ("Couldn't pause …", "Couldn't branch …").

Upgrading a runner restarts the agents on it: that's the person's to do.

Watch:
- `agents.list`: a paused session has `frozen` (when it was paused), a paused harness subagent
  `paused_at` in its brief, and each its `perms`; a variant has `variant_of` ("<agent>/<subagent>"
  for one of a harness's own), and its name is what the person wrote;
- a paused kiln subagent's processes: held where they are (on Linux its cgroup's `cgroup.freeze`
  is 1), and going again on Resume.

## Risks

- **Every tool call of a Claude session asks kiln's hook first** (`kiln-hold`, registered at
  initialize): one round trip on the control protocol, answered at once when nothing holds it.
- **A pause needs the runner's pacer.** Where headroom isn't running, or can't hold a process on
  that machine, a pause of a kiln subagent is refused and says why. A Claude subagent's pause holds
  its next call for at most seven days (the hook's timeout), then lets it go.
- **Only Claude sessions hold permissions** (the `limits` feature). For Codex, ACP agents and
  Antigravity the panel is off. Their own subagents can't be paused.
- **A folder is checked as written** (`.` and `..` worked out, links not followed). A command can
  reach outside it, so each command under a folder is asked.
- **A paused session stays paused across a runner's restart** on this build (held again as it
  starts), but not across a rollback (below).

## Rollback

To a build before this train, nothing has to be undone first:
- An older server and runner ignore the new fields: `Spec`, `Info`, `State`, `Task` and the briefs
  don't deny unknown fields. Paused sessions run again, permissions are no longer held, and
  variants show as subagents of the agent they were made under.
- A Claude subagent held at a call isn't held once its runner restarts into the older build:
  that runner's Claude sessions register no such hook.
- An older runner refuses the three new commands and says so; an older server's UI never sends
  them.

## Open

- The runner upgrade, the person's: each runner carries the pause, the permissions and branching
  for its own sessions once upgraded.
