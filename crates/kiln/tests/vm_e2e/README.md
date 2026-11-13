# Harnesses in Shards microVMs: the E2E (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 34; step 35 builds on it)

kiln drives a harness in a microVM of its own through its public surfaces: its REST API, its MCP
server and its audit trail. Step 34 runs real Codex and Claude Code sessions so, on Linux, macOS and
Windows hosts. ROOT runs the microVM runs; real harnesses and real tasks only where ROOT points them
at one.

## What each part is

| Path | What |
|---|---|
| `crates/kiln/tests/vm_harnesses.rs` | step 34's tests: `checks` (the acceptance's checks against samples), `stub` (the stand-in driven as a process), `host` (Claude Code's and Codex's interfaces driven through kiln on the host), `step34` (each in a microVM of its own) |
| `crates/kiln/tests/vm_e2e/mod.rs` | the drive (start, steer, interrupt, stop, resume, through the API), the checks it feeds, `in_microvm` and `on_host`, which step 35's file builds on |
| `crates/kiln/tests/fixtures/vm-stub/stub` | the stand-in harness: Claude Code's stream-json, Codex's app-server, ACP, Antigravity's stream-json and Pi's RPC; no model |
| `crates/kiln/tests/fixtures/vm-stub/Agentfile`, `harness/` | the stand-in as a microVM image, its grants as kiln's built-in Agentfiles grant theirs |

Step 35's own (`vm_e2e/step35.rs`, `vm_e2e/gaps.json`, `vm_e2e/mcp.rs`, `ui/e2e/vm-harnesses.spec.ts`)
are its agent's and say how they run.

## The drive, and what each step checks

For a harness, through `POST /api/harness/sessions` and its `command`s:

1. **Start** with the task (`spec.isolation`: `{"kind": "microvm", "image": …}`); its first turn ends
   saying the done marker.
2. **Steer** a message into a turn under way (a harness that takes none mid-turn gets it next, as
   kiln queues it); kiln delivers it and the harness answers it.
3. **Interrupt** a turn under way; it stops, short of its end.
4. **Stop** (`DELETE`); kiln lists it no more. An agent it started from where it runs is found, checked
   and stopped first.
5. **Resume** in a new session (`spec.resume`: its harness session); the harness takes up that same
   session; stopped too.

Then where it ran:

| Acceptance | Check | Read from |
|---|---|---|
| the negotiated suite is `TLS13_AES_256_GCM_SHA384` on every link, both ends | `check_all_links`: every link of `REQUIRED_LINKS` (`server-runner`, `kiln-hex`, `agent-control`, `mcp`) read, and every link read at both ends | `VM_E2E_LINKS` (below); a required link not read is a failure |
| nothing ran outside the microVMs | `harnesses_on_host`: no harness process in the session's process trees on the host (the runner's, and `VM_E2E_HOST_ROOTS`') at any of the drive's looks; `check_in_guest`: the kernel the harness's own shell said is Linux, and on a Linux host a boot other than the host's (a container shares the host's) | `ps` (Linux, macOS) or CIM's `Win32_Process` (Windows); the harness's `WHERE <uname -s> <boot id>` line |
| the project at its host path | `check_project_path`: kiln's `cwd` is the host path, and the harness's own `CWD <path>` is that path as its guest sees it (the same path on Linux and macOS; Shards D235's WSL form on Windows, `C:\X` at `/mnt/c/X`); `check_written`: the file the harness wrote in its project (`{file}`, its name its words) is at the host path | `GET /api/harness/sessions/<id>`, the timeline, the host's file |
| the harness reaches its provider | a real task's turns end with ROOT's marker, which only the model's answer says | the timeline |
| kiln's MCP works from inside | `mcp_round_trip`: a call of a kiln tool that came back done (`server: kiln`), or the stand-in's `MCP_OK <tools>` | the timeline |

The `host` runs are the same drive with each harness a process on the host behind the stand-in: every
step passes, and every check of where it ran says "the host" (the stand-in seen in the runner's tree at
every look, the host's kernel, no link read), as it must.

## What runs anywhere, now (light: no VM, no vendor)

```sh
cargo test -p kiln --test vm_harnesses checks::   # any OS, Windows included
cargo test -p kiln --test vm_harnesses stub::     # Linux, macOS (python3)
cargo test -p kiln --test vm_harnesses host::     # Linux, macOS (python3): Claude Code and Codex through kiln
```

`step34::` skips, in the runner's own words (`facts.isolation.why_not`), until the runner a test
starts says it starts microVMs (`facts.virt.microvm`: kiln plan steps 16 and 17). From kiln's own
image for a harness (`harness-<kind>`: the vendor's CLI) it then skips until ROOT gives a real task,
every one of the five `VM_E2E_*` words below (`untasked`): the stand-in's words never go to a model.

## The microVM runs (ROOT)

### Variables

| Variable | What |
|---|---|
| `VM_E2E=1` | a runner that starts no microVMs fails the run rather than skips it |
| `VM_E2E_LINKS` | the links to read, each end's path on kiln's API (`{session}`: the session's id) and the JSON pointer to its suite: `[{"name": "agent-control", "client": {"url": "…", "pointer": "…"}, "server": {"url": "…", "pointer": "…"}}, …]`, one for each of `server-runner`, `kiln-hex`, `agent-control` and `mcp` at least |
| `VM_E2E_HOST_ROOTS` | pids (comma-separated) of processes besides the runner whose trees hold the session's processes: a daemon its microVMs run under that left the runner's tree |
| `VM_E2E_PROJECT` | a real project's folder on this host, for a real task (unset: an empty one of the test's own) |
| `VM_E2E_PROMPT`, `VM_E2E_RESUMED_PROMPT`, `VM_E2E_STEERED_PROMPT`, `VM_E2E_STOPPED_PROMPT`, `VM_E2E_DONE_MARKER` | a real task, the resumed session's turn, a long one to steer into, a long one to stop, and what the harness says at a task's end; `{kind}` in each is the harness's id, `{file}` the file to write. Unset: the stand-in's, for an image ROOT names (the stand-in's); from kiln's own `harness-<kind>` the run skips unless all five are given (fails, with `VM_E2E=1`) |
| `VM_E2E_IMAGE_<KIND>` | the image a harness's microVM is made from (`agentfile:<id>`); unset: kiln's Agentfile for it, `harness-<kind>` |
| `VM_E2E_TURN_SECS` | how long a turn may take before the test fails (600 unset): a failing test's patience, not a value kiln runs by |
| `VM_E2E_SERVER`, `VM_E2E_TOKEN`, `VM_E2E_MACHINE`, `VM_E2E_RUNNER_PID` | ROOT's own kiln (one with the sign-ins and credentials real tasks need) at `http://127.0.0.1:<port>`, its admin token (from a file, never on a command line or in a log), its runner's machine id and that runner's pid; unset: a server and a runner of the test's own, its headroom off |

On a busy host kiln holds a queued message in its own queue until the machine has room (docs/headroom.md),
and the drive's turns then wait there. The test's own runner has its headroom off; turn ROOT's runner's off
for a run (`PUT /api/runners/<machine>/headroom` with `{"enabled": false}`) and back on after.

### The links, by name, and where each end will be read

| Name | Client end | Server end | Owed by |
|---|---|---|---|
| `server-runner` | the runner's own view of its link to the server | the server's view of that runner's connection | kiln plan 01a1d2c1ef09707daafe3fe9b77754e6 step 2 (AES-256-GCM alone there, each end on kiln's API, read from step 16's `Endpoint::negotiated`) |
| `kiln-hex` | kiln's Hex client | the Hex node's own report of each peer link | kiln's step 17 (its Hex wiring) and plan 01a1d2c1 step 3 (Hex's node report) |
| `agent-control` | Shards' `control::controller` in kiln | the agent-control endpoint's own report | Shards step 11 (on its control lane) and kiln's step 17 (on the session) |
| `mcp` | the harness's client | kiln's MCP as the microVM reaches it | Decided (2026-11-13, Shards D254): kiln's MCP rides the agent-control link as its own `mcp` lane (step 11 with step 18), so no network grant names it. Until that lane is built and the session's link report names it, this link has no suite and the run fails, saying so |

### A real task's words

Each of a real harness's turns says what the checks read, a line each:

```text
VM_E2E_PROMPT:          Run `pwd`, `uname -s` and `cat /proc/sys/kernel/random/boot_id` in your shell and reply
                        with the lines `CWD <the folder>` and `WHERE <the kernel name> <the boot id>`. Write a file
                        named {file} in this folder whose only words are {file}. Call kiln's `runners` tool.
                        Then <the real task>. End your reply with the line E2E-DONE.
VM_E2E_RESUMED_PROMPT:  Say the name of the file you wrote in this conversation. End your reply with the line E2E-DONE.
VM_E2E_STEERED_PROMPT:  Run `for i in $(seq 1 120); do echo $i; sleep 1; done` in your shell, then end your reply
                        with the line E2E-DONE.
VM_E2E_STOPPED_PROMPT:  Run `sleep 600` in your shell, then end your reply with the line E2E-DONE.
VM_E2E_DONE_MARKER:     E2E-DONE
```

### Linux x86_64 (KVM: adacachy, or adacachy2 through kiln run)

From a kiln checkout at main, once steps 16, 17, 18 and 33 and the links' owners above have landed:

```sh
export VM_E2E=1 VM_E2E_LINKS="$(cat links.json)"
export VM_E2E_SERVER=http://127.0.0.1:<port> VM_E2E_TOKEN="$(cat <admin token file>)" \
       VM_E2E_MACHINE=<runner's machine id> VM_E2E_RUNNER_PID=<runner's pid>
export VM_E2E_PROJECT=<a real project's folder> VM_E2E_DONE_MARKER=E2E-DONE \
       VM_E2E_PROMPT="$(cat prompt.txt)" VM_E2E_RESUMED_PROMPT="$(cat resumed.txt)" \
       VM_E2E_STEERED_PROMPT="$(cat steered.txt)" VM_E2E_STOPPED_PROMPT="$(cat stopped.txt)"
nice -n 10 cargo test -p kiln --test vm_harnesses step34:: -- --nocapture --test-threads 1
```

arm64 Linux has no KVM host here: owed until one is (Shards' nested L1 path on an M3 or later Mac).

### macOS arm64 (Hypervisor.framework: adalundhe-macbook-pro or adas-mac-studio.local, through kiln run)

The same commands. The test's `kiln` carries the VM process signed with the hypervisor entitlement
(step 16), or its runner says no `virt.microvm` and the runs skip (fail, with `VM_E2E=1`). A session's
timing at an agent's utility QoS clamp is no part of this check; time one through Shards'
`docs/research/measurements/macos-sched/launchd-run.sh`.

### Windows x86_64 (WHP: jupiter, through kiln run)

```powershell
$env:VM_E2E = "1"; $env:VM_E2E_LINKS = Get-Content links.json -Raw
$env:VM_E2E_SERVER = "http://127.0.0.1:<port>"; $env:VM_E2E_TOKEN = Get-Content <admin token file> -Raw
$env:VM_E2E_MACHINE = "<runner's machine id>"; $env:VM_E2E_RUNNER_PID = "<runner's pid>"
$env:VM_E2E_PROJECT = "<a real project's folder>"; $env:VM_E2E_DONE_MARKER = "E2E-DONE"
$env:VM_E2E_PROMPT = Get-Content prompt.txt -Raw; $env:VM_E2E_RESUMED_PROMPT = Get-Content resumed.txt -Raw
$env:VM_E2E_STEERED_PROMPT = Get-Content steered.txt -Raw; $env:VM_E2E_STOPPED_PROMPT = Get-Content stopped.txt -Raw
cargo test -p kiln --test vm_harnesses step34:: -- --nocapture --test-threads 1
```

The host's processes are read through `Get-CimInstance Win32_Process`; the project's guest path is its
WSL form. `stub::` and `host::` are Unix's (the stand-in is a Python script run by its name).

### Without a vendor's turn

The stand-in's Agentfile runs the same drive with no model once kiln builds it as a microVM image:
step 17 takes kiln's own images (`harness-<kind>`) and a person's own Agentfiles (`agentfile:<id>`)
alone, a person's Agentfile is kept as its text alone today, and the stand-in's HARNESS needs its
script and `harness.json` beside it and the harness's own name (`claude`, `codex`). Owed to step 17's
Shards slice; then `VM_E2E_IMAGE_CLAUDE=agentfile:<id>` and `VM_E2E_IMAGE_CODEX=agentfile:<id>` with
no `VM_E2E_PROMPT`.

## What's owed, by whom, and what isn't checked yet

Step 34's plan (kiln plan 01a1d2c1ef09707daafe3fe9b77754e6) step 4. Each item names the step that owes
it, the surface step 34's check reads, and the evidence, at kiln main 562268830 and Shards main
8479be870 (2026-11-13).

1. **The agent-control link's ends, the microVM's identity and no host pid, on the session**: step 17's
   Shards slice. Read from `GET /api/harness/sessions/<id>`, through `VM_E2E_LINKS`' `agent-control`
   entry (the controller's end from Shards' `control::Controller` in kiln, the endpoint's own report),
   and by `check_in_guest`. Evidence: the session's info (`harness/session.rs`, `Info`) says its
   `isolation` (kind and image) and no links, microVM identity or host pid; step 17's branch
   (`p/s17-launch-seam-v1`, 64dfab21f) reads its process's `host_pid()` inside (`session.rs` 4328 and
   4724) and says none of it; Shards' `control::Controller` (`crates/remote/src/control/drive.rs`)
   says no negotiated suite: the only reader is a test's (`#[cfg(test)] mod handshake`'s `Side::suite`
   in `crates/remote/src/lib.rs`, asserted at both ends in `control.rs`'s tests).
2. **The agent-control endpoint's own suite**: step 11 (Shards D238). Read from the endpoint's report on
   its control lane, as kiln relays it on the session (`VM_E2E_LINKS`' `agent-control` server end).
   Evidence: D238 asks for a test asserting each link's suite (D239), and Shards main has it, with no
   report at run time; step 11's branches (`p/s11-control-v1`, `p/s11-controller-v1`) are on Shards
   main.
3. **kiln's MCP from inside, and its suite**: step 11 with step 18, as decided (the person and ROOT,
   2026-11-13; Shards D254, which step 11 writes): kiln's MCP rides the agent-control link as its own
   `mcp` lane, so its suite is that link's and no network grant names kiln's MCP. Read by
   `mcp_round_trip`, and through `VM_E2E_LINKS`' `mcp` entry, the session's link report naming the
   `mcp` lane on the agent-control link. Evidence of what it replaces: step 18's branch
   (`p/s18-host-in-vm-v1`, a5ce21085) reaches `/api/mcp` as plain HTTP at
   `host.docker.internal:<port>` (`harness/isolated/mcp.rs`), which crosses no TLS, and its built-ins
   grant `--egress=host.docker.internal:7070`; D238's "Control that crosses no TLS" (ROOT's ruling,
   2026-11-13) takes the request slots, `engine.sock` and the endpoint's AF_UNIX and vsock leg as
   host-local and names no MCP. Until the lane is built, `mcp` has no suite and step 34's runs fail,
   saying so.
4. **kiln's MCP on a Windows host**: step 18. Read by `mcp_round_trip` on a Windows runner. Evidence: on
   the host, kiln gives a session its MCP server over the runner's Unix socket alone
   (`harness/kinds.rs`, `local_tools`, `#[cfg(unix)]`; its other twin gives none), so Codex and the ACP
   harnesses on Windows reach it only from a microVM, over item 3's `mcp` lane.
5. **The server-runner link's ends**: step 34's plan step 2 (AES-256-GCM alone on the link,
   `net::LINK_PROVIDER`, and each end's negotiated suite on kiln's API). Read through `VM_E2E_LINKS`'
   `server-runner` entry.
6. **The kiln-Hex link's Hex end**: step 34's plan step 3, through Hex's lead (the node's own report of
   each peer link's suite and group); the kiln end is step 17's Hex client. Read through
   `VM_E2E_LINKS`' `kiln-hex` entry.
7. **A microVM image without a vendor's turn**: step 17's Shards slice ("Without a vendor's turn",
   above).
8. **Claude Code past a steer**: kiln's own, found here (ROOT has the finding). kiln hands a Claude Code
   session no command after a message steered into its turn (the next queued message and a steered
   one each answered 202 and never reached the harness; Linux and macOS, 2026-11-13, with or without
   an agent started from inside, the stand-in saying Claude Code 2.1's `queued` lifecycle and replay).
   `host::s34_claude_code_through_kiln_on_the_host` is ignored until kiln delivers past a steer
   (`cargo test -p kiln --test vm_harnesses -- --ignored host::` shows it), and step 34's Claude Code
   run stops at its interrupt step until then.
9. **A harness that ran a process on the host and ended it between two looks**: no step owns it yet. The
   looks see the harness itself, a process of the whole session, and the kernel line is the harness's
   own shell's word on where it ran. kiln's own record of what it started for a session would close
   this.

## Step 35: every harness through kiln's UI and API (its agent's)

Step 35 runs all ten harnesses (steps 19 to 28: `claude`, `codex`, `gemini`, `opencode`, `goose`,
`amp`, `devin`, `antigravity`, `cursor`, `pi`) on step 34's drive, each in a microVM of its own from
kiln's Agentfile (`harness-<kind>`) and from a person's own (`agentfile:<id>`), through kiln's REST
API, its MCP tools and its UI. Each works as on the host, or its gap is in `gaps.json` with its cited
reason (plan 01a1d2c16eb074f19cf7ae7f1025c0be, steps 2 and 7).

| Path | What |
|---|---|
| `vm_e2e/step35.rs` | `step35::checks` (gaps.json held to kiln's code and its sources, the cite rule, the vendor-CLI rule), `step35::host` (the eight harnesses step 34 doesn't drive, through kiln on the host, the stand-in behind each name), `step35::rest` (the ten in microVMs through REST: start, steer, interrupt, stop, resume, kiln's MCP from inside, an agent started there) |
| `vm_e2e/mcp.rs` | `step35::mcp`: the ten in microVMs through kiln's MCP tools: `agents_start` naming the Agentfile, `agents_send` steering, `agents_interrupt`, `agents_stop`, `agents_start` with `resume` |
| `vm_e2e/gaps.json` | each harness's flags (`linux_build`, `mcp`, `mcp_in_microvm`, `resume`, `ui_pick_up`), each gap with why and its cites, caveats, and the sources its reason rests on; kiln's code cited by the text it holds (`path#text`), recorded evidence by its line |
| `ui/e2e/vm-harnesses.spec.ts` | against the mock: kiln's ten Agentfiles listed read-only, Claude Code and Codex started in a microVM from kiln's (step 34's), Codex from a person's own; against ROOT's server: each harness from kiln's Agentfile and a person's own, started, steered, its turn stopped, ended, and picked up again from the composer where `ui_pick_up` holds |

A vendor's own CLI takes ROOT's task alone: from kiln's Agentfile, and from a person's own made from
kiln's text, a drive skips unless all five task words are given (`VM_E2E=1`: fails). An image of the
stand-in that ROOT names (`VM_E2E_IMAGE_<KIND>`) takes the stand-in's words.

### What runs anywhere, now (light: no VM, no vendor)

```sh
cargo test -p kiln --test vm_harnesses step35::checks::   # any OS, Windows included
cargo test -p kiln --test vm_harnesses step35::host::     # Linux, macOS (python3): the eight through kiln on the host
cd ui && npm run build && npx playwright test e2e/vm-harnesses.spec.ts                 # the mock: Chromium, desktop and phone
cd ui && E2E_WEBKIT=1 npx playwright test e2e/vm-harnesses.spec.ts                      # and WebKit (`npx playwright install webkit` once)
```

`step35::rest::` and `step35::mcp::` skip, in the runner's own words, until the runner a test starts
says it starts microVMs (`facts.virt.microvm`); `VM_E2E=1` makes that a failure.

### The microVM runs (ROOT)

Step 34's variables (above), and:

| Variable | What |
|---|---|
| `VM_E2E_KINDS` | the harnesses to drive, comma-separated (`claude,codex`); unset: all ten |
| `VM_E2E_OWN_<KIND>` | a file holding the text of a person's own Agentfile for the harness (`VM_E2E_OWN_GEMINI=gemini.Agentfile`), kept through `POST /api/agentfiles` and started as `agentfile:<id>`; unset: kiln's own text kept as the person's (the vendor's CLI, so it takes ROOT's task); with `VM_E2E_IMAGE_<KIND>` (a stand-in run) and no text named, the person's own is skipped, saying so |
| `E2E_BASE_URL`, `E2E_TOKEN`, `E2E_VM_MACHINE` | the UI spec against ROOT's server: its URL, its admin token (from a file), and the name of its runner that starts microVMs as the composer's machine picker shows it |

They wait on (kiln plan 01a1d2c16eb074f19cf7ae7f1025c0be): step 1 (`agents_start`'s `image` and
`resume`, on kiln main), step 3 (kiln's MCP from inside a microVM, over the agent-control link's
`mcp` lane, Shards D254: goals 58's and 61's, read as step 34's drive reads it, by `mcp_round_trip`
and `VM_E2E_LINKS`' `mcp` entry), step 4 (Antigravity's MCP over that lane, or its gap), step 5 (a microVM conversation picked up again in one
from the UI), and step 34's own (kiln plan steps 16, 17, 18 and 33 and the links' owners).

#### Linux x86_64 (KVM: adacachy, or adacachy2 through kiln run)

With step 34's exports (`VM_E2E`, `VM_E2E_LINKS`, ROOT's server, the five task words, `VM_E2E_PROJECT`):

```sh
nice -n 10 cargo test -p kiln --test vm_harnesses step35::rest:: -- --nocapture --test-threads 1
nice -n 10 cargo test -p kiln --test vm_harnesses step35::mcp:: -- --nocapture --test-threads 1
```

The stand-in first, with no vendor's turn, once kiln builds it as a microVM image (step 34's
"Without a vendor's turn"): each harness's `VM_E2E_IMAGE_<KIND>` as that section says, and
`VM_E2E_OWN_<KIND>` naming a file of the stand-in's Agentfile text for a person's own, none of the
five task words.

#### macOS arm64 (Hypervisor.framework: adalundhe-macbook-pro or adas-mac-studio.local, through kiln run)

The same commands, as step 34's.

#### Windows x86_64 (WHP: jupiter, through kiln run)

```powershell
cargo test -p kiln --test vm_harnesses step35::rest:: -- --nocapture --test-threads 1
cargo test -p kiln --test vm_harnesses step35::mcp:: -- --nocapture --test-threads 1
```

With step 34's `$env:` lines first. `step35::host::` is Unix's.

#### The UI, against ROOT's server (desktop, once per harness)

```sh
cd ui && E2E_BASE_URL=https://<ROOT's kiln> E2E_TOKEN="$(cat <admin token file>)" E2E_VM_MACHINE=<runner name> \
  VM_E2E_PROMPT="$(cat prompt.txt)" VM_E2E_RESUMED_PROMPT="$(cat resumed.txt)" VM_E2E_STEERED_PROMPT="$(cat steered.txt)" \
  VM_E2E_STOPPED_PROMPT="$(cat stopped.txt)" VM_E2E_DONE_MARKER=E2E-DONE \
  npx playwright test e2e/vm-harnesses.spec.ts --project desktop
```

A harness whose `ui_pick_up` is false says its gap (a test annotation) in place of the pick-up.
