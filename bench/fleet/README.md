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
  so kiln's between-turns sweep leaves them; the run's end ends anything they left.
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
  image's template are first-run costs no fleet start pays again.
- Each VM is `shards run vm -i --rm --name fleet-<token> IMAGE cat`: the stub harness. Its lane is
  its client's stdin and stdout, and `cat` echoes what it reads, so a round trip is bytes written and
  the same bytes back, checked byte for byte (`corrupt` otherwise). Closing its stdin ends `cat` and
  so the run. `--ballast BYTES` has the workload hold that much resident (tmpfs pages) before it
  answers, for a real harness's footprint (The profile, below).
- The view (which VM process the daemon says runs each ask) is `shards inspect vm`'s `.State.Pid`.
  The scan is the OS's own process table: the daemon's descendants, and, for what a daemon that died
  left behind, every process whose arguments name the run's home; each `shards-net` is paired to its
  `shards-vm` by the MAC address both are given (`--net …,MAC` and `--mac MAC`).

## What is measured

| Metric | What one sample is | Where |
|---|---|---|
| density | VMs running at once on the host, at each scan of the hold; and each VM's host memory (its `shards-vm` and `shards-net` processes: PSS on Linux, physical footprint on macOS, working set on Windows) | `scan` |
| start latency | an ask's client spawned to its lane's first echo: the VM booted, its workload answering | `started` |
| control round trip | one control message (below) written on a lane and read back whole, every VM's lane at once, one outstanding each | `rtt` |
| stream throughput | one transfer of the converged size written while its echo is read back, every lane at once; MiB/s per stream | `bulk` |
| floor | the same round trips and transfers through the same loop to a local `cat`: the harness's own cost, beside every number | `rtt`/`bulk` with `floor` |

Each host's report gives n, p50, p90, p99 and max of each (nearest rank, Hyndman and Fan's definition
1: every value a measured sample), with the host's name, OS and version, kernel, CPU, memory, kiln
revision, the shards binary's SHA-256 and version, and the load at the start.

## Every number, and where it comes from (Shards R03)

| What | Derived from |
|---|---|
| The room | what `kiln run --needs mem=…,cpus=…` asked for and kiln holds the run to, which the agent reads from `KILN_NEEDS`; no memory room, no run |
| Density limit | min over dimensions of floor(room / cost), the cost the most any VM on the host was measured holding, re-derived at every scan (`derive.density_limit`) |
| Ramp | waves doubling from 1 up to the room left: every concurrency from one to the limit within a factor of two, in log2(limit) waves |
| Waits | twice the slowest answer of their kind (Hex's failure-bound rule, `hex_core::patience` TIMES_SLOWEST), only rising, a run-out doubling it (Karn's point, RFC 6298 §5.5); seeded before their own first answer from an answer that bounds them from above: a start from the setup's run of the image, a round trip, a transfer and a VM's settle from the slowest start (a start ends with a round trip on its lane), the daemon's settle from its start. Setup's first waits have nothing before them and are bounded by the run's deadline alone |
| Samples | 100 a host, the fewest whose nearest-rank p99 is a sample below the max (`stats.least_n`): each VM is asked ceil(100 / VMs) round trips and transfers |
| Control message | the bytes kiln writes to interrupt a Claude Code turn (`crates/kiln/src/harness/claude.rs` `request`, kiln d6ffa45d8): `{"request":{"subtype":"interrupt"},"request_id":"kiln_N","type":"control_request"}` and a newline, 83 bytes at N = 1 |
| Transfer size | doubled from one vsock packet (64 KiB, `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`) on one lane until doubling raises its median rate no higher than the highest at the size before; three transfers a size, the fewest whose median is neither their min nor their max; bounded by the room the VMs leave |
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
| `orphan` | a network process whose VM is gone |
| `unasked` | the placer runs a VM for an ask never made, or one already ended |
| `ended_unasked` | a live VM's run ended with nobody ending it (typed, with its client's exit) |
| `stray`, `left` | any VM or network process before teardown, or any process naming the home after it |

`lost`, `leaked`, `orphan` and `unasked` are judged only at quiescent points (nothing starting or
stopping). A run passes when every witness counted zero, no failure was recorded and nothing was left.

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
clock at launch for the skew), `phase`, `daemon`, `pulled`, `first_run`, `ask`, `started`, `late`,
`start_failed`, `scan` (every scope process, the view, the room, the limit, free memory and load),
`witness`, `rtt`, `bulk`, `converge`, `bulk_size`, `chaos`, `ended`, `ended_unasked`, `ran_out`,
`failed`, `verdict`. `fleet.py report records.jsonl…` prints the report from them alone.

## Commands

From a kiln checkout at the revision under test (its `git rev-parse HEAD` is recorded). The shards
binary is the one under test, on each host (a test-release build of the Shards revision, or the
copy kiln carries once step 16 lands).

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

**The fleet**: a plan, then one command.

```json
{
  "image": "alpine:3.24.4@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf",
  "chaos": "vm,daemon,client", "rounds": 3, "deadline_s": 3600, "seed": 36,
  "hosts": [
    {"on": "adacachy", "needs": "mem=32g,cpus=8", "shards": "/home/adalundhe/…/shards"},
    {"on": "adalundhe-macbook-pro", "needs": "mem=24g,cpus=6", "shards": "/Users/ada.lundhe/…/shards"},
    {"on": "jupiter", "needs": "mem=32g,cpus=8", "shards": "C:/Users/corph/…/shards.exe", "python": "python3.14"}
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
lanes on loopback (`cat`: round trips and a 4 MiB transfer on four lanes at once, a lane that alters
bytes found `corrupt`, a lane's end as an event), the OS's process table, the scan's pairing and
orphans, and the report. `mutate.py` removes each of 16 guards in a copy and requires the tests to
fail.

## The profile

A stub's VM holds what the stub holds. To count VMs as a real harness's would be, give `--ballast`
the resident memory a recorded harness VM held past the stub's: from a real Codex or Claude Code run's
record (steps 34 and 35, ROOT's), that VM's measured memory less the stub VM's in the same scan. No
such record exists yet, so every number here is the stub's own, and density is an upper bound for
real harnesses.

## Record

Light test, functional, not a measurement (Light test, below, is filled in by the commit that ran it).

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
- **Windows lanes**: its pipes can't be selected on; the agent there needs a lane per thread.
- **Runs**: a few VMs on one Linux host were run (Record). Hundreds across the fleet, macOS hosts
  (through launchd-run.sh), jupiter, and every chaos round at density are ROOT's.
