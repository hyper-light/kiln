//! A run's report: every round's numbers, each number's median and noise across them, and, for
//! two runs (an A/B, or a run against an earlier one), each difference with its noise and what it
//! came to. Kept as JSON and as Markdown beside it.

use std::collections::{BTreeMap, BTreeSet};
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
    /// Where the server and its hogs ran (`--layout`, `--server-cpus`; goal 10, plan 01a1a87b2dae
    /// step 1): `shared` and the lab's CPUs where a run doesn't say.
    #[serde(default = "shared")]
    pub layout: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_cpus: Option<String>,
    pub rounds: Vec<Round>,
    /// Rounds that didn't finish, and why.
    #[serde(default)]
    pub failures: Vec<String>,
}

fn shared() -> String {
    "shared".to_owned()
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

    /// Each histogram whose top passed its last bound in a round of `build`'s (`<family>_past_last_bound` above 0; main's
    /// ask, 2026-11-06): the observations past it in all those rounds, the most of the bounds they passed (the least the
    /// top reached, `<family>_top_at_least_ms`), and in how many rounds.
    pub fn past_last_bound(&self, build: &str) -> BTreeMap<String, (f64, f64, usize)> {
        let mut out: BTreeMap<String, (f64, f64, usize)> = BTreeMap::new();
        for r in self.rounds.iter().filter(|r| r.build == build) {
            for (k, v) in &r.numbers {
                let Some(family) = k.strip_suffix("_past_last_bound") else { continue };
                if *v <= 0.0 {
                    continue;
                }
                let bound = r.numbers.get(&format!("{family}_top_at_least_ms")).copied().unwrap_or(0.0);
                let e = out.entry(family.to_owned()).or_insert((0.0, 0.0, 0));
                e.0 += v;
                e.1 = e.1.max(bound);
                e.2 += 1;
            }
        }
        out
    }

    /// What `build`'s rounds saw finer than its classes (each API read's route, the frames against the
    /// attaches: `measure::seen_by_what`, `lab::frames_against_attaches`): for each, the samples in all
    /// its rounds, and the median of the rounds' p50, p99, p99.9 and most.
    pub fn detail(&self, build: &str) -> Vec<(String, u64, [Option<f64>; 4])> {
        let mut by: BTreeMap<String, Vec<&crate::measure::Seen>> = BTreeMap::new();
        for r in self.rounds.iter().filter(|r| r.build == build) {
            for (k, s) in r.seen.iter().filter(|(k, _)| k.contains('.')) {
                by.entry(k.clone()).or_default().push(s);
            }
        }
        by.into_iter()
            .map(|(k, all)| {
                let med = |f: fn(&crate::measure::Seen) -> Option<f64>| {
                    let v: Vec<f64> = all.iter().filter_map(|s| f(s)).collect();
                    stats::summarize(&v, LEVEL, 0).map(|s| s.median)
                };
                (k, all.iter().map(|s| s.n).sum(), [med(|s| s.p50_ms), med(|s| s.p99_ms), med(|s| s.p999_ms), med(|s| s.max_ms)])
            })
            .collect()
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

/// A number as a report shows it: a latency (`_ms`) to the microsecond (goal 10, plan 01a1a87b2dae
/// step 1), anything else to about three figures.
fn show(name: &str, v: f64) -> String {
    if name.ends_with("_ms") { format!("{v:.3}") } else { fmt(v) }
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
        let server = run.server_cpus.as_deref().map_or_else(|| "the server on the lab's CPUs".to_owned(), |c| format!("the server on CPUs {c} of its own"));
        let _ = writeln!(o, "Machine: {} ({}), {} CPUs for the lab, {server}, {} CPU hogs beside it ({} layout), {} shards.\n", m.host, m.os, m.cpus, run.hogs, run.layout, r.shards);
    }
    let _ = writeln!(o, "Profile: {}\n", serde_json::to_string(&run.profile).unwrap_or_default());
    for build in run.builds() {
        let _ = writeln!(o, "## {build}\n");
        let _ = writeln!(o, "Machine: {}.\n", machine_line(run.rounds.iter().filter(|r| r.build == build)));
        let _ = writeln!(o, "Each number is the median of {} rounds; noise is the interval that median moves in when the rounds are drawn again ({:.0}%).\n", run.rounds.iter().filter(|r| r.build == build).count(), LEVEL * 100.0);
        let _ = writeln!(o, "| number | median | noise (low to high) |\n| --- | --- | --- |");
        let mut s: Vec<(String, Summary)> = run.summary(&build).into_iter().collect();
        s.sort_by_key(|(k, _)| order(k));
        // A top past its histogram's last bound in any round is said as at least that bound, with how many went past it:
        // the median of the other rounds' tops would hide it (main's ask, 2026-11-06).
        let past = run.past_last_bound(&build);
        let rounds = run.rounds.iter().filter(|r| r.build == build).count();
        let past_row = |k: &str, (n, at_least, r): (f64, f64, usize)| format!("| {k} | ≥ {} ms ({n:.0} past, in {r} of {rounds} rounds) | - |", fmt(at_least));
        for (k, v) in &s {
            match k.strip_suffix("_top_ms").and_then(|family| past.get(family)) {
                Some(p) => {
                    let _ = writeln!(o, "{}", past_row(k, *p));
                }
                None => {
                    let _ = writeln!(o, "| {k} | {} | {} to {} |", show(k, v.median), show(k, v.lo), show(k, v.hi));
                }
            }
        }
        for (family, p) in &past {
            let k = format!("{family}_top_ms");
            if !s.iter().any(|(name, _)| *name == k) {
                let _ = writeln!(o, "{}", past_row(&k, *p));
            }
        }
        let detail = run.detail(&build);
        if !detail.is_empty() {
            let _ = writeln!(o, "\nFiner than the classes (not compared): each API read's route, each API write's kind, and the frames against the attaches (`websocket.own_attach`: published before the viewer's own snapshot came; `websocket.while_another_attaches`: while another viewer of the session attached). Samples in all rounds, then the median of the rounds' figures, in ms:\n");
            let _ = writeln!(o, "| what | samples | p50 | p99 | p99.9 | most |\n| --- | --- | --- | --- | --- | --- |");
            for (k, n, q) in detail {
                let cell = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.3}"));
                let _ = writeln!(o, "| {k} | {n} | {} | {} | {} | {} |", cell(q[0]), cell(q[1]), cell(q[2]), cell(q[3]));
            }
        }
        let (window, fill): (fn(&Round) -> &[Held], fn(&Round) -> &[Held]) = (|r| &r.held, |r| &r.fill_held);
        for (when, of) in [("the window", window), ("the fill", fill)] {
            o.push_str(&held_markdown(run.rounds.iter().filter(|r| r.build == build), when, of));
        }
        o.push_str(&listed_markdown(&run.rounds.iter().filter(|r| r.build == build).collect::<Vec<_>>()));
        let _ = writeln!(o, "\nRounds (machine load before and after each window):\n");
        for r in run.rounds.iter().filter(|r| r.build == build) {
            let load = |l: Option<[f64; 3]>| l.map_or_else(|| "?".to_owned(), |[one, five, fifteen]| format!("{one:.1} {five:.1} {fifteen:.1}"));
            let psi = |p: Option<f64>| p.map_or_else(String::new, |p| format!(", cpu some {p:.1}%"));
            let mhz = |v: Option<f64>| v.map_or_else(String::new, |v| format!(", {v:.0} MHz"));
            // A window whose clients were stopped or frozen (goal 1, plan 01a1a59c2a68 step 3): its numbers include it.
            let stopped = r.machine.interrupted_reads.filter(|n| *n > 0).map_or_else(String::new, |n| format!("; **the lab was stopped or frozen in its window** ({n} of its reads interrupted): its latencies include those stops"));
            let _ = writeln!(
                o,
                "- round {}: filled in {:.1} s; load {} then {}{}{}{}{}{stopped}",
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
/// mean over the rounds (a round it held nothing in counts as none), the mean hold, the longest seen, and the
/// share of its time the host held (preempted; steps 3 and 4): what no code of it held.
fn held_markdown<'a>(rounds: impl Iterator<Item = &'a Round>, when: &str, of: fn(&Round) -> &[Held]) -> String {
    /// A site and label's figures over the rounds.
    #[derive(Default)]
    struct Row {
        polls_s: f64,
        ms_s: f64,
        polls: f64,
        ms: f64,
        longest: f64,
        host_ms: f64,
        cpu_held_ms: f64,
        cpu_ms: f64,
        longest_cpu: f64,
    }
    let rounds: Vec<&Round> = rounds.collect();
    let mut by: BTreeMap<(String, String), Row> = BTreeMap::new();
    for r in &rounds {
        for h in of(r) {
            let row = by.entry((h.site.clone(), h.label.clone())).or_default();
            row.polls_s += h.polls_per_s;
            row.ms_s += h.ms_per_s;
            row.polls += h.polls;
            row.ms += h.ms;
            row.longest = row.longest.max(h.longest_ms);
            row.host_ms += h.host_ms;
            row.cpu_held_ms += h.cpu_held_ms;
            row.cpu_ms += h.cpu_ms;
            row.longest_cpu = row.longest_cpu.max(h.longest_cpu_ms);
        }
    }
    let mut o = String::new();
    if by.is_empty() {
        return o;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = rounds.len().max(1) as f64;
    let mut rows: Vec<((String, String), Row)> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1.ms_s.total_cmp(&a.1.ms_s));
    let _ = writeln!(o, "\nWhat held the shards past 100 µs in {when} (spawn site and label; each a second, the mean of {} rounds):\n", rounds.len());
    // Goal 9, plan 01a1a87b2cec step 3: of those holds, kiln's (its code on the CPU) and the rest (off the CPU: the
    // host's, or a call that blocked), where the build's shards said their time on the CPU.
    let (held, cpu) = rows.iter().fold((0.0, 0.0), |(h, c), (_, r)| (h + r.cpu_held_ms, c + r.cpu_ms));
    if held > 0.0 {
        let _ = writeln!(o, "Of those holds, {} on the CPU (kiln's code ran) and {} off it (the host's, or a call that blocked: the `blocking` slow polls say how many), by the shards' own clocks.\n", pct(cpu / held), pct((held - cpu) / held));
    }
    let _ = writeln!(o, "| site | label | polls a second | ms held a second | mean hold ms | longest ms | on the CPU | longest on the CPU ms | host's share |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for ((site, label), r) in rows.into_iter().take(HELD_ROWS) {
        let mean = if r.polls > 0.0 { r.ms / r.polls } else { 0.0 };
        let host = if r.ms > 0.0 { r.host_ms / r.ms } else { 0.0 };
        let (on, longest_cpu) = if r.cpu_held_ms > 0.0 { (pct(r.cpu_ms / r.cpu_held_ms), fmt(r.longest_cpu)) } else { ("-".to_owned(), "-".to_owned()) };
        let _ = writeln!(o, "| {site} | {label} | {} | {} | {} | {} | {on} | {longest_cpu} | {} |", fmt(r.polls_s / n), fmt(r.ms_s / n), fmt(mean), fmt(r.longest), fmt(host));
    }
    o
}

/// A share as a percentage, to a whole one.
fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

/// The paths goal 9's step 2 listed as holding the shards past 100 µs (plan 01a1a87b2cec's notes), in its order, and
/// whose they are: each as [`listed`] finds it in a build's spawn sites and labels.
const LISTED: [(&str, &str); 11] = [
    ("custody's receipts of the feeds' batches (`custody/feeds.rs`)", "goal 9"),
    ("runners' custody records, with the hub's other tasks (`hub.rs`)", "goal 9"),
    ("the feed to the mirrors (`convo/feed.rs`)", "goal 9"),
    ("the hub's net loop (its labels `runner …`)", "goal 9"),
    ("read /api/harness/sessions", "goal 10"),
    ("write /api/mcp", "goal 10"),
    ("sessions' owners (`harness/copies.rs`)", "goal 9"),
    ("the server's own tasks: its supervised children, Error Tracking's save (`server.rs`, `sup.rs`)", "goal 9"),
    ("a viewer's socket, with the API's other tasks (`api.rs`)", "goal 10"),
    ("read /api/agents", "goal 10"),
    ("read /api/overview", "goal 10"),
];

/// Which of [`LISTED`] a spawn site and label are: by the file of the site, but for the hub's net loop and the HTTP
/// server's requests, which kiln's server spawns from its own file, by the sites that carry their labels in the
/// same rounds (`net`: a runner's message's, `runner …`; `http`: a request's route, `read /…`, `write /…`, `page`).
fn listed(site: &str, label: &str, net: &BTreeSet<&str>, http: &BTreeSet<&str>) -> Option<usize> {
    let file = site.split(':').next().unwrap_or(site).replace('\\', "/");
    let is = |path: &str| file.ends_with(&format!("kiln/src/{path}"));
    if is("custody/feeds.rs") {
        Some(0)
    } else if is("hub.rs") {
        Some(1)
    } else if is("convo/feed.rs") {
        Some(2)
    } else if net.contains(site) {
        Some(3)
    } else if http.contains(site) {
        match label {
            "read /api/harness/sessions" => Some(4),
            "write /api/mcp" => Some(5),
            "read /api/agents" => Some(9),
            "read /api/overview" => Some(10),
            _ => None,
        }
    } else if is("harness/copies.rs") {
        Some(6)
    } else if is("server.rs") || is("sup.rs") {
        Some(7)
    } else if is("api.rs") {
        Some(8)
    } else {
        None
    }
}

/// Whether a spawn site is the shards' probes' (`crate::shard_health`, a probe a shard).
fn is_probe(site: &str) -> bool {
    site.split(':').next().unwrap_or(site).replace('\\', "/").ends_with("kiln/src/shard_health.rs")
}

/// Format: the sites and labels past the quantum that step 2 didn't list, the longest first.
const UNLISTED_ROWS: usize = 5;

/// Goal 9's step 3 done-when, "the lab shows no poll past the new quantum from the listed paths" (plan 01a1a87b2cec),
/// for a build's rounds, in Markdown: met or not in its first line, then each path step 2 listed with its longest poll
/// in the rounds (the window's and the fill's: `kiln_shard_held_longest_seconds` is the longest since the server
/// started), where it was, the rounds it passed the quantum in (and of them, those it passed the shards' probes' longest
/// poll in too: a probe's own work is tens of microseconds, so the host held its shard that long, and may have held any
/// poll as long), its polls past 100 µs a second in the window and the share of their time the host was found to hold
/// (preempted: step 4's, not the code's); then what else passed the quantum. The quantum is the least the rounds say
/// their shards ran with (`Round::quantum_us`, or `kiln-lab report --quantum-us`).
///
/// A round passed the quantum where a poll did in its fill or its window, as the server counted them
/// (`Held::past_polls`, `Held::past_cpu_polls`); with a build from before it counted, where the longest since the server
/// started did, which takes in what the server did before the fill (its runners connecting: their first messages' polls
/// stood for every round, goal 9's step 3).
fn listed_markdown(rounds: &[&Round]) -> String {
    /// A path's longest poll, its site and label, the rounds past the quantum, its window's polls past 100 µs a
    /// second (all rounds' added), and their time and the host's.
    #[derive(Default)]
    struct Path {
        seen: bool,
        longest_ms: f64,
        at: (String, String),
        past: usize,
        past_probe: usize,
        polls_s: f64,
        ms: f64,
        host_ms: f64,
        /// Where the build's shards said their time on the CPU: the longest any of its polls ran on it, and the
        /// rounds that passed the quantum so (kiln's code ran that long).
        cpu_seen: bool,
        longest_cpu_ms: f64,
        past_cpu: usize,
    }
    let mut o = String::new();
    // A build without the series has nothing to check.
    let all: Vec<&Held> = rounds.iter().flat_map(|r| r.held.iter().chain(r.fill_held.iter())).collect();
    if all.is_empty() {
        return o;
    }
    let net: BTreeSet<&str> = all.iter().filter(|h| h.label.starts_with("runner ")).map(|h| h.site.as_str()).collect();
    let http: BTreeSet<&str> = all.iter().filter(|h| h.label.starts_with("read /") || h.label.starts_with("write /") || h.label == "page").map(|h| h.site.as_str()).collect();
    let quantum_ms = rounds.iter().filter_map(|r| r.quantum_us).reduce(f64::min).map(|us| us / 1000.0);
    let mut paths: Vec<Path> = LISTED.iter().map(|_| Path::default()).collect();
    let mut unlisted: BTreeMap<(String, String), f64> = BTreeMap::new();
    // The shards' probes' longest poll, the most of any round's: a probe's own work is tens of microseconds (a note of
    // how late it ran: `shard_health`), so what held its poll past that was the host's, and the host may have held any
    // other poll as long.
    let mut probe_most_ms = 0.0_f64;
    for r in rounds {
        let mut longest_here = vec![0.0_f64; LISTED.len()];
        let (mut past_here, mut past_cpu_here) = (vec![false; LISTED.len()], vec![false; LISTED.len()]);
        let probe_ms = r.held.iter().chain(r.fill_held.iter()).filter(|h| is_probe(&h.site)).map(|h| h.longest_ms).fold(0.0_f64, f64::max);
        probe_most_ms = probe_most_ms.max(probe_ms);
        for (in_window, h) in r.held.iter().map(|h| (true, h)).chain(r.fill_held.iter().map(|h| (false, h))) {
            // Whether a poll passed the quantum in this phase, by its hold and on the CPU.
            let (past, past_cpu) = match quantum_ms {
                Some(_) if h.counts_past => (h.past_polls > 0.0, h.past_cpu_polls > 0.0),
                Some(q) => (h.longest_ms > q, h.cpu_polls > 0.0 && h.longest_cpu_ms > q),
                None => (false, false),
            };
            let Some(i) = listed(&h.site, &h.label, &net, &http) else {
                if past {
                    let most = unlisted.entry((h.site.clone(), h.label.clone())).or_default();
                    *most = most.max(h.longest_ms);
                }
                continue;
            };
            if let (Some(p), Some(c)) = (past_here.get_mut(i), past_cpu_here.get_mut(i)) {
                *p |= past;
                *c |= past_cpu;
            }
            let Some(p) = paths.get_mut(i) else { continue };
            p.seen = true;
            if h.longest_ms > p.longest_ms {
                p.longest_ms = h.longest_ms;
                p.at = (h.site.clone(), h.label.clone());
            }
            if in_window {
                p.polls_s += h.polls_per_s;
            }
            p.ms += h.ms;
            p.host_ms += h.host_ms;
            if let Some(most) = longest_here.get_mut(i) {
                *most = most.max(h.longest_ms);
            }
            if h.cpu_polls > 0.0 {
                p.cpu_seen = true;
                p.longest_cpu_ms = p.longest_cpu_ms.max(h.longest_cpu_ms);
            }
        }
        for (((p, most), past), past_cpu) in paths.iter_mut().zip(&longest_here).zip(&past_here).zip(&past_cpu_here) {
            if *past {
                p.past += 1;
                if *most > probe_ms {
                    p.past_probe += 1;
                }
            }
            if *past_cpu {
                p.past_cpu += 1;
            }
        }
    }
    let n = rounds.len();
    match quantum_ms {
        Some(q) => {
            let past = paths.iter().filter(|p| p.past > 0).count();
            let verdict = if past == 0 { "**met**".to_owned() } else { format!("**not met**, {past} of {} past it", LISTED.len()) };
            let beyond = paths.iter().filter(|p| p.past_probe > 0).count();
            // By the shards' own clocks (Linux), kiln's code alone: what ran on the CPU past the quantum.
            let on_cpu = if paths.iter().any(|p| p.cpu_seen) {
                let past_cpu = paths.iter().filter(|p| p.past_cpu > 0).count();
                let v = if past_cpu == 0 { "**met**".to_owned() } else { format!("**not met**, {past_cpu} of {} ran past it", LISTED.len()) };
                format!(" On the CPU, by the shards' own clocks (kiln's code alone, the host's time left out): {v}.")
            } else {
                String::new()
            };
            let _ = writeln!(
                o,
                "\nStep 3's check (goal 9, plan 01a1a87b2cec): no poll past the quantum ({q:.3} ms) from the paths step 2 listed: {verdict}. Past it and past the shards' probes' longest poll too ({probe_most_ms:.3} ms; a probe's own work is tens of microseconds, so that much was the host's), {beyond} of {}.{on_cpu}\n",
                LISTED.len()
            );
        }
        None => {
            let _ = writeln!(o, "\nStep 3's check (goal 9, plan 01a1a87b2cec): these rounds don't say the quantum their shards ran with (a build from before `kiln_shard_quantum_seconds`): `kiln-lab report --quantum-us <µs>` holds them to one.\n");
        }
    }
    let _ = writeln!(o, "Each path step 2 listed, its longest poll in {n} rounds (the window's and the fill's) and where, the rounds it passed the quantum in, its polls past 100 µs a second in the window (the mean of the rounds), the host's share of their time, and the longest of its polls on the CPU with the rounds that passed the quantum so:\n");
    let _ = writeln!(o, "| listed path | whose | longest ms | where | rounds past the quantum (and the probe's longest) | polls past 100 µs a second | host's share | longest on the CPU ms | rounds past on the CPU |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    #[allow(clippy::cast_precision_loss)]
    let rounds_f = n as f64;
    for ((name, whose), p) in LISTED.iter().zip(&paths) {
        let past = quantum_ms.map_or_else(|| "-".to_owned(), |_| format!("{} of {n} ({})", p.past, p.past_probe));
        let (cpu, past_cpu) = if p.cpu_seen { (format!("{:.3}", p.longest_cpu_ms), quantum_ms.map_or_else(|| "-".to_owned(), |_| format!("{} of {n}", p.past_cpu))) } else { ("-".to_owned(), "-".to_owned()) };
        if !p.seen {
            let _ = writeln!(o, "| {name} | {whose} | - | none past 100 µs | {past} | 0 | - | - | - |");
            continue;
        }
        let at = if p.at.1.is_empty() { format!("`{}`", p.at.0) } else { format!("`{}` {}", p.at.0, p.at.1) };
        let host = if p.ms > 0.0 { fmt(p.host_ms / p.ms) } else { "-".to_owned() };
        let _ = writeln!(o, "| {name} | {whose} | {:.3} | {at} | {past} | {} | {host} | {cpu} | {past_cpu} |", p.longest_ms, fmt(p.polls_s / rounds_f));
    }
    if !unlisted.is_empty() {
        let mut rows: Vec<((String, String), f64)> = unlisted.into_iter().collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        let shown: Vec<String> = rows.iter().take(UNLISTED_ROWS).map(|((site, label), ms)| if label.is_empty() { format!("`{site}` {ms:.3} ms") } else { format!("`{site}` {label} {ms:.3} ms") }).collect();
        let more = rows.len().saturating_sub(UNLISTED_ROWS);
        let more = if more > 0 { format!(", and {more} more") } else { String::new() };
        let _ = writeln!(o, "\nPast the quantum, not listed (the longest first): {}{more}.", shown.join("; "));
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
        let cells: Vec<String> = summaries.iter().map(|s| s.get(&k).map_or_else(|| "-".to_owned(), |v| format!("{} ({} to {})", show(&k, v.median), show(&k, v.lo), show(&k, v.hi)))).collect();
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
    let server_mhz = med(&|r| r.machine.server_mhz_before);
    let say = |v: Option<f64>, unit: &str| v.map_or_else(|| "?".to_owned(), |v| format!("{}{unit}", fmt(v)));
    let server = server_mhz.map_or_else(String::new, |v| format!(", the server's at {}", say(Some(v), " MHz")));
    format!("{} rounds; load {} before each window, CPU pressure {}, the lab's CPUs at {}{server}", rounds.len(), say(load, ""), say(psi, "%"), say(mhz, " MHz"))
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
        let rel = d.rel.map_or_else(|| show(k, d.delta), |r| format!("{:+.1}%", r * 100.0));
        let noise = d.noise.map_or_else(|| format!("{} to {}", show(k, d.lo), show(k, d.hi)), |n| format!("±{:.1}%", n * 100.0));
        let verdict = match d.verdict {
            Verdict::Same => "same",
            Verdict::Better => "**better**",
            Verdict::Worse => "**worse**",
        };
        let _ = writeln!(o, "| {k} | {} | {} | {rel} | {noise} | {verdict} |", show(k, d.a), show(k, d.b));
    }
    let _ = writeln!(o);
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::Machine;

    fn held(site: &str, label: &str, longest_ms: f64) -> Held {
        Held { site: site.to_owned(), label: label.to_owned(), polls: 10.0, ms: 2.0, polls_per_s: 1.0, ms_per_s: 0.2, longest_ms, ..Held::default() }
    }

    fn round(held: Vec<Held>, fill_held: Vec<Held>, quantum_us: Option<f64>) -> Round {
        Round { build: "b".to_owned(), index: 1, hogs: 0, machine: Machine::default(), fill_s: 1.0, shards: 2, numbers: BTreeMap::new(), seen: BTreeMap::new(), held, fill_held, quantum_us, locks: BTreeMap::new() }
    }

    /// main's ask (2026-11-06): a top past its histogram's last bound in any round prints as at least that bound, with
    /// how many went past it in how many rounds, not as the other rounds' median; one past it in every round still prints.
    #[test]
    fn a_top_past_the_last_bound_prints_as_at_least_the_bound() {
        let numbers = |pairs: &[(&str, f64)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect::<BTreeMap<String, f64>>();
        let mut rounds = Vec::new();
        for n in [
            numbers(&[("bus.delivery_top_ms", 25.0), ("bus.delivery_past_last_bound", 0.0), ("compute.piece_past_last_bound", 3.0), ("compute.piece_top_at_least_ms", 10.0)]),
            numbers(&[("bus.delivery_past_last_bound", 10.0), ("bus.delivery_top_at_least_ms", 5000.0), ("compute.piece_past_last_bound", 1.0), ("compute.piece_top_at_least_ms", 10.0)]),
            numbers(&[("bus.delivery_past_last_bound", 35.0), ("bus.delivery_top_at_least_ms", 5000.0), ("compute.piece_past_last_bound", 2.0), ("compute.piece_top_at_least_ms", 10.0)]),
        ] {
            let mut r = round(vec![], vec![], None);
            r.numbers = n;
            rounds.push(r);
        }
        let run = Run { command: "lab".to_owned(), started: "now".to_owned(), profile: crate::lab::Profile::small(), hogs: 0, layout: "shared".to_owned(), server_cpus: None, rounds, failures: Vec::new() };
        let md = markdown(&run);
        assert!(md.contains("| bus.delivery_top_ms | ≥ 5000 ms (45 past, in 2 of 3 rounds) | - |"), "{md}");
        assert!(md.contains("| compute.piece_top_ms | ≥ 10.0 ms (6 past, in 3 of 3 rounds) | - |"), "a top no round had still prints: {md}");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the done-when's check finds step 2's paths by the files of their sites, and
    /// the net loop's and the requests' by the sites their labels name; holds each path's longest poll, the window's
    /// or the fill's, to the quantum round by round, and to the shards' probes' longest in the round (the host's);
    /// says met only where none passed the quantum; names apart what else passed it; and says so of rounds that
    /// don't say their quantum, with no verdict.
    #[test]
    fn g9_s3_the_listed_paths_are_held_to_the_quantum() {
        let (net, http) = ("crates/kiln/src/server.rs:170:9", "crates/kiln/src/server.rs:477:29");
        let one = round(
            vec![
                held(net, "runner AgentMsg::Harness { .. }", 0.2),
                held(http, "read /api/harness/sessions", 2.23),
                held(http, "read /api/me", 1.49),
                held("crates/kiln/src/custody/feeds.rs:32:9", "", 0.25),
                held("crates/kiln/src/shard_health.rs:268:12", "", 2.5),
            ],
            vec![held("crates/kiln/src/harness/copies.rs:1480:13", "owner Update::Events { .. }", 3.17), held("crates/kiln/src/server.rs:245:9", "", 0.1)],
            Some(266.666),
        );
        let two = round(vec![held(net, "runner AgentMsg::Wire(..)", 0.3), held("crates/kiln/src/reads.rs:338:14", "", 1.14)], vec![], Some(266.666));
        let md = listed_markdown(&[&one, &two]);
        assert!(md.contains("no poll past the quantum (0.267 ms) from the paths step 2 listed: **not met**, 3 of 11 past it."), "{md}");
        // The sessions' read, 2.23 ms beside a probe held 2.5 ms in its round, is past the quantum and not the probe's.
        assert!(md.contains("Past it and past the shards' probes' longest poll too (2.500 ms; a probe's own work is tens of microseconds, so that much was the host's), 2 of 11."), "{md}");
        let row = |name: &str| md.lines().find(|l| l.starts_with(&format!("| {name} |"))).unwrap_or_default().to_owned();
        assert!(row("the hub's net loop (its labels `runner …`)").contains("| 0.300 | `crates/kiln/src/server.rs:170:9` runner AgentMsg::Wire(..) | 1 of 2 (1) | 1.00 | 0.000 |"), "{md}");
        assert!(row("read /api/harness/sessions").contains("| 2.230 | `crates/kiln/src/server.rs:477:29` read /api/harness/sessions | 1 of 2 (0) | 0.500 |"), "{md}");
        assert!(row("custody's receipts of the feeds' batches (`custody/feeds.rs`)").contains("| 0.250 | `crates/kiln/src/custody/feeds.rs:32:9` | 0 of 2 (0) |"), "{md}");
        assert!(row("sessions' owners (`harness/copies.rs`)").contains("| 3.170 | `crates/kiln/src/harness/copies.rs:1480:13` owner Update::Events { .. } | 1 of 2 (1) |"), "the fill's too: {md}");
        assert!(row("the server's own tasks: its supervised children, Error Tracking's save (`server.rs`, `sup.rs`)").contains("| 0.100 | `crates/kiln/src/server.rs:245:9` | 0 of 2 (0) |"), "{md}");
        assert!(row("write /api/mcp").contains("| - | none past 100 µs | 0 of 2 (0) | 0 | - | - | - |"), "{md}");
        assert!(!md.contains("On the CPU, by the shards' own clocks"), "a build whose shards don't say their CPU time: no verdict on it");
        assert!(md.contains("Past the quantum, not listed (the longest first): `crates/kiln/src/shard_health.rs:268:12` 2.500 ms; `crates/kiln/src/server.rs:477:29` read /api/me 1.490 ms; `crates/kiln/src/reads.rs:338:14` 1.140 ms."), "{md}");
        let calm = round(vec![held(net, "runner AgentMsg::Harness { .. }", 0.2)], vec![], Some(266.666));
        assert!(listed_markdown(&[&calm]).contains("from the paths step 2 listed: **met**."));
        let md = listed_markdown(&[&round(vec![held(net, "runner AgentMsg::Harness { .. }", 0.2)], vec![], None)]);
        assert!(md.contains("don't say the quantum their shards ran with") && !md.contains("met**"), "{md}");
        assert_eq!(listed_markdown(&[&round(vec![], vec![], Some(266.666))]), "", "a build without the series");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: where the shards say their time on the CPU, the check also holds each path's
    /// longest poll on the CPU to the quantum (kiln's code alone), and what held the shards says its share on the CPU.
    #[test]
    fn g9_s3_the_check_holds_kilns_own_time_on_the_cpu_to_the_quantum() {
        let net = "crates/kiln/src/server.rs:170:9";
        let cpu = |site: &str, label: &str, longest_ms: f64, longest_cpu_ms: f64| Held { cpu_polls: 10.0, cpu_held_ms: 2.0, cpu_ms: 0.5, longest_cpu_ms, ..held(site, label, longest_ms) };
        // The net loop held its shard 3 ms, 0.2 of it on the CPU: the host's; the owners ran 0.4 ms on the CPU.
        let r = round(vec![cpu(net, "runner AgentMsg::Harness { .. }", 3.0, 0.2), cpu("crates/kiln/src/harness/copies.rs:1480:13", "owner Update::State { .. }", 0.5, 0.4)], vec![], Some(266.666));
        let md = listed_markdown(&[&r]);
        assert!(md.contains("listed: **not met**, 2 of 11 past it."), "{md}");
        assert!(md.contains("On the CPU, by the shards' own clocks (kiln's code alone, the host's time left out): **not met**, 1 of 11 ran past it."), "{md}");
        let row = |name: &str| md.lines().find(|l| l.starts_with(&format!("| {name} |"))).unwrap_or_default().to_owned();
        assert!(row("the hub's net loop (its labels `runner …`)").ends_with("| 0.200 | 0 of 1 |"), "{md}");
        assert!(row("sessions' owners (`harness/copies.rs`)").ends_with("| 0.400 | 1 of 1 |"), "{md}");
        let held_md = held_markdown([&r].into_iter(), "the window", |r| &r.held);
        assert!(held_md.contains("Of those holds, 25% on the CPU (kiln's code ran) and 75% off it"), "{held_md}");
        assert!(held_md.contains("| 25% | 0.200 |"), "{held_md}");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: where the server counts its polls past the quantum, a round passed it where one did
    /// in its fill or its window, by its hold and on the CPU; a longest since the server started (its runners connecting,
    /// before the fill) is no round's, though it's still the path's longest.
    #[test]
    fn g9_s3_a_round_passed_the_quantum_where_a_poll_did_in_it() {
        let net = "crates/kiln/src/server.rs:170:9";
        let counted = |site: &str, label: &str, longest_ms: f64, past_polls: f64, past_cpu_polls: f64| Held { cpu_polls: 10.0, cpu_held_ms: 2.0, cpu_ms: 0.5, longest_cpu_ms: longest_ms, past_polls, past_cpu_polls, counts_past: true, ..held(site, label, longest_ms) };
        // The net loop's longest, 0.9 ms, from before the fill: none past the quantum in the round. The owners passed it
        // in the fill, on the CPU too.
        let r = round(vec![counted(net, "runner AgentMsg::Offers { .. }", 0.9, 0.0, 0.0)], vec![counted("crates/kiln/src/harness/copies.rs:1480:13", "owner Update::Events { .. }", 0.4, 2.0, 1.0)], Some(266.666));
        let md = listed_markdown(&[&r]);
        assert!(md.contains("listed: **not met**, 1 of 11 past it."), "{md}");
        assert!(md.contains("On the CPU, by the shards' own clocks (kiln's code alone, the host's time left out): **not met**, 1 of 11 ran past it."), "{md}");
        let row = |name: &str| md.lines().find(|l| l.starts_with(&format!("| {name} |"))).unwrap_or_default().to_owned();
        assert!(row("the hub's net loop (its labels `runner …`)").contains("| 0.900 | `crates/kiln/src/server.rs:170:9` runner AgentMsg::Offers { .. } | 0 of 1 (0) |"), "{md}");
        assert!(row("the hub's net loop (its labels `runner …`)").ends_with("| 0.900 | 0 of 1 |"), "{md}");
        assert!(row("sessions' owners (`harness/copies.rs`)").ends_with("| 0.400 | 1 of 1 |"), "{md}");
        let calm = round(vec![counted(net, "runner AgentMsg::Offers { .. }", 0.9, 0.0, 0.0)], vec![], Some(266.666));
        assert!(listed_markdown(&[&calm]).contains("from the paths step 2 listed: **met**."), "none past it in the round");
    }
}
