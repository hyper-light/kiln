# ci/sim: kiln's own CI, replayed

W25 step 1 (plan "Demand-driven CI: adopt, cover and stagger instead of cancel"). The 7 days of
kiln's v1 CI from Oct 18 to 25, 2026 (1,042 runs) are replayed under six supersession policies
by `kiln_ci::sched::sim::replay`, and compared.

## Files

| File | What |
|---|---|
| `runs_fetch.py` | Fetches a repository's v1 runs and jobs from a kiln server's API into a JSON file (read only; the admin token comes from `kiln token` and is never printed or kept). |
| `runs_analyze.py` | The measured week: runner time on superseded jobs, queue waits, how many pushes to main concluded, the gaps between verdicts. |
| `runs_cancel_progress.py` | How far superseded jobs had got when they were cancelled. |
| `load_history.py` | Each runner's CPU and load over the last hour. |
| `export_trace.py` | Turns the fetched runs and the repository's git history into `trace.json`: trimmed and anonymous. |
| `trace.json` | The trace the replayer reads (below). |
| `trace_check.py` | Recomputes `runs_analyze.py`'s figures from `trace.json`, and checks the trace is anonymous. |
| `replay_check.py` | An independent Python replay of P0 from `trace.json`: the Rust replayer's cross-check. |

The replayer is `kiln_ci::sched::sim::replay` (`crates/kiln-ci/src/sched/sim/replay/`): pure, with no
clock, disk or randomness beyond a seed, and the same seed gives the same outcome, byte for byte.
`cargo run -p kiln-ci --example replay -- ci/sim/trace.json --sweeps` prints the report and the
sweeps below. `cargo test -p kiln-ci sched::sim::replay` checks P0 against the measured week for
seeds 1-8, the synthetic lane, each policy's mechanisms, and that this README's report is current.

```sh
python3 ci/sim/runs_fetch.py hyper-light/kiln 7 /tmp/cires/runs.json   # on a machine with `kiln token`
python3 ci/sim/runs_analyze.py /tmp/cires/runs.json
python3 ci/sim/export_trace.py /tmp/cires/runs.json ci/sim/trace.json
python3 ci/sim/trace_check.py ci/sim/trace.json /tmp/cires/runs.json
python3 ci/sim/replay_check.py ci/sim/trace.json 1 2 3 4 5 6 7 8
```

## trace.json

One JSON object; `runs` holds one run per line. Times are whole seconds from the window's
start; `end` is when the runs were fetched. Fields at their defaults (null, false, empty, an
attempt of 0) are left out.

- `measured`: the figures the replay is calibrated against (`runs_analyze.py`'s): the share of
  runner time on jobs cancelled as superseded, and how many of the ci pushes to main concluded.
- `pools`: runner capacity by label. A pool with `slots` is a machine: a job goes to one that
  carries all its labels and has a free slot; an `exclusive` job only to an empty one, which it
  then holds alone, nothing being held back for it while it waits (v1's `pick_agent`). The `k8s`
  pool is the jobs namespace's quota: each job's `req` (millicores, MiB) must fit beside the
  others'.
- `commits`: each commit the runs built, in order of first push, with `keys`: for each input
  class, the first 8 hex digits of a SHA-256 over the git tree ids of the class's paths (rust:
  crates, Cargo.toml, Cargo.lock, vendor, .cargo, docs, AGENTS.md; ui: ui; vendor: vendor,
  scripts; binary: rust's without docs, plus ui; uichecks: binary's plus ci/ui; release: binary's
  plus ci/release; image: binary's plus Dockerfile and deploy; bench: rust's without docs or ui).
  Two commits with the same key built the same inputs.
- `runs`: `workflow`, `event`, `branch`, `group` (the concurrency group, without the repository)
  and `cancel_in_progress` (as the workflow file at the run's commit had it: the newer run's
  setting decides, as v1's does), `at` (created), `commit` (an index into `commits`), `status`,
  `finished`, and `jobs`:
  - `key`, `labels`, `needs`, `class` (the input class keying it), `exclusive`, `req` (k8s only,
    from the workflow file at the run's commit);
  - `queued`, `started`, `finished`, `status`, `why` (`superseded`, `user`, `cancelled`,
    `dependency`, `timeout`, `step`, `upload`), `attempt`, `failed_step`;
  - `shared`: the durations of the steps another pipeline's work could replace (P5): `ui` (the UI
    build) and `glibc` (`cargo build --release --locked -p kiln`).

Left out: run and job ids, authors, messages, shas, repository ids and machine names.

### Where the capacity comes from

| Pool | Capacity | Source |
|---|---|---|
| `server` | 8 slots; labels baremetal, local, bench, linux | the server machine's local agent (`slots = 8`); in the trace it ran every `[baremetal, local]` and `[baremetal, bench]` job, up to 8 at once |
| `linux` | 8 slots | the other Linux runner (32 cores, kiln's default of one slot per 4); ui-checks ran there |
| `mac-1`, `mac-2` | 2 slots each | the two MacBooks that built macOS releases, at most 2 at once each in the trace (the Mac Studios lacked node until Oct 24, and their builds failed at once) |
| `k8s` | 18 CPUs, 36Gi, 64 pods | the jobs namespace's ResourceQuota, shared with other repositories; each k8s job also keeps its recorded start delay, which carries that sharing |

## The replay

Every run arrives when it did, with its own jobs, `needs` and labels. Each job takes its recorded
duration and outcome where it finished. Where the trace doesn't say (a job cancelled or never
started), the value is drawn from the nearest completed runs of the same job, right-censored: a job
cancelled after `t` seconds takes longer than `t`, and a pod that hadn't started after `w` seconds
waits longer than `w`. Drawn without that condition, the replay concludes 62% of ci's pushes, not
the week's 51%. Every job draws once per seed from a stream of its own, so every policy meets the
same draws.

- **P0**, today: each run's concurrency group as the workflow at its commit had it (ci and release
  cancel in progress; the image finishes from 2026-10-23 20:41 UTC; bench has no group).
- **P1**, image.yaml's setting for every workflow: the run in progress finishes, and the newest
  pending run replaces older pending ones (bench grouped too).
- **P2**, adopt and withdraw: each job of a push is a demand keyed by its inputs at that commit (the
  `keys` above). A newer push withdraws its lane's older demands and cancels nothing itself. A demand
  reuses a finished result with its key, or adopts the execution in flight with it, or runs. Work no
  live demand wants is dropped if it hasn't started. Once running, it finishes, unless live work waits
  for its capacity and it has more than `finish_within` left; then it is cancelled, and a cancelled
  k8s job's progress goes to the next run of its job on the lane (`warm_credit`).
- **P3**, P2 plus verdicts, decided by `kiln_ci::verdict` (docs/ci.md W28-R50 to R53): each job is
  a check of its own, and a commit affects the jobs whose input keys it changed. A job a commit
  could affect, with no result of its own, is covered by the nearest later commit's result for it (a
  pass, or a failure in the step already failing at its base). A new failure's suspects are the
  commits since its base that changed the job's inputs, the failing one among them: bisected in
  ceil(log2 n) runs at low priority for n of them (one suspect is the culprit at once), the culprit
  named and the rest cleared when the bisection ends.
- **P4**, P3 plus admission: a push waits `quiet` while its pools are saturated, a burst never past
  `max_wait`; exclusive jobs and Mac jobs run on a cadence (the newest commit when the resource
  frees, never cancelled for other work), and an exclusive job's machine takes no new work while it
  waits.
- **P5**, P4 plus sharing: one UI build per UI tree (ci's ui job, release's UI steps, ci's
  release-binary and ui-checks), and one `cargo build --release -p kiln` per source tree (ci's
  release-binary and bench's ci-bench).

## The report

A verdict is when a commit is answered: under P0 to P2 its own run concluding; from P3 on, when
`kiln_ci::verdict` answers it (every job its diff could affect direct or covered, or the commit
named a culprit), counted from when it was answered for good: a covered verdict a nearer commit's
new failure takes back counts again only once it is answered again. "Longest gap between verdicts" is `runs_analyze.py`'s (it
includes nights without pushes); "longest wait for any verdict" is the longest a push waited for
the next verdict on main, its own or a later commit's. Bench wait is from a push to its exclusive
bench job's start. Runner-hours per verdict are the workflow's runner-hours over its commits given
a verdict.

<!-- report -->
Medians over seeds 1-8, with their ranges; ci's commits on main; latency over every push ("never" counted as such).

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| Measured, Oct 18-25 | 84.3 | 48.9% | 2.6 h | never | 6.6 h | 5.0 h | 53 min | 0.37 |
| P0 | 90.3 (88.7–91.1) | 50.4% (50.0%–51.1%) | 49 min (41 min–1.1 h) | never | 6.6 h | 3.9 h (3.5 h–3.9 h) | 51 min (50 min–53 min) | 0.36 (0.36–0.37) |
| P1 | 0.0 | 66.7% (64.1%–68.9%) | 34 min (29 min–36 min) | never | 6.7 h (6.6 h–6.8 h) | 3.0 h (2.7 h–3.1 h) | 32 min (31 min–34 min) | 0.30 (0.29–0.31) |
| P2 | 2.2 (1.8–2.7) | 65.6% (63.3%–66.7%) | 24 min | never | 6.8 h (6.7 h–6.8 h) | 3.5 h (3.3 h–3.5 h) | 20 min | 0.31 (0.30–0.32) |
| P3 | 2.3 (1.8–2.8) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min | 0.21 |
| P4 | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| P5 | 1.2 (0.9–1.4) | 77.4% (75.6%–79.6%) | 22 min (21 min–23 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.8 h (2.8 h–3.1 h) | 7 min | 0.20 |

P0's calibration against the measured week:

- P0 seed 1: runner time 340.4 h, superseded 88.7 h (26.0%) against 25.0%; ci pushes to main concluded 144 of 270 (53.3%) against 137 of 270
- P0 seed 2: runner time 340.3 h, superseded 90.3 h (26.5%) against 25.0%; ci pushes to main concluded 140 of 270 (51.9%) against 137 of 270
- P0 seed 3: runner time 340.3 h, superseded 90.4 h (26.6%) against 25.0%; ci pushes to main concluded 143 of 270 (53.0%) against 137 of 270
- P0 seed 4: runner time 340.0 h, superseded 89.8 h (26.4%) against 25.0%; ci pushes to main concluded 142 of 270 (52.6%) against 137 of 270
- P0 seed 5: runner time 340.3 h, superseded 90.3 h (26.6%) against 25.0%; ci pushes to main concluded 143 of 270 (53.0%) against 137 of 270
- P0 seed 6: runner time 339.2 h, superseded 88.7 h (26.1%) against 25.0%; ci pushes to main concluded 143 of 270 (53.0%) against 137 of 270
- P0 seed 7: runner time 339.6 h, superseded 90.0 h (26.5%) against 25.0%; ci pushes to main concluded 141 of 270 (52.2%) against 137 of 270
- P0 seed 8: runner time 340.9 h, superseded 91.1 h (26.7%) against 25.0%; ci pushes to main concluded 142 of 270 (52.6%) against 137 of 270

Where ci's commits on main without a verdict an hour after their push stood then (W28-R53), medians over seeds with their ranges:

| Policy | a suspect: its bisection still going | pending: its answer's job waiting for a runner | pending: its answer's job running | pending: its answer's job waiting for its needs | pending: its answer's push not made yet | no answer in the window |
|---|---|---|---|---|---|---|
| P3 | 1 (0–2) | 4 (2–4) | 7 (4–8) | 13 (10–16) | 31 (27–35) | 4 (0–5) |
| P4 | 1 (0–2) | 6 (4–9) | 6 (3–8) | 14 (11–16) | 33 (26–38) | 5 (0–7) |
| P5 | 1 (0–2) | 6 (4–9) | 5 (3–6) | 14 (11–16) | 33 (26–38) | 5 (0–7) |

ci on main:

| Policy | Verdict within 60 min | Latency p50 | p90 | Longest gap | Longest wait for any verdict | Runner-h used | Wasted | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| Measured | 48.9% | 2.6 h | never | 6.6 h | 5.0 h | 50.3 | 11.2 | 0.37 |
| P0 | 50.4% (50.0%–51.1%) | 49 min (41 min–1.1 h) | never | 6.6 h | 3.9 h (3.5 h–3.9 h) | 52.0 (51.6–52.1) | 10.8 (10.6–10.9) | 0.36 (0.36–0.37) |
| P1 | 66.7% (64.1%–68.9%) | 34 min (29 min–36 min) | never | 6.7 h (6.6 h–6.8 h) | 3.0 h (2.7 h–3.1 h) | 62.1 (59.4–64.5) | 0.0 | 0.30 (0.29–0.31) |
| P2 | 65.6% (63.3%–66.7%) | 24 min | never | 6.8 h (6.7 h–6.8 h) | 3.5 h (3.3 h–3.5 h) | 55.5 (55.1–56.7) | 0.0 | 0.31 (0.30–0.32) |
| P3 | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 55.9 (55.3–56.7) | 0.0 | 0.21 |
| P4 | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 55.2 (54.7–56.1) | 0.0 | 0.21 |
| P5 | 77.4% (75.6%–79.6%) | 22 min (21 min–23 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.8 h (2.8 h–3.1 h) | 52.4 (51.7–53.3) | 0.0 | 0.20 |

release on main:

| Policy | Verdict within 60 min | Latency p50 | p90 | Longest gap | Longest wait for any verdict | Runner-h used | Wasted | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| Measured | 30.4% | never | never | 8.5 h | 8.2 h | 176.2 | 57.4 | 2.03 |
| P0 | 30.4% (29.6%–31.2%) | never | never | 12.0 h (8.2 h–12.0 h) | 11.1 h (7.4 h–11.1 h) | 182.9 (182.0–183.9) | 61.4 (60.7–62.3) | 2.11 (2.07–2.16) |
| P1 | 55.4% (52.3%–57.3%) | 53 min (50 min–57 min) | never | 7.1 h (6.6 h–7.1 h) | 1.5 h (1.5 h–1.8 h) | 189.7 (186.7–192.3) | 0.0 | 1.06 (1.06–1.08) |
| P2 | 81.9% (79.2%–83.8%) | 32 min (31 min–33 min) | 1.4 h (1.4 h–1.5 h) | 7.1 h (6.6 h–7.1 h) | 1.4 h (1.2 h–1.4 h) | 219.3 (217.4–222.9) | 1.0 (0.5–1.4) | 0.92 (0.91–0.94) |
| P3 | 89.6% (86.9%–92.7%) | 30 min (28 min–31 min) | 1.1 h (56 min–1.2 h) | 7.2 h (6.8 h–7.3 h) | 1.4 h (1.1 h–1.4 h) | 221.3 (219.4–226.6) | 1.0 (0.5–1.6) | 0.87 (0.85–0.88) |
| P4 | 83.5% (81.5%–85.8%) | 32 min (31 min–33 min) | 1.2 h (1.1 h–1.3 h) | 7.2 h (6.8 h–7.3 h) | 1.4 h (1.3 h–2.1 h) | 210.6 (204.9–214.9) | 0.1 (0.1–0.3) | 0.83 (0.81–0.84) |
| P5 | 84.2% (82.3%–86.9%) | 30 min (30 min–31 min) | 1.2 h (1.1 h–1.3 h) | 7.2 h (6.8 h–7.3 h) | 1.4 h (1.3 h–2.1 h) | 204.2 (198.4–207.8) | 0.1 (0.1–0.3) | 0.80 (0.79–0.81) |

image on main:

| Policy | Verdict within 60 min | Latency p50 | p90 | Longest gap | Longest wait for any verdict | Runner-h used | Wasted | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| Measured | 43.4% | 2.6 h | never | 6.8 h | 3.6 h | 76.4 | 15.7 | 0.56 |
| P0 | 43.8% (43.0%–44.9%) | never (4.6 h–never) | never | 6.8 h (6.8 h–8.6 h) | 4.5 h (3.6 h–7.5 h) | 69.7 (69.4–70.2) | 17.9 (17.1–18.5) | 0.52 (0.51–0.53) |
| P1 | 70.2% (68.8%–72.4%) | 36 min (35 min–37 min) | never | 6.6 h (6.5 h–6.6 h) | 1.1 h (56 min–1.2 h) | 80.5 (79.2–82.0) | 0.0 | 0.39 (0.38–0.39) |
| P2 | 87.9% (86.8%–89.3%) | 24 min | 1.1 h (1.0 h–never) | 6.6 h (6.5 h–6.6 h) | 1.3 h (56 min–1.3 h) | 93.2 (91.3–94.8) | 1.1 (0.9–1.5) | 0.38 (0.37–0.39) |
| P3 | 96.7% (96.3%–97.1%) | 23 min (22 min–23 min) | 42 min (40 min–43 min) | 6.6 h (6.5 h–6.6 h) | 52 min (51 min–1.0 h) | 93.8 (92.3–95.8) | 1.1 (0.9–1.5) | 0.35 (0.34–0.35) |
| P4 | 88.6% (86.4%–89.3%) | 27 min (25 min–27 min) | 1.0 h (1.0 h–1.1 h) | 6.6 h (6.6 h–6.7 h) | 2.4 h (2.4 h–2.5 h) | 83.2 (80.8–84.4) | 1.0 (0.8–1.3) | 0.31 (0.30–0.31) |
| P5 | 88.6% (86.4%–89.0%) | 27 min (25 min–28 min) | 1.0 h (1.0 h–1.1 h) | 6.6 h (6.6 h–6.7 h) | 2.4 h (2.4 h–2.5 h) | 82.8 (80.8–83.7) | 1.1 (0.8–1.3) | 0.31 (0.30–0.31) |

bench on main:

| Policy | Verdict within 60 min | Latency p50 | p90 | Longest gap | Longest wait for any verdict | Runner-h used | Wasted | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| Measured | 54.9% | 55 min | 3.2 h | 7.5 h | 3.1 h | 34.8 | 0.0 | 0.18 |
| P0 | 59.2% (57.8%–59.7%) | 50 min (50 min–51 min) | 3.1 h (2.9 h–3.2 h) | 7.5 h | 4.2 h (3.4 h–6.2 h) | 35.6 (35.5–35.8) | 0.0 | 0.17 |
| P1 | 51.9% (48.5%–54.9%) | 57 min (51 min–1.0 h) | never | 7.0 h (7.0 h–7.4 h) | 4.3 h (2.8 h–5.0 h) | 23.2 (22.2–23.5) | 0.0 | 0.16 |
| P2 | 49.0% (48.1%–50.0%) | 1.2 h (1.0 h–1.3 h) | never | 7.3 h | 4.5 h (3.7 h–6.3 h) | 20.0 (19.7–20.3) | 0.2 | 0.19 (0.18–0.19) |
| P3 | 70.9% (69.4%–72.3%) | 41 min (41 min–42 min) | 2.2 h (1.9 h–2.4 h) | 7.3 h (7.3 h–7.8 h) | 4.5 h (3.7 h–6.3 h) | 20.0 (19.7–20.3) | 0.2 | 0.10 |
| P4 | 87.9% (85.9%–88.8%) | 22 min (21 min–23 min) | 1.1 h (1.0 h–1.2 h) | 6.8 h | 2.9 h (2.9 h–3.9 h) | 28.3 (27.8–28.9) | 0.0 | 0.14 (0.13–0.14) |
| P5 | 87.4% (85.9%–88.8%) | 22 min (21 min–22 min) | 1.1 h (1.0 h–1.2 h) | 6.8 h | 2.9 h (2.9 h–3.9 h) | 28.3 (27.8–28.9) | 0.0 | 0.14 (0.13–0.14) |
<!-- /report -->

## Defaults for later steps

From the sweeps below; each varied on the policy it shapes, the others at their defaults (medians
over seeds 1-8).

| Parameter | Default | Why |
|---|---|---|
| `finish_within` | 300 s | On P3, 0 s wastes 4.5 runner-h and gives 79.6% of ci's commits a verdict within 60 min; 300 s, 2.3 h and 79.3%; 600 s, 1.3 h and 78.9%; 1200 s, none and 78.9%. 300 s keeps waste under 1% of runner time, within about a point of the most verdicts. |
| `warm_credit` | 0.5 | No effect measured (2.5 runner-h wasted at 0, 2.4 h from 0.25 to 0.75): once newer pushes withdraw instead of cancel, few k8s jobs are cancelled. The design's half stays. |
| `quiet` | 60 s | On P4: 0 s wastes 8.7 runner-h, as the bench's drain cancels more unwanted work; 60 s, 1.1 h with 76.7% of verdicts within 60 min; 120 s, 1.2 h and 75.9%; 300 s, 1.9 h and 75.9%. |
| `max_wait` | 600 s | No burst of the week was held 5 min: 300 to 1800 s give the same numbers. 600 s bounds a burst at 10 min. |
| cadence | on (exclusive jobs, the macOS release) | The bench waits 7 min (p50) with it, 20 min without, at about 3 points of ci's verdicts within 60 min (76.7% against 79.3%): the drain holds the server's slots that ci's release-binary also needs. |
| `image_compile_share` | 0 | No effect in the replay: the image starts beside release's Linux build, not after it, so there is no finished compile to take. W25's step 7, which has the Dockerfile take release's binary, decides it. |

## What the numbers say

- **P0 reproduces the week.** Runner time on superseded jobs is 26.0-26.7% for seeds 1-8
  (measured 25.0%), and ci concluded 140-144 of 270 pushes (measured 137). The columns it wasn't
  calibrated on agree too: 50.4% of verdicts within 60 min (48.9%), the bench waiting 51 min (53),
  0.36 runner-h per verdict (0.37). Its median latency sits where half the commits never get a
  verdict, so it jumps with the last few runs concluded (2.6 h measured, 49 min replayed).
- **P1** wastes nothing and gives 66.7% of ci's commits a verdict within 60 min, but commits replaced
  while pending get none, and release's 55% is far from P2's 82%.
- **P2** wastes 2.2 runner-h a week (0.6% of runner time; W25's step 4 asks at most 5%), with a
  median latency of 24 min.
- **P3** gives 79.3% of ci's commits a verdict within 60 min (75.9% before `kiln_ci::verdict` decided
  them), and nine in ten one within 2.6 h: still short of the 95% W25's step 5 asks. An hour after
  their push (the table under the report), of the week's 59 or so late commits about 31 waited for
  a push made more than an hour after theirs, 13 for a job whose needs hadn't finished, 7 for a run
  going and 4 for a runner; a bisection held up 1, and 4 had no answer in the window. Commit by
  commit it is capacity, not verdicts: over two hours of steady pushes (runs 140 to 174) no ci rust
  job started at all, each queued for the k8s quota and dropped when the next push came, so no
  commit was answered until a push's job got a pod. Two verdict-side levers were tried and left the
  share where it was (78.9%): keeping each lane's newest running job from being cancelled for other
  work, and letting a newer demand take a withdrawn job's place in the queue. The k8s quota and the
  server's slots are W25's steps 6 and 8 (admission, spreading load).
- **P4** brings the bench's wait to 7 min (W25's step 6 asks at most 15) and waste to 1.1 runner-h,
  at 76.7% of ci's verdicts within 60 min.
- **P5** saves about 6 runner-h a week of release builds and 3 of ci (0.20 runner-h per ci verdict,
  against 0.36 today).
- **The longest gap between verdicts** stays near 6.7 h under every policy: it is the week's longest
  stretch without pushes. The longest wait for any verdict (5.0 h measured) falls to 3.0-3.5 h
  under P1 to P5.

Limits: recorded durations stay fixed whatever runs beside them, so a policy that runs more at once
looks a little better than it would on the loaded server; input keys come from path sets, an
approximation of W24's observed reads; verdicts are by job, not by test.

## Sweeps

finish_within on P3:

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| finish_within 0 s | 4.5 (3.4–4.9) | 79.6% (77.0%–81.9%) | 18 min (18 min–20 min) | 2.6 h (1.9 h–2.8 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.4 h) | 19 min | 0.21 (0.20–0.21) |
| finish_within 120 s | 4.2 (3.2–4.5) | 79.6% (77.0%–81.9%) | 18 min (18 min–20 min) | 2.6 h (1.9 h–2.8 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 19 min (19 min–20 min) | 0.21 (0.20–0.21) |
| finish_within 300 s | 2.3 (1.8–2.8) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min | 0.21 |
| finish_within 600 s | 1.3 (0.8–1.8) | 78.9% (76.3%–81.1%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.8 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 21 min | 0.21 (0.21–0.22) |
| finish_within 1200 s | 0.0 | 78.9% (76.3%–81.1%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.8 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 21 min (20 min–21 min) | 0.21 (0.21–0.22) |

warm_credit on P3:

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| warm_credit 0 | 2.4 (1.9–2.8) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min | 0.21 |
| warm_credit 0.25 | 2.3 (1.8–2.8) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min | 0.21 |
| warm_credit 0.5 | 2.3 (1.8–2.8) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min | 0.21 |
| warm_credit 0.75 | 2.3 (1.8–2.8) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min | 0.21 |

quiet on P4 (max_wait 600 s):

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| quiet 0 s | 8.7 (8.0–9.1) | 78.9% (76.3%–81.5%) | 19 min (19 min–21 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.8 h (2.8 h–3.1 h) | 2 min (2 min–3 min) | 0.21 (0.20–0.21) |
| quiet 60 s | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| quiet 120 s | 1.2 (0.9–1.5) | 75.9% (74.4%–77.8%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.8 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| quiet 300 s | 1.9 (1.2–2.3) | 75.9% (74.4%–77.8%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.8 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |

max_wait on P4 (quiet 60 s):

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| max_wait 300 s | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| max_wait 600 s | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| max_wait 900 s | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| max_wait 1800 s | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |

cadence on P4:

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| cadence on | 1.1 (0.9–1.6) | 76.7% (75.2%–78.5%) | 22 min (21 min–24 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 7 min | 0.21 |
| cadence off | 2.7 (1.8–3.0) | 79.3% (77.4%–81.9%) | 19 min (18 min–20 min) | 2.6 h (1.9 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.9 h (2.9 h–3.1 h) | 20 min (20 min–21 min) | 0.21 |

image_compile_share on P5 (assumed: the trace can't split the image's build):

| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |
|---|---|---|---|---|---|---|---|---|
| image_compile_share 0 | 1.2 (0.9–1.4) | 77.4% (75.6%–79.6%) | 22 min (21 min–23 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.8 h (2.8 h–3.1 h) | 7 min | 0.20 |
| image_compile_share 0.3 | 1.2 (0.9–1.4) | 77.4% (75.6%–79.6%) | 22 min (21 min–23 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.8 h (2.8 h–3.1 h) | 7 min | 0.20 |
| image_compile_share 0.6 | 1.2 (0.9–1.4) | 77.4% (75.6%–79.6%) | 22 min (21 min–23 min) | 2.6 h (2.2 h–2.7 h) | 6.8 h (6.7 h–6.8 h) | 2.8 h (2.8 h–3.1 h) | 7 min | 0.20 |
