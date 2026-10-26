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
runner and takes the same words too, with `harness=codex` and `-name` (not that one).

## 3. In a workflow

A job's `resources:`, `requires:` and `exclusive:` are its needs (kiln's keys: `needs:` is
GitHub's, for the jobs a job waits for). A GitHub workflow takes them from its overlay,
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

## 5. How placement chooses

**A CI leg** (W05) goes to a runner that is **eligible** (its labels for `runs-on`, its facts for
the constraints, its capabilities and executors) and has **room** (a slot, or in resource mode its
request within capacity × `overcommit`; an exclusive leg's share held, W05-R03). Among those it
takes its preferences' best (W05-R23) when the job or a rule declares any, else the least loaded.
One that can't go anywhere yet waits saying why: `oversized` when it asks for more than any
eligible runner has, naming what each has ("Too big for any runner"), `capacity` while none has
room, `unroutable` when none could ever take it.

**A command** (`kiln run`, MCP's `run`, code_run's `kiln.machines.run`) goes to a connected runner
that takes reach links, meets the constraints (its reported facts, with its latest sample's
platform, cores and memory over them) and has the room asked for idle and free (docs/reach.md §2).
The asking machine comes first while it suits and has 4 GiB free beyond what's asked; else the
idlest: idle cores count most, then free memory, and CI work on it, a tunnelled path and being
overloaded count against it. It says which and why:

```text
[kiln] on big · linux/x86_64 · 32 of 32 cores idle · 60 GB free · room for 8 CPUs, 32 GiB of memory
```

and, when none suits, why each doesn't (`small: 16 GB free, under 32 GB`). The command finds its
needs in `KILN_NEEDS`. Reach holds nothing for it: what's idle when it's picked is what it gets.

**The gate** is a command like any other, `kiln run --needs cpus=12,mem=40g,linux/amd64 --detach
-- ci/gate.sh` (AGENTS.md). `ci/gate.sh` holds itself to what it asked (a systemd user unit:
MemoryMax from the needs, as many rustc and test threads as its CPUs, CPUWeight 1, nice 19, I/O caps
on the disk its build is on) and stops itself when the machine's own services stall on I/O;
`ci/gate.sh --dry-run` prints its steps and caps.
