# Train notes: what a person does with an agent's subagents

An agent's subagents as a tree a person reads, writes to, pauses, limits and branches (docs/agents.md,
"What a person does with an agent's subagents"; docs/headroom.md, "A person's pause"): what it
delivers, what deploying it does and how to watch it, its risks, its rollback, and what's still open.

**Where it stands (2026-11-01, when written):**
- **Merged:** not yet. Branch `subagents-tree` (origin/main 920961e71b merged in at 86d20b551e,
  with the ACP agents' branch at b8c4e6427a and Antigravity's at e4b4556d17) lands as a merge into
  main, main its first parent, on Ada's word.
- **Deployed:** no. Production runs an earlier build of main.
- **Runners:** none run it yet. Each runner carries its part when it's upgraded (below).
- **Checked** on 86d20b551e, through kiln's MCP `run` on adacachy and Mac Studio 3:
  - the UI: typecheck, lint, the unit tests and the build, each exit 0;
  - clippy for the kiln crate, all targets: exit 0;
  - kiln's harness and wire lib tests: 706 passed, 2 failed under the host's load (acp's catalog
    deadline test, session's takeover test: timing tests this train doesn't touch), both passing
    alone;
  - the 14 subagent e2e specs in Chromium and WebKit at desktop and phone widths: 165 passed, 3
    skipped (viewport-only);
  - not ci/gate.sh: on Ada's word, focused checks that hold its behaviour on each platform, after
    the gate's guard stopped it twice on others' I/O. clippy for the kiln crate, all targets, for
    macOS (aarch64-apple-darwin): exit 0. Its harness lib tests on a Mac (Studio 3): 603 passed,
    2 failed the same way at plain main there (acp's catalog deadline test, survivors' W27-R03
    mark), in files this train doesn't touch. clippy for Windows (x86_64-pc-windows-gnu) fails on
    two errors main 920961e71b has too (`ci/review_cli.rs`'s unused `super::*` and
    `tests/verify.rs`' `common::agents`, which is Unix only), and none of this train's.
- **Tests** that hold each harness to it:
  - Claude Code: `harness::claude::tests::a_paused_subagent_s_next_tool_call_waits_until_it_s_resumed_and_so_do_those_under_it`,
    `harness::claude::tests::a_subagent_s_perms_hold_at_each_of_its_tool_calls`;
  - Codex: `harness::codex::tests::kilns_hook_is_trusted_for_its_session_by_the_hash_codex_computes`,
    `codex_listing_kilns_hook_trusted_turns_its_pause_and_permissions_on`,
    `a_paused_codex_subagents_next_call_waits_until_its_resumed_and_the_agent_goes_on`,
    `what_a_codex_subagent_may_do_holds_at_each_of_its_calls`,
    `codex_calls_are_told_apart_by_their_tool_and_a_patch_by_its_files`;
  - ACP agents: `harness::acp::tests::acp_subagent_paused_request_waits_until_resumed`,
    `acp_off_refuses_and_ask_shows_with_reason`, `acp_bypass_still_held_to_limits`,
    `acp_session_perms_hold_own_requests`, `acp_opencode_children_are_subagents_in_the_tree`,
    `acp_nested_child_under_its_parent`, `acp_opencode_message_goes_by_its_parent`;
  - Antigravity: `harness::antigravity::tests::a_paused_subagents_next_call_waits_until_its_resumed`,
    `what_the_session_and_each_subagent_may_do_holds_at_each_call`,
    `what_it_wasnt_allowed_is_said_and_not_kept_going`, and kinds.rs's
    `antigravitys_hooks_get_kilns_hook_once`;
  - shared: `harness::model::tests::a_subagent_s_perms_never_let_it_do_more_than_what_it_s_under`,
    `harness::lineage::tests::*`, `harness::subagent::tests::*`, `harness::goback::tests::*`,
    `headroom::tests::a_session_a_person_paused_is_held_all_the_time_whatever_the_engine_gave_it`;
  - the UI: ui/tests/subagent-tree.test.ts, and the e2e specs mission-subagent-tree,
    mission-subagent-asks, composer-subagent-tree, plan-step-agents, subagents, subagent-relays,
    subagent-thread, subagent-pause, subagent-perms, subagent-branch, subagent-needs-you,
    subagent-codex, subagent-acp and subagent-agy.

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
  started under it with it. A harness's own subagent is held at its next tool call, and so are
  those under it. Resume lets them go.
- **What each may do.** Read, Edit, Run and Web, each Allow, Ask or Off; a folder; minutes and
  tokens. Never more than what it's under: the server narrows each to its parent's. Held at each
  tool call, the session's own and each subagent's: Off refuses with the reason, Ask asks the
  person, a folder keeps file calls in it and asks for each command, past a limit it's told to
  finish.
- **Per harness:**
  - **Claude Code:** all of it, through kiln's hook on its control protocol (`kiln-hold`).
  - **Codex:** all of it, through a `PreToolUse` command hook kiln gives Codex's app-server in its
    session flags, `kiln harness-hook`, with its trust for that session alone (the person's own
    hooks keep theirs). On only when Codex lists the hook as trusted; otherwise the session says
    why. Codex's hook can't ask, so kiln holds the call and asks the person itself. Its web search
    is hosted, out of any hook's reach: Web is set for the whole agent (`web_whole`), Off or Ask
    turning it off as Codex starts again once idle, the conversation going on.
  - **ACP agents:** held at each call the agent asks about (`session/request_permission`), and no
    others (`limits_asked`; the panel says so). opencode has all of it: its child sessions in the
    tree, a message passed on by the agent that started it, Pause, and what each may do. Gemini CLI,
    Goose, Amp and Devin report no subagents over ACP, so they have the session's permissions at
    the calls they ask about, and no tree.
  - **Antigravity:** all of it, through one entry, `kiln`, kiln puts in agy's own
    `~/.gemini/config/hooks.json` (`PreToolUse`, `PreInvocation`, `Stop`): each subagent agy starts
    in the tree, a message at its next model call, Pause, and what each may do call by call, Web
    included. Not a subagent's own subagents (agy doesn't report them), nor a message to one that
    ended. On once the hook is first heard.
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
`ApprovalBrief::agent`. No postcard variant or capability bit is added. The runner's harness socket takes one more JSON
request, `hook` (from the hidden `kiln harness-hook`), and the features grow by `limits`,
`limits_asked` and `web_whole`.

What it changes on a runner's machine:
- Codex's app-server starts with two more `-c` flags: kiln's `hooks.PreToolUse` and that hook's
  `hooks.state` trust. Nothing is written to the person's Codex config.
- agy's `~/.gemini/config/hooks.json` gets the `kiln` entry, beside the person's own hooks, written
  only when it differs (a new kiln binary rewrites it). Run outside kiln, agy's calls pass through
  it unchanged; once its kiln binary is gone (a version pruned, kiln uninstalled), it passes too.

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
- **What isn't held.** Codex's web search can't be held for one subagent (`web_whole`). An ACP
  agent's calls are held only where it asks before them: what its own rules let it do without
  asking goes as they say, and only opencode's subagents are shown. Antigravity's nested subagents
  aren't shown; the session's limits still hold their calls.
- **Every Codex and Antigravity tool call runs `kiln harness-hook` first**, the person's own agy
  runs too (the entry is in agy's global hooks.json): a process start and a round trip to the
  runner's socket, answered at once when nothing holds the call. agy's hook also reads the
  conversation's transcript (up to 64 MiB) to count its tokens at each call.
- **A held Codex or Antigravity call waits at most seven days** (its hook's timeout), then Codex
  lets it go and agy fails it.
- **The guard in agy's entry runs through a shell**, as agy's docs say its hooks do; that it passes
  once kiln's binary is gone is tested under `sh`, not yet against a live agy (the Mac with agy was
  offline: `~/.cache/kiln-subagents-agy/probe/probe5.sh` checks it).
- **On a phone, a request card at the bottom can lie over the panel's last rows**: swipe it away to
  reach them.
- **A folder is checked as written** (`.` and `..` worked out, links not followed). A command can
  reach outside it, so each command under a folder is asked.
- **A paused session stays paused across a runner's restart** on this build (held again as it
  starts), but not across a rollback (below).

## Rollback

To a build before this train, nothing has to be undone first:
- An older server and runner ignore the new fields: `Spec`, `Info`, `State`, `Task` and the briefs
  don't deny unknown fields. Paused sessions run again, permissions are no longer held, and
  variants show as subagents of the agent they were made under.
- A subagent held at a call isn't held once its runner restarts into the older build: that
  runner's sessions register no such hook, and Codex starts without kiln's flags.
- agy's hooks.json keeps the `kiln` entry: run by an older kiln, or with its binary gone, it lets
  every call go. Taking it out is by hand (the `kiln` key).
- An older runner refuses the three new commands and says so; an older server's UI never sends
  them.

## Open

- The runner upgrade, the person's: each runner carries the pause, the permissions and branching
  for its own sessions once upgraded.
- probe5.sh on a Mac with agy, to see the entry's guard run under agy itself.
- Devin over ACP, untested without a live Devin session: kiln shows none of its subagents.
