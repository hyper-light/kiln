//! A run's report: every round's numbers, each number's median and noise across them, and, for
//! two runs (an A/B, or a run against an earlier one), each difference with its noise and what it
//! came to. Kept as JSON and as Markdown beside it.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::lab::{Profile, Round};
use crate::measure::Held;
use crate::stats::{self, Diff, Summary, Verdict};

/// Every number's noise is its median's interval at this level, for one run alone.
pub const LEVEL: f64 = 0.95;

/// A run: the rounds of one or two builds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    /// What ran it: the lab's command line, when, where.
    pub command: String,
    pub started: String,
    pub profile: Profile,
    pub hogs: usize,
    pub rounds: Vec<Round>,
    /// Rounds that didn't finish, and why.
    #[serde(default)]
    pub failures: Vec<String>,
}

impl Run {
    /// The builds in it, in the order they first ran.
    pub fn builds(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in &self.rounds {
            if !out.contains(&r.build) {
                out.push(r.build.clone());
            }
        }
        out
    }

    /// Each number of `build`'s rounds, across them.
    pub fn numbers(&self, build: &str) -> BTreeMap<String, Vec<f64>> {
        let mut out: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for r in self.rounds.iter().filter(|r| r.build == build) {
            for (k, v) in &r.numbers {
                out.entry(k.clone()).or_default().push(*v);
            }
        }
        out
    }

    /// Each number of `build`'s, its median and noise.
    pub fn summary(&self, build: &str) -> BTreeMap<String, Summary> {
        self.numbers(build).into_iter().filter_map(|(k, v)| Some((k.clone(), stats::summarize(&v, LEVEL, stats::seed_of(&k))?))).collect()
    }
}

/// Two sets of rounds compared, number by number, at a level that holds for all of them at once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub a: String,
    pub b: String,
    /// The level each number's interval is held to, and the family's.
    pub each_level: f64,
    pub family_level: f64,
    pub diffs: BTreeMap<String, Diff>,
    /// Each side's machine, as `machine_line` says it.
    #[serde(default)]
    pub a_machine: String,
    #[serde(default)]
    pub b_machine: String,
}

impl Comparison {
    pub fn of(a_label: &str, a: &BTreeMap<String, Vec<f64>>, b_label: &str, b: &BTreeMap<String, Vec<f64>>) -> Self {
        let both: Vec<&String> = a.keys().filter(|k| b.contains_key(*k)).collect();
        let family_level = LEVEL;
        let each_level = stats::each_level(family_level, both.len());
        let diffs = both
            .into_iter()
            .filter_map(|k| {
                let d = stats::compare(a.get(k)?, b.get(k)?, each_level, crate::lab::better(k), stats::seed_of(k))?;
                Some((k.clone(), d))
            })
            .collect();
        Self { a: a_label.to_owned(), b: b_label.to_owned(), each_level, family_level, diffs, a_machine: String::new(), b_machine: String::new() }
    }

    /// With each side's machine said.
    pub fn on(mut self, a: String, b: String) -> Self {
        self.a_machine = a;
        self.b_machine = b;
        self
    }

    /// The numbers whose difference is past the noise.
    pub fn called(&self) -> Vec<(&String, &Diff)> {
        self.diffs.iter().filter(|(_, d)| d.verdict != Verdict::Same).collect()
    }
}

fn fmt(v: f64) -> String {
    let a = v.abs();
    if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else if a >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

/// The headline numbers, in the order a reader wants them: each class's latency and stall tail,
/// then the server's.
fn order(name: &str) -> (usize, String) {
    let classes = ["page", "api_read", "api_write", "websocket", "attach"];
    let at = classes.iter().position(|c| name.starts_with(&format!("{c}."))).unwrap_or(classes.len());
    (at, name.to_owned())
}

/// One run's report, in Markdown.
pub fn markdown(run: &Run) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# kiln lab run, {}\n", run.started);
    let _ = writeln!(o, "`{}`\n", run.command);
    if let Some(r) = run.rounds.first() {
        let m = &r.machine;
        let _ = writeln!(o, "Machine: {} ({}), {} CPUs for the lab, {} CPU hogs beside the server, {} shards.\n", m.host, m.os, m.cpus, run.hogs, r.shards);
    }
    let _ = writeln!(o, "Profile: {}\n", serde_json::to_string(&run.profile).unwrap_or_default());
    for build in run.builds() {
        let _ = writeln!(o, "## {build}\n");
        let _ = writeln!(o, "Machine: {}.\n", machine_line(run.rounds.iter().filter(|r| r.build == build)));
        let _ = writeln!(o, "Each number is the median of {} rounds; noise is the interval that median moves in when the rounds are drawn again ({:.0}%).\n", run.rounds.iter().filter(|r| r.build == build).count(), LEVEL * 100.0);
        let _ = writeln!(o, "| number | median | noise (low to high) |\n| --- | --- | --- |");
        let mut s: Vec<(String, Summary)> = run.summary(&build).into_iter().collect();
        s.sort_by_key(|(k, _)| order(k));
        for (k, v) in s {
            let _ = writeln!(o, "| {k} | {} | {} to {} |", fmt(v.median), fmt(v.lo), fmt(v.hi));
        }
        let (window, fill): (fn(&Round) -> &[Held], fn(&Round) -> &[Held]) = (|r| &r.held, |r| &r.fill_held);
        for (when, of) in [("the window", window), ("the fill", fill)] {
            o.push_str(&held_markdown(run.rounds.iter().filter(|r| r.build == build), when, of));
        }
        let _ = writeln!(o, "\nRounds (machine load before and after each window):\n");
        for r in run.rounds.iter().filter(|r| r.build == build) {
            let load = |l: Option<[f64; 3]>| l.map_or_else(|| "?".to_owned(), |[one, five, fifteen]| format!("{one:.1} {five:.1} {fifteen:.1}"));
            let psi = |p: Option<f64>| p.map_or_else(String::new, |p| format!(", cpu some {p:.1}%"));
            let mhz = |v: Option<f64>| v.map_or_else(String::new, |v| format!(", {v:.0} MHz"));
            let _ = writeln!(
                o,
                "- round {}: filled in {:.1} s; load {} then {}{}{}{}{}",
                r.index,
                r.fill_s,
                load(r.machine.load_before),
                load(r.machine.load_after),
                psi(r.machine.cpu_pressure_before),
                psi(r.machine.cpu_pressure_after),
                mhz(r.machine.mhz_before),
                mhz(r.machine.mhz_after)
            );
        }
        let _ = writeln!(o);
    }
    if !run.failures.is_empty() {
        let _ = writeln!(o, "## Rounds that didn't finish\n");
        for f in &run.failures {
            let _ = writeln!(o, "- {f}");
        }
        let _ = writeln!(o);
    }
    let builds = run.builds();
    if let (Some(a), Some(b)) = (builds.first(), builds.get(1)) {
        let c = Comparison::of(a, &run.numbers(a), b, &run.numbers(b)).on(machine_line(run.rounds.iter().filter(|r| &r.build == a)), machine_line(run.rounds.iter().filter(|r| &r.build == b)));
        o.push_str(&comparison_markdown(&c));
    }
    o
}

/// Format: the sites and labels a report lists of what held the shards, the most time first.
const HELD_ROWS: usize = 25;

/// What held a build's shards past 100 µs in `when` (the window or the fill; goal 9, plan 01a1a87b2cec step 2),
/// in Markdown: each spawn site and label, its polls a second and the time they held a shard a second, each the
/// mean over the rounds (a round it held nothing in counts as none), the mean hold and the longest seen.
fn held_markdown<'a>(rounds: impl Iterator<Item = &'a Round>, when: &str, of: fn(&Round) -> &[Held]) -> String {
    let rounds: Vec<&Round> = rounds.collect();
    let mut by: BTreeMap<(String, String), (f64, f64, f64, f64, f64)> = BTreeMap::new();
    for r in &rounds {
        for h in of(r) {
            let row = by.entry((h.site.clone(), h.label.clone())).or_default();
            *row = (row.0 + h.polls_per_s, row.1 + h.ms_per_s, row.2 + h.polls, row.3 + h.ms, row.4.max(h.longest_ms));
        }
    }
    let mut o = String::new();
    if by.is_empty() {
        return o;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = rounds.len().max(1) as f64;
    let mut rows: Vec<((String, String), (f64, f64, f64, f64, f64))> = by.into_iter().collect();
    rows.sort_by(|a, b| (b.1).1.total_cmp(&(a.1).1));
    let _ = writeln!(o, "\nWhat held the shards past 100 µs in {when} (spawn site and label; each a second, the mean of {} rounds):\n", rounds.len());
    let _ = writeln!(o, "| site | label | polls a second | ms held a second | mean hold ms | longest ms |\n| --- | --- | --- | --- | --- | --- |");
    for ((site, label), (polls_s, ms_s, polls, ms, longest)) in rows.into_iter().take(HELD_ROWS) {
        let mean = if polls > 0.0 { ms / polls } else { 0.0 };
        let _ = writeln!(o, "| {site} | {label} | {} | {} | {} | {} |", fmt(polls_s / n), fmt(ms_s / n), fmt(mean), fmt(longest));
    }
    o
}

/// Runs side by side, in Markdown: each number's median, and its noise, in each (each run's first
/// build).
pub fn table(runs: &[(String, Run)]) -> String {
    let mut o = String::new();
    let summaries: Vec<BTreeMap<String, Summary>> = runs.iter().map(|(_, r)| r.builds().first().map(|b| r.summary(b)).unwrap_or_default()).collect();
    let mut names: Vec<String> = summaries.iter().flat_map(|s| s.keys().cloned()).collect();
    names.sort_by_key(|k| order(k));
    names.dedup();
    let _ = writeln!(o, "| number | {} |", runs.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join(" | "));
    let _ = writeln!(o, "| --- |{}", " --- |".repeat(runs.len()));
    for k in names {
        let cells: Vec<String> = summaries.iter().map(|s| s.get(&k).map_or_else(|| "-".to_owned(), |v| format!("{} ({} to {})", fmt(v.median), fmt(v.lo), fmt(v.hi)))).collect();
        let _ = writeln!(o, "| {k} | {} |", cells.join(" | "));
    }
    o
}

/// The machine under a set of rounds, in a line: its load averages before each window, its CPU
/// pressure and its CPUs' clock (medians of the rounds'), so two runs made apart can be told to
/// have run on the same machine, or on one that had changed.
pub fn machine_line<'a>(rounds: impl Iterator<Item = &'a Round>) -> String {
    let rounds: Vec<&Round> = rounds.collect();
    let med = |f: &dyn Fn(&Round) -> Option<f64>| {
        let v: Vec<f64> = rounds.iter().filter_map(|r| f(r)).collect();
        stats::summarize(&v, LEVEL, 0).map(|s| s.median)
    };
    let load = med(&|r| r.machine.load_before.map(|[one, _, _]| one));
    let psi = med(&|r| r.machine.cpu_pressure_before);
    let mhz = med(&|r| r.machine.mhz_before);
    let show = |v: Option<f64>, unit: &str| v.map_or_else(|| "?".to_owned(), |v| format!("{}{unit}", fmt(v)));
    format!("{} rounds; load {} before each window, CPU pressure {}, the lab's CPUs at {}", rounds.len(), show(load, ""), show(psi, "%"), show(mhz, " MHz"))
}

/// A comparison, in Markdown.
pub fn comparison_markdown(c: &Comparison) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "## {} against {}\n", c.b, c.a);
    for (label, line) in [(&c.a, &c.a_machine), (&c.b, &c.b_machine)] {
        if !line.is_empty() {
            let _ = writeln!(o, "- {label}: {line}");
        }
    }
    let _ = writeln!(o);
    let called = c.called();
    if called.is_empty() {
        let _ = writeln!(o, "No difference beyond noise in any of the {} numbers.\n", c.diffs.len());
    } else {
        let _ = writeln!(o, "{} of {} numbers differ beyond noise.\n", called.len(), c.diffs.len());
    }
    let _ = writeln!(
        o,
        "Each difference's interval holds at {:.2}% so that all {} hold together at {:.0}% (Bonferroni); a difference is called better or worse only when its interval leaves out none.\n",
        c.each_level * 100.0,
        c.diffs.len(),
        c.family_level * 100.0
    );
    let _ = writeln!(o, "| number | {} | {} | difference | noise | verdict |\n| --- | --- | --- | --- | --- | --- |", c.a, c.b);
    let mut rows: Vec<(&String, &Diff)> = c.diffs.iter().collect();
    rows.sort_by_key(|(k, _)| order(k));
    for (k, d) in rows {
        let rel = d.rel.map_or_else(|| fmt(d.delta), |r| format!("{:+.1}%", r * 100.0));
        let noise = d.noise.map_or_else(|| format!("{} to {}", fmt(d.lo), fmt(d.hi)), |n| format!("±{:.1}%", n * 100.0));
        let verdict = match d.verdict {
            Verdict::Same => "same",
            Verdict::Better => "**better**",
            Verdict::Worse => "**worse**",
        };
        let _ = writeln!(o, "| {k} | {} | {} | {rel} | {noise} | {verdict} |", fmt(d.a), fmt(d.b));
    }
    let _ = writeln!(o);
    o
}
