# Reach: commands on other runners

An agent, a person or a script on one runner runs commands on another — a Linux x86-64 build
from a Mac, a test on the machine with the GPU, `uptime` on a box by name — over an end-to-end
encrypted link the kiln server only relays.

```text
kiln run --on linux/amd64 -- cargo test      # the idlest Linux x86-64 runner; this repo synced over first
kiln run --on adacachy --no-sync -- uptime   # a runner by name, in its home folder
kiln runners linux                           # the Linux runners, best first, and why others don't suit
```

Agents get the same as MCP tools (`runners`, `run`, `output`, `stop`), so Claude Code, Codex or any
harness that takes MCP servers can decide for itself to build or test elsewhere.

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

`--on` (or the tool's `on`) is a runner's name or what it must be: `linux/amd64`, `macos`,
`arm64`, `+gpu` (a label), `cpus>=16`, `mem>=32g`, `harness=codex`, `-name` to exclude one; words
separated by commas or spaces. Aliases are understood (`amd64`, `x64` → `x86_64`; `arm64` →
`aarch64`; `darwin`, `mac` → `macos`).

Of the connected runners that take reach links and suit, the one with the most idle cores wins,
then free memory; CI work on it and a tunnelled path (HTTP instead of UDP) count against it, and
the asking machine itself wins a tie (nothing crosses the network — it then runs locally). The
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

Shell text, run by `bash -c` (`sh` where there is none) in its own process group, with the
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
commit the copy is made to match exactly. The command then runs in the copy's folder matching
where the asker was. Paths are checked (relative, plain, never through a symlink) so a list can't
write outside the copy.

Measured on this repository (8,100 files, 36 MB, through the relay): first sync 2.5 s, an
unchanged tree 0.5 s.

## 6. Local socket

`<runtime dir>/kiln/reach-<runner>-<hash>.sock` (`0600` in a `0700` folder), beside `kiln spawn`'s.
Terminals and agents kiln starts get it as `KILN_REACH_SOCK`; elsewhere `kiln run` finds the only
one on the machine. One connection is one request (`local::Req`) answered by a stream of
`local::Ev`.

## 7. Agents

kiln gives every agent it starts on a runner the `kiln-reach` MCP server (`kiln reach mcp`):
Claude Code through `--mcp-config`, Codex through `-c mcp_servers.kiln_reach.*`. Its instructions
tell the model when reach helps; its tools:

| tool | does |
| --- | --- |
| `runners` | the machines, best first for `want` |
| `run` | a command on `on` (default: the session's preference, else the best), synced first; returns exit code and output (the middle cut past 30k characters), or an id with `background` |
| `output` | a background run's new output, and whether it ended |
| `stop` | stop a background run |

The timeline shows a `run` as the command itself, with its output, its exit code and "on beta".

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
   machine, it works in its home folder.
2. **The turn** (`harness/session.rs`, on the runner it is on). A move waits for the turn under way
   to end; kiln's queue holds what is sent meanwhile. Then the harness ends, its record complete.
   **Move now** doesn't wait: once the other runner is ready (its copy of the repository made), the
   turn is interrupted, and the other runner picks it up first thing — the conversation, the
   interrupted reply included, and a message saying it moved (`move {"now": true}`).
3. **The hand-over** (`harness/moving.rs`). The runner packs the session — the harness's own record
   (Claude Code's transcript as it is; Codex's conversation as a new thread's history, as a switch of
   harness carries it), kiln's timeline and state with its queue, its settings with the folder it goes
   on in — and hands it over a reach link: end to end encrypted, in pieces within a window, started
   over if the link drops (`Ask::Adopt`, `Say::AdoptGot`, `Say::Adopted`).
4. **The take-over** (the other runner). It puts the record where its harness looks for it, starts the
   session under the same id with its timeline as it was, and says so; the server follows the session
   there and viewers stay on it. Only then does the first runner let go of it. A runner that can't take
   it over says why, and the session goes on where it was.

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
- Windows runners as the doing side (the process side is there; the local socket is Unix-only).
- Moving agents of ACP harnesses (Gemini, opencode, goose, amp): their sessions are the harness's own.
- Moving the tasks an agent left running (shells, monitors): they stay, and stop, where they were.
