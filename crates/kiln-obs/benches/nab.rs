//! The anomaly detectors scored on the Numenta Anomaly Benchmark (`docs/observe.md` §9.1, §12):
//! 58 labelled series, 365,551 points, 116 anomaly windows (Lavin and Ahmad 2015).
//!
//! First the scorer (`kiln_obs::anomaly::nab`) is checked: the results NAB publishes for its own
//! detectors are scored here and must come out as NAB's `final_results.json` says. Then each of
//! kiln's algorithms runs over every series as it would live: started on the series' probation (its
//! first 15%, NAB's learning period, unscored), then each point's anomaly score from what the
//! detector expected before it saw it. Scored by every profile, with the time a point takes.
//!
//! `NAB=/path/to/NAB cargo bench -p kiln-obs --bench nab` (a checkout of github.com/numenta/NAB at
//! v1.1's commit; `data/`, `labels/`, `results/`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use kiln_obs::anomaly::nab::{self, File, PROFILES};
use kiln_obs::anomaly::{Algorithm, Detector, NORMAL_AT_LEVEL, Scorer, Tail};

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` (UTC) in unix seconds.
fn seconds(s: &str) -> i64 {
    let s = s.trim().trim_matches('"');
    let (date, time) = s.split_once(' ').unwrap_or((s, "00:00:00"));
    let d: Vec<i64> = date.split('-').map(|x| x.parse().unwrap()).collect();
    let t: Vec<f64> = time.split(':').map(|x| x.parse().unwrap()).collect();
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, m, day) = (d[0], d[1], d[2]);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + (t[0] * 3600.0 + t[1] * 60.0 + t[2]) as i64
}

/// A CSV's columns by name: each row's fields.
fn csv(path: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines();
    let head: Vec<String> = lines.next().unwrap_or("").split(',').map(str::to_owned).collect();
    let rows = lines.filter(|l| !l.trim().is_empty()).map(|l| l.split(',').map(str::to_owned).collect()).collect();
    (head, rows)
}

struct Corpus {
    files: Vec<File>,
    values: Vec<Vec<f64>>,
}

fn corpus(root: &Path) -> Corpus {
    let labels: BTreeMap<String, Vec<Vec<String>>> = serde_json::from_str(&std::fs::read_to_string(root.join("labels/combined_windows.json")).unwrap()).unwrap();
    let mut files = Vec::new();
    let mut values = Vec::new();
    for (name, windows) in labels {
        let (_, rows) = csv(&root.join("data").join(&name));
        files.push(File { name: name.clone(), timestamps: rows.iter().map(|r| seconds(&r[0])).collect(), windows: windows.iter().map(|w| (seconds(&w[0]), seconds(&w[1]))).collect() });
        values.push(rows.iter().map(|r| r[1].parse::<f64>().unwrap()).collect());
    }
    Corpus { files, values }
}

/// A NAB detector's published anomaly scores, file by file; none if it is not checked out.
fn published(root: &Path, detector: &str, files: &[File]) -> Option<Vec<Vec<f64>>> {
    let mut out = Vec::new();
    for f in files {
        let (dir, file) = f.name.rsplit_once('/').unwrap();
        let path: PathBuf = root.join("results").join(detector).join(dir).join(format!("{detector}_{file}"));
        if !path.exists() {
            return None;
        }
        let (head, rows) = csv(&path);
        let col = head.iter().position(|h| h == "anomaly_score")?;
        out.push(rows.iter().map(|r| r[col].parse::<f64>().unwrap()).collect());
    }
    Some(out)
}

/// The points' usual distance: the median of their differences (seconds).
fn spacing(ts: &[i64]) -> f64 {
    let mut d: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    d.sort_unstable();
    d.get(d.len() / 2).copied().unwrap_or(300) as f64
}

/// What NAB's sweeper leaves unscored: the first 15% (750 rows at the most).
fn probation(n: usize) -> usize {
    ((0.15 * n as f64).floor().min(750.0)) as usize
}

/// kiln's scorer over a file, as it would run live: started on the probation, each later point's
/// NAB score its error in calibrated deviations (an episode's first point alone, past the
/// threshold), mapped into [0, 1).
fn scorer(algo: Algorithm, f: &File, values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let learn = probation(n).max(8);
    let mut out = vec![0.0; learn.min(n)];
    let Some(mut s) = Scorer::new(algo, &values[..learn.min(n)], spacing(&f.timestamps)) else { return vec![0.0; n] };
    for &x in &values[learn.min(n)..] {
        out.push(match s.step(x) {
            Some(p) if p.score.abs() >= NORMAL_AT_LEVEL && !p.onset => 0.0,
            Some(p) => unit(p.score.abs() / NORMAL_AT_LEVEL),
            None => 0.0,
        });
    }
    out
}

/// How a point's |z| becomes its anomaly score (the study of §12.12).
#[derive(Clone, Copy, PartialEq)]
enum Calibration {
    /// `|z|` itself, mapped into [0, 1) keeping its order: one threshold of |z| over the corpus.
    Raw,
    /// One less the probability of so large a |z| for its series (`anomaly::Tail`).
    Tail,
    /// `|z|` over the largest |z| of the series' past: above 1, a new record.
    Record,
    /// `|z|` over the series' past |z|'s quantile.
    Quantile(f64),
    /// As `Quantile`, an episode (from a point past the quantile until one back under it) scored by
    /// its first point alone.
    Episodes(f64),
    /// As `Episodes`, the quantile estimated over the threshold (peaks over the 98% quantile, an
    /// exponential tail): sound past the largest value seen, when few are.
    Pot(f64),
}

/// The `q` quantile of sorted `past`: peaks over their 98% quantile `t` taken as exponential (their
/// mean excess σ), q = t + σ ln(N_t / (n (1 - q))); the empirical quantile where that is lower than
/// it (a tail heavier than exponential, seen).
fn pot_quantile(past: &[f64], q: f64) -> f64 {
    let n = past.len();
    if n == 0 {
        return 0.0;
    }
    let at = ((n as f64 * 0.98) as usize).min(n - 1);
    let t = past[at];
    let peaks = &past[at + 1..];
    let empirical = past[((n as f64 * q) as usize).min(n - 1)];
    if peaks.is_empty() {
        return empirical;
    }
    let sigma = peaks.iter().map(|x| x - t).sum::<f64>() / peaks.len() as f64;
    let pot = t + sigma * (peaks.len() as f64 / (n as f64 * (1.0 - q))).ln().max(0.0);
    pot.max(empirical)
}

/// A score above 0 mapped into [0, 1), keeping its order.
fn unit(x: f64) -> f64 {
    if x.is_infinite() { 1.0 } else { x / (1.0 + x) }
}

/// A detector's anomaly scores for a file, as it would run live: started on the probation, each
/// later point scored from what it expected before it saw it.
fn run(algo: Algorithm, cal: Calibration, f: &File, values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let learn = probation(n).max(2);
    let sp = spacing(&f.timestamps);
    let days = std::env::var("NAB_DAYS").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
    let window = ((days * 86_400.0 / sp).round() as usize).clamp(24, learn);
    // The probation's own |z|, for the tail: the detector run over it as over the rest.
    let half = (learn / std::env::var("NAB_EARLY").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2)).max(2);
    let mut early = Detector::new(algo, &values[..half], sp, window);
    let seen: Vec<f64> = values[half..learn].iter().filter_map(|&x| early.step(x).map(|s| s.z.abs())).collect();
    let mut tail = Tail::new(&seen);
    let mut past: Vec<f64> = seen.iter().copied().filter(|z| z.is_finite()).collect();
    past.sort_unstable_by(f64::total_cmp);
    let mut d = Detector::new(algo, &values[..learn], sp, window);
    let mut out = vec![0.0; learn];
    let mut episode = false;
    for &x in &values[learn..] {
        let z = d.step(x).map_or(0.0, |s| s.z.abs());
        let score = match (cal, &mut tail) {
            // -log10 P, mapped into [0, 1): 1 - P would round every P below 1e-16 to one.
            (Calibration::Tail, Some(t)) => {
                let p = t.observe(z);
                unit(if p > 0.0 { -p.log10() } else { 400.0 })
            }
            (Calibration::Record, _) => unit(z / past.last().copied().unwrap_or(0.0).max(1e-9)),
            (Calibration::Quantile(q), _) => {
                let at = ((past.len() as f64 * q) as usize).min(past.len().saturating_sub(1));
                unit(z / past.get(at).copied().unwrap_or(0.0).max(1e-9))
            }
            (Calibration::Episodes(q) | Calibration::Pot(q), _) => {
                let reference = if let Calibration::Pot(_) = cal {
                    pot_quantile(&past, q)
                } else {
                    let at = ((past.len() as f64 * q) as usize).min(past.len().saturating_sub(1));
                    past.get(at).copied().unwrap_or(0.0)
                };
                let r = z / reference.max(1e-9);
                let first = r >= 1.0 && !episode;
                episode = r >= 1.0;
                if r >= 1.0 && !first { 0.0 } else { unit(r) }
            }
            _ => unit(z),
        };
        if z.is_finite() {
            let at = past.partition_point(|v| *v < z);
            past.insert(at, z);
        }
        out.push(score);
    }
    out
}

fn main() {
    let Some(root) = std::env::var_os("NAB").map(PathBuf::from) else {
        println!("NAB=<a checkout of github.com/numenta/NAB> cargo bench -p kiln-obs --bench nab");
        return;
    };
    let t0 = Instant::now();
    let c = corpus(&root);
    let points: usize = c.values.iter().map(Vec::len).sum();
    println!("{} files, {} points, {} windows (read in {:?})", c.files.len(), points, c.files.iter().map(|f| f.windows.len()).sum::<usize>(), t0.elapsed());

    // The scorer, checked against NAB's own results.
    let finals: BTreeMap<String, BTreeMap<String, f64>> = serde_json::from_str(&std::fs::read_to_string(root.join("results/final_results.json")).unwrap()).unwrap();
    println!("\nthe scorer against NAB's published scores");
    println!("{:<18} {:<20} {:>10} {:>10} {:>10}", "detector", "profile", "here", "NAB", "diff");
    // NAB scores each detector at the threshold `config/thresholds.json` keeps (its optimiser's,
    // when it was run): scored at it here, the scores must be NAB's; the threshold this sweep finds
    // is shown beside, and its score, which is never lower.
    let thresholds: BTreeMap<String, BTreeMap<String, BTreeMap<String, f64>>> = serde_json::from_str(&std::fs::read_to_string(root.join("config/thresholds.json")).unwrap()).unwrap();
    println!("{:<18} {:<20} {:>10} {:>10} {:>10} {:>10} {:>12}", "detector", "profile", "here", "NAB", "diff", "swept", "its threshold");
    let mut worst = 0.0f64;
    for detector in ["numenta", "twitterADVec", "windowedGaussian", "random", "null"] {
        let Some(scores) = published(&root, detector, &c.files) else { continue };
        for p in &PROFILES {
            let theirs = finals.get(detector).and_then(|d| d.get(p.name)).copied().unwrap_or(f64::NAN);
            let threshold = thresholds.get(detector).and_then(|d| d.get(p.name)).and_then(|t| t.get("threshold")).copied();
            let here = threshold.map_or_else(|| nab::score(&c.files, &scores, p), |t| nab::score_at(&c.files, &scores, p, t)).normalised;
            let swept = nab::score(&c.files, &scores, p);
            worst = worst.max((here - theirs).abs());
            println!("{detector:<18} {:<20} {here:>10.4} {theirs:>10.4} {:>10.2e} {:>10.4} {:>12.6}", p.name, here - theirs, swept.normalised, swept.threshold);
        }
    }
    println!("largest difference: {worst:.2e}");

    // A detector's view of each file (NAB_DIAG=basic|agile|robust, NAB_CAL=raw|tail): each window's
    // best score, the worst of those, and how many points outside the windows (after probation)
    // score at least that.
    if let Some(algo) = std::env::var("NAB_DIAG").ok().and_then(|a| Algorithm::parse(&a)) {
        let cal = match std::env::var("NAB_CAL").as_deref() {
            Ok("tail") => Calibration::Tail,
            Ok("record") => Calibration::Record,
            Ok("q999") => Calibration::Quantile(0.999),
            _ => Calibration::Raw,
        };
        println!("\n{:<56} {:>7} {:>9} {:>9} {:>9} {:>9}", "file", "rows", "w best", "w worst", "out above", "out max");
        for (f, v) in c.files.iter().zip(&c.values) {
            let n = v.len();
            let learn = probation(n).max(2);
            let scores = run(algo, cal, f, v);
            let in_window = |i: usize| f.windows.iter().any(|&(a, b)| (a..=b).contains(&f.timestamps[i]));
            let bests: Vec<f64> = f.windows.iter().map(|&(a, b)| (learn..n).filter(|&i| (a..=b).contains(&f.timestamps[i])).map(|i| scores[i]).fold(0.0, f64::max)).collect();
            let best = bests.iter().copied().fold(0.0, f64::max);
            let worst = bests.iter().copied().fold(f64::INFINITY, f64::min);
            let out: Vec<f64> = (learn..n).filter(|&i| !in_window(i)).map(|i| scores[i]).collect();
            let floor = if bests.is_empty() { 0.0 } else { worst };
            let above = out.iter().filter(|&&x| x >= floor).count();
            let out_max = out.iter().copied().fold(0.0, f64::max);
            println!("{:<56} {:>7} {:>9.4} {:>9.4} {:>9} {:>9.4}", f.name, n, best, if bests.is_empty() { f64::NAN } else { worst }, above, out_max);
        }
    }

    // Where a detector loses at its global threshold (NAB_LOSS=basic|agile|robust, NAB_CAL as
    // above): each file's windows found and missed, and its false positives.
    if let Some(algo) = std::env::var("NAB_LOSS").ok().and_then(|a| Algorithm::parse(&a)) {
        let cal = match std::env::var("NAB_CAL").as_deref() {
            Ok("e99") => Calibration::Episodes(0.99),
            Ok("q999") => Calibration::Quantile(0.999),
            _ => Calibration::Episodes(0.999),
        };
        let scores: Vec<Vec<f64>> = c.files.iter().zip(&c.values).map(|(f, v)| run(algo, cal, f, v)).collect();
        let t = nab::score(&c.files, &scores, &PROFILES[0]).threshold;
        println!("\nat its threshold {t:.6}: {:<52} {:>6} {:>6} {:>6} {:>10}", "file", "found", "missed", "FP", "first FP");
        let (mut found_all, mut missed_all, mut fp_all) = (0, 0, 0);
        for ((f, v), s) in c.files.iter().zip(&c.values).zip(&scores) {
            let learn = probation(v.len()).max(2);
            let hit = |i: usize| s[i] >= t;
            let found = f.windows.iter().filter(|&&(a, b)| (learn..v.len()).any(|i| (a..=b).contains(&f.timestamps[i]) && hit(i))).count();
            let scorable = f.windows.iter().filter(|&&(_, b)| f.timestamps.iter().position(|&x| x == b).is_some_and(|i| i >= learn)).count();
            let fps: Vec<usize> = (learn..v.len()).filter(|&i| hit(i) && !f.windows.iter().any(|&(a, b)| (a..=b).contains(&f.timestamps[i]))).collect();
            found_all += found;
            missed_all += scorable - found;
            fp_all += fps.len();
            if scorable != found || !fps.is_empty() {
                println!("{:<70} {:>6} {:>6} {:>6} {:>10}", f.name, found, scorable - found, fps.len(), fps.first().map_or(String::new(), |i| i.to_string()));
            }
        }
        println!("windows found {found_all}, missed {missed_all}; false positives {fp_all}");
    }

    // kiln's scorer.
    println!("\nkiln's anomaly scorer");
    println!("{:<10} {:>10} {:>10} {:>10} {:>12} {:>12} {:>10}", "algorithm", "standard", "low FP", "low FN", "ns a point", "threshold", "per file");
    for algo in [Algorithm::Basic, Algorithm::Agile, Algorithm::Robust] {
        let t = Instant::now();
        let scores: Vec<Vec<f64>> = c.files.iter().zip(&c.values).map(|(f, v)| scorer(algo, f, v)).collect();
        let ns = t.elapsed().as_nanos() as f64 / points as f64;
        let s: Vec<nab::Scored> = PROFILES.iter().map(|p| nab::score(&c.files, &scores, p)).collect();
        let bound = nab::per_file_best(&c.files, &scores, &PROFILES[0]);
        println!("{:<10} {:>10.2} {:>10.2} {:>10.2} {:>12.1} {:>12.8} {:>10.2}", algo.as_str(), s[0].normalised, s[1].normalised, s[2].normalised, ns, s[0].threshold, bound);
    }
    if std::env::var("NAB_STUDY").is_err() {
        return;
    }

    // The study: how |z| became a score (NAB_STUDY=1; NAB_EARLY, NAB_DAYS, NAB_LEVELS).
    println!("\nthe study");
    println!("{:<10} {:<6} {:>10} {:>10} {:>10} {:>12} {:>12} {:>10}", "algorithm", "score", "standard", "low FP", "low FN", "ns a point", "threshold", "per file");
    for algo in [Algorithm::Basic, Algorithm::Agile, Algorithm::Robust] {
        let levels: Vec<f64> = std::env::var("NAB_LEVELS").ok().map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_else(|| vec![0.998, 0.999, 0.9995]);
        let study: Vec<(Calibration, String)> = [(Calibration::Raw, "|z|".to_owned()), (Calibration::Tail, "tail".to_owned()), (Calibration::Record, "record".to_owned()), (Calibration::Quantile(0.999), "q999".to_owned())]
            .into_iter()
            .chain(levels.iter().flat_map(|&q| {
                let digits = format!("{q}")[2..].to_owned();
                [(Calibration::Episodes(q), format!("e{digits}")), (Calibration::Pot(q), format!("p{digits}"))]
            }))
            .collect();
        for (cal, name) in study {
            let t = Instant::now();
            let scores: Vec<Vec<f64>> = c.files.iter().zip(&c.values).map(|(f, v)| run(algo, cal, f, v)).collect();
            let ns = t.elapsed().as_nanos() as f64 / points as f64;
            let s: Vec<nab::Scored> = PROFILES.iter().map(|p| nab::score(&c.files, &scores, p)).collect();
            let bound = nab::per_file_best(&c.files, &scores, &PROFILES[0]);
            println!("{:<10} {:<6} {:>10.2} {:>10.2} {:>10.2} {:>12.1} {:>12.8} {:>10.2}", algo.as_str(), name, s[0].normalised, s[1].normalised, s[2].normalised, ns, s[0].threshold, bound);
        }
    }
}
