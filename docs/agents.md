# Agent sessions

kiln runs coding agents (Claude Code, Codex, ACP agents) as structured sessions and is their
front end: it starts the harness in its structured mode, turns everything the harness says into
one normalized model (timeline events, state, meta) and turns the console's commands into the
harness's own protocol. A session runs on any machine (the server, or a runner) and outlives the
browsers watching it.

| Harness | Protocol | Adapter |
|---|---|---|
| Claude Code | `claude -p` stream-json + control protocol | `harness/claude.rs` |
| Codex | `codex app-server` (JSON-RPC v2) | `harness/codex.rs` |
| Gemini CLI, opencode, Goose, Amp | Agent Client Protocol | `harness/acp.rs` |
| Antigravity CLI | none (runs in a kiln terminal) | — |

## API

| Method | Path | |
|---|---|---|
| GET | `/api/harness/kinds?target=` | the harnesses each machine offers (`KindInfo[]`: id, name, verified, available, modes, structured) |
| GET | `/api/harness/sessions` | `Info[]` |
| POST | `/api/harness/sessions` | `{target: "local" \| "agent:<name>", spec: Spec, count?}` → `Info[]` |
| GET / DELETE | `/api/harness/sessions/:id` | one session / close it for good |
| POST | `/api/harness/sessions/:id/command` | a `Command` → 204 |
| POST | `/api/harness/command` | `{ids, command}`: one command for several sessions |
| POST | `/api/harness/files?name=&mime=` | a file for a later message → `{file_id, size}` |
| GET | `/api/harness/sessions/:id/tasks/:task/output?since=` | a task's output, read on its machine: `{data, offset, live, truncated, path?, gone?}` or `{error}`; without `since` the last 64 KiB, with it what came after that byte (up to 256 KiB); ask again from `offset` to follow it |
| WS | `/api/harness/sessions/:id/ws` | updates (below); commands may be sent on it too |
| GET | `/api/ai-sessions` | the sessions found running on the machines (Claude Code, Codex, Gemini CLI started by hand), each with `machine`, `pid`, `tool`, `id`, `cwd`, `terminal` (the kiln terminal it runs in, while kiln holds it) and `takeover` |
| POST | `/api/ai-sessions/:machine/:pid/takeover` | take a found session over → `Info[]` (the new agent session); see below |

Updates (`t`): `open {info}` (a snapshot follows), `state {state}`, `meta {meta}`,
`events {events}` (upserts by id), `stream {id, text}` (text appended to a streaming event),
`changes {files}` (the whole changeset), `reply {client, value}` (the answer to one client's
request), `reset` (the timeline starts over), `closed`.

`Spec`: `kind`, `cwd?`, `model?`, `mode?`, `effort?`, `resume?` (the harness's own session id),
`resume_at?`, `fork?`, `prompt?`, `name?`, `shell?`, `allow_bypass?`, `takeover_pid?` (set by a
take-over: start only once that process has ended).

### Taking over a found session

A session found running in one of kiln's terminals (a Claude Code or Codex TUI someone started
there) can be continued as a kiln agent session. `takeover` on each found session says whether:

| `takeover` | meaning | the console |
|---|---|---|
| `"ready"` | it runs in a kiln terminal: taking it over ends that terminal and continues the conversation as an agent session | offer it |
| `"outside"` | it runs outside kiln: it must be quit there first | explain; once it has ended, resume it with `POST /api/harness/sessions` and `spec: {kind, cwd, resume: id}` |
| `"no_id"` | it has no conversation to continue yet (still starting) | wait |
| `"unsupported"` | kiln cannot run that harness as an agent session | — |

`POST /api/ai-sessions/:machine/:pid/takeover` (no body): kiln closes the terminal, starts an
agent session on that machine with the same harness, folder, conversation (its session id) and
model, and answers with its `Info[]` (status `starting`). The machine starts the harness only once
the old process has ended (up to 15 s; meanwhile `state.doing` says it is waiting); the session's
history is loaded from the conversation. Errors: 404 (not found: it may have ended), 409
(`outside`, `no_id`), 400 (`unsupported`).

## Commands

Every command is JSON with a `type`. Those that answer one client carry `client` (any string the
console picks) and are answered with a `reply` update to that client.

| type | fields | answer |
|---|---|---|
| `send` | `id, text, mode: "queue"\|"steer"\|"now", attachments?, overrides?: {model?, effort?}` | `state.delivery[id]`: queued → sent → started → completed / cancelled |
| `queue_edit` / `queue_remove` / `queue_move` / `queue_send_now` | `id` (+ `text` / `index`) | |
| `approve` | `id, decision: "allow"\|"always"\|"deny", message?, always?, answers?` | |
| `interrupt`, `restart`, `snapshot` | | |
| `set_mode` / `set_model` / `set_effort` | `mode` / `model` / `effort` | `state.mode` / `state.model` / `state.effort` |
| `stop_task` | `task` | the task's `status` (stopped) |
| `stop_tasks` | | every live task stopped |
| `suggest` | `client, query` | `{suggestions: [{path}], cwd}` |
| `control` | `id, value?` (a `meta.controls` entry) | |
| `mcp_enable` | `name, enabled` | `state.mcp` |
| `mcp_reconnect` / `mcp_refresh` | `name` / — | `state.mcp` |
| `mcp_auth` | `name, client, redirect?` | `{url, callback, redirect}` to open, or `{ok, message}`; send it again with `redirect: <the address the sign-in ended on>` to finish a sign-in from another device |
| `mcp_add` | `name, scope?, transport?, command?, args?, env?, url?, headers?, client?` | `{ok, message \| error}` |
| `mcp_remove` | `name, scope?, client?` | `{ok, message \| error}` |
| `skill_view` | `name, client` | `{content, path}`, `{content, builtin: true, source}` (no file), or `{error}` |
| `skill_run` | `name, args?, id?, mode?` | delivered like `send` |
| `skills_reload` | | `state.skills` |

## State

Besides the timeline-related fields (`status`, `approvals`, `queue`, `delivery`, `tasks`,
`todos`, `usage`, …):

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

`Info` (the lists) carries `limits`, `account`, `plan` and `left_behind` too.

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
| resume, with the history | ✓ | unit | unit (when the agent loads sessions) | ? |
| survives server and runner restarts | ✓ | ? | ? | ? |
| send, queue | ✓ | ✓ | ✓ | ? |
| steer into a running turn | ✓ | ✓ | — (waits for the turn) | — |
| send now | ✓ | ✓ | unit | ? |
| interrupt | ✓ | ✓ | ✓ | ? |
| approvals, always-allow, deny with feedback | ✓ | ✓ | ✓ (allow) | ? |
| questions | unit | unit | — | — |
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
| refuse one running outside kiln | ✓ | ✓ | — | — |
