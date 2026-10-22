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

A failure says its seed and how to replay it (here the third bug's, before its fix):

```
kiln-sim moves: seeds 2000..=2000 (1) in 0.04 s, 1409 seeds/min on one thread, 1 failed
  seed 2000: 37.637443715s: the session left m2 with its write to agent/notes.md (120 bytes, at 35.987614043s) not in the log through index 23: left behind
    replay: KILN_SIM_SEED=2000
```

## How fast it runs

Each run's report says its seeds a minute on one thread, timed by the simulator itself. Measured
on 2026-10-22 on fedora (an i9-14900HX) with a release build:

- one test thread pinned to one CPU;
- 2,000 seeds a run, from seed 100,000;
- each run started once the machine's 1-minute load average was below 2.

| Model | Seeds a minute |
| --- | --- |
| sync | 7,097 and 6,046 (two runs) |
| moves | 4,921 |

Other work started on the machine during these runs: by the time each ended, the load average was
between 4 and 7. To measure again, build with `cargo test --release -p kiln --lib --no-run`, then
run the test binary it builds:

```
KILN_SIM_SEEDS=2000 KILN_SIM_FROM=100000 taskset -c <cpu> <the binary> --exact <the model's test> --test-threads 1 --nocapture
```

The model's test is `sim::sync::tests::sync_converges_and_loses_nothing_under_the_nemesis` or
`sim::moves::tests::a_moving_session_never_leaves_its_work_behind`.

## What it is

| Part | |
| --- | --- |
| `rng` | SplitMix64, drawn as the sends simulation draws it, with named streams: a source that draws more leaves the others' draws as they were |
| `world` | the seeded scheduler: each node's tasks run on a kiln-seams `SimSpawn`, all on one `SimClock`; the seed picks which node's ready task is polled next, one poll at a time; a crash drops a node's tasks wherever they wait |
| `queue` | a model's events, by when they're due |
| `net` | frames between nodes while the nemesis lasts: datagrams lost, doubled and overtaken; frames on an ordered lane (a QUIC stream) lost but never reordered or doubled; requests lost or late; links partitioned. After it, every frame arrives |
| `disk` | a machine's disk. Data is durable at its file's fsync. Names are POSIX's in half the seeds (a folder's fsync makes durable the changes of names in it, no other folder's) and one journal's in the rest (any fsync makes every change before it durable, as on ext4 and XFS). A power loss keeps what was made durable and a prefix of the rest (nothing in a folder whose own name was lost), and leaves each unsynced file as written, as before, torn at a page, or zeroed. Also: fsync and write errors (EIO); ENOSPC; a crash at a chosen write; modification times as coarse as the seed's filesystem |
| `records` | a kiln-seams `Records` store that crashes at a chosen commit, just before it takes effect or just after: what a journal's crash-at-every-write test runs on |
| `trace` | what a run did, folded into a digest; a replay keeps its last steps in words |
| `seeds` | budgets, seeds a minute, one seed's replay, and running a seed twice (two runs that differ fail on their own) |

## kiln's models

| Model | What of kiln's it runs | Its nemesis | What it checks |
| --- | --- | --- | --- |
| sync | a synced repo's copies on two or three machines and the server's member: `sync::Copy`, `Feed` and `Index` as hyper-durable replicas, `sync::server::take_steps`. The runner's own decisions: its looks (`sync::runner::look_now_at`) and what it sends (`to_send`). The model plays the network around them, as the runner's and the server's shells would. Each copy is on a simulated disk through `sync::fs` | frames lost and cut off on the sync lane (its order kept, as a QUIC stream keeps it); contents' requests lost, and in some seeds sent twice and overtaking one another (a client trying again), their answers lost; runners and the server crashing; power losses; a crash at a chosen write; fsync and write errors; full disks; people editing all the while, a file's size often kept | nothing spreads that nobody wrote; every copy converges on the log's repo; no one's last write is lost |
| moves | the same, with an agent session moving between the machines: `sync::runner`'s `look_now_at`, `left_at` and `arrived`, as `harness::session`'s `files_out` and `files_in` use them | the sync model's | a session leaves no write of its own behind, goes on with each in its folder, and every move finishes |
| goal driver | the goal driver's own round of the stacks (`Driver::round`, adopting, resuming and stepping them) each tick, as `Driver::drive` runs it, on a simulated clock, through `SimHarness`. Its store is inline, so each call is a wait a crash can come at | the server crashing at waits the seed picks (up to 0.8% of the driver's polls) and between them; machines away; harnesses starting again; usage limits; people clearing goals | one goal under way, in order, ended ones never back; nothing to a paused agent; no goal again within 30 s unless the agent was cut off from it; every stack runs to its end |

Not modelled yet, and where they will be:

- **Sends:** Sends T3's own deterministic simulation (`harness/inbox/sim.rs`, with the network nemesis) moves
  onto kiln-sim once both are on main.
- **The harness host's record:** CP-T3.1 replaces the host's JSON file with a journal on `Records`; its
  crash test (a crash at every write) runs on `kiln_sim::records::Crashing`. Today's host runs its
  sessions' tasks on kiln's runtime and real harness processes, which a seed can't replay; the goal
  driver's model drives the host's side of the `HarnessPort` seam (agents restarting, away, paused).

What the models leave out, so what no seed can find:

- **Members' logs and the server's contents:** the runners' and the server's group logs, and the
  server's contents store, are kept in memory. A crash keeps all they wrote; the disk's nemesis
  doesn't reach them.
- **An fsync that failed:** the model lets kiln try it again, and a later one can succeed. Linux
  may have dropped the pages that failed to write.
- **Looks while cut off:** the model's runners look at their folders while cut off from the server.
  kiln's runner waits until it's connected, so the model explores more.
- **New copies:** a copy adopting the group's files (a machine joining with files already in its
  folder) isn't modelled.
- **The runner's sweep** of contents no copy needs isn't modelled. It uses `Feed::named`, which has
  its own test.
- **The goal driver's model has one node, the server's:** the scheduler never chooses between nodes
  there.
- **kiln's temporary files** left in a folder (`.kiln-sync-*.tmp`) are left out of the check.

## The bugs it found

Each is fixed in a commit of its own, with the seeds that found it kept as its regression test,
and a test of the contract it broke. Each seed replays its failure from a clean build of the commit
just before the fix, to the same output byte for byte, twice.

| # | Bug | Found by | Its regression tests |
| --- | --- | --- | --- |
| 1 | Sync wrote other machines' changes into a folder through a temporary file renamed over the name unsynced, then saved state saying the folder held them: a power loss left the name empty, zeroed or torn, and the next look spread that to every copy as an edit (or, size and time unchanged, kept it for good). Now a file's bytes are fsynced before its name, and the folders whose names changed before the state | sync seeds 3 and 163 (26 of 200 failed), moves 57 and 90 | `sim::sync`: `the_seeds_where_a_power_loss_tore_a_file_sync_had_written_converge`, `a_file_sync_wrote_holds_it_after_a_power_loss_once_its_folders_are_synced` |
| 2 | A write failing part-way through applying a step (an I/O error, a full disk) left the step taken by the copy's tree with the rest of its files unwritten, for good: tried again, the tree had it. Now a step is applied to the copy's state all or nothing | sync seeds 184 and 125, moves 136 and 170 | `sim::sync`: `the_seeds_where_a_write_failed_part_way_through_a_step_converge`, `a_step_a_write_failed_part_way_through_is_applied_whole_when_tried_again` |
| 3 | A file written again just after a look, keeping its size, keeps its time too where times are stamped by the second (or the clock's tick): no look after saw the change. A moving session left its machine with its last write not in the log. Now a file changed within 3 s of a look is read again at the next one (git's racily clean entries) | moves seed 2000, sync 1848 | `sim::sync`: `the_seeds_where_a_change_kept_its_file_s_size_and_time_converge`; `sync::scan`: `a_change_that_keeps_a_file_s_size_and_time_just_after_a_look_is_still_seen` |
| 4 | A copy's save failing after it proposed a step (a full disk, an I/O error), the runner sent the step anyway; a crash then lost the copy's record of it, and its next step took the same number, which the server had taken: that step was let go as sent again, its changes pending for good. A moving session left with its last write never in the log. Now a step goes to the server only once the copy's saved state holds it (`Feed::sendable`), and a look saves again while one doesn't | moves seeds 3174 and 3083 | `sim::sync`: `the_seeds_where_a_step_went_out_before_it_was_saved_converge`, `a_step_goes_to_the_server_only_once_the_copy_s_saved_state_holds_it` |
| 5 | A step's version of a file was kept back from a copy whose file was changed there, unproposed; when that change was then undone (or the file deleted) before a look proposed it, the folder was as the copy last synced it, so no look proposed anything, and the step was taken, so nothing brought its version: the copy kept the old file (or none) for good. Now a copy notes what it kept back (`State::behind`) and brings the log's version at the look that finds the change gone | sync seed 1331 | `sim::sync`: `the_seed_where_a_kept_back_version_never_came_converges`, `a_kept_back_version_comes_once_the_change_here_is_undone_or_deleted` |

## The planted bug

T-T6.1.1 plants a lost write after a torn page in a test build, as the first bug was:
`Copy::planted_unsynced` renames what sync writes into a folder in with its bytes unsynced and its
folder never synced. `sim::sync`'s `the_planted_lost_write_after_a_torn_page_is_found` runs the CI
budget with it, and passes once the checker finds it: a power loss tears, zeroes or loses such a
file, and contents nobody wrote spread, a last write is lost, or a copy stays apart from the log.
The seed that finds it must pass without the plant.
