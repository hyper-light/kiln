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
| `net` | frames between nodes while the nemesis lasts: datagrams lost, doubled and overtaken; frames on an ordered lane (a QUIC stream) lost but never reordered or doubled; requests lost or late; links partitioned. After it, every frame arrives |
| `disk` | a machine's disk: data durable at its file's fsync, names at their folder's (or any fsync, on a filesystem with one journal); a power loss keeps a prefix of the names and leaves each unsynced file as written, as before, torn at a page, or zeroed; fsync and write errors (EIO); ENOSPC; a crash at a chosen write; modification times as coarse as the seed's filesystem |
| `records` | a kiln-seams `Records` store that crashes at a chosen commit, just before it takes effect or just after: what a journal's crash-at-every-write test runs on |
| `trace` | what a run did, folded into a digest; a replay keeps its last steps in words |
| `seeds` | budgets, seeds a minute, one seed's replay, and running a seed twice (two runs that differ fail on their own) |

## kiln's models

| Model | What of kiln's it runs | Its nemesis | What it checks |
| --- | --- | --- | --- |
| sync | a synced repo's copies on two or three machines and the server's member: `sync::Copy`, `Feed` and `Index` as hyper-durable replicas, `sync::server::take_steps`, the runner's and server's shells; each copy on a simulated disk through `sync::fs` | frames lost and cut off on the sync lane (its order kept, as a QUIC stream keeps it), contents' transfers lost; runners and the server crashing; power losses; a crash at a chosen write; fsync and write errors; full disks; people editing all the while, a file's size often kept | nothing spreads that nobody wrote; every copy converges on the log's repo; no one's last write is lost |
| moves | the same, with an agent session moving between the machines: `sync::runner`'s `look_now_at`, `left_at` and `arrived`, as `harness::session`'s `files_out` and `files_in` use them | the sync model's | a session leaves no write of its own behind, goes on with each in its folder, and every move finishes |
| goal driver | `goals`' `adopt` and `step` as the driver's loop runs them, on a simulated clock, through `SimHarness`, its store inline (a crash at each of its waits) | the server crashing at any wait; machines away; harnesses starting again; usage limits; people clearing goals | one goal under way, in order, ended ones never back; nothing to a paused agent; no goal again within 30 s unless the agent was cut off from it; every stack runs to its end |

## The bugs it found

Each is fixed in a commit of its own, with the seeds that found it kept as its regression test
(and a test of the contract it broke), each seed having replayed the failure exactly, twice,
before the fix.

| # | Bug | Found by | Its regression tests |
| --- | --- | --- | --- |
| 1 | Sync wrote other machines' changes into a folder through a temporary file renamed over the name unsynced, then saved state saying the folder held them: a power loss left the name empty, zeroed or torn, and the next look spread that to every copy as an edit (or, size and time unchanged, kept it for good). Now a file's bytes are fsynced before its name, and the folders whose names changed before the state | sync seeds 3 and 163 (26 of 200 failed), moves 57 and 90 | `sim::sync`: `the_seeds_where_a_power_loss_tore_a_file_sync_had_written_converge`, `a_file_sync_wrote_holds_it_after_a_power_loss_once_its_folders_are_synced` |
| 2 | A write failing part-way through applying a step (an I/O error, a full disk) left the step taken by the copy's tree with the rest of its files unwritten, for good: tried again, the tree had it. Now a step is applied to the copy's state all or nothing | sync seeds 184 and 125, moves 136 and 170 | `sim::sync`: `the_seeds_where_a_write_failed_part_way_through_a_step_converge`, `a_step_a_write_failed_part_way_through_is_applied_whole_when_tried_again` |
| 3 | A file written again just after a look, keeping its size, keeps its time too where times are stamped by the second (or the clock's tick): no look after saw the change. A moving session left its machine with its last write not in the log. Now a file changed within 3 s of a look is read again at the next one (git's racily clean entries) | moves seed 2000, sync 1848 | `sim::sync`: `the_seeds_where_a_change_kept_its_file_s_size_and_time_converge`; `sync::scan`: `a_change_that_keeps_a_file_s_size_and_time_just_after_a_look_is_still_seen` |
| 4 | A copy's save failing after it proposed a step (a full disk, an I/O error), the runner sent the step anyway; a crash then lost the copy's record of it, and its next step took the same number, which the server had taken: that step was let go as sent again, its changes pending for good. A moving session left with its last write never in the log. Now a step goes to the server only once the copy's saved state holds it (`Feed::sendable`), and a look saves again while one doesn't | moves seeds 3174 and 3083 | `sim::sync`: `the_seeds_where_a_step_went_out_before_it_was_saved_converge`, `a_step_goes_to_the_server_only_once_the_copy_s_saved_state_holds_it` |
