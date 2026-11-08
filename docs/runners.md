# Runners: what work needs

Work says what it needs, not where to run. A CI leg, a command run with `kiln run` or MCP's `run`
and `code_run`, and kiln's own gate all say it in one spelling, and kiln picks the runner that has
it. Nothing names a machine except work that has to happen on that one (`kiln run --on <name>`).

The grammar is `kiln_ci::sched::needs`; placement is W05 of docs/ci.md for CI legs (W05-R03 room,
W05-R22 constraints) and docs/reach.md §2 for commands.

## 1. Three kinds of need

- **Requests**: the room the work asks for: CPUs, memory, disk, GPUs. A CI leg's are reserved on
  the runner it goes to (resource mode, W05-R03). A command's are what the runner must have idle
  and free when it's picked; reach holds nothing for it.
- **Constraints**: what the runner must be and have: its platform, labels, tools, executors, CPU
  flags and any other fact it reports (§4). Checked, never reserved.
- **Exclusive**: the runner to itself. A CI leg holds the CPUs and memory it asks for, or the whole
  runner when it asks for none (W05-R03); a command goes to a runner with no CI work on it.

## 2. The spelling

Words separated by commas or spaces. Each reads as the needs it says (the second column is what
kiln says back, in placement's messages):

| Spelling | Reads as | |
| --- | --- | --- |
| `cpus=8` | `8 CPUs` | reserved; `cpu=8`, `cpus=0.5` and `cpus=500m` too |
| `mem=32g` | `32 GiB of memory` | reserved; `memory=`; `g`, `G`, `GB` and `Gi` are all binary |
| `disk=50g` | `50 GiB of disk` | reserved |
| `gpus=1` | `1 GPU` | reserved; `gpu=1` too |
| `linux/amd64` | `os == linux, arch == x86_64` | the platform, by any of its names |
| `macos` | `os == macos` | `darwin`, `mac` and `osx` too; `linux`, `windows`, `freebsd` |
| `arm64` | `arch == aarch64` | `aarch64`, `armv8`, `apple-silicon`; `amd64`, `x64`, `intel` are `x86_64` |
| `+gpu` | `label gpu` | a label it carries; `label=gpu` too |
| `tool:cargo` | `tools has cargo` | a tool on its PATH |
| `executor:docker` | `executors has docker` | an executor that works there |
| `flag:avx2` | `cpu.flags has avx2` | a CPU flag |
| `binfmt:linux/arm64` | `binfmt has linux/arm64` | a platform it runs under emulation |
| `cpus>=16` | `cpu.threads >= 16` | CPUs in all: checked, not reserved |
| `mem>=32g` | `mem.free >= 32Gi` | memory free: checked, not reserved |
| `disk.free>=50Gi` | `disk.free >= 50Gi` | any fact (§4), with `==`, `!=`, `>=`, `<=`, `>` or `<` |
| `custom.rack==a3` | `custom.rack == a3` | a fact of your own (W13-R02) |
| `exclusive` | `the runner whole` | nothing else beside it |
| `together:sim/3` | `one of 3 starting together (sim)` | one of a gang of 3 (2 to 64) that start together or not at all (W05-R40) |

A word that isn't a need is an error that names it: `memm=4g: no such need`, `cpu.coress>=8: no
such fact (…)`, `linx/amd64: no such OS (…)`. Some whole spellings:

```needs
cpus=8,mem=32g,linux/amd64
cpus=12,mem=40g,linux/amd64
macos,tool:xcodebuild
+gpu,gpus=1,mem=16g
linux executor:docker binfmt:linux/arm64
cpus>=16,mem>=32g
disk.free>=50Gi,os.distro==fedora
cpu=500m,memory=512Mi
exclusive,cpus=16,mem=48g
```

`kiln run` and the MCP tools take the spelling as `--needs` (`needs`); `--on` (`on`) names a
runner and takes the same words too, with `harness=codex` and `-name` (not that one). They refuse
`together:` until commands are placed by the engine (docs/needs.md E5).

## 3. In a workflow

A job's `resources:`, `requires:`, `exclusive:` and `together:` are its needs (kiln's keys:
`needs:` is GitHub's, for the jobs a job waits for). A GitHub workflow takes them from its overlay,
`.kiln/overlays/<stem>.yaml`. This job needs `cpus=8,mem=32g,linux/amd64,exclusive`:

```yaml
jobs:
  build:
    resources: { cpu: 8, memory: 32Gi }
    requires: ["os == linux", "arch == x86_64"]
    exclusive: true
```

`requires:` takes routing's whole language (W05-R22: `in [..]`, `matches`, `&&`, `||`, `!`), of
which the spelling's constraints are the common part. `kiln validate` reports a bad `resources:`
or `requires:` at its line.

A job's `together: true` makes its legs, its matrix, one gang (W05-R40): they start together or
not at all, as `together:<name>/<n>` says, the gang named by the run and the job and as big as the
job has legs. Its members may share a runner. This job's three legs start at once, once there's
room for all three:

```yaml
jobs:
  sim:
    strategy: { matrix: { node: [a, b, c] } }
    together: true
```

A gang is two legs at least and 64 at most, and starts them all at once, which `max-parallel` would
hold back: `kiln validate` reports a job it can't make one at the key's line (KCI1022), and lets be
a matrix it can't count until the run is planned. Rerunning one of its legs reruns them all.

## 4. Facts

What a runner reports of itself (W13-R01; `kiln runners facts <name>` shows them):

| Fact | What |
| --- | --- |
| `os`, `os.version`, `os.distro`, `kernel` | its OS (`linux`, `macos`, `windows`), version, distribution (`fedora`), kernel |
| `arch`, `arch.variant` | its architecture (`x86_64`, `aarch64`) and variant (`v8`) |
| `cpu.cores`, `cpu.threads`, `cpu.model`, `cpu.flags` | cores, hardware threads (CPUs), model, flags (`avx2`) |
| `mem.total`, `mem.free` | memory in all and free, bytes (`32Gi` in comparisons) |
| `disk.total`, `disk.free` | the volume its work is on |
| `gpu.count`, `gpu.model`, `gpu.vendor`, `gpu.memory` | its GPUs |
| `virt.kvm`, `virt.hvf`, `virt.nested` | the virtualization it offers |
| `executors`, `executors.<name>` | executors that work there (`docker`, `firecracker`), and each one's version |
| `binfmt` | platforms it runs under emulation (`linux/arm64`) |
| `tools`, `tools.<name>` | tools on its PATH, and each one's version (`tools.cargo >= 1.90`) |
| `net.egress` | whether it reaches the internet |
| `custom.<name>` | facts its administrator declared, and declared probes' results (W13-R02) |

In `requires:` a fact no runner has is false, never an error at run time; the spelling refuses a
name it doesn't know, as a typo (`custom.<name>` for one of your own). A runner that hasn't
reported facts (an older one, or one just started) meets no constraint of a CI leg; for a command,
only those its platform, cores and memory show.

### Pace

Beside its facts, a runner measures what bears on how fast a run would finish on it, on events
and never on a timer (docs/needs.md §17.2, W05-R26): its per-core speed (the server's fit of the
fleet's run history, said in the directory as `speed`), its slowness under contention now (PSI on
Linux; the run queue, the work device's latency and swap-ins on macOS and Windows), its clocks now
against its usual (thermal throttling: cpufreq, Windows' throttle limit, macOS's thermal level),
its work device's rate under its load, and its battery. Its words to owners carry `speed`,
`slowness` and `disk_bps`. On battery a runner takes nothing new by default:

```toml
[runner]
on_battery = "drain"   # or "take": as on its charger
```

## 5. How placement chooses

**A CI leg** (W05) goes to a runner that is **eligible** (its labels for `runs-on`, its facts for
the constraints, its capabilities and executors) and has **room** (a slot, or in resource mode its
request within capacity × `overcommit`; an exclusive leg's share held, W05-R03), and that it leaves
**within the target use** by the runner's live headroom: what its latest sample (taken as a run
starts or ends and as the OS says its pressure, clocks or power moved; never on a clock) has in
use of its CPUs and memory, its own work's and kiln's alike, what kiln placed there since, and the
leg's request stay within its target use (`[ci.scheduler] target_use` where a person sets it; else
its CPUs' M/M/m knee, 74% for 8 CPUs and 86% for 32, and all its available memory) of its CPUs and of its
memory, so memory is never placed past what a runner has. A runner confined to some of its
machine's CPUs (an affinity, a cgroup's quota) counts the machine's load on them, the machine's
spread over all its CPUs, so its load against its CPUs is the machine's against its cores. Among
those it takes its preferences' best (W05-R23) when the job or a rule declares any, then the one it
leaves the most headroom: the larger of its CPUs' and its memory's share in use after it, its I/O
pressure on top, the lowest; then the least loaded. A runner whose latest sample is over a minute
old is placed as before samples, by its room alone, after the runners with one. A v1 job goes by
the same rule. One that
can't go anywhere yet waits saying why: `oversized` when it asks for more than any eligible runner
has, naming what each has ("Too big for any runner"), `headroom` while every runner with room
would pass the target with it ("Runners at their target use"), `capacity` while none has room,
`unroutable` when none could ever take it. A leg asking for more than any runner has left under
the target doesn't wait for ever while smaller legs fill what comes free: once it's waited a minute
first in line, it holds the runner whose running legs are expected to leave it room soonest, and
there only legs expected to end by then, or that fit beside it, start; its wait says what it waits
for ("waiting for 14 CPUs and 1 GiB of memory free on a linux/x86_64 runner: a is held for it, with
room about 09:42"). Fair share between repositories counts the CPUs legs ask, so a repository of
big legs gets no more of the fleet than one of small legs at the same weight.

**One rule, in the shape Hex's placement takes.** The engine and the v1 dispatcher place through
one function, `kiln_ci::sched::place` (reach ranks a command by the same measure): what's needed and
the nodes in, a node or a refusal saying why out, the shape of kiln-seams' `Placement`
(docs/seams.md), whose suite it passes. Hex's placement (CP-T7.1, with priorities, quotas and fair
share) takes its place behind that shape: which node, what's held for whom, and each node's use.

**A command** (`kiln run`, MCP's `run`, code_run's `kiln.machines.run`) goes to a connected runner
that takes reach links, meets the constraints (its reported facts, with its latest sample's
platform, cores and memory over them) and has the room asked for idle and free (docs/reach.md §2).
Every one that suits is weighed by the engine's measure, the asking machine like any other: the
one the command leaves the most headroom (the larger of its CPUs' and its memory's share in use with
what's asked, its memory and I/O stalls on top) goes first, the asking machine only on a tie. Idle cores are
CPUs times their idle share; a load average plays no part. It says which and why:

```text
[kiln] on big · linux/x86_64 · 32 of 32 cores idle · 60 GB free · room for 8 CPUs, 32 GiB of memory
```

and, when none suits, why each doesn't (`small: 16 GB free, under 32 GB`). The command finds its
needs in `KILN_NEEDS`. Reach holds nothing for it: what's idle when it's picked is what it gets.

**A change's gate** is commands like any other, composed by the agent that owns the change
(AGENTS.md): a run per platform, side by side, each `ci/gate.sh [clippy|test] --since origin/main`
through `kiln run` with `--needs` its platform and the CPUs and memory its steps were measured to
need (`cpus=`, `mem=`; clippy on kiln-ci alone peaked at 4 GiB at six rustcs: `kiln run --needs
linux/amd64,cpus=6,mem=4g -- ci/gate.sh clippy -p kiln-ci`), natively on Linux, macOS or Windows, scoped to the
crates the change touches, those whose code reads a changed file by path, and those that depend on
them (a changed file it can't place: the whole workspace). `ci/gates.sh` starts the runs, never
detached, waits for each and passes only when every one exited 0 and said `GATE: PASS`.
`ci/gate.sh` holds each step to what its run asked, and runs nothing unasked (on Linux a systemd
user unit: MemoryMax from the needs, a rustc and a test thread per CPU asked, CPUWeight 1, nice 19,
I/O caps on the disk its build is on; elsewhere nice 19), says each step's peak memory (in its unit
the anonymous memory, page cache left out; elsewhere its processes' resident memory), takes its
verdict from what its steps wrote once all had ended, never from an exit code alone, and yields
(`GATE: YIELDED`, exit 75) when the machine's own services stall on I/O;
`ci/gate.sh --since origin/main --dry-run` prints its scope and why, its steps and its caps.
