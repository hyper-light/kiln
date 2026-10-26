---
name: kiln-ci-runners
description: Understand and change where kiln CI legs run - runners and their facts, executors (host, docker, microVMs, Kubernetes), executor pools, runner images, and routing rules - with explain. Use when a leg waits forever or lands on the wrong machine, a job needs a GPU, an architecture, more memory or an exclusive runner, or a new runner image is wanted.
---

# Runners, executors, pools, images and routing

## Why does this leg wait?

```js
// code_run: what a waiting (or running) leg needs, and how each runner measures up.
const r = await kiln.ci.legs.routing({ leg: input.leg });
return r; // every runner and pool: eligible or not, each requirement with its value and pass/fail, the choice and why
```

The scheduler explains the legs it holds: a finished leg answers `known: false`, and where it
ran is its `runner` (`kiln.ci.runs.get`); to see why a job goes where it goes, ask where it
would go (below) with its `runs-on`.

A leg waits for one of: no runner connected that carries its labels, a runner lacking a
requirement (a capability, an architecture, memory), every eligible runner busy, a concurrency
group, a quota, or a reviewer (an environment). The answer names which; fix that, not the
symptom. Don't widen `runs-on` to "any runner" to make a leg start.

## Where would a job go?

Before writing a job's `runs-on` or `requires`, ask:

```js
const w = await kiln.ci.routing.explain({ repo: input.repo, runs_on: ["linux", "gpu"], requires: ["mem.total >= 32Gi"] });
return { labels: w.labels, constraints: w.constraints, executors: w.executors, image: w.image, rules: w.rules, runners: w.runners };
```

`rules` are the routing rules that applied, each with where it comes from: the server's, the
repository's organization's, or the repository's trunk `.kiln/routing.yaml`.

## The runners and what they are

`kiln.ci.runners.list()` gives each runner: connected or not, its labels, its facts (OS,
architecture and variant, CPU model, cores, memory, disk, GPUs, executors it can run, virtualization,
tools and their versions, declared facts), what it is running and its slots. Requirements are
written against those facts, by these names:

- `os == linux`, `arch == aarch64`, `os.distro == ubuntu`, `kernel`;
- `cpu.cores >= 8`, `cpu.model matches "*EPYC*"`, `cpu.flags has avx512f`;
- `mem.total >= 16Gi`, `mem.free`, `disk.free >= 50Gi`, `disk.total`;
- `gpu.count >= 1`, `gpu.vendor has nvidia`, `gpu.memory >= 24Gi`;
- `executors has docker`, `tools.node >= 20` (versions compare part by part), `custom.<name>` (a
  runner's declared facts);
- a fact alone holds when it's true or there: `virt.kvm`, `tools.cargo`;
- `label bench`; `!`, `&&`, `||` and parentheses combine them (`arch == aarch64 || label arm`).

A job asks with `requires: [...]` (kiln dialect, or an overlay for a GitHub workflow), and leans
with `prefer: [{ fact: disk.free, higher: true }]`. `exclusive: true` takes a whole runner (for
benchmarks); `resources: { cpu, memory }` reserves part of one.

## Routing rules

Ordered data: the first rules that match a leg add their requirements, executors, image and
preferences. A repository's are its trunk's `.kiln/routing.yaml`, changed by a diff like any other
file:

```yaml
- match: { labels: [ubuntu-latest] }
  require: ["os == linux", "arch == x86_64"]
  executor: [docker, host]
  image: kiln/runner-ubuntu-24.04
- match: { labels: [gpu] }
  require: ["gpu.count >= 1"]
```

The server's and organizations' rules are an administrator's (`kiln.ci.routing.rules()` lists
them all, in order). GitHub's hosted labels map to rules only when the administrator turned that
on; otherwise `ubuntu-latest` waits for a runner that carries that label.

## Executors and pools

A runner runs legs on the host, in Docker containers (with kiln's Ubuntu runner images, or a job's
`container:`), in microVMs (`firecracker`, `shards`) or as Kubernetes pods. A pool keeps executors
warm for a kind of leg: `kiln.ci.pools.list()` shows each pool's kind, image and how many
executors are starting, idle and busy. A leg that waits on `scaling` is waiting for a pool to
start an executor.

## Runner images

`kiln.ci.images.list({ repo })` lists the images a repository may use, with their platforms and
digests. A new one is a Dockerfile (or a shards Agentfile) proposed as a diff, checked first:

```js
const content = input.dockerfile;
const v = await kiln.ci.images.validate({ content, path: `.kiln/images/${input.name}/Dockerfile` });
if (v.diagnostics?.some(d => d.severity === "error")) return v;
return await kiln.ci.images.propose({ repo: input.repo, name: input.name, kind: "dockerfile", content });
```

Once it lands, kiln's image build runs it and registers the image with its platforms; jobs then
name it (`image:`) or routing rules do.

## Changing runners themselves

Draining a runner, resizing a pool or registering a machine change the fleet, which is a person's
call: say what you'd change and why, and leave it to them unless your attachment allows
`manage-runners`.
