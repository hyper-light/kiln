# Agent sessions

kiln runs coding agents (Claude Code, Codex, ACP agents, the Antigravity CLI, Pi) as structured sessions and is their
front end: it starts the harness in its structured mode, turns everything the harness says into
one normalized model (timeline events, state, meta) and turns the console's commands into the
harness's own protocol. A session runs on any machine (the server, or a runner) and outlives the
browsers watching it.

| Harness | Protocol | Adapter |
|---|---|---|
| Claude Code | `claude -p` stream-json + control protocol | `harness/claude.rs` |
| Codex | `codex app-server` (JSON-RPC v2) | `harness/codex.rs` |
| Devin, Gemini CLI, opencode, Goose, Amp, Cursor CLI | Agent Client Protocol | `harness/acp.rs` |
| Antigravity CLI | `agy` headless stream-json, one turn per message on stdin (agy 1.4 or newer) | `harness/antigravity.rs` |
| Pi | `pi --mode rpc`: JSON commands on stdin, responses and its session's events on stdout | `harness/pi.rs` |

Devin runs as `devin acp`, with its modes, models and effort announced by the harness. Its modes
include Code (`accept-edits`, its own default), Ask, Plan, Smart and Bypass; a child starts in Ask or Plan
when its parent's permissions require it. The same session controls, approvals, messages, MCP
tools and conversation resumption run over ACP. Its own terminal interface is `devin`. Live
settings, steering, interruption, queueing, resume and native terminal discovery were tested against
Devin CLI 3000.11.92.

Devin's advertised `cognition.ai/midTurnPrompts` capability enables Steer and queued-message
steering without cancellation. Concurrent prompts share a turn; kiln records their completion
and usage once. A runner upgrade refreshes this capability on existing Devin sessions without
restarting their processes. Other ACP agents keep steering queued.

Before a Devin session starts, each machine loads its model catalog with a bounded ACP handshake
in a temporary configuration that keeps its native default model, with imports, MCP servers and
automatic updates off. It sends no prompts. Each model's effort choices are read from the agent
and kept in the existing model cache for Mission control's composer, which starts agents. Shared effort choices are offered only
when every listed model
supports them. The composer uses the native default model's levels before a model is chosen,
then the chosen model's own levels. Resuming keeps the conversation's model and sends a selected
effort only if that model supports it. The native permission modes are available before discovery
finishes.

ACP sessions prefer [config options](https://agentclientprotocol.com/protocol/v1/session-config-options)
(`configOptions`) over the older `modes` and `models` fields. The first select
in the `mode`, `model` and `thought_level` categories drives kiln's existing permission, model
and effort selectors, and a select with the id `speed` (Devin's Standard and Fast, on the models
that go faster) drives kiln's speed; additional options, including ones with no category or an unknown
category, are ordinary session controls. Grouped choices keep their group names in the labels.
Boolean options use the existing toggles, advertised with `session.configOptions.boolean`.
Unrecognized option types are kept with the adapter's state but have no control. The agent's
order is retained within each selector and control group.

Every `config_option_update` and `session/set_config_option` reply replaces the full option
list, including current values and choices that depend on another setting. Requested startup
settings are applied before the first prompt, mode first and model before effort; a failed
setting holds the prompt. Later changes also run one at a time, so a model's updated effort
choices are loaded before applying effort. Agents without config options retain the legacy
setters. ACP resume
loads the harness's saved conversation; a replay already in kiln's timeline is not appended
again. Context occupancy comes from `usage_update`; token totals use the prompt's reported
usage, and costs are displayed only when denominated in USD.

kiln offers ACP's [session compaction](https://agentclientprotocol.com/protocol/v1/prompt-turn)
(`clientCapabilities.session.compaction`): a compaction an agent reports with `compaction_update`
is under way (`state.compacting`) until it says how it ended (`completed`, `failed`,
`cancelled`), then its marker shows where it ran, with the tokens and time
`_meta.contextCompaction` gives (claude-agent-acp); its summary isn't shown. Devin keeps its own
compactions out of ACP, so a `/compact` sent to an agent whose catalog has one is a compaction for
its turn, its answer saying how it went.

Cursor's CLI runs as `agent acp`, its ACP server (cursor.com/docs/cli/acp), its program `agent`
(its installer puts that and the older name, `cursor-agent`, in `~/.local/bin`). Its modes are its
own: Agent (its default: every tool, asking first for what its approval settings don't let go ahead,
which kiln places with Accept edits), Plan and Ask (neither changes anything), with kiln's Bypass
beside them; a child starts in Plan or Ask when its parent's permissions require it. Its questions (`cursor/ask_question`) and its plans to
approve (`cursor/create_plan`), requests it waits on, are yours to answer, whatever the mode: a
question's answer goes back as the ids of the options chosen, and a plan you reject carries your
words. Its to-dos (`cursor/update_todos`) are the session's. kiln never asks it to sign in by itself
(its `authenticate` would open a browser on the machine, and kiln starts it with `NO_OPEN_BROWSER`):
a session that couldn't open for want of a sign-in says so, and opens once you sign it in with
`/login`. Its own terminal interface is `agent`. Written from its docs and its CLI's own source
(2026.11.11); not yet run end to end.

The Antigravity CLI runs as `agy --input-format stream-json --output-format stream-json`: kiln
writes `{"event":"user","message":{"content":…}}` for each turn and reads `init` (its conversation,
which a restart resumes with `--conversation`), `step_update`s (the reply as it streams, each tool
call with its input and output) and one `result` per turn. In that mode it can't ask anything: a
tool that needs your approval is denied, the turn's result lists it (`denied_actions`), and kiln
says what was denied and which mode lets it go ahead. Its modes are Ask (its own default:
everything that needs approval is denied), Accept edits (a new agent's: it edits files; commands
that need approval are denied), Plan and Bypass (`--dangerously-skip-permissions`). It can't be
stopped mid-turn either: a stop ends its process, which keeps the turn so far, and kiln starts it
again on the same conversation. A new model, effort or mode is a flag, so it starts again with it
between turns. Files you attach go to it as their paths on its machine; its models come from
`agy models`.

Pi runs as `pi --mode rpc` (pi.dev/docs/latest/rpc): kiln writes one command a line (`prompt`, `steer`,
`abort`, `set_model`, `set_thinking_level`, `compact`, ...), each under an id its `response` names,
and reads its session's events (`message_update` as the reply streams, `tool_execution_*` for each
tool call, `agent_settled` once it won't go on by itself). A message's own id is its prompt's. It
asks before no tool call (its docs: "it does not ask for approval before every tool call"), so its
modes are the tools it starts with: Read only (`read`, `grep`, `find`, `ls`), Edit files (those,
`edit` and `write`: a new agent's) and Full access (every tool it has, commands included, each run
unasked: a choice that skips every check), the narrower two without MCP tools; a new mode starts it
again on the same session (`--session-id`, which a restart resumes too). Its model and thinking
level change over RPC, its models (`provider/model`) and each one's levels from Pi itself. A message
for a turn under way goes in as its steering, after the turn's tool calls and before its next model
call; a stop takes back what it hadn't taken yet (`clear_queue`), then aborts the turn. An
extension's dialogs (select, confirm, input, editor) are yours to answer; its notices are the
timeline's. Pictures go to it inline, other files as their paths on its machine. Its own terminal
interface is `pi`. Written from its docs and its RPC mode's source (Pi 1.5.2); not yet run end to
end.

Its subagents (`invoke_subagent`, each a conversation of agy's own) are tasks of the session: the
stream names each one its agent starts (a `subagent` step's `subagent_info`: its conversation, role,
type and first prompt), under the call that started it, its transcript as its output. Their own
steps aren't in the stream, so kiln hears of them from its hook: agy reads hooks only from its
hooks.json, so kiln puts one entry, `kiln`, in `~/.gemini/config/hooks.json` as an agent starts
(beside the person's own, rewritten only when this kiln's path differs): `PreToolUse` for every tool
(its answer awaited up to 7 days; past that agy fails the call), `PreInvocation` and `Stop`, each
`kiln harness-hook --agy <event>` behind a shell guard: the entry stays in the person's hooks.json,
and once that kiln binary is gone (its version pruned, kiln uninstalled) it answers agy's pass
itself, as agy fails a call whose hook fails. It hands each to the socket of the runner process
that started the session (`KILN_RUNNER_SOCK`), else the machine's harness socket (`KILN_HARNESS_SOCK`;
agy hands both on), with its conversation's tokens so far from its transcript; agy run outside kiln
gets the answer that changes nothing (`allow`). A subagent's `Stop` ends its task (completed); its
agent's `manage_subagents` kill, stopped. A subagent's own subagents are named nowhere kiln reads
(not in the stream, and a hook names only the conversation it's in), so they aren't in the tree;
the session's limits hold their calls.

An agent goes on in any other of these harnesses, its conversation carried over: picking another
harness's model in the head's line (`set_model` with `kind`; a harness that lists no models yet
offers its own default) switches it once the turn under way ends. The conversation is read from
Claude Code's transcript or Codex's rollout where it ran there, else from kiln's timeline with the
plan as it stands, and only what the model still sees goes (`harness/handoff.rs`). Claude Code
takes it as a transcript it resumes and Codex as its new thread's history, each call as its own
where it has one; Devin, the other ACP agents and Antigravity take it in words, given once with the
first message they get (a restart before that gives it then), in less room. The permission mode
goes at the same latitude, never wider (an ACP agent that names its modes only once started gets
kiln's Ask, or Bypass from a mode that skipped every check), and the effort as near as the model
takes. The to-dos go in the note it's given ("The plan as it stands"), and the session goes on
with none: the new harness keeps a list of its own, so keep-going ([goals.md](goals.md)) never
holds it to one only the old harness could change. The timeline marks the line between the two. A
message the old harness never got (one sent as Codex or an ACP agent was still starting: a harness
that can't get going is switched out of at once) goes to the new one first, still shown queued where
it was said, then what waited after it (docs/sends.md, "A harness that ends").

## Where a harness is found

A machine offers a harness (`/api/harness/kinds`, the machines Mission control's composer lists) when its
program is on the runner's PATH or on the PATH a login shell of the machine's user has, asked as
reach asks it for runs (`$SHELL -l -i`; as the runner starts, then kept until one of that shell's startup files
changes: `crate::harness::kinds::watching`). The runner says what it offers again as it changes, never on a clock: a
folder of that PATH changing (a harness installed, upgraded or removed), the login shell's PATH asked again, a
harness's sessions listing new models (`crate::harness::kinds::changed`; it looked every minute). A runner's
service keeps the PATH of the terminal it was installed from, so a harness installed since in a
folder that PATH lacks (Claude Code's and Codex's installers put theirs in `~/.local/bin`) is
found all the same, within a minute, without the runner starting again. The harness runs with
that PATH too: the runner's folders first, then those only the login shell has. An agent started
from a kiln shell (`kiln spawn`) has that shell's PATH, as it has the rest of its environment.

## What an agent left running

Every agent session's processes are held together by the system where it can (`crate::os::contain`):

| Where | How | What it gives |
|---|---|---|
| Linux with cgroup v2 the runner may write (a systemd user unit, a container with its own cgroup namespace) | a cgroup of its own, `kiln-agents/<session>` under the runner's (`kiln-agents-<pid>/<session>` for a runner its supervisor didn't start, such as a test's or a lab's, which may run in another runner's cgroup) | its harness joins it between fork and exec, so nothing it starts leaves it by forking, `setsid` or a double fork; its processes are its `cgroup.procs`; whose a process is, one read of `/proc/<pid>/cgroup`; ending them all, one write to `cgroup.kill` |
| Windows | a job object of its own, `Local\kiln-agent-<session>` | its harness is created suspended, put in the job, then let run; the job lists its processes, says whether it holds one, and ends them all; it ends with the runner, as every job kiln makes |
| macOS | followed fork by fork | its harness is watched with kqueue; whenever one of its processes forks, the children are listed and watched too; once a minute the user's processes are looked at for the agent's mark, for one that left between a fork and its listing |

Every harness kiln starts also carries `KILN_AGENT=<session id>` in its environment, and so does
every process it starts, however it's parented later: where the system holds nothing together
(cgroup v1, a read-only cgroup filesystem, a cgroup another user owns), and for sessions whose
harness started before the runner had groups, that mark is how their processes are found, which
takes looking at every process on the machine. The runner logs how it holds them as it starts
(`agents' processes are held together by`). Only the runner and the server, which start agents,
hold them together: a test or a tool that links kiln and starts a harness leaves its processes
where they are, so a test run in a kiln terminal makes no groups in the runner's cgroup. Where the
unit kiln runs in declares a floor for kiln's own processes (production's server's), the agents'
groups share what the unit has past it, so what they run never presses on the server's memory
([priority.md](priority.md), "Memory").

A harness has none of kiln's own variables (`KILN_*`) from the environment of the server or runner
that starts it (its data folder, config, supervisor's socket, admin token), nor from the shell
`kiln spawn` took the environment of (a kiln terminal's has the server's): only those kiln gives it,
`KILN_AGENT`, its sockets (`KILN_HARNESS_SOCK`, `KILN_RUNNER_SOCK`, `KILN_REACH_SOCK`) and, for
Antigravity, its kiln MCP server's (`KILN_REACH_ON`, `KILN_SESSION`). A test server an agent started
opened production's database through its inherited `KILN_DATA_DIR` (2026-11-06).

Every ten seconds the runner notes what runs under each harness and what else is the agent's (in
its group, or carrying its mark), and keeps that in its record of its sessions (beside each one's
harness process). What an agent started that runs on apart from its harness (a server started with
`&` or `setsid`, a script left going) shows with its background tasks as "left running" (task id
`left-<pid>`) while its turn goes on; stopping that task stops it and what runs under it. Nothing
outlives the turn that left it: once the agent is between turns (idle, nothing waiting on a
person), what its commands left running apart from its harness is stopped. Its harness's own
background tasks (a shell or a monitor it runs in the background, which run under it, however many
processes they make) are its work and stay until they end, it stops them, or the agent ends; a
server meant to keep going runs as one. Its MCP servers stay too, with what they run (a browser one
drives). So does a job of the machine's own: a systemd user unit (`systemd-run --user`, a service or a
scope) leaves the agent's group of processes, and kiln leaves it alone. An `ssh-agent` a shell's startup file starts (`eval "$(ssh-agent -s)"`) is no work of the
agent's at all: never shown as left running, and stopped between turns or once 30 minutes old.

An agent's harness is never something it left running, nor is anything under it: neither any
session's harness as it is now, nor a process kiln started itself (its parent is the runner, which is
no subreaper, so what a command leaves runs on under init or the nearest subreaper). A harness can
start while a look goes on (a sign-in starts every Claude Code agent's again at once): the look
knew the harness before it, so the session it belongs to is left out of that look and looked at
again ten seconds later. On 2026-10-31 such harnesses were noted as left running and stopped once
their agents were between turns, and 26 agents on one machine ended.

When the runner starts again (an upgrade, a crash, the system stopping it, a re-join), a harness
that outlived it is stopped before the session resumes (what it started is kept), so two harnesses
never work one conversation; then whatever the session started that still runs is taken back over
the same way, and named to the agent if kiln tells it to pick up a turn the restart cut short;
it goes when that turn ends, as anything else left running.
Ending the agent stops what it left running, then ends whatever else is still in its group (what
left its harness's tree included) and removes the group. kiln's own helpers and MCP servers aren't
counted.

## An agent lives until it's ended

An agent lives from its start until a person ends it, or the agent that started it does (`agents.stop`).
Its machine's record of its sessions holds it, and the server's conversation store holds its
conversation. Its harness's process is only how it runs now. When that process ends and nobody
ended the agent, whatever ended it (a signal from outside it, a machine short of memory, a crash,
an end of its own), the agent goes on. kiln starts its harness again on the same conversation, and
a turn the end cut short goes on first: the agent is told its process ended part-way through it and
to pick up where it left off. Its timeline says how the process ended ("exit 143: SIGTERM from
outside it") and that it starts again. Until it's back, it stands as working, and its `exit` says
when it starts again (`again_at`, Unix ms).

The first start again waits 2 s. Each after it waits twice as long as the one before, at most
5 minutes, and once a process runs 10 minutes the wait is 2 s again. A harness that never starts
(its program is missing), or that ends within 20 s of each of its last three starts, won't start:
it stands stopped, saying why, until a person restarts it or sends it a message. Where its
machine holds as many agents at work as it may, it starts again once there's room.

A runner that starts again (an upgrade, a crash, the system stopping it) starts each of its agents
again the same way, from its record (above).

### Crashed agents

An agent reads **offline** only when its machine is: the computer is off, or its runner can't reach
kiln. One whose harness is gone while its machine is up reads **crashed** (stand `crashed`, status
`crashed`), its `exit.error` saying why. kiln knows one so when the runner process that held it
ended and the one its machine runs now doesn't hold it (a blue/green upgrade whose new build didn't
take it: Amphora, 2026-11-12 21:12:49Z). A blue/green hand-over never leaves one so itself: a
session the new build refuses with its process running goes again as its record alone, which the
new build starts again from; refused even so, it stays with the old runner, which retires only
once it holds none.

A crashed agent is resumed in place (`agents.resume`, `POST /api/harness/sessions/:id/resume`, its
page's **Resume**): a fresh harness process on its own machine, under its own id, from the server's
copy of its conversation, so its plans and goal stack stay its own, and each of its subagents that
crashed with it is resumed with it, still its child (one that ended or errored stays so). Never
beside a process of it: only a crashed agent is resumed so, and no runner holds a crashed one. Its
harness's own record of the conversation is on its machine, so every harness goes on in its own
conversation, by its own id (`moving::Carry::Native`): Claude Code `--resume`, Codex `thread/resume`
of its thread, Antigravity `--conversation`, Pi `--session-id`, and an ACP agent (Devin, Gemini CLI,
OpenCode, Goose, Amp, Cursor) `session/load` of its session where it advertises `loadSession`. One
that doesn't load sessions starts a new one, given the conversation in words with its first
message, and says so. Its timeline says which.

**Not on Windows yet** (owed, Windows parity: kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, 2026-11-12).
W20's native hand-over moves a native session between kiln processes with its descriptors passed
over Unix sockets (`SCM_RIGHTS`), so these exist on Linux and macOS alone, their callers Unix's
(`#[cfg(unix)]`), and a Windows runner has none of them:

- **The session's mail journal, handed over with it:** its transfer manifest and journal file go to
  the process that takes it (`harness::host::Saved::mail_manifest`, `journal`, `attach_journal`),
  and the source timeline is signed before the journal changes owner
  (`harness::core::Frozen::handover_source`).
- **Promoting a session an older kiln opened** to the hand-over's epoch, blue/green:
  - its proof (`runner::custody::LegacyProof::of`, and the receipt that settles it, `receipt`);
  - its durable promotion record (`harness::inbox::Inbox::prepare_promotion`, `Host::prepare_legacy_promotion`);
  - the step to the new epoch, and its certificate from the server (`Host::promote_legacy`,
    `certify_legacy`);
  - the waits on that certificate (`runner::custody::legacy_waiting`, `legacy_identity_pending`);
  - finding a promotion under way, or one cut short, after a restart (`Host::legacy_promotion`,
    `legacy_promotion_next`, `legacy_recovery_pending`, `wake_legacy_promotion`,
    `recover_legacy_owner`);
  - the native transfer's fence of the two epochs (`legacy_fence_epochs`, `persist_legacy_fence`).
- **Their tests on Unix alone:** three of goal messages across a restart, which run a native fake
  agent (`goals::native_goal_program`, `settled_native`): a message refused on its retry keeps its
  id, and one sent before a restart keeps its id, and reaches its agent once, when cleared goals are
  brought back; and one of the host's promotion, through the native fence and wake
  (`harness::session`'s `legacy_promotion_does_not_advance_past_uncommitted_input_effects`). The
  promotion's proof, its source timeline and the inbox's promotion record are kept in Windows'
  test builds, so the rest of their tests run there too.

## How many agents a machine runs

A machine has at most so many agent sessions at work at once (`crate::harness::limit`). Only
those at work count: starting, on a turn (waiting on you for an approval or an answer in the
middle of one included), or between turns with subagents they sent off still at work, as Mission
control shows them working, or needing you mid-turn. Finished, ready, paused, stopped and ended
agents don't count, nor do ones whose goals wait for you. A new agent while that many are at work
(or waiting for room) is refused, saying how many and the limit. One already there goes to work
again only once there's room: what it's sent (a person's message, a skill, kiln's own: a goal, a
plan step, a parent's news) waits in its queue, the longest waiting going first, and Mission
control says it's waiting for room. A stopped one starts again (restarted, sent a message, or by
itself after a crash) only once there's room too. Its limit is the first of these there is:

1. **What's set for it on the server**: Runners, the runner's row opened, *Agent sessions*; or
   `PUT /api/runners/:name/agent-limit` with `{"max_sessions": 128}` (`null` takes it away). It's
   kept in the server's store, sent to the runner as it connects and whenever it changes, and kept
   by the runner for its restarts. It wins over the config.
2. **Its config's** `[agent] max_agent_sessions` (`kiln config set agent.max_agent_sessions 128`). A
   runner reads it again within seconds of a change, with nothing restarting. The server's own
   `kiln.toml` holds it for the server's own agents, read as the server starts.
3. **kiln's default**, 64.

A limit is from 1 to 4096. Lowered under what's at work, those go on, each through what waits in
its queue, and none goes to work again until fewer are. Every session a machine ran comes back
after a restart, however few the limit says then, and what a restart cut short goes on once there's
room. So does each one a newer runner process of the machine takes over.

The runner says what it holds and where that comes from. The Runners page shows it beside how many
of the machine's agents are at work now, out of all it has, and how many wait for room. The
server's own runner runs no agents: the server runs its own (the `local` machine), held to what's
set for its runner. A runner whose kiln is older than this holds 64, whatever its config or the
server says, and counts every session it has, ended ones included, until it's upgraded.

## Mission control

- **How they stand.** Under the page's title, one rule across the page has a length for each way
  your agents stand (needs you, stopped, paused, working, finished, ready, offline), as long as
  its share of them; under it, each one's count (and on a desktop its name). Working's length
  carries the prism flowing along it and needs you's brightens and dims; the rest hold still, and
  nothing moves with reduced motion or kiln's motion paused.
- **Activity and attention (plan `01a199e1b177752e84dad5dc990e6a15`).** Working means the
  main agent or one of its subagents is at work, or input is queued or waiting for room on its
  machine. A person-paused live agent says Paused, including while input waits; resuming it releases
  that process pause. Goals held for the person show that they need you in both Mission Control and
  the chat header. Requests and held goals take precedence over the pause word. Ready means neither
  agent is working, though shells, scripts and monitors may remain. Finished means none of those runs, including the commands it
  ran through kiln's Reach on other machines. A command a restarted runner can no longer follow
  is said lost, and no longer counts. Ready and Finished describe activity; neither says that an
  objective was met. Standalone agents started
  through kiln's MCP or API have rows and statuses of their own; harness-native and kiln-managed
  subagents remain on their parent's row and contribute to its activity. **Updated** marks a real
  return since you last looked, independently of status; clear it from the row's ⋯ menu (**Clear
  Updated**) or open the agent. Routine account, usage and status refreshes change neither Updated
  nor the row's time.
- **What counts as your use of an agent.** Every command you send it counts, by every path: its
  page's composer, Mission control's box, the CLI (`kiln op`), MCP and the HTTP API, with your own
  sign-in or token. So do opening it, pinning, parking, unparking, archiving and restoring it, and
  its own turns and its subagents' turns, each by when it started and when it ended. Reads and
  refreshes (snapshots, MCP servers' state, completions, a skill's text) don't count, and nothing
  another agent or kiln itself sends it (a goal, keep-going, a child's news) is yours.
  `GET /api/harness/attention/uses` lists them: `{you, uses: [{agent, kind, at, edge?}]}`, `edge`
  saying `start` or `end` for a turn's (`turn`, `subagent_turn`). Your answer times are your own:
  ending or removing an agent never shortens your usual or longest time to answer.
- **Order.** Requests that need you come first, followed by agents stopped with an error that need
  intervention. Your pins follow in a section of their own, in your order.
  Pending requests use the server's order against your observed answer times; without comparable
  history, they use oldest-first. **Stale** marks a request waiting longer than previously answered
  requests, only when enough history exists to establish that.
  The remaining rows follow the server's attention order: each chat's ACT-R activation over your
  uses of it and its own and its subagents' turns (a weight for each kind of use and one decay,
  Petrov's approximation of its history), learned from where you went. A pick from the list is a
  choice; a pin is a best choice, and a park or an archive a worst one, each with a strength of its
  own. Each choice is learned as it comes, under a prior as informative as one choice (the
  unit-information prior, on ACT-R's recommended decay with every kind of use weighted alike), and
  before learning from it each simpler order (your most recent use, your uses without agents'
  turns, the latest activity) is scored on it too. The learned order is used only once it predicts
  your next choice better than every simpler order on the same choices; until then your most
  recent interaction orders the list, and the settings say which and why. Each visit or return
  draws the order afresh from what has been learned (Thompson sampling). While you read, an agent's
  work changes its words in place, and requests still rise immediately. Your own actions (a send,
  an answer, a pin or park) move only the rows they acted on, at once, to their new rank in the same
  visit; every other row stays where it is. Park, wake and correction reasons appear on the status
  line; routine recency adds no text. Shell and monitor activity does not raise the score. What has
  been learned is kept for each person (`attention_learned`), so a server that starts again goes on
  from it. **Suggested** is the learned order; **Last Messaged** opts out and follows your own
  interactions. Other explicit sorts still apply as chosen.
- **Organization.** A row's ⋯ menu pins it (and moves a pin up or down), parks it (**Park
  until…**) and archives it (**Archive…**); the bulk **Actions** menu parks or archives the agents
  selected; on a phone a swipe left offers Park (a swipe right selects, as ever). A request marked
  Stale offers Park and Archive beside its answers. Park's times and Archive's confirmation open in
  a line under the row (or the list's head), never over the page. Pins and parks belong to the
  person. Park until later today, this evening, tomorrow morning or a chosen time; a new request, a
  new error or a message from you brings it back early. Parking a request already waiting defers
  that request; a request whose identity or time is unknown comes back conservatively. A park
  always ends, and its row says why it came back: "It asks you something", "It proposes a plan",
  "It asks for your approval", "Its goals wait for you", "It waits for you", "It stopped with an
  error", "Its park time ended", "You messaged it" (or answered or restored it), or "You brought
  it back". **Parked** remains available below the list, folded until opened, each saying until
  when and why. Bringing back a chat kiln parked by itself is marked a **Correction** of kiln, for
  review (`correction` on its placement). After a pin, park, unpark, archive or restore, one line
  with **Undo** stands where the row was (one for a batch), keeping its height, until your next
  action or you leave. Archived chats leave the list and are found under the State filter's
  **Archived**; Archive stops the agent for everyone but keeps the conversation, and **Restore**
  continues it. If Restore can't (its original harness conversation may be gone), the row offers
  **Continue from kiln's saved copy…**, which asks before starting a new harness session from
  kiln's saved copy. A failed restore keeps the archive and its history. Only people allowed to
  command an agent may archive or restore it.
- **Attention settings** (Settings → You → Attention; Mission control links there). Automatic
  parking is off until you turn it on: then a chat with nothing of its own running is parked once a
  set number of days (3 when you turn it on) pass with nothing between you and it, neither a use of
  yours nor a turn of its own. Automatic archive is on unless you turn it off: a parked chat is
  archived a set number of days (default 7) after it was parked, unless something of its own runs
  then or another person used it since, when it comes back instead. An automatic park ends at that
  archive when archive is on; when it's off, it ends after as long as the quiet that parked it (the
  parking setting's days), and the chat comes back saying its park time ended. Either way, a chat
  that comes back isn't parked again until the parking setting's days of quiet pass from its
  return. Each automatic action says why. The park menu's Later today delay and evening and
  morning hours are set there too, in local time. The group shows how the learned order predicts
  your picks against simpler orders, in words, and replays an order at a chosen time; incomplete
  historical exposure data is identified explicitly. Position adjustment learns from completed
  list visits, including departures without a selection. The order the list displayed is sent
  with what you do (a pick, an organize action); a visit you leave without a pick closes, with its
  last displayed order, when the next snapshot is asked for. It compares two sampled orders through
  the same filters, request and pin constraints. Phone and desktop evidence stays separate. Open
  visits remain incomplete; manual sorts and filters changed after a draw do not teach position
  adjustment.
- **Requests anywhere.** On any other page (and an agent's page, for the other agents), a card
  says what each agent has for you, the most pressing first: its oldest request (a tool call is
  allowed or denied there, a plan it proposes approved there, **Review** to read it, reject it or
  answer in your own words in its Plan tab, docs/plans.md "Reviewing a plan"; a question opens the
  agent); goals it paused for you (**Approve** and **Review** when they wait on a plan, else
  **Resume**); a stop of its own, its harness
  ended with an error (**Restart**); a usage limit it reached, only said, with when it goes on.
  Two at once (one on a phone), then a line for the rest. Where kiln's other notifications are,
  on every screen: at the bottom, above any passing ones. ×, or a swipe, puts one away until
  there's something new.
- **Answers at once.** An answer shows as given wherever its request showed (its row, its card, the
  agent's page) before the agent has it; if it doesn't go, the request is back, saying why.
- **Selecting by touch.** On a touch screen rows have no checkbox: tap Select in the list's head,
  press and hold a row (and drag over more), or swipe one right; then tap rows. A ring slides in
  beside each name as selecting starts, and the row under the finger stays under it, whatever the
  selection changes above it (the page scrolls by as much). Clear, or putting back the last one
  picked, ends selecting. With a mouse, rows keep their checkboxes.
- **What to do with those selected.** While selecting, the list's head is one line, the same from
  the moment it starts until it ends (by touch, from Select or a row held, before any is picked):
  the ring or checkbox that picks every agent shown (or none), over the rows' own, how many are
  selected, **Actions** and **Clear**. Actions is one agent's own menu for all of them, each action
  with its icon: **Resume** (those paused at a usage limit, and those whose goals are paused
  part-way), **Pause** (their goals running), **Move to…**, **Stop their turns**, **Restart**,
  where their commands run, keeping them going, muting their notifications, **End…** and
  **Remove** (those stopped or ended). One that none of them takes is turned off, not taken away,
  and one that's for some of them says how many ("2 of 3"); nothing in the line counts them but the
  head, so picking more moves nothing. Moving, ending and where their commands run are asked in a
  line under it, which stays as others are picked, said for them: Move offers each machine that
  takes some of them (how many, when not all, and why the rest stay), once their turns are over or
  now; End says what goes with them and ends the agents they started that still run too, unless
  unticked. Each goes to every agent it's for at once; a line says how it went (done, skipped,
  failed), opens to each agent's outcome and why, and stays until it's put away or the next one
  goes. Those all ended leave the selection. Turning to selecting and back crossfades in place
  (Select for Actions and Clear, the count's words), and each line under the head slides open and
  shut, the list under it moving smoothly; where motion is paused, each change is at once.
- **The composer.** At the top of the page, on a desktop and a phone alike, whole and the agent
  page's composer's size from the start. There is no New agents button and no dialog: with nothing selected it starts an
  agent, its words the agent's first message. Above the box is the agent page's own line,
  permissions · model · effort, and the bolt where the model goes faster (the speeds each model
  runs at come with the machine's models, `KindInfo.models[].speeds`; a machine whose runner
  lists none yet gives Claude Code's Opus models fast mode, as their agents' pages show it), each
  opened in place with the same controls. The agent is picked as its page switches one: by a model
  of another harness the machine runs, each listed under its name (one that hasn't listed its
  models yet, as "Default"; a machine that hasn't listed a harness's models takes another
  machine's list). Its permissions and effort go along as the page's switch carries them (the
  same latitude, never wider; the effort as near as the model takes): the composer keeps them as
  chosen last, with the harness they were chosen in, so a switch back finds them as they were, and
  a mode that skips every check goes along only as confirmed for this start. The box is the agent
  page's composer, the field and its bar. Under it, More
  options opens to one row each: the machine, the folder, how many, its name, its workspace, where
  its commands run, a shell to run in and the machine's past sessions to continue (closed, it says
  what isn't as it starts out). Each opens in place. Start starts it and opens it, waiting for
  nothing (docs/ci.md W21-R15): the page makes the agent's id and the server starts it under that
  id (`ids`, below), so its page opens at once, one view transition carrying each part of the
  composer to its place on the page, listed as starting everywhere agents are. Until its machine
  has started it the page shows its first message as sent and says it's starting, and its message
  box takes what's written meanwhile, kept to go once it can; a start that's refused says why
  there, with Try again (under the same id: nothing starts twice) and Back (its message back in
  Mission control's box). Its first snapshot names its settings as the composer did,
  before its harness says anything (it may wait for room on its machine, then take seconds to
  start): its permissions, model, effort and speed, by its harness's lists as the machine knows
  them; what the harness says once it runs takes their place, as it does when a switch starts
  another. Several stay on Mission control, in the list at once as they start: the line under the
  box says how they go (Try again if they couldn't), and opens the first. Files go with the first message, sent once the
  agent can take it. Everything else that started agents (Duplicate, a message's "Start new agent
  on this", Ask an agent, a shell's "Start agents in this shell", the command palette) fills the
  composer in and says where it came from, with Clear for your own defaults.
- **Messaging agents.** With agents selected (or one picked by typing @ and part of a name, machine
  or harness), the same composer writes to them, in the rows it has starting an agent: the line
  above the box is theirs, and under the box, where More options is, the To line names each, in
  the order they were picked (each new one at the end), put back with its ×, Clear for them all.
  The To line is one line: as many as fit, the rest folded into "+N more" after them. What's
  written stays as it turns, and with the same rows (the word about the machines under them too)
  it keeps its height, so the list under it doesn't move as the first is picked or the last put
  back. A prompt goes with files (the paperclip, a paste or a drop) and what you say into the mic,
  queued, steered or sent now as an agent's own box sends it. On the line above, unset, each agent
  keeps its own; chosen (the speed too), only the agents that have it are sent to, and given it
  first (Reset unsets them all). In the field, / lists kiln's own commands for all of them first
  (`/interrupt`, `/restart`, `/allow` and `/deny` what waits on you, `/end` those selected),
  each asked once in a line under the To line, then the agents' own; Stop beside Send stops the
  turns of those working. It empties as it sends, and that line says how it went; it slides open
  and shut, so the list under it moves smoothly. Every agent the list shows is Select all.

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/kinds?target=` | the harnesses each machine offers (`KindInfo[]`: id, name, verified, available, modes, structured, and its models with the efforts and speeds each takes) |
| GET | `/api/harness/sessions` | `Info[]` |
| POST | `/api/harness/sessions` | `{target: "local" \| "agent:<name>", spec: Spec, count?, ids?}` → `Info[]`; `ids`: the ids to start them under, one each (32 lowercase hex digits), made by the client so its page can open the agent's own page before it has started (docs/ci.md W21-R15); an id that's a session's already, or another start's under way, is refused (409) before anything starts, so a start asked again under its ids never starts twice |
| GET / DELETE | `/api/harness/sessions/:id` | one session / close it for good (409 while its archive or restore is under way: it goes on; end it once that's done) |
| POST | `/api/harness/sessions/:id/command` | a `Command` → its runner's answer: 204 taken, 202 `{parked: true}` (its machine is away and it waits for it), 404 when its runner doesn't have the session, 409 for another refusal (how and why) or a machine that's away for one refused while away, 504 when its link dropped before it answered (docs/wire.md, "Commands"); for a message (a `send`, a skill run with its `id`) its inbox's answer: 200 `{sent, message, accepted}` (`accepted.repeat` when its id was taken before), or a typed refusal (409 for an epoch the agent left, a machine it moved from, or an id given to other words). A message's `id` is its idempotency key; an `Idempotency-Key` header makes it from that instead (docs/sends.md), and takes any other command once under it |
| POST | `/api/harness/broadcast` | one thing to many agents on every machine: a prompt or slash command, a command, or answers to what waits on you; see below |
| POST | `/api/harness/command` | `{ids, command, key?}`: one command for the sessions listed → `{sent, report}` (the same sending as `/broadcast`); each agent's message id is made from `key` (or the `Idempotency-Key` header, else the command's own `id`) and the agent, so sending it again sends the same messages |
| POST | `/api/harness/files?name=&mime=` | a file for a later message → `{file_id, size}` |
| GET | `/api/harness/sessions/:id/tasks/:task/output?since=` | a task's output, read on its machine: `{data, offset, live, truncated, path?, gone?}` or `{error}`; without `since` the last 64 KiB, with it what came after that byte (up to 256 KiB); ask again from `offset` to follow it. Native Codex subagents return `reset: true`: replace previous output with this snapshot (last 64 KiB), regardless of `since`; `pending: true` means a live child has not produced output yet |
| WS | `/api/harness/sessions/:id/ws?z=gzip&patch=state&window=1&lean=1` | updates (below); commands go on it too, each under an id and answered `ack` or `nack` (docs/wire.md, "The UI's sockets"); `window=1`, a first load of its newest events alone, `lean=1`, settled tool calls without their bodies (docs/wire.md, "Windows") |
| GET | `/api/harness/sessions/:id/terminals` | the terminals beside it (its shell, its harness's own screen), live ones first → `[{terminal, what, command?, to_type?}]`; see below |
| GET / POST / PUT | `/api/harness/sessions/:id/goals` | its goal stack: goals one after another (docs/goals.md) |
| GET / PUT / DELETE | `/api/harness/sessions/:id/keep` | its copies on other runners (each with what the rules say of it) and its own rules for them (docs/reach.md §5.1); DELETE: its repository's and runner's apply |
| POST | `/api/harness/sessions/:id/goals/{start,pause,skip,clear,restore}` | run, pause, skip the goal under way, empty its goal stack, or bring back the goals cleared last |
| POST | `/api/harness/sessions/:id/terminals` | `{what: "shell" \| "screen", command?, cols?, rows?}`: open one beside it → 201 `{terminal, what, command?, to_type?}`; see below |
| GET | `/api/ai-sessions` | the sessions found running on the machines (Claude Code, Codex, Devin, Gemini CLI started by hand), each with `machine`, `pid`, `tool`, `id`, `cwd`, `terminal` (the kiln terminal it runs in, while kiln holds it), `takeover`, `fork` (a copy can be continued) and `managed_by?` (the agent session already continuing that conversation) |
| POST | `/api/ai-sessions/:machine/:pid/takeover` | take a found session over → `Info[]` (the new agent session); `{"stop": true}` for one running outside kiln; see below |
| POST | `/api/ai-sessions/:machine/:pid/fork` | continue a copy of a found session, leaving it running → `Info[]`; see below |
| GET | `/api/harness/history?target=&kind=&limit=` | a machine's past sessions to continue → `PastSession[]`, newest first; see below |
| GET | `/api/harness/sessions/:id/events?before=&limit=` | the session's history from the conversation store, beyond what its timeline holds → `{events}`, oldest first ([conversations.md](conversations.md)) |
| GET | `/api/harness/sessions/:id/timeline?before=&lean=` | the page of its events just before number `before`, where a window (or the last page) begins → `{events, before?, more}`, oldest first, whole turns where they fit; from the server's copy, then the conversation store ([wire.md](wire.md), "Windows") |
| GET | `/api/harness/sessions/:id/events/:event?seq=` | one event whole: what a lean one held back, asked for as it's opened ([wire.md](wire.md), "Windows") |
| GET | `/api/conversations/search?q=&agent=&kind=&tool=&status=&since=&until=&limit=` | events across every agent's conversation, newest first → `{hits: [{agent, name, machine, event}]}` |

Updates (`t`): `open {info}` (a snapshot follows), `state {state}`, `meta {meta}`,
`events {events}` (upserts by id), `stream {id, text}` (text appended to a streaming event),
`changes {files}` (the whole changeset), `reply {client, value}` (the answer to one client's
request), `reset` (the timeline starts over), `synced` (the snapshot is complete), `closed`.

A command on the socket is audited as its caller (`agent.command`), its row written beside the
socket by a writer of its own, in order: the command goes on to the session without waiting for
kiln's store, which a busy disk can hold for seconds (a message then showed seconds late).

On a slow link most of what a long conversation sends is its snapshot, and during a turn its
state changes all the time while most of it (skills, plugins, MCP servers) stays the same. A
viewer can ask for less of both, in the socket's query:

- `z=gzip`: each snapshot comes as one binary message, gzip'd: its frames a line each, `synced`
  last. A long conversation's arrives in about a tenth of the bytes.
- `patch=state`: after the snapshot's whole `state`, a state comes as only what changed in it:
  `state_patch {set, gone?}`, the top-level fields set, and those it no longer has.
- `timing=1`: each live frame is followed by `timing {published, read, arrived?, took?, runner?}`,
  the server's times with it in microseconds since the epoch (`runner`: its runner's hold, a
  duration), as `Kiln-Timing` gives an API read its own; the load lab splits slow frames by them.

`Spec`: `kind`, `cwd?`, `model?`, `mode?`, `effort?`, `resume?` (the harness's own session id),
`resume_at?`, `fork?`, `prompt?`, `name?`, `shell?`, `allow_bypass?`, `takeover_pid?` (set by a
take-over: start only once that process has ended). `parent` and `plan_run` are kiln's to set
(`agents.start`, `plans.run`): given here they're dropped. An `Info` carries them too (`parent?`:
the agent that started it; `plan_run?`: the plan run it does a step of), and `no_bypass` when it
was started without bypass.

### Terminals beside an agent

A terminal in the agent's folder, on its machine, shown under its conversation:

- **its shell** (`what: "shell"`; in the console `/terminal`, `/shell`, or `!command`, which runs
  the command there instead of sending it): for what the agent can't or won't run. One per agent:
  asking again returns it, with `command` typed into it.
- **its harness's own screen** (`what: "screen"`; `/screen`, and the commands only that screen
  does, routed `terminal` in the catalog): Claude Code's `/permissions` editor, `/doctor`,
  `/config`-like setups, Codex's `/experimental` … Claude Code starts on `command`; harnesses that
  can't start on one say `to_type: true` and you type it there. When it ends (you quit it, or close
  it), the agent starts again once it's idle, its conversation continued, to load what you changed
  (`reload_config`); even when the server restarted while it was open.

They are ordinary kiln terminals (any device can attach; the server's terminal switch and limits
apply), kept off the Terminal page and found again by the agent they belong to. "Add to message"
puts the end of what's on screen into the message being written.

### Taking over a found session

A session found running on a machine (a Claude Code, Codex or Devin TUI someone started, in one of
kiln's terminals or any other) can be continued as a kiln agent session. `takeover` on each found
session says whether:

| `takeover` | meaning | the console |
|---|---|---|
| `"ready"` | it runs in a kiln terminal: taking it over ends that terminal and continues the conversation as an agent session | offer it |
| `"stop"` | it runs outside kiln: taking it over ends it there (asked to quit, killed after 10 s), then continues the conversation as an agent session | offer it, saying so; confirm with `{"stop": true}` |
| `"outside"` | it runs outside kiln as an ACP agent, which might start anew rather than resume it: it must be quit there first | explain; once it has ended, resume it with `POST /api/harness/sessions` and `spec: {kind, cwd, resume: id}` |
| `"upgrade"` | the machine's runner is too old for this take-over | link to System → Updates |
| `"no_id"` | it has no conversation to continue yet (still starting) | wait |
| `"unsupported"` | kiln cannot run that harness as an agent session | — |

`POST /api/ai-sessions/:machine/:pid/takeover` (no body for `ready`; `{"stop": true}`, or
`?stop=1`, for `stop`): kiln closes the terminal, or for `stop` the machine ends the process
(SIGTERM, then SIGKILL with what it started after 10 s; only if that pid still runs the same
harness), starts an agent session on that machine with the same harness, folder, conversation
(its session id) and model, and answers with its `Info[]` (status `starting`). The machine starts
the harness only once the old process has ended (up to 15 s; meanwhile `state.doing` says it is
waiting, or ending it); the session's history is loaded from the conversation. Errors: 404 (not
found: it may have ended), 409 (`stop` without `{"stop": true}`, `outside`, `upgrade`, `no_id`),
400 (`unsupported`).

`POST /api/ai-sessions/:machine/:pid/fork` (no body; when `fork` is true): starts an agent session
that continues a copy of the conversation (Claude Code's `--fork-session`, Codex's
`thread/fork`), named `<name> (branch)`, leaving the found session running as it is. Errors: 404,
409 (no conversation yet, or a runner too old to branch), 400 (a harness that can't branch).

Devin's native sessions are read from its local `devin/cli/sessions.db` under `XDG_DATA_HOME`
(by default `~/.local/share`; `LOCALAPPDATA` on Windows). Only the selected main message chain
is shown. A held session lock identifies the owning ACP process and its native terminal parent;
a stale lock or an ambiguous owner does not identify a conversation. Native sessions can be taken over, and ended
conversations appear in the existing past-session list, excluding hidden and running ones.
Devin does not offer conversation branching through ACP.

### Messaging many agents at once

`POST /api/harness/broadcast` sends one thing to the agents picked, on every machine at once:

```json
{
  "to": { "all": true },
  "text": "/compact keep the test plan",
  "mode": "queue"
}
```

- `to`: `ids` (exactly those, ended or not), or `all`, or filters that must all match:
  `machines` (names; `local` or `server` for the server's own), `kinds` (`claude`, `codex`, ...)
  and `status` (`starting`, `idle`, `running`, `waiting`, `exited`). Without `status`, agents that
  have ended are left out. With none of these, nothing is picked.
- What to send, exactly one of:
  - `text`: a prompt, or `/name args`, read in each agent's own catalog as its console would: a
    prompt route sends its words, a control sends its command, a panel (`/model`) or a command the
    harness can't run from kiln is skipped, saying why; a name not in its catalog goes as typed
    (its harness may know it, as a skill). `mode` for a prompt: `queue` (after the current turn,
    the default), `steer` (into it) or `now` (stopping it). `attachments`: files with the prompt
    (or alone, without `text`), as an agent's own message takes them (each uploaded first with
    `POST /api/harness/files`, then named by `file_id`): each agent gets its own copy.
  - `command`: a command as the console sends one (`{"type": "interrupt"}`,
    `{"type": "set_mode", "mode": "plan"}`, `{"type": "restart"}`, ...). A `send` gets its own
    message id per agent.
  - `approvals`: `"allow"` or `"deny"`: answers every tool call waiting on you on each agent
    (questions are left: they need answers).
- `key` (or the `Idempotency-Key` header): sent again with the same key, each agent's message is
  the same one (its id made from the key and the agent), and each agent gets it once. Kiln's
  broadcast panel sends one again under the key it first had when the first try didn't come back.

→ `{id, sent, skipped, failed, results: [{id, name, machine, kind, result, reason?}]}`, `result`
being `sent`, `skipped` or `failed` (an agent's inbox refusing it is `failed`, saying why). Each
broadcast is in the audit log.

### Signing harnesses in and out

`/login` and `/logout` open kiln's account card (the views `login` and `logout`). kiln runs the
harness's own CLI on the agent's machine, in a terminal it opens for the purpose (never listed),
and shows what a sign-in asks for:

- Claude Code (`claude auth login --claudeai|--console|--sso`): a page to open on any device, then
  the code that page shows, pasted back (a browser on the same machine may finish without one).
- Codex (`codex login --device-auth`): a page and a one-time code to enter there; it finishes by
  itself. Or an OpenAI API key (`--with-api-key`), never echoed.
- Devin (`devin auth login --force-manual-token-flow`): a page to open on any device, then the
  code that page shows, pasted back. Its account card reads `devin auth status`; signing out runs
  `devin auth logout`. The CLI may also try to open that page in the machine's default browser;
  the link in kiln works from another device. After sign-in, existing ACP sessions reload their
  conversation before queued messages continue, refreshing credentials and model choices while
  retaining their permission mode.
- Cursor CLI (`agent login`, with `NO_OPEN_BROWSER` so it opens no browser on the machine): a page
  to open on any device; it finishes by itself once you've signed in there. Its account card reads
  `agent status --format json`; signing out runs `agent logout`. After sign-in, its sessions take it
  through ACP's `authenticate` (`cursor_login`, the sign-in its CLI has on the machine) and reload
  their conversation, or open the one they couldn't for want of it.
- Pi signs in to each model provider on its own (`pi auth login <provider>`). kiln offers the
  subscriptions it signs in to with no browser on the machine (`--no-browser`): Claude Pro or Max
  (a page to open on any device, then the code Anthropic shows, pasted back) and ChatGPT Plus or Pro
  (a page and a one-time code to enter there; it finishes by itself). Its account card reads
  `pi --offline --list-models`: signed in when some provider has credentials, those providers its
  method. Signing out runs `pi auth logout` for each of the two. Its other providers sign in in its
  own screen (`/screen`, then its `/login`), or by their API keys in its environment. A session
  starts again on its conversation once idle, to take the sign-in.

Not on Windows machines yet. What is typed in goes to the CLI only: never logged, kept once the
sign-in ends, or shown in its output.

| Method | Path | |
|---|---|---|
| GET | `/api/accounts?target=local\|agent:<name>` | `Account[]`, for the harnesses installed there: `kind`, `name`, `signed_in` (`null`: couldn't tell, see `error`), `method?`, `email?`, `org?`, `plan?`, `methods` (`{id, label, asks?}`) |
| POST | `/api/accounts/login` | `{target, kind, method?}` → `Flow` (running) |
| GET | `/api/accounts/flows/:id` | `Flow`: `state` (`running`, `done`, `failed`, `cancelled`), `url?` (the page to open), `code?` (to enter there), `asks?` (`code` or `api_key`: waiting for it), `lines` (its output, never what was typed), `account?` (once ended), `error?` |
| POST | `/api/accounts/flows/:id/input` | `{text}`: the code or the key it asks for → 204 |
| DELETE | `/api/accounts/flows/:id` | cancel it → 204 |
| POST | `/api/accounts/logout` | `{target, kind}` → the finished `Flow`, with `account` |

Each change is also an `account_flow` event (`{type, flow}`) on `/api/events`.

### Continuing a past session

`GET /api/harness/history?target=local|agent:<name>&kind=claude|codex|devin&limit=50` lists the
conversations a machine keeps that are not running, read on that machine from its own
transcripts (Claude Code's `projects/` in `CLAUDE_CONFIG_DIR` or `~/.claude`, Codex's `sessions/`
in `CODEX_HOME` or `~/.codex`) or Devin's native session database: newest first, `kind` for one
harness, `limit` up to 200 (50 by
default). Each: `kind`, `id` (the harness's session or thread id), `title` (Claude Code's
`/rename` or generated title, Codex's thread name; else the last prompt's first line), `cwd`,
`branch?`, `model?`, `last_activity` (unix seconds of the transcript's last write),
`last_prompt?` (clipped).

Not listed: sessions running in a terminal (they are in `/api/ai-sessions`), Claude Code sessions
with a live session record of any kind, and the conversations kiln's own agent sessions on that
machine hold (running or not: they are in the agents list). A Codex thread's forks and later
parts count once. Only the head and the tail of each transcript are read, again only when it
changed. Errors: 400 (a bad `target` or `kind`), 404 (an ephemeral runner, which keeps none), 502
(the machine is not connected or did not answer).

Continue one with `POST /api/harness/sessions` and `spec: {kind, cwd, resume: id}` on the same
target. Leave `model` unset: the harness keeps the conversation's own. Its history is in the
timeline when the session turns idle (Codex's latest 100 turns, listed with
`thread/turns/list`; a notice says when earlier ones are not shown). Claude Code writes a
`/rename` into its transcript with the next exchange: a name given just before the session ends
is not kept.

## Whose agents

Each agent session is someone's: the person who opened it, whatever token they used, or the person
it acts for (`crate::mcp::whose`). A child is its parent's person's.

| Caller | Reaches |
|---|---|
| The owner, or a person with `agents.manage` (the `maintainer` role has it) | every session: lists, reads, watches, commands and closes it |
| Any other person | their own sessions, their children included, as fully; no other |
| An agent | itself and the agents it started (and theirs), to control, interrupt and end; it sees (lists, reads, watches) the other agents that work for its person, and may send them steered or queued messages ("Messages between agents") |
| A job, a peer | no session |

A session a caller doesn't reach is one kiln doesn't run, as far as they can tell. It isn't in their
list (`GET /api/harness/sessions`, `agents.list`, the events socket's agent events, conversation
search). Every route under `/api/harness/sessions/<id>` and the session's socket answer 404, the
socket before it's upgraded. So do the agents operations (`get`, `children`, `result`, `changes`,
`workspace`, `send`, `interrupt`, `stop`, `change`, `land`, `wait`), over MCP and `/api/ops`. A
session they see but don't act on (an agent watching a fellow agent) is read-only: its socket
answers each command with an error. A broadcast and `POST /api/harness/command` pick only among
the sessions the caller acts on, and name the others they were asked for (`unreached`). A session
kiln can't say whose it is (opened before kiln kept that) is reached by the owner and managers
alone.

Agents run programs as their machine's user, as terminals do, so opening one needs
`runners.terminal` on that machine: `runner:<name>`, or the server's own scope for `local`. That
holds for `POST /api/harness/sessions`, for `agents.start`, and for a plan's steps, which start
only where the person who ran the plan could start them. The sessions found running on a machine
(`/api/ai-sessions`: listed, read, taken over or branched) and the ones it keeps
(`/api/harness/history`) need it too. A session taken over or branched is the person's who did it.

Commands and closes are audited as the person who made them. A request's own row names them, and
what its handlers note joins that row. A command sent on a session's socket gets a row of its own,
`agent.command`, which names the command's kind but not what it says.

## Agents that run agents

An agent can start agents for parts of its work, on any machine, wait for them and read what
they concluded (docs/agentic.md, O). Each agent it starts is its **child**: the child's `parent`
is the agent that asked, which kiln knows from how it asked (its MCP token, its runner vouching
for it, or the machine seeing its process), never from what it asked. A child gets no more than
its parent has, and what it asks a person stays the person's: no operation answers another
agent's approvals, questions or plans.

Mission Control lists every agent kiln runs, in one flat list. The one exception is a card's helper
(`agents.about`, below): it answers what a person tapped in a conversation, so it's no row, its
parent isn't told of it, and kiln ends it once no card has read its answer for 90 seconds. In
Ship, code no agent made (a person's change, a lone commit, a host's pull request) is asked of a
helper kiln starts for the person instead (`ci.repos.about`, `POST /api/repos/:repo/about` with the
`commit` the code is at): it gets the repository at that commit into a folder of its own, reads and
searches it there and changes nothing, and it's the card's in the same way. An agent started through kiln's API
is a top-level agent, whoever started it: a row of its own, in the counts, filters, conversation
search and bulk actions, with no nesting under the agent that started it. A harness's own
subagents stay subagents: they aren't rows, and their status and output stay inside their agent's
chat, including after they finish. So do the subagents kiln starts for an agent's goals or plan
steps side by side, and for each step of a plan run the agent runs (`subagent` on the session,
set by kiln alone: `lineage::start_subagent`; docs/goals.md, "A plan's steps side by side"):
sessions of their own on the agent's machine, with its harness, folder, model, effort, speed, mode,
Subagents setting, pause point, environment and program. The agent's row works through them and
carries what one asks you (answered as that one's); its chat lists them with its harness's own
(the line by its composer, Tools → Subagents), each opening its thread in place, and a subagent's
own page opens there too (`?own=1` keeps it a page of its own). One whose agent has gone is a row
of its own. A subagent that waits on you, either kind (its harness's own by the requests naming it,
`Approval.agent`), says so on the line by its agent's composer, and has a row over the composer
(two at most), drawn as Mission Control's row that needs you: its name, what it asks, and the
answer, given as that subagent's. A question opens its thread, as the row and Review do; on its
agent's page, a subagent kiln started has no card of its own for it.
The session list carries small
`subagents` summaries derived from `state.tasks`; prompts and output stay in the conversation.
Shells, monitors, workflows and background MCP calls are not counted as subagents. These summaries
do not create kiln sessions or change the harness's isolation, permissions or execution.

### Told, not made to wait

Nothing makes a parent wait for its children, or for its own work that runs on its own (a
subagent or a command its harness started in the background): it decides when to wait.

- **When a child settles** (its turn over, a person needed, stopped, ended, paused at a usage
  limit), kiln tells its parent (`crate::harness::told`), in a message after the parent's turn
  that starts its next turn if it's idle, as Claude Code tells an agent of its own background
  subagents: "Agent “Fix the parser” (01a1b2c3), which you started, ended its turn: … agents_result
  {"agent": …} has its whole reply and what it changed." Children that settle together come in
  one message; only a settle after work the server saw is told (not one from before a restart), a
  few seconds on (a wait under way returns first). What the parent saw itself through
  `agents_wait` or `agents_result` isn't told again.
- **One message of news waits at a time.** While the parent's turn goes on, its news waits in its
  queue as one message, each child's latest line in it once: a child that settles again changes
  its line there, one that settles anew joins it, and nothing more is queued behind it (kiln edits
  the message, `queue_edit`). What the parent sees itself (`agents_wait`, `agents_result`) comes
  out of it, and the message goes (`queue_remove`) once no line is left. So a parent that spends
  hours in one turn finds, when it ends, each child's latest news, not every settle since. kiln
  sees the queue as the parent's machine last reported it: an edit that reaches the machine just
  after the turn ended, the message gone as it was, does nothing there, so the lines it carried go
  again in the next message. The message names at most 64 children (those that settled last) and
  says how many more did. Its id says it's news (`news-…`): a server that started since (an
  upgrade) finds the one waiting and adds to it, what it said first.
- **Waiting is the parent's choice**: `agents_wait` blocks until its children settle, for one that
  can't go on without them.
- **A parent whose children are at work isn't prodded**: keep-going (docs/goals.md) and a goal
  stack's reminders leave it be until they settle, as keep-going does while its background tasks
  run.
- **A Claude Code subagent in the background is its own**: its output (forwarded with its parent
  call's id) shows under it, and doesn't make its parent's ended turn start again, so a message to
  the parent goes to it at once rather than after the subagent. Claude Code starts the parent's
  next turn itself when the subagent is done.

### Operations

Each is an operation of kiln's one registry: an HTTP route, an MCP tool (`agents_start`, …), a
code-mode function (`kiln.agents.start`) and, from a kiln shell, a CLI command.

| Operation | Route | Input → answer |
|---|---|---|
| `agents.list` | `GET /api/harness/agents` | `machine?` (a runner's name, `local` for the server), `harness?`, `state?` (a stand or a status), `parent?`, `plan_run?` → `{agents: [Agent]}` |
| `agents.get` | `GET /api/harness/agents/:id` | → `{agent: Agent, info, state}` (its state without the catalogs) |
| `agents.start` | `POST /api/harness/agents` | `{kind, on?, cwd?, prompt?, model?, effort?, mode?, name?, count?, workspace?}` → `{agents: [Agent], parent}` |
| `agents.about` | `POST /api/harness/agents/:id/about` | `{path, code, line?, end_line?, selection?, change?, question?, now?}` → `{agents: [Agent], itself}`: a child of the agent, reading only, answers what a person tapped in its conversation (a name, a line, a change) in four parts (what it is, what it does here, where else it's used, what's worth knowing); when the agent runs no subagents (`max_subagents` 0) it answers itself (`itself: true`, `sent` as `agents.send` answers), after its turn or with `now` between its steps. The child is a helper (`helper: "about"` on it): no row of Mission Control, its parent not told of it, ended once no card has read it (`agents.result`) for 90 seconds |
| `agents.send` | `POST /api/harness/agents/:id/send` | `{text, mode?: "queue" \| "steer" \| "now", key?, status?: {key, task?}}` (or the `Idempotency-Key` header) → durable receipt `{sent: true, retained: true, message, id, revision, generation, disposition, repeat}`: retained custody is separate from actual model consumption; mode defaults to `steer` for agents, `queue` for people ("Sending again" and "Replaceable progress", below) |
| `agents.messages` | `GET /api/harness/agents/:id/messages` | `{from?, status_key?, task?, after?, bytes?}` → byte-bounded `{items, next}` of accepted originals, including superseded statuses; another sender's incoming history requires permission to see the recipient conversation |
| `agents.message_body` | `GET /api/harness/agents/:id/message-body` | `{revision, from?, path?, offset?, bytes?}` → bounded original-value inspection: JSON-pointer object keys, array length, scalar or UTF-8 string range; `/$transfers` inspects custody-transfer receipts |
| `agents.wait` | `POST /api/harness/agents/wait` | `{agent?` or `agents?` (a list, or `a,b`), `until?: "settled" \| "change"`, `any?`, `timeout_s?`, `hand_over?}` → `{done, timed_out, waited_ms, until, agents: [Agent], messages?: [{from, name, text, at}]}` (`messages`: what other agents sent the agent that waits, handed over: "Waiting") |
| `agents.result` | `GET /api/harness/agents/:id/result` | → `{agent, reply: {text, at}, last_turn, changes: {files: [{path, added, removed, created, deleted}], files_changed, lines_added, lines_removed}, cwd, machine, cost_usd, workspace, branch, change_request}` |
| `agents.children` | `GET /api/harness/agents/:id/children` | → `{children: [{id, name, kind, status, parent}]}`: the ones still running, and theirs |
| `agents.interrupt` | `POST /api/harness/agents/:id/interrupt` | → `{interrupted}` |
| `agents.resume` | `POST /api/harness/sessions/:id/resume` | → `{resumed, subagents: [id]}`: a crashed agent (stand `crashed`) goes on in place, under its id ("Crashed agents", below) |
| `agents.stop` | `POST /api/harness/agents/:id/stop` | `{children?}` → `{ended: [id]}`: with `children`, what it started (and theirs) end first |
| `agents.change` | `POST /api/harness/agents/:id/change` | `{title?, body?, draft?, ready?, trunk?, fix_checks?, threads?}` → `{change_request: Change}`: opens its change request from its work, or pushes a new version; `draft` opens it a draft on every host, `ready` marks its draft ready, and `threads` names the threads a new version fixes ("Change requests from agents", below) |
| `agents.changes` | `GET /api/harness/changes` | `agent?` → `{changes: [Change]}`, newest first |
| `agents.workspace` | `GET /api/harness/agents/:id/workspace` | → `{workspace: {kind, over, sub, volume, mount} \| null, drift: [path]}` ("Workspaces of their own", below) |
| `agents.commits` | `GET /api/harness/agents/:id/commits?picks=&branch=&trunk=` | → `{log: {top, head, branch, trunk, base, dirty: {files, added, removed} \| null, commits: [{sha, parents, subject, author, at, files, added, removed, session}], more, told, branches, listed}, picked: {from, files: [FileChange], needs: {pick, commit, subject}} \| null}` ("Commits", below) |
| `agents.refs` | `GET /api/harness/agents/:id/refs` | → `{refs: {branches, tags, commits: [{sha, subject, at, ...}], trunk}}` ("Compare", below) |
| `agents.compare` | `GET /api/harness/agents/:id/compare?from=&to=&split=&path=` | → `{compared: {files: [FileChange], counts_only, split, changed_by: {path: [who]}}}`; a point is `now`, `began`, `agent:<who>` or a branch, tag or commit ("Compare", below) |
| `agents.land` | `POST /api/harness/agents/:id/land` | `{}` → `{plan, excludes, can_write, why_not?}`; `{manifest}` (that plan's; a person only) → `{landed}`, or `{changed, plan, …}` with nothing written when the plan isn't that one any more |
| `agents.credential` | `POST /api/harness/agents/:id/credential` | → `{token, expires_at}`: the agent's own token for git, for a day, so its pushes to kiln's repositories go as the agent, for its person (ci.md W27-R03). Only that very agent is given it, as its runner asks when its git needs it: never a person or another agent, and it's no tool of an agent's |

Each takes an input type of its own (`crate::ops::Input`), as every operation of the registry
does (kiln's own, CI's, releases' and deployments'): its JSON Schema (the MCP tool's
`inputSchema`, `GET /api/ops`' `input`) is generated from it, and a call's input is read as it
before the call runs. A field it doesn't take is refused, as its schema
says (`additionalProperties: false`), and so is one of the wrong type. Over MCP that's a tool error
(`isError`) whose structured content says what's wrong (`code`: `unknown_field`, `missing_field`,
`invalid_type` or `invalid_value`), where (`path`, a JSON Pointer) and why, so the agent can
correct the call; it's noted as a message kiln couldn't read (docs/wire.md, "What can't be read"),
on the agent's page.

The tools that aren't operations are typed too: code mode's `code_search` and `code_run`, and on a
runner its own MCP server's tools (`run`, `runners`, `output`, `stop` and the page tools) and a
program's `kiln.machines.*` (docs/reach.md §7). Their arguments are read as their types before
they run, and refused and noted the same way.

Every route needs `agents.run`. An agent is named by its id or the start of it (8 characters, as
lists shorten it). Called by an agent, `agents.change` without `agent` is about that agent itself,
as are the goal stack's operations (`goals.*`); what ends, stops or steers an agent always names it.
kiln's MCP server tells each agent its id. `workspace` is `folder` (the folder itself, the
default) or `volume` (a Slates volume of its own, where its machine can make one); in a result it
is `null` for an agent in its folder, and `branch` and `change_request` are `null` until it has a
change request.

An `Agent` is `{id, name, kind, machine, cwd, status, stand, mode, model, queued, summary,
files_changed, lines_added, lines_removed, cost_usd, doing?, parent?, plan_run?, last_turn?,
pause?, exit?, needs?}`. `stand` says where it is, in a word:

| `stand` | |
|---|---|
| `working` | a main or subagent turn is under way, or input is queued, was just sent, or waits for room |
| `idle` | neither agent is at work, with no queued input or wait for room |
| `needs_you` | an approval, a question, a plan or held goals wait on a person (only a person answers them) |
| `paused` | a person paused its processes, or its account reached a usage limit |
| `stopped` | it crashed or couldn't start (`exit` says why) |
| `offline` | its machine is away; it goes on when it's back |
| `ended` | it ended, or kiln doesn't list it any more |

An agent that couldn't start keeps the first message it was given until its harness takes it: a
Restart once the harness is installed, its runner starting again, or a move to a machine that has
it (docs/reach.md §8) gives it then.

### Where a child runs

`on` is a machine's name (`local` or `server` for the server, `agent:<name>` too), or what it
needs, as reach writes it: `linux/amd64`, `macos`, `arm64`, `+gpu`, `cpus>=16`, `mem>=32g`. Of
the machines that suit and have the harness installed, the idlest (most idle cores, then free
memory) gets it; outposts and job pods never do. Without `on`, an agent's child runs on the
agent's own machine, in its folder unless `cwd` says otherwise; a person's runs on the server
when it runs agents, else on the best runner with the harness. A child on another machine starts
in that machine's home unless `cwd` names a folder there. A machine named, or the agent's own,
that kiln knows hasn't the harness (its list of harnesses says so; the server's own asked again
with its user's login shell) is refused, naming the machines that have it, rather than given an
agent that can't start there.

### Sending again

A message's id is made once, by whoever sends it, and every retry carries it: the agent's inbox
takes each id once (docs/sends.md). Give `agents.send` a `key` of yours (1 to 256 bytes; over HTTP
the `Idempotency-Key` header does too) and a call with the same key is the same message: the agent
gets it once, and every call names the same committed publication, `repeat` on the ones after.
A key is its caller's: an agent's is the same on whichever machine it runs, so its retry after it
moves is the same message. Without a key, each call is a message of its own. A key given to other
words is refused (409). From a kiln shell: `kiln agents send <id> --key <key> <text>`.
Use `--queue`, `--steer` or `--now` to choose the mode explicitly.

### Messages between agents

Agents that work for the same person can talk to each other, whoever started them: any of them may
send any other a message (`agents.send`), steered into its turn by default (`steer`), or after
its turn when explicitly queued (`queue`). A person's default remains `queue`.
Agents a person started apart (one on kiln, one on Mantle, one on Focal) work things out between
them, without the person carrying notes.
The message reaches its receiver as one from a fellow agent, never as its person's:
"Agent “Mantle” (01a14887, claude on adas-macbook-pro-2.local), which works for the same person as
you, sent this message. It's from a fellow agent, not from your person. To answer, send it a
message: agents_send {"agent": "…", "text": …}." Steering uses the harness's support for input
during a turn; a harness without that support queues it. A receiver waiting for its children
(`agents_wait`) gets queued messages with the wait ("Waiting", below). Control stays with
the agents an agent started: sending now, interrupting, ending, and their
goals and plan runs are theirs alone. Agents that work for different people aren't sent to.

### Replaceable progress

Use `status: {task, key}` only for progress whose latest pending value may replace an earlier
value of the same purpose. The stream belongs to the authenticated sender and recipient;
`task` is your workflow label, not source task routing or extra permission. For example:

```json
{"agent":"receiver-id","text":"The focused check is running.","key":"check-publication-1","status":{"task":"parser-fix","key":"validation"}}
```

A later changed progress report uses a new publication `key` and the same `status`. Retrying
the same publication uses its original `key` and exact content. Every accepted original stays
in `agents.messages`; only never-dispatched status presentation is replaced. A dispatched
revision is frozen, and its latest successor waits for actual consumption. A retained receipt
does not say that the model has read the message. Typed backpressure/storage failure means
no custody was accepted, so keep the original and wait for the actual readiness event.

Publish progress when it changes what the recipient can do. Send required policy readbacks once
per task/policy revision; unchanged “still waiting” or build checkpoints do not need repeated
messages. Instructions, requests for decisions and final results use ordinary distinct sends.
Kiln never infers replaceability from those words or silently merges them.

### No more than its parent

| Rule | |
|---|---|
| Its mode | at most its parent's, in one order for every harness: plan < ask < accept edits < auto < bypass. Asked for more: refused, saying which modes it may have. Asked for nothing: its harness's default when that's within its parent's, else the most its harness has that is (always given, so the harness's own settings can't start it higher). |
| Bypass | only under a parent in bypass that was itself allowed it (`allow_bypass`); a child of a parent without it can't be switched to bypass either |
| Its reach | a parent whose commands run only on its own machine (`reach` off) has children that run there too, with reach off |
| Depth | at most `[agents] max_depth` (3) agents deep below the one a person started |
| Children | as many subagents at work at once as its own settings say (Subagents → At once), one pool whatever each does: the agents it started that work, wait on a person or are held to a goal still to go on a stack that runs, and its harness's own still going; one idle with its work done, or ended, holds no place. None set: no limit, unless kiln's config sets `[agents] max_children` for every agent |
| Messages | an agent may send a steered (the default) or queued message to any agent that works for the same person, whoever started it (`agents.send`, `steer` or `queue`): its receiver is told who sent it, that it isn't from its person, and how to answer ("Messages between agents") |
| Control | an agent sends now, interrupts and ends only itself and the agents it started (and theirs); the same holds for their goals (`goals.add`, `goals.set`, `goals.control`, and the console's goal routes when an agent calls them) and for running a plan on an agent that runs (`plans.run` with `session`); it stops and retries only the plan runs it ran, or that the agents it started ran (`plans.runs.stop`, `plans.runs.retry`) |
| Plans | a plan run on a new agent (`plans.run` with `new`, any mode) starts it as `agents.start` would: the caller's child, under every rule above |

The modes, harness by harness: Claude Code `plan` (plan), `default` and `dontAsk` (ask),
`acceptEdits` (accept edits), `auto` (auto), `bypassPermissions` (bypass); Codex `read-only`
(plan), `untrusted` (ask), `auto` (auto), `full-access` (bypass); the Antigravity CLI `plan`,
`default` (ask), `accept-edits`, `bypass`; ACP agents kiln's `kiln-ask` and `kiln-bypass`, and
their own modes by their words (`yolo` is bypass, `autoEdit` accept edits, `plan` plan). A mode
kiln can't place is refused for a child; a parent in one counts as ask. The limits hold when a
child starts: a person may change its mode later, as for any agent.

Codex applies approval and sandbox settings per turn. Changing them during a turn interrupts
that turn and continues its work with the new settings before queued messages. A stop or a new
"Send now" message takes precedence over that continuation. Pending requests from the interrupted
turn are cancelled, never approved automatically. Choosing approval and sandbox settings
separately shows `Custom` unless they match a preset. An explicit Full access choice is retained
across restarts.
Runners advertise this behavior as `permissions_live`; the picker can also reapply the current
preset to a turn inherited from an older runner. Older runners still show "From the next turn."

A native Codex subagent keeps the settings it started with (its spawner's) until a turn kiln
starts on its thread for a person's message ("Messages to its subagents", below), which takes the
session's settings then; Codex has no other way to change them. kiln notes them when it
sees the spawn. A change stops (interrupts) subagents still working with more than the new
settings allow. Under an approval policy of `never` (Full access), nothing started with the
session's settings asks, so a request for approval comes from work started with earlier ones: the
session's own turn (one inherited from an older runner) is interrupted and continued with the
current settings, and a subagent is stopped. Such a request is held back from the person while
that work stops, which withdraws it; it's never answered for them. Should Codex refuse to stop it
(or it's still waiting after 30 s), it's shown after all. The parent hears which subagents kiln
stopped and is told to start new ones for their unfinished work, since new ones take the current
settings: in its turn, or in the turn continuing after the change (a change between turns that
stopped some starts one), else with the next message it's sent.

### Waiting

`agents.wait` holds no thread and no lock: it listens to the server's changes to the agents it
waits for and looks again every 5 seconds. It returns as soon as they're `settled` (each idle,
needing a person, paused, stopped or ended), or with `until: "change"` when anything about them
changes, or `any` when the first is; else once `timeout_s` passes: by default 300 seconds, **at
most 600 per call** (call again to wait longer). A message sent with `agents.send`, or a prompt
`agents.start` gave, counts until the agent has had its turn: an agent idle with a message on
its way is `working`. An agent kiln no longer lists has `ended`.

An agent that waits is in a turn, and what's queued for it waits behind that turn. Messages other
agents sent it (`agents.send`, `queue`) don't have to: its wait also returns as soon as one is in
its own queue (`done` then says whether the agents it waits for are settled, and `timed_out` is
false), and hands them over in `messages`, each with who sent it (`from`, its id, and `name`), its
words (`text`, without what kiln says of a fellow agent) and when it was queued (`at`), taking them
out of its queue: they're delivered now, not again after the turn. Its person's messages, kiln's
own (news of its children, its goals) and its notes to itself stay where they are. A wait hands
them only to the turn it began in: one left running past it (the turn interrupted, a wait in the
background) hands nothing over, since the agent takes its queue a turn at a time. It hands over at
most 16 messages and 48 KiB of their words (the rest wait for the next wait, and one longer than
that on its own for the turn's end), each once, however many waits run. `hand_over: false` hands
nothing over; a program's waits in code mode don't unless it passes `hand_over: true`, as what a
program returns of the answer is its own. `kiln agents wait` prints them after where each agent
stands. A message an agent sends another says so in its id (`from-agent-<sender>-…`), which is how
a wait finds it.

So a wait is a change, not a read: a `POST`, written in the audit trail, and not marked read-only to
MCP clients. What it hands over lives only in its answer, and a client may run a read-only tool beside
others, again, or ahead of time and drop its answer; a wait's messages would be lost with it.
`agents.result` stays a read: all it changes is what its caller is told of what it has just read in
full (the line in its news, and the settle not told again).

### Who asks

| How it asks | Who kiln takes it for |
|---|---|
| kiln's MCP server on the server (`/api/mcp`) | the agent whose own MCP token it is: kiln gives every agent it starts on the server a token of its own (`kiln_mcp_session-<id>_…`, in a 0600 config file), which works only while kiln runs that agent |
| kiln's MCP server on a runner (`kiln reach mcp`) | the agent session the runner runs it for |
| `kiln spawn`, `kiln agents` on a machine | the agent whose command it is (the process is in the agent's group of processes, its harness is an ancestor of the process, or the process carries the agent's `KILN_AGENT` mark), else the person in the kiln shell it runs in |

An agent acts for whoever started it (the owner on a kiln nobody else signs in to): its calls
are audited as `agent:<id>[@<machine>] for <them>`, and its children act for them too. A person's
kiln shell acts for whoever opened the terminal, audited as `shell:<id>[@<machine>] for <them>`.

### From a kiln shell

Every harness kiln starts has `KILN_HARNESS_SOCK`, so these work in an agent's own commands as
in a kiln terminal (run by an agent, what they start are its children). It also has
`KILN_RUNNER_SOCK`, the socket of the runner process that started it, which alone knows it as an
agent and holds its key. These commands, its hooks and `kiln-git-sign` (its commit signatures and
its credential for kiln, docs/ci.md W27-R03) ask that one first, then the machine's socket: while
an upgrade's new build runs beside the old one, the old one serves `KILN_HARNESS_SOCK`, and the
sessions the new build starts are still known as agents there (what they start are their
children, their hooks hold them) and sign with their own keys. The runner's own socket answers
only the processes of its own sessions; anyone else is sent to the machine's.

| Command | |
|---|---|
| `kiln spawn codex "fix the tests" --on linux/amd64 [--cwd ~/src/app] [--mode untrusted] [--model …] [-n 2]` | start agents on another machine, picked as above |
| `kiln agents wait <id>… [--timeout 300] [--any] [--change] [--json]` | wait for agents anywhere, then print where each stands |
| `kiln agents result <id> [--json]` | its last reply, how its last turn ended, what it changed, its cost |

`kiln spawn` without `--on` starts agents on this machine, as before (run by an agent, through
the server, as its children); `kiln agents list|send|interrupt|stop` work on this machine's agents
(an agent sends to and stops only its own). The skill `kiln-agents` (docs: `kiln mcp`, skills)
says when to fan out, how to brief a child, how to wait and read results, how to keep children's
permissions low and how to merge their work.

## Workspaces of their own

An agent can work in **its own Slates volume** instead of the folder itself (Mission control's
composer → More options → Workspace → Own volume; `agents.start` with `workspace: "volume"`) (docs/agentic.md, W):

- **What it is.** On the agent's machine, before its harness starts, kiln lays an overlay volume
  over the folder's repository and mounts it under the runner's state directory
  (`workspaces/kiln-<id>`); the agent's folder is the same place in it. What it hasn't touched it
  reads from the folder as it is now; what it changes (its edits, its build's output, git's own
  writes) stays in the volume, in memory: the folder and other agents never see it. Several agents
  on one folder each get their own.
- **Where its work goes.** Up for review as a change request from the volume (**Open change
  request**, `agents.change`: everything that changed there), or back into the folder:
  **Write to disk…** shows what it would create, replace and delete (git's own directory stays
  out), and any file changed in the folder since the agent changed it, in which
  case nothing is written until they agree. Only a person writes it: an agent's `agents.land` with
  a plan is refused. Slates takes a person's go-ahead from kiln on macOS and Windows; not on Linux
  yet (Slates can't take a grant from outside its anchor's own session there), and the plan says so.
  Slates' command line leaves out one path per landing, so kiln leaves out `.git` and writes only
  what git sees changed: when the agent's build output or dependencies would be written too, the
  plan says to put the work up for review instead.
- **How long it lasts.** As long as the agent: ended for good, its volume goes with it. It outlives
  the runner's restarts and upgrades (Slates' anchor runs apart from the runner: its own systemd
  user unit on Linux), but not the machine's (Slates keeps volumes in memory). An agent in one
  doesn't move to another machine yet.
- **Where it can.** A machine with Slates installed (`slates` on `PATH`, or kiln's own under the
  runner's state directory) and a mount it can make: FUSE through `fusermount3` on Linux, NFS on
  loopback on macOS. `GET /api/harness/workspaces` says which machines can, and why each other
  can't. kiln keeps one Slates instance, `kiln`, per machine (two shards).

## From an Agentfile, in a microVM of its own

An agent can start in **a microVM of its own, made from an Agentfile** (kiln plan
01a1cd8e862e7169a074db3d5fa33f3d, step 31): Mission control's composer → More options →
Agentfile, which offers kiln's for its harness (`harness-claude` …) and each of yours. The choice
is kept in this browser per harness and per project (the folder it starts in, as typed; its home
and a shell's folder are none), so the next agent of that harness there starts from the same one;
None is a process on its machine, as agents start without one. Its start names it as the
microVM's image (`spec.isolation: {kind: "microvm", image}`, docs/api.md "Agentfiles"). It goes
only to a runner that says it starts microVMs (its facts' `virt.microvm`), and anywhere else it's
refused, never run as a process; where the machine picked would refuse it, the composer says so
under the row. Once such a start goes through, its harness runs as its image's HARNESS member, its
stdio the member's streams, and its session drives it through the same adapter as on the host: it
starts, is steered, interrupted, resumed and stopped as a process is (docs/isolation.md, step 17).
A runner's kiln starts no microVM itself yet: kiln holds Shards and Hex now (docs/shards.md), but it
doesn't yet start a harness through Hex (the plan's step 17), so until then such a start is refused
there, saying so (its facts' `isolation.why_not`).

**Agentfiles** (Agents → Agentfiles, `/agents/agentfiles`) keeps yours: each opened on its own page
in the code editor's Agentfile mode, checked by Shards as it's typed (`POST /api/agentfiles/check`,
what it finds marked on its lines and said under the editor), saved with a new version for each new
text (one Shards refuses isn't kept, and its words say why; nor is one that grants a way to kiln
itself, below), renamed (no new version), each version
read and any made current again (as a new version), and deleted with its versions after a question
asked in place. A new one is written in place on the list; kiln's are read there and copied as
yours, under a name that isn't kiln's, to change.

### What an isolated agent is

An isolated agent runs its harness in a Linux microVM of its own instead of as a process on its
machine. kiln drives it as it drives any agent: the same session, the same harness protocol, the
same controls. It reaches only what its Agentfile grants: no folder of the machine but the volumes
given to it, no network but the ports and hosts its grants name, nothing coming in.

- **Asking for one.** The composer's Agentfile choice (above); `agents.start` with `isolation:
  "microvm"`; or `POST /api/harness/sessions` with `spec.isolation` (docs/api.md, "Agentfiles").
- **What it starts is isolated too.** An agent an isolated agent starts runs in a microVM of its
  own as well. Asked for as a process on its machine (`isolation: "none"`), it's refused: "an
  isolated agent's agents are isolated too: it can't start one as a process on its machine".
- **Where it runs.** Only on a runner that says it starts microVMs (its facts' `virt.microvm`, the
  need `microvm`). Anywhere else it's refused, never run as a process.
- **Not yet.** No runner starts microVMs yet. kiln holds Shards and Hex at one Shards revision and
  can carry Shards' programs (docs/shards.md), but its releases don't carry them yet, and kiln
  doesn't yet start a harness through Hex. Until then every isolated start is refused, saying so.
  The rest of this section is what one gets once its start goes through.

### What kiln's Agentfiles give it

kiln ships an Agentfile for each harness it drives, `harness-<id>`. Read one on the Agentfiles page
(Agents → Agentfiles), or in kiln's repository at `.kiln/images/harness-<id>/`. Each one holds:

- **The publisher's own Linux build**, pinned by its version and the checksum its publisher
  publishes, for amd64 and arm64, and checked as kiln fetches it. kiln turns off a harness's own
  updater where it has one, so the pinned version is the one that runs. Gemini CLI, whose Linux
  build is an npm package, runs on a pinned Node.js of its own.
- **How kiln drives it**, in its `harness.json`: the program and the protocol kiln's adapter speaks
  (the harness table at the top of this page).
- **Its network, every part named.** TCP out to its harness's own hosts alone, each by name: its
  provider's API and its sign-in's token hosts (the table below). Only those names resolve, nothing
  comes in, and no other host, port or protocol goes out. kiln's MCP isn't among them: it comes over
  the link kiln drives the harness by, never the network, so no Agentfile grants anything for it. A harness that takes any provider (Goose, opencode, Pi) is granted
  every hosted provider its own build knows at a fixed public host. A cloud platform's model
  service (Bedrock, Vertex AI, Azure), a self-hosted or local server, and a provider whose address
  you give it are none of those: copy kiln's Agentfile into one of yours and add your provider's
  `--egress=HOST:PORT`. Each Agentfile's comments say what each host is for and where it comes
  from; kiln's step 18 record (`docs/research/s18-host-in-vm`) has the sources.
- **Your project at the path it has on your machine**, plugged in as the agent starts, so the same
  absolute paths work inside and out and kiln reads its changes where it always does. No volume
  holds it. A project under `/tmp` can't be plugged in: that's the microVM's own scratch.
- **Your volume as its whole HOME**, `/home/<harness>`, given to it at run time: where the harness
  keeps its settings, sign-in, history and sessions, and where what the programs it runs keep in a
  home lands (a `git config --global`, a tool's sign-in).
- **No credential.** An image is shared; your sign-in isn't (below).

| Harness | Agentfile | Your volume, its HOME | What it reaches, by name |
|---|---|---|---|
| Claude Code | `harness-claude` | `/home/claude` | `api.anthropic.com`, `claude.ai`, `platform.claude.com` |
| Codex | `harness-codex` | `/home/codex` (`CODEX_HOME` in it) | `api.openai.com`, `chatgpt.com`, `auth.openai.com` |
| Gemini CLI | `harness-gemini` | `/home/gemini` | `cloudcode-pa`, `generativelanguage`, `oauth2` and `www.googleapis.com` |
| opencode | `harness-opencode` | `/home/opencode` | each provider its build knows and their sign-ins, `opencode.ai`, `models.dev`, GitHub for its ripgrep (32 hosts) |
| Goose | `harness-goose` | `/home/goose` | each provider its build knows and their sign-ins (64 hosts) |
| Amp | `harness-amp` | `/home/amp` | `ampcode.com` |
| Devin | `harness-devin` | `/home/devin` | `server.codeium.com`, `api.devin.ai`, `unleash.codeium.com` |
| Antigravity CLI | `harness-antigravity` | `/home/antigravity` | `cloudcode-pa.googleapis.com`, `oauth2.googleapis.com` |
| Cursor CLI | `harness-cursor` | `/home/cursor` | `api2`, `api2direct`, `repo42`, `api5` and its six agent hosts `.cursor.sh` |
| Pi | `harness-pi` | `/home/pi` | each provider its build knows and their sign-ins, `pi.dev` (44 hosts) |

The harness sees its image read-only, so only your volume makes its files writable and lasting,
and with it as the whole HOME everything the harness and the programs it runs keep in a home lasts
from run to run, yours alone. Caches that follow XDG's rule stay on the microVM's scratch, as they
did before, and go with it (all but Claude Code's, Gemini CLI's and opencode's). No volume goes
under `/tmp`, which is each harness's scratch. kiln keeps your volumes on the machine that runs
the microVM, one for each person and harness (`<kiln's state>/homes/<you>/<harness>/`, readable by
kiln's user alone), and reads your sessions there to show and resume them.

Its base is `buildpack-deps:trixie-scm`, pinned by digest: git, an ssh client, curl and CA
certificates, for both platforms. For your project's own toolchain, copy kiln's Agentfile into one of
yours and set its `ARG BASE` to your project's image, or add what your project needs to it. Claude
Code's Agentfile also builds on a musl base (`CLAUDE_LIBC=musl`). What each harness needs of its base
is in its notes (below, "Notes per harness") and its pin's records (`docs/research/s19-claude-agentfile`
to `s28-pi-agentfile`).

### Signing in, and your credentials

- **None in an image.** kiln's Agentfiles carry no credential, and neither should yours: whoever can
  read an image can read what's in it. A key or a token belongs to a run, given to its harness alone.
- **Yours stay in your volume.** What a harness keeps of its sign-in it keeps in your volume, which
  no other person's agent is given.
- **Signing in.** The harness's own sign-in, as the account card runs it on a machine today
  ("Signing harnesses in and out"), will run inside its microVM, on its own terminal. Not yet: kiln
  has no terminal into a microVM yet.
- **Where credentials are going.** No real value inside the microVM at all: kiln keeps it (in its
  secret store, Veil's later), the harness holds a placeholder, and the microVM's own egress puts the
  real value in only for the hosts that credential is granted to, never for any other. You sign in
  once, the token's refresh happens outside the microVM, and a run's credentials are revoked as it
  ends. A harness that pins its certificates, or checks its token itself, gets its value in a file
  on the microVM's scratch instead, revoked as the run ends, each such case listed. Not yet built.

### What an isolated agent can't do

- **Use your machine's own tools.** The microVM is Linux. Xcode, the macOS SDKs and simulators, MSVC
  and the Windows SDK aren't in it and can't be. Work that needs them runs on the machine itself: an
  agent started as a process (no isolation). Where its project's tracked files say it builds with
  them (an Xcode project or workspace, a Visual C++ project), kiln refuses to start it isolated,
  naming the tools and the system each runs on. An isolated agent can't start a process (above):
  start it from your own session, or from an agent that isn't isolated.
- **Reach what its Agentfile doesn't grant.** No other folder, port, host or device of the machine.
  kiln's own MCP takes no grant at all: it comes over the encrypted link kiln drives the harness
  by, as that link's own MCP lane (Shards D254), on every OS kiln runs on. Inside the microVM each
  harness names the lane's bridge, `/run/shards/mcp`, as a stdio server in its own form (Claude
  Code's `--mcp-config`, Codex's `-c mcp_servers.kiln.*`, an ACP agent's `mcpServers`, one fixed
  entry in Antigravity's own `mcp_config.json`), and kiln answers it itself, with no token of
  kiln's in the microVM: the lane says which session asks. So no Agentfile grants anything for it,
  and kiln refuses to keep one that grants a way to kiln itself, at its line, typed (`ReachesKiln`:
  "kiln's MCP rides the agent-control lane (D254)"). That covers a destination or a remote MCP
  server that is the machine its runner is (`host.docker.internal`; a loopback, link-local or
  unspecified address; a network's own gateway) or kiln's server (the hosts its configuration
  names). Other remote MCP servers, a GitHub MCP for one, stay yours to grant by name. The runner's
  end of the lane is still to come.
- **Use a tool its image doesn't have.** The project's toolchain comes from the image: build your
  own Agentfile on your project's image (above).
- **Move to another machine, or switch to another harness.** Not yet: it stays where it started, in
  the harness it started with, and says so when asked. To go on elsewhere or in another harness, end
  it and start a new one there.
- **Not yet**, as well as the start itself: a terminal into the microVM, its sign-in and exec
  there; git signing and a git host's credentials by the microVM's own key; kiln's MCP, over that
  lane; and the commands it asks kiln to run on other machines,
  which run in microVMs too or not at all, never on a bare host.

### Each feature, in a microVM of its own

What this page describes, once an isolated agent's start goes through:

| Feature | In a microVM of its own |
|---|---|
| Driving it: sending, steering, interrupting, stopping, models, modes, commands, questions, state, usage limits | Works: kiln speaks the same harness protocol over the microVM's stdio. |
| Said at once when it can't sign in or reach its provider (State's `trouble`) | Works, as for every agent. |
| The Changes tab, Commits, Compare, whose a file is, a subagent's edits, change requests | Works: the project is at its own path inside, so kiln reads its diffs, HEAD and checkpoints where it always does. |
| Continuing a past session | Works: kiln reads its transcript from your volume on the machine. |
| Its harness's settings, sign-in and memories | Kept in your volume, yours alone; no credential leaves it. |
| Agents that run agents, and kiln's other tools | Not yet: through kiln's MCP, which comes over the link kiln drives the harness by (its own MCP lane, through the bridge `/run/shards/mcp` each harness names), never the network, on every OS; what it starts is isolated too. The runner's end of the lane is still to come. Pi takes no MCP server from kiln. |
| Skills kiln installs, and kiln's own | Its plugin folders are plugged in read-only at their own paths as it starts. |
| Files in messages | A file in your project or in its home shows; one on the microVM's scratch (`/tmp`) can't, as it's nowhere on the machine. |
| What it leaves running | Inside its microVM, and ends with it. |
| Signing it in and out; a terminal beside it; exec | Not yet: they run on the microVM's own terminal (plan steps 6 and 11). |
| git signing and a git host's credentials | Not yet: by the microVM's own key (step 11), never a socket of the machine. |
| kiln's variables in its environment (`KILN_*`, above) | `KILN_AGENT`, the session's mark, is set as on the machine. The machine's sockets (`KILN_HARNESS_SOCK`, `KILN_RUNNER_SOCK`, `KILN_REACH_SOCK`) and `kiln reach mcp`'s (`KILN_REACH_ON`, `KILN_SESSION`) aren't: a microVM can't reach the machine's sockets, and kiln's MCP does their work. Nothing else of the machine's environment goes in. |
| `kiln spawn` and `kiln agents` from a kiln shell | Can't: they use the machine's own sockets, which a microVM doesn't have; kiln's MCP does their work. |
| Taking over a found session | Can't: a microVM's harness is never found on the machine. |
| Moving it to another machine | Not yet: your volume stays on the machine it ran on. |
| The machine's own tools (Xcode, MSVC) | Can't: refused, saying which and why (above). |

## The Changes tab

An agent's Changes tab shows what it changed, its change request above (see "Change requests from
agents"). Its head says what's shown, a row each, each choice in a menu:

- **Session**: the files it changed since it began, each against how it was before.
- **Commits**: its branch's commits, and what the picked ones change together (see "Commits").
- **Compare**: what changes from one point to another (see "Compare").
- **Scope**: Project at first, then User, then All (see "Who changed a file, and whose it is"). The
  browser keeps the choice.
- **By**: everyone, the agent itself, or one of its harness's subagents, each with how many of the
  files at that scope it changed. Narrowed to one, a file someone else changed too shows only their
  part and says who else changed it (see "One subagent's own edits"). Under their own heading, the
  agents it started through kiln (`agents.start`, its goals' and plan steps'): each a session of its
  own, maybe on another machine, so picking one shows that agent's own changes, read from its own
  session, at the same scope.

The head's last row carries what's shown in all (`+added −removed`), and Sort (by path, latest change,
most changed, new, changed or deleted, or who changed it) sits at its right. Its files are a flat
list, each saying on the line under it who changed it, and which worktree it's in when that isn't its
folder's; the person's files elsewhere and the agents' own are under bands of their own. A file picked
shows its diff alone; picked again, every file's. The tab's count is the files its scope shows.

## Commits

The Changes tab's Commits lists its branch's commits since its trunk (its remote's default branch,
else `main` or `master`), newest first along its first parents, at most 200; a branch with none of
its own since lists its latest 50. What isn't committed yet (as `git add -A` would take it, new files
included) is on top. Its machine answers, in the working tree its work is in (`agents.commits`):

- **This session's.** A session keeps the commit its folder had checked out as it began (its state's
  `started_head`); the commits after it are its own, under a band of their own, and picked at first.
  A session from before kiln kept it, or whose branch was rewritten since, tells its own by when they
  were made.
- **What's shown.** The picked commits' change, together. A run of them is one diff, from the first
  one's parent to the last. With gaps, each picked commit is applied in order onto the first one's
  parent in a scratch index beside the folder (its branch, index and working tree untouched); one
  that doesn't go without a commit left out says which.
- **Picking.** As Mission control picks agents: a tap shows one commit alone; press and hold picks
  it, or unpicks it if it was picked, and dragging on takes each commit passed the same way, the
  list not scrolling under the finger; with a mouse, Shift picks a run and ⌘/Ctrl adds or takes away
  one. The This session band's ring picks all of the session's, with a dash when only some are.
- **On** picks the branch: the one checked out, or another of its repository's (the latest dozen).

A runner answers only with `CAP_WORKSPACE_COMMITS` (docs/wire.md). Without it, or in a folder that
isn't a repository, Commits is turned off, saying why.

## Compare

The Changes tab's Compare shows what changes from one point to another, From and To each picked in
place from a list that filters as you type:

- **This session:** Now (the folder as it is, untracked files too, as `git add -A` would take it,
  in a scratch index beside the folder), Before it began (the commit it began at, `started_head`),
  and each agent's version: the agent itself or one of its subagents, its files as it left them
  (see "One subagent's own edits") over how the folder was as the session began. Only its machine
  knows how they left them, so the server's own agents' versions are refused, saying so.
- **Elsewhere:** its repository's branches (its own, then its remote's), tags and latest commits
  (`agents.refs`).
- **Between commits**, "Only what To adds since they split" (git's three-dot diff) is on at first;
  it sits in the head's menu with Swap. Two agents' versions compare plainly, From → To, each
  file's line naming who of them changed it, or saying both did.
- **Large compares** (past 300 files or 20,000 lines) come as counts first, and a file's lines once
  it's opened (`agents.compare` with `path`).

From is its trunk at first, To is Now. A runner answers only with `CAP_WORKSPACE_COMPARE`
(docs/wire.md); without it, or in a folder that isn't a repository, Compare is turned off, saying
why.

## Who changed a file, and whose it is

Each file of a session's changeset (`changes {files}`, `agents.result`'s `changes.files`) says who
changed it and whose it is, for the Changes tab's By and Scope:

- **`by`**: who changed it, in the order they first did. `main` is the agent itself; anything else is
  a subagent run of its harness, by the id of the call that spawned it (as its events' `parent` says):
  Claude Code's Agent tool, Codex's subagent threads. Every harness's subagents' edits count, in the
  session's changeset and so in what a change request takes.
- **`place`**: whose it is.
  - `project`: in a folder the session has worked in (where it started, and each it moved to), or in a
    working tree of one of their repositories, wherever that is: a worktree under `~/.cache` is. The
    home folder, or a folder above it, is no project's: a session started there has the repositories it
    works in.
  - `agent`: an agent's own. In a harness's home (`~/.claude` or `CLAUDE_CONFIG_DIR`, `~/.codex` or
    `CODEX_HOME`, `~/.gemini`, Antigravity's, Goose's), but for what a person keeps there to tell it what
    to do; in kiln's own folders (a runner's state); in the machine's cache (`XDG_CACHE_HOME`, else
    `~/.cache`, macOS's `~/Library/Caches`) and temporary folders.
  - `user`: anything else it changed for the person: another repository, their home folder's files,
    and what they keep in a harness's home: its instructions (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`),
    its settings (`settings.json`, `config.toml`...) and its `skills`, `commands`, `agents`, `prompts`.

A file with neither (from a runner older than these) is read as this project's. The state's
`files_changed`, `lines_added` and `lines_removed` (the tab's count, Mission control's) count this
project's files only; the rest stay in the changeset, for the scope that shows them.

## One subagent's own edits

Narrowed to one who changed its files (By), the Changes tab shows each file as they changed it alone.
A file only they changed shows the session's change to it. A file someone else changed too shows
their part, and the line under it names who else changed it:

- **What's kept.** The session's core keeps, for each file, who changed it in turn, and how the file
  was as each turn began: the session's baseline for the first, then how the next one found it, as
  its harness said before its edit (Claude Code's `originalFile`; Codex's patch, undone from the
  file by its change job; an ACP agent's old text). One's part is each of their turns, from how they
  found the file to how the next one did, or to how it is now for the latest. Taken in turns, one
  after another, that's their edits alone; one who came back to it after another shows each of their
  turns, edit by edit. An in-place upgrade keeps it all (`Frozen`).
- **How it's asked for.** The panel asks on the session's stream (`own_edits {client, by}`) and the
  session answers that client alone (`reply`), as file suggestions are answered, so the stream itself
  doesn't grow: `{by, files: [{path, tree, kept: true, edits: [one FileChange a turn]}]}`, worked out
  off the event loop. Sessions whose features have `own_edits` answer, whatever their harness.
- **The cap.** The session keeps 64 MiB of files as turns began at them at most (the baselines
  aside), and 256 turns a file. Past it, or where its harness didn't say how it found a file (a shell
  command changed it), or where the runner started again since, a file's answer is `kept: false` with
  why (`cap`, `unknown`): the panel then shows their edits one by one from the conversation's events,
  and says so under them. It does the same for a runner that doesn't answer, saying that.

## Change requests from agents

An agent's work goes up for review from its Changes tab (**Open PR**), or by the agent
itself (`agents.change`; an agent may open one for its own work and for the agents it started)
(docs/agentic.md, C1–C4):

- **What goes:** the files the agent changed (its Changes tab's list), as they are in its folder,
  committed on the folder's last commit. The commit is made beside the folder, on a scratch index,
  so the folder's branch, index and working tree are left exactly as they were; a person's own
  edits in the same folder stay theirs, and git's ignore rules apply. Its message is the title,
  the body, and a `Kiln-Agent: <name> (<harness>, <id>)` line; its author is the machine's own
  git identity.
- **In a worktree:** an agent that goes into a worktree of its folder's repository (`git worktree
  add`, Claude Code's `EnterWorktree`) works there: each file it changes is placed in the working
  tree it's in, by its path there, and its work is where its latest change is. Its change request
  takes that working tree's files, committed on its last commit (the worktree's branch, not the
  checkout's), its trunk the checkout's branch when the remote names none; so do its facts, a plan
  step's commits, and the Changes tab, where a file in another working tree than the agent's
  folder is shown where it is. Claude Code's agent moves into a worktree as it enters it, and back
  as it leaves.
- **Where it goes:** the branch `agents/<what it's about>-<the start of its id>`.
  - A repository kiln hosts: pushed to kiln's git as a change request
    (`refs/for/<trunk>/branch/agents/…`), with an agent token minted for that push alone
    (`agent:<id>`, acting for the agent's person, limited to that repository, revoked once the
    push is done). The change request's author is the agent, for its person.
  - A repository kiln follows on GitHub, GitLab or Bitbucket: the branch is pushed to the
    folder's remote with the machine's own credentials, as a person there would push it, and
    the pull or merge request is opened through the provider when kiln has an integration with
    it; otherwise the change's link opens one.
  - A folder whose repository kiln doesn't know: the branch goes to its `origin`.
- **Each push is a version.** Pushing again (**Push update**, or `agents.change` again)
  makes the commit anew from the agent's files as they are then.
- **Drafts** (docs/ci.md W26-R18). `draft` opens it a draft wherever it goes: in kiln's own review
  (the push's `draft` option), as a GitHub pull request (its flag), a GitLab merge request
  ("Draft: " in its title) or a Bitbucket pull request (its flag). Its new versions stay a draft.
  `ready` marks it ready for review: with new work, along with the new version; with no edit since
  its last version, nothing is pushed and the draft is marked ready where it is. On a change that
  isn't a draft, `ready` says it's ready already. A person marks an agent's draft ready from Ship
  (**Ready** on its row), as the agent's person or one who can land on the repository.
- **Where it stands**, in a word above its files: **Pending** (not up for review yet), **Pushed**
  (its branch is pushed, its pull request not opened yet), **Checking**, **Failed** or **Fixing**
  (its checks failed; with **Auto-Fix** on, the agent fixes them), **Changes requested**,
  **In review**, **Approved**, then **Merged** or **Closed**. While it's open, failing checks come
  first, then changes asked for, then checks still running. On a card 400px wide or more it's one
  line, what to do at its end; on a narrower one (a phone), a line each.
- **What comes back** to the agent, as a message after its turn, while it runs: a comment on
  its change (with the file, the lines, the version and a link), a review asking for changes,
  CI that failed on its newest version (the failed legs and the first one's failure excerpt;
  only when **Auto-Fix** is on, which it is unless you turn it off when
  opening it), and the land queue taking it out (with the reason). Approvals and landing only
  update where it stands. An agent that has ended is sent nothing.
- **Agents' feedback** (docs/ci.md W26-R18): another agent's comment on its change reaches it only
  where the repository lets agents fix code from agents' feedback (the switch in its Review section
  in Repos, off until turned on), saying why kiln sent it, and at most three times in a thread until
  a person speaks there. A new version that names the threads it fixes (`threads`) is the agent's
  fix of them, and each thread says which version fixed it. Ship's **Hand off** sends it every open
  thread at once, each with its id, place and version.
- **A plan run's steps** (`plans.run` with `mode: "agents"`, steps in order; docs/agentic.md P4)
  go up by themselves, as each step is met: the files the step's agent changed, as one commit on
  the step before's. On a repository kiln hosts that is one diff per step (`D12`), pushed to
  `refs/for/<trunk>` under the run's topic (`plan-<run>`) as that step's agent, and kiln links
  them as a stack; elsewhere, a branch per step (`agents/<plan>-<run>-<step>`), each pull request
  onto the one before. A step that changed nothing has no diff; one whose commit couldn't be made
  hands its files on to the next step's. What review and CI say to a step's agent waits while the
  run goes on (the next step works in the same folder) and reaches it when the run ends. Then
  `agents.change` from that agent makes a new version of its own diff: what it changed since,
  taken against the stack's top, goes into its step's commit, and the steps above are made again
  on it. When its change touches what a step above changed too, it's refused, saying so. Before
  the run ends, `agents.change` from a step's agent is refused, saying why. The run's page shows
  each step's diff and where it stands.

A `Change` is `{agent, repo?, provider, change?, url, branch, trunk, sha, version, files,
title, state, review?, draft?, checks?, fix_checks, opened_at, updated_at, stack?}`: `provider` is `kiln`,
`github`, `gitlab`, `bitbucket` or `git`; `change` is `CR12`, `D12` or `#34` (none until a pull
request is opened); `state` is `open`, `landed` or `closed`; `draft` says whether it's a draft,
read again from kiln's own review or the host's pull request as kiln last read it; `stack`
(`{run, step}`) says it is a step of a plan run's stack. Mission Control shows each agent's change
on its row, with **Review** on a finished agent's open one; the Changes tab shows where it
stands, linked, and pushes new versions. Where kiln has W14's review operations, a comment's
message tells the agent how to answer in its thread (`ci.diffs.reply` with the thread's id);
otherwise it answers a comment with a new version.

## Attached agents

An agent attached to what it looks after wakes when something happens there (docs/ci.md W20-R08 to
R10; docs/agentic.md K2 and R4). **Watched by**, on a repository's panel (its workflows' and
environments' too) and on a runner's details (every runner's too), lists them and attaches one;
`attachments.*` and `incidents.*` do the same for agents and code mode. An attachment names:

- **What it looks after** (`on`, written as access scopes are): `repo:acme/app`,
  `workflow:acme/app:.github/workflows/ci.yml`, `environment:acme/app:production`, `pool:gpu`,
  `runner:metal-1`, or `runners` for every runner; for code, the `branches` whose runs wake it
  (none: any).
- **Its agent**: a harness kiln starts for each incident (`harness: {kind, on?, cwd?, model?,
  effort?, mode?}`, `on` and `cwd` the machine and the checkout it works in), or a live agent it
  tells instead (`session`).
- **What wakes it** (`wake`): `failure` (a run of what it looks after failed, or a deployment to its
  environment), `flaky` (a run had flaky tests), `ejected` (the land queue took a change out),
  `stuck` (a job waited for a runner past `limits.stuck_minutes`), `slow` (a green run's critical
  path took longer than the median of its workflow's last green runs on its branch by more than
  `limits.slow_percent`), `unroutable` (a job no runner meets what it asks for, a minute on),
  `degraded` (an app kiln CD deploys to its environment turned degraded), `mention` (`@name` in a
  comment), `review` (a push asks it to review a change, `reviewer=agent:<name>`: it reviews
  read-only, its words signed as itself, and joins the change's turn, a draft's once it's marked
  ready; docs/ci.md W26-R18), `schedule` (`schedule`, five-field cron, UTC); on runners, `offline` (gone two
  minutes), `upgrade` (an upgrade failed) and `check` (kiln's checks of the machine, every half
  hour, found something failing).
- **What it may do** (`can`, W20-R09): it always reads what it looks after; beyond that `rerun`,
  `cancel`, `diagnose`, `fix`, `edit-pipelines`, `quarantine`, `comment`, `experiment`,
  `manage-runners`, `deploy`, `land`. Its agent acts for whoever attached it (who needs
  `agents.attach` there), and its calls to kiln are held to these capabilities on what it looks
  after: kiln refuses it the rest.
- **Its limits** (`limits`): incidents a day (`sessions_per_day`, 10), tokens a day
  (`tokens_per_day`, 5,000,000, its agents' sessions together), runner minutes a day
  (`runner_minutes_per_day`, 300, the runs its agents asked for and the attempts they reran),
  reruns per failure (`reruns_per_failure`, 2), when a job is stuck (`stuck_minutes`, 15), when a
  run is slow (`slow_percent`, 25); **its playbook**, and **who reviews** what it opens.

What wakes it opens an **incident**, or adds to the open one with the same signature (what failed
and how, with numbers, paths and hashes taken out), so one cause never starts two agents. Its
agent is given the bundle: what happened, each failed job's failing step with the log around the
error, the last green run, the same thing before and how it went, what it may do, its playbook,
and how to report. It says what it found with `incidents.report`: `class` (flaky,
infrastructure, regression, pipeline, dependency, other), `summary`, `links`, `state` (reported,
resolved; dismissing is a person's). A failure that keeps coming back from reruns past its limit
tells the agent to stop rerunning. An incident waits for a person instead (`waiting`, saying why)
while its attachment is paused, past one of its limits for the day, or when its agent couldn't
start.
**Pause** stops one; `attachments.pause` with a repository as `on` stops everything attached to it,
its workflows' and environments' too. Mission Control lists the agents an attachment woke with
the other agents; their questions come to Needs you and to notifications as any agent's do. A runner doctor's
open incident shows in the runner's row in System → Updates.

| Operation | Route | |
|---|---|---|
| `attachments.list` | `GET /api/attachments` | `on?` → `{attachments: [{attachment, open, today, latest}]}` |
| `attachments.get` | `GET /api/attachments/:id` | → `{attachment, grants, incidents}` |
| `attachments.save` | `POST /api/attachments`, `PUT /api/attachments/:id` | an attachment → `{attachment, grants}` |
| `attachments.remove` | `DELETE /api/attachments/:id` | its incidents stay |
| `attachments.pause` | `POST /api/attachments/pause` | `{attachment?, on?, paused}` |
| `incidents.list` | `GET /api/incidents` | `state?`, `attachment?`, `limit?` → `{incidents}`, newest first |
| `incidents.get` | `GET /api/incidents/:id` | the bundle, and each time it happened |
| `incidents.report` | `POST /api/incidents/:id/report` | `{class?, summary?, links?, state?}`: its agent, or a person who may attach there |

A run that woke an attached agent shows the incident on its page: who looks after it, where it
stands, what its agent found and opened, and a link to the agent. Under **Watched by** each one
attached is a chip saying how it stands in a word (watching, on it, waiting for you, paused); a
chip opens its line (what wakes it, its agent, its latest incident; Pause, Remove), and **Attach**
asks three things: its name, its agent and what wakes it. The rest is kiln's defaults (ten
incidents, 5,000,000 tokens and 300 runner minutes a day; it may read), or what trunk's
`.kiln/agents.yaml` says.

**In the repository.** A repository can keep its attachments in trunk's `.kiln/agents.yaml`, read
like the rest of its policy (from trunk, never from a change, within a minute of landing):

```yaml
agents:
  ci-doctor:
    harness: claude              # claude, codex, acp:<name>
    model: opus
    effort: high
    machine: linux/amd64         # where its agents run (a machine, or what it needs)
    cwd: ~/src/app               # their folder there
    attach: { pipelines: [ci], branches: [main] }   # or environments: [production]
    wake: [failure, flaky, stuck, ejected]          # or { schedule: "0 6 * * 1" }
    can: [read, rerun, diagnose, fix, comment, quarantine]
    limits: { per-day: { sessions: 10, tokens: 5M, runner-minutes: 300 }, reruns-per-failure: 2 }
    instructions: .kiln/agents/ci-doctor.md         # its playbook: a file on trunk, or the words
    reviewers: [ada]
```

Each looks after the repository, narrowed to its pipelines, environments and branches. It acts for
kiln's owner, held to its capabilities there, and is changed by a change to that file. Kiln keeps
only its pause switch. An agent the file names but can't be (an unknown harness, a wake that isn't
one) is left out, and the server's log says why. `@name` in a comment calls a repository's own
attachment only on that repository's diffs.

Not yet: a workspace of its own per incident (its agent works in the folder named; workspaces wait
on Slates).

## Commands

Every command is JSON with a `type`. Those that answer one client carry `client` (any string the
console picks) and are answered with a `reply` update to that client.

Each command is answered: on the session's socket, sent under an id (`{"t": "cmd", "id", "c"}`),
with `ack` or `nack`, taken once however often it's sent; over the API, as its status. What waits
for an agent's machine while it's away and what's refused at once, how each answer comes, and what
a reconnect or a restart does to commands on their way: docs/wire.md ("Commands", "The UI's
sockets").

| type | fields | answer |
|---|---|---|
| `send` | `id, text, mode: "queue"\|"steer"\|"now", attachments?, overrides?: {model?, effort?}, task?` | `state.delivery[id]`: queued → sent → started → completed / cancelled; with `task`, to that subagent ("Messages to its subagents") |
| `queue_edit` / `queue_remove` / `queue_move` / `queue_send_now` | `id` (+ `text` / `index`) | |
| `queue_steer` | `id` | the queued message goes into the running turn as steering, without interrupting it (sent as the next turn when none runs); feature `"queue_steer"` (Claude Code, Codex, Devin when advertised) |
| `approve` | `id, decision: "allow"\|"always"\|"deny", message?, always?, choices?` (or the older `answers?`) | see "Questions" below |
| `interrupt`, `restart`, `snapshot` | | |
| `set_mode` / `set_model` / `set_effort` | `mode` / `model` / `effort` | `state.mode` / `state.model` / `state.effort` |
| `set_speed` | `speed`: `""` (standard) or one of `meta.speeds` (the model's faster speeds, slowest first) | `state.speed`, from the next turn when one is under way; `state.speed_blocked` says why it can't go faster (its account), `state.speed_cooldown` that fast mode's own rate limit is cooling down; feature `"set_speed"` (Claude Code's fast mode, Codex's service tiers, Devin's `speed` option) |
| `stop_task` | `task` | the task's `status` (stopped) |
| `stop_tasks` | | every live task stopped |
| `remove_task` | `task` | a subagent of its harness's own that ended, and those under it that ended, gone from `tasks` (and its `subagents`); one still going stays. Feature `"remove_task"` |
| `suggest` | `client, query` | `{suggestions: [{path}], cwd}` |
| `control` | `id, value?` (a `meta.controls` entry, or one a catalog route names) | those with `client`: `{title, text, ...}` (below) |
| `mcp_enable` | `name, enabled` | `state.mcp` |
| `mcp_reconnect` / `mcp_refresh` | `name` / — | `state.mcp` |
| `mcp_auth` | `name, client, redirect?` | `{url, callback, redirect}` to open, or `{ok, message}`; send it again with `redirect: <the address the sign-in ended on>` to finish a sign-in from another device |
| `mcp_add` | `name, scope?, transport?, command?, args?, env?, url?, headers?, client?` | `{ok, message \| error}` |
| `mcp_remove` | `name, scope?, client?` | `{ok, message \| error}` |
| `skill_view` | `name, client` | `{content, path}`, `{content, builtin: true, source}` (no file), or `{error}` |
| `skill_run` | `name, args?, id?, mode?` | delivered like `send` |
| `skills_reload` | | `state.skills` |
| `resume` | | ends a pause now (`state.pause`): the turn it cut off goes on, then the queue |
| `auto_resume` | `on` | whether a pause ends by itself at its limit's reset (`state.pause.auto`, and `state.manual_resume` for later pauses) |
| `pause_at` | `percent` (null: off) | pause once a limit is this full, between turns or at the next step of one under way (`state.pause_at`) |
| `max_subagents` | `count` (null: no limit of its own) | the most subagents it has at work at once (`state.max_subagents`, Settings → Subagents): its harness's own and the agents it starts through kiln, one pool whatever each does. Claude Code's Agent tool asks kiln first (a PreToolUse hook kiln registers at `initialize`, whatever its permissions), counting those running and those let start that haven't yet, and past the limit is told to wait for one to finish; Codex takes it as its own `agents.max_concurrent_threads_per_session` when it next starts; kiln refuses `agents.start` past it, counting the agents it started that are at work (working, waiting on a person, or held to a goal still to go on a stack that runs) and its harness's own still going, so one idle or ended holds no place; a plan's steps side by side go to subagents within it, on its goal stack (docs/goals.md, "A plan's steps side by side") or in a run with an agent per step, the rest waiting for room; and a plan run on it as one message or a step a turn tells it so |
| `move_on_limit` | `on, to?, kind?, model?` | at a limit, go on on another machine with room (`state.move_on_limit`): `to` (`agent:<name>`) that machine alone, `kind` and `model` that harness and model there (`state.move_plan`) |
| `limit_hold` | `window?, label?, until?, said?, account?` | the server's: another session on `account` reached a usage limit, so this one pauses until it resets too, if it's signed in to that account and its own usage, asked then, says the limit is its own as well |

### Questions

An approval with `tool: "question"` carries `questions`: `id` (the harness's own, Codex),
`question`, `header`, `multi` (several may be chosen), `other` (free text is accepted; always
for Claude Code), `secret` (mask what is typed; Codex), `options: [{label, description,
preview}]` (`preview`: what to show while the option is focused, Claude Code). Answer with
`approve {id, decision: "allow", choices: [{question, id?, selected: [labels], other?,
note?}]}`, one per question: labels with commas, free text and notes reach the harness as they
are.

- Claude Code (AskUserQuestion): `answers` maps each question to the labels chosen and the free
  text, joined with ", " (the tool's own format); a note goes in `annotations[question].notes`,
  with the chosen option's `preview`, as its own dialog does.
- Codex (request_user_input): each label chosen and the free text are one answer each; a note
  rides as `user_note: …`, as Codex's terminal sends it. Codex asks only in its Plan mode.
- The older `answers` (question → one string) still works; each string is one choice.

## The command catalog (`meta.commands`)

Every slash command the harness has, and what running it from kiln does. Known from the start
(kiln's own entries) and completed when the harness lists its commands: Claude Code at
initialize, then at init (which comes with the first turn); Codex with its skills; an ACP agent
with `available_commands_update`. Sorted by group, then name; one entry per name.

| field | |
|---|---|
| `name`, `description`, `hint` | `hint` is the placeholder for its words ("<name>", "[instructions]") |
| `group` | "Session", "Model & mode", "Context", "Tools & MCP", "Skills", "Plugins", "Kiln" (kiln's own), in that order |
| `args` | "none", "optional", "required" |
| `aliases` | other names it runs under; never another entry's name, so a typed name matches a name before an alias |
| `route` | what to do with it, by `kind`: |

| `route.kind` | fields | the console |
|---|---|---|
| `prompt` | `text?`, `skill?` | sends a message: `text` with `{args}` replaced when there is one, else `/name words`; with `skill: true`, `skill_run {name, args}` |
| `control` | `command` | sends `command` (a kiln command) with `{args}` (the words typed after the name) and `{client}` (its client id) replaced in its strings. One that carries `{client}` answers that console alone with `reply {client, value}`: `{title, text, format?: "markdown", filename?, path?}`, or `{error}` |
| `view` | `view` | opens a kiln panel: `model`, `effort`, `permissions` (those three: that run setting in the head), `settings`, `mcp`, `skills`, `plugins`, `hooks`, `tasks`, `changes`, `usage`, `status`, `context`, `files`, `new_agent`, `terminal` (its shell), `screen` (its harness's own screen) |
| `unsupported` | `reason` | sends nothing; says why |
| `terminal` | `command` | opens the harness's own screen beside the agent, on `command` (see "Terminals beside an agent") |

kiln changes nothing about the account itself (sign in or out goes through the account card;
plan upgrades, privacy settings and integrations open the harness's own screen, where you do them;
feedback that uploads the conversation, and handing this session to the cloud, are `unsupported`).

- **Claude Code**: the CLI's own list (what it runs without its terminal: its commands, bundled
  and user skills, plugins' and MCP servers' commands), sent as text, except where kiln does the
  job itself: `/compact`, `/clear` (with its aliases `/reset`, `/new`), `/rename`, `/model`,
  `/effort`, `/context`, `/mcp`, `/usage`, `/reload-skills`, `/reload-plugins` (a control
  request: the CLI binds the command to its terminal). `/clear` starts the conversation over: once
  Claude Code says it has (`conversation_reset`, then an init under its new session id), so does
  the timeline (docs/conversations.md, "A clear"). Then the commands of its terminal
  interface: those kiln runs with the CLI's control requests (`/btw` a side question, its answer
  kept out of the conversation; `/cost`; `/export`; `/plan`; `/permission-rules` the rules in force
  and their sources; `/memory` the instruction files and memory folders; `/git-diff` the
  workspace's diff file by file), `/branch` (the conversation continued in another agent), kiln's
  panels (`/status`, `/diff`, `/tasks`, `/hooks`, `/skills`, `/plugin`, `/workflows`). Claude
  Code's own screen, beside the agent: `/permissions` (its editor of allow, ask and deny rules),
  the commands bound to the CLI's terminal (init's `terminal_slash_commands`: `/doctor`, `/color`,
  `/focus`), its terminal's settings (`/theme`, `/tui`, `/terminal-setup` …), account settings and
  setups (`/upgrade`, `/privacy-settings`, `/setup-bedrock`, `/install-github-app`, `/import` …) and
  cloud setups and reviews (`/web-setup`, `/remote-env`, `/ultrareview`, `/autofix-pr` …). The rest
  (handing this session elsewhere, `/feedback`, `/voice`, `/ide`) are unsupported with why. When
  rows share a name, `/name` runs the one marked built-in, as in the CLI.
- **Codex**: its terminal interface's commands (its app-server runs none): panels (`/model`,
  `/approvals`, `/permissions`, `/diff`, `/mention`, `/status`, `/usage`, `/mcp`, `/ps`,
  `/agent`, `/hooks`, `/skills`, `/plugins`; `/new` and `/clear` start another agent), controls
  (`/review`, `/compact`, `/rename`, `/goal`, `/fork`, `/undo`, `/export`, `/rollout`, `/pwd`),
  `/init` (Codex's own prompt); in Codex's own screen, beside the agent (typed there): `/memory`,
  `/apps`, `/experimental`, `/debug-config`, `/theme`, `/statusline`, `/title`, `/raw`, `/vim`;
  the rest unsupported. Its skills join; one named like a command
  runs from the skills panel. `/goal <objective>` sets the goal and Codex starts working toward
  it, turn after turn, until it is done; `/goal pause`, `/goal resume`, `/goal clear` as in
  Codex; `/goal` alone shows it with its status and the tokens used.
- **ACP agents**: the agent's own commands, sent as text (one named like kiln's wins), and
  kiln's: `/export` (the timeline as Markdown), `/status`, `/model`, `/diff`, `/restart`, `/new`.
- Every harness: `/stop-tasks` (`stop_tasks`), `/restart`, `/terminal` (`/shell`), `/screen` and
  `/goals` (its goal stack, docs/goals.md) in "Kiln".

`control` ids the catalog uses: Claude Code `side_question {text, client}`, `session_cost`,
`export`, `plan`, `permission_rules`, `memory`, `workspace_diff` (each `{client}`),
`reload_plugins`, `fork`; Codex `goal {text?, client}`, `fork`, `undo`, `export`, `rollout`,
`pwd` (`{client}`); ACP `export {client}`. Claude Code and Codex: `reload_config {what}`, sent
when the harness's own screen beside it ends: start again once idle, resuming, to load settings
changed outside it.

## State

Besides the timeline-related fields (`status`, `approvals`, `queue`, `delivery`, `tasks`,
`todos`, `usage`, …):

- `compacting?` — its harness compacting a conversation now (summarizing it to free context):
  `{since, tokens?, parent?}`, `parent` a subagent's (its spawning call), one at a time, the main
  thread's over a subagent's; meanwhile `doing` is "Compacting the conversation". Once it's done
  its marker, a notice whose id starts `compaction:`, is where it ran: "Compacted · 967k → 48k
  tokens · 1m 38s", as much as its harness says; a warning when it didn't compact ("Couldn't
  compact: …"), or stopped first (its turn or its process ended: "Compaction stopped after 41s").
  Claude Code says one with `status: "compacting"` (again every 30 s), its end (`compact_result`,
  `compact_error`) and its boundary (`compact_metadata`); Codex with its `contextCompaction` item
  (its older `thread/compacted` alone, from a Codex without the item); ACP agents as above. The
  Antigravity CLI says nothing of one. The conversation draws the one under way where its live
  line is, and each marker as the boundary it is, kiln's earlier words for one
  ("Conversation compacted …") too.

- `objective?` — the goal the harness itself pursues across turns (Claude Code's `/goal`,
  Codex's thread goal) and how it stands: `{text, status: "active" | "met" | "unmet" | "paused" |
  "cleared", reason?, at}`. Claude Code's comes from its `/goal` output and the verdict it writes
  to its transcript; Codex's from `thread/goal/updated`. Goal stacks follow it (docs/goals.md).
- `limits: Limit[]` — the account's plan limits, window by window: `id`, `label`,
  `used_percent`, `resets_at` (Unix ms), `window` ("5h", "7d"), `used`/`limit`/`unit` (extra
  usage, spend caps), `status` ("allowed", "warning", "rejected").
- `account` — `"<harness>:<plan>[:<short hash>]"`, the same across the account's sessions,
  never an address. `plan` — the plan in words ("Max", "Pro", "API key", "Signed out").
- `harness_status: StatusItem[]` — the harness's own status lines (`group`, `label`, `value`,
  `tone`): Claude Code's /status rows; put together for Codex and ACP agents.
- `mcp: McpServer[]` — `name`, `scope`, `transport`, `status` (connected, pending, failed,
  needs_auth, disabled), `error`, `enabled`, `tools`, `resources`, `prompts`, `target` (the
  command line or URL, never the environment, credentials or query), `server`, `auth`.
- `skills: Skill[]` — `name`, `description`, `source` (user, project, plugin, built-in,
  claude.ai, mcp, system, …), `path`, `args_hint`, `user_invocable`, `enabled`.
  `plugins: Plugin[]` — read-only.
- `inventory` — for `mcp`, `skills`, `plugins` and `limits`: `"loading"` (not reported yet),
  `"ready"` (an empty list means none) or `"unsupported"` (the harness cannot report it).
- `pause?` — it waits for a usage limit to reset (below): `reason` ("limit", "threshold"),
  `window?` and `label?` (the `Limit` reached), `said?` (the harness's words), `until?` (Unix ms
  of the reset), `since`, `auto` (it goes on by itself at the reset), `by` ("harness": it reached
  the limit; "account": another session on its account did; "threshold"), `cut?` (`{message?,
  text, at}`: the turn the limit cut off), `checkpoint?` (`{git_ref, commit, at}`), `tries`,
  `checking?`, `next_check?`. `pause_at?` — its stop point (percent). `manual_resume` — pauses end
  only when a person resumes them. `move_on_limit` — at a limit it goes on on another machine with
  room; `move_plan` (`{to?, kind?, model?}`, always sent by runners that take one) — where and in
  what, as a person chose.

- `trouble?` — what keeps its harness from its provider, said from the harness's own stream at its
  first retry rather than once it gives up (Claude Code retries a refused key for about four
  minutes; Codex without a network waits as long as it runs): `kind` (`auth`: its credential was
  refused, 401 or 403; `reach`: no answer came), `status?` (the HTTP status), `said` (the harness's
  words), `since`, `times` (its retries since). A notice says it once in the timeline, and another
  as its model answers again, which ends it. kiln stops nothing on its own: sign it in, grant the
  host, or stop the turn. From Claude Code's `api_retry` and its own errors, and Codex's
  `codexErrorInfo`.

`Info` (the lists) carries `limits`, `account`, `plan`, `left_behind`, `pause`, `pause_at`,
`move_on_limit` and `move_plan` too.

## Usage limits: pause and resume

When the account an agent runs on reaches a usage limit (Claude Code's 5-hour or weekly window,
Codex's rate limits, an ACP agent's limit error), the agent **pauses** instead of failing turn after
turn (`harness/pause.rs`):

- **Once, in one place.** The harness's words go into one notice; the agent's state reads Paused,
  in its head and on Mission control, with when it goes on. A turn still under way (a goal's Stop
  hook asking again and again) is stopped.
- **Nothing more is sent.** Its queue, goal stack and plan steps wait. A message sent meanwhile
  (from the console, a broadcast, the API) joins the queue.
- **Checkpoint.** Its folder, when it is a git repository, is kept as it was in a commit outside
  every branch: `refs/kiln/checkpoints/<agent>/<unix ms>` (`state.pause.checkpoint`). A throwaway
  index is used, so the folder, its index and its branches don't change. Untracked files are kept,
  ignored ones aren't. `git diff <commit>` shows what changed since; `git restore --source <commit>
  -- .` brings it back.
- **The whole account.** The server tells every other session on the same account, on every
  machine (`limit_hold`, naming the account), so they don't each find out by failing; each one the
  limit is its own as well pauses. Only the account a session is signed in to now counts: one
  signed in as someone else by now (as it knows itself, ahead of the server) goes on, and so does
  one whose own usage shows the limit isn't its own: that window starts over at another time
  there (another account's), or it's another model's (Weekly · Opus, to a session on Sonnet).
  Every other session asks its harness who it's signed in as and its usage first, and pauses only
  if that answer shows the limit reached on the same account: what it knew of its limits may be
  out of date, or from before a sign-in, so a session with room is never paused for another's
  limit. A harness that doesn't answer within 30 s isn't paused. Paused, it keeps to its own
  settings: it goes on by itself at the reset or not, and goes on elsewhere or not, as set for it.
- **Going on.** Once the window starts over (`until`, plus 30 s and a spread of up to 90 s across
  the account's sessions), kiln asks the harness for its usage (Claude Code's `get_usage`, Codex's
  `account/rateLimits/read`). A limit no longer reached ends the pause: the turn it cut off goes on
  first, as kiln's own message ("Your previous turn was cut off by a usage limit, which has reset
  now. Continue where you left off."), then the queue, then the goal stack. A limit still reached
  waits for its next reset. With no reset time known (an ACP agent, Codex's "try again in …"
  words aside), kiln checks after 10, 20, 40 and then every 60 minutes. A harness that can't
  answer goes on and finds out.
- **Your call.** `resume` ends it now (after a plan upgrade, say). Turn off "Resume by itself at the
  reset" (`auto_resume`) to keep it paused until you do. Picking another harness's model
  (`set_model` with `kind`) ends it too: the conversation goes on there, the cut-off turn first.
  The pause panel folds down to its limit and reset time. **Switch account** opens the same login
  dialog as `/login`, on the agent's machine; after signing in, **Resume now** retries the work.
- **Stop point.** `pause_at` (the head's usage badge → Pause at, or the agent's Settings → Usage
  limits: 80, 90 or 95 % used) pauses the agent once a limit is that full, before the limit cuts a
  turn off. Between turns, or in the middle of one (an agent working on its own can go for hours
  in one turn): at its next step, the command or edit under way finishing first. The turn it
  stopped goes on from there, first, once the limit resets. A turn waiting on you (an approval)
  is left to you. Off by default.
- **Go on elsewhere.** With `move_on_limit` on ("At a limit" under the usage badge and in the
  agent's Settings: wait for the reset, go on wherever has room, or go on on a machine you pick,
  then the harness and model there; off by default, a conversation at a time), a pause sends the
  agent to another of your machines instead of waiting (`harness/elsewhere.rs`). A machine and a
  harness chosen are the only ones it goes to; otherwise kiln picks one that is
  connected, takes sessions over (reach), and has the same harness signed in to another account,
  one kiln doesn't know to be as full. Failing that, another harness its conversation can be
  carried into (any other), signed in with room, with the first model it lists there (its own
  default, when it lists none).
  Its own harness comes first, then a machine with a copy of its repository, then the emptiest
  account; a repository not there yet is copied over and kept in step. It moves as a move you ask
  for does (`docs/reach.md` §8), and goes on there unpaused, the cut-off turn first. With nowhere
  to go its timeline says why, and kiln looks again every few minutes. A runner that doesn't
  take a choice (a kiln from before it) leaves it as it was, and the page says so after a few
  seconds, with the way to upgrade it; one from before a machine could be chosen offers only
  "wherever has room".
- **Go on now.** The paused line above the composer: resume now, resume at the reset or not, or
  go on now: here in another harness (any other on its machine), or on another machine that takes
  agents over, in its own harness or one its conversation can be carried into, with the model
  chosen; the turn the limit cut off goes on there. What you do there that its runner doesn't do
  (an older kiln: it stops again at its stop point as it resumes, or leaves a command it doesn't
  know) is said after a few seconds, with the way to upgrade it, and at a stop point with what
  works on it: resuming with the stop point off.
- **Restarts.** A pause, its timer, its stop point and going on elsewhere are kept with the session
  on its machine (the runner's `harness.json`, the server's `local.json`) and go on after a restart.
- **Queued meanwhile.** While it is paused the composer queues (its words and button say so), and
  what is queued waits for the reset; nothing queued can jump the pause (no steer, no send now).

Repeated identical notices in a turn (such as a limit said over and over by a build before this)
show once in the timeline, with a count.

### Runner upgrades: agents keep their processes

A runner under `kiln supervise` (how `kiln install --agent` runs one on Linux and macOS) upgrades
blue/green (docs/releases.md): the new build starts beside it, and once it's connected, every
agent session is handed to it over a local socket, its harness process still running. Its pipes are
passed across, not reopened. A runner upgraded in place (a new kiln binary where its own is, run
without a supervisor) re-executes itself instead, same pid, and hands every agent session to its
new image the same way. Either way nothing the agent is doing stops:
- a turn mid-reply goes on;
- a permission prompt waiting stays waiting, and is answered as before;
- the commands it left running in the background keep running;
- its timeline, queue and pause are where they were.

The upgrade doesn't wait for turns to end. If a blue/green upgrade's new build doesn't come up, the
sessions never leave the runner.

How: each session stops reading its harness's output at a line boundary. What was read is taken
in; the rest waits in the pipe. It writes out its whole state: the timeline, and the adapter's
(requests in flight, prompts waiting, streaming state). Then it keeps its pipes open, across the
exec or passed to the new build (`SCM_RIGHTS`), and the new process takes the harness over by its
pipes and its pid, and reads on. Some sessions can't be handed over: a switch, move or setting
waiting for its turn to end, or on Windows, which has neither upgrade. Those are stopped and start
again from their record, continuing the conversation, as every session did before.

### Messages while its machine is away

When an agent's machine is offline (or the connection to the server drops), the composer keeps
working: what you send is kept in your browser ("Kept here", editable and removable, across
reloads) and goes to the agent in order once it can take it, ahead of anything sent after.
Files attached go along when they were uploaded; ones that couldn't be (no server) stay with the
page until it reloads.

A message sent while one of its files still uploads waits there too (W21-R15): Send never waits
for a file, and the box is free at once for what's written next. Its row says each file on its
way; once they're up it goes, in its turn, carrying each that made it (one that didn't is said in
a toast, and the message goes without it), and what's queued after it waits behind it. Kept from
before a reload, its uploads went with that page: it waits for you to send it without each (its ×)
or remove it. Mission control's Start doesn't wait for files either: the agent starts, and its first
message waits for them on its page; several agents get theirs once they've started and the files
are up.

### A copy left behind

An agent that goes on on another machine while its own is cut off (moved there from the server's
copy) leaves its copy running where it was, at the epoch it left (docs/ci.md W27-R08). kiln fences
what that copy does through it. Each act an agent makes through kiln (a kiln operation it calls, a
page it shows, a run on another machine) is signed by the runner process holding it, with the
agent's key at its epoch, and kiln takes it only from the agent's holder now. The copy's acts are
refused, answered with why (the epoch it's at, and where and when the agent went on), and recorded
with who signed them, that epoch and the transfer the agent went on with (`kiln transfers
refused`). One its runner couldn't put to kiln while cut off isn't done, and kiln hears of it once
the runner is back.
At the runner's first word back, kiln tells it to let go of the copy. What kiln can't refuse is
what the copy does on its own machine without kiln (the files it edits there, the commands it runs
there): that goes on until its runner hears from kiln. A runner from before signed acts is taken by
its machine, as before.

### What you sent, at once

A message shows at the end of the conversation the moment it goes, marked *sending…*, then *sent*
once its machine has it (the socket's `ack`), until the agent's session shows it (its prompt, or
its place in a queue, the agent's or a subagent's), which then takes its place (`SessionStore`'s
`outgoing`, `ui/src/api/harness.ts`). While it waits (its machine away, its inbox slow to answer)
it goes again under its id by itself, as every command does (docs/wire.md, "The UI's sockets"),
and the line above the composer says why. One refused for good (the socket's `nack`, or no answer
at all) or whose agent ended before it got there says *failed to send* and why, with Retry and
Remove. Retry sends its words, files and reply again as a new message: its id's answer stays the
refusal. These are kept in the page only; CP-T3.4's outbox keeps them across reloads and tabs.

### A machine on battery

A laptop's runner says how its battery stands as it connects, and as the OS says its power source
or its charge moved (`AgentMsg::Power`; notify(3)'s power-source and time-remaining keys on macOS,
the kernel's power-supply uevents on Linux, the power-setting callbacks on Windows). On macOS it
reads `pmset -g batt`; on Linux, `/sys/class/power_supply`, where a peripheral's battery doesn't
count. The Runners page shows it under the machine's name ("battery 34% · 2h 15m left", "charging",
"on its charger"). A machine on battery that drops to 20% or less is low: Runners and Mission
Control say so at the top, with what stops when it sleeps (its agents, its jobs). The server logs
it, audits it and sends it to webhook subscribers as `machine.battery_low`, once, as the machine
gets there, and to the devices of everyone with an agent on it (docs/notifications.md). A Mac on battery hibernates at about 1%, and its runner, agents and jobs go with it
until it's plugged in; kiln can't keep it up, only say so in time.

### Tasks: shells, monitors and other work an agent leaves running

`state.tasks` holds every task a harness runs beside its turns: subagents, and also commands it
sends to the background, monitors and workflows. Besides `id`, `description`, `status`
("running", "pending", "paused", "completed", "failed", "stopped", "killed"), `background`,
`started`/`ended`:

- `class`: "shell" (a command left running), "monitor" (a watch: each event it sees wakes the
  agent), "agent" (a subagent), "workflow", "mcp" (an MCP call moved to the background).
- `command`, `cwd`: what a shell or monitor runs, and where.
- `output`: where its output is written on its machine (Claude Code, while it runs). Read it with
  the task output endpoint, whatever the harness: Codex's terminals are kept by kiln itself.
- `process`: the harness's own handle for its process (Codex). `exit_code` once it has ended.
- `events`, `last_event`: a monitor's events so far and the latest (one line). `wakes`: who they
  wake, "agent" or the subagent task that started the monitor.
- `stoppable`: `stop_task` can stop it.
- `takes`, `via`, `queue` (subagents): how a person's message reaches it now (`queue`, `steer`,
  `now`; none: it takes none), `"parent"` when its agent passes it on, and the messages waiting
  for it to finish ("Messages to its subagents", below).

### Messages to its subagents

With a subagent's thread open (`?task=`), the agent page's composer writes to that subagent,
as the agent's own composer writes to the agent: queued for when it finishes, steered into its
turn, or sent now (its turn stopped, this one the next), as its `takes` allows; Stop (and Esc)
stops it (`stop_task`). Its queue shows above the field, edited, moved, steered or sent now from
there (`queue_edit`, `queue_move`, `queue_steer`, `queue_send_now`, `queue_remove`, by id). One
that takes none (it ended, or its harness has no way in) leaves the composer writing to the
agent, as before. The message is a `send` with `task` (its task's id); it shows in the
subagent's thread, its delivery as any message's. One a subagent can't take, or one that waited
for a subagent that ended, is said not sent in its thread, and why. While the session is paused,
messages for its subagents wait in their queues too.

| Harness | How it reaches the subagent | takes |
|---|---|---|
| Codex (multi-agent v1, its default) | on the subagent's own thread: `turn/start` (with the session's permissions then), `turn/steer` into its turn, `turn/interrupt` then `turn/start` for now; Codex says it back under kiln's id (`clientUserMessageId`) | queue, steer, now while it works; queue (a turn at once) between turns |
| Codex (multi-agent v2; a subagent this process didn't spawn) | Codex refuses input there (`direct app-server input is not allowed for multi-agent v2 sub-agents`), so kiln asks the agent, in its turn, to pass it on with its own agent tools (`send_message`, `followup_task`; v1's `send_input`); delivered once the agent's call reaches it | queue, steer; agents it started itself only |
| Claude Code | it takes nothing for a subagent from kiln (2.1.309 and 2.1.310 answer `send_task_message` "Unsupported control request subtype" in `-p`, and a prompt naming the subagent's call goes to the agent), so kiln asks the agent, in its turn, to pass it on with its SendMessage tool: at the subagent's next tool round while it works in the background, or resuming it once it has finished; under way when the agent's SendMessage to it starts, delivered when that succeeds | queue, steer while it works in the background; queue while in the foreground (its agent's turn waits on it) or once it finished; none once stopped (Claude Code doesn't resume one stopped) |
| Antigravity CLI | before the subagent's next model call: kiln's hook (below) answers agy's `PreInvocation` in that subagent's conversation with the message (`injectSteps`, a `userMessage`; the files attached by their paths); under way then, delivered once it goes on past that call. Not once it has ended (agy calls a stopped subagent's model no more), nor before kiln's hook is heard from it | steer while it goes |
| ACP agents (opencode) | kiln can't prompt an opencode subagent itself (an ACP prompt goes to a session kiln opened), so it asks the agent, once its turn is free, to pass it on with opencode's subagent tool, which takes a follow-up by the subagent's session id, running or ended; sent once kiln asks | queue |
| ACP agents (Gemini CLI, Goose, Amp, Devin) | kiln has no subagent threads for them: their subagents aren't reported over ACP (below) | none |

What kiln asks an agent to pass on carries the person's words as written (the files attached by
their paths) and is kiln's own message in its timeline (`kiln-relay-<message id>`); a turn that
took it and ended without passing it on leaves the message not sent, saying so.

Codex's native subagents keep their own threads and execution settings. kiln tracks each one by
its native thread ID and keeps its work under the original spawning call, including later
messages and follow-up work. Read-only `thread/read` and `thread/turns/list` requests fill in its
status, latest 100 turns, elapsed time and result; live child notifications update that same
view. Tracking never starts, resumes or attaches another executor to the child (it may interrupt
one working with other permissions than the session's: see permission modes above). Its messages,
commands and usage stay separate from the parent's turn; its edits join the session's changeset,
credited to its spawning call (see "Who changed a file, and whose it is"). A spawn call
finishing does not mean the child has finished, and unavailable child history does not end the
parent. As in Codex's own TUI, a message to a child (`send_message`, `followup_task`) doesn't
change whether it runs: a message queued for an idle child starts no turn, and a follow-up's turn
is heard from the child itself (its notifications, and the reads that follow the message). A
child kiln never read whose session went on in another harness (no Codex to ask) has its turns
read from Codex's own record of its thread (its rollout) instead, off the event loop.

A Codex terminal (unified exec, also when code mode starts it) runs on past its call and its turn,
interrupted or not, as in Codex's own TUI, and ends when Codex says its process ended. At each
turn's end, and every 30 s while some show running, kiln also asks Codex which it still runs
(`thread/backgroundTerminals/list`): one it no longer runs reads "completed", without an exit
code, until Codex's own word on it comes.

Claude Code's list of live background tasks (`background_tasks_changed`) ends any task kiln shows
left running that the list leaves out, should its end never have come.

Nothing a harness ran outlives its process: when it ends (a switch to another harness, a stop, a
restart, a crash), its live tasks end "stopped" and its calls still under way are cancelled, all
of them kept to read; a new process starts with nothing of an earlier one's running. A message it
was handed and never finished ends dropped; one its adapter held for it, which it never got, goes
to the next process first (docs/sends.md, "A harness that ends"). What kiln
took back over (`left-<pid>`: "What an agent left running") is the exception. The state keeps
the newest 200 tasks (the oldest finished go first, with the output kiln kept of them), and a
finished task keeps 2,000 characters of a long command (its call keeps it whole), so the state
always reaches viewers whole.

The task output endpoint projects a native Codex child's assistant messages and tool results,
scoped to that child and its original spawning call. Its stored final result remains readable
after timeline eviction. Each read replaces the previous snapshot, so history imports and
streaming replacements do not duplicate output. An unavailable completed result returns an
error rather than a successful empty read.

`state.left_behind` counts the tasks left running while no turn is under way (the agent idle or
exited): shells, monitors, workflows, background calls and subagents sent to the background.

Ending an agent (`DELETE /api/harness/sessions/:id`) stops what it left running: each live task
through the harness (`stop_task`), then anything still under the harness process (commands that
left its process group included), then the harness. Its last state, what the ended agent shows,
has every task stopped. A console can warn first with `left_behind`.
No harness offers pausing a task: Claude Code reports a workflow it restored as "paused", with no
way to resume it from outside.

A tool call reads `pending` while it is asked about (an approval in `state.approvals` names its
`tool_id`) or while it waits its turn behind one; `running` once it is under way. MCP calls name
their `server` and `tool_short`; skill calls their `skill`.

### What a person does with an agent's subagents

An agent's subagents show as one tree: those kiln started for it (sessions of their own, `subagent`
with it as `parent`, and theirs) and its harness's own (`subagents` in lists, nested by
`parent_task`; Antigravity's from its stream, but not a subagent's own), each under what started it, in the order they started. Mission control's row of
an agent with subagents has a toggle (the subagent mark and a chevron) that opens the tree under
it in the row's own columns: each subagent's piece of the path (the lines of those it's under
passing by, a curve into its own mark), its name, its word and what it's at, its machine, time and
harness, its pause, and the way to its thread. One that needs you has its own piece of the path
tan, the whole of its row, and its request is answered there unless its agent's row already
answers it: Allow, Deny or Approve at once, and a question under its row (the options, Other,
Decline and Submit, as the chat draws it), so Mission control answers it without leaving. The line
by the composer opens the same tree with a pick box each, a select-all where its mark was; the
Plan tab shows each step's agent live under its step. The path runs on unbroken from row to row.

One at work turns its open ring through the prism once every 1.4 s, as Ship's running glyph does,
every ring on the page's clock, and its word carries the prism, as Mission control's working rows'
do; nothing else in the tree moves, and nothing turns where motion is reduced or kiln's motion is
paused.

One that ended (Done, Failed, Stopped) is removed as kiln's Remove removes an agent that has: its
trash where its pause was, a swipe sideways by touch, or Remove at the head of the composer's tree
for those picked that ended (one that ended can be picked; it takes no message). One kiln started
is its session removed (`DELETE /api/harness/sessions/:id`, as Mission control's Remove); one of its
harness's own leaves its agent's session (`remove_task`; a machine from before it can't, and its
trash is off). Removed, it's gone from the tree at once, and back with why if its Remove didn't go.

With subagents picked, the composer's To line names them (two on a phone, four wider, then +N),
and a message goes to each its own way: one of the agent's own harness's as a `send` with its
`task` on the agent's socket, any other over HTTP to its session (with `task` for a harness's
own). Each name then has its mark: delivered, waiting (queued, or its session paused), or not
sent.

A subagent's crumbs name its whole path from the agent at the top, each earlier name opening its
own thread, with Pause, Branch and Permissions on that line. What its parent wrote it (its brief,
what it sent it: in a harness's own subagent's thread, a message without a client id; in a
session of its own, its first and those whose id is `from-agent-<parent>-…`) is under its
parent's name, with the way back to its parent's thread. A person's message its agent passes on
(`via: "parent"`) says so ("You · via …"). A finished one ends with its word, when, and the way
back.

**Pausing** (`pause`, `{on, task?}`). Without `task`, the session's processes stop where they are:
its machine's pacer holds its group at share 0 (cgroup.freeze on Linux, SIGSTOP on macOS, every
thread suspended on Windows) under the pacer's guard, which starts them again should the runner
end (docs/headroom.md, "A person's pause"). `state.frozen` says since when; a runner that can't
pause says why, and one that restarts holds a paused session again. The server sends the pause,
and its end, to every subagent kiln started under it. With `task`, that subagent of its harness's
own is held at its next tool call: kiln has a hook on every one of Claude Code's tool calls (a
`PreToolUse` matcher for every tool, its answer awaited up to 7 days), and on each of Codex's local
ones (commands, `apply_patch`, MCP calls and its read tools): a `PreToolUse` command hook kiln gives
in the session's flags, with that hook's trust for this session alone (the hash Codex trusts it
by), so the person's own hooks keep theirs. Its command, `kiln harness-hook`, hands the call to the
runner's harness socket, which knows the session by the asking process; Codex names no subagent,
so kiln tells it by the call's turn. Antigravity's are on each of its tool calls too: kiln's entry
in agy's hooks.json (above), the subagent told by the call's conversation. The call of a paused subagent, or of one under it, waits for
its answer until it's resumed (`tasks[].paused_at`). `tasks[].pausable` says which can be held so:
Claude Code's while they go; Codex's once Codex lists kiln's hook as trusted (a session whose hook
it doesn't trust says why); an ACP agent's where its requests name them (opencode's, below);
Antigravity's once kiln's hook is heard from it (its first hook turns `limits` on in its
`meta.features`). What a pause holds says Paused; the
one paused offers Resume, and those under it wait for that.

**What each may do** (`perms`, `{perms?, task?}`): Read, Edit, Run and Web, each `allow`, `ask` or
`off`; a `folder` its file calls stay in; the most `minutes` and `tokens` it may use. Never more
than what it's under: the server narrows a session's to its parent's, and the subagents kiln
started under it to its own (narrowing one narrows those under it; widening it widens none);
Claude Code's, Codex's and Antigravity's adapters narrow one of their own subagents' to the
session's and the one it's under. They hold at each tool call the same hook sees: `off` refuses the call with the reason,
`ask` makes it ask the person (Codex's hook can't ask, nor can headless agy, so kiln holds the call
and asks them itself; a yes is agy's `auto_approve`, so it goes even where agy's mode would deny it,
and a no its `deny` with their words), a file outside its folder is refused (a command there asks; for Codex, the files an
`apply_patch` names), and past its minutes or tokens each call is refused and it's told to finish.
Codex's web search is hosted, out of any hook's reach (`web_whole` in its `meta.features`): `off`,
and `ask`, which it can't do, turn it off for the session's threads (`web_search = "disabled"`), as
it starts again once idle, the conversation going on; a subagent of its own has no Web choice.
Antigravity's web tools (`search_web`, `read_url_content`, `browser_*`) are calls its hook sees, so
Web holds per call there as each other kind does. The
shield on the thread's crumbs opens the panel under them, every choice at its right edge; a
session whose harness kiln doesn't answer that way (no `limits` in its `meta.features`) has it off.

**Branching** (`branch`, `{words, top?, task?}`). The fork on a subagent's crumbs takes what
changes, then a Variant or a New agent, and its runner makes the new session on its machine. A
subagent kiln started branches from its own spec, so it lands beside it, under the same agent: its
conversation forked where its harness can (`resume` in its features: its harness session, `fork`),
the person's words its first message, and handed over in words where it can't. One of its
harness's own is handed over in words (what it was asked, said and was told) as a subagent of its
agent. A New agent (`top`) is an agent of its own. Each carries `variant_of`, and the tree marks
it with the fork. A finished variant's thread ends with Keep, which tells its parent to use its
result in place of the one it varied, and to read it whole with `agents_result`.

**ACP agents** (Devin, Gemini CLI, opencode, Goose, Amp). ACP has one place a client can hold a
tool call, the agent's `session/request_permission`, and the agent decides when to send it: kiln
holds an ACP agent's calls **at the calls it asks about**, and no others. A call its own rules let
it make without asking goes as they say. `meta.features` has `limits` and `limits_asked`, and the
Permissions panel says so in one line. At each request: one from a paused subagent, or one under
it, waits unanswered and unshown until it's resumed (answered `cancelled` if the turn is cancelled
first); what its perms (the session's, narrowed by each subagent's) refuse is refused at once
(`reject_once`), the reason on the call; what they say a person sees is shown with the reason,
whatever kiln's mode, Bypass too; anything else goes as it always did. Per agent:

| Agent | Its subagents in the tree | A message to one | Pause one | What each may do |
|---|---|---|---|---|
| opencode | yes: kiln asks for its child sessions (`_meta["opencode/child-session-updates"]` at `initialize`) and each `opencode/session/child_update` makes a subagent, its tool calls under it, ended as it says | through the agent that started it (`via: "parent"`), once its turn is free: opencode's own subagent tool takes a follow-up by the subagent's session id, running or ended | yes, at each call it asks about (its requests name the subagent in `_meta["opencode/child-session"]`) | the session's and each subagent's, at each call it asks about |
| Gemini CLI | no: its subagents' calls never reach ACP (its policy engine decides them inside the agent) | no | no | the session's, at each call it asks about |
| Goose | no: it passes a subagent's request on under a new id, naming no subagent | no | no | the session's, at each call it asks about |
| Amp (amp-acp) | no: its adapter shows a subagent's calls inline and leaves permissions to Amp | no | no | the session's, at each call it asks about |
| Devin | not known: its docs don't say what of its subagents reaches ACP, and kiln shows none | no | no | the session's, at each call it asks about |

kiln's tests hold each to this with a stand-in ACP agent (`harness::acp`):
`acp_subagent_paused_request_waits_until_resumed`, `acp_off_refuses_and_ask_shows_with_reason`,
`acp_bypass_still_held_to_limits`, `acp_session_perms_hold_own_requests`,
`acp_opencode_children_are_subagents_in_the_tree`, `acp_nested_child_under_its_parent` and
`acp_opencode_message_goes_by_its_parent`; the UI's in `ui/e2e/subagent-acp.spec.ts`.

## Skills kiln installs

An agent's Tools tab lists its skills: the packages kiln installed for it, and those its harness
found on its own (its plugins too). **Add** takes skills from:

- **a link**: a GitHub repository (`github.com/owner/repo`), a folder or file in one
  (`…/tree/<branch>/<folder>`, `…/blob/<branch>/<file>`), or any git URL. kiln fetches it at one
  commit with the server's git credentials (`[gitops]`), so private repositories it can reach work;
- **a folder** on the agent's machine (`~` is that machine's home);
- **an upload**: a `.zip`, a `.tar.gz` or one `.md`;
- **words written in the page**: a name, when to use it, and its steps.

kiln looks before it installs anything (`kiln_ci::skills`, pure). In order, the first that
matches: a marketplace (`.claude-plugin/marketplace.json`, else `.cursor-plugin/`'s): its plugin
by the name the link ends in, else its only one, else a choice; a plugin's manifest
(`.claude-plugin/plugin.json`, `.cursor-plugin/`, `.codex-plugin/`, or agent-plugins.org's
`plugin.json`): its `skills` (a folder or a list) and `agents`; every `SKILL.md` a few folders
down, but under `deprecated`, `misc`, dot-folders and `node_modules`; or one Markdown file, a skill
by its frontmatter. `cursor/plugins`' pstack is read from the copy its marketplace names for Claude
Code; `mattpocock/skills` from its manifest's list; `t3dotgg/tstack` from its skill folders, with
the tools its README's Requirements name.

What it found shows before it installs: the package's name, version, commit and license; its
skills and agents; its hooks and MCP servers, which run code on their own and are off until turned
on; the setup each skill asks for on a machine (`uv sync` for a `pyproject.toml`, a venv and `pip`
for a `requirements.txt`, `go build -o bin/ ./...` for a `go.mod`, `npm ci --ignore-scripts` for a
`package.json`, `cargo build --release` for a `Cargo.toml`); and the tools it needs (setup's, a
script's interpreter, a Requirements list). A package is installed **for this agent**, **for all my
agents** (every agent that acts for the person who installed it, on every machine), or **for this
repo** (every agent working in this agent's repository folder, on its machine). The same package
installed again for the same scope replaces it. An agent installs nothing: every operation refuses
an agent calling, and none is an agent's tool.

A package is pinned to its commit (an upload's, folder's or written skill's: its content's digest),
its files kept once each in kiln's store of bytes by their SHA-256 (`skills/files/<sha256>`). A
link's branch is looked at again for a newer commit as the panel shows it, at most once each
`[gitops] poll_seconds`; **Update** fetches it, keeping what was picked and whom it's for. A package
turned off stays installed and no agent has it; **Remove** uninstalls it, and its files go once
nothing holds them.

Each machine with agents that have a package lays it out (`crate::skills::place`): the server says
what's installed and which session has which (`ServerMsg::Skills`, to runners that name `skills`,
docs/wire.md), the runner asks for the files it lacks (`AgentMsg::Skills` `want`,
`ServerMsg::SkillFile`), keeps each once by its digest, lays each package out per installation and selection as a
Claude Code plugin (`<agents>/skills/pkgs/<id>-<fingerprint>/`: `.claude-plugin/plugin.json`,
`skills/<name>/`, `agents/<name>.md`, and `hooks/` and `.mcp.json` only when turned on), runs its
setup there at a lower priority, a package's commands in turn, and checks for the tools it needs.
Each says its state back: fetching, setting up, ready, needs a tool (what's missing, by machine:
its skills are there anyway), or failed (why, and the last of its output). **Retry setup** repeats the
installation of that commit after a failure or after its missing tool was installed. The server's own
sessions are laid out by the server itself, beside its runner's, each letting go only of what
neither holds.

A Claude Code session loads a folder of plugins of its own (`<agents>/skills/sessions/<session>`,
`--plugin-dir`, beside kiln's own skills), a link to each package it has. As installs change, its
machine relinks it and tells it to load its plugins again (`reload_plugins`), so a running agent has
a new package's skills at once, as `/<package>:<skill>`. A session gets them as it starts, once
whose it is is known.

Installed skills work in every structured harness. W20-R01's delivery bridge (`skills::bridge`)
reads that session's package links on the blocking pool at actual prompt delivery, after holds and
queueing. Normal turns receive the current skills' names, descriptions, and absolute SKILL.md paths
for progressive discovery; a selected skill also carries its asset directory and user arguments.
Codex registers that session's directory with process-local `skills/extraRoots/set` and holds
queued turns until the registration is answered, then refreshes native discovery. A selected skill
gets a native `skill` input item. The selected instructions also travel as text, so older Codex
versions that lack root registration keep working through the portable path without native items.
ACP agents
(Devin, OpenCode, Gemini CLI, Goose, and Amp) and Antigravity get the selected instructions in their
prompt transport. Images, resource links, message ids and queue semantics are retained. Skills
remain readable while setup runs or needs repair. Disabling or removing a package updates the
next delivered turn's catalogue, including a turn that was already queued. Nothing writes to the
repository or a shared harness home, so agent/person/repository scopes stay isolated.

| Harness | Installed skill delivery |
|---|---|
| Claude Code | Native plugin discovery and reload |
| Codex | Process-local native discovery, native skill input items, portable fallback |
| Devin, OpenCode, Gemini CLI, Goose, Amp | Scoped catalogue; instructions and asset directory over ACP |
| Antigravity | Scoped catalogue; instructions and asset directory over headless stream-json |

Python/Go/Node/Rust setup is independent of the harness. Agent definitions, hooks and package MCP
configuration currently activate as Claude Code components; the UI says so for other harnesses,
and never claims that these optional components have been translated. Future ACP harnesses inherit
the portable delivery path. Codex's integration follows its [app-server protocol](https://learn.chatgpt.com/docs/app-server).
A runner whose kiln is older says: "Upgrade kiln on <machine> to
install skills".

| Method | Path | |
|---|---|---|
| GET | `/api/skills?agent=` | `{packages, machine, installs, loads, native_components}`: the packages the agent has, each with its source, commit, skills, agents, hooks, MCP servers, setup, needs, scope, `on`, `update` (a newer commit) and its state on each machine it went to (`machines`) |
| POST | `/api/skills/look` | `{agent, from: link\|folder\|write, link?, path?, plugin?, name?, description?, body?}`: what it holds, as a `draft` to install; a marketplace of several plugins answers `{choose}` |
| POST | `/api/skills/upload?agent=&name=&plugin=` | the file's bytes (`.zip`, `.tar.gz`, `.md`): as `look` |
| POST | `/api/skills` | `{agent, draft, scope: agent\|mine\|repo, picks: {skills, agents, hooks, mcp, setup}}`: installed |
| PATCH | `/api/skills/:id` | `{on}` |
| POST | `/api/skills/:id/update` | fetched again at its branch's commit now |
| DELETE | `/api/skills/:id` | uninstalled |
| GET | `/api/skills/:id/skills/:skill` | `{content}`: a skill's SKILL.md |

Code mode has them as `kiln.skills.*` (`skills.list`, `skills.look`, `skills.install`, `skills.set`,
`skills.update`, `skills.remove`, `skills.read`).

## Files in messages

A file a message mentions shows in the conversation: what the agent or the person wrote, never
what a tool read or printed. Pictures and recordings show in place, their space taken before they
load so nothing moves; sounds, PDFs, text and logs show as a chip that opens in place.

- **Mentioned:** a full path (`/tmp/shot.png`, `~/rec/flow.webm`) or a `file://` address in a
  Markdown image or link, or a path in the session's folder in one; and, in code or among the
  words, full paths to pictures, recordings, sounds and documents (so prose about `/api/...`
  routes isn't taken for files).
- **Read on its machine:** the server asks the runner holding the session (`CAP_FILES`;
  `ServerMsg::HarnessFileRead`, answered on lane 7 in pieces of a megabyte, out of the way of
  jobs' logs, terminals and the conversation feed). A runner from before this can't, and the
  console says to upgrade it. The server's own sessions are read in place.
- **Kept:** once a message is done, the server keeps a copy of each file it mentions, by content,
  in `<data>/harness/files/`, so it still shows after the file is moved, changed or deleted, each
  message showing the file as it was then. Up to 64 MiB a file, 512 MiB a conversation and 4 GiB
  in all; past those a file is read from its machine when someone looks. A closed conversation's
  copies go.
- **Never:** anything that looks like a secret (keys, `.env` files, credentials, kiln's own
  config, `/proc`), nor types a person can't look at.
- **API:** `GET /api/harness/sessions/:id/files/:sha` (a kept copy, cached for good) and
  `GET /api/harness/sessions/:id/file?path=` (read now, and only a path one of its messages
  mentions); both take `Range`. Viewers hear what's kept as `{"t":"files","files":[...]}`, sent
  ahead of the events when they attach.

kiln's MCP instructions tell agents to show files this way, and to show something live (a chart, a
mock-up, a small tool) as a page: see docs/previews.md.

## Notes per harness

- **Claude Code** reads its MCP configuration when it starts: after `mcp_add` / `mcp_remove`
  (`claude mcp add/remove`) it starts again between turns, continuing the conversation (and not
  while it has tasks left running, which starting again would end). A
  sign-in finished through kiln (`redirect`) is handed to the CLI and the server reconnected.
  Skills are listed before the first turn (skill commands and the /skills menu) and completed
  with init's list after it.
- **Codex** runs commands that outlive their call in terminals of its own (unified exec): kiln
  shows each as a "shell" task, kept running when the turn ends, with its output kept by kiln
  (the last 256 KiB). Codex has no way to stop one from outside: `stop_task` stops the process
  itself (the newest under Codex running that command, with what it started).
- **Codex** refuses to fork a thread for a moment after a turn ends (it is still writing the
  thread's history out), and to resume one its app-server daemon still holds for a terminal
  that just quit (about a minute after a take-over ends the TUI): kiln opens it again at each
  tick, for up to two minutes, with `state.doing` saying so meanwhile.
- **Codex** TUIs are matched to their rollout by the thread they began after they started (or
  the id `codex resume <id>` names). One resumed from Codex's own menu writes to an older
  rollout that could be another session's: it is listed without a conversation (`no_id`) and
  cannot be taken over.
- **Codex**: stdio servers are added with `codex mcp add`; http servers are written to its
  configuration through its own API (the CLI would sign in at once, in a browser on the agent's
  machine). Removing an http server signs it out first. Effort and model are set per turn.
- **Antigravity CLI (agy) in a Shards microVM**: kiln's built-in Agentfile `harness-antigravity`
  (`.kiln/images/harness-antigravity/`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26).
  - **Its pin.** pin.sh takes Google's static musl builds for linux/amd64 and linux/arm64. It checks
    each archive's SHA-512 against the manifest Google's updater serves over HTTPS, as Google's
    install.sh does (Google publishes no signature). The updater keeps a manifest for its current
    version alone, so pin.sh pins that version and refuses any other. pin.json records each
    archive and its one member, `antigravity`, which becomes `bin/agy`, and the smoke
    (`bin/agy --version` prints the version).
  - **Its base.** The build needs no loader or libc. It does need CA certificates, because agy
    checks TLS against the system's bundle and carries no roots: on debian:trixie-slim and busybox
    its HTTPS fails with "certificate signed by unknown authority". The default,
    buildpack-deps:trixie-scm by digest, has them, as Alpine does.
  - **Its HOME.** agy keeps its settings, MCP servers, conversations and, with no D-Bus, its
    sign-in tokens under `$HOME`, and no variable of its own moves them. A harness sees its image
    read-only, so harness.json's run.env sets `HOME=/home/antigravity`, and that whole home is a
    harness volume, the person's. It also sets `AGY_CLI_DISABLE_AUTO_UPDATE=true` (the pin is the
    version that runs) and `XDG_CACHE_HOME=/tmp/.cache` (caches on the harness's scratch).
  - **The Shards it needs.** A shards at or after Shards main 997f4e47e, where run.env's HOME
    replaces init's `HOME=/tmp`. v0.8.0 is the first release with it. No `env HOME=` launcher is
    kept, since no earlier release starts a harness from its harness.json at all: that came with
    Shards main 1da9132b7, after v0.7.0.
  - **Volumes and grants.** `/workspace` and `/home/antigravity`, both outside `/tmp`, which each
    harness's scratch covers. The network grants are the bring-up's: names through the microVM's
    resolver and TCP 443 out, nothing in. agy speaks HTTPS alone, including its telemetry to
    play.googleapis.com. A connect over IPv6 fails on Shards 8f3cd5b7, so names reach out over IPv4.
  - **What later steps change.**
    - Step 7 gives every harness a writable HOME of Shards' own.
    - Step 8 plugs the project at its host path in place of `/workspace`.
    - Step 9 grants providers by name instead of TCP 443 to anywhere.
    - Steps 17 and 18 have kiln start agy in the microVM through Hex, with its config on a volume
      kiln reads.
    - Step 33 brings its sign-in as placeholders filled in at the microVM's egress, so no token
      is ever kept in its volume.
  - **Records.** pin.sh's record is `pin.log`, beside it. docs/research/s26-agy-agentfile/ holds
    the rest: the tests' runs, the microVM build, smokes and probe on both platforms
    (`evidence/step4/`), and what stayed unverified (its README).
- **Gemini CLI in a Shards microVM**: kiln's built-in Agentfile `harness-gemini`
  (`.kiln/images/harness-gemini/`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 21).
  - **Its pin.** Google ships no Linux executable of Gemini CLI: its Linux build is the npm package
    `@google/gemini-cli`, run by Node.js. pin.sh checks the package as npm documents it: each
    registry signature over its name, version and integrity, then the tarball's SHA-512 (Google
    publishes no provenance for it). The Node.js is derived, not chosen: the newest major that
    Gemini's own Linux CI tests and Node still supports, at its newest release, from nodejs.org's
    archives, whose SHA-256 Node's signed SHASUMS256.txt gives. pin.json takes the package whole at
    the harness's root and Node's `bin/node` and `LICENSE` alone, at `node/`, and kiln's unpacker
    holds each to its pinned totals or digest. Its smoke runs that node on the package's
    `bundle/gemini.js` with `--version`, which prints the bare version.
  - **Its base.** Node's Linux builds link glibc, libstdc++ and libgcc_s, and Node publishes no arm64
    musl build, so the image has no musl variant and Alpine can't start it. The default base,
    buildpack-deps:trixie-scm by digest, has them, and bash for Gemini's shell tool.
  - **Node off PATH.** harness.json starts `/harness/gemini/node/bin/node
    /harness/gemini/bundle/gemini.js --acp`. A harness's own directory and its `bin/` lead the PATH
    its commands inherit, so the harness's node, elsewhere, leaves the project's own node to the
    model's commands. No `npm` or `npx` is in the harness, and no `gemini` is on its PATH: a project
    whose MCP servers need npm brings it in its image.
  - **No update check.** Gemini checks npm for a newer version, and installs it, only from its
    interactive screen. harness.json points `GEMINI_CLI_SYSTEM_SETTINGS_PATH` at kiln's
    `system-settings.json`, which turns both off in every mode. Gemini reads a system settings file
    only when it and every directory above it belong to root and no group or other may write them:
    in the image they belong to root, and kiln's unpacker gives the harness's directories 0755. kiln
    writes `harness.json` and `system-settings.json` themselves under its umask, so a context made
    under a umask that leaves group write (002) has Gemini skip its settings, with a warning.
  - **Its HOME.** Gemini keeps its settings, its sign-in (`oauth_creds.json`, or its file keychain
    `gemini-credentials.json`, whose key is salted with the hostname and the user), its history and
    its projects under `$HOME/.gemini`. harness.json sets `HOME=/home/gemini`, and that whole home is
    a harness volume, the person's. Folder trust is on out of the box: whether a project is trusted
    is the session's to say.
  - **Two processes.** Every start is two node processes: a light parent that starts node again with
    a heap limit of half the machine's memory.
  - **Volumes and grants.** `/workspace` and `/home/gemini`. The network grants are the bring-up's:
    names through the microVM's resolver and TCP 443 out, nothing in. Every endpoint Gemini reaches
    by default speaks HTTPS: Code Assist for a Google sign-in, the Gemini API for a key, Google's
    OAuth endpoints and its usage statistics (play.googleapis.com).
  - **What later steps change.** As agy's: steps 7, 8 and 9 (HOME, the project at its host path,
    providers by name), 17 and 18 (the start through Hex, its config on a volume kiln reads, and its
    sign-in on the terminal lane, which starts the same node and script), and 33 (its sign-in filled
    in at the microVM's egress).
  - **Records.** docs/research/s21-gemini-agentfile/: its research's evidence, the pin's runs and its
    tests, and what stayed unverified (its README).
- **ACP agents** report no MCP servers, skills or plan limits; their slash commands run with
  `skill_run`.
- **Cursor CLI** offers subagents as sessions of their own only to a client that takes ACP's
  `subagents` capability, which kiln doesn't yet: its `cursor/task` notices aren't read, so its
  subagents aren't tasks of the session.
- **Pi** runs no command from kiln's catalog but its own (`get_commands`: its extensions' commands,
  prompt templates and skills, each sent as `/name`) and kiln's (`/compact` is Pi's `compact`, run
  once a turn under way ends). Its built-in terminal commands (`/settings`, `/login`, `/model` ...)
  are its own screen's.
- kiln never lets a harness CLI it runs open a browser on the machine; sign-in pages are shown in
  the console, wherever the person is.

## Metrics

On the server's `/metrics` (Prometheus; Observe reads it): its goal driver's counts, and its own
harness host's (the sessions on the server's machine). A runner's host counts its own in its
process, which serves no `/metrics`. Every series is there from the start, at 0. The table is
the code's (`metrics::AGENTS`): a test holds the two together.

| Metric | Type | Labels | Counts |
| --- | --- | --- | --- |
| `kiln_goals_sent_total` | counter | `kind`: `start`, `reminder` | Goals the goal driver sent an agent: a goal's first send, or afresh once resumed or reworded (start), or again after the agent stopped short of it (reminder). |
| `kiln_goals_ended_total` | counter | `status`: `met`, `skipped` | Goals that ended: met (as its harness, the agent or you said, or with its plan's steps or run), or skipped (cleared in the agent, skipped through goal control, or taken out while under way). |
| `kiln_harness_polls_total` | counter | | Looks the harness host's watch took at what its harnesses run (every 10 s). |
| `kiln_harness_records_written_total` | counter | | Writes of the harness host's record of its sessions. |

## Capability matrix

Checked end to end against the real CLIs with scripted model APIs and isolated configuration
homes, on the server and through a runner: Claude Code 2.1.296 and Codex 0.162.1; ACP through
Zed's Claude agent adapter (claude-agent-acp). Devin has separate installed-CLI smoke tests for
settings, steering, interruption, queueing, resumption, native history and terminal ownership.
Cursor's CLI and Pi were written from their docs and their own source (Cursor CLI 2026.11.11, Pi
1.5.2) and are covered by their adapters' tests; neither has been run end to end yet.

✓ verified end to end · unit: covered by the adapter's tests (fed the harness's real message
shapes), not end to end · — the protocol has no way · ? not run against a real install.

| | Claude Code | Codex | ACP (claude-agent-acp) | Gemini / opencode / Goose / Amp | Cursor CLI | Pi |
|---|---|---|---|---|---|---|
| start | ✓ | ✓ | ✓ | ? | unit | unit |
| resume, with the history | ✓ | ✓ (latest 100 turns, paged) | unit (when the agent loads sessions) | ? | unit (`session/load`, as ACP loads sessions) | unit (`--session-id`) |
| survives server and runner restarts | ✓ | ? | ? | ? | ? | ? |
| a runner upgrade keeps its process (mid-turn, background commands) | ✓ | unit | unit | ? | unit | unit |
| send, queue | ✓ | ✓ | ✓ | ? | unit | unit |
| steer into a running turn | ✓ | ✓ | — (waits for the turn) | — | — (waits for the turn) | unit (its `steer`) |
| a queued message steered in (`queue_steer`) | ✓ | ✓ | — | — | — | unit |
| send now | ✓ | ✓ | unit | ? | unit | unit |
| interrupt | ✓ | ✓ | ✓ | ? | unit | unit (`clear_queue`, then `abort`) |
| approvals, always-allow, deny with feedback | ✓ | ✓ | ✓ (allow) | ? | unit (ACP's) | — (it asks before nothing) |
| questions: previews, free text, notes, labels with commas | ✓ | unit (Codex asks only in its Plan mode, which kiln does not run) | — | — | unit (`cursor/ask_question`: options by id, several at once; no free text) | unit (an extension's dialogs: a choice, yes or no, free text) |
| plans to approve | unit | — | — | — | unit (`cursor/create_plan`) | — |
| permission modes from the start (`meta.modes`), `set_mode` | ✓ | ✓ (interrupts and continues an active turn) | ✓ (the agent's modes) | ? | unit (its modes) | unit (tool sets; a new one starts it again) |
| `set_model` mid-session | ✓ | ✓ (from the next turn) | ? (sent as session/set_model) | ? | ? | unit (`set_model`) |
| effort from the start (`meta.efforts`), `set_effort` | ✓ | ✓ (from the next turn) | — | — | ? | unit (its model's thinking levels) |
| speed where the model goes faster (`meta.speeds`), `set_speed` | unit (fast mode, a setting of the session's alone; what keeps it off and its cooldown from each turn) | unit (each model's service tiers, with every turn) | unit (an agent's `speed` option, as Devin offers it) | ? | ? | — |
| one message's own model and effort | unit | ✓ | — | — | — | — |
| attachments: images inline, files by path | ✓ | ✓ | ? (images when offered) | ? | ? (images when offered) | unit |
| diffs, approval previews, cumulative changeset | ✓ | ✓ | ✓ | ? | ? | unit (an edit's replacements) |
| calls waiting behind a prompt read as pending | ✓ | — (Codex reports each call's own state) | — | — | — | — |
| subagents: tasks, nested messages, stop | ✓ | unit (read-only task and thread tracking; no direct stop) | — | — | — (its `cursor/task` notices aren't read) | — (it has none) |
| a person's message to a subagent: queue, steer, now | unit (by way of its agent's SendMessage; no now) | unit (v1 on its own thread; v2 by way of its agent) | — | — | — | — |
| rewind, branch, compact, rename | ✓ | ✓ | — | — | — | unit (compact) |
| a compaction shown as it goes on, then marked where it ran (`compacting`) | unit | unit | unit (`compaction_update`; Devin: its own `/compact`) | — (their ACP says nothing of one) | ? | unit (`compaction_start`, `compaction_end`) |
| restore files on rewind | ✓ | — | — | — | — | — |
| thinking display / budget, output style | ✓ | ✓ (reasoning summaries) | — | — | ? | unit (its thinking, its levels) |
| plan limits (`limits`) | ✓ none for an API key; unit for subscriber windows | ✓ (pushed during turns); unit for ChatGPT reads | — | — | — | — |
| harness status (`harness_status`) | ✓ | ✓ | unit | ? | unit | unit |
| MCP servers: list with tools, enable/disable, reconnect | ✓ | ✓ | — | — | — | — |
| MCP sign-in (OAuth, finished from another device) | ✓ | ✓ | — | — | — | — |
| MCP add / remove | ✓ (`claude mcp`) | ✓ (`codex mcp`, http via config) | — | — | — | — |
| skills: list with source, view, run, reload | ✓ | ✓ | run only | ? | run only | run only (`get_commands`) |
| plugins | ✓ | ✓ | — | — | — | — |
| MCP server and skill named on tool calls | ✓ | unit (MCP) | — | — | — | — |
| inventories known vs loading (`inventory`) | ✓ | ✓ | unit (unsupported) | ? | unit (unsupported) | unit (unsupported) |
| shells it leaves running as tasks (command, folder, left behind) | ✓ | ✓ (terminals) | — | — | — | — |
| task output, read on its machine, followed from an offset | ✓ | ✓ | — | — | — | — |
| stop a task / all tasks | ✓ (`stop_task`) | ✓ (kiln stops the process) | — | — | — | — |
| ending the agent stops what it left running | ✓ | ✓ | — | — | — | — |
| monitors: events counted, the latest kept | unit (not offered to API-key sessions) | — | — | — | — | — |
| pause / resume a task | — | — | — | — | — | — |
| take over a session found running in a kiln terminal | ✓ | ✓ | — | — | — | — |
| past sessions listed (`/api/harness/history`), continued with their history and model | ✓ | ✓ | — | — | — | — |
| command catalog: every command with its route, aliases | ✓ | ✓ | ✓ | ? | unit | unit |
| catalog commands kiln runs, answered to the asking console | ✓ (/btw /cost /export /plan /permissions /memory /git-diff) | ✓ (/goal /export /rollout /pwd) | ✓ (/export) | — | unit (/export) | unit (/export, /compact) |
| /branch, /fork, /undo from the catalog | ✓ (/branch) | ✓ (/fork, /undo) | — | — | — | — |
| refuse one running outside kiln | ✓ | ✓ | — | — | — | — |
| usage limit: pause once, queue and goals held, checkpoint | unit | unit | unit (limit words) | ? | unit (limit words) | unit (limit words) |
| resume at the reset after a usage check | unit (`get_usage`) | unit (`account/rateLimits/read`) | — (retries on a schedule) | ? | — (retries on a schedule) | — (retries on a schedule) |
