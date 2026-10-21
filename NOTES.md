# CP-M2 working notes (branch cp/m2-seams; delete this file before landing)

Goal: master plan 01a157fddb8276a2996246ea931c4808, CP-M2 (shared seams). Started on Ada's /goal;
Kiln Distro (adalundhe-61) asked for it too, as its helper. Pushing needs Ada's word.

## Where it stands (2026-10-20, night)
- Branch cp/m2-seams, cut from main 5806c0d ("main at the goal's start"); main merged at eff8cf2
  (a89cb69); worktree /home/adalundhe/Projects/kiln/.claude/worktrees/critical-path.
- The adversarial verifier (brief ~/.cache/kiln-cp/m2-verify-brief.md, its report
  ~/.cache/kiln-cp/scratch/verifier-report.txt): ALL MET at 6451883 as worded, with items to fix
  before landing. All fixed in 24bcc56..d08056b: SimRecords' snapshot horizon and wait
  (RECORDS-14 added), SimHarness bounded, the Dockerfile copies .cargo/, the host's not-before
  checks on the clock's deadlines, saves() counts writes made, the registry's local host on the
  hub's clock, the registry fixture runs every clause (3 and 5 fail only as unobservable), a hub
  per case, SecretValue zeroed by zeroize, SimSpawnFixture::finish bounded, run's no-deadline
  said, Run documented, doc-truth checks Run's methods and every stated bound, AC-M2.n/T-M2.1
  cited in the tests.
- Gate: rerun on the head after a89cb69 (logs ~/.cache/kiln-cp/gate-m2/<commit>/ on fedora).
  Earlier gate at 6451883: all green but one load flake.
- Timings (rounds-6451883.txt): goals 0.190 -> 0.060 s median (-68%), host 0.590 -> 0.290 (-51%).
  Re-measure on the final head (the host's tests changed).
- Sleeps in goals/host tests: 1 -> 0.
- Change request draft: ~/.cache/kiln-cp/cr-m2.md (goes in the landing merge commit's message).

## What's where
- crates/kiln-seams: ten seams (trait + Sim* + conformance cases, 95 cases), tests/doc_truth.rs.
- docs/seams.md: contracts, clause ids = case ids, bounds = the code's; doc-truth holds them.
- kiln: rt.rs RtClock/RtSpawn (+ suites on real time); harness/port.rs Hub as HarnessPort (holds
  HARNESS-1/2/4; 3/5 wait on CP-T3.2/3.3); goals/mod.rs Driver + back-off test (T-M2.1);
  goals/keep.rs via the port; harness/host.rs with_clock, looks/saves, Host::until.

## Others building on it
- Hex T7 (kiln agent 01a15c10306176b4a096f6c8d3100c54, CP-T7.1) builds on Sandbox and Placement
  (agreed shapes in d964504). Change traits, sims, suites and docs/seams.md together if it asks.

## Left
- Metrics: names proposed to Kiln Metrics (kiln_goal_sends_total, kiln_goal_ends_total,
  kiln_harness_watch_looks_total, kiln_harness_records_written_total); no answer; not exported.
- Landing: gate green, re-measure, cr-m2.md updated, report to Kiln Distro and Ada; on Ada's word:
  fetch, merge main if it moved (re-gate), delete this file, push (ask adalundhe-33 about holds).
- Live: the next production deploy carries it (Ada's call, through the deploy trains).
