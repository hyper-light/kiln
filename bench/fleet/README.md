# The fleet harness: microVMs at each host's derived limit, measured, put through chaos, watched

kiln plan `01a1cd8e862e7169a074db3d5fa33f3d`, step 36: hundreds of concurrent harness microVMs across
kiln's machines, placed by Hex and kiln's balancer peer to peer; density, start latency, control round
trip and stream throughput as n, p50, p90, p99 and max per host; chaos on VMs, Hex nodes, runners and
links; no oversubscription, no lost or doubled placement and no leaked VM after it. ROOT runs it
(Shards R14); this file says what it does, every number it keeps and where each comes from, and the
exact commands.

It lives in kiln because the VMs under test are kiln's isolated sessions (steps 17, 32 and 35: placed by
kiln's balancer, Hex under them), the fleet and its runners are kiln's and are reached through
`kiln run`, and kiln vendors Hex and Shards at one revision (step 16), so one kiln revision pins the
whole stack a run measured. Hex is driven through its own scheduler's command line.

## What runs where

- **The controller** (`fleet.py run --plan PLAN --out DIR`, on the machine ROOT runs it from): one
  `kiln run --on HOST --needs NEEDS` per host of the plan, all at once. kiln syncs this checkout to the
  host, holds the run to the room it asks for, and streams the agent's stdout back as it is written
  (measured: lines arrive within 0.2 s). Every record goes into `DIR/records.jsonl`, anything else a
  host prints into `DIR/logs/<host>.out`; at the end `DIR/summary.txt` is written.
- **The host agent** (`fleet.py host`, one per host, under `kiln run`): owns everything the run starts
  on its host, in a scope of its own (`OUT/s`: the Shards home, its Docker config), fills the host,
  measures, takes the chaos, drains, verifies and tears down. Its processes are the kiln run's own,
  so kiln's between-turns sweep leaves them; the run's end ends anything they left. Each live VM's
  lane holds two of its descriptors, so it lifts its open-file limit to the most the OS lets it hold
  (the hard limit; on macOS kern.maxfilesperproc, where a shell's soft limit is 256) and records it
  (`nofile` in its `host` record); an ask whose client the host can't spawn is refused, typed
  (`start_failed`, `refused`), and the fill stops there. An ended ask's descriptors close with it.
- **The driver** decides how a VM is asked for. Built: `shards`, the direct driver (below), which is
  the baseline every placer is held to and the harness's light test. Owed: `hex` and `kiln`
  (Owed, below).

`kiln run`'s stdin is buffered until it ends (measured: eight lines written 2 s apart all arrived
after the last), so it carries no commands to a running agent: each agent runs its plan on its own
clock, and the controller only starts, collects and reports.

## The direct driver and its workload

- A Shards daemon of the run's own, in the foreground as the agent's child (`shards run daemon`) with
  `SHARDS_POOL=0`, so every VM process belongs to an ask, in its own home, with an empty Docker config
  and no local image store, as Shards' control-end harness runs one.
- The image is pulled once into that home, pinned by digest (a tag that moved would measure another
  image), and run once to its end before anything is measured: the guest kernel's download and the
  image's template are first-run costs no fleet start pays again. That run's VM may still be exiting
  as its client returns: the agent waits for it to go (`first_run_gone`, its linger recorded) before
  the first scan.
- Each VM is `shards run vm -i --rm --name fleet-<token> IMAGE cat`: the stub harness. Its lane is
  its client's stdin and stdout, and `cat` echoes what it reads, so a round trip is bytes written and
  the same bytes back, checked byte for byte (`corrupt` otherwise). Closing its stdin ends `cat` and
  so the run. `--ballast BYTES` has the workload hold that much resident (tmpfs pages) before it
  answers, for a real harness's footprint (The profile, below).
- The view (which VM process the daemon says runs each ask) is `shards inspect vm`'s `.State.Pid`.
  The scan is the OS's own process table: the daemon's descendants, and, for what a daemon that died
  left behind, every process whose arguments name the run's home; each `shards-net` is paired to its
  `shards-vm` by the MAC address both are given (`--net …,MAC` and `--mac MAC`). A network process
  starts under its VM's parent and names no home (Shards' `crates/shards/src/netproc.rs`, D183), so
  every MAC a scan has paired with one of the run's VMs is kept: a network process with one is the
  run's while it lives, found even after its VM and its daemon are gone.

## What is measured

| Metric | What one sample is | Where |
|---|---|---|
| density | VMs running at once on the host, at each scan of the hold; and each VM's host memory (its `shards-vm` and `shards-net` processes: PSS on Linux, physical footprint on macOS, working set on Windows) | `scan` |
| start latency | an ask's client spawned to its lane's first echo: the VM booted, its workload answering. A wave's lanes are served between its spawns, so a start is charged its own spawn and those under way until its echo came, never the rest of its wave's (each ask's spawn is recorded, `spawn_ns`; the first light test's starts were charged up to the whole wave's, 414 ms of spawns in a wave of 25) | `started` |
| control round trip | one control message (below) written on a lane and read back whole, every VM's lane at once, one outstanding each | `rtt` |
| stream throughput, a turn | one turn's stream (below) written while its echo is read back, every lane at once; MiB/s per stream | `turn` |
| stream throughput, bulk | one transfer of the converged size (below) written while its echo is read back, every lane at once; MiB/s per stream. Each size's round is kept (`bulk` with `size`, `bulk_round`); the report reads the converged size's | `bulk`, `bulk_size` |
| floor | the same round trips and transfers through the same loop to a local `cat`: the harness's own cost, beside every number | `rtt`/`turn`/`bulk` with `floor` |

Each host's report gives n, p50, p90, p99 and max of each (nearest rank, Hyndman and Fan's definition
1: every value a measured sample), with the host's name, OS and version, kernel, CPU, memory, kiln
revision, the shards binary's SHA-256 and version, and the load at the start.

## Every number, and where it comes from (Shards R03)

| What | Derived from |
|---|---|
| The room | what `kiln run --needs mem=…,cpus=…` asked for and kiln holds the run to, which the agent reads from `KILN_NEEDS`; no memory room, no run |
| Density limit | min over dimensions of floor(room / cost), the cost the most any VM on the host was measured holding, re-derived at every scan (`derive.density_limit`) |
| Ramp | waves doubling from 1 up to the room left: every concurrency from one to the limit within a factor of two, in log2(limit) waves |
| Waits | twice the slowest answer of their kind (Hex's failure-bound rule, `hex_core::patience` TIMES_SLOWEST), only rising, a run-out doubling it (Karn's point, RFC 6298 §5.5); seeded before their own first answer from an answer that bounds them from above: a start from the setup's run of the image, a round trip, a transfer, a VM's settle and the view's settle from the slowest start (a start ends with a round trip on its lane), the daemon's settle from its start, the first run's linger from the first run. A batch of operations on the lanes (round trips, turns, a bulk round) is bounded on a stall, the wait for its next operation to finish, never on its length: twice the slowest single operation of its kind, which each operation teaches, per byte for transfers (each bulk round's are twice the last's), seeded from a turn's time per byte (RFC 6298's timer is a segment's). A stall is recorded (`ran_out`, with each lane's operation in flight) and backed off; it never cuts the batch short, since that would drop the slowest samples, the tail being measured. Only a signal or the run's deadline ends a batch unfinished. Setup's first waits have nothing before them and are bounded by the run's deadline alone |
| Samples | 100 a host, the fewest whose nearest-rank p99 is a sample below the max (`stats.least_n`): each VM is asked ceil(100 / VMs) round trips and transfers |
| Control message | the bytes kiln writes to interrupt a Claude Code turn (`crates/kiln/src/harness/claude.rs` `request`, kiln d6ffa45d8): `{"request":{"subtype":"interrupt"},"request_id":"kiln_N","type":"control_request"}` and a newline, 83 bytes at N = 1 |
| A turn's stream | the bytes a real harness turn wrote: Claude Code's signed-in turn, the largest turn recorded (Shards e7825cb32, `docs/research/measurements/harness-bringup-v2/records/claude-signed-in/stdout.bin`: 60,783 bytes; Codex's turn there wrote 765) |
| Transfer size | doubled from one vsock packet (64 KiB, `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`), a full round a size (the samples above, every lane at once), until the VMs' median rate no longer rises: the size before is the converged one. A median over every lane's samples, because three samples on one lane spread fourfold on a loaded host (the first light test); bounded by the room the VMs leave (the harness holds every size's pattern and the next: four times the size) |
| Probes | where the OS gives no event (a process gone), the process table is read at doubling intervals from 1 ms: O(log T) reads for a settle of T. A lane's end and a client's exit are events (its read end at EOF) |

The run's own inputs are the person's (ROOT's) and are recorded in its `host` record: the room
(`--needs`), the deadline (`--deadline-s`), the chaos actions and rounds, the seed and the image.

## Witnesses (forbidden effects, Shards R15)

Judged at every scan, from the ledger of asks (each an explicit state machine:
asked → starting → live → stopping → ended, or refused, failed, killed), the daemon's view and the OS's
scan, each read on its own (`fleetlib/witness.py`):

| Witness | The effect |
|---|---|
| `over_room` | the scope's processes hold more memory than the room |
| `over_limit` | more VMs run than the derived limit lets in |
| `doubled` | one ask has two VM processes, or one VM process two asks, or (controller) one ask is live on two hosts |
| `lost` | an ask the ledger holds live has no VM process |
| `leaked` | a VM process in scope that no live ask holds |
| `orphan` | a network process whose VM is gone: one whose MAC a scan paired with a VM of the run's, or one under the daemon whose MAC no VM of the run's ever had |
| `unasked` | the placer runs a VM for an ask never made, or one already ended |
| `ended_unasked` | a live VM's run ended with nobody ending it (typed, with its client's exit) |
| `stray`, `left` | any VM or network process before teardown, or any process naming the home after it |

`lost`, `leaked`, `orphan` and `unasked` are judged only at quiescent points (nothing starting or
stopping). A lag can make each of them for a moment: the daemon's records behind its VMs, a VM still
exiting after its run ended. So at a quiescent scan, one that shows is judged again, at doubling
intervals, until it is gone or twice the slowest start has passed (`view.settle`): it is a witness
only if it outlasts that, and the lag is recorded either way (`view_lag`, with whether it resolved).
A process the OS lists as a zombie (exited, its parent yet to reap it) runs no VM and holds no
memory: the scan leaves it out. A run passes when every witness counted zero, no failure was
recorded and nothing was left.

## Chaos

Each action kills one thing the run started, by its pid and its start (a pid the OS gave another
process is refused, `reused`; one outside the scope, `out_of_scope`), then the agent settles, refills
to the limit and lets the witnesses judge. Targets are drawn by a generator seeded with `--seed`.

| Action | Kills | Then | Settled when |
|---|---|---|---|
| `vm` | a live ask's `shards-vm` (SIGKILL) | its ask is killed; a new ask takes its room | its lane ended and its VM and network processes are gone |
| `daemon` | the home's daemon (SIGKILL) | the daemon is started again in the same home; each live ask either ended with it or is listed by the new daemon | every live ask's run is one or the other |
| `client` | a live ask's client, the far end of its lane (the link) | whether its run outlived it is recorded; the run is removed by name | its VM and network processes are gone |

## Records

`records.jsonl`, one JSON object a line, each with `kind`, `t` (the host's wall clock, ns),
`mono` (its monotonic clock, ns), `host` and `run`. Kinds: `host` (facts, knobs, the controller's
clock at launch for the skew), `phase`, `daemon`, `pulled`, `first_run`, `first_run_gone`, `ask`,
`started`, `late`, `start_failed`, `scan` (every scope process, the view, the room, the limit, free
memory and load), `view_lag`, `witness`, `fill_stopped`, `rtt`, `turn`, `bulk` (and `rtt_failed`,
`turn_failed`, `bulk_failed` for an operation whose lane ended under it), `bulk_round`,
`bulk_bounded`, `bulk_size`, `chaos`, `ended`, `ended_unasked`, `ran_out`, `drain_left`,
`teardown_kill`, `stopped`, `failed`, `verdict`.
`fleet.py report records.jsonl…` prints the report from them alone.

## Commands

From a kiln checkout at the revision under test (its `git rev-parse HEAD` is recorded). The shards
binary is the one under test, on each host: a release build of the Shards revision (every
measurement keeps `release`, Shards' CLAUDE.md; "A known Shards on each host", below), or the copy
kiln carries once step 16 lands.

**One host** (the light test's form), as a background `kiln run` so kiln's sweep leaves it:

```sh
OUT="$HOME/.cache/kiln-fleet/run-1/HOST"; mkdir -p "$(dirname "$OUT")"
kiln run --on HOST --needs mem=16g,cpus=8 -- \
  nice -n 10 python3 bench/fleet/fleet.py host --out "$OUT" --label HOST \
    --shards /path/to/shards \
    --image 'alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf' \
    --chaos vm,daemon,client --rounds 3 --deadline-s 3600 --seed 36
```

(From an agent: kiln MCP `run` with `on`, `needs`, `background: true` and `cwd` the checkout.) The
host's `OUT/records.jsonl` and `OUT/summary.txt` stay on the host.

kiln's checkout is large, and its sync to a busy host can fail to finish (the first light test's
three syncs to adacachy timed out). Then copy the harness alone and name the revision it is, since a
copy has none of its own:

```sh
git archive HEAD bench/fleet | ssh HOST 'rm -rf ~/.cache/kiln-fleet/src && mkdir -p ~/.cache/kiln-fleet/src && tar xf - -C ~/.cache/kiln-fleet/src'
kiln run --on HOST --needs mem=16g,cpus=8 --no-sync -- \
  'cd ~/.cache/kiln-fleet/src && nice -n 10 python3 bench/fleet/fleet.py host --rev REV …'   # REV: git rev-parse HEAD
```

**The fleet**: a plan, then one command.

```json
{
  "image": "alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf",
  "chaos": "vm,daemon,client", "rounds": 3, "deadline_s": 3600, "seed": 36,
  "hosts": [
    {"on": "adacachy", "needs": "mem=32g,cpus=8", "shards": "/home/adalundhe/…/shards"},
    {"on": "adalundhe-macbook-pro", "needs": "mem=24g,cpus=6", "shards": "/Users/ada.lundhe/…/shards"},
    {"on": "jupiter", "needs": "mem=32g,cpus=8", "shards": "C:/Users/corph/…/shards.exe", "python": "C:/Users/corph/.cache/kiln-fleet/python-3.14.10/python.exe"}
  ]
}
```

```sh
python3 bench/fleet/fleet.py run --plan plan.json --out ~/.cache/kiln-fleet/fleet-1
```

Run the controller itself as a background `kiln run` (or a background task of the shell that waits
for it within its turn). On macOS, an agent's session runs at a utility QoS clamp (Shards PM M317):
for numbers a user's programs would see, start the agent through
`docs/research/measurements/macos-sched/launchd-run.sh` of the Shards checkout.

## Tests

```sh
python3 -m unittest discover -s bench/fleet/tests -v   # the light tests: no microVM
python3 bench/fleet/tests/mutate.py                   # every guard removed in turn: each must fail them
```

The light tests read "do X, expect Y": percentiles, every derivation, each witness planted and found
(and not found where it must not be), the ask's state machine, the chaos scope and pid-reuse refusals,
lanes on loopback, each kind (`Selected` where it runs, and `Threaded` everywhere) held to the same
cases (`cat`, or a Python echo where there's none: round trips and a 4 MiB transfer on four lanes at
once, a lane that alters bytes or says them twice found `corrupt`, a lane's end as an event), a
threaded lane's late word after it was dropped reaching no one and a lane the OS gives no thread
refused leaving no descriptor, the OS's process table (one process read alone as the table reads
it, Windows' image names, an exited process `gone`), the scan's pairing, orphans and zombies, a
network process found and judged `orphan` after its VM and daemon are gone, a lag that settles and
one that doesn't, asks on the real direct driver with a stand-in `shards` that is an echo (a wave's
echoes read between its slow spawns, no descriptor left by an ended ask, a spawn the host refuses
and a lane it can't serve typed and stopping the fill), a hold over echo lanes (every kind of sample
on every lane), a mute lane's stall backed off to the run's deadline, and the report. `mutate.py
[WORDS…]` removes each of 31 guards (or those whose description holds a word) in a copy and requires
the tests to fail.

## The profile

A stub's VM holds what the stub holds. To count VMs as a real harness's would be, give `--ballast`
the resident memory a recorded harness VM held past the stub's: from a real Codex or Claude Code run's
record (steps 34 and 35, ROOT's), that VM's measured memory less the stub VM's in the same scan. No
such record exists yet, so every number here is the stub's own, and density is an upper bound for
real harnesses.

## Record

Two light tests on one host, functional, not measurements: a few stub VMs, to show the harness
fills, measures, takes its chaos, judges and cleans up. The campaign's numbers are ROOT's. Each
directory under `records/` holds a run's raw records and its summary;
`python3 bench/fleet/fleet.py report bench/fleet/records/<run>/records.jsonl.gz` prints the summary
again (the second's byte for byte).

- `records/2026-11-12-adacachy-light-1/`, kiln b82b865aa: adacachy, Linux 7.2.9-2-cachyos x86_64,
  AMD Ryzen 9 9950X3D2 (32 CPUs), load 43 to 72; shards 0.7.0 test-release, sha256 3298d7d4…
  (built on adacachy on 2026-11-12 at 14:41 from a tree since removed: its Shards revision is
  unknown); room mem=1g, 56 VMs at the limit, chaos vm, daemon and client once each, seed 36. FAIL,
  and each failure the harness's own: 9 `leaked` witnesses that were lags (the first run's VM still
  exiting, the daemon's view behind its newest VMs, zombies the daemon's kill left), starts charged
  their wave's later spawns (wave 13: 414 ms of spawns, the first three asks' echoes all read 432 ms
  in), and EBADF from finalizers at exit. Fixed in c28878d05 and 5e9254453.
- `records/2026-11-12-adacachy-light-2/`, kiln 5e9254453 (the harness copied with `--rev`, run by a
  background `kiln run --no-sync`, `KILN_NEEDS=mem=256m`): the same host and binary, load 31 to 48;
  limit 12 VMs (cost 20.4 MiB, the most a VM held), 26 asks, chaos vm, daemon and client once each,
  seed 36. PASS: no witness, nothing stray before teardown or left after it, no start failure, no
  wait run out. One lag of the daemon's view settled in 88 ms; the first run's VM was gone 116 ms
  after its client returned; spawns took 0.1 to 3.1 ms, and starts in a wave's ask order show no
  staircase; `nofile` was lifted to 1,048,576.

Light tests (`python3 -m unittest discover -s bench/fleet/tests`), at the revision that adds this
record: macOS arm64 (this checkout, Python 3.9.6) 44 pass; Linux x86_64 (adacachy, Python 3.14.8) 44
pass; Windows x86_64 (jupiter, python.org's 3.14.10 embeddable build, its SHA-256 checked, in a
directory removed after) 34 pass and 10 skip (the lanes, then owed, and the open-file limit, which
Windows doesn't keep). `mutate.py`: 25 of 25 on macOS.

## Windows

select() waits on sockets alone on Windows (Python's select docs), so there each lane's reader and
writer block in threads of their own (`lane.Threaded`; on Linux and macOS one selector serves every
lane, `lane.Selected`, and the light tests hold both to the same cases). Each lane thread keeps a
duplicate of its descriptor that it alone closes, so a descriptor the agent closes and the OS numbers
again is never one a lane thread reads; the reader times an operation where its last byte is read,
in that thread, and hands the agent's thread only the outcome. Two threads a VM at hundreds of VMs:
a lane the OS gives no thread or descriptor is a typed failed start (`start_failed`, `lane: …`) that
stops the fill, as a refused spawn does. The process table names Windows images without `.exe`
(Win32_Process's Name is the file's), and a process's start, CPU time and working set come from the
Win32 API (GetProcessTimes, K32GetProcessMemoryInfo), read alone for one pid (no PowerShell per ask)
and the same in the table; an exited process (one a handle still holds) is `gone`, never killed. The
floor runs wherever a `cat` is found (under `kiln run` on jupiter, Git's).

jupiter has no Python of its own (only the Microsoft Store's alias). It has python.org's nuget.org
package, the distribution meant for CI and build machines with no system-wide Python (Python's
"Using Python on Windows", 4.3; its full installer is deprecated since 3.14):
`C:/Users/corph/.cache/kiln-fleet/python-3.14.10/python.exe`, the package's SHA-512 checked against
nuget.org's catalog (`7d9967d9…902a5371`) before it was unpacked. Again, from Git Bash under
`kiln run --on jupiter`:

```sh
v=3.14.10; d="$HOME/.cache/kiln-fleet/python-$v"; t="$d.part"; mkdir -p "$t"
curl -fsSL -o "$t/p.zip" "https://api.nuget.org/v3-flatcontainer/python/$v/python.$v.nupkg"
sha512sum "$t/p.zip"   # equal to the catalog's packageHash (https://api.nuget.org/v3/registration5-semver1/python/$v.json → catalogEntry), base64 there
powershell -NoProfile -Command "Expand-Archive -Path '$(cygpath -w "$t/p.zip")' -DestinationPath '$(cygpath -w "$t/x")'"
mv "$t/x/tools" "$d" && rm -rf "$t"
```

## A known Shards on each host

`shards-build.sh REV [OUT]` (and `shards-build.ps1 -Rev REV` on a Windows host with no POSIX shell)
fetches REV alone from Shards' public repository and builds it as Shards' release workflow does
(`cargo build --locked --release -p shards`, `SHARDS_GIT_COMMIT` its first seven characters, so
`shards --version` names it), without the GPU's renderer (such a `shards` carries no GPU VM process
and refuses a GPU, which no fleet run asks for), into `~/.cache/kiln-fleet/shards/REV/` with
`BUILD.txt`, and removes its build tree. Built on 2026-11-13 at e82b9403b (the Shards kiln's step 16
and Hex pin), each `shards version 0.7.0, build e82b940`:

| Host | Binary | SHA-256 |
|---|---|---|
| adacachy (Linux 7.2.9 x86_64) | `/home/adalundhe/.cache/kiln-fleet/shards/e82b9403b863718930fe9f224186d3ae862bfd61/shards` | `fa18ba38ff1b5ea3e8898cc2e07d45a626b1d90d6aa43d06c6c86f8d6d00f576` |
| adacachy2 (Linux 7.3.3 x86_64) | the same path | the same: the two builds are byte for byte one |
| adalundhe-macbook-pro (Darwin 25.5 arm64) | `/Users/ada.lundhe/.cache/kiln-fleet/shards/e82b9403b863718930fe9f224186d3ae862bfd61/shards` | `ca19e40f252e535338626ac84ce870abb5e2da774412210c7256ecf238a20ac9` |
| adas-mac-studio-3.local (Darwin 25.2 arm64) | `/Users/adalundhe/.cache/kiln-fleet/shards/e82b9403b863718930fe9f224186d3ae862bfd61/shards` | `5e17e42cc9bf6025bafa524b4a36fdbabe8efd629ea4130bdea5ec907e8f65ec` |
| adacachy's Windows 11 guest (x86_64) | `C:\Users\shards\.cache\kiln-fleet\shards\e82b9403b863718930fe9f224186d3ae862bfd61\shards.exe` | `f271d140d5677dc3780bf53c77c8766bad463ec83422f3435d38c964d70d52fd` |

None was built on adas-mac-studio.local or adas-mac-studio-2.local (19 and 11 GiB free, 96-98% full),
adas-mac-studio.local-2 (25 KB/s Wi-Fi) or jupiter (its kiln runs held by the headroom guard).

## Owed

- **The `hex` driver** (steps 12 to 15): asks as `hex schedule call --socket … Submit` with a
  `ShardsImage` box (Hex's `docs/scheduler-live.md`), `Status` until created, `Delete`; the view from
  Hex's records and each node's run hosts (`shards api-run` under `hex node`); the room from the
  machine's capacity view (step 13, `docs/design/capacity-view.md`); chaos on `hex node`,
  `hex schedule serve` and the peers' tunnels, each a process the agent starts. Hex's image class boots
  on Linux only today (`Awaits(RunHostStdioOnly)` on macOS, `Awaits(WindowsRunHost)` on Windows) and
  gives a member's stdio no lane yet (`Awaits(MemberExec)`, step 11), and a create is refused until
  its waits are recorded outside the census (Hex `docs/design/shards-image-class.md` §6, §8).
- **The `kiln` driver** (steps 17 and 32): an isolated session started by `agents.start` with
  `isolation: microvm`, its harness kiln's fake Claude Code (`crates/kiln/tests/fixtures/fake-claude`,
  no model, no account) in a user Agentfile; placed by the balancer among runners with `virt.microvm`;
  its round trip a control request and its stream a turn's lines. Chaos on runners needs runners of the
  run's own (a second runner process per machine, `KILN_MACHINE_ID`, joined to a server of the run's
  own): production runners are never restarted.
- **jupiter's shards**: the Windows build below sits in adacachy's Windows 11 guest; copying it to
  jupiter (or building there once its headroom guard lets a build run) is ROOT's.
- **Runs**: a few VMs on one Linux host were run (Record). Hundreds across the fleet, macOS hosts
  (through launchd-run.sh), jupiter, Linux arm64 (no KVM host), and every chaos round at density are
  ROOT's.
