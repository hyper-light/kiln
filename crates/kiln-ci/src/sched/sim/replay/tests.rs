//! W25 step 1: the replay of kiln's recorded week (ci/sim/trace.json) and its policies.

use std::collections::BTreeMap;

use super::draw::draws;
use super::report::{SEEDS, calibration, fingerprint, report};
use super::synth::{Lane, lane};
use super::trace::{Commit, Job, Pool, Run};
use super::verdict::{Kind, verdicts};
use super::*;

/// The recorded week, embedded at compile time (the replay itself reads no disk).
const WEEK: &str = include_str!("../../../../../../ci/sim/trace.json");
/// ci/sim/README.md, which keeps the report.
const README: &str = include_str!("../../../../../../ci/sim/README.md");

fn week() -> Result<Trace, String> {
    Trace::parse(WEEK).map_err(|e| e.to_string())
}

fn job(key: &str, labels: &[&str], ran: Option<(i64, i64)>, status: &str) -> Job {
    Job { key: key.into(), labels: labels.iter().map(|l| (*l).to_owned()).collect(), started: ran.map(|r| r.0), finished: ran.map(|r| r.1), status: status.into(), ..Job::default() }
}

fn push(workflow: &str, at: i64, group: Option<&str>, cancel_in_progress: bool, jobs: Vec<Job>) -> Run {
    Run { workflow: workflow.into(), event: "push".into(), branch: "main".into(), group: group.map(str::to_owned), cancel_in_progress, at, status: "succeeded".into(), jobs, ..Run::default() }
}

fn machine(name: &str, labels: &[&str], slots: u32) -> Pool {
    Pool { name: name.into(), labels: labels.iter().map(|l| (*l).to_owned()).collect(), slots: Some(slots), ..Pool::default() }
}

fn tiny(pools: Vec<Pool>, runs: Vec<Run>) -> Trace {
    Trace { end: 1_000_000, measured: None, pools, commits: vec![Commit::default()], runs }
}

/// The first start of each run's first job.
fn starts(out: &Outcome) -> Vec<Option<i64>> {
    (0..out.runs.len()).map(|r| out.execs.iter().filter(|e| e.run == r).filter_map(|e| e.started).min()).collect()
}

/// W25 step 1, acceptance: P0 replays the recorded week within 5 points of the measured shares,
/// for every seed: runner time on superseded jobs (25.0%) and ci pushes to main concluded (137 of
/// 270).
#[test]
fn w25_s1_p0_replay_matches_the_measured_week() -> Result<(), String> {
    let t = week()?;
    let m = t.measured.clone().ok_or("the trace has no measured figures")?;
    let want = f64::from(m.ci_concluded) / f64::from(m.ci_pushes);
    for seed in 1..=8 {
        let out = replay(&t, &Policy::p0(), seed);
        let (done, n) = out.concluded(&t, "ci");
        let concluded = done as f64 / n as f64;
        let line = calibration(&t, &out);
        assert_eq!(n, m.ci_pushes as usize, "{line}");
        assert!((out.superseded_share() - m.superseded_share).abs() <= 0.05, "{line}");
        assert!((concluded - want).abs() <= 0.05, "{line}");
    }
    Ok(())
}

/// W25 step 1, acceptance: pure and deterministic. The same seed gives the same outcome, byte for
/// byte; another seed differs only where draws enter.
#[test]
fn w25_s1_same_seed_same_outcome() -> Result<(), String> {
    let t = week()?;
    for p in Policy::all() {
        let a = fingerprint(&replay(&t, &p, 1));
        assert_eq!(a, fingerprint(&replay(&t, &p, 1)), "{}", p.name);
        assert_ne!(a, fingerprint(&replay(&t, &p, 2)), "{}: a seed decides the draws", p.name);
    }
    // What the trace recorded isn't drawn: every completed job keeps its duration whatever the seed.
    let (d1, d2) = (draws(&t, 1), draws(&t, 2));
    for (r, run) in t.runs.iter().enumerate() {
        for (j, job) in run.jobs.iter().enumerate() {
            let (x, y) = (d1.get(r).and_then(|v| v.get(j)), d2.get(r).and_then(|v| v.get(j)));
            if let (Some((took, end)), Some(x), Some(y)) = (job.completed(), x, y) {
                assert!(x.duration == took && y.duration == took && x.end == end && !x.drawn, "run {r} job {j}");
            }
        }
    }
    Ok(())
}

/// W25 step 1, acceptance: P1 (image.yaml's setting) cancels no running job, and a run replaced
/// while pending never started.
#[test]
fn w25_s1_p1_cancels_no_running_job() -> Result<(), String> {
    let t = week()?;
    let out = replay(&t, &Policy::p1(), 1);
    assert_eq!(out.runner_time().1, 0, "{}", calibration(&t, &out));
    assert!(out.execs.iter().all(|e| e.stop != Some(Stop::Superseded)));
    for (r, run) in out.runs.iter().enumerate() {
        if run.end == RunEnd::Cancelled {
            assert!(out.execs.iter().filter(|e| e.run == r).all(|e| e.started.is_none()), "run {r} was cancelled after it started");
        }
    }
    let (p0, p1) = (replay(&t, &Policy::p0(), 1).concluded(&t, "ci").0, out.concluded(&t, "ci").0);
    assert!(p1 > p0, "finishing the run in progress concludes more: P0 {p0}, P1 {p1}");
    Ok(())
}

/// W25 step 1: a job cancelled after running 600 s is given a duration longer than that, drawn from
/// its neighbours (right-censored), whatever the seed.
#[test]
fn w25_s1_a_cancelled_job_draws_longer_than_it_ran() {
    let m = machine("m", &["x"], 4);
    let done = |at: i64, took: i64| push("w", at, None, false, vec![job("j", &["x"], Some((at, at + took)), "succeeded")]);
    let mut cancelled = job("j", &["x"], Some((15, 615)), "cancelled");
    cancelled.why = Some("superseded".into());
    let t = tiny(vec![m], vec![done(0, 300), done(10, 900), push("w", 15, None, false, vec![cancelled]), done(20, 1200), done(30, 100)]);
    for seed in 1..=20 {
        let d = draws(&t, seed);
        let x = d.get(2).and_then(|v| v.first()).map(|x| (x.duration, x.drawn));
        assert!(matches!(x, Some((900 | 1200, true))), "seed {seed}: {x:?}");
    }
}

/// W25 step 1: an exclusive job starts only on an empty machine, and nothing is held for it: a job
/// queued after it fills the machine meanwhile (v1's pick_agent).
#[test]
fn w25_s1_exclusive_waits_for_an_empty_machine() {
    let m = machine("bench", &["bench"], 2);
    let mut b = job("b", &["bench"], Some((1, 11)), "succeeded");
    b.exclusive = true;
    let t = tiny(vec![m], vec![
        push("w", 0, None, false, vec![job("a", &["bench"], Some((0, 100)), "succeeded")]),
        push("w", 1, None, false, vec![b]),
        push("w", 2, None, false, vec![job("c", &["bench"], Some((2, 102)), "succeeded")]),
    ]);
    let out = replay(&t, &Policy::p0(), 1);
    assert_eq!(starts(&out), vec![Some(0), Some(102), Some(2)]);
}

/// W25 step 1: three pushes into a group. With cancel-in-progress, each stops the run in progress
/// (superseded) and the newest runs; without it (P1), the run in progress finishes, the pending one
/// is replaced before it starts, and the newest runs next.
#[test]
fn w25_s1_groups_cancel_or_finish() {
    let m = machine("m", &["x"], 4);
    let three = |cip: bool| tiny(vec![m.clone()], (0..3).map(|i| push("w", 10 * i, Some("g"), cip, vec![job("j", &["x"], Some((10 * i, 10 * i + 100)), "succeeded")])).collect());
    let out = replay(&three(true), &Policy::p0(), 1);
    assert_eq!(out.runs.iter().map(|r| r.end).collect::<Vec<_>>(), vec![RunEnd::Cancelled, RunEnd::Cancelled, RunEnd::Passed]);
    assert_eq!(out.runner_time(), (120, 20));
    assert_eq!(out.runs.get(2).and_then(|r| r.concluded), Some(120));
    let out = replay(&three(true), &Policy::p1(), 1);
    assert_eq!(out.runs.iter().map(|r| r.end).collect::<Vec<_>>(), vec![RunEnd::Passed, RunEnd::Cancelled, RunEnd::Passed]);
    assert_eq!(starts(&out), vec![Some(0), None, Some(100)]);
    assert_eq!(out.runner_time(), (200, 0));
}

/// The synthetic lane of W25 step 1's test: a push every 5 min, a 20 min run, for 2 hours.
const TWO_HOURS: Lane = Lane { every: 300, run: 1200, until: 7200, runners: 4 };

fn job_out(out: &Outcome, r: usize, j: usize) -> Option<&JobOut> {
    out.jobs.get(r).and_then(|v| v.get(j))
}

/// W25 step 1, test: a push every 5 min, a 20 min run, for 2 hours, with runners enough for the
/// runs in flight. P0 gives no verdict while the pushes arrive (each cancels the run before it;
/// only the last push's run concludes, after the 2 hours); P2 and later give every commit a direct
/// or covered verdict.
#[test]
fn w25_s1_synthetic_lane() {
    let t = lane(&TWO_HOURS);
    assert_eq!(t.runs.len(), 24);
    let v = verdicts(&t, &replay(&t, &Policy::p0(), 1));
    assert!(v.iter().flatten().all(|x| x.at >= TWO_HOURS.until), "P0: {v:?}");
    assert_eq!(v.iter().flatten().count(), 1, "P0: only the last push's run concludes");
    for p in [Policy::p2(), Policy::p3(), Policy::p4(), Policy::p5()] {
        let v = verdicts(&t, &replay(&t, &p, 1));
        assert!(v.iter().all(Option::is_some), "{}: every commit gets a verdict: {v:?}", p.name);
        assert!(v.iter().flatten().all(|x| x.at < TWO_HOURS.until + TWO_HOURS.run), "{}: {v:?}", p.name);
    }
}

/// W25 step 1, test: the same lane with one runner. Only coverage answers every commit then:
/// P3, P4 and P5 give every commit a direct or covered verdict; P2's direct verdicts are fewer.
#[test]
fn w25_s1_synthetic_lane_with_one_runner() {
    let t = lane(&Lane { runners: 1, ..TWO_HOURS });
    for p in [Policy::p3(), Policy::p4(), Policy::p5()] {
        let v = verdicts(&t, &replay(&t, &p, 1));
        assert!(v.iter().all(Option::is_some), "{}: {v:?}", p.name);
        assert!(v.iter().flatten().any(|x| x.kind == Kind::Covered), "{}", p.name);
    }
    let direct = verdicts(&t, &replay(&t, &Policy::p2(), 1)).iter().flatten().count();
    assert!(direct < t.runs.len(), "P2 alone answers {direct} of {} commits with one runner", t.runs.len());
}

/// W25 step 1: a push whose job has the same inputs as one in flight adopts it: one execution
/// answers both commits.
#[test]
fn w25_s1_p2_adopts_the_execution_in_flight() {
    let mut t = lane(&Lane { every: 100, run: 1000, until: 200, runners: 4 });
    for c in &mut t.commits {
        c.keys.insert("lane".into(), "same".into());
    }
    let out = replay(&t, &Policy::p2(), 1);
    assert_eq!(out.execs.len(), 1);
    assert_eq!(job_out(&out, 1, 0).and_then(|j| j.how), Some(How::Adopted));
    assert_eq!(out.runs.iter().map(|r| r.concluded).collect::<Vec<_>>(), vec![Some(1000), Some(1000)]);
}

/// W25 step 1: a push whose job has the inputs of one already finished reuses its result, at once.
#[test]
fn w25_s1_p2_reuses_a_finished_result() {
    let mut t = lane(&Lane { every: 2000, run: 1000, until: 4000, runners: 4 });
    for c in &mut t.commits {
        c.keys.insert("lane".into(), "same".into());
    }
    let out = replay(&t, &Policy::p2(), 1);
    assert_eq!(out.execs.len(), 1);
    assert_eq!(job_out(&out, 1, 0).and_then(|j| j.how), Some(How::Reused));
    assert_eq!(out.runs.get(1).and_then(|r| r.concluded), Some(2000));
}

/// W25 step 1: work no live demand wants is dropped before it starts; once running, it is
/// cancelled for live work only past `finish_within` left, and otherwise finishes.
#[test]
fn w25_s1_p2_drops_and_finishes_within() {
    let one = |every: i64| lane(&Lane { every, run: 1000, until: 3 * every, runners: 1 });
    // Nothing cancelled: the queued middle push is dropped, the running one finishes.
    let t = one(100);
    let out = replay(&t, &Policy { finish_within: 10_000, ..Policy::p2() }, 1);
    let ex = |r: usize| out.execs.iter().find(|e| e.run == r).cloned();
    assert_eq!(ex(1).map(|e| (e.started, e.stop)), Some((None, Some(Stop::Dropped))));
    assert_eq!(ex(0).map(|e| (e.ended, e.stop)), Some((Some(1000), None)));
    assert_eq!(ex(2).and_then(|e| e.started), Some(1000));
    // 900 s left, past finish_within (300): cancelled for the live push.
    let out = replay(&t, &Policy::p2(), 1);
    assert_eq!(out.execs.first().map(|e| (e.ended, e.stop)), Some((Some(100), Some(Stop::Unwanted))));
    // 200 s left, within it: it finishes, and the live push waits for it.
    let t = one(800);
    let out = replay(&t, &Policy::p2(), 1);
    assert_eq!(out.execs.first().map(|e| (e.ended, e.stop)), Some((Some(1000), None)));
    assert_eq!(out.execs.get(1).and_then(|e| e.started), Some(1000));
}

/// W25 step 1: a cancelled k8s job's progress goes to its lane's next run of the job: it starts
/// ahead by its progress (at most `warm_credit` of a run), and that time isn't wasted.
#[test]
fn w25_s1_p2_warm_credit() {
    let mut t = lane(&Lane { every: 400, run: 1000, until: 800, runners: 1 });
    t.pools = vec![Pool { name: "k8s".into(), labels: vec!["k8s".into()], cpu_milli: Some(1000), mem_mib: Some(1024), pods: Some(1), ..Pool::default() }];
    for r in &mut t.runs {
        for j in &mut r.jobs {
            j.labels = vec!["k8s".into()];
            j.req = Some((1000, 1024));
        }
    }
    let out = replay(&t, &Policy::p2(), 1);
    let (a, b) = (out.execs.first().cloned(), out.execs.get(1).cloned());
    assert_eq!(a.as_ref().map(|e| (e.stop, e.used(), e.salvaged, e.wasted())), Some((Some(Stop::Unwanted), 400, 400, 0)));
    assert_eq!(b.map(|e| (e.credit, e.runtime, e.ended)), Some((400, 600, Some(1000))));
}

fn with_end(mut t: Trace, r: usize, status: &str, step: Option<&str>) -> Trace {
    if let Some(j) = t.runs.get_mut(r).and_then(|run| run.jobs.first_mut()) {
        j.status = status.into();
        j.failed_step = step.map(str::to_owned);
    }
    t
}

/// A lane: a first push alone, then four close together on one runner (the last three cancel the
/// run before theirs).
fn burst() -> Trace {
    let mut t = lane(&Lane { every: 100, run: 1000, until: 500, runners: 1 });
    for (i, r) in t.runs.iter_mut().enumerate().skip(1) {
        let at = 1900 + 100 * i64::try_from(i).unwrap_or(0);
        r.at = at;
        for j in &mut r.jobs {
            (j.queued, j.started, j.finished) = (Some(at), Some(at), Some(at + 1000));
        }
    }
    t
}

/// W25 step 1: a commit without a result of its own is covered by a later one's pass, or by a later
/// failure in the step already failing at its base; a new failure bisects the commits it may have
/// come from, in ceil(log2(n + 1)) runs, and answers them when the bisection ends.
#[test]
fn w25_s1_p3_covers_and_bisects() {
    // The last push passes: the three before it are covered by it.
    let t = burst();
    let out = replay(&t, &Policy::p3(), 1);
    let v = verdicts(&t, &out);
    assert_eq!(v.iter().map(|x| x.map(|x| (x.at, x.kind))).collect::<Vec<_>>(), vec![Some((1000, Kind::Direct)), Some((3300, Kind::Covered)), Some((3300, Kind::Covered)), Some((3300, Kind::Covered)), Some((3300, Kind::Direct))]);
    // It fails in the step the base failed in: covered too.
    let t = with_end(with_end(burst(), 0, "failed", Some("test")), 4, "failed", Some("test"));
    let v = verdicts(&t, &replay(&t, &Policy::p3(), 1));
    assert!(v.iter().skip(1).take(3).all(|x| x.is_some_and(|x| x.kind == Kind::Covered)), "{v:?}");
    // A new failure: three suspects, bisected in two runs of the job (3300-4300, 4300-5300).
    let t = with_end(burst(), 4, "failed", Some("test"));
    let out = replay(&t, &Policy::p3(), 1);
    assert_eq!(out.bisections.iter().map(|b| (b.suspects.clone(), b.done)).collect::<Vec<_>>(), vec![(vec![1, 2, 3], Some(5300))]);
    let v = verdicts(&t, &out);
    assert!(v.iter().skip(1).take(3).all(|x| *x == Some(verdict::Verdict { at: 5300, kind: Kind::Bisected })), "{v:?}");
    // Without verdicts (P2) the suspects have none.
    assert!(verdicts(&t, &replay(&t, &Policy::p2(), 1)).iter().skip(1).take(3).all(Option::is_none));
}

/// W25 step 1: coalescing holds a push only while its pools are saturated, each hold at most
/// `quiet`, and a burst never past `max_wait`; on an idle system a push starts at once.
#[test]
fn w25_s1_p4_coalesces_within_max_wait() {
    let idle = lane(&Lane { every: 300, run: 100, until: 300, runners: 1 });
    assert_eq!(replay(&idle, &Policy::p4(), 1).execs.first().and_then(|e| e.started), Some(0));
    // A runner busy with another workflow's long job; a push every 30 s from 10 s on.
    let mut t = lane(&Lane { every: 30, run: 100, until: 600, runners: 1 });
    for r in &mut t.runs {
        r.at += 10;
        for j in &mut r.jobs {
            (j.queued, j.started, j.finished) = (Some(r.at), Some(r.at), Some(r.at + 100));
        }
    }
    let blocker = Job { key: "long".into(), labels: vec!["runner".into()], queued: Some(0), started: Some(0), finished: Some(2000), status: "succeeded".into(), ..Job::default() };
    t.runs.insert(0, Run { workflow: "other".into(), event: "push".into(), branch: "main".into(), at: 0, commit: 0, status: "succeeded".into(), jobs: vec![blocker], ..Run::default() });
    let out = replay(&t, &Policy::p4(), 1);
    let holds: Vec<(i64, i64)> = out.decisions.iter().filter_map(|d| d.what.strip_prefix("held until ").and_then(|w| w.split(':').next()).and_then(|u| u.parse().ok()).map(|u| (d.at, u))).collect();
    assert!(!holds.is_empty());
    assert!(holds.iter().all(|(at, until)| until - at <= QUIET), "{holds:?}");
    let first = holds.first().map_or(0, |h| h.0);
    assert!(holds.iter().filter(|(at, _)| *at < first + MAX_WAIT).all(|(_, until)| *until <= first + MAX_WAIT), "{holds:?}");
    assert!(out.decisions.iter().any(|d| d.at == first + MAX_WAIT && d.what.starts_with("released")), "the burst is released at max_wait");
}

/// W25 step 1: on a cadence (P4) an exclusive job's machine takes no new work while it waits, so
/// it starts once the work there finishes; without (P2), later work keeps filling the machine.
#[test]
fn w25_s1_p4_cadence_drains_for_an_exclusive_job() {
    let m = machine("bench", &["bench"], 2);
    let mut b = job("b", &["bench"], Some((10, 20)), "succeeded");
    b.exclusive = true;
    let t = tiny(vec![m], vec![
        push("a", 0, None, false, vec![job("a", &["bench"], Some((0, 100)), "succeeded")]),
        push("b", 10, None, false, vec![b]),
        push("c", 20, None, false, vec![job("c", &["bench"], Some((20, 120)), "succeeded")]),
    ]);
    // Cadence alone (coalescing would hold the exclusive job, its machine being busy when it came).
    let cadence = Policy { coalesce: None, ..Policy::p4() };
    assert_eq!(starts(&replay(&t, &cadence, 1)), vec![Some(0), Some(100), Some(110)]);
    assert_eq!(starts(&replay(&t, &Policy::p2(), 1)), vec![Some(0), Some(120), Some(20)]);
}

/// W25 step 1: with sharing (P5), a step another pipeline's job is making is taken from it: the
/// UI is built once for two pipelines.
#[test]
fn w25_s1_p5_shares_a_step() {
    let m = machine("m", &["x"], 2);
    let mut a = job("linux", &["x"], Some((0, 500)), "succeeded");
    a.shared = BTreeMap::from([("ui".to_owned(), 50)]);
    let mut b = job("windows", &["x"], Some((10, 310)), "succeeded");
    b.shared = BTreeMap::from([("ui".to_owned(), 50)]);
    let mut t = tiny(vec![m], vec![push("release", 0, None, false, vec![a]), push("release-win", 10, None, false, vec![b])]);
    t.commits = vec![Commit { keys: BTreeMap::from([("ui".to_owned(), "u1".to_owned())]) }];
    let runtime = |p: &Policy| replay(&t, p, 1).execs.iter().map(|e| e.runtime).collect::<Vec<_>>();
    assert_eq!(runtime(&Policy::p4()), vec![500, 300]);
    assert_eq!(runtime(&Policy::p5()), vec![500, 250]);
}

/// W25 step 1, test: the bench on a cadence over the recorded week (P4). Each bench run measures the
/// newest commit's inputs when it starts (an older one waiting is withdrawn), and the commits it
/// skips get verdicts from coverage.
#[test]
fn w25_s1_p4_bench_on_cadence_over_the_week() -> Result<(), String> {
    let t = week()?;
    let out = replay(&t, &Policy::p4(), 1);
    let v = verdicts(&t, &out);
    let key = |r: &Run| t.commits.get(r.commit).and_then(|c| c.keys.get("bench")).cloned();
    for e in out.execs.iter().filter(|e| e.bisect.is_none() && e.started.is_some() && t.runs.get(e.run).is_some_and(|r| r.workflow == "bench")) {
        let (Some(run), Some(start)) = (t.runs.get(e.run), e.started) else { continue };
        let newer = t.runs.iter().filter(|o| o.workflow == "bench" && o.at > run.at && o.at < start && key(o) != key(run)).count();
        assert_eq!(newer, 0, "bench run at {} started at {start} behind newer inputs", run.at);
    }
    // Its machine takes no new work while it waits: it waits at most for what ran there when it was
    // queued, and for the exclusive jobs that went before it.
    let exclusive = |o: &Exec| t.runs.get(o.run).and_then(|r| r.jobs.get(o.job)).is_some_and(|j| j.exclusive);
    for (i, e) in out.execs.iter().enumerate().filter(|(_, e)| e.bisect.is_none() && exclusive(e)) {
        let Some(start) = e.started else { continue };
        let q = e.queued;
        let ran = out.execs.iter().filter(|o| o.pool == e.pool && o.started.is_some_and(|s| s <= q) && o.ended.is_some_and(|x| x > q)).filter_map(|o| o.ended.map(|x| x - q)).max().unwrap_or(0);
        let before: i64 = out.execs.iter().enumerate().filter(|(k, o)| *k != i && o.pool == e.pool && exclusive(o) && o.started.is_some_and(|s| s > q && s < start)).map(|(_, o)| o.used()).sum();
        assert!(start - q <= ran + before, "exclusive execution {i} waited {} s; what ran then needed {ran} s, the exclusive ones before it {before} s", start - q);
    }
    let bench: Vec<usize> = (0..t.runs.len()).filter(|r| t.runs.get(*r).is_some_and(|x| x.workflow == "bench" && x.is_push())).collect();
    let answered = bench.iter().filter(|r| v.get(**r).is_some_and(Option::is_some)).count();
    assert!(answered * 100 >= bench.len() * 95, "bench commits with a verdict: {answered} of {}", bench.len());
    Ok(())
}

/// W25 step 1, acceptance: for each policy P0-P5 the report gives wasted runner-hours, the share
/// of main commits with a verdict within 60 min, verdict latency p50 and p90, the longest gap, bench
/// wait p50 and runner-hours per verdict; and ci/sim/README.md keeps it as rendered from the
/// committed trace and seeds, byte for byte.
#[test]
fn w25_s1_readme_holds_the_report() -> Result<(), String> {
    let t = week()?;
    let r = report(&t, &Policy::all(), &SEEDS);
    for column in ["Wasted runner-h", "verdict within 60 min", "Verdict latency p50", "| p90 |", "Longest gap", "Bench wait p50", "Runner-h per verdict"] {
        assert!(r.contains(column), "the report lacks {column}");
    }
    for p in Policy::all() {
        assert!(r.contains(&format!("\n| {} | ", p.name)), "the report lacks {}", p.name);
    }
    let (open, close) = ("<!-- report -->\n", "<!-- /report -->");
    let start = README.find(open).ok_or("ci/sim/README.md has no report")? + open.len();
    let end = README.find(close).ok_or("ci/sim/README.md's report doesn't end")?;
    let kept = README.get(start..end).ok_or("ci/sim/README.md's report markers are out of order")?;
    assert!(kept == r, "ci/sim/README.md's report is stale: cargo run -p kiln-ci --example replay -- ci/sim/trace.json --sweeps");
    Ok(())
}
