# Reach: commands on other runners

An agent, a person or a script on one runner runs commands on another — a Linux x86-64 build
from a Mac, a test on the machine with the GPU, `uptime` on a box by name — over an end-to-end
encrypted link the kiln server only relays.

```text
kiln run --needs linux/amd64 -- cargo test           # this machine if it is one, else the idlest; this repo synced over first
kiln run --needs cpus=8,mem=32g,linux/amd64 -- make  # one with 8 cores idle and 32 GiB free
kiln run --on adacachy --no-sync -- uptime           # a runner by name, in its home folder
kiln runners linux                                   # the Linux runners, best first, and why others don't suit
```

Agents get the same through kiln's MCP server (§7): one command at a time (`runners`, `run`,
`output`, `stop`), or a program that runs commands on several machines at once and returns only what
matters (`code_run`), so Claude Code, Codex or any harness that takes MCP servers can decide for
itself to build or test elsewhere.

## 1. Pieces

| Where | What | Code |
| --- | --- | --- |
| every runner | the node: its links, the runs it follows elsewhere, the runs it does for others | `reach/node.rs` |
| every runner | its identity and the keys it pinned | `reach/e2e.rs` |
| every runner | the local socket `kiln run` and the MCP tools talk to | `reach/local.rs` |
| the server | the relay: forwards a link's frames between its two runners | `reach/relay.rs` |
| the server | the directory: every runner, what it is, how busy, its identity | `hub.rs` `reach_directory` |
| both | picking a runner for what the work needs | `reach/directory.rs` |

Runners announce `CAP_REACH`, send their identity (`AgentMsg::ReachHello`) after Hello, and talk
on their own lane (`LANE_REACH`, interactive class, so output never queues behind job logs).

## 2. Picking a runner

`--needs` (or the tools' `needs`) says what the work needs, in the spelling CI's jobs use too
(`kiln_ci::sched::needs`): what the runner must be and have — `linux/amd64`, `macos`, `arm64`,
`+gpu` (a label), `tool:cargo`, `executor:docker`, `flag:avx2`, `binfmt:linux/arm64`, `cpus>=16`
(cores in all), `mem>=32g` (memory free), any fact routing knows (`disk.free>=50Gi`,
`custom.rack==a3`) — and the room it asks for, idle and free on the runner as it's picked:
`cpus=8`, `mem=32g`, `disk=50g`, `gpus=1`; `exclusive` takes a runner with no CI work on it. Reach
holds nothing for the work: the room is what's idle then. Words are separated by commas or spaces,
and one that isn't a need is an error that names it (`memm=4g: no such need`). Aliases are
understood (`amd64`, `x64` → `x86_64`; `arm64` → `aarch64`; `darwin`, `mac` → `macos`). The
constraints are checked against the facts each runner reports (docs/ci.md W13-R01), its platform,
cores and memory from its latest sample; one that hasn't reported facts (an older runner) meets
only those.

`--on` (or the tools' `on`) names a runner, for work that has to happen on that machine; it must
still meet `--needs`. It takes what `--needs` does too, as it did before needs, and `harness=codex`
and `-name` to exclude one. A runner older than the needs spelling reads a word it doesn't know as a
runner's name, so it refuses such a run rather than running it where it doesn't suit.

Of the connected runners that take reach links and suit, the asking machine itself comes first
while it has 4 GiB of memory free beyond what the work asks and isn't overloaded (nothing crosses the network — it then runs
locally — and another machine's resources stay its own): work goes elsewhere only when it asks for
what this machine isn't or hasn't (another platform, a GPU, more cores or memory). Of the others,
the one with the most idle cores wins (by its busy share or its load average, whichever leaves
fewer: work waiting on disk or memory shows in its load), then free memory; CI work on it and a
tunnelled path (HTTP instead of UDP) count against it, and the asking machine wins a tie when it's
short of memory. A machine that's overloaded (under 2 GiB or a twentieth of its memory free, or a
load past twice its cores) gets work only when no other suits, and says so (`· overloaded`). The
directory comes from the server (cached 30 s, refreshed after every reconnect); the choice is the
asking runner's. `GET /api/reach/runners?want=…` shows the same ranking.

## 3. Security

### 3.1 End to end

Two runners talk TLS 1.3 to each other *inside* the relay, so the server forwards ciphertext:

- key exchange: hybrid post-quantum only — ML-KEM-1024 with ECDH P-384 first, ML-KEM-768 with
  X25519 second (hyper-raft's node groups); a classical-only peer finds nothing in common;
- records: AES-256-GCM only, a key per direction; no session tickets, no 0-RTT: every link is a
  full handshake with fresh keys;
- authentication: both sides present their **reach identity** and each accepts exactly the key it
  expects for that runner (`e2e::Expect`).

A runner's reach identity is a P-384 key generated on the machine (`<state>/reach/identity.key`,
`0600`, never sent anywhere) in a long-lived self-signed certificate naming the runner. Its
fingerprint is the SHA-256 of the key's SubjectPublicKeyInfo.

### 3.2 Trust

The first time two runners meet, each trusts the key the server's directory lists for the other,
and **pins** it (`<state>/reach/pins.json`). From then on the pin wins: if the server later lists
another key for that runner — a compromised server, a swapped machine — the link is refused with
a message saying so, and nothing is sent. After reinstalling a runner, `kiln reach forget <name>`
on the others drops its pin (its next key is pinned on first use).

What this protects: the content of commands, their input and output, and synced files, from the
server, from anyone on the network, and from a server compromised *after* two runners first met.
What it does not: at first contact a malicious server could hand out its own key (trust on first
use); compare fingerprints (`kiln reach status`) where that matters. And what an agent does with a
remote command's output enters its conversation, which kiln shows through the server like any
other: reach encrypts the runner-to-runner path, not the agent's transcript.

### 3.3 Who may run what

- The server relays only between runners that are not job pods, and only to a runner that
  announced it accepts links; frames of a link are accepted only from its two ends' connections,
  stamped with the sender's certificate name.
- A runner takes commands from others unless started with `--no-reach` (or `KILN_NO_REACH`), and
  never when its terminals are off (`--no-terminal`): a command from another runner is a
  terminal's worth of access.
- An agent's own permission mode applies: a reach tool call or a `kiln run` in its shell asks
  first, like any command, unless the person chose a mode that doesn't.

## 4. The link

Inside the TLS stream: postcard messages behind a 4-byte length (`reach/wire.rs`). The side that
opened the link asks (`Ask`), the other does the work and answers (`Say`).

```text
asker                                         doer
  Hello{session, resume:[(run, from)]}  ──▶
                                        ◀──   Welcome{host, resumed, lost}
  Run{id, command, cwd, mirror, env, …}  ──▶   (spawns: bash, its own process group, the login
                                        ◀──    shell's environment)
                                        ◀──   Started{pid, cwd}
                                        ◀──   Output{id, offset, stream, bytes}   (window: 4 MiB unacked)
  Ack{id, upto}                          ──▶
                                        ◀──   Exit{id, code, error, end, lost}
  Forget{id}                             ──▶
```

### 4.1 Drops

A run outlives its link. Its output is numbered by byte offset; the doer keeps what was not
acknowledged (up to 16 MiB, then the command itself waits, like a full pipe). When the asker's
link drops — its server connection, the server, the other runner's — it opens a new link (backoff
200 ms → 5 s), names its session and how far each run got, and the doer replays from there. A run
waits for its asker 15 minutes without a link (`linger`), then is stopped. A link quiet for 15 s
is pinged; at 45 s it is replaced.

Nothing on the runner waits on reach (its input is unbounded, bounded in practice by the windows),
and reach never waits on the runner: output to the server is batched, many messages per frame.

### 4.2 Runs

Shell text, run by `bash -c` (`sh` where there is none; on Windows, Git for Windows' bash, found on
PATH, beside the `git` on PATH or where it installs, and never WSL's `bash.exe`, which would run the
command in Linux and, with no Linux installed, hangs: without a bash the run fails at once, saying
so) in its own process group, with the
environment a login shell of the machine's user has (resolved once, kept 10 minutes; the PATH
people's tools live on), plus `KILN_REACH_FROM=<asking runner>`. A named shell (`--shell`, the
tool's `shell`) carries its working folder and exported variables from one run to the next.
Signals: Ctrl-C on `kiln run` goes over as SIGINT; a client that goes away stops its run
(SIGTERM, then SIGKILL after 5 s) unless it said `--detach`. `--timeout` stops it too.

## 5. Sync

Before a run, the asker copies its working tree to the other runner (on by default inside a git
repository, `--no-sync` to skip):

- a git repository as git sees it: tracked files, untracked-but-not-ignored ones (so `target/`,
  `node_modules/` stay home), and `.git` itself, so `git status`, `git diff`, `git log` agree over
  there. A worktree becomes a repository of its own in the copy: the shared objects and refs, with
  the worktree's HEAD, index and logs, and none of the other worktrees;
- any other folder: everything but the usual build output.

The copy lives at `~/.kiln/reach/<asking runner>/<tree>-<hash>` and is kept, so the next sync
sends only what changed: the asker sends the list (path, size, time, mode), the doer asks for the
files that differ, the asker sends them (256 KiB pieces, at most 4 MiB unacknowledged), and on
commit the copy is made to match the list: what was sent before and is no longer listed goes,
while what was never sent (untracked and ignored build output such as `target/`, `ui/dist` and
`node_modules`) stays, so builds over there stay incremental (docs/ci.md W24-R24). The list last
applied is kept beside the copy (`<copy>.kiln-sent`), never in it; a copy without one is made to
match exactly, once. The command then runs in the copy's folder matching
where the asker was. Paths are checked (relative, plain, never through a symlink) so a list can't
write outside the copy.

Measured on this repository (8,100 files, 36 MB, through the relay): first sync 2.5 s, an
unchanged tree 0.5 s.

## 6. Local socket

`<runtime dir>/kiln/reach-<runner>-<hash>.sock` (`0600` in a `0700` folder), beside `kiln spawn`'s.
Terminals and agents kiln starts get it as `KILN_REACH_SOCK`; elsewhere `kiln run` finds the only
one on the machine. A socket another live kiln process answers on is never taken, but by a new
build taking the machine over; one left dead is bound again within seconds (`harness::socket::Kept`,
docs/releases.md). One connection is one request (`local::Req`) answered by a stream of
`local::Ev`. `Req::Ask` carries an agent's question for the kiln server (kiln's operations, §7)
over the runner's own connection (`AgentMsg::ReachAsk`), answered with `Ev::Answer`.

## 7. Agents

kiln gives every agent it starts on a runner its own MCP server, `kiln` (`kiln reach mcp`, run by
the runner's kiln): Claude Code through `--mcp-config` (with kiln's skills through `--plugin-dir`),
Codex through `-c mcp_servers.kiln.*` (with a tool timeout as long as a program may run, where Codex
otherwise gives up after 60 s), and ACP agents (Gemini CLI, opencode, Goose, Amp) through their
session's `mcpServers`. Antigravity's headless mode takes no MCP servers; its agents use `kiln run`
from their shell. Its instructions name the kiln's machines (asked of the runner when the server
starts), tell the agent who it is (its id in kiln) and how its goal stack and plans work, and tell
the model when reach helps. When a step has to happen on another machine, such as
the person's Mac, the agent runs it there itself rather than handing the person a command to type.
When the step needs a person at that machine (a password, a system dialog to allow), the agent
starts it and says what will appear. Reach also serves builds and tests that need another platform,
a GPU, or more memory, disk or cores than the agent's machine has; everything else the agent runs
on its own machine, with its own tools. Its tools:

| tool | does |
| --- | --- |
| `code_run` | a JavaScript program against kiln (below); only what it returns comes back |
| `code_search` | a program over the operations a program may call, and kiln's skills |
| `runners` | the machines, best first for `want` |
| `run` | a command on `on` (default: the session's preference, else this machine), synced first; returns exit code and output (the middle cut past 30k characters), or an id with `background`. What it leaves running (`nohup … &`, a daemon) ends with it: everything in its own group of processes where the machine holds them (a cgroup on Linux, however parented), else its process group. A server runs as a background run; a job meant to outlive it, as a systemd user unit (`systemd-run --user`), which leaves the run's cgroup and is left alone |
| `output` | a background run's new output, and whether it ended |
| `stop` | stop a background run |
| `agents_*` | kiln's agents operations (docs/agents.md, "Agents that run agents"): what it starts is its child |
| `goals_*` | its goal stack (docs/goals.md): without `agent`, its own |
| `research_*`, `conversations_search` | research it records, each finding graded by kiln from its sources (docs/research.md), and every agent's conversations kiln keeps |
| `plans_*` | plans kept, run (on itself without `session` or `new`; one agent per step, in its harness, on its machine and in its folder unless it says otherwise) and followed (docs/plans.md); a plan's versions and deleting one are code mode's |

The `agents_*`, `goals_*`, `research_*` and `plans_*` tools are the ones the kiln server says agents
have (its answer for the operations names them), each as the server describes it, so a server
deployed with new ones offers them to agents on runners not yet upgraded. Every minute the MCP
server looks again: when they've changed it tells the harness (`notifications/tools/list_changed`;
Claude Code asks for them again at once), so agents at work get them mid-conversation. From a
server that doesn't say, or while it's out of reach, it offers its own (those it last had from the
server, else this binary's).

**Code mode.** A program runs in the same bounded QuickJS sandbox as the server's code mode
(`mcp/code.rs`), here on the runner. `kiln.machines.run({on, command})` runs a command as `run` does
and resolves when it ends with `{runner, exit_code, output}` (a command that fails resolves with its
exit code; a machine that can't be reached rejects). Calls run side by side, up to 16 at once, so
`await Promise.all([...])` builds on the Mac and on Linux at the same time. Also
`kiln.machines.list`, `.start` (in the background), `.output` and `.stop`. kiln's own operations
(`kiln.ci.*`, `kiln.cd.*`, as `code_search` lists them) go to the server through the runner's own
connection (`Req::Ask` → `AgentMsg::ReachAsk`, `mcp::runner_ask`), for the session: it must be one
the server knows on that runner, and not a job's runner. The session acts for whoever started it
(their rights, or the owner's), audited as `agent:<session>@<runner> for <them>`; on a kiln nobody
but the owner signs in to, a session kiln doesn't know the starter of acts for the owner, and on
others it can't call kiln's operations. A program has 10 minutes unless `timeout_s` says otherwise
(at most an hour), 4 MiB per answer; runs it waited on and left behind are stopped. Its skills are
the MCP server's resources too.

The timeline shows a `run` as the command itself, with its output, its exit code and "on beta", and a
program by its first line.

Per agent (New agents → More → Commands; `Spec.reach`): any runner it needs (the default), a
runner or platform it prefers (`run` goes there unless the agent says otherwise), or only its own
machine (no tools). An agent's **Where commands run…** changes it while it lives (the `reach`
control): its harness takes the tools when it starts, so it starts again once its turn is over, the
conversation going on, and the timeline says so; an agent that isn't running takes it when it next
starts.

## 8. Moving an agent to another runner

**Move to…** on an agent (or `POST /api/harness/sessions/:id/move {"to": "agent:<runner>"}`) moves
it, mid-conversation, to another runner that has its harness installed and takes reach links. Nothing
of it is lost on the way, and nothing waits on a person: what is sent meanwhile waits in kiln's queue
and is answered over there.

When the agent's machine is away, it can't hand the agent over. The move then goes on from the
server's copy of the conversation instead, its transcript included (`{"from_store": true}`; see
[conversations.md](conversations.md), "Resuming on another machine").

1. **The folder** (`harness/relocate.rs`, on the server). An agent working in a git repository goes
   on in that repository's copy on the other machine, kept in step by sync (*Repositories on
   machines*, [sync.md](sync.md)): a copy there already, or one a sync task makes now — the source's
   working tree, uncommitted work included, then both copies kept in step, so moving back is just as
   quick. It goes where the repository is under the runner's home (`~/Projects/kiln` stays
   `~/Projects/kiln`), beside a folder of it already there, never over it. An agent in any other
   folder moves without its files (the timeline says so); where that folder isn't on the other
   machine, it works in its home folder. *Go on elsewhere at a limit* never moves one: its work
   would be left behind.
2. **The turn** (`harness/session.rs`, on the runner it is on). A move waits for the turn under way
   to end; kiln's queue holds what is sent meanwhile. Then the harness ends, its record complete.
   **Move now** doesn't wait: once the other runner is ready (its copy of the repository made), the
   turn is interrupted, and the other runner picks it up first thing — the conversation, the
   interrupted reply included, and a message saying it moved (`move {"now": true}`).
3. **Its files, then its repository** (`harness/session.rs`, `harness/gitstate.rs`). Once the
   harness has ended, the runner looks at the folder at once (sync otherwise looks every 2 s) and
   waits until its last changes are in the sync log: their contents on the server, the log having
   brought them back. Where its copy then stands in the log (an index) goes with the package. Then
   the repository besides its files, which sync doesn't carry: every branch (commits pushed nowhere
   included), the tags, the stash, what is staged and where HEAD is, with the commits the other
   machine lacks (its tips, asked over reach) as a git bundle. Then the files git ignores, which
   sync leaves alone (`.env`, local settings, keys, a local database), but not what a build makes
   again (`node_modules`, `target`, `.venv`, `dist` and the like), each with its permissions. An
   agent working in a git worktree inside the repository (`.claude/worktrees/x`) carries that
   worktree instead (`harness/worktree.rs`): the commit its HEAD is at, what differs from it
   (changed, staged, new and deleted files) and the files git ignores there. It doesn't move while
   a merge, rebase, cherry-pick, revert or bisect is under way (git keeps those in `.git` alone),
   with more than 128 MB of commits or 64 MB of ignored files to carry (16 MB a file), with files
   too big for sync, or from a repository of its own nested inside the synced one (or a
   submodule): the timeline says why, and it goes on where it is.
4. **The hand-over** (`harness/moving.rs`). The runner packs the session — the harness's own record
   (Claude Code's transcript as it is; Codex's conversation as a new thread's history, as a switch of
   harness carries it; for ACP agents and Antigravity, whose own records kiln doesn't keep, the
   conversation in words from kiln's timeline, given with the first message the harness gets there),
   kiln's timeline and state with its queue, its settings with the folder it goes on in, and what its
   harness keeps for it in its own home (below) — and hands it over a reach link: end to end
   encrypted, in pieces within a window, started over if the link drops (`Ask::Adopt`,
   `Say::AdoptGot`, `Say::Adopted`). A package in words is version 2 of the format, one with what
   the harness keeps version 3, which an older runner refuses whole (the agent stays where it was
   rather than arrive without them); the others stay version 1, which older runners take.
5. **The take-over** (the other runner). It puts the record where its harness looks for it, then what
   the harness keeps, starts the session under the same id with its timeline as it was, and says so
   (what came, what went beside, what stayed); the server follows the session
   there and viewers stay on it. Only then does the first runner let go of it. A runner that can't take
   it over says why, and the session goes on where it was. The agent starts only once its folder there
   has taken the sync log as far as it stood on the other machine ("Waiting for its folder's last
   changes"; messages sent meanwhile wait, and a restart waits again), so it never starts on files
   older than the ones it left. Then its repository is put as it was, touching no file of the working
   tree: branches made, or moved on where they were behind; one with other commits there keeps them,
   beside it (`<name>.before-move` for the branch HEAD is on, `<name>.from-<machine>` for another);
   stash entries stored; the index read from what was staged; HEAD pointed where it was. The files
   git ignores are written where they're missing, their permissions kept; one that differs there
   stays, and the one that came goes beside it (`<name>.from-<machine>`), which git there is told to
   ignore (`info/exclude`), so it's never committed nor synced. A worktree is made from the
   repository's copy at its commit, the differences written over it, its HEAD and index put as they
   were; one there already (the agent coming back) keeps what changed in it, stashed. What couldn't
   come is said, and stays on the machine it left: nothing is deleted on either side.

**What its harness keeps** (`harness/homes.rs`). Its memories and the session's own files go with
the conversation, and land where the harness looks for them for the folder the agent works in there:

| Harness | What goes | Where it lands there |
|---|---|---|
| Claude Code | its auto memory of the project (`MEMORY.md` and its files; not a team's) | the project's `projects/<project>/memory/`, the project being the repository's main folder (a worktree's too), else the folder |
| | the session's own: its subagents' transcripts, tool results and title; its checkpoints for rewind; its tasks and todos; the plans its transcript names | beside its transcript (`projects/<folder>/<session>/`), `file-history/<session>/`, `tasks/<session>/`, `todos/`, `plans/` |
| Codex | its memories (`memories/`, but `raw_memories.md`, which it makes again; `memories_extensions/`) | the same folders |
| | the thread's goal, read from its database | set on the new thread once the conversation is in (`thread/goal/set`) |
| Gemini CLI | its memory of the project (`tmp/<project>/memory/`) | the project's, by the name Gemini CLI knows the folder by there, else the one it would give it, marked (`.project_root`) so it finds it |
| Antigravity | its knowledge (`knowledge/`) | the same folder |
| Goose | its memories (`memory/<category>.txt`) | the same folder |
| opencode, Amp, Devin | nothing: what they keep is the session's own record | |

A memory there is never written over: one that isn't there is written; one that differs stays, and
the one that came goes beside it (`builds.from-<machine>.md`); an index (`MEMORY.md`) takes the
lines it lacks, pointing at what went beside, and Goose's memories the entries they lack. The
session's own files take the place of the ones it left there coming back, as its record does. A
file over 16 MB, or past 64 MB in all, stays and is said: memories go first, checkpoints last.

What stays, and why:
- **The person's own instructions** (`~/.claude/CLAUDE.md` and `rules/`, `~/.codex/AGENTS.md`,
  `~/.gemini/GEMINI.md`, opencode's and Amp's `AGENTS.md`, Goose's `.goosehints`). They are the
  machine's, for every project and agent on it, and can hold what is true of that machine alone:
  written there by a move, they would change every other agent's instructions. The package carries
  a digest of them, and when the ones where it lands differ, the timeline says it goes on with that
  machine's. Those in the repository go with it (`CLAUDE.local.md` among the files git ignores).
- **Codex's databases** (`goals_*`, `memories_*`, `state_*`, `thread_history_*.sqlite`), never
  written: Codex's other processes use them, their schema is its version's, and they're keyed by a
  thread that doesn't go on there. The goal goes over its protocol instead, its memories as the
  files Codex makes of them.
- **Codex's rollout.** The conversation goes as the new thread's history, which Codex records there
  in a rollout of its own; a copy of the old one would be a second thread of the same conversation
  there, in a folder that isn't. It stays, whole, where it was.
- **Credentials and settings**: `.credentials.json`, `auth.json`, `oauth_creds.json`,
  `credentials.toml`, secrets files, keys; `~/.claude.json` (the account, and per-project MCP
  servers with their tokens), settings, hooks, skills, plugins. And what is the machine's: running
  processes and their keys (`sessions/`), shell snapshots, hook environments, logs, caches.
- **Records of a session that starts anew there**: an ACP agent and Antigravity go on in a new
  session, given the conversation in words, so Gemini CLI's chats and checkpoints, Antigravity's
  conversation and its artifacts (`brain/<conversation>/`), and opencode's, Goose's and Devin's
  sessions stay.
- The project's memory when the agent's folder isn't on the machine it goes to (it works in the home
  folder there), or Claude Code keeps it elsewhere (`autoMemoryDirectory`); and all of it when the
  agent goes on from the server's copy, or in another harness.

**Into another harness.** `move {"to": …, "kind": "codex", "model": "…"}` moves it to a runner that
has that harness instead of its own, the conversation carried over as a switch of harness carries it
(Claude Code ⇄ Codex: read from the harness's own record, fitted, written the way the other keeps
its own; the permission mode never wider, the nearest effort). The timeline says it goes on in the
other harness, there too. Not from the server's copy: a machine that's away moves an agent only in its
own harness.

**A paused agent goes on there.** A pause for a usage limit is the old machine's account's: moved,
the agent goes on (the turn the limit cut off first), and the old account's limits stay behind. This
is how *Go on elsewhere at a limit* works ([agents.md](agents.md), "Usage limits: pause and resume"):
the server picks the machine (its own harness signed in to another account with room first, then
another harness it can be carried into) and moves it. Measured on one machine with two runners and
two accounts: paused at its stop point, on the other runner 2.5 s later, the turn going on there.

**Its work comes whole.** Checked end to end with two runners and a fake agent that works on files
(`~/.cache/kiln-moves-lab`): told to move in the middle of a turn that writes a file at its very end,
with a read queued behind it, the agent's first turn on the other machine reads that last write, 0.3 s
after it was made; without the wait, the same read found the file as it was before. Its untracked
file, its edit to a tracked one, a commit pushed nowhere, a branch, a stash entry and a staged file
came too, and `git status` read the same on both machines. Every answer the agent gave stays in the
timeline: a harness started again (a restart, a move) names what it says anew, rather than over what
it said before.

Measured on one machine with two runners, each with a home of its own: an idle agent moves in 1.6 s;
one in a git repository in 6.9 s the first time (5.3 s of it copying the repository over, uncommitted
work and all) and in about a second after that, either way, its copies being in step. One moved
mid-turn moves as the turn ends, and a message sent meanwhile is answered on the new runner with the
whole conversation in its context; moved now instead, it is on the new runner in 1.6 s, and the turn
goes on there 5 s later (Claude Code starting). An agent that comes back to a runner it was on replaces the record
it left there; one resumed there meanwhile, with turns of its own, is never written over, and the
agent stays where it is.

## 9. Not yet

- A direct runner-to-runner path when two runners can reach each other (the link would move to it
  and keep its session); today every link goes through the server.
- Pinning fingerprints from the UI (today: `kiln reach status`, `kiln reach forget`).
- Windows runners as the doing side (the process side is there; the local socket is Unix-only), and
  so `kiln run` and the `kiln` MCP server for agents on Windows runners.
- Agents that agents start (`kiln spawn`): kiln doesn't record whom they act for yet, so on a kiln
  others sign in to they can't call kiln's operations (the machines they can).
- Moving agents of ACP harnesses (Gemini, opencode, goose, amp) and Antigravity keeps their
  conversation and their memories, but not the harness's own record of the session: kiln's timeline
  goes with them in words, and the harness starts a new session there with it
  (`moving::Carry::Words`), so what it keeps of that session (Gemini CLI's checkpoints, Antigravity's
  artifacts) stays.
- Moving the tasks an agent left running (shells, monitors): they stay, and stop, where they were.
