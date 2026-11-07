//! A browser run's numbers (docs/browser-lab.md, "The numbers"; plan 01a1a832e9cc steps 6 to 10).
//! A cell is one case × link × loss × browser × measure, judged at p50, p99 and p99.9. Each
//! target's number in a cell is the median of its rounds' quantiles, with the interval that median
//! moves in when the rounds are drawn again (kiln-lab's bootstrap, 95%), and the operations it
//! rests on. The best baseline in a cell is the lowest median among the targets measured there.
//! Everything here reads run files only, so the doc's tables are regenerated from them.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::runfile::{BrowserRun, JobRecord};
use crate::stats::{self, Better, Verdict};

/// The measures, their keys' stems in a job's numbers, and their units.
pub const MEASURES: [(&str, &str, &str); 3] = [("time", "time", "ms"), ("bytes", "bytes", "B"), ("cpu", "cpu_us", "µs")];
pub const QUANTILES: [(&str, &str); 3] = [("p50", "p50"), ("p99", "p99"), ("p99.9", "p999")];

/// A number's key in a job: `time.p99_ms`, `bytes.p50`, `cpu_us.p999`.
pub fn key(measure: &str, q: &str) -> String {
    match measure {
        "time" => format!("time.{q}_ms"),
        m => format!("{m}.{q}"),
    }
}

/// A cell's address.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub case: String,
    pub browser: String,
    pub condition: String,
}

/// Each target's rounds in each cell.
pub fn group(run: &BrowserRun) -> BTreeMap<Cell, BTreeMap<String, Vec<&JobRecord>>> {
    let mut out: BTreeMap<Cell, BTreeMap<String, Vec<&JobRecord>>> = BTreeMap::new();
    for j in &run.jobs {
        let c = Cell { case: j.case.clone(), browser: j.browser.clone(), condition: j.condition.clone() };
        out.entry(c).or_default().entry(j.target.clone()).or_default().push(j);
    }
    out
}

/// A target's number in a cell: its rounds' median, its interval, and the samples it rests on.
#[derive(Clone, Debug)]
pub struct Number {
    pub median: f64,
    pub lo: f64,
    pub hi: f64,
    pub n: f64,
}

pub fn number(jobs: &[&JobRecord], k: &str, n_key: &str) -> Option<Number> {
    let v: Vec<f64> = jobs.iter().filter_map(|j| j.numbers.get(k).copied()).collect();
    let s = stats::summarize(&v, 0.95, stats::seed_of(k))?;
    let n = jobs.iter().filter_map(|j| j.numbers.get(n_key).copied()).sum();
    Some(Number { median: s.median, lo: s.lo, hi: s.hi, n })
}

fn fmt(v: f64) -> String {
    let a = v.abs();
    if a >= 1000.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// The CPU measure's floor in a target's cell (µs): the background's noise a second (each job's
/// record) over its CPU readings' typical length (`cpu_us.window_ms.p50`), the median of its rounds.
/// A per-operation CPU percentile at or under it is the background's, not the operation's.
pub fn cpu_floor_us(jobs: &[&JobRecord]) -> Option<f64> {
    let mut v: Vec<f64> = jobs.iter().filter_map(|j| j.numbers.get("cpu_us.window_ms.p50").map(|w| j.noise_us_per_s * w / 1000.0)).filter(|f| f.is_finite()).collect();
    v.sort_by(f64::total_cmp);
    super::cells::quantile(&v, 0.5)
}

/// The run's tables, in Markdown: one for each case and browser, a row for each condition,
/// measure and quantile, a column for each target, and the best baseline.
pub fn tables(run: &BrowserRun) -> String {
    let mut o = String::new();
    let groups = group(run);
    let mut targets: Vec<String> = run.jobs.iter().map(|j| j.target.clone()).collect();
    targets.sort();
    targets.dedup();
    // kiln today first, then the baselines.
    targets.sort_by_key(|t| (!t.starts_with("kiln"), t.clone()));
    let mut by_table: BTreeMap<(String, String), Vec<&Cell>> = BTreeMap::new();
    for c in groups.keys() {
        by_table.entry((c.case.clone(), c.browser.clone())).or_default().push(c);
    }
    for ((case, browser), cells) in by_table {
        let _ = writeln!(o, "#### {case} in {browser}\n");
        let _ = writeln!(o, "| condition | measure | {} | best |", targets.join(" | "));
        let _ = writeln!(o, "| --- | --- |{} --- |", " --- |".repeat(targets.len()));
        for c in cells {
            let Some(per) = groups.get(c) else { continue };
            for (m, stem, unit) in MEASURES {
                for (qname, q) in QUANTILES {
                    let k = key(stem, q);
                    let n_key = format!("{stem}.n");
                    let mut nums: Vec<Option<Number>> = targets.iter().map(|t| per.get(t).and_then(|js| number(js, &k, &n_key))).collect();
                    // A CPU percentile at or under the background's noise over a reading's length
                    // gives the cell's mean instead, and says so (step 4).
                    let mut floors: Vec<Option<f64>> = vec![None; targets.len()];
                    if stem == "cpu_us" {
                        for ((t, n), f) in targets.iter().zip(nums.iter_mut()).zip(floors.iter_mut()) {
                            let (Some(js), Some(v)) = (per.get(t), n.as_ref()) else { continue };
                            let Some(floor) = cpu_floor_us(js) else { continue };
                            if v.median <= floor {
                                *n = number(js, "cpu_us.mean", &n_key);
                                *f = Some(floor);
                            }
                        }
                    }
                    let best = targets.iter().zip(&nums).zip(&floors).filter_map(|((t, n), f)| n.as_ref().map(|n| (t, n.median, f.is_some()))).min_by(|a, b| a.1.total_cmp(&b.1));
                    // A p99.9 of time also carries its within-round band (the median round's).
                    let band = |t: &String| {
                        let js = per.get(t)?;
                        let lo = number(js, "time.p999_band_lo_ms", "time.n")?;
                        let hi = number(js, "time.p999_band_hi_ms", "time.n")?;
                        Some(format!(" band {}–{}", fmt(lo.median), fmt(hi.median)))
                    };
                    let cellv: Vec<String> = targets
                        .iter()
                        .zip(&nums)
                        .zip(&floors)
                        .map(|((t, n), floor)| {
                            n.as_ref().map_or_else(
                                || "-".to_owned(),
                                |n| {
                                    let tail = match floor {
                                        Some(f) => format!(" (mean: the {qname} is under the noise floor, {} µs)", fmt(*f)),
                                        None if stem == "time" && q == "p999" => band(t).unwrap_or_default(),
                                        None => String::new(),
                                    };
                                    format!("{} ({}–{}) n={}{tail}", fmt(n.median), fmt(n.lo), fmt(n.hi), fmt(n.n))
                                },
                            )
                        })
                        .collect();
                    let _ = writeln!(o, "| {} | {m} {qname} ({unit}) | {} | {} |", c.condition, cellv.join(" | "), best.map_or_else(|| "-".to_owned(), |(t, v, mean)| format!("{t} {}{}", fmt(v), if mean { " (mean)" } else { "" })));
                }
            }
        }
        let _ = writeln!(o);
    }
    o
}

/// Two targets' rounds compared cell by cell (an A/A, or the K-links check across two runs),
/// at a level that holds across every number compared at once (Bonferroni): how many differ.
pub fn compare(a: &[&JobRecord], b: &[&JobRecord]) -> (usize, usize, Vec<String>) {
    let mut pairs = Vec::new();
    let cell = |j: &JobRecord| (j.case.clone(), j.browser.clone(), j.condition.clone());
    let mut ga: BTreeMap<_, Vec<&JobRecord>> = BTreeMap::new();
    let mut gb: BTreeMap<_, Vec<&JobRecord>> = BTreeMap::new();
    for j in a {
        ga.entry(cell(j)).or_default().push(*j);
    }
    for j in b {
        gb.entry(cell(j)).or_default().push(*j);
    }
    for (c, ja) in &ga {
        let Some(jb) = gb.get(c) else { continue };
        for (_, stem, _) in MEASURES {
            for (_, q) in QUANTILES {
                let k = key(stem, q);
                let va: Vec<f64> = ja.iter().filter_map(|j| j.numbers.get(&k).copied()).collect();
                let vb: Vec<f64> = jb.iter().filter_map(|j| j.numbers.get(&k).copied()).collect();
                if va.len() >= 2 && vb.len() >= 2 {
                    pairs.push((format!("{} {} {} {k}", c.0, c.1, c.2), va, vb));
                }
            }
        }
    }
    let level = stats::each_level(0.95, pairs.len());
    let mut differ = Vec::new();
    for (name, va, vb) in &pairs {
        if let Some(d) = stats::compare(va, vb, level, Better::Lower, stats::seed_of(name))
            && d.verdict != Verdict::Same
        {
            differ.push(format!("{name}: {} against {}", fmt(d.a), fmt(d.b)));
        }
    }
    (differ.len(), pairs.len(), differ)
}

/// The doc's tables between their markers, and the run's regenerated, line by line: each line
/// that differs, as (the doc's, the run's).
pub fn doc_differs(doc: &str, run: &BrowserRun) -> Vec<(String, String)> {
    let (start, end) = ("<!-- browser-lab tables: begin -->", "<!-- browser-lab tables: end -->");
    let inside = doc.split_once(start).and_then(|(_, r)| r.split_once(end)).map_or("", |(t, _)| t);
    let want = tables(run);
    let a: Vec<&str> = inside.trim().lines().collect();
    let b: Vec<&str> = want.trim().lines().collect();
    let mut out = Vec::new();
    // Each line under its table's heading (`#### case in browser`): with the row's condition and
    // measure, that names the cell.
    let mut table = "";
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(""), b.get(i).copied().unwrap_or(""));
        if let Some(h) = y.strip_prefix("#### ").or_else(|| x.strip_prefix("#### ")) {
            table = h;
        }
        if x != y {
            out.push((format!("{table}: {x}"), format!("{table}: {y}")));
        }
    }
    out
}

/// The calibration's residual limit on the good link (µs), where the page's timer resolves finer.
const CALIBRATION_US: f64 = 200.0;

/// The calibration's residual limit (µs) for a job: 200 µs, or the page's timer grain where that
/// is coarser (plan 01a1a832e9cc v5, step 4).
fn calibration_limit_us(grain_us: Option<f64>) -> f64 {
    grain_us.filter(|g| g.is_finite()).map_or(CALIBRATION_US, |g| g.max(CALIBRATION_US))
}

/// Step 4's checks of a run (plan 01a1a832e9cc): the calibration's residuals on the good link (p99
/// at most 0.2 ms), the counters against a capture (within 1%), the probe's sizes above the
/// transport against the real UI's in the catalogue (within 2%), the requests behind a stall (their
/// whole wait counted), and each target's CPU noise floor. The text, and whether every check passed.
pub fn check_probe(run: &BrowserRun, catalogue: &serde_json::Value) -> (String, bool) {
    let mut o = String::new();
    let mut pass = true;
    let good: Vec<&JobRecord> = run.jobs.iter().filter(|j| j.profile == "good").collect();
    // Each browser's jobs against its own limit: 200 µs, or the page's timer grain where that is
    // coarser (WebKit's performance.now() steps by 1 ms even cross-origin isolated, so no
    // calibration in it resolves finer: plan v5's step 4).
    let mut browsers: BTreeMap<&str, Vec<(f64, f64)>> = BTreeMap::new();
    for j in &good {
        if let Some(p) = j.calibration_p99_us {
            browsers.entry(j.browser.as_str()).or_default().push((p, calibration_limit_us(j.timer_grain_us)));
        }
    }
    if browsers.is_empty() {
        pass = false;
        let _ = writeln!(o, "- calibration: **no job on the good link has one**");
    }
    for (b, jobs) in &browsers {
        let mut cal: Vec<f64> = jobs.iter().map(|(p, _)| *p).collect();
        cal.sort_by(f64::total_cmp);
        let ok = jobs.iter().all(|(p, limit)| p <= limit);
        let limit = jobs.iter().map(|(_, l)| *l).fold(CALIBRATION_US, f64::max);
        pass &= ok;
        let _ = writeln!(o, "- calibration: {b}: p99 of the residuals on the good link, worst job {} µs, median job {} µs, over {} jobs: {}", cal.last().map_or_else(|| "-".to_owned(), |w| fmt(*w)), super::cells::quantile(&cal, 0.5).map_or_else(|| "-".to_owned(), fmt), cal.len(), if ok { format!("within {} µs", fmt(limit)) } else { format!("**not within {} µs**", fmt(limit)) });
    }
    let caps: Vec<(&JobRecord, u64, u64)> = run.jobs.iter().filter_map(|j| j.capture.map(|(a, b)| (j, a, b))).collect();
    if caps.is_empty() {
        let _ = writeln!(o, "- capture: none in this run");
    }
    for (j, seen, counted) in &caps {
        #[allow(clippy::cast_precision_loss)]
        let off = (*seen as f64 - *counted as f64).abs() / (*counted).max(1) as f64 * 100.0;
        pass &= off <= 1.0;
        let _ = writeln!(o, "- capture: {} {} {} {}: {seen} bytes captured, {counted} counted, {off:.2}% apart{}", j.condition, j.target, j.browser, j.case, if off <= 1.0 { "" } else { " (**over 1%**)" });
    }
    // The probe's bytes above the transport for each operation against the UI's own.
    let ops = catalogue.get("operations").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
    for op in &ops {
        let name = op.get("op").and_then(serde_json::Value::as_str).unwrap_or_default();
        // Only where the UI's payloads were the same each time (p50 and p99 within 1%): elsewhere
        // they follow the lab kiln's state (its sessions' titles, its agents' last words), which a
        // run's kiln doesn't share with the recording's.
        let size = |q: &str| op.pointer(&format!("/bodies/{q}")).or_else(|| op.pointer(&format!("/frames_down/{q}"))).and_then(serde_json::Value::as_f64);
        let (Some(p50), Some(p99)) = (size("p50"), size("p99")) else { continue };
        if (p99 - p50).abs() > p50.max(1.0) * 0.01 {
            let _ = writeln!(o, "- sizes: {name}: not compared, its payloads varied in the recording ({} to {} bytes)", fmt(p50), fmt(p99));
            continue;
        }
        let ui = Some(p50);
        let probe_name = if op.get("case").and_then(serde_json::Value::as_str) == Some("stream") { name.to_owned() } else { name.split('?').next().unwrap_or(name).to_owned() };
        let jobs: Vec<&&JobRecord> = good.iter().filter(|j| j.target.starts_with("kiln-today") && j.numbers.contains_key(&format!("op/{probe_name}/app_bytes"))).collect();
        let (Some(ui), false) = (ui, jobs.is_empty()) else { continue };
        let app: f64 = jobs.iter().filter_map(|j| j.numbers.get(&format!("op/{probe_name}/app_bytes"))).sum();
        let n: f64 = jobs.iter().map(|j| j.numbers.get(&format!("op/{probe_name}/n")).copied().unwrap_or(0.0) + j.numbers.get(&format!("op/{probe_name}/frames")).copied().unwrap_or(0.0)).sum();
        if n == 0.0 {
            continue;
        }
        let mine = app / n;
        let off = (mine - ui).abs() / ui.max(1.0) * 100.0;
        pass &= off <= 2.0;
        let _ = writeln!(o, "- sizes: {name}: the probe's {} bytes an operation against the UI's {} ({off:.1}% apart{})", fmt(mine), fmt(ui), if off <= 2.0 { "" } else { ", **over 2%**" });
    }
    if let Some(at) = run.settings.stall_s {
        for j in run.jobs.iter().filter(|j| j.case == "request") {
            // The request due as the stall began waited all of it: the longest is at least 2 s.
            let top = j.numbers.get("time.max_ms").copied().unwrap_or(0.0);
            let ok = top >= 2000.0;
            pass &= ok;
            let _ = writeln!(o, "- stall at {at} s: {} {} {}: the longest request {} ms, p99 {} ms{}", j.condition, j.target, j.browser, fmt(top), fmt(j.numbers.get("time.p99_ms").copied().unwrap_or(0.0)), if ok { ", the whole wait counted" } else { " (**the stall's wait is missing**)" });
        }
    }
    // Step 5: every path of every job came over the protocol its target is configured for (h2 or
    // h3; h2 in WebKit, which has no HTTP/3 on Linux), as the page's nextHopProtocol says.
    // (The reference servers': kiln today's sockets are HTTP/1.1 connections of their own.)
    for t in run.settings.targets.iter().filter(|t| t.server != super::targets::Server::Kiln) {
        let jobs: Vec<&JobRecord> = run.jobs.iter().filter(|j| j.target == t.name).collect();
        if jobs.is_empty() {
            continue;
        }
        // (paths, of them from the server's log, those over another protocol)
        let mut by_browser: BTreeMap<&str, (usize, usize, Vec<String>)> = BTreeMap::new();
        for j in &jobs {
            let e = by_browser.entry(j.browser.as_str()).or_default();
            let logged = j.server_protocols.iter().filter(|(p, _)| !j.protocols.contains_key(*p) && !p.starts_with("/__lab"));
            for (path, proto, from_log) in j.protocols.iter().map(|(p, v)| (p, v, false)).chain(logged.map(|(p, v)| (p, v, true))) {
                e.0 += 1;
                e.1 += usize::from(from_log);
                if *proto != t.protocol {
                    e.2.push(format!("{path} {proto}{}", if from_log { " (server's log)" } else { "" }));
                }
            }
        }
        for (b, (paths, logged, odd)) in by_browser {
            let ok = paths > 0 && odd.is_empty();
            pass &= ok;
            let from_log = if logged > 0 { format!(" ({logged} of them by the server's log: the page has no timing entry for them)") } else { String::new() };
            let _ = writeln!(o, "- protocol: {} {b}: {paths} paths over {}{from_log}{}", t.name, t.protocol, if ok { String::new() } else if paths == 0 { " (**none recorded**)".to_owned() } else { format!(" (**not: {}**)", odd.join(", ")) });
        }
    }
    let mut floors: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for j in &run.jobs {
        floors.entry(j.target.clone()).or_default().push(j.noise_us_per_s);
    }
    for (t, mut v) in floors {
        v.sort_by(f64::total_cmp);
        let _ = writeln!(o, "- CPU noise floor: {t}: two background windows apart by {} µs of CPU a second (median of {} jobs)", super::cells::quantile(&v, 0.5).map_or_else(|| "-".to_owned(), fmt), v.len());
    }
    (o, pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// plan 01a1a832e9cc v5 step 4: a job's calibration is held to 200 µs, or to its page's timer
    /// grain where that is coarser (WebKit's 1 ms); a job without a grain recorded, to 200 µs.
    #[test]
    fn g1_s4_calibration_limit_is_the_coarser_of_200_us_and_the_grain() {
        assert!((calibration_limit_us(Some(5.0)) - 200.0).abs() < f64::EPSILON);
        assert!((calibration_limit_us(Some(1000.0)) - 1000.0).abs() < f64::EPSILON);
        assert!((calibration_limit_us(None) - 200.0).abs() < f64::EPSILON);
        assert!((calibration_limit_us(Some(f64::INFINITY)) - 200.0).abs() < f64::EPSILON);
    }

    fn cpu_job(round: usize, p50: f64, mean: f64, noise_us_per_s: f64, window_ms: f64) -> Option<JobRecord> {
        let v = serde_json::json!({
            "condition": "50k/1", "profile": "50k", "loss": "1", "seed": 7, "round": round, "target": "kiln-today",
            "browser": "chromium", "case": "stream", "k": 8,
            "numbers": { "cpu_us.p50": p50, "cpu_us.p99": 50_000.0, "cpu_us.p999": 90_000.0, "cpu_us.n": 100.0, "cpu_us.mean": mean, "cpu_us.window_ms.p50": window_ms },
            "protocols": {}, "calibration_p99_us": null, "background_us_per_s": 0.0, "noise_us_per_s": noise_us_per_s,
            "errors": [], "machine": {}, "started_us": 0
        });
        serde_json::from_value(v).ok()
    }

    /// plan 01a1a832e9cc step 4 (and step 7's pilot, where kiln today's CPU p50 at 50k/1 came out
    /// negative): a CPU percentile at or under the background's noise over a reading's length gives
    /// the cell's mean instead, and says so; one above it stays the percentile.
    #[test]
    fn g1_s4_cpu_under_the_noise_floor_gives_the_mean() {
        // 20,000 µs a second of noise over 250 ms readings: a floor of 5,000 µs.
        let jobs: Vec<JobRecord> = [-150.0, -90.0, -20.0, 40.0, 900.0].into_iter().enumerate().filter_map(|(r, p50)| cpu_job(r, p50, 1_200.0, 20_000.0, 250.0)).collect();
        assert_eq!(jobs.len(), 5);
        let refs: Vec<&JobRecord> = jobs.iter().collect();
        assert!(cpu_floor_us(&refs).is_some_and(|f| (f - 5_000.0).abs() < 1e-9));
        let run: Result<BrowserRun, _> = serde_json::from_value(serde_json::json!({
            "command": "", "started_us": 0, "certs_spki": "", "failures": [], "network": [],
            "settings": { "targets": [], "profiles": [], "losses": [], "browsers": [], "cases": [], "rounds": 5, "k": 8, "seed": 7, "window_ms": 25, "background_s": 5, "web": "web" },
            "jobs": serde_json::to_value(&jobs).unwrap_or_default()
        }));
        let Ok(run) = run else { return assert!(run.is_ok(), "{run:?}") };
        let t = tables(&run);
        let p50 = t.lines().find(|l| l.contains("cpu p50")).unwrap_or_default();
        assert!(p50.contains("1200 (1200–1200) n=500.0 (mean: the p50 is under the noise floor, 5000 µs)"), "{p50}");
        assert!(p50.ends_with("kiln-today 1200 (mean) |"), "{p50}");
        let p99 = t.lines().find(|l| l.contains("cpu p99 ")).unwrap_or_default();
        assert!(p99.contains("50000 (50000–50000) n=500.0 |"), "{p99}");
    }

    /// plan 01a1a832e9cc step 10's test: a number of the run changed by hand is a line that
    /// differs, named by its table (case and browser), its condition and measure, with the doc's
    /// value and the run's; the run as it was, none.
    #[test]
    fn g1_s10_a_changed_number_names_its_cell_and_both_values() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let doc = std::fs::read_to_string(root.join("docs/browser-lab.md")).unwrap_or_default();
        let Some(path) = doc.lines().find_map(|l| l.trim().strip_prefix("<!-- browser-lab run: ")?.strip_suffix(" -->")) else { return };
        let run: Result<BrowserRun, _> = serde_json::from_slice(&std::fs::read(root.join(path)).unwrap_or_default());
        let Ok(mut run) = run else { return assert!(run.is_ok(), "{path}: not a run file") };
        assert!(doc_differs(&doc, &run).is_empty(), "the doc isn't {path}'s as it is");
        let cell = |j: &JobRecord| j.case == "burst" && j.browser == "chromium" && j.condition == "50k/1" && j.target == "h2o-h2";
        assert!(run.jobs.iter().any(cell), "{path} has no h2o-h2 burst at 50k/1 in Chromium");
        for j in run.jobs.iter_mut().filter(|j| cell(j)) {
            if let Some(v) = j.numbers.get_mut("time.p50_ms") {
                *v *= 2.0;
            }
        }
        let d = doc_differs(&doc, &run);
        assert_eq!(d.len(), 1, "{d:?}");
        let (was, now) = d.first().map(|(a, b)| (a.as_str(), b.as_str())).unwrap_or_default();
        assert!(was.starts_with("burst in chromium: | 50k/1 | time p50 (ms) |") && now.starts_with("burst in chromium: | 50k/1 | time p50 (ms) |"), "{was}\n{now}");
        assert_ne!(was, now);
    }

    /// plan 01a1a832e9cc step 10: docs/browser-lab.md's tables are the run files' it names
    /// (`<!-- browser-lab run: PATH -->`, a path from the repository's root), regenerated here; a
    /// number changed by hand in either fails, naming the row (its cell) and both values.
    #[test]
    fn browser_doc_truth() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let doc = std::fs::read_to_string(root.join("docs/browser-lab.md")).unwrap_or_default();
        let runs: Vec<&str> = doc.lines().filter_map(|l| l.trim().strip_prefix("<!-- browser-lab run: ")?.strip_suffix(" -->")).collect();
        if runs.is_empty() {
            // No measured matrix yet: the doc says so where its tables go.
            assert!(doc.contains("<!-- browser-lab tables: begin -->"), "docs/browser-lab.md has no tables' markers");
            return;
        }
        for r in runs {
            let text = std::fs::read(root.join(r)).unwrap_or_default();
            let run: Result<BrowserRun, _> = serde_json::from_slice(&text);
            assert!(run.is_ok(), "{r}: not a run file");
            let Ok(run) = run else { return };
            let d = doc_differs(&doc, &run);
            assert!(d.is_empty(), "docs/browser-lab.md's tables aren't {r}'s:\n{}", d.iter().take(10).map(|(a, b)| format!("doc: {a}\nrun: {b}")).collect::<Vec<_>>().join("\n"));
        }
    }
}
