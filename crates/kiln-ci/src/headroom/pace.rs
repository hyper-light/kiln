//! How fast running work may go (docs/headroom.md, "Pacing"): when what kiln's running work wants
//! to write is more than the budget, the budget is shared out class by class, highest first, and
//! within a class max-min fairly (water-filling): every group gets what it wants up to one level,
//! the level that spends what's left. A group's share of time is what it gets over what it wants.
//! A person's work and a protected target are never slowed, and work that doesn't write is never
//! touched: only the resource that's short is rationed.

use serde::{Deserialize, Serialize};

use super::admit::Class;

/// A running group of work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub id: String,
    pub class: Class,
    /// What it writes a second when not slowed.
    pub demand_bps: f64,
    /// A protected target's own processes (never slowed).
    pub protected: bool,
}

/// A group's share of time from now (1: full speed), and why.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pace {
    pub id: String,
    pub share: f64,
    pub why: String,
}

fn mb(b: f64) -> String {
    format!("{:.0} MB", b / 1e6)
}

/// The level that, given to every demand up to it, spends `left` (max-min fairness).
fn level(demands: &[f64], left: f64) -> f64 {
    let mut d: Vec<f64> = demands.iter().copied().filter(|d| *d > 0.0).collect();
    d.sort_by(f64::total_cmp);
    let mut left = left.max(0.0);
    let mut n = d.len() as f64;
    for x in &d {
        if x * n <= left {
            left -= x;
            n -= 1.0;
        } else {
            return left / n;
        }
    }
    f64::INFINITY
}

/// Each running group's share under `budget` (none: no limit).
pub fn plan(budget: Option<f64>, groups: &[Running]) -> Vec<Pace> {
    let wanted: f64 = groups.iter().map(|g| g.demand_bps.max(0.0)).sum();
    let Some(budget) = budget.filter(|b| wanted > *b) else {
        let why = if budget.is_some() { "all of kiln's work fits the write budget" } else { "no write budget: nothing has been in trouble" };
        return groups.iter().map(|g| Pace { id: g.id.clone(), share: 1.0, why: why.into() }).collect();
    };
    let exempt = |g: &Running| g.protected || g.class == Class::Person || g.demand_bps <= 0.0;
    let mut left = budget - groups.iter().filter(|g| exempt(g)).map(|g| g.demand_bps.max(0.0)).sum::<f64>();
    let mut levels: Vec<(Class, f64)> = Vec::new();
    let mut classes: Vec<Class> = groups.iter().filter(|g| !exempt(g)).map(|g| g.class).collect();
    classes.sort();
    classes.dedup();
    for c in classes.into_iter().rev() {
        let demands: Vec<f64> = groups.iter().filter(|g| !exempt(g) && g.class == c).map(|g| g.demand_bps).collect();
        let l = level(&demands, left);
        left -= demands.iter().map(|d| d.min(l)).sum::<f64>();
        levels.push((c, l));
    }
    groups
        .iter()
        .map(|g| {
            if g.protected {
                return Pace { id: g.id.clone(), share: 1.0, why: "a protected target's: never slowed".into() };
            }
            if g.class == Class::Person {
                return Pace { id: g.id.clone(), share: 1.0, why: "a person's: never slowed".into() };
            }
            if g.demand_bps <= 0.0 {
                return Pace { id: g.id.clone(), share: 1.0, why: "writes nothing: the disk is what's short".into() };
            }
            let l = levels.iter().find(|(c, _)| *c == g.class).map_or(0.0, |(_, l)| *l);
            let got = g.demand_bps.min(l);
            let share = (got / g.demand_bps).clamp(0.0, 1.0);
            let why = if share >= 1.0 {
                format!("writes {}/s, within its class's fair level", mb(g.demand_bps))
            } else if share <= 0.0 {
                format!("stopped: higher classes take all {}/s of the write budget", mb(budget))
            } else {
                format!("slowed to {:.0}%: wants {}/s, its fair part of the {}/s budget is {}/s", share * 100.0, mb(g.demand_bps), mb(budget), mb(got))
            };
            Pace { id: g.id.clone(), share, why }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn g(id: &str, class: Class, d: f64) -> Running {
        Running { id: id.into(), class, demand_bps: d, protected: false }
    }

    #[test]
    fn the_lowest_class_is_slowed_first_and_people_and_readers_never() {
        let groups = [g("person", Class::Person, 500e6), g("a1", Class::Agent, 100e6), g("a2", Class::Agent, 400e6), g("bench", Class::Background, 300e6), g("reader", Class::Agent, 0.0)];
        let p = plan(Some(800e6), &groups);
        let share = |id: &str| p.iter().find(|x| x.id == id).map_or(-1.0, |x| x.share);
        assert_eq!(share("person"), 1.0);
        assert_eq!(share("reader"), 1.0);
        // 300 MB/s left after the person: agents 100 + 200 of 400, the bench stopped.
        assert_eq!(share("a1"), 1.0);
        assert!((share("a2") - 0.5).abs() < 1e-9);
        assert_eq!(share("bench"), 0.0);
    }

    #[test]
    fn a_budget_of_zero_stops_every_writer_and_no_budget_slows_nothing() {
        let groups = [g("a", Class::Agent, 1e6), g("b", Class::Background, 1e6)];
        assert!(plan(Some(0.0), &groups).iter().all(|p| p.share == 0.0));
        assert!(plan(None, &groups).iter().all(|p| p.share == 1.0));
    }

    proptest! {
        #[test]
        fn shares_spend_at_most_the_budget_fairly_within_a_class_and_higher_classes_first(
            budget in 0f64..2e9,
            groups in prop::collection::vec((0u8..3, 0f64..1e9, any::<bool>()), 1..16)
        ) {
            let classes = [Class::Background, Class::Agent, Class::Person];
            let gs: Vec<Running> = groups.iter().enumerate().map(|(i, (c, d, p))| Running {
                id: format!("g{i}"), class: classes.get(usize::from(*c)).copied().unwrap_or(Class::Agent), demand_bps: *d, protected: *p,
            }).collect();
            let p = plan(Some(budget), &gs);
            let got = |r: &Running| p.iter().find(|x| x.id == r.id).map_or(0.0, |x| x.share * r.demand_bps);
            let exempt = |r: &Running| r.protected || r.class == Class::Person || r.demand_bps <= 0.0;
            let exempt_sum: f64 = gs.iter().filter(|r| exempt(r)).map(|r| r.demand_bps).sum();
            let paced_sum: f64 = gs.iter().filter(|r| !exempt(r)).map(&got).sum();
            let total: f64 = gs.iter().map(|r| r.demand_bps).sum();
            if total > budget {
                prop_assert!(paced_sum <= (budget - exempt_sum).max(0.0) + 1e-3);
            }
            for r in gs.iter().filter(|r| exempt(r)) {
                prop_assert_eq!(p.iter().find(|x| x.id == r.id).map(|x| x.share), Some(1.0));
            }
            // Within a class, one held back gets at least what any other in it gets.
            for a in gs.iter().filter(|r| !exempt(r)) {
                if got(a) + 1e-3 < a.demand_bps {
                    for b in gs.iter().filter(|r| !exempt(r) && r.class == a.class) {
                        prop_assert!(got(a) + 1e-3 >= got(b), "{} {} vs {} {}", a.id, got(a), b.id, got(b));
                    }
                    // A lower class gets something only when this one is whole.
                    for b in gs.iter().filter(|r| !exempt(r) && r.class < a.class) {
                        prop_assert!(got(b) <= 1e-3);
                    }
                }
            }
        }
    }
}
