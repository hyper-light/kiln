//! Experiments (docs/ci.md W20-R11, after docs/experiments.md): which variants an experiment
//! measures, from its options and its strategy; and what its measurements say. Each variant's
//! required checks, each metric against the baseline with its 95 % interval, each option's main
//! effect and each pair's interaction (the combined effect less the sum of the parts), and the
//! recommendation: the best primary objective among the variants that pass every required check.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::stats::{self, Better, Interval, Verdict};

/// An experiment measures at most this many variants.
pub const MOST_VARIANTS: usize = 32;

/// How an experiment's options make variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Each option alone: the options are alternatives.
    Each,
    /// Every combination: a few options that may stack (3 make 7).
    All,
    /// Each alone and every pair: main effects and two-way interactions.
    Pairs,
    /// Rounds: each alone, then the best so far with each of the rest, while one adds enough.
    Greedy,
    /// The sets given.
    Custom(Vec<Vec<String>>),
}

/// A variant: the options it has, in the options' order; none for the baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    /// `baseline`, `a`, `a+b`, …
    pub name: String,
    pub options: Vec<String>,
}

impl Variant {
    pub fn of(options: Vec<String>) -> Self {
        let name = if options.is_empty() { "baseline".to_owned() } else { options.join("+") };
        Variant { name, options }
    }
}

/// The variants `strategy` makes of `options` (for `greedy`, its first round), the baseline first
/// when `baseline`; or why it can't.
pub fn variants(options: &[String], strategy: &Strategy, baseline: bool) -> Result<Vec<Variant>, String> {
    if options.is_empty() {
        return Err("an experiment needs at least one option".into());
    }
    let mut seen: Vec<&str> = Vec::new();
    for o in options {
        let ok = !o.is_empty() && o.len() <= 40 && o.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && o != "baseline";
        if !ok {
            return Err(format!("`{o}`: an option's name is up to 40 letters, digits, `-` and `_` (and not `baseline`)"));
        }
        if seen.contains(&o.as_str()) {
            return Err(format!("`{o}` is there twice"));
        }
        seen.push(o);
    }
    let single = |i: usize| options.get(i).cloned().into_iter().collect::<Vec<String>>();
    let mut sets: Vec<Vec<String>> = match strategy {
        Strategy::Each | Strategy::Greedy => (0..options.len()).map(single).collect(),
        Strategy::Pairs => {
            let mut v: Vec<Vec<String>> = (0..options.len()).map(single).collect();
            for i in 0..options.len() {
                for j in i + 1..options.len() {
                    v.push([single(i), single(j)].concat());
                }
            }
            v
        }
        Strategy::All => {
            if options.len() > 5 {
                return Err(format!("every combination of {} options is {} variants, more than {MOST_VARIANTS}: pairs or greedy stacking take on more", options.len(), (1usize << options.len()) - 1));
            }
            let mut v: Vec<Vec<String>> = (1usize..(1 << options.len())).map(|mask| options.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, o)| o.clone()).collect()).collect();
            v.sort_by_key(Vec::len);
            v
        }
        Strategy::Custom(given) => {
            let mut v = Vec::new();
            for set in given {
                if set.is_empty() {
                    return Err("a custom variant has at least one option (the baseline is `baseline`)".into());
                }
                if let Some(o) = set.iter().find(|o| !options.contains(o)) {
                    return Err(format!("`{o}` isn't one of the options"));
                }
                // In the options' order, once each.
                let ordered: Vec<String> = options.iter().filter(|o| set.contains(o)).cloned().collect();
                if v.contains(&ordered) {
                    return Err(format!("{} is there twice", ordered.join("+")));
                }
                v.push(ordered);
            }
            v
        }
    };
    if baseline {
        sets.insert(0, Vec::new());
    }
    if sets.len() > MOST_VARIANTS {
        return Err(format!("{} variants are more than {MOST_VARIANTS}: fewer options, or pairs or greedy stacking", sets.len()));
    }
    Ok(sets.into_iter().map(Variant::of).collect())
}

/// Greedy stacking's next round: the best so far with each option it hasn't, in the options' order.
pub fn greedy_next(options: &[String], best: &[String]) -> Vec<Variant> {
    options.iter().filter(|o| !best.contains(o)).map(|o| Variant::of(options.iter().filter(|x| best.contains(x) || *x == o).cloned().collect())).collect()
}

/// What a criterion measures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Kind {
    /// The workflow's run succeeds on the variant.
    Pipeline { workflow: String },
    /// The command exits 0 in a checkout of the variant.
    Command {
        command: String,
        #[serde(default, rename = "runs-on", alias = "runs_on")]
        runs_on: Option<String>,
    },
    /// A metric the variant's runs emit (`$KILN_METRICS`), lower or higher being better.
    Metric {
        metric: String,
        #[serde(default)]
        better: Better,
    },
}

/// A threshold a metric meets.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Threshold {
    /// Its median at most this.
    AtMost(f64),
    /// Its median at least this.
    AtLeast(f64),
    /// Better than the baseline by at least this fraction (`0.05`: 5 %), all across its interval.
    Better(f64),
}

/// A criterion: what it measures, whether it's required (every repetition must pass) or an
/// objective, and its threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub name: String,
    #[serde(flatten)]
    pub kind: Kind,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<Threshold>,
}

/// One repetition of one variant: whether each pass/fail criterion passed (pipelines and commands)
/// and the metrics its runs emitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub variant: String,
    pub round: usize,
    pub passed: BTreeMap<String, bool>,
    pub metrics: BTreeMap<String, f64>,
}

/// A metric of a variant: its median, against the baseline's (the ratio's interval), and its
/// criterion's verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricResult {
    pub name: String,
    pub better: Better,
    pub median: Option<f64>,
    pub samples: usize,
    /// `median / the baseline's median`: its point estimate and 95 % interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vs_baseline: Option<Ratio>,
    /// `improved`, `regressed`, `no clear difference`; or none, for the baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Whether it meets its criterion's threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meets: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Ratio {
    pub low: f64,
    pub mid: f64,
    pub high: f64,
}

impl From<Interval> for Ratio {
    fn from(i: Interval) -> Self {
        Ratio { low: i.low, mid: i.mid, high: i.high }
    }
}

/// A variant's results.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VariantResult {
    pub name: String,
    pub options: Vec<String>,
    pub repetitions: usize,
    /// Each required pass/fail criterion: its passes of its repetitions.
    pub checks: BTreeMap<String, (usize, usize)>,
    /// Whether it passes every required criterion, each repetition.
    pub passes: bool,
    pub metrics: Vec<MetricResult>,
}

/// An option's main effect on a metric: how much better a variant is with it than without, as a
/// gain (`0.1`: 10 % better).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effect {
    pub option: String,
    pub metric: String,
    pub gain: f64,
}

/// Two options' interaction on a metric: their variant's gain less the sum of each one's.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Interaction {
    pub a: String,
    pub b: String,
    pub metric: String,
    pub interaction: f64,
    /// `synergistic` (better together than apart), `antagonistic` (they cancel out), `additive`.
    pub kind: String,
}

/// What an experiment's measurements say.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Results {
    pub variants: Vec<VariantResult>,
    pub effects: Vec<Effect>,
    pub interactions: Vec<Interaction>,
    /// The variant recommended, when one is clearly best; and why, or why none is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    pub why: String,
}

/// An interaction smaller than this (as a gain) is additive.
const ADDITIVE: f64 = 0.01;
/// Gains closer than this are a tie: fewer options win it.
const TIE: f64 = 0.005;
/// Bootstrap rounds.
const ROUNDS: usize = 2000;

/// A ratio as a gain: how much better than the baseline (`0.1`: 10 % better), lower or higher
/// being better.
fn gain(ratio: f64, better: Better) -> f64 {
    match better {
        Better::Lower => 1.0 - ratio,
        Better::Higher => ratio - 1.0,
    }
}

/// What `measurements` of `variants` say against `criteria`; the first objective metric (or the
/// first metric) is the primary one the recommendation goes by. `seed` makes the intervals
/// reproducible.
pub fn analyze(variants: &[Variant], criteria: &[Criterion], measurements: &[Measurement], seed: u64) -> Results {
    let metrics: Vec<(&Criterion, &str, Better)> = criteria
        .iter()
        .filter_map(|c| match &c.kind {
            Kind::Metric { metric, better } => Some((c, metric.as_str(), *better)),
            _ => None,
        })
        .collect();
    let values = |variant: &str, metric: &str| -> Vec<f64> { measurements.iter().filter(|m| m.variant == variant).filter_map(|m| m.metrics.get(metric).copied()).collect() };
    let baseline = variants.iter().find(|v| v.options.is_empty()).map(|v| v.name.clone());
    let mut out: Vec<VariantResult> = Vec::new();
    for v in variants {
        let mine: Vec<&Measurement> = measurements.iter().filter(|m| m.variant == v.name).collect();
        let mut checks: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for c in criteria.iter().filter(|c| c.required && !matches!(c.kind, Kind::Metric { .. })) {
            let tried: Vec<bool> = mine.iter().filter_map(|m| m.passed.get(&c.name).copied()).collect();
            checks.insert(c.name.clone(), (tried.iter().filter(|p| **p).count(), tried.len()));
        }
        let mut results = Vec::new();
        for (c, metric, better) in &metrics {
            let mut mine_values = values(&v.name, metric);
            let median = stats::median(&mut mine_values);
            let vs = baseline.as_ref().filter(|b| **b != v.name).and_then(|b| stats::ratio_interval(&values(b, metric), &values(&v.name, metric), ROUNDS, seed));
            // Clearly better or worse: the whole interval on one side.
            let verdict = vs.map(|i| match stats::judge(i, 1e-9, *better) {
                Verdict::Improved { .. } => "improved".to_owned(),
                Verdict::Regressed { .. } => "regressed".to_owned(),
                Verdict::NoClearDifference => "no clear difference".to_owned(),
            });
            let meets = match c.threshold {
                Some(Threshold::AtMost(x)) => median.map(|m| m <= x),
                Some(Threshold::AtLeast(x)) => median.map(|m| m >= x),
                Some(Threshold::Better(f)) => vs.map(|i| matches!(stats::judge(i, f, *better), Verdict::Improved { .. })),
                None => None,
            };
            results.push(MetricResult { name: (*metric).to_owned(), better: *better, median, samples: values(&v.name, metric).len(), vs_baseline: vs.map(Ratio::from), verdict, meets });
        }
        let checks_pass = checks.values().all(|(passed, tried)| *tried > 0 && passed == tried);
        // A required metric criterion is met by its threshold (the baseline's relative one, by
        // definition, isn't asked of it).
        let metrics_pass = metrics.iter().zip(&results).filter(|((c, _, _), _)| c.required).all(|((c, _, _), r)| match c.threshold {
            Some(Threshold::Better(_)) if Some(&v.name) == baseline.as_ref() => true,
            Some(_) => r.meets == Some(true),
            None => r.samples > 0,
        });
        out.push(VariantResult { name: v.name.clone(), options: v.options.clone(), repetitions: mine.len(), checks, passes: checks_pass && metrics_pass, metrics: results });
    }
    // Main effects and interactions, on each metric, as gains against the baseline.
    let gain_of = |r: &VariantResult, metric: &str| -> Option<f64> { r.metrics.iter().find(|m| m.name == metric).and_then(|m| Some(gain(m.vs_baseline?.mid, m.better))) };
    let mut effects = Vec::new();
    let mut interactions = Vec::new();
    let options: Vec<String> = {
        let mut o: Vec<String> = Vec::new();
        for v in variants {
            for x in &v.options {
                if !o.contains(x) {
                    o.push(x.clone());
                }
            }
        }
        o
    };
    for (_, metric, _) in &metrics {
        let gains: Vec<(&VariantResult, f64)> = out.iter().filter_map(|r| Some((r, if r.options.is_empty() { 0.0 } else { gain_of(r, metric)? }))).collect();
        for o in &options {
            // The mean gain of the variants with it, less that of the variants without it (the
            // baseline's 0 among them): a single option's gain when only singles are measured.
            let with: Vec<f64> = gains.iter().filter(|(r, _)| r.options.contains(o)).map(|(_, g)| *g).collect();
            let without: Vec<f64> = gains.iter().filter(|(r, _)| !r.options.contains(o)).map(|(_, g)| *g).collect();
            if with.is_empty() || without.is_empty() {
                continue;
            }
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            effects.push(Effect { option: o.clone(), metric: (*metric).to_owned(), gain: mean(&with) - mean(&without) });
        }
        let single = |o: &str| gains.iter().find(|(r, _)| r.options.len() == 1 && r.options.first().is_some_and(|x| x == o)).map(|(_, g)| *g);
        for (r, g) in gains.iter().filter(|(r, _)| r.options.len() == 2) {
            let (Some(a), Some(b)) = (r.options.first(), r.options.get(1)) else { continue };
            let (Some(ga), Some(gb)) = (single(a), single(b)) else { continue };
            let i = g - ga - gb;
            let kind = if i > ADDITIVE {
                "synergistic"
            } else if i < -ADDITIVE {
                "antagonistic"
            } else {
                "additive"
            };
            interactions.push(Interaction { a: a.clone(), b: b.clone(), metric: (*metric).to_owned(), interaction: i, kind: kind.to_owned() });
        }
    }
    // The recommendation: the best primary metric among the variants that pass, clearly better
    // than the baseline; a tie goes to fewer options.
    let primary = metrics.iter().find(|(c, _, _)| !c.required).or_else(|| metrics.first()).map(|(_, m, _)| (*m).to_owned());
    let (recommendation, why) = match &primary {
        None => (None, "no metric to go by: the variants that pass every required criterion are listed".to_owned()),
        Some(metric) => {
            let mut clear: Vec<(&VariantResult, f64)> = out
                .iter()
                .filter(|r| !r.options.is_empty() && r.passes)
                .filter(|r| r.metrics.iter().any(|m| m.name == *metric && m.verdict.as_deref() == Some("improved")))
                .filter_map(|r| Some((r, gain_of(r, metric)?)))
                .collect();
            clear.sort_by(|(a, ga), (b, gb)| gb.total_cmp(ga).then_with(|| a.options.len().cmp(&b.options.len())));
            let best = clear.first().map(|(r, g)| (*r, *g));
            match best {
                None => (None, format!("no variant that passes every required criterion is clearly better than the baseline on {metric}")),
                Some((top, g)) => {
                    // Within a tie of the best, the fewest options.
                    let pick = clear.iter().filter(|(_, x)| g - x < TIE).min_by_key(|(r, _)| r.options.len()).map_or(top, |(r, _)| *r);
                    let pg = gain_of(pick, metric).unwrap_or(g);
                    (Some(pick.name.clone()), format!("{} is {:.1}% better than the baseline on {metric}, and passes every required criterion", pick.name, pg * 100.0))
                }
            }
        }
    };
    Results { variants: out, effects, interactions, recommendation, why }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[Variant]) -> Vec<&str> {
        v.iter().map(|x| x.name.as_str()).collect()
    }

    fn opts(o: &[&str]) -> Vec<String> {
        o.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn w20_r11_strategies_make_their_variants() -> Result<(), String> {
        let o = opts(&["a", "b", "c"]);
        assert_eq!(names(&variants(&o, &Strategy::Each, true)?), ["baseline", "a", "b", "c"]);
        assert_eq!(names(&variants(&o, &Strategy::All, true)?), ["baseline", "a", "b", "c", "a+b", "a+c", "b+c", "a+b+c"], "2ⁿ − 1, and the baseline");
        assert_eq!(names(&variants(&o, &Strategy::Pairs, false)?), ["a", "b", "c", "a+b", "a+c", "b+c"]);
        assert_eq!(names(&variants(&o, &Strategy::Greedy, true)?), ["baseline", "a", "b", "c"], "greedy's first round");
        assert_eq!(names(&greedy_next(&o, &opts(&["b"]))), ["a+b", "b+c"], "the best so far with each of the rest");
        assert_eq!(names(&variants(&o, &Strategy::Custom(vec![opts(&["c", "a"])]), true)?), ["baseline", "a+c"], "in the options' order");
        assert!(variants(&opts(&["a", "a"]), &Strategy::Each, true).is_err());
        assert!(variants(&opts(&["x1", "x2", "x3", "x4", "x5", "x6"]), &Strategy::All, true).is_err(), "63 variants");
        assert!(variants(&o, &Strategy::Custom(vec![opts(&["d"])]), true).is_err());
        Ok(())
    }

    /// Noise around `center`, three repetitions, from a seed.
    fn three(center: f64, seed: u64) -> [f64; 3] {
        let n = |k: u64| center * (1.0 + 0.004 * (((seed * 7 + k * 13) % 5) as f64 - 2.0));
        [n(0), n(1), n(2)]
    }

    /// W20-R11 and AC06's scenario: three options, every combination, a time to lower and the
    /// tests required. `a` takes 20 % off, `b` 10 %, and together 40 % (synergistic); `c` halves
    /// the time but breaks the tests with `b`. The recommendation is `a+c` (60 % better); `a+b+c`
    /// is better still but fails the tests.
    #[test]
    fn w20_r11_measurements_say_what_helps_and_what_fights() {
        let o = opts(&["a", "b", "c"]);
        let vs = variants(&o, &Strategy::All, true).unwrap_or_default();
        let criteria = vec![
            Criterion { name: "tests".into(), kind: Kind::Pipeline { workflow: ".kiln/ci.yaml".into() }, required: true, threshold: None },
            Criterion { name: "time".into(), kind: Kind::Metric { metric: "p99_ms".into(), better: Better::Lower }, required: false, threshold: Some(Threshold::Better(0.05)) },
        ];
        let time = |v: &Variant| -> f64 {
            let (a, b, c) = (v.options.contains(&"a".to_owned()), v.options.contains(&"b".to_owned()), v.options.contains(&"c".to_owned()));
            let base = match (a, b) {
                (true, true) => 60.0,
                (true, false) => 80.0,
                (false, true) => 90.0,
                (false, false) => 100.0,
            };
            if c { base * 0.5 } else { base }
        };
        let mut ms = Vec::new();
        for (k, v) in vs.iter().enumerate() {
            for (round, t) in three(time(v), k as u64).into_iter().enumerate() {
                let broken = v.options.contains(&"b".to_owned()) && v.options.contains(&"c".to_owned());
                ms.push(Measurement { variant: v.name.clone(), round, passed: [("tests".to_owned(), !broken)].into(), metrics: [("p99_ms".to_owned(), t)].into() });
            }
        }
        let r = analyze(&vs, &criteria, &ms, 7);
        assert_eq!(r.recommendation.as_deref(), Some("a+c"), "{r:#?}");
        let ab = r.variants.iter().find(|v| v.name == "a+b").cloned();
        let ratio = ab.as_ref().and_then(|v| v.metrics.first()).and_then(|m| m.vs_baseline);
        assert!(ratio.is_some_and(|i| (i.mid - 0.6).abs() < 0.01 && i.low <= i.mid && i.mid <= i.high && i.high - i.low < 0.05), "{ratio:?}");
        assert!(r.variants.iter().find(|v| v.name == "b+c").is_some_and(|v| !v.passes && v.checks.get("tests") == Some(&(0, 3))), "b+c fails the tests");
        let ab_i = r.interactions.iter().find(|i| i.a == "a" && i.b == "b").cloned();
        assert!(ab_i.as_ref().is_some_and(|i| i.kind == "synergistic" && (i.interaction - 0.1).abs() < 0.02), "{ab_i:?}");
        assert!(r.effects.iter().any(|e| e.option == "a" && e.gain > 0.1), "{:?}", r.effects);
    }
}
