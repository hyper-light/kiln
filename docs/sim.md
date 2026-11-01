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

The model's test is `sim::sync::tests::sync_converges_and_loses_nothing_under_the_nemesis`,
`sim::sched::tests::the_engine_and_runners_hold_their_capacity_and_settle_every_leg_under_the_nemesis` or
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
| moves | the same, with an agent session moving between the machines: `sync::runner`'s `look_now_at`, `left_at` and `arrived` (the copy it goes to has saved the log through where it left), as `harness::session`'s `files_out` and `files_in` use them | the sync model's | a session leaves no write of its own behind, goes on with each in its folder, and every move finishes |
| goal driver | the goal driver's own round of the stacks (`Driver::round`, adopting, resuming and stepping them) each tick, as `Driver::drive` runs it, on a simulated clock, through `SimHarness`. Its store is inline, so each call is a wait a crash can come at | the server crashing at waits the seed picks (up to 0.8% of the driver's polls) and between them; machines away; harnesses starting again; usage limits; people clearing goals | one goal under way, in order, ended ones never back; nothing to a paused agent; no goal again within 30 s unless the agent was cut off from it; every stack runs to its end |
| scheduler | kiln's CI engine (`kiln_ci::sched::Scheduler`) on the server's node beside its store, and runners taking or refusing what it grants by the runner's own admission (`agent::admits`) and keeping their leases by the runner's own keeper (`runner::lease::Leases`). The model plays the dispatcher's and hub's shells around them (docs/needs.md E3.8): each grant in the store before its `Assign` goes, fenced and leased; a runner renewing what it holds with each heartbeat, and the answer saying what isn't its own; refusals, writes and results taken only under their leg's live grant, the rest refused and the runner told once to stop that grant; a runner stopping a stop grant at its lease's end, by its own clock; each runner's connection an ordered lane a lost frame breaks, a runner hearing nothing connecting again with what it runs (`Hello.running`), what it no longer has lost, what isn't its own stopped; results sent again until `Received`; a runner silent past its grace taken as lost; the server started again in the next epoch, rebuilding from the store as the dispatcher does (W05-R25). Since docs/needs.md E4.6 it backfills (W05-R39): each leg has a p90, from three fifths of how long it takes to a fifth past it, and among them come a big item or two, wanting a runner whole, and gangs of two or three (W05-R40), drawn from a stream of their own so the seeds' other draws are as they were | frames lost (each breaking its connection), doubled (a runner's, and the server's answers: never its commands, which its command channel applies once) and late; runners cut off from the server; the server crashing and starting again, its memory gone and its store kept; runner processes paused, often past their lease (Kleppmann's pause); runners' machines asleep, their jobs with them, their lease clock counting the sleep; runners refusing what they're sent, draining, while an upgrade is pending | at every step: no runner holds more than its capacity (W05-R03, counted plainly from its jobs). G2: no two grants share a fence and each is greater than every one before; a leg is granted only while none of its grants is live; whatever the server takes (a refusal, a write, a result, a renewal) names its leg's live grant, from that grant's runner; a stop grant never acts (writes or ends) past its runner's lease end, nor after a later grant of its leg has acted. G10: a runner acts only on a grant the store has had; the engine holds each leg's grant and attempt as the store does, through every restart; an attempt that ended is never granted again. G4: no gang runs in part, some of its members under a grant while others wait in the store. G6: an item that held one room kept for it until it started, started by its bound, the room's time plus how far the grants it waited for (on its runner, or its gang's members' runners, expected to end by then) ran past their expected ends as the store saw them end, each no further than its hard end; room that goes (worked out again later or elsewhere, its runner gone, a class before its own granted into it, a restart) starts the bound again from the next. Once the nemesis stops, every leg settles, but one left waiting, as W05-R13 says, for another runner than the only one big enough, which refused it (counted) |

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
- **The scheduler's server** is one process beside one store: a crash keeps all the store has, as SQLite keeps it, and nothing else, and its epoch rises with each start, as `runs::next_epoch` raises it with the store's group off. The group itself, and a failover from one node to another, aren't modelled; nor are pools, warm placement, windows kept for legs (W05-R12), groups, v1's dispatcher beside the engine, a machine's runner process handing over to another, or a grant's tokens (W10-R03), which die with the grant by the same table the model plays. The model's runner looks at its leases at each lease's end, where kiln's looks every half second, within the tenth of the lease it holds back; its jobs act only by writing and ending.

## The bugs it found

Each is fixed in a commit of its own, with the seeds that found it kept as its regression test,
and a test of the contract it broke. Each seed replays its failure from a clean build of the commit
just before the fix (or, for the bugs found by one sweep, the commit it ran on), to the same output
byte for byte, twice. And each fix's core undone at the head, its seeds' test and its contract's
fail: they catch it still.

| # | Bug | Found by | Its regression tests |
| --- | --- | --- | --- |
| 1 | Sync wrote other machines' changes into a folder through a temporary file renamed over the name unsynced, then saved state saying the folder held them: a power loss left the name empty, zeroed or torn, and the next look spread that to every copy as an edit (or, size and time unchanged, kept it for good). Now a file's bytes are fsynced before its name, and the folders whose names changed before the state | sync seeds 3 and 163 (26 of 200 failed), moves 57 and 90 | `sim::sync`: `the_seeds_where_a_power_loss_tore_a_file_sync_had_written_converge`, `a_file_sync_wrote_holds_it_after_a_power_loss_once_its_folders_are_synced` |
| 2 | A write failing part-way through applying a step (an I/O error, a full disk) left the step taken by the copy's tree with the rest of its files unwritten, for good: tried again, the tree had it. Now a step is applied to the copy's state all or nothing | sync seeds 184 and 125, moves 136 and 170 (and, as later fixes moved those runs off it, sync 68 and 119, moves 10 and 16 keep it caught) | `sim::sync`: `the_seeds_where_a_write_failed_part_way_through_a_step_converge`, `a_step_a_write_failed_part_way_through_is_applied_whole_when_tried_again` |
| 3 | A file written again just after a look, keeping its size, keeps its time too where times are stamped by the second (or the clock's tick): no look after saw the change. A moving session left its machine with its last write not in the log. Now a file changed within 3 s of a look is read again at the next one (git's racily clean entries) | moves seed 2000, sync 1848 | `sim::sync`: `the_seeds_where_a_change_kept_its_file_s_size_and_time_converge`; `sync::scan`: `a_change_that_keeps_a_file_s_size_and_time_just_after_a_look_is_still_seen` |
| 4 | A copy's save failing after it proposed a step (a full disk, an I/O error), the runner sent the step anyway; a crash then lost the copy's record of it, and its next step took the same number, which the server had taken: that step was let go as sent again, its changes pending for good. A moving session left with its last write never in the log. Now a step goes to the server only once the copy's saved state holds it (`Feed::sendable`), and a look saves again while one doesn't | moves seeds 3174 and 3083 | `sim::sync`: `the_seeds_where_a_step_went_out_before_it_was_saved_converge`, `a_step_goes_to_the_server_only_once_the_copy_s_saved_state_holds_it` |
| 5 | A step's version of a file was kept back from a copy whose file was changed there, unproposed; when that change was then undone (or the file deleted) before a look proposed it, the folder was as the copy last synced it, so no look proposed anything, and the step was taken, so nothing brought its version: the copy kept the old file (or none) for good. Now a copy notes what it kept back (`State::behind`) and brings the log's version at the look that finds the change gone | sync seed 1331 | `sim::sync`: `the_seed_where_a_kept_back_version_never_came_converges`, `a_kept_back_version_comes_once_the_change_here_is_undone_or_deleted` |
| 6 | The contents store renamed what a look put in it into its folders and never synced them. A power loss on a disk that keeps each folder's names apart until that folder is synced, as POSIX allows, could lose them while the state naming them, saved and synced, survived: the copy's steps carrying them could never be sent, and its changes stayed pending for good. Found once the disk model's names were POSIX's (a fifth of the seeds failed). Now the store notes the folders it changed, and a copy syncs them with its own before its state names what's in them; then, as sync seed 4079 found, also the folders of contents a process put there before a crash, which the next look finds and doesn't put again | sync seeds 12 and 14, moves 12 and 14; then sync 4079 | `sim::sync`: `the_seeds_where_contents_a_look_kept_were_lost_converge`, `contents_a_look_kept_outlive_a_power_loss_once_the_state_naming_them_is_saved` |
| 7 | A file sync wrote into folders made for it, its first try failing after it made them (an fsync error), was renamed in on the next try, which found the folders there and synced only the file's own, not their places in the folders above. A power loss took the folders and the file while the state said the folder held it: the next look proposed it deleted, and a person's last write was lost, or a moving session went on without its files. The same follows folders a process made before a crash. Now a write syncs its folder and every one above it to the copy's root (each once a save) | sync seed 3277, moves 385 | `sim::sync`: `the_seeds_where_a_file_s_new_folders_were_never_synced_converge`, `a_file_in_folders_made_for_it_holds_after_a_power_loss_whoever_made_them` |
| 8 | A moving session went on on a copy once its folder had taken the session's files, though the copy's saves of that were failing (fsync errors). A power loss put the copy's state back behind them, so the session's next writes there went to the log as new files against its own older versions: conflicts, the log keeping the older version at the name, and the session went on elsewhere without its latest. Now a session goes on only once the copy's saved state holds the log through its mark (`Standing::saved`) | moves seed 1000807 | `sim::sync`: `the_seed_where_a_session_went_on_before_its_files_were_saved_converges`, `a_session_goes_on_only_once_the_copy_s_saved_state_holds_its_files` |
| 9 | A file a runner wrote into its folder, crashing before it synced the folders, was found in place by the next process ("here already") and taken as synced, no folder synced. Its state saved, a power loss took the name, and the copy proposed the file deleted: a moving session went on without it. Now a version found in place has its folders synced as a write's are | moves seed 738 | `sim::sync`: `the_seed_where_a_file_found_in_place_was_never_synced_converges`, `a_file_found_in_place_holds_after_a_power_loss_once_the_folders_are_synced` |
| 10 | A leg lost with its runner was retried on another when one could take it, else on the same one (W05-R16), but "could take it" asked only that another be eligible and connected, not that it be big enough: a leg holding a share of 4 CPUs, the other runner having 2, waited for good for that one, its own runner back and idle, the reason saying `capacity`. Now another runner counts only when it could hold the leg (a share within its size; in resource mode, what the leg asks within its capacity × overcommit) | scheduler seeds 48, 106, 151 and 196 (4 of its first 200); in the model as committed, 222, 284, 699 and 902; in E3.8's, 228, 678, 893 and 1155 (7 of its first 2,000 fail so with the fix undone) | `sim::sched`: `the_seeds_where_a_lost_leg_waited_for_a_runner_too_small_settle`; `kiln_ci::sched`: `w05_r16_a_lost_leg_goes_back_when_no_other_runner_could_hold_it` |
| 11 | The dispatcher's rebuild after a restart (W05-R25) queued each leg the store had queued at its first attempt, whatever attempt the store had: the engine's `LegIn` carried none. A leg whose first attempt was lost with its runner, waiting for its second, came back as its first: an attempt that had ended was granted again (G10), the claim wrote over its record (its failure reset to running), and its retry policy counted from one again. Now `LegIn` carries the attempt the store has, and the engine queues the leg at it | found reading the dispatcher's rebuild while modelling the server's restart (docs/needs.md E3.8); with the fix undone, 424 of the scheduler model's first 2,000 seeds fail as the server starts again, the first 0, 9, 10 and 11 (G10: the store has a leg queued at its second attempt, the engine at its first) | `sim::sched`: `the_seeds_where_a_restart_queued_a_leg_at_its_first_attempt_again_hold`; `kiln_ci::sched`: `w05_r25_a_leg_queued_after_a_restart_keeps_its_attempt`; `ci::bench`: `w05_r25_a_queued_leg_keeps_its_attempt_through_a_restart` |
| 12 | On a runner whose size isn't known, a share takes the whole runner (W05-R03), as the engine claims one there; but the rule both dispatchers and a runner's own admission use (`room`) gave an ordinary leg every slot beside a share held there. The engine never sent one, but a share whose grant was lost (its runner paused past its lease) runs on to its end, and the engine gives its runner another leg: the runner took it, and ran both. Now a share held on such a runner leaves no room beside it | scheduler seeds 200, 390, 1364 and 1652 (4 of its first 2,000), once E3.8's model let shares run on after their grant | `sim::sched`: `the_seeds_where_a_runner_of_no_known_size_took_a_leg_beside_a_share_hold_their_capacity`; `kiln_ci::sched`: `w05_r03_a_share_on_a_runner_of_no_known_size_holds_it_whole`; `agent`: `a_runner_admits_work_beside_what_a_bench_holds`; `hub`: `a_v1_bench_holds_its_share_and_jobs_run_beside_it` |
| 13 | A stop item whose grant was lost with its runner (silent past its grace, its lease still running) waits for its next attempt until the grant's expiry and the lock-delay (W05-R34), by which its runner has stopped it by its own clock. The engine kept that wait in memory only, the store having the leg queued at its next attempt: a restart within it granted the next attempt at once, while the runner, cut off, went on with the one before, and both acted (G2). Now a stop item queued at a later attempt, as only a restart's rebuild queues one, waits out a whole lease and the lock-delay first, past the end of any lease from before | scheduler seeds 44084, 112073, 191361, 312354, 758005 and 772989, 6 of E3.8's million | `sim::sched`: `the_seeds_where_a_restart_granted_a_stop_item_again_within_its_lock_delay_hold`; `kiln_ci::sched`: `w05_r34_a_stop_item_queued_again_after_a_restart_waits_out_a_lease` |
| 14 | A gang counted as waiting for room when it couldn't start for room kept for another of its class (W05-R39, R40): of two whole gangs of a class, each with room only beside the room kept for it, the one holding the class's room didn't wait, so the other was its first and took that room, and so on for good, the engine never done with one input (a seed ran on with no end). A gang held off only by a leg's room took that room too, and started past its time (G6). Now a gang waits for room as a leg does, by room alone, the room kept for others aside; and one passed over is tried again as soon as the room worked out for its class lets it, before those behind it | scheduler seeds 1201 and 11388 (each ran on alone past 20 s), 5000 (G6), of E4.6's probe of seeds 0 to 124,999 in release, where 14 of its 25 ranges of 5,000 ran on | `sim::sched`: `the_seeds_where_gangs_took_their_class_s_room_while_they_had_room_hold`; `kiln_ci::sched`: `w05_r40_two_whole_gangs_of_a_class_start_in_turn_not_taking_its_room_from_each_other` |
| 15 | A restart rebuilds a gang from the members still running (W05-R25, R40). One whose mate had settled before it came back as though none of its start had ended: lost then, it took its gang to the next attempts, alone, and waited for good for a mate that wouldn't come. Now a start with fewer members than make the gang has had one end, and a member lost after ends failed, `runner-lost` | scheduler seed 5147 of E4.6's probe | `sim::sched`: `the_seed_where_a_member_was_tried_again_alone_after_a_restart_settles`; `kiln_ci::sched`: `w05_r40_a_member_lost_after_a_restart_its_mate_ended_before_is_not_tried_alone` |
| 16 | Room kept for a leg on the runner it was lost on, while no other could take it (W05-R16, R39), stood once another was back and the leg stayed off its own: the leg went elsewhere, past its room's time (G6), and what waited behind that room left the runner idle (seed 5607's for 17 s, five legs behind). Now room goes once its runner wouldn't take its item | scheduler seeds 5607 and 6165 of E4.6's probe | `sim::sched`: `the_seeds_where_room_stood_kept_where_its_leg_wouldn_t_go_hold`; `kiln_ci::sched`: `w05_r39_room_kept_where_its_leg_now_stays_off_is_dropped` |

## The planted bugs

T-T6.1.1 plants a lost write after a torn page in a test build, as the first bug was:
`Copy::planted_unsynced` renames what sync writes into a folder in with its bytes unsynced and its
folder never synced. `sim::sync`'s `the_planted_lost_write_after_a_torn_page_is_found` runs the CI
budget with it, and passes once the checker finds it: a power loss tears, zeroes or loses such a
file, and contents nobody wrote spread, a last write is lost, or a copy stays apart from the log.
The seed that finds it must pass without the plant.

docs/needs.md E3.8 plants four bugs in the scheduler's model, one at a time
(`sim::sched::Mutation`), each the kind of slip fencing and leases are there to catch: a write
taken by its leg alone, whatever grant it names; a renewal that renews every grant a runner names,
whatever its attempt; a restart that doesn't raise the epoch; a stop grant that doesn't stop
itself at its lease's end. E4.6 plants two more, in the engine itself (kiln-ci's `mutants` feature,
which kiln's test builds alone turn on): a backfill granted past the room kept for another where it
doesn't fit beside it (G6 catches it: that item starts past its bound), and a gang started without
its last member (G4). Each has a test of its own (`a_write_taken_by_leg_alone_is_caught` and the
five beside it) that passes once a seed of the 200 `cargo test` runs fails with it, and names that
seed: `KILN_SIM_SEED=<seed>` replays it alone, with its last steps.
