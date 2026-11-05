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
                    let nums: Vec<Option<Number>> = targets.iter().map(|t| per.get(t).and_then(|js| number(js, &k, &n_key))).collect();
                    let best = targets.iter().zip(&nums).filter_map(|(t, n)| n.as_ref().map(|n| (t, n.median))).min_by(|a, b| a.1.total_cmp(&b.1));
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
                        .map(|(t, n)| n.as_ref().map_or_else(|| "-".to_owned(), |n| format!("{} ({}–{}) n={}{}", fmt(n.median), fmt(n.lo), fmt(n.hi), fmt(n.n), if stem == "time" && q == "p999" { band(t).unwrap_or_default() } else { String::new() })))
                        .collect();
                    let _ = writeln!(o, "| {} | {m} {qname} ({unit}) | {} | {} |", c.condition, cellv.join(" | "), best.map_or_else(|| "-".to_owned(), |(t, v)| format!("{t} {}", fmt(v))));
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
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(""), b.get(i).copied().unwrap_or(""));
        if x != y {
            out.push((x.to_owned(), y.to_owned()));
        }
    }
    out
}

/// Step 4's checks of a run (plan 01a1a832e9cc): the calibration's residuals on the good link (p99
/// at most 0.2 ms), the counters against a capture (within 1%), the probe's sizes above the
/// transport against the real UI's in the catalogue (within 2%), the requests behind a stall (their
/// whole wait counted), and each target's CPU noise floor. The text, and whether every check passed.
pub fn check_probe(run: &BrowserRun, catalogue: &serde_json::Value) -> (String, bool) {
    let mut o = String::new();
    let mut pass = true;
    let good: Vec<&JobRecord> = run.jobs.iter().filter(|j| j.profile == "good").collect();
    let mut cal: Vec<f64> = good.iter().filter_map(|j| j.calibration_p99_us).collect();
    cal.sort_by(f64::total_cmp);
    let worst = cal.last().copied();
    let ok = worst.is_some_and(|w| w <= 200.0);
    pass &= ok;
    let _ = writeln!(o, "- calibration: p99 of the residuals on the good link, worst job {} µs, median job {} µs, over {} jobs: {}", worst.map_or_else(|| "-".to_owned(), fmt), super::cells::quantile(&cal, 0.5).map_or_else(|| "-".to_owned(), fmt), cal.len(), if ok { "within 200 µs" } else { "**not within 200 µs**" });
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
        let ui = op.pointer("/bodies/mean").or_else(|| op.pointer("/frames_down/p50")).and_then(serde_json::Value::as_f64);
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
            let top = j.numbers.get("time.p99_ms").copied().unwrap_or(0.0);
            let ok = top >= 2000.0;
            pass &= ok;
            let _ = writeln!(o, "- stall at {at} s: {} {} {}: requests' p99 {} ms{}", j.condition, j.target, j.browser, fmt(top), if ok { ", the whole wait counted" } else { " (**the stall's wait is missing**)" });
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
