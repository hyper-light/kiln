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
| Gemini CLI, opencode, Goose, Amp | Agent Client Protocol | `harness/acp.rs` |
| Antigravity CLI | `agy` headless stream-json, one turn per message on stdin (agy 1.4 or newer) | `harness/antigravity.rs` |

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

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/kinds?target=` | the harnesses each machine offers (`KindInfo[]`: id, name, verified, available, modes, structured) |
| GET | `/api/harness/sessions` | `Info[]` |
| POST | `/api/harness/sessions` | `{target: "local" \| "agent:<name>", spec: Spec, count?}` → `Info[]` |
| GET / DELETE | `/api/harness/sessions/:id` | one session / close it for good |
| POST | `/api/harness/sessions/:id/command` | a `Command` → 204 |
| POST | `/api/harness/broadcast` | one thing to many agents on every machine: a prompt or slash command, a command, or answers to what waits on you; see below |
| POST | `/api/harness/command` | `{ids, command}`: one command for the sessions listed → `{sent, report}` (the same sending as `/broadcast`) |
| POST | `/api/harness/files?name=&mime=` | a file for a later message → `{file_id, size}` |
| GET | `/api/harness/sessions/:id/tasks/:task/output?since=` | a task's output, read on its machine: `{data, offset, live, truncated, path?, gone?}` or `{error}`; without `since` the last 64 KiB, with it what came after that byte (up to 256 KiB); ask again from `offset` to follow it |
| WS | `/api/harness/sessions/:id/ws` | updates (below); commands may be sent on it too |
| GET | `/api/harness/sessions/:id/terminals` | the terminals beside it (its shell, its harness's own screen), live ones first → `[{terminal, what, command?, to_type?}]`; see below |
| GET / POST / PUT | `/api/harness/sessions/:id/goals` | its goal stack: goals one after another (docs/goals.md) |
| POST | `/api/harness/sessions/:id/goals/{start,pause,skip,clear}` | run, pause, skip the goal under way, or empty its goal stack |
| POST | `/api/harness/sessions/:id/terminals` | `{what: "shell" \| "screen", command?, cols?, rows?}`: open one beside it → 201 `{terminal, what, command?, to_type?}`; see below |
| GET | `/api/ai-sessions` | the sessions found running on the machines (Claude Code, Codex, Gemini CLI started by hand), each with `machine`, `pid`, `tool`, `id`, `cwd`, `terminal` (the kiln terminal it runs in, while kiln holds it), `takeover`, `fork` (a copy can be continued) and `managed_by?` (the agent session already continuing that conversation) |
| POST | `/api/ai-sessions/:machine/:pid/takeover` | take a found session over → `Info[]` (the new agent session); `{"stop": true}` for one running outside kiln; see below |
| POST | `/api/ai-sessions/:machine/:pid/fork` | continue a copy of a found session, leaving it running → `Info[]`; see below |
| GET | `/api/harness/history?target=&kind=&limit=` | a machine's past sessions to continue → `PastSession[]`, newest first; see below |
| GET | `/api/harness/sessions/:id/events?before=&limit=` | the session's history from the conversation store, beyond what its timeline holds → `{events}`, oldest first ([conversations.md](conversations.md)) |
| GET | `/api/conversations/search?q=&agent=&kind=&tool=&status=&since=&until=&limit=` | events across every agent's conversation, newest first → `{hits: [{agent, name, machine, event}]}` |

Updates (`t`): `open {info}` (a snapshot follows), `state {state}`, `meta {meta}`,
`events {events}` (upserts by id), `stream {id, text}` (text appended to a streaming event),
`changes {files}` (the whole changeset), `reply {client, value}` (the answer to one client's
request), `reset` (the timeline starts over), `closed`.

`Spec`: `kind`, `cwd?`, `model?`, `mode?`, `effort?`, `resume?` (the harness's own session id),
`resume_at?`, `fork?`, `prompt?`, `name?`, `shell?`, `allow_bypass?`, `takeover_pid?` (set by a
take-over: start only once that process has ended).

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

A session found running on a machine (a Claude Code or Codex TUI someone started, in one of
kiln's terminals or any other) can be continued as a kiln agent session. `takeover` on each found
session says whether:

| `takeover` | meaning | the console |
|---|---|---|
| `"ready"` | it runs in a kiln terminal: taking it over ends that terminal and continues the conversation as an agent session | offer it |
| `"stop"` | it runs outside kiln: taking it over ends it there (asked to quit, killed after 10 s), then continues the conversation as an agent session | offer it, saying so; confirm with `{"stop": true}` |
| `"outside"` | it runs outside kiln as an ACP agent, which might start anew rather than resume it: it must be quit there first | explain; once it has ended, resume it with `POST /api/harness/sessions` and `spec: {kind, cwd, resume: id}` |
| `"upgrade"` | the machine's runner is too old for this take-over | link to Settings → Updates |
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
    the default), `steer` (into it) or `now` (stopping it).
  - `command`: a command as the console sends one (`{"type": "interrupt"}`,
    `{"type": "set_mode", "mode": "plan"}`, `{"type": "restart"}`, ...). A `send` gets its own
    message id per agent.
  - `approvals`: `"allow"` or `"deny"`: answers every tool call waiting on you on each agent
    (questions are left: they need answers).

→ `{id, sent, skipped, failed, results: [{id, name, machine, kind, result, reason?}]}`, `result`
being `sent`, `skipped` or `failed`. Each broadcast is in the audit log.

### Signing harnesses in and out

`/login` and `/logout` open kiln's account card (the views `login` and `logout`). kiln runs the
harness's own CLI on the agent's machine, in a terminal it opens for the purpose (never listed),
and shows what a sign-in asks for:

- Claude Code (`claude auth login --claudeai|--console|--sso`): a page to open on any device, then
  the code that page shows, pasted back (a browser on the same machine may finish without one).
- Codex (`codex login --device-auth`): a page and a one-time code to enter there; it finishes by
  itself. Or an OpenAI API key (`--with-api-key`), never echoed.

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

`GET /api/harness/history?target=local|agent:<name>&kind=claude|codex&limit=50` lists the
conversations a machine keeps that are not running, read on that machine from its own
transcripts (Claude Code's `projects/` in `CLAUDE_CONFIG_DIR` or `~/.claude`, Codex's `sessions/`
in `CODEX_HOME` or `~/.codex`): newest first, `kind` for one harness, `limit` up to 200 (50 by
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
target. Leave `model` unset: both harnesses keep the conversation's own. Its history is in the
timeline when the session turns idle (Codex's latest 100 turns, listed with
`thread/turns/list`; a notice says when earlier ones are not shown). Claude Code writes a
`/rename` into its transcript with the next exchange: a name given just before the session ends
is not kept.

## Commands

Every command is JSON with a `type`. Those that answer one client carry `client` (any string the
console picks) and are answered with a `reply` update to that client.

| type | fields | answer |
|---|---|---|
| `send` | `id, text, mode: "queue"\|"steer"\|"now", attachments?, overrides?: {model?, effort?}` | `state.delivery[id]`: queued → sent → started → completed / cancelled |
| `queue_edit` / `queue_remove` / `queue_move` / `queue_send_now` | `id` (+ `text` / `index`) | |
| `queue_steer` | `id` | the queued message goes into the running turn as steering, without interrupting it (sent as the next turn when none runs); feature `"queue_steer"` (Claude Code, Codex; ACP cannot steer) |
| `approve` | `id, decision: "allow"\|"always"\|"deny", message?, always?, choices?` (or the older `answers?`) | see "Questions" below |
| `interrupt`, `restart`, `snapshot` | | |
| `set_mode` / `set_model` / `set_effort` | `mode` / `model` / `effort` | `state.mode` / `state.model` / `state.effort` |
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
| `pause_at` | `percent` (null: off) | stop between turns once a limit is this full (`state.pause_at`) |
| `limit_hold` | `window?, label?, until?, said?` | the server's: another session on the account reached a usage limit, so this one pauses until it resets too |

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
  request: the CLI binds the command to its terminal). Then the commands of its terminal
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
  only when a person resumes them.

`Info` (the lists) carries `limits`, `account`, `plan`, `left_behind` and `pause` too.

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
- **The whole account.** The server pauses every other session on the same account, on every
  machine (`limit_hold`), so they don't each find out by failing.
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
- **Stop point.** `pause_at` (the head's usage badge → Stop between turns: 80, 90 or 95 %) pauses
  the agent between turns once a limit is that full, before the limit cuts a turn off. Off by
  default.
- **Restarts.** A pause, its timer and its stop point are kept with the session on its machine
  (the runner's `harness.json`, the server's `local.json`) and go on after a restart.

Repeated identical notices in a turn (such as a limit said over and over by a build before this)
show once in the timeline, with a count.

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

`state.left_behind` counts the tasks left running while no turn is under way (the agent idle or
exited): shells, monitors, workflows, background calls and subagents sent to the background.

Ending an agent (`DELETE /api/harness/sessions/:id`) stops what it left running: each live task
through the harness (`stop_task`), then anything still under the harness process (commands that
left its process group included), then the harness. A console can warn first with `left_behind`.
No harness offers pausing a task: Claude Code reports a workflow it restored as "paused", with no
way to resume it from outside.

A tool call reads `pending` while it is asked about (an approval in `state.approvals` names its
`tool_id`) or while it waits its turn behind one; `running` once it is under way. MCP calls name
their `server` and `tool_short`; skill calls their `skill`.

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

## Capability matrix

Checked end to end against the real CLIs with scripted model APIs and isolated configuration
homes, on the server and through a runner: Claude Code 2.1.296 and Codex 0.162.1; ACP through
Zed's Claude agent adapter (claude-agent-acp), the only ACP agent run so far.

✓ verified end to end · unit: covered by the adapter's tests (fed the harness's real message
shapes), not end to end · — the protocol has no way · ? not run against a real install.

| | Claude Code | Codex | ACP (claude-agent-acp) | Gemini / opencode / Goose / Amp |
|---|---|---|---|---|
| start | ✓ | ✓ | ✓ | ? |
| resume, with the history | ✓ | ✓ (latest 100 turns, paged) | unit (when the agent loads sessions) | ? |
| survives server and runner restarts | ✓ | ? | ? | ? |
| send, queue | ✓ | ✓ | ✓ | ? |
| steer into a running turn | ✓ | ✓ | — (waits for the turn) | — |
| a queued message steered in (`queue_steer`) | ✓ | ✓ | — | — |
| send now | ✓ | ✓ | unit | ? |
| interrupt | ✓ | ✓ | ✓ | ? |
| approvals, always-allow, deny with feedback | ✓ | ✓ | ✓ (allow) | ? |
| questions: previews, free text, notes, labels with commas | ✓ | unit (Codex asks only in its Plan mode, which kiln does not run) | — | — |
| plans to approve | unit | — | — | — |
| permission modes from the start (`meta.modes`), `set_mode` | ✓ | ✓ (from the next turn) | ✓ (the agent's modes) | ? |
| `set_model` mid-session | ✓ | ✓ (from the next turn) | ? (sent as session/set_model) | ? |
| effort from the start (`meta.efforts`), `set_effort` | ✓ | ✓ (from the next turn) | — | — |
| one message's own model and effort | unit | ✓ | — | — |
| attachments: images inline, files by path | ✓ | ✓ | ? (images when offered) | ? |
| diffs, approval previews, cumulative changeset | ✓ | ✓ | ✓ | ? |
| calls waiting behind a prompt read as pending | ✓ | — (Codex reports each call's own state) | — | — |
| subagents: tasks, nested messages, stop | ✓ | ? (subagent activity is shown; not run) | — | — |
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
