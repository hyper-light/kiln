//! What the replay reports (W25 step 1): for each policy, beside the measured week, runner time
//! wasted, how soon ci's commits on main had verdicts, the longest wait between verdicts, the
//! bench's wait and the runner time each verdict cost; each the median over seeds, with its range.

use std::fmt::Write as _;

use super::verdict::verdicts;
use super::{Coalesce, MAX_WAIT, Outcome, Policy, QUIET, Trace, replay};

/// The seeds the report replays.
pub const SEEDS: [u64; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
/// The workflows the per-workflow tables cover.
pub const WORKFLOWS: [&str; 4] = ["ci", "release", "image", "bench"];
/// A verdict within this long of its push counts as soon.
const SOON: i64 = 3600;
/// "Never", in latencies and gaps.
const NEVER: i64 = i64::MAX;

/// One workflow's figures on main, under one policy and seed (or as measured).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Figures {
    /// Runner-hours used, and wasted on work no one used.
    pub used_h: f64,
    pub wasted_h: f64,
    /// The share of pushes with a verdict within an hour.
    pub soon: f64,
    /// Verdict latency over every push, [`NEVER`] for none.
    pub p50: i64,
    pub p90: i64,
    /// The longest time between two verdicts.
    pub gap: i64,
    /// The longest a push waited for any verdict on main (its own or a later commit's).
    pub stale: i64,
    /// Runner-hours per push given a verdict.
    pub per_verdict: f64,
}

/// The report's columns, under one policy and seed (or as measured).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row {
    /// Runner-hours wasted, every workflow.
    pub wasted_h: f64,
    /// ci's figures.
    pub ci: Figures,
    /// The exclusive bench jobs' wait from their push to their start, median.
    pub bench_wait: i64,
}

/// `runs_analyze.py`'s quantile: the sorted values' element nearest `p` of the way.
fn quantile(xs: &[i64], p: f64) -> i64 {
    let mut v = xs.to_vec();
    v.sort_unstable();
    let i = ((v.len().saturating_sub(1)) as f64 * p).round() as usize;
    v.get(i).copied().unwrap_or(NEVER)
}

fn quantile_f(xs: &[f64], p: f64) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let i = ((v.len().saturating_sub(1)) as f64 * p).round() as usize;
    v.get(i).copied().unwrap_or(0.0)
}

/// Figures from each push's verdict time (`None`: never) and the workflow's runner time.
fn figures(pushes: &[(i64, Option<i64>)], used: i64, wasted: i64) -> Figures {
    let lat: Vec<i64> = pushes.iter().map(|(at, v)| v.map_or(NEVER, |v| v - at)).collect();
    let mut times: Vec<i64> = pushes.iter().filter_map(|(_, v)| *v).collect();
    times.sort_unstable();
    let gap = times.windows(2).filter_map(|w| Some(w.get(1)? - w.first()?)).max().unwrap_or(0);
    let stale = pushes.iter().filter_map(|(at, _)| times.get(times.partition_point(|t| t < at)).map(|v| v - at)).max().unwrap_or(0);
    let answered = pushes.iter().filter(|(_, v)| v.is_some()).count();
    let soon = pushes.iter().filter(|(at, v)| v.is_some_and(|v| v - at <= SOON)).count();
    Figures {
        used_h: used as f64 / 3600.0,
        wasted_h: wasted as f64 / 3600.0,
        soon: if pushes.is_empty() { 0.0 } else { soon as f64 / pushes.len() as f64 },
        p50: quantile(&lat, 0.5),
        p90: quantile(&lat, 0.9),
        gap,
        stale,
        per_verdict: if answered == 0 { 0.0 } else { used as f64 / 3600.0 / answered as f64 },
    }
}

fn on_main(r: &super::trace::Run, workflow: &str) -> bool {
    r.workflow == workflow && r.branch == "main" && r.is_push()
}

/// A workflow's figures as replayed.
pub fn workflow(trace: &Trace, out: &Outcome, workflow: &str) -> Figures {
    let v = verdicts(trace, out);
    let pushes: Vec<(i64, Option<i64>)> = trace.runs.iter().zip(&v).filter(|(r, _)| on_main(r, workflow)).map(|(r, v)| (r.at, v.map(|v| v.at))).collect();
    let mine = |e: &&super::Exec| trace.runs.get(e.run).is_some_and(|r| r.workflow == workflow);
    let used = out.execs.iter().filter(mine).map(super::Exec::used).sum();
    let wasted = out.execs.iter().filter(mine).map(super::Exec::wasted).sum();
    figures(&pushes, used, wasted)
}

/// The report's columns as replayed.
pub fn row(trace: &Trace, out: &Outcome) -> Row {
    let waits: Vec<i64> = out
        .execs
        .iter()
        .filter(|e| e.bisect.is_none())
        .filter_map(|e| {
            let run = trace.runs.get(e.run)?;
            let job = run.jobs.get(e.job)?;
            (run.workflow == "bench" && job.exclusive).then_some(e.started? - run.at)
        })
        .collect();
    Row { wasted_h: out.wasted() as f64 / 3600.0, ci: workflow(trace, out, "ci"), bench_wait: quantile(&waits, 0.5) }
}

/// A workflow's figures as the trace recorded them (`runs_analyze.py`'s definitions).
pub fn measured_workflow(trace: &Trace, workflow: &str) -> Figures {
    let pushes: Vec<(i64, Option<i64>)> = trace
        .runs
        .iter()
        .filter(|r| on_main(r, workflow))
        .map(|r| (r.at, if matches!(r.status.as_str(), "succeeded" | "failed" | "timed_out") { r.finished } else { None }))
        .collect();
    let jobs = || trace.runs.iter().filter(|r| r.workflow == workflow).flat_map(|r| r.jobs.iter());
    let ran = |j: &super::trace::Job| match (j.started, j.finished) {
        (Some(s), Some(f)) if f >= s => f - s,
        _ => 0,
    };
    let used = jobs().map(ran).sum();
    let wasted = jobs().filter(|j| j.status == "cancelled" && j.why.as_deref() == Some("superseded")).map(ran).sum();
    figures(&pushes, used, wasted)
}

/// The report's columns as the trace recorded them.
pub fn measured(trace: &Trace) -> Row {
    let waits: Vec<i64> = trace.runs.iter().filter(|r| r.workflow == "bench").flat_map(|r| r.jobs.iter()).filter(|j| j.exclusive).filter_map(|j| Some(j.started? - j.queued?)).collect();
    let wasted: f64 = WORKFLOWS.iter().map(|w| measured_workflow(trace, w).wasted_h).sum();
    Row { wasted_h: wasted, ci: measured_workflow(trace, "ci"), bench_wait: quantile(&waits, 0.5) }
}

fn dur(s: i64) -> String {
    if s == NEVER {
        "never".into()
    } else if s < 3600 {
        format!("{} min", (s + 30) / 60)
    } else {
        format!("{:.1} h", s as f64 / 3600.0)
    }
}

/// The median and range over seeds, as text.
fn spread_i(xs: &[i64], show: fn(i64) -> String) -> String {
    let (m, lo, hi) = (quantile(xs, 0.5), quantile(xs, 0.0), quantile(xs, 1.0));
    if show(lo) == show(hi) { show(m) } else { format!("{} ({}–{})", show(m), show(lo), show(hi)) }
}

fn spread_f(xs: &[f64], show: fn(f64) -> String) -> String {
    let (m, lo, hi) = (quantile_f(xs, 0.5), quantile_f(xs, 0.0), quantile_f(xs, 1.0));
    if show(lo) == show(hi) { show(m) } else { format!("{} ({}–{})", show(m), show(lo), show(hi)) }
}

fn hours(h: f64) -> String {
    format!("{h:.1}")
}

fn hours2(h: f64) -> String {
    format!("{h:.2}")
}

fn share(s: f64) -> String {
    format!("{:.1}%", 100.0 * s)
}

const HEAD: &str = "| Policy | Wasted runner-h | ci commits with a verdict within 60 min | Verdict latency p50 | p90 | Longest gap between verdicts | Longest wait for any verdict | Bench wait p50 | Runner-h per verdict |\n|---|---|---|---|---|---|---|---|---|\n";

fn table_row(s: &mut String, name: &str, rows: &[Row]) {
    let col_f = |f: fn(&Row) -> f64| rows.iter().map(f).collect::<Vec<f64>>();
    let col_i = |f: fn(&Row) -> i64| rows.iter().map(f).collect::<Vec<i64>>();
    let _ = writeln!(
        s,
        "| {name} | {} | {} | {} | {} | {} | {} | {} | {} |",
        spread_f(&col_f(|r| r.wasted_h), hours),
        spread_f(&col_f(|r| r.ci.soon), share),
        spread_i(&col_i(|r| r.ci.p50), dur),
        spread_i(&col_i(|r| r.ci.p90), dur),
        spread_i(&col_i(|r| r.ci.gap), dur),
        spread_i(&col_i(|r| r.ci.stale), dur),
        spread_i(&col_i(|r| r.bench_wait), dur),
        spread_f(&col_f(|r| r.ci.per_verdict), hours2),
    );
}

/// The report: the policies' table beside the measured week, P0's calibration, and a table per
/// workflow. Every replayed value is the median over `seeds`, with its range.
pub fn report(trace: &Trace, policies: &[Policy], seeds: &[u64]) -> String {
    let outs: Vec<(String, Vec<Outcome>)> = policies.iter().map(|p| (p.name.clone(), seeds.iter().map(|s| replay(trace, p, *s)).collect())).collect();
    let mut s = String::new();
    let seeds_text = match (seeds.first(), seeds.last()) {
        (Some(a), Some(b)) if a != b => format!("seeds {a}-{b}"),
        (Some(a), _) => format!("seed {a}"),
        _ => "no seeds".into(),
    };
    let _ = writeln!(s, "Medians over {seeds_text}, with their ranges; ci's commits on main; latency over every push (\"never\" counted as such).\n");
    s.push_str(HEAD);
    table_row(&mut s, "Measured, Oct 18-25", &[measured(trace)]);
    for (name, runs) in &outs {
        table_row(&mut s, name, &runs.iter().map(|o| row(trace, o)).collect::<Vec<_>>());
    }
    if let Some((_, p0)) = outs.iter().find(|(n, _)| n == "P0") {
        let _ = writeln!(s, "\nP0's calibration against the measured week:\n");
        for o in p0 {
            let _ = writeln!(s, "- {}", calibration(trace, o));
        }
    }
    for wf in WORKFLOWS {
        let _ = writeln!(s, "\n{wf} on main:\n");
        s.push_str("| Policy | Verdict within 60 min | Latency p50 | p90 | Longest gap | Longest wait for any verdict | Runner-h used | Wasted | Runner-h per verdict |\n|---|---|---|---|---|---|---|---|---|\n");
        let mut line = |name: &str, f: &[Figures]| {
            let col_f = |g: fn(&Figures) -> f64| f.iter().map(g).collect::<Vec<f64>>();
            let col_i = |g: fn(&Figures) -> i64| f.iter().map(g).collect::<Vec<i64>>();
            let _ = writeln!(
                s,
                "| {name} | {} | {} | {} | {} | {} | {} | {} | {} |",
                spread_f(&col_f(|x| x.soon), share),
                spread_i(&col_i(|x| x.p50), dur),
                spread_i(&col_i(|x| x.p90), dur),
                spread_i(&col_i(|x| x.gap), dur),
                spread_i(&col_i(|x| x.stale), dur),
                spread_f(&col_f(|x| x.used_h), hours),
                spread_f(&col_f(|x| x.wasted_h), hours),
                spread_f(&col_f(|x| x.per_verdict), hours2),
            );
        };
        line("Measured", &[measured_workflow(trace, wf)]);
        for (name, runs) in &outs {
            line(name, &runs.iter().map(|o| workflow(trace, o, wf)).collect::<Vec<_>>());
        }
    }
    s
}

/// The sweeps the defaults come from: each parameter varied on the policy it shapes, the others at
/// their defaults.
pub fn sweeps(trace: &Trace, seeds: &[u64]) -> String {
    let mut s = String::new();
    let mut sweep = |title: &str, cases: Vec<(String, Policy)>| {
        let _ = writeln!(s, "\n{title}:\n");
        s.push_str(HEAD);
        for (name, p) in cases {
            let rows: Vec<Row> = seeds.iter().map(|sd| row(trace, &replay(trace, &p, *sd))).collect();
            table_row(&mut s, &name, &rows);
        }
    };
    sweep("finish_within on P3", [0, 120, 300, 600, 1200].into_iter().map(|f| (format!("finish_within {f} s"), Policy { finish_within: f, ..Policy::p3() })).collect());
    sweep("warm_credit on P3", [0.0, 0.25, 0.5, 0.75].into_iter().map(|w| (format!("warm_credit {w}"), Policy { warm_credit: w, ..Policy::p3() })).collect());
    let p4 = |quiet: i64, max_wait: i64| Policy { coalesce: Some(Coalesce { quiet, max_wait }), ..Policy::p4() };
    sweep(&format!("quiet on P4 (max_wait {MAX_WAIT} s)"), [0, 60, 120, 300].into_iter().map(|q| (format!("quiet {q} s"), p4(q, MAX_WAIT))).collect());
    sweep(&format!("max_wait on P4 (quiet {QUIET} s)"), [300, 600, 900, 1800].into_iter().map(|m| (format!("max_wait {m} s"), p4(QUIET, m))).collect());
    sweep("cadence on P4", vec![("cadence on".into(), Policy::p4()), ("cadence off".into(), Policy { cadence: false, ..Policy::p4() })]);
    let window = |initial: u32| Policy { window: Some(crate::sched::admit::Window { initial, floor: 1, cap: 20 }), ..Policy::p4() };
    let mut cases: Vec<(String, Policy)> = [1, 2, 4, 8].into_iter().map(|w| (format!("window {w}"), window(w))).collect();
    cases.push(("no window".into(), Policy { window: None, ..Policy::p4() }));
    sweep("speculation window on P4 (bisection runs; floor 1, cap 20)", cases);
    sweep("image_compile_share on P5 (assumed: the trace can't split the image's build)", [0.0, 0.3, 0.6].into_iter().map(|c| (format!("image_compile_share {c}"), Policy { share: Some(c), ..Policy::p5() })).collect());
    s
}

/// P0's calibration: the replayed shares against runs_analyze.py's measured ones.
pub fn calibration(trace: &Trace, out: &Outcome) -> String {
    let mut s = String::new();
    let (used, superseded) = out.runner_time();
    let (done, n) = out.concluded(trace, "ci");
    let _ = write!(s, "{} seed {}: runner time {:.1} h, superseded {:.1} h ({:.1}%)", out.policy, out.seed, used as f64 / 3600.0, superseded as f64 / 3600.0, 100.0 * out.superseded_share());
    if let Some(m) = &trace.measured {
        let _ = write!(s, " against {:.1}%", 100.0 * m.superseded_share);
    }
    let _ = write!(s, "; ci pushes to main concluded {done} of {n} ({:.1}%)", if n == 0 { 0.0 } else { 100.0 * done as f64 / n as f64 });
    if let Some(m) = &trace.measured {
        let _ = write!(s, " against {} of {}", m.ci_concluded, m.ci_pushes);
    }
    s
}

/// Every execution, run, job result, bisection and decision, one per line.
pub fn fingerprint(out: &Outcome) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{} seed {}", out.policy, out.seed);
    for e in &out.execs {
        let _ = writeln!(s, "x {} {} {} q{} s{:?} e{:?} {:?} {:?} p{:?} rt{} c{} sv{} b{:?}", e.run, e.job, e.key, e.queued, e.started, e.ended, e.end, e.stop, e.pool, e.runtime, e.credit, e.salvaged, e.bisect);
    }
    for (i, r) in out.runs.iter().enumerate() {
        let _ = writeln!(s, "r {i} {:?} {:?}", r.end, r.concluded);
        for (j, o) in out.jobs.get(i).into_iter().flatten().enumerate() {
            let _ = writeln!(s, "  j {j} {:?} {:?} {:?} {} {:?}", o.at, o.end, o.how, o.skipped, o.failed_step);
        }
    }
    for b in &out.bisections {
        let _ = writeln!(s, "b {} {} {:?} {:?}", b.lane, b.key, b.suspects, b.done);
    }
    for d in &out.decisions {
        let _ = writeln!(s, "d {} {} {}", d.at, d.run, d.what);
    }
    s
}
