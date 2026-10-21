# kiln-sim: kiln's deterministic simulator

> **CP-T6.1** of the critical path (its T6 plan, "kiln-sim, and the real bugs it finds"). The
> crate is `crates/kiln-sim`; kiln's models are beside the code they run (`crates/kiln/src/sim`,
> `crates/kiln/src/goals/sim.rs`).

One seed decides everything a run does: which task is polled next, when each frame arrives or
whether it's lost, which write a crash comes at, what a power loss tears. So every failure replays
exactly from its seed, and a bug it finds is fixed with its seed kept as the regression test.

## Running it

| | |
| --- | --- |
| Every change request | each model's own budget of seeds, inside `cargo test` (kiln CI's `rust` job) |
| Nightly | `.kiln/sim.yaml`: each model over a large budget, from seeds no night before tried |
| One seed | `KILN_SIM_SEED=<seed> cargo test -p kiln --lib <model's test>`: that seed alone, with its last steps in words |
| More seeds | `KILN_SIM_SEEDS=<n>` (and `KILN_SIM_FROM=<first>`) |

A failure says its seed and how to replay it:

```
kiln-sim sync: seeds 0..=199 (200) in 41.20 s, 291 seeds/min on one thread, 1 failed
  seed 117: m3's folder isn't the log's repo: ...
    replay: KILN_SIM_SEED=117
```

## What it is

| Part | |
| --- | --- |
| `rng` | SplitMix64, drawn as the sends simulation draws it, with named streams: a source that draws more leaves the others' draws as they were |
| `world` | the seeded scheduler: each node's tasks run on a kiln-seams `SimSpawn`, all on one `SimClock`; the seed picks which node's ready task is polled next, one poll at a time; a crash drops a node's tasks wherever they wait |
| `queue` | a model's events, by when they're due |
| `net` | frames between nodes: lost, doubled, delayed (so reordered) and partitioned while the nemesis lasts, delivered after it |
| `disk` | a machine's disk: data durable at its file's fsync, names at their folder's (or any fsync, on a filesystem with one journal); a power loss keeps a prefix of the names and leaves each unsynced file as written, as before, torn at a page, or zeroed; fsync and write errors (EIO); ENOSPC; a crash at a chosen write; modification times as coarse as the seed's filesystem |
| `trace` | what a run did, folded into a digest; a replay keeps its last steps in words |
| `seeds` | budgets, seeds a minute, one seed's replay, and running a seed twice (two runs that differ fail on their own) |

## kiln's models

| Model | What of kiln's it runs | Its nemesis | What it checks |
| --- | --- | --- | --- |
| sync | a synced repo's copies on two or three machines and the server's member: `sync::Copy`, `Feed` and `Index` as hyper-durable replicas, `sync::server::take_steps`, the runner's and server's shells; each copy on a simulated disk through `sync::fs` | frames lost, doubled, reordered, cut off; runners and the server crashing; power losses; a crash at a chosen write; fsync and write errors; full disks; people editing all the while | nothing spreads that nobody wrote; every copy converges on the log's repo; no one's last write is lost |
| goal driver | `goals`' `adopt` and `step` as the driver's loop runs them, on a simulated clock, through `SimHarness`, its store inline (a crash at each of its waits) | the server crashing at any wait; machines away; harnesses starting again; usage limits; people clearing goals | one goal under way, in order, ended ones never back; nothing to a paused agent; no goal again within 30 s unless the agent was cut off from it; every stack runs to its end |

## The bugs it found

Each is fixed in a commit of its own, its seed kept as the regression test, replayed twice.

| Bug | Model, seed | Fixed in |
| --- | --- | --- |
