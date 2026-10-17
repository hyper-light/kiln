//! What makes a plan's step done, and how it's tested: criteria, each in words or a measure (a
//! target, how it compares, a threshold), and tests (a scenario, and the criteria it passes by).
//! Every criterion reads as one sentence an agent takes as written ("p99.9 latency is at most
//! 200 ms, measured by the soak test"); that's what goes to the agent, in a step's message and in
//! its /goal.

use serde::{Deserialize, Deserializer, Serialize};

/// One criterion: in words, or a measure against a threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Criterion {
    /// In words, as written.
    Qualitative { text: String },
    /// A measure: `target` (what's measured, "p99.9 latency") compared by `op` with `value` (and
    /// `value2`: the upper bound of `between`, the tolerance of `within`), in `unit`; the
    /// rises/falls comparisons are percentages against `baseline`. `measured`: how it's measured.
    Quantitative {
        target: String,
        op: Op,
        value: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        value2: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        unit: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        baseline: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        measured: String,
    },
}

/// How a measure compares with its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
    /// From `value` to `value2`, both included.
    Between,
    /// `value`, give or take `value2`.
    Within,
    /// Percentages against the baseline.
    RisesAtLeast,
    FallsAtLeast,
    RisesAtMost,
    FallsAtMost,
}

impl Op {
    fn relative(self) -> bool {
        matches!(self, Self::RisesAtLeast | Self::FallsAtLeast | Self::RisesAtMost | Self::FallsAtMost)
    }
}

/// A test: a scenario, and what it passes by.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Test {
    pub scenario: String,
    pub criteria: Vec<Criterion>,
}

/// A number and its unit, as a sentence has them: "200 ms", "99.9%".
fn amount(value: &str, unit: &str) -> String {
    let (v, u) = (value.trim(), unit.trim());
    if u.is_empty() {
        v.to_owned()
    } else if u.starts_with('%') || u.starts_with('°') || u.starts_with('x') || u.starts_with('×') {
        format!("{v}{u}")
    } else {
        format!("{v} {u}")
    }
}

impl Criterion {
    /// The criterion as one sentence an agent reads.
    pub fn sentence(&self) -> String {
        match self {
            Self::Qualitative { text } => text.split_whitespace().collect::<Vec<_>>().join(" "),
            Self::Quantitative { target, op, value, value2, unit, baseline, measured } => {
                let t = target.trim();
                let a = amount(value, unit);
                let base = if baseline.trim().is_empty() { "the baseline" } else { baseline.trim() };
                let pct = format!("{}%", value.trim());
                let s = match op {
                    Op::Lt => format!("{t} is less than {a}"),
                    Op::Le => format!("{t} is at most {a}"),
                    Op::Eq => format!("{t} is exactly {a}"),
                    Op::Ne => format!("{t} is not {a}"),
                    Op::Ge => format!("{t} is at least {a}"),
                    Op::Gt => format!("{t} is more than {a}"),
                    Op::Between => format!("{t} is between {a} and {}", amount(value2, unit)),
                    Op::Within => format!("{t} is within {} of {a}", amount(value2, unit)),
                    Op::RisesAtLeast => format!("{t} rises by at least {pct} compared with {base}"),
                    Op::FallsAtLeast => format!("{t} falls by at least {pct} compared with {base}"),
                    Op::RisesAtMost => format!("{t} rises by no more than {pct} compared with {base}"),
                    Op::FallsAtMost => format!("{t} falls by no more than {pct} compared with {base}"),
                };
                if measured.trim().is_empty() { s } else { format!("{s}, measured by {}", measured.split_whitespace().collect::<Vec<_>>().join(" ")) }
            }
        }
    }

    /// What's wrong with it, if anything.
    pub fn problem(&self) -> Option<String> {
        let number = |v: &str| v.trim().parse::<f64>().is_ok_and(f64::is_finite);
        match self {
            Self::Qualitative { text } if text.trim().is_empty() => Some("a criterion in words needs its words".into()),
            Self::Qualitative { .. } => None,
            Self::Quantitative { target, op, value, value2, .. } => {
                if target.trim().is_empty() {
                    return Some("a measure needs what it measures (its target)".into());
                }
                if !number(value) {
                    return Some(format!("{}'s threshold is a number", target.trim()));
                }
                if matches!(op, Op::Between | Op::Within) && !number(value2) {
                    return Some(format!("{}'s {} is a number", target.trim(), if *op == Op::Between { "upper bound" } else { "tolerance" }));
                }
                if op.relative() && value.trim().parse::<f64>().is_ok_and(|v| v < 0.0) {
                    return Some(format!("{}'s percentage isn't negative: pick rises or falls", target.trim()));
                }
                None
            }
        }
    }
}

impl Test {
    /// The test as one sentence: its scenario, then what it passes by.
    pub fn sentence(&self) -> String {
        let scenario = self.scenario.split_whitespace().collect::<Vec<_>>().join(" ");
        let pass = self.criteria.iter().map(Criterion::sentence).collect::<Vec<_>>().join("; ");
        match (scenario.is_empty(), pass.is_empty()) {
            (false, false) => format!("{scenario} — passes when {pass}"),
            (false, true) => scenario,
            (true, _) => pass,
        }
    }
}

/// Criteria as given: a list, or (as plans kept them before) words, one criterion.
pub fn criteria_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Criterion>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Given {
        Words(String),
        List(Vec<Criterion>),
    }
    Ok(match Option::<Given>::deserialize(d)? {
        Some(Given::List(l)) => l,
        Some(Given::Words(w)) if !w.trim().is_empty() => vec![Criterion::Qualitative { text: w }],
        _ => Vec::new(),
    })
}

/// Tests as given: a list, or (as plans kept them before) words, one test.
pub fn test_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Test>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Given {
        Words(String),
        List(Vec<Test>),
    }
    Ok(match Option::<Given>::deserialize(d)? {
        Some(Given::List(l)) => l,
        Some(Given::Words(w)) if !w.trim().is_empty() => vec![Test { scenario: w, criteria: Vec::new() }],
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(target: &str, op: Op, value: &str, unit: &str) -> Criterion {
        Criterion::Quantitative { target: target.into(), op, value: value.into(), value2: String::new(), unit: unit.into(), baseline: String::new(), measured: String::new() }
    }

    #[test]
    fn measures_read_as_sentences() {
        assert_eq!(measure("p99.9 latency", Op::Le, "200", "ms").sentence(), "p99.9 latency is at most 200 ms");
        assert_eq!(measure("availability", Op::Ge, "99.95", "%").sentence(), "availability is at least 99.95%");
        assert_eq!(measure("error count", Op::Eq, "0", "").sentence(), "error count is exactly 0");
        let between = Criterion::Quantitative { target: "heap".into(), op: Op::Between, value: "64".into(), value2: "128".into(), unit: "MB".into(), baseline: String::new(), measured: "the soak test".into() };
        assert_eq!(between.sentence(), "heap is between 64 MB and 128 MB, measured by the soak test");
        let within = Criterion::Quantitative { target: "clock skew".into(), op: Op::Within, value: "0".into(), value2: "5".into(), unit: "ms".into(), baseline: String::new(), measured: String::new() };
        assert_eq!(within.sentence(), "clock skew is within 5 ms of 0 ms");
        let rises = Criterion::Quantitative { target: "throughput".into(), op: Op::RisesAtLeast, value: "20".into(), value2: String::new(), unit: String::new(), baseline: "main".into(), measured: String::new() };
        assert_eq!(rises.sentence(), "throughput rises by at least 20% compared with main");
        let budget = Criterion::Quantitative { target: "p50 latency".into(), op: Op::RisesAtMost, value: "5".into(), value2: String::new(), unit: String::new(), baseline: String::new(), measured: String::new() };
        assert_eq!(budget.sentence(), "p50 latency rises by no more than 5% compared with the baseline");
        assert_eq!(Criterion::Qualitative { text: "  the docs\nsay so ".into() }.sentence(), "the docs say so");
    }

    #[test]
    fn a_measure_needs_its_numbers() {
        assert!(measure("x", Op::Le, "200", "ms").problem().is_none());
        assert!(measure("", Op::Le, "200", "ms").problem().is_some());
        assert!(measure("x", Op::Le, "fast", "").problem().is_some());
        assert!(measure("x", Op::Between, "1", "").problem().is_some());
        assert!(measure("x", Op::RisesAtLeast, "-3", "").problem().is_some());
        assert!(Criterion::Qualitative { text: " ".into() }.problem().is_some());
    }

    #[test]
    fn tests_read_as_their_scenario_and_what_they_pass_by() {
        let t = Test { scenario: "1 GB upload\nover a 10 Mbps link".into(), criteria: vec![measure("time", Op::Lt, "15", "min"), Criterion::Qualitative { text: "the checksum matches".into() }] };
        assert_eq!(t.sentence(), "1 GB upload over a 10 Mbps link — passes when time is less than 15 min; the checksum matches");
    }

    #[test]
    fn words_kept_before_read_as_one_criterion_or_test() {
        #[derive(Deserialize)]
        struct S {
            #[serde(default, deserialize_with = "criteria_list")]
            a: Vec<Criterion>,
            #[serde(default, deserialize_with = "test_list")]
            t: Vec<Test>,
        }
        let s: S = serde_json::from_str(r#"{"a": "tests pass", "t": "hex cases"}"#).unwrap_or(S { a: vec![], t: vec![] });
        assert_eq!(s.a, vec![Criterion::Qualitative { text: "tests pass".into() }]);
        assert_eq!(s.t.first().map(|t| t.scenario.as_str()), Some("hex cases"));
        let s: S = serde_json::from_str(r#"{"a": [{"kind": "quantitative", "target": "x", "op": "ge", "value": "1"}], "t": []}"#).unwrap_or(S { a: vec![], t: vec![] });
        assert_eq!(s.a, vec![measure("x", Op::Ge, "1", "")]);
        let s: S = serde_json::from_str(r#"{"a": "", "t": null}"#).unwrap_or(S { a: vec![measure("x", Op::Ge, "1", "")], t: vec![] });
        assert!(s.a.is_empty() && s.t.is_empty());
    }
}
