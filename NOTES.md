# CP-M2 working notes (branch cp/m2-seams; delete this file before landing)

Goal: master plan 01a157fddb8276a2996246ea931c4808, CP-M2 (shared seams). Started on Ada's /goal;
Kiln Distro (adalundhe-61) asked for it too, as its helper. Pushing needs Ada's word.

## Where it stands (2026-10-20 evening)
- Branch cp/m2-seams, cut from main 5806c0d ("main at the goal's start"), head 6451883 with main
  (8ccd24e) merged; worktree /home/adalundhe/Projects/kiln/.claude/worktrees/critical-path.
- Gate at 6451883 on fedora (~/.cache/kiln-cp/gate-m2/6451883/): clippy Linux + macOS + Windows
  (zig) clean; cargo test 1464 pass + 1 load flake (w14_r05, "server didn't start" at load 196)
  passed rerun alone; vendor checks pass; UI npm ci/typecheck/lint/build pass (rerun with node on
  PATH: the systemd unit lacked npm).
- Timings (rounds-6451883.txt, 15 alternating rounds, CPU >= 92% idle): goals 0.190 -> 0.060 s
  median (-68%), host 0.590 -> 0.290 (-51%); best -71% / -57%. Host median varies 49-59% with
  fedora's process count (its tests scan /proc).
- Sleeps in goals/host tests: 1 -> 0 (grep thread::sleep|rt::sleep over their test modules).
- Adversarial verifier (subagent, brief ~/.cache/kiln-cp/m2-verify-brief.md): running.
- Change request draft: ~/.cache/kiln-cp/cr-m2.md (goes in the landing merge commit's message).

## What's where
- crates/kiln-seams: ten seams (trait + Sim* + conformance cases, 93 cases), tests/doc_truth.rs.
- docs/seams.md: contracts, clause ids = case ids; the doc-truth test holds them together.
- kiln: rt.rs RtClock/RtSpawn (+ suites on real time); harness/port.rs Hub as HarnessPort (passes
  HARNESS-1/2/4; 3/5 wait on CP-T3.2/3.3); goals/mod.rs Driver (store, clock, shutdown, port, hub
  for told/elsewhere) + back-off test; goals/keep.rs via the port; harness/host.rs with_clock,
  looks/saves counters, Host::until; survivors stop polls 1 ms doubling to 100 ms.
- Test speed: scratch hubs (unsynced PKI, store + keys on the blocking pool, conversations.off for
  tests that don't need it), Tasks::open writes only on change, SQLite opt-level 2 in dev,
  .cargo/config.toml LIBSQLITE3_FLAGS=-DSQLITE_DEFAULT_MEMSTATUS=0 (stores in parallel threads
  serialized on SQLite's memory-stats lock).

## Left
- The verifier's report; fix whatever it finds.
- Metrics: names proposed to Kiln Metrics (kiln_goal_sends_total, kiln_goal_ends_total,
  kiln_harness_watch_looks_total, kiln_harness_records_written_total); no answer yet; not exported.
- Landing: merge origin/main, re-gate what changed, put cr-m2.md in the merge message, delete this
  file, push on Ada's word (holds from adalundhe-33 come and go: ask before pushing).
- Live: the next production deploy carries it (Ada's call, through the deploy trains).
