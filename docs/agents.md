# Agent sessions

kiln runs coding agents (Claude Code, Codex, ACP agents, the Antigravity CLI) as structured sessions and is their
front end: it starts the harness in its structured mode, turns everything the harness says into
one normalized model (timeline events, state, meta) and turns the console's commands into the
harness's own protocol. A session runs on any machine (the server, or a runner) and outlives the
browsers watching it.

| Harness | Protocol | Adapter |
|---|---|---|
| Claude Code | `claude -p` stream-json + control protocol | `harness/claude.rs` |
| Codex | `codex app-server` (JSON-RPC v2) | `harness/codex.rs` |
| Devin, Gemini CLI, opencode, Goose, Amp | Agent Client Protocol | `harness/acp.rs` |
| Antigravity CLI | `agy` headless stream-json, one turn per message on stdin (agy 1.4 or newer) | `harness/antigravity.rs` |

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
and kept in the existing model cache for the start dialog. Shared effort choices are offered only
when every listed model
supports them. The start dialog uses the native default model's levels before a model is chosen,
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

## Where a harness is found

A machine offers a harness (`/api/harness/kinds`, the machines the new-agent dialog lists) when its
program is on the runner's PATH or on the PATH a login shell of the machine's user has, asked as
reach asks it for runs (`$SHELL -l -i`; as the runner starts, then kept ten minutes). A runner's
service keeps the PATH of the terminal it was installed from, so a harness installed since in a
folder that PATH lacks (Claude Code's and Codex's installers put theirs in `~/.local/bin`) is
found all the same, within a minute, without the runner starting again. The harness runs with
that PATH too: the runner's folders first, then those only the login shell has. An agent started
from a kiln shell (`kiln spawn`) has that shell's PATH, as it has the rest of its environment.

## What an agent left running

Every agent session's processes are held together by the system where it can (`crate::os::contain`):

| Where | How | What it gives |
|---|---|---|
| Linux with cgroup v2 the runner may write (a systemd user unit, a container with its own cgroup namespace) | a cgroup of its own, `kiln-agents/<session>` under the runner's | its harness joins it between fork and exec, so nothing it starts leaves it by forking, `setsid` or a double fork; its processes are its `cgroup.procs`; whose a process is, one read of `/proc/<pid>/cgroup`; ending them all, one write to `cgroup.kill` |
| Windows | a job object of its own, `Local\kiln-agent-<session>` | its harness is created suspended, put in the job, then let run; the job lists its processes, says whether it holds one, and ends them all; it ends with the runner, as every job kiln makes |
| macOS | followed fork by fork | its harness is watched with kqueue; whenever one of its processes forks, the children are listed and watched too; once a minute the user's processes are looked at for the agent's mark, for one that left between a fork and its listing |

Every harness kiln starts also carries `KILN_AGENT=<session id>` in its environment, and so does
every process it starts, however it's parented later: where the system holds nothing together
(cgroup v1, a read-only cgroup filesystem, a cgroup another user owns), and for sessions whose
harness started before the runner had groups, that mark is how their processes are found, which
takes looking at every process on the machine. The runner logs how it holds them as it starts
(`agents' processes are held together by`). Only the runner and the server, which start agents,
hold them together: a test or a tool that links kiln and starts a harness leaves its processes
where they are, so a test run in a kiln terminal makes no groups in the runner's cgroup.

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

When the runner starts again (an upgrade, a crash, the system stopping it, a re-join), a harness
that outlived it is stopped before the session resumes (what it started is kept), so two harnesses
never work one conversation; then whatever the session started that still runs is taken back over
the same way, and named to the agent if kiln tells it to pick up a turn the restart cut short;
it goes when that turn ends, as anything else left running.
Ending the agent stops what it left running, then ends whatever else is still in its group (what
left its harness's tree included) and removes the group. kiln's own helpers and MCP servers aren't
counted.

## Mission control

- **How they stand.** Under the page's title, one rule across the page has a length for each way
  your agents stand (needs you, stopped, paused, working, finished, ready, offline), as long as
  its share of them; under it, each one's count (and on a desktop its name). Working's length
  carries the prism flowing along it and needs you's brightens and dims; the rest hold still, and
  nothing moves with reduced motion or kiln's motion paused.
- **Order.** By default (Needs you first) the agents that wait on you (a request, or goals they
  paused for you) come first, longest waiting first, then those stopped, paused, finished since
  you last looked, working, ready, offline and ended. The list follows the agents as they change:
  one that comes to need you rises at once. While you use the list (a finger or button pressed on
  it, focus on one of its controls, a menu or dialog open, agents selected, or a moment after the
  pointer last moved over it), its rows hold still, then glide to their places. A sort chosen on
  purpose (by name, age, activity, runner, harness or cost) is followed as it is.
- **Requests anywhere.** On any other page (and an agent's page, for the other agents), a card
  says what each agent has for you, the most pressing first: its oldest request (a tool call is
  allowed or denied there, a plan it proposes approved there, **Review** to keep it planning with
  what to change; a question opens the agent); goals it paused for you
  (**Review the plan**, when they wait on one, and **Resume**); a stop of its own, its harness
  ended with an error (**Restart**); a usage limit it reached, only said, with when it goes on.
  Two at once (one on a phone), then a line for the rest. Where kiln's other notifications are,
  on every screen: at the bottom, above any passing ones. ×, or a swipe, puts one away until
  there's something new.
- **Answers at once.** An answer shows as given wherever its request showed (its row, its card, the
  agent's page) before the agent has it; if it doesn't go, the request is back, saying why.
- **Selecting by touch.** On a touch screen rows have no checkbox: press and hold a row to select
  it (and drag over more), tap rows while selecting, or swipe one right; the list's head says how
  many, with Select all and Done. With a mouse, rows keep their checkboxes.
- **The message box.** It writes to the agents selected (each named in its To line, put back with
  its ×), or with none selected to every agent the list shows; @ and part of a name, machine or
  harness finds more. A prompt goes with files (the paperclip, a paste or a drop) and what you say
  into the mic, queued, steered or sent now as an agent's own box sends it. Above it, the agent
  page's line of settings, permissions · model · effort, each opened in place with the same
  controls: unset, each agent keeps its own; chosen, only the agents that have it are sent to, and
  given it first (Reset unsets them all). In the field, / lists kiln's own commands for all of them
  first (`/interrupt`, `/restart`, `/allow` and `/deny` what waits on you, `/end` those selected),
  each asked once under the field, then the agents' own; Stop beside Send stops the turns of those
  working. It empties as it sends, and says how it went.

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/kinds?target=` | the harnesses each machine offers (`KindInfo[]`: id, name, verified, available, modes, structured) |
| GET | `/api/harness/sessions` | `Info[]` |
| POST | `/api/harness/sessions` | `{target: "local" \| "agent:<name>", spec: Spec, count?}` → `Info[]` |
| GET / DELETE | `/api/harness/sessions/:id` | one session / close it for good |
| POST | `/api/harness/sessions/:id/command` | a `Command` → its runner's answer: 204 taken, 202 `{parked: true}` (its machine is away and it waits for it), 404 when its runner doesn't have the session, 409 for another refusal (how and why) or a machine that's away for one refused while away, 504 when its link dropped before it answered (docs/wire.md, "Commands"); for a message (a `send`, a skill run with its `id`) its inbox's answer: 200 `{sent, message, accepted}` (`accepted.repeat` when its id was taken before), or a typed refusal (409 for an epoch the agent left, a machine it moved from, or an id given to other words). A message's `id` is its idempotency key; an `Idempotency-Key` header makes it from that instead (docs/sends.md), and takes any other command once under it |
| POST | `/api/harness/broadcast` | one thing to many agents on every machine: a prompt or slash command, a command, or answers to what waits on you; see below |
| POST | `/api/harness/command` | `{ids, command, key?}`: one command for the sessions listed → `{sent, report}` (the same sending as `/broadcast`); each agent's message id is made from `key` (or the `Idempotency-Key` header, else the command's own `id`) and the agent, so sending it again sends the same messages |
| POST | `/api/harness/files?name=&mime=` | a file for a later message → `{file_id, size}` |
| GET | `/api/harness/sessions/:id/tasks/:task/output?since=` | a task's output, read on its machine: `{data, offset, live, truncated, path?, gone?}` or `{error}`; without `since` the last 64 KiB, with it what came after that byte (up to 256 KiB); ask again from `offset` to follow it. Native Codex subagents return `reset: true`: replace previous output with this snapshot (last 64 KiB), regardless of `since`; `pending: true` means a live child has not produced output yet |
| WS | `/api/harness/sessions/:id/ws?z=gzip&patch=state` | updates (below); commands go on it too, each under an id and answered `ack` or `nack` (docs/wire.md, "The UI's sockets") |
| GET | `/api/harness/sessions/:id/terminals` | the terminals beside it (its shell, its harness's own screen), live ones first → `[{terminal, what, command?, to_type?}]`; see below |
| GET / POST / PUT | `/api/harness/sessions/:id/goals` | its goal stack: goals one after another (docs/goals.md) |
| POST | `/api/harness/sessions/:id/goals/{start,pause,skip,clear,restore}` | run, pause, skip the goal under way, empty its goal stack, or bring back the goals cleared last |
| POST | `/api/harness/sessions/:id/terminals` | `{what: "shell" \| "screen", command?, cols?, rows?}`: open one beside it → 201 `{terminal, what, command?, to_type?}`; see below |
| GET | `/api/ai-sessions` | the sessions found running on the machines (Claude Code, Codex, Devin, Gemini CLI started by hand), each with `machine`, `pid`, `tool`, `id`, `cwd`, `terminal` (the kiln terminal it runs in, while kiln holds it), `takeover`, `fork` (a copy can be continued) and `managed_by?` (the agent session already continuing that conversation) |
| POST | `/api/ai-sessions/:machine/:pid/takeover` | take a found session over → `Info[]` (the new agent session); `{"stop": true}` for one running outside kiln; see below |
| POST | `/api/ai-sessions/:machine/:pid/fork` | continue a copy of a found session, leaving it running → `Info[]`; see below |
| GET | `/api/harness/history?target=&kind=&limit=` | a machine's past sessions to continue → `PastSession[]`, newest first; see below |
| GET | `/api/harness/sessions/:id/events?before=&limit=` | the session's history from the conversation store, beyond what its timeline holds → `{events}`, oldest first ([conversations.md](conversations.md)) |
| GET | `/api/conversations/search?q=&agent=&kind=&tool=&status=&since=&until=&limit=` | events across every agent's conversation, newest first → `{hits: [{agent, name, machine, event}]}` |

Updates (`t`): `open {info}` (a snapshot follows), `state {state}`, `meta {meta}`,
`events {events}` (upserts by id), `stream {id, text}` (text appended to a streaming event),
`changes {files}` (the whole changeset), `reply {client, value}` (the answer to one client's
request), `reset` (the timeline starts over), `synced` (the snapshot is complete), `closed`.

On a slow link most of what a long conversation sends is its snapshot, and during a turn its
state changes all the time while most of it (skills, plugins, MCP servers) stays the same. A
viewer can ask for less of both, in the socket's query:

- `z=gzip`: each snapshot comes as one binary message, gzip'd: its frames a line each, `synced`
  last. A long conversation's arrives in about a tenth of the bytes.
- `patch=state`: after the snapshot's whole `state`, a state comes as only what changed in it:
  `state_patch {set, gone?}`, the top-level fields set, and those it no longer has.

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
| An agent | itself and the agents it started (and theirs), to steer, interrupt and end; it sees (lists, reads, watches) the other agents that work for its person, and may send them queued messages ("Messages between agents") |
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

Mission Control lists every agent kiln runs, in one flat list. An agent started through kiln's API
is a top-level agent, whoever started it: a row of its own, in the counts, filters, conversation
search and bulk actions, with no nesting under the agent that started it. A harness's own
subagents stay subagents: they aren't rows, and their status and output stay inside their agent's
chat, including after they finish.
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
| `agents.send` | `POST /api/harness/agents/:id/send` | `{text, mode?: "queue" \| "steer" \| "now", key?}` (or the `Idempotency-Key` header) → `{sent, message, accepted?}`: `accepted` is the agent's inbox's answer, `repeat` on every one after the first ("Sending again", below) |
| `agents.wait` | `GET /api/harness/agents/wait` | `agent?` or `agents?` (a list; over HTTP `a,b`), `until?: "settled" \| "change"`, `any?`, `timeout_s?`, `hand_over?` → `{done, timed_out, waited_ms, until, agents: [Agent], messages?: [{from, name, text, at}]}` (`messages`: what other agents sent the agent that waits, handed over: "Waiting") |
| `agents.result` | `GET /api/harness/agents/:id/result` | → `{agent, reply: {text, at}, last_turn, changes: {files: [{path, added, removed, created, deleted}], files_changed, lines_added, lines_removed}, cwd, machine, cost_usd, workspace, branch, change_request}` |
| `agents.children` | `GET /api/harness/agents/:id/children` | → `{children: [{id, name, kind, status, parent}]}`: the ones still running, and theirs |
| `agents.interrupt` | `POST /api/harness/agents/:id/interrupt` | → `{interrupted}` |
| `agents.stop` | `POST /api/harness/agents/:id/stop` | `{children?}` → `{ended: [id]}`: with `children`, what it started (and theirs) end first |
| `agents.change` | `POST /api/harness/agents/:id/change` | `{title?, body?, draft?, trunk?, fix_checks?}` → `{change_request: Change}`: opens its change request from its work, or pushes a new version ("Change requests from agents", below) |
| `agents.changes` | `GET /api/harness/changes` | `agent?` → `{changes: [Change]}`, newest first |
| `agents.workspace` | `GET /api/harness/agents/:id/workspace` | → `{workspace: {kind, over, sub, volume, mount} \| null, drift: [path]}` ("Workspaces of their own", below) |
| `agents.land` | `POST /api/harness/agents/:id/land` | `{}` → `{plan, excludes, can_write, why_not?}`; `{manifest}` (that plan's; a person only) → `{landed}`, or `{changed, plan, …}` with nothing written when the plan isn't that one any more |

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
| `working` | a turn is under way, or a message waits in its queue or was just sent to it |
| `idle` | its turn is over, nothing queued |
| `needs_you` | an approval, a question or a plan waits on a person (`needs` lists them; only a person answers them) |
| `paused` | its account reached a usage limit |
| `stopped` | it crashed or couldn't start (`exit` says why) |
| `offline` | its machine is away; it goes on when it's back |
| `ended` | it ended, or kiln doesn't list it any more |

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
gets it once, and every call is answered as the first was, `accepted.repeat` on the ones after.
A key is its caller's: an agent's is the same on whichever machine it runs, so its retry after it
moves is the same message. Without a key, each call is a message of its own. A key given to other
words is refused (409). From a kiln shell: `kiln agents send <id> --key <key> <text>`.

### Messages between agents

Agents that work for the same person can talk to each other, whoever started them: any of them may
send any other a queued message (`agents.send`, `queue`). Agents a person started apart (one on
kiln, one on Mantle, one on Focal) work things out between them, without the person carrying notes.
The message reaches its receiver after its turn, as one from a fellow agent, never as its person's:
"Agent “Mantle” (01a14887, claude on adas-macbook-pro-2.local), which works for the same person as
you, sent this message. It's from a fellow agent, not from your person. To answer, send it a
message: agents_send {"agent": "…", "text": …}." A receiver in a long turn, waiting for its
children (`agents_wait`), gets it at once, with the wait ("Waiting", below). Control stays with
the agents an agent started: steering into a turn, sending now, interrupting, ending, and their
goals and plan runs are theirs alone. Agents that work for different people aren't sent to.

### No more than its parent

| Rule | |
|---|---|
| Its mode | at most its parent's, in one order for every harness: plan < ask < accept edits < auto < bypass. Asked for more: refused, saying which modes it may have. Asked for nothing: its harness's default when that's within its parent's, else the most its harness has that is (always given, so the harness's own settings can't start it higher). |
| Bypass | only under a parent in bypass that was itself allowed it (`allow_bypass`); a child of a parent without it can't be switched to bypass either |
| Its reach | a parent whose commands run only on its own machine (`reach` off) has children that run there too, with reach off |
| Depth | at most `[agents] max_depth` (3) agents deep below the one a person started |
| Children | as many subagents at work at once as its own settings say (Subagents → At once), one pool whatever each does: the agents it started that work, wait on a person or are held to a goal still to go on a stack that runs, and its harness's own still going; one idle with its work done, or ended, holds no place. None set: no limit, unless kiln's config sets `[agents] max_children` for every agent |
| Messages | an agent may send a queued message to any agent that works for the same person, whoever started it (`agents.send`, `queue`): its receiver is told who sent it, that it isn't from its person, and how to answer ("Messages between agents") |
| Steering | an agent steers into a turn, sends now, interrupts and ends only itself and the agents it started (and theirs); the same holds for their goals (`goals.add`, `goals.set`, `goals.control`, and the console's goal routes when an agent calls them) and for running a plan on an agent that runs (`plans.run` with `session`); it stops and retries only the plan runs it ran, or that the agents it started ran (`plans.runs.stop`, `plans.runs.retry`) |
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
in a kiln terminal (run by an agent, what they start are its children):

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

An agent can work in **its own Slates volume** instead of the folder itself (New agents →
Workspace → Its own volume; `agents.start` with `workspace: "volume"`) (docs/agentic.md, W):

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
title, state, review?, checks?, fix_checks, opened_at, updated_at, stack?}`: `provider` is `kiln`,
`github`, `gitlab`, `bitbucket` or `git`; `change` is `CR12`, `D12` or `#34` (none until a pull
request is opened); `state` is `open`, `landed` or `closed`; `stack` (`{run, step}`) says it is a
step of a plan run's stack. Mission Control shows each agent's change
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
  comment), `schedule` (`schedule`, five-field cron, UTC); on runners, `offline` (gone two
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
  carried into (Claude Code ⇄ Codex), signed in with room, with the first model it lists there.
  Its own harness comes first, then a machine with a copy of its repository, then the emptiest
  account; a repository not there yet is copied over and kept in step. It moves as a move you ask
  for does (`docs/reach.md` §8), and goes on there unpaused, the cut-off turn first. With nowhere
  to go its timeline says why, and kiln looks again every few minutes. A runner that doesn't
  take a choice (a kiln from before it) leaves it as it was, and the page says so after a few
  seconds, with the way to upgrade it; one from before a machine could be chosen offers only
  "wherever has room".
- **Go on now.** The paused line above the composer: resume now, resume at the reset or not, or
  go on now: here in another harness (Claude Code ⇄ Codex), or on another machine that takes
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

### A machine on battery

A laptop's runner says how its battery stands about once a minute (`AgentMsg::Power`). On macOS it
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
| ACP agents, Antigravity CLI | kiln has no subagent threads for them | none |

What kiln asks an agent to pass on carries the person's words as written (the files attached by
their paths) and is kiln's own message in its timeline (`kiln-relay-<message id>`); a turn that
took it and ended without passing it on leaves the message not sent, saying so.

Codex's native subagents keep their own threads and execution settings. kiln tracks each one by
its native thread ID and keeps its work under the original spawning call, including later
messages and follow-up work. Read-only `thread/read` and `thread/turns/list` requests fill in its
status, latest 100 turns, elapsed time and result; live child notifications update that same
view. Tracking never starts, resumes or attaches another executor to the child (it may interrupt
one working with other permissions than the session's: see permission modes above). Its messages,
commands, usage and edits stay separate from the parent's turn and changeset. A spawn call
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
of them kept to read; a new process starts with nothing of an earlier one's running. What kiln
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
- **ACP agents** report no MCP servers, skills or plan limits; their slash commands run with
  `skill_run`.
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

✓ verified end to end · unit: covered by the adapter's tests (fed the harness's real message
shapes), not end to end · — the protocol has no way · ? not run against a real install.

| | Claude Code | Codex | ACP (claude-agent-acp) | Gemini / opencode / Goose / Amp |
|---|---|---|---|---|
| start | ✓ | ✓ | ✓ | ? |
| resume, with the history | ✓ | ✓ (latest 100 turns, paged) | unit (when the agent loads sessions) | ? |
| survives server and runner restarts | ✓ | ? | ? | ? |
| a runner upgrade keeps its process (mid-turn, background commands) | ✓ | unit | unit | ? |
| send, queue | ✓ | ✓ | ✓ | ? |
| steer into a running turn | ✓ | ✓ | — (waits for the turn) | — |
| a queued message steered in (`queue_steer`) | ✓ | ✓ | — | — |
| send now | ✓ | ✓ | unit | ? |
| interrupt | ✓ | ✓ | ✓ | ? |
| approvals, always-allow, deny with feedback | ✓ | ✓ | ✓ (allow) | ? |
| questions: previews, free text, notes, labels with commas | ✓ | unit (Codex asks only in its Plan mode, which kiln does not run) | — | — |
| plans to approve | unit | — | — | — |
| permission modes from the start (`meta.modes`), `set_mode` | ✓ | ✓ (interrupts and continues an active turn) | ✓ (the agent's modes) | ? |
| `set_model` mid-session | ✓ | ✓ (from the next turn) | ? (sent as session/set_model) | ? |
| effort from the start (`meta.efforts`), `set_effort` | ✓ | ✓ (from the next turn) | — | — |
| speed where the model goes faster (`meta.speeds`), `set_speed` | unit (fast mode, a setting of the session's alone; what keeps it off and its cooldown from each turn) | unit (each model's service tiers, with every turn) | unit (an agent's `speed` option, as Devin offers it) | ? |
| one message's own model and effort | unit | ✓ | — | — |
| attachments: images inline, files by path | ✓ | ✓ | ? (images when offered) | ? |
| diffs, approval previews, cumulative changeset | ✓ | ✓ | ✓ | ? |
| calls waiting behind a prompt read as pending | ✓ | — (Codex reports each call's own state) | — | — |
| subagents: tasks, nested messages, stop | ✓ | unit (read-only task and thread tracking; no direct stop) | — | — |
| a person's message to a subagent: queue, steer, now | unit (by way of its agent's SendMessage; no now) | unit (v1 on its own thread; v2 by way of its agent) | — | — |
| rewind, branch, compact, rename | ✓ | ✓ | — | — |
| restore files on rewind | ✓ | — | — | — |
| thinking display / budget, output style | ✓ | ✓ (reasoning summaries) | — | — |
| plan limits (`limits`) | ✓ none for an API key; unit for subscriber windows | ✓ (pushed during turns); unit for ChatGPT reads | — | — |
| harness status (`harness_status`) | ✓ | ✓ | unit | ? |
| MCP servers: list with tools, enable/disable, reconnect | ✓ | ✓ | — | — |
| MCP sign-in (OAuth, finished from another device) | ✓ | ✓ | — | — |
| MCP add / remove | ✓ (`claude mcp`) | ✓ (`codex mcp`, http via config) | — | — |
| skills: list with source, view, run, reload | ✓ | ✓ | run only | ? |
| plugins | ✓ | ✓ | — | — |
| MCP server and skill named on tool calls | ✓ | unit (MCP) | — | — |
| inventories known vs loading (`inventory`) | ✓ | ✓ | unit (unsupported) | ? |
| shells it leaves running as tasks (command, folder, left behind) | ✓ | ✓ (terminals) | — | — |
| task output, read on its machine, followed from an offset | ✓ | ✓ | — | — |
| stop a task / all tasks | ✓ (`stop_task`) | ✓ (kiln stops the process) | — | — |
| ending the agent stops what it left running | ✓ | ✓ | — | — |
| monitors: events counted, the latest kept | unit (not offered to API-key sessions) | — | — | — |
| pause / resume a task | — | — | — | — |
| take over a session found running in a kiln terminal | ✓ | ✓ | — | — |
| past sessions listed (`/api/harness/history`), continued with their history and model | ✓ | ✓ | — | — |
| command catalog: every command with its route, aliases | ✓ | ✓ | ✓ | ? |
| catalog commands kiln runs, answered to the asking console | ✓ (/btw /cost /export /plan /permissions /memory /git-diff) | ✓ (/goal /export /rollout /pwd) | ✓ (/export) | — |
| /branch, /fork, /undo from the catalog | ✓ (/branch) | ✓ (/fork, /undo) | — | — |
| refuse one running outside kiln | ✓ | ✓ | — | — |
| usage limit: pause once, queue and goals held, checkpoint | unit | unit | unit (limit words) | ? |
| resume at the reset after a usage check | unit (`get_usage`) | unit (`account/rateLimits/read`) | — (retries on a schedule) | ? |
