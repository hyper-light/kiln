//! The run plan, computed before any measured round (docs/browser-lab.md, "The run plan"; plan
//! 01a1a832e9cc step 6): for each condition, case and target row, how many operations a round
//! gathers on K links, how many rounds the sample floor takes (at least 5), and the hours; then,
//! while the whole doesn't fit the budget, the costliest cells fall from the 10,000-sample floor to
//! 3,688 (the least that bounds a p99.9 at all: f23), and the doc lists them.

use serde::{Deserialize, Serialize};

use super::link::{self, Profile};
use super::runfile::workload;
use super::targets::{Server, Target};

pub const FLOOR: u64 = 10_000;
pub const LEAST: u64 = 3_688;
pub const MIN_ROUNDS: u64 = 5;

/// The seconds a job costs beyond its own: two background windows, the browsers' start, login,
/// calibration and settling, and its tail.
pub fn job_overhead_s(case: &str, p: &Profile, background_s: u64) -> u64 {
    2 * background_s + 15 + workload(case, p).tail_s
}

/// A server's start, once a round for each target.
pub fn start_s(t: &Target) -> u64 {
    if t.server == Server::Kiln { 75 } else { 5 }
}

/// Operations one link gathers a second in a case.
pub fn ops_per_s(case: &str, p: &Profile) -> f64 {
    let w = workload(case, p);
    match case {
        "stream" => w.session_per_s + 2.0 * w.lines_per_s,
        #[allow(clippy::cast_precision_loss)]
        "burst" => w.burst_events as f64 / w.burst_gap_s.max(1) as f64,
        #[allow(clippy::cast_precision_loss)]
        _ => 1000.0 / w.request_ms.max(1) as f64,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub condition: String,
    pub case: String,
    /// Target rows (target × browser) it runs for.
    pub rows: u64,
    pub ops_per_round: f64,
    pub floor: u64,
    pub rounds: u64,
    pub hours: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunPlan {
    pub k: u64,
    pub budget_h: f64,
    pub rows: Vec<Row>,
    pub total_h: f64,
    pub fits: bool,
}

fn rounds_for(floor: u64, per_round: f64) -> u64 {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let r = (floor as f64 / per_round.max(1e-9)).ceil() as u64;
    r.max(MIN_ROUNDS)
}

/// The plan for `k` links over `targets`, `browsers`, the bar's profiles and losses and `cases`.
pub fn plan(k: u64, targets: &[Target], browsers: &[String], cases: &[String], background_s: u64, budget_h: f64) -> RunPlan {
    let rows_n: u64 = targets.iter().map(|t| t.browsers.iter().filter(|b| browsers.contains(b)).count() as u64).sum();
    let starts_s: u64 = targets.iter().map(start_s).sum();
    let mut rows = Vec::new();
    for p in link::profiles().into_iter().filter(|p| p.in_bar) {
        for loss in link::bar_losses() {
            for case in cases {
                let w = workload(case, &p);
                #[allow(clippy::cast_precision_loss)]
                let per_round = ops_per_s(case, &p) * w.duration_s as f64 * k as f64;
                let rounds = rounds_for(FLOOR, per_round);
                rows.push(Row { condition: format!("{}/{}", p.name, loss.name()), case: case.clone(), rows: rows_n, ops_per_round: per_round, floor: FLOOR, rounds, hours: 0.0 });
            }
        }
    }
    let hours = |r: &Row| {
        let p = link::profile_named(r.condition.split('/').next().unwrap_or_default());
        let (dur, over) = p.map_or((0, 0), |p| (workload(&r.case, &p).duration_s, job_overhead_s(&r.case, &p, background_s)));
        #[allow(clippy::cast_precision_loss)]
        let per_round_s = (r.rows * (dur + over)) as f64 + starts_s as f64 / cases.len().max(1) as f64;
        #[allow(clippy::cast_precision_loss)]
        let h = r.rounds as f64 * per_round_s / 3600.0;
        h
    };
    for r in &mut rows {
        r.hours = hours(r);
    }
    // Over the budget: the costliest cells to the least floor, one at a time.
    loop {
        let total: f64 = rows.iter().map(|r| r.hours).sum();
        if total <= budget_h {
            break;
        }
        let Some(worst) = rows.iter_mut().filter(|r| r.floor == FLOOR).max_by(|a, b| a.hours.total_cmp(&b.hours)) else { break };
        worst.floor = LEAST;
        worst.rounds = rounds_for(LEAST, worst.ops_per_round);
        worst.hours = hours(worst);
    }
    let total_h = rows.iter().map(|r| r.hours).sum();
    RunPlan { k, budget_h, rows, total_h, fits: total_h <= budget_h }
}

pub fn markdown(p: &RunPlan) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let _ = writeln!(o, "K = {} links: {:.1} h in all against a budget of {:.0} h ({}).\n", p.k, p.total_h, p.budget_h, if p.fits { "fits" } else { "over" });
    let _ = writeln!(o, "| condition | case | target rows | operations a round | floor | rounds | hours |\n| --- | --- | --- | --- | --- | --- | --- |");
    for r in &p.rows {
        let _ = writeln!(o, "| {} | {} | {} | {:.0} | {} | {} | {:.2} |", r.condition, r.case, r.rows, r.ops_per_round, r.floor, r.rounds, r.hours);
    }
    let floored: Vec<String> = p.rows.iter().filter(|r| r.floor == LEAST).map(|r| format!("{} {}", r.condition, r.case)).collect();
    let _ = writeln!(o, "\nCells at the 3,688 floor: {}.", if floored.is_empty() { "none".to_owned() } else { floored.join(", ") });
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::targets;

    /// plan 01a1a832e9cc step 6: the plan prints its hours for K = 1, 8 and 16, and every cell gets
    /// at least 5 rounds and at least the floor it is held to.
    #[test]
    fn g1_s6_the_run_plan_for_k_1_8_16() {
        let ts: Vec<Target> = ["kiln-today", "h2o-h2", "caddy-h2", "h2o-h3", "caddy-h3"].iter().filter_map(|n| targets::named(n)).collect();
        let browsers = vec!["chromium".to_owned(), "webkit".to_owned()];
        let cases: Vec<String> = ["stream", "burst", "request"].iter().map(|c| (*c).to_owned()).collect();
        for k in [1, 8, 16] {
            let p = plan(k, &ts, &browsers, &cases, 5, 120.0);
            assert_eq!(p.rows.len(), 27);
            assert!(p.rows.iter().all(|r| r.rounds >= MIN_ROUNDS && r.rows == 8));
            #[allow(clippy::cast_precision_loss)]
            let enough = p.rows.iter().all(|r| r.ops_per_round * r.rounds as f64 >= r.floor as f64);
            assert!(enough, "K={k}");
            assert!(markdown(&p).contains("Cells at the 3,688 floor"));
        }
    }
}
