//! What the asker of a run is told at once, and as it changes (docs/reach.md §2, docs/needs.md
//! §13): started, queued or never, each with what the goal names: where, what it got and why;
//! reasons counted across runners, its place, an expected start as a range and the nearest ask
//! that would start now; the need no runner meets and the nearest that exists. Times are the
//! owner's clock's: the owner corrects each runner's by its link's measured offset.

use serde::{Deserialize, Serialize};

use super::msg::{Dim, Estimate};
use crate::sched::needs;
use crate::sched::state::{Cpus, Resources};

/// A run's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "kebab-case")]
pub enum Answer {
    /// `runner` holds room for it (`got`), and its tree is being copied there: it hasn't started.
    Starting { runner: String, got: Resources },
    /// It started on `runner`, by that runner's word, with `got` held for it there, because that
    /// runner had room for it first (`why`: what was free there).
    Started { runner: String, got: Resources, why: String },
    /// It waits: `of` runners known, how many said each reason (the counts sum to `of`), its best
    /// place (and where), when it's expected to start (a range), and the nearest ask that would
    /// start now, where one would.
    Queued { of: u32, reasons: Vec<Count>, place: Option<At>, start: Estimate, nearest: Option<Nearest> },
    /// No runner could run it as asked, even idle: the need none meets, and the runner nearest to
    /// it (what it is or has).
    Never { need: String, nearest: Option<String> },
    /// It ended on `runner`: its process exited (`ok`), or it couldn't start or was lost there.
    Ended { runner: String, ok: bool },
    /// It can't be followed any more: its runner is gone, or started again under a new epoch, as
    /// it started or ran there (it may have run).
    Lost { runner: String, why: String },
}

/// How many runners gave one reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub reason: String,
    pub runners: u32,
}

/// A run's place on a runner: from `lo` to `hi` in its line (1 first), to within a power of two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct At {
    pub lo: u32,
    pub hi: u32,
    pub runner: String,
}

/// An ask nearest to the one made that would start now on `runner`: what was asked, less in each
/// dimension that runner is short of, down to what it has free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nearest {
    pub ask: Resources,
    pub runner: String,
}

impl Answer {
    /// In words, one line, as `kiln run` prints it and an agent reads it: `queued: 0 of 4 can start
    /// it now (2 too few idle CPUs, 2 not linux) · 1st at kiln-a094a12d · expected in 3 to 9 min ·
    /// cpus=6 would start now on kiln-a094a12d`. `now`: the owner's time the estimate is told from.
    pub fn words(&self, now: i64) -> String {
        match self {
            Answer::Starting { runner, got } => {
                let got = needs::request_words(got);
                if got.is_empty() { format!("starting on {runner}") } else { format!("starting on {runner}: {} held for it", got.join(", ")) }
            }
            Answer::Started { runner, got, why } => {
                let got = needs::request_words(got);
                if got.is_empty() { format!("started on {runner} ({why})") } else { format!("started on {runner}: {} held for it ({why})", got.join(", ")) }
            }
            // Held back for its tenant's budget before any runner was asked (LB08,
            // `super::budget`): its one reason, counted at no runner.
            Answer::Queued { of: 0, reasons, place: None, start, nearest: None } if reasons.len() == 1 && reasons.iter().all(|c| c.runners == 0) => {
                let reason = reasons.first().map(|c| c.reason.as_str()).unwrap_or_default();
                match (start.soonest, start.latest) {
                    (None, None) => format!("held: {reason}"),
                    _ => format!("held: {reason} · {}", expected(start, now)),
                }
            }
            Answer::Queued { of, reasons, place, start, nearest } => {
                let mut parts = vec![format!("queued: 0 of {of} can start it now ({})", reasons.iter().map(|c| format!("{} {}", c.runners, c.reason)).collect::<Vec<_>>().join(", "))];
                if let Some(p) = place {
                    let at = if p.lo == p.hi { ordinal(p.lo) } else { format!("{} to {}", ordinal(p.lo), ordinal(p.hi)) };
                    parts.push(format!("{at} at {}", p.runner));
                }
                parts.push(expected(start, now));
                if let Some(n) = nearest {
                    parts.push(format!("{} would start now on {}", spelling(&n.ask), n.runner));
                }
                parts.join(" · ")
            }
            Answer::Never { need, nearest } => match nearest {
                Some(n) => format!("never: no runner {need} (the nearest: {n})"),
                None => format!("never: no runner {need}"),
            },
            Answer::Ended { runner, ok } => {
                if *ok {
                    format!("ended on {runner}")
                } else {
                    format!("ended on {runner}: it couldn't start, or was lost there")
                }
            }
            Answer::Lost { runner, why } => format!("lost: {runner} {why}"),
        }
    }
}

/// `1st`, `2nd`, `3rd`, `4th`, `11th`, `21st`.
pub fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// An estimate in words, from `now`, in whole minutes rounded up: `expected in 3 to 9 min`,
/// `expected in 3 min or later`, `no expected start yet` (nothing bounds it).
pub fn expected(e: &Estimate, now: i64) -> String {
    let mins = |at: i64| at.saturating_sub(now).max(0).saturating_add(59_999) / 60_000;
    match (e.soonest, e.latest) {
        (Some(s), Some(l)) if mins(s) == mins(l) => format!("expected in {} min", mins(s)),
        (Some(s), Some(l)) => format!("expected in {} to {} min", mins(s), mins(l)),
        (Some(s), None) => format!("expected in {} min or later", mins(s)),
        (None, Some(l)) => format!("expected within {} min", mins(l)),
        (None, None) => "no expected start yet (what runs or waits ahead of it has no prediction)".into(),
    }
}

/// An ask in the needs spelling: `cpus=6,mem=40g`.
pub fn spelling(r: &Resources) -> String {
    let mut w = Vec::new();
    if !r.cpu.is_zero() {
        w.push(format!("cpus={}", r.cpu));
    }
    if r.mem > 0 {
        w.push(format!("mem={}", size_word(r.mem)));
    }
    if r.disk > 0 {
        w.push(format!("disk={}", size_word(r.disk)));
    }
    if r.gpu > 0 {
        w.push(format!("gpus={}", r.gpu));
    }
    w.join(",")
}

/// The units sizes are said in, largest first: binary, as the needs spelling reads them.
const UNITS: [(u64, &str); 4] = [(1 << 40, "t"), (1 << 30, "g"), (1 << 20, "m"), (1 << 10, "k")];

/// A size as the needs spelling reads it, rounded down to what's whole (`40g`, `512m`).
fn size_word(n: u64) -> String {
    UNITS.iter().find(|(k, _)| n >= *k).map_or_else(|| n.to_string(), |(k, u)| format!("{}{u}", n / k))
}

/// The nearest ask to `ask` that fits `free`: each dimension it's short of brought down to what's
/// free (sizes rounded down to the unit they're said in, CPUs to whole ones), none if a dimension
/// it asks for has nothing free.
pub fn nearest(ask: &Resources, free: &Resources) -> Option<Resources> {
    let mut n = *ask;
    let mut changed = false;
    for d in Dim::ALL {
        let (a, f) = (d.of(ask), d.of(free));
        if a <= f {
            continue;
        }
        changed = true;
        match d {
            Dim::Cpu => n.cpu = Cpus::whole(f / 1000),
            Dim::Mem => n.mem = round_down(f),
            Dim::Disk => n.disk = round_down(f),
            Dim::Gpu => n.gpu = u32::try_from(f).unwrap_or(u32::MAX),
        }
        if d.of(&n) == 0 {
            return None;
        }
    }
    changed.then_some(n)
}

/// Down to the whole unit it's said in (`40g`, `512m`).
fn round_down(n: u64) -> u64 {
    UNITS.iter().find(|(k, _)| n >= *k).map_or(n, |(k, _)| n / k * k)
}

/// How much of `ask` `n` keeps, in millionths: the least over the dimensions asked for.
pub fn kept(ask: &Resources, n: &Resources) -> u64 {
    Dim::ALL.into_iter().filter(|d| d.of(ask) > 0).map(|d| u64::try_from(u128::from(d.of(n)).saturating_mul(1_000_000) / u128::from(d.of(ask).max(1))).unwrap_or(u64::MAX)).min().unwrap_or(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn e5_answers_in_words() {
        let q = Answer::Queued {
            of: 4,
            reasons: vec![Count { reason: "too few idle CPUs".into(), runners: 2 }, Count { reason: "not linux".into(), runners: 2 }],
            place: Some(At { lo: 1, hi: 1, runner: "kiln-a094a12d".into() }),
            start: Estimate { soonest: Some(3 * 60_000), latest: Some(9 * 60_000) },
            nearest: Some(Nearest { ask: Resources { cpu: Cpus::whole(6), mem: 40 * GIB, ..Resources::default() }, runner: "kiln-a094a12d".into() }),
        };
        assert_eq!(q.words(0), "queued: 0 of 4 can start it now (2 too few idle CPUs, 2 not linux) · 1st at kiln-a094a12d · expected in 3 to 9 min · cpus=6,mem=40g would start now on kiln-a094a12d");
        let s = Answer::Started { runner: "adacachy".into(), got: Resources { cpu: Cpus::whole(8), mem: 32 * GIB, ..Resources::default() }, why: "room for it here: 24 CPUs, 50 GiB free".into() };
        assert_eq!(s.words(0), "started on adacachy: 8 CPUs, 32 GiB of memory held for it (room for it here: 24 CPUs, 50 GiB free)");
        let n = Answer::Never { need: "is linux/arm64".into(), nearest: Some("adacachy, linux/x86_64".into()) };
        assert_eq!(n.words(0), "never: no runner is linux/arm64 (the nearest: adacachy, linux/x86_64)");
        assert_eq!([1, 2, 3, 4, 11, 12, 13, 21, 22, 101].map(ordinal), ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st"]);
        assert_eq!(expected(&Estimate { soonest: Some(30_000), latest: None }, 0), "expected in 1 min or later");
        let range = Answer::Queued { of: 1, reasons: vec![], place: Some(At { lo: 4, hi: 7, runner: "r".into() }), start: Estimate::default(), nearest: None };
        assert!(range.words(0).contains("4th to 7th at r"), "{}", range.words(0));
    }

    /// The nearest ask: each dimension short brought down to what's free, rounded down to its unit;
    /// none where a dimension asked for has nothing free.
    #[test]
    fn e5_the_nearest_ask_comes_down_to_what_is_free() {
        let ask = Resources { cpu: Cpus::whole(12), mem: 40 * GIB, ..Resources::default() };
        let free = Resources { cpu: Cpus::milli(6_500), mem: 64 * GIB, ..Resources::default() };
        assert_eq!(nearest(&ask, &free), Some(Resources { cpu: Cpus::whole(6), mem: 40 * GIB, ..Resources::default() }));
        assert_eq!(spelling(&Resources { cpu: Cpus::whole(6), mem: 40 * GIB, ..Resources::default() }), "cpus=6,mem=40g");
        assert_eq!(nearest(&ask, &Resources { cpu: Cpus::milli(500), mem: 64 * GIB, ..Resources::default() }), None, "no whole CPU free");
        assert_eq!(nearest(&ask, &Resources { cpu: Cpus::whole(16), mem: 64 * GIB, ..Resources::default() }), None, "it fits as asked");
        assert_eq!(kept(&ask, &Resources { cpu: Cpus::whole(6), mem: 40 * GIB, ..Resources::default() }), 500_000);
    }
}
