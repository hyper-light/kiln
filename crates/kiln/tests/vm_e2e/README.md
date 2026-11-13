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
| `crates/kiln/tests/fixtures/vm-stub/harness/stub` | the stand-in harness: Claude Code's stream-json, Codex's app-server, ACP, Antigravity's stream-json and Pi's RPC; no model |
| `crates/kiln/tests/fixtures/vm-stub/harness/`, `vm_e2e::stub_agentfile` | the stand-in as a microVM image: its harness directory, and its Agentfile, its text alone, which the drive writes for each harness and keeps in kiln for the run |

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
| nothing ran outside the microVMs | `harnesses_on_host`: no harness process in the session's process trees on the host (the runner's, and `VM_E2E_HOST_ROOTS`') at any of the drive's looks; `check_in_guest`: the kernel the harness's own shell said is Linux, and on a Linux host a boot other than the host's (a container shares the host's); `check_no_host_pid` and `check_starts`: kiln says the session runs in a microVM holding no process of the host, and every start of its harness was in one, so a harness that ran on the host and ended between two looks is found | `ps` (Linux, macOS) or CIM's `Win32_Process` (Windows); the harness's `WHERE <uname -s> <boot id>` line; the session's `/runs` and `/starts` (kiln plan step 17), read just before it is stopped and after its resumed turn |
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
| `VM_E2E_LINKS` | the links to read, each end's path on kiln's API (`{session}`: the session's id) and the JSON pointer to its suite: `[{"name": "agent-control", "client": {"url": "…", "pointer": "…"}, "server": {"url": "…", "pointer": "…"}, "lanes": {"url": "…", "pointer": "…"}}, …]`, one for each of `server-runner`, `kiln-hex`, `agent-control` and `mcp` at least; `agent-control`'s `lanes` is where its report lists its lanes' names, among which a member's `mcp:<member>` lane must be (Shards D254). With kiln plan step 17's report, the agent-control entry is `{"name": "agent-control", "client": {"url": "/api/harness/sessions/{session}", "pointer": "/runs/control/controller/suite"}, "server": {"url": "/api/harness/sessions/{session}", "pointer": "/runs/control/endpoint/suite"}, "lanes": {"url": "/api/harness/sessions/{session}", "pointer": "/runs/control/lanes"}}`; and the server-runner entry, read from kiln's list of runners, the session's picked by its name (`{machine}`: step 34's plan step 2, kiln main 458330ae3), is `{"name": "server-runner", "client": {"url": "/api/agents", "pick": {"name": "{machine}"}, "pointer": "/link/runner/suite"}, "server": {"url": "/api/agents", "pick": {"name": "{machine}"}, "pointer": "/link/server/suite"}}` |
| `VM_E2E_HOST_ROOTS` | pids (comma-separated) of processes besides the runner whose trees hold the session's processes: a daemon its microVMs run under that left the runner's tree |
| `VM_E2E_PROJECT` | a real project's folder on this host, for a real task (unset: an empty one of the test's own) |
| `VM_E2E_PROMPT`, `VM_E2E_RESUMED_PROMPT`, `VM_E2E_STEERED_PROMPT`, `VM_E2E_STOPPED_PROMPT`, `VM_E2E_DONE_MARKER` | a real task, the resumed session's turn, a long one to steer into, a long one to stop, and what the harness says at a task's end; `{kind}` in each is the harness's id, `{file}` the file to write. Unset: the stand-in's, for an image ROOT names (the stand-in's); from kiln's own `harness-<kind>` the run skips unless all five are given (fails, with `VM_E2E=1`) |
| `VM_E2E_IMAGE_<KIND>` | the image a harness's microVM is made from (`agentfile:<id>`), or `stub` for the stand-in (below); unset: kiln's Agentfile for it, `harness-<kind>` |
| `VM_E2E_SERVER`, `VM_E2E_TOKEN`, `VM_E2E_MACHINE`, `VM_E2E_RUNNER_PID`, `VM_E2E_RUNNER_LOG` | ROOT's own kiln (one with the sign-ins and credentials real tasks need) at `http://127.0.0.1:<port>`, its admin token (from a file, never on a command line or in a log), its runner's machine id, that runner's pid and its log (where it writes each call of kiln's MCP over a lane); unset: a server and a runner of the test's own, its headroom off |

No wait has a time of its own (`vm_e2e::Pace`; docs/research/s35-every-harness/waits/README.md). Until a
harness gives its first answer in a drive, and while it works on a turn the drive asked to be long, a wait
follows its liveness and progress alone. After that, the slowest answer the harness has given in the drive
bounds how long it may go without progress. A wait ends at once, typed and saying why, where kiln's own
word says no answer can come. Each refusal prints the session's state, each of its messages' delivery,
and its timeline.

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

`VM_E2E_IMAGE_CLAUDE=stub` and `VM_E2E_IMAGE_CODEX=stub` run the same drive in microVMs with the
stand-in for each harness, and no `VM_E2E_PROMPT`: no vendor's CLI and no model anywhere. The drive
writes the stand-in's Agentfile for the harness (`stub_agentfile`: its text alone, as kiln plan step
17 builds a person's, kiln's own images' base, the harness named as kiln starts it, its home as
kiln's images mount theirs: its harness.json's `run.env` sets `HOME=/home/stub` for every kind, and
that is its harness's volume, else kiln refuses it (`NoConfigVolume`), no network at all), keeps it in kiln (`POST /api/agentfiles`), boots `agentfile:<id>` and removes it after. Its
`HARNESS` comes from kiln's repository at this checkout's commit, which Shards' own git client fetches
on the runner: the commit must be on GitHub, and the runner must reach github.com. ARGs expand in
`HARNESS ... FROM`, as kiln's own Agentfiles use them; a Git source built so is a first (owed: ROOT's
run).

## What's owed, by whom, and what isn't checked yet

Step 34's plan (kiln plan 01a1d2c1ef09707daafe3fe9b77754e6) step 4. Each item names the step that owes
it, the surface step 34's check reads, and the evidence, at kiln main 562268830 and Shards main
8479be870 (2026-11-13).

1. **The agent-control link's ends, the microVM's identity and no host pid, on the session**: step 17's
   Shards slice, as its agent names them: `/runs/in`, `/runs/vm`, `/runs/image`, `/runs/host_pid`,
   `/runs/control/{controller,endpoint}/{suite,group}`, `/runs/control/lanes` and `/starts`. Read from
   `GET /api/harness/sessions/<id>`, through `VM_E2E_LINKS`' `agent-control` entry, and by
   `check_no_host_pid` and `check_starts`. The link's ends say nothing until it carries the stdio
   (Shards' control link and kiln's re-vendor). Evidence before it: the session's info (`harness/session.rs`, `Info`) says its
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
   `mcp_round_trip`, through `VM_E2E_LINKS`' `mcp` entry, by `check_mcp_lane` (the agent-control
   link's report naming the `mcp` lane), by `mcp_calls_over_lane` (the runner's own log of each call
   it took over the session's lane, `reach::mcp::serve_over`, kiln main dace992a6), and, before the drive boots anything, by `grants_to_kiln`
   (no grant of the image's Agentfile reaching kiln: a destination at the machine itself at kiln's
   port, a remote MCP server there, or kiln's port open to any address). Evidence of what it
   replaces: step 18's branch
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
5. **The server-runner link's ends**: built (step 34's plan step 2, kiln main 458330ae3): AES-256-GCM
   alone on the link (`net::LINK_PROVIDER`), and each end's suite and group on `GET /api/agents`'s
   `link` (`server`, `runner`). Read through `VM_E2E_LINKS`' `server-runner` entry (above).
6. **The kiln-Hex link's Hex end**: step 34's plan step 3, through Hex's lead (the node's own report of
   each peer link's suite and group); the kiln end is step 17's Hex client. Read through
   `VM_E2E_LINKS`' `kiln-hex` entry.
7. **A microVM image without a vendor's turn**: built from its text alone since step 17 (kiln main
   522f14c31); the stand-in's is written so ("Without a vendor's turn", above). Owed: ROOT's run of it.
8. **A harness that ran on the host and ended between two looks**: step 17's `/starts`, kiln's record
   of each start of the session's harness, every one kept, which `check_starts` reads: a start on the
   host is found whenever it ran.

## Step 17 part 6: a run's network narrowed to its person's provider (`narrowed::`)

`vm_e2e/narrowed.rs` runs a Goose session of kiln's own Agentfile (`harness-goose`) in a microVM, on
a server and a runner of its own (never ROOT's: it writes the person's home in the runner's folder).

- **No vendor's CLI and no prompt off the machine.** The stand-in takes Goose's place. Before any
  session, the test builds kiln's Goose Agentfile, its `HARNESS` line the stand-in's and every grant,
  volume and command kiln's, into the runner's Shards home (`kiln::sandbox::shards_home`). It tags it
  as kiln finds Goose's image (`kiln::sandbox::image::tag_of`). Each session starts with no prompt;
  the stand-in is given its words only once `/runs/image` names that image's digest, and a session of
  any other image is stopped unasked.
- **Narrowed.** The person's home (`owner`: an admin's session acts for the owner) names Anthropic
  (`GOOSE_PROVIDER: anthropic`, in `GOOSE_PATH_ROOT/config/config.yaml`, where kiln's Goose image
  reads it). `/runs/reach` must be `{"as": "narrowed", "uses": ["anthropic"], "hosts": […]}`, the
  hosts kiln's table keeps for it (`BY_USE`). Inside, the stand-in's `CONNECT=api.anthropic.com:443`
  connects, and `CONNECT=api.groq.com:443` is refused by the microVM's network: its resolver answers
  REFUSED, RCODE 5 (Shards' D236), said `CONNECT_REFUSED api.groq.com:443 dns rcode 5`. No notice
  says it isn't narrowed.
- **Whole, and said.** With the person's config unreadable (a folder at its path), `/runs/reach` is
  `{"as": "whole", "why": {"unread": {"file": "<its path in the microVM>", …}}}`, a warning notice
  says the network isn't narrowed and why, and both hosts connect: every destination the image names.

The run needs the runner to reach github.com (Shards fetches the stand-in from kiln's repository at
this checkout's commit, which must be on GitHub) and both providers' hosts on 443. The light checks
run anywhere:

```sh
cargo test -p kiln --test vm_harnesses narrowed::checks::      # any OS
cargo test -p kiln --test vm_harnesses stub::s17_               # Linux, macOS (python3): CONNECT's words
```

The microVM run (ROOT), on each host OS, with `VM_E2E=1` so that a runner without microVMs fails it:

```sh
VM_E2E=1 nice -n 10 cargo test -p kiln --test vm_harnesses narrowed:: -- --nocapture --test-threads 1
```

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
| `ui/e2e/vm-harnesses.spec.ts` | against the mock: kiln's ten Agentfiles listed read-only, Claude Code and Codex started in a microVM from kiln's (step 34's), Codex from a person's own, a Codex conversation kept in a person's microVM volume picked up from the composer's past sessions in a microVM (one of the machine's own as a process); against ROOT's server: each harness from kiln's Agentfile and a person's own, started, steered, its turn stopped, ended, and picked up again from the composer where `ui_pick_up` holds |

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
and `VM_E2E_LINKS`' `mcp` entry), step 4 (Antigravity's, one fixed entry in its shared config, over that lane), step 5 (a microVM conversation picked up again in one
from the UI, on kiln main), and step 34's own (kiln plan steps 16, 17, 18 and 33 and the links' owners).

#### Linux x86_64 (KVM: adacachy, or adacachy2 through kiln run)

With step 34's exports, every variable its "Variables" table lists (the runner's log among them, where the drive reads each MCP call over the lane):

```sh
nice -n 10 cargo test -p kiln --test vm_harnesses step35::rest:: -- --nocapture --test-threads 1
nice -n 10 cargo test -p kiln --test vm_harnesses step35::mcp:: -- --nocapture --test-threads 1
```

The stand-in first, with no vendor's turn (step 34's "Without a vendor's turn"): `VM_E2E_IMAGE_<KIND>=stub`
for each harness, and none of the five task words. The drive keeps the stand-in's Agentfile in kiln and
boots it as `agentfile:<id>`, so that run is one from a person's own Agentfile; the second, a person's
own beside it, is skipped, saying so, unless `VM_E2E_OWN_<KIND>` names a file of an Agentfile's text.

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
