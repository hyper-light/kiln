//! The land queue's replay (docs/ci.md W28-R35 to R37, AC31 to AC33): synthetic queues, and the
//! recorded week.

use std::collections::{BTreeMap, BTreeSet};

use super::super::report::SEEDS;
use super::super::trace::{Commit, Job, Pool, Run};
use super::*;

/// The recorded week, embedded at compile time (the replay itself reads no disk).
const WEEK: &str = include_str!("../../../../../../../ci/sim/trace.json");
/// ci/sim/README.md, which keeps the land queue's report.
const README: &str = include_str!("../../../../../../../ci/sim/README.md");

fn week() -> Result<Trace, String> {
    Trace::parse(WEEK).map_err(|e| e.to_string())
}

fn machine(name: &str, slots: u32) -> Pool {
    Pool { name: name.into(), labels: vec![name.into()], slots: Some(slots), ..Pool::default() }
}

/// A change of a synthetic queue: when it's pushed, the classes it changes, and its check's jobs
/// as (key, class, runner, seconds, how it ends).
struct Push {
    at: i64,
    changes: Vec<String>,
    jobs: Vec<(String, String, String, i64, &'static str)>,
}

/// A change to one class whose check is one job on `runner`.
fn push(at: i64, class: &str, job: &str, runner: &str, secs: i64, ends: &'static str) -> Push {
    Push { at, changes: vec![class.into()], jobs: vec![(job.into(), class.into(), runner.into(), secs, ends)] }
}

/// A synthetic queue: the trunk's own push at 0, which changes every class and checks nothing (so
/// it lands at once, and the pushes after it are changes against it), then `pushes`, in order.
fn queue(pools: Vec<Pool>, pushes: &[Push]) -> Trace {
    let classes: BTreeSet<String> = pushes.iter().flat_map(|p| p.changes.iter().cloned()).collect();
    let mut keys: BTreeMap<String, String> = classes.iter().map(|c| (c.clone(), "0".to_owned())).collect();
    let mut commits = vec![Commit { keys: keys.clone() }];
    let mut runs = vec![Run { workflow: "ci".into(), event: "push".into(), branch: "main".into(), at: 0, commit: 0, status: "succeeded".into(), ..Run::default() }];
    for (n, p) in pushes.iter().enumerate() {
        for c in &p.changes {
            keys.insert(c.clone(), (n + 1).to_string());
        }
        commits.push(Commit { keys: keys.clone() });
        let jobs = p
            .jobs
            .iter()
            .map(|(key, class, runner, secs, ends)| Job { key: key.clone(), labels: vec![runner.clone()], class: Some(class.clone()), queued: Some(p.at), started: Some(p.at), finished: Some(p.at + secs), status: (*ends).into(), ..Job::default() })
            .collect();
        runs.push(Run { workflow: "ci".into(), event: "push".into(), branch: "main".into(), at: p.at, commit: n + 1, status: "succeeded".into(), jobs, ..Run::default() });
    }
    Trace { end: 1_000_000, measured: None, pools, commits, runs }
}

/// When work for change `i` first ran.
fn started(o: &LandOut, i: usize) -> Option<i64> {
    o.items.get(i).and_then(|x| x.started)
}

fn landed(o: &LandOut, i: usize) -> Option<i64> {
    o.items.get(i).and_then(|x| x.landed)
}

/// W28-AC31: a burst of 20 small agent changes, 15 in components of their own and 5 sharing
/// kiln-ci, whose checks share two runners. The 15 land without waiting for each other, each as
/// its own check ends; the 5 start the critical path first (their head, which every landing behind
/// it waits for), then the shortest expected service, as the runners free.
#[test]
fn w28_ac31_a_burst_of_twenty_agent_changes() {
    let mut pushes: Vec<Push> = (1..=15).map(|k| push(1, &format!("crate-{k}"), "check", "small", 120 + 10 * k, "succeeded")).collect();
    for (k, secs) in [400, 100, 300, 50, 200].into_iter().enumerate() {
        pushes.push(push(1, "kiln-ci", &format!("kiln-ci-{k}"), "ci", secs, "succeeded"));
    }
    let t = queue(vec![machine("small", 15), machine("ci", 2)], &pushes);
    let o = replay(&t, &LandPolicy::components(Order::default()), 1);
    for k in 1..=15 {
        let i = usize::try_from(k).unwrap_or(0);
        assert_eq!((started(&o, i), landed(&o, i)), (Some(1), Some(1 + 120 + 10 * k)), "#{i} starts at once and lands as its own check ends");
    }
    let mut by_start: Vec<(Option<i64>, usize)> = (16..=20).map(|i| (started(&o, i), i)).collect();
    by_start.sort_unstable();
    // #16 heads the component, so its check is the critical path; then 50 s, 100 s, 200 s, 300 s.
    assert_eq!(by_start, vec![(Some(1), 16), (Some(1), 19), (Some(51), 17), (Some(151), 20), (Some(351), 18)], "critical path first, then shortest first");
    // They land in queue order, each once it and every change ahead of it passed.
    let lands: Vec<Option<i64>> = (16..=20).map(|i| landed(&o, i)).collect();
    assert_eq!(lands, vec![Some(401), Some(401), Some(651), Some(651), Some(651)]);
    assert!(o.decisions.iter().any(|d| d.contains("#17 queued: waits for #16: both affect kiln-ci (unit kiln-ci)")), "a change that waits says what it shares");
    assert!(o.decisions.iter().any(|d| d.contains("#2 queued: lands beside every change ahead of it: it shares no unit with any")), "one that doesn't says so");
}

/// W28-AC32: long changes behind a constant stream of short ones, on one runner. Shortest-first
/// keeps taking the short ones, and aging lifts each long change, so each starts within the aging
/// bound; without aging, none starts while the stream lasts.
#[test]
fn w28_ac32_long_changes_behind_constant_short_ones_start_within_the_aging_bound() {
    let mut pushes: Vec<Push> = (1..=180).map(|n| push(60 * n, &format!("short-{n}"), "short", "runner", 60, "succeeded")).collect();
    for (n, at) in [90, 5400, 10_800].into_iter().enumerate() {
        pushes.push(push(at, &format!("long-{n}"), "long", "runner", 600, "succeeded"));
    }
    pushes.sort_by_key(|p| p.at);
    let longs: Vec<usize> = pushes.iter().enumerate().filter(|(_, p)| p.jobs.iter().any(|j| j.0 == "long")).map(|(k, _)| k + 1).collect();
    assert_eq!(longs.len(), 3);
    let t = queue(vec![machine("runner", 1)], &pushes);
    let o = replay(&t, &LandPolicy::components(Order::default()), 1);
    for &i in &longs {
        let at = o.items.get(i).map_or(0, |x| x.at);
        let wait = started(&o, i).map_or(i64::MAX, |s| s - at);
        assert!(wait > 0 && wait <= o.aging_bound, "long change #{i}, pushed at {at} s, waited {wait} s (bound {} s)", o.aging_bound);
    }
    let unaged = replay(&t, &LandPolicy::components(Order { aging_per_minute: 0.0, ..Order::default() }), 1);
    let first = longs.first().copied().unwrap_or(0);
    assert!(started(&unaged, first).is_none_or(|s| s > 10_800), "without aging the first long change waits out the stream: started at {:?}", started(&unaged, first));
}

/// W28-R36 and R32: four changes to one unit tested at once, the second failing. It is ejected and
/// its component's window halves; the changes behind it run again only the job whose tree its
/// leaving changed, and the job of a unit none of them changes runs once for all four.
#[test]
fn w28_r36_a_failure_ejects_halves_the_window_and_reruns_only_what_it_changed() {
    let pushes: Vec<Push> = (0..4)
        .map(|k| Push {
            at: 1,
            changes: vec!["x".into()],
            jobs: vec![("x".into(), "x".into(), "r".into(), 100, if k == 1 { "failed" } else { "succeeded" }), ("y".into(), "y".into(), "r".into(), 100, "succeeded")],
        })
        .collect();
    let t = queue(vec![machine("r", 10)], &pushes);
    let o = replay(&t, &LandPolicy::components(Order::default()), 1);
    assert_eq!(o.items.get(2).map(|x| x.ejected), Some(true), "#2 is ejected");
    assert_eq!((landed(&o, 1), landed(&o, 3), landed(&o, 4)), (Some(101), Some(201), Some(201)));
    let runs = |job: &str| o.execs.iter().filter(|e| e.key.starts_with(&format!("{job}@"))).count();
    assert_eq!((runs("x"), runs("y")), (6, 1), "x runs again behind the ejection; y, which nobody changes, runs once");
    assert!(o.decisions.iter().any(|d| d.contains("#2 ejected: x failed on its stack; its component's window halves to 4")));
    assert!(o.window_max <= o.window_cap);
}

/// W28-R37: six changes to one unit on a machine of five slots. The head starts, and speculative
/// candidates only while the machine is below 80% committed: four run at once, a fifth slot free;
/// the rest wait, the deferral saying why, and start once the machine frees.
#[test]
fn w28_r37_speculative_candidates_defer_at_the_utilization_cap() {
    let pushes: Vec<Push> = (0..6).map(|_| push(1, "x", "x", "r", 1000, "succeeded")).collect();
    let t = queue(vec![machine("r", 5)], &pushes);
    let o = replay(&t, &LandPolicy::components(Order::default()), 1);
    assert_eq!(o.execs.iter().filter(|e| e.started == Some(1)).count(), 4);
    assert!(o.decisions.iter().any(|d| d.contains("#5: deferred: speculative, and the bottleneck its check needs is at 80% of its capacity, at or past the 80% cap")));
    let lands: Vec<Option<i64>> = (1..=6).map(|i| landed(&o, i)).collect();
    assert_eq!(lands, vec![Some(1001), Some(1001), Some(1001), Some(1001), Some(2001), Some(2001)]);
}

/// Today's queue (W14-R08): four changes in one batch, the third breaking the check and the fourth
/// inheriting it. The batch fails and halves; its first half lands; the rest is tested whole again,
/// halved, then each change alone, both ejected.
#[test]
fn serial_batch_and_bisect_halves_down_to_the_culprit() {
    let pushes: Vec<Push> = ["succeeded", "succeeded", "failed", "failed"].into_iter().map(|ends| push(1, "x", "x", "r", 100, ends)).collect();
    let t = queue(vec![machine("r", 4)], &pushes);
    let o = replay(&t, &LandPolicy::serial(4), 1);
    assert_eq!(o.candidates, vec![vec![0], vec![1, 2, 3, 4], vec![1, 2], vec![3, 4], vec![3, 4], vec![3], vec![4]]);
    let fates: Vec<(bool, bool)> = o.items.iter().map(|x| (x.landed.is_some(), x.ejected)).collect();
    assert_eq!(fates, vec![(true, false), (true, false), (true, false), (false, true), (false, true)]);
    assert_eq!(o.used(), 600, "six candidates of one 100 s job each; the trunk's own checks nothing");
}

/// W28-AC33, on the recorded week, seeds 1-8: step 9's queue at least halves the mean time from
/// enqueue to landed against today's serial queue; no execution waits for a runner past the aging
/// bound; no window passes its cap. Runner-hours per landed change fall too, though by less than
/// AC33's half (ci/sim/README.md says why).
#[test]
fn w28_ac33_the_recorded_week() -> Result<(), String> {
    let t = week()?;
    for seed in SEEDS {
        let s = replay(&t, &LandPolicy::serial(1), seed);
        let c = replay(&t, &LandPolicy::components(Order::default()), seed);
        assert!(c.mean_to_land() <= 0.5 * s.mean_to_land(), "seed {seed}: {:.0} s against serial's {:.0} s", c.mean_to_land(), s.mean_to_land());
        assert!(c.max_wait() <= c.aging_bound, "seed {seed}: an execution waited {} s for a runner, past the aging bound of {} s", c.max_wait(), c.aging_bound);
        assert!(c.window_max <= c.window_cap, "seed {seed}: a window opened to {}, past its cap of {}", c.window_max, c.window_cap);
        assert!(c.runner_hours_per_landed() < s.runner_hours_per_landed(), "seed {seed}: {:.3} runner-h per landed change against serial's {:.3}", c.runner_hours_per_landed(), s.runner_hours_per_landed());
    }
    Ok(())
}

/// W28-AC33: ci/sim/README.md keeps the land queue's report as rendered from the committed trace
/// and seeds, byte for byte.
#[test]
fn w28_ac33_readme_holds_the_land_report() -> Result<(), String> {
    let t = week()?;
    let r = report(&t, &SEEDS);
    for p in LandPolicy::pair() {
        assert!(r.contains(&format!("\n| {} | ", p.name)), "the report lacks {}", p.name);
    }
    let (open, close) = ("<!-- land report -->\n", "<!-- /land report -->");
    let start = README.find(open).ok_or("ci/sim/README.md has no land report")? + open.len();
    let end = README.find(close).ok_or("ci/sim/README.md's land report doesn't end")?;
    let kept = README.get(start..end).ok_or("ci/sim/README.md's land report markers are out of order")?;
    assert!(kept == r, "ci/sim/README.md's land report is stale: cargo run -p kiln-ci --example land -- ci/sim/trace.json");
    Ok(())
}
