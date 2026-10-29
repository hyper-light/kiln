//! The Numenta Anomaly Benchmark's scoring (Lavin and Ahmad, "Evaluating Real-time Anomaly
//! Detection Algorithms — the Numenta Anomaly Benchmark", IEEE ICMLA 2015), ported from NAB v1.1
//! (`nab/sweeper.py`, `nab/optimizer.py`, `nab/scorer.py`, `nab/runner.py`'s normalisation) line
//! for line, so a detector's score here is NAB's: `benches/nab.rs` checks it against the scores NAB
//! publishes for its own detectors' results.
//!
//! - Each labelled window rewards its earliest detection by a scaled sigmoid of where in the window
//!   it fell (its left edge 1, its right edge 0); a detection outside every window costs the
//!   profile's false-positive weight, less if it follows a window closely; a window without a
//!   detection costs the false-negative weight.
//! - The first 15% of each file (750 rows at the most) is probation: nothing in it is scored.
//! - A detector has one threshold, the one that scores the whole corpus best for the profile.
//! - The corpus' score is normalised: 100 × (raw − null) / (perfect − null), the null detector's
//!   raw score its zero and every window detected at its left edge its hundred.

/// A profile's weights (`config/profiles.json`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub name: &'static str,
    pub tp: f64,
    pub fp: f64,
    pub r#fn: f64,
}

pub const PROFILES: [Profile; 3] = [
    Profile { name: "standard", tp: 1.0, fp: 0.11, r#fn: 1.0 },
    Profile { name: "reward_low_FP_rate", tp: 1.0, fp: 0.22, r#fn: 1.0 },
    Profile { name: "reward_low_FN_rate", tp: 1.0, fp: 0.11, r#fn: 2.0 },
];

/// The part of each file not scored, and its cap (`probationPercent * 5000` rows).
const PROBATION: f64 = 0.15;

/// A labelled file: its timestamps (unix seconds, in order) and its anomaly windows' first and last
/// timestamps.
#[derive(Debug, Clone)]
pub struct File {
    pub name: String,
    pub timestamps: Vec<i64>,
    pub windows: Vec<(i64, i64)>,
}

/// Where a point is, as the sweeper names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Where {
    Probation,
    Outside,
    /// In window `n` (unique over the corpus).
    Window(u32),
}

/// A point as the sweeper keeps it: its anomaly score, its weighted score if detected, and where.
#[derive(Debug, Clone, Copy)]
struct Point {
    score: f64,
    sweep: f64,
    at: Where,
}

/// A threshold's outcome: every point scoring at or above it a detection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outcome {
    pub threshold: f64,
    pub score: f64,
    pub tp: u64,
    pub tn: u64,
    pub fp: u64,
    pub r#fn: u64,
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// NAB's `scaledSigmoid`: 0.98661 at a window's left edge (-1), 0 at its right (0), -0.98661 a
/// window's width past it, -1 beyond three.
fn scaled_sigmoid(position: f64) -> f64 {
    if position > 3.0 { -1.0 } else { 2.0 * sigmoid(-5.0 * position) - 1.0 }
}

fn probation(rows: usize) -> f64 {
    (PROBATION * rows as f64).floor().min(PROBATION * 5000.0)
}

/// `calcSweepScore`: each point's weighted score and place. `first` numbers the file's windows
/// apart from the corpus' others.
fn sweep(file: &File, scores: &[f64], p: &Profile, first: u32) -> Vec<Point> {
    let max_tp = scaled_sigmoid(-1.0);
    let probation = probation(file.timestamps.len());
    let mut windows = file.windows.iter().copied().enumerate().peekable();
    let index = |t: i64| file.timestamps.iter().position(|&x| x == t);
    // The window the point is in: its number, last timestamp, right index and width.
    let mut cur: Option<(u32, i64, usize, f64)> = None;
    let mut prev: Option<(usize, f64)> = None;
    let mut out = Vec::with_capacity(scores.len());
    for (i, (&t, &score)) in file.timestamps.iter().zip(scores).enumerate() {
        if let Some(&(n, (start, end))) = windows.peek()
            && t == start
        {
            windows.next();
            let right = index(end).unwrap_or(i);
            let left = index(start).unwrap_or(i);
            cur = Some((first + n as u32, end, right, (right as f64) - (left as f64) + 1.0));
        }
        let sweep = match cur {
            Some((_, _, right, width)) => {
                let position = -((right as f64) - (i as f64) + 1.0) / width;
                scaled_sigmoid(position) * p.tp / max_tp
            }
            None => {
                let unweighted = match prev {
                    None => -1.0,
                    Some((right, width)) => {
                        let den = width - 1.0;
                        if den <= 0.0 { -1.0 } else { scaled_sigmoid(((right as f64) - (i as f64)).abs() / den) }
                    }
                };
                unweighted * p.fp
            }
        };
        let at = if (i as f64) < probation {
            Where::Probation
        } else {
            cur.map_or(Where::Outside, |(n, ..)| Where::Window(n))
        };
        out.push(Point { score, sweep, at });
        if let Some((_, end, _, width)) = cur
            && t == end
        {
            prev = Some((i, width));
            cur = None;
        }
    }
    out
}

/// `calcScoreByThreshold`: the outcome of each threshold the points' scores make, from the highest
/// (nothing detected) down.
fn by_threshold(points: &[Point], p: &Profile, windows: usize) -> Vec<Outcome> {
    let mut scorable: Vec<&Point> = points.iter().filter(|x| x.at != Where::Probation).collect();
    // Python's `sorted(reverse=True)` is stable, ties in their order; so is this.
    scorable.sort_by(|a, b| b.score.total_cmp(&a.score));
    // Each window in a scorable point: missed (-fn) until a detection in it scores better.
    let mut best: Vec<Option<f64>> = vec![None; windows];
    for x in &scorable {
        if let Where::Window(n) = x.at
            && let Some(b) = best.get_mut(n as usize)
        {
            *b = Some(-p.r#fn);
        }
    }
    let mut fp_sum = 0.0;
    let total = |best: &[Option<f64>], fp_sum: f64| fp_sum + best.iter().flatten().sum::<f64>();
    let mut tn = scorable.iter().filter(|x| x.at == Where::Outside).count() as u64;
    let mut r#fn = scorable.len() as u64 - tn;
    let (mut tp, mut fp) = (0u64, 0u64);
    let mut out = Vec::new();
    let mut threshold = 1.1;
    for x in &scorable {
        if x.score != threshold {
            out.push(Outcome { threshold, score: total(&best, fp_sum), tp, tn, fp, r#fn });
            threshold = x.score;
        }
        match x.at {
            Where::Window(n) => {
                tp += 1;
                r#fn -= 1;
                if let Some(Some(b)) = best.get_mut(n as usize) {
                    *b = b.max(x.sweep);
                }
            }
            _ => {
                fp += 1;
                tn -= 1;
                fp_sum += x.sweep;
            }
        }
    }
    out.push(Outcome { threshold, score: total(&best, fp_sum), tp, tn, fp, r#fn });
    out
}

/// A detector's anomaly scores for a corpus, scored for a profile: the threshold it is given, its
/// raw score and counts, and its normalised score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored {
    pub threshold: f64,
    pub raw: f64,
    pub normalised: f64,
    pub tp: u64,
    pub fp: u64,
    pub r#fn: u64,
}

/// Scores `scores` (each file's, in its rows' order, each in [0, 1]) over `files` for `p`, its
/// threshold the corpus' best (`optimizer.py`), each file scored at it (`scorer.py`), the total
/// normalised against the null detector's (`runner.py`).
pub fn score(files: &[File], scores: &[Vec<f64>], p: &Profile) -> Scored {
    let (windows, swept) = swept(files, scores, p);
    let all: Vec<Point> = swept.iter().flatten().copied().collect();
    // The best outcome; of equal ones the first (the highest threshold), as Python's stable sort.
    let best = by_threshold(&all, p, windows).into_iter().fold(None::<Outcome>, |b, o| match b {
        Some(b) if b.score >= o.score => Some(b),
        _ => Some(o),
    });
    at(files, &swept, windows, p, best.map_or(1.1, |b| b.threshold))
}

/// As [`score`], at a threshold given (NAB's `config/thresholds.json`'s, say).
pub fn score_at(files: &[File], scores: &[Vec<f64>], p: &Profile, threshold: f64) -> Scored {
    let (windows, swept) = swept(files, scores, p);
    at(files, &swept, windows, p, threshold)
}

fn swept(files: &[File], scores: &[Vec<f64>], p: &Profile) -> (usize, Vec<Vec<Point>>) {
    let windows: usize = files.iter().map(|f| f.windows.len()).sum();
    let mut first = 0u32;
    let swept = files
        .iter()
        .zip(scores)
        .map(|(f, s)| {
            let points = sweep(f, s, p, first);
            first += f.windows.len() as u32;
            points
        })
        .collect();
    (windows, swept)
}

fn at(files: &[File], swept: &[Vec<Point>], windows: usize, p: &Profile, threshold: f64) -> Scored {
    let (mut raw, mut tp, mut fp, mut r#fn) = (0.0, 0, 0, 0);
    for points in swept {
        let rows = by_threshold(points, p, windows);
        // The row of the threshold, else the one before the first below it (`scoreDataSet`).
        let mut matching = None;
        let mut prev = None;
        for r in &rows {
            if r.threshold == threshold {
                matching = Some(*r);
                break;
            } else if r.threshold < threshold {
                matching = prev;
                break;
            }
            prev = Some(*r);
        }
        if let Some(r) = matching.or(prev) {
            raw += r.score;
            tp += r.tp;
            fp += r.fp;
            r#fn += r.r#fn;
        }
    }
    let null = null(files, p);
    let perfect = windows as f64 * p.tp;
    Scored { threshold, raw, normalised: 100.0 * (raw - null) / (perfect - null), tp, fp, r#fn }
}

/// Not NAB's: each file at its own best threshold, the files' best raw scores summed and normalised.
/// An upper bound on any one threshold's score: what a detector's ranking within each file allows,
/// apart from how its scores compare across files.
pub fn per_file_best(files: &[File], scores: &[Vec<f64>], p: &Profile) -> f64 {
    let (windows, swept) = swept(files, scores, p);
    let raw: f64 = swept.iter().map(|points| by_threshold(points, p, windows).iter().map(|o| o.score).fold(f64::NEG_INFINITY, f64::max)).sum();
    let null = null(files, p);
    100.0 * (raw - null) / (windows as f64 * p.tp - null)
}

/// The null detector's raw score: every point the same score, so nothing detected at the best
/// threshold (`nab/detectors/null`).
fn null(files: &[File], p: &Profile) -> f64 {
    let windows: usize = files.iter().map(|f| f.windows.len()).sum();
    let mut n = 0u32;
    let mut raw = 0.0;
    for f in files {
        let points = sweep(f, &vec![0.5; f.timestamps.len()], p, n);
        n += f.windows.len() as u32;
        if let Some(r) = by_threshold(&points, p, windows).first() {
            raw += r.score;
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: i64, windows: &[(i64, i64)]) -> File {
        File { name: "f".into(), timestamps: (0..n).collect(), windows: windows.to_vec() }
    }

    #[test]
    fn the_sigmoid_is_nab_s() {
        assert!((scaled_sigmoid(-1.0) - 0.986_614_298_151_430_3).abs() < 1e-12);
        assert!((scaled_sigmoid(-0.5) - 0.848_283_639_957_512_8).abs() < 1e-12);
        assert_eq!(scaled_sigmoid(0.0), 0.0);
        assert_eq!(scaled_sigmoid(3.5), -1.0);
    }

    /// One window, 1,000 rows (150 of probation): detected at its left edge, a perfect score; never
    /// detected, the null's; one false positive far from it costs the profile's weight.
    /// 1,000 scores, 1 at `at`, 0 elsewhere.
    fn ones(at: &[usize]) -> Vec<f64> {
        (0..1000).map(|i| if at.contains(&i) { 1.0 } else { 0.0 }).collect()
    }

    #[test]
    fn a_window_detected_at_its_start_scores_perfect_and_a_false_positive_costs_its_weight() {
        let f = vec![file(1000, &[(500, 599)])];
        let perfect = ones(&[500]);
        let s = score(&f, &[perfect], &PROFILES[0]);
        assert!((s.normalised - 100.0).abs() < 1e-9, "{s:?}");
        assert_eq!((s.tp, s.fp, s.r#fn), (1, 0, 99));
        let none = score(&f, &[vec![0.0; 1000]], &PROFILES[0]);
        assert!(none.normalised.abs() < 1e-9, "{none:?}");
        // The window found, and one false positive before it: -0.11 of 1.11.
        let s = score(&f, &[ones(&[200, 500])], &PROFILES[0]);
        assert!((s.raw - (1.0 - 0.11)).abs() < 1e-9 && (s.normalised - 100.0 * (0.89 + 1.0) / 2.0).abs() < 1e-9, "{s:?}");
        // In probation, nothing counts.
        assert!(score(&f, &[ones(&[10])], &PROFILES[0]).normalised.abs() < 1e-9);
    }
}
