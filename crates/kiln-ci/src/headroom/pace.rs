//! How fast running work may go (docs/headroom.md, "Pacing"): when what kiln's running work wants
//! to write is more than the budget, the budget is shared out class by class, highest first, and
//! within a class max-min fairly (water-filling): every group gets what it wants up to one level,
//! the level that spends what's left. A group's share of time is what it gets over what it wants,
//! in each resource that has a budget (writes, CPU), the smaller of the two. A person's work and a
//! protected target are never slowed, and work that doesn't use a short resource isn't slowed for
//! it: only what's short is rationed.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::admit::Class;

/// A running group of work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub id: String,
    pub class: Class,
    /// What it writes a second when not slowed.
    pub demand_bps: f64,
    /// The CPU cores it uses when not slowed.
    #[serde(default)]
    pub demand_cpu: f64,
    /// A protected target's own processes (never slowed).
    pub protected: bool,
}

/// How finely a share is applied: the runner's pacer runs a group a whole number of percent of
/// each period.
pub const SHARE_STEPS: f64 = 100.0;

/// The shortest cycle (µs) the pacer may use for what was `asked_us`, given `lag_us`, how long its
/// actuator takes to make a switch take effect, measured (LB05: replaces the pacer's 50 ms tick and
/// its 1 s floor). A share is a whole percent of a cycle ([`SHARE_STEPS`]): the least slice is a
/// step of it, so the least cycle is [`SHARE_STEPS`] lags long, or a slice would end before its
/// switch took effect. None measured yet: what was asked, the first switch measures it.
pub fn cycle_us(asked_us: u64, lag_us: Option<f64>) -> u64 {
    let least = lag_us.filter(|l| l.is_finite() && *l > 0.0).map_or(0, |l| (l * SHARE_STEPS).ceil() as u64);
    asked_us.max(least).max(1)
}

/// When paced groups run (LB05, G11): each has a place of its own in its cycle, stopped for the first
/// part of every cycle and running for its share at the end, so any share above 0 runs and every
/// cycle holds exactly its share. A place is a position in the cycle, a function of time: there's no
/// credit to earn or spend, and nothing to catch up after a stall (the 4-tick cap's reason, gone with
/// it). Nothing here looks at a clock: `now_us` comes in, and when the group next switches goes out,
/// a due time for that one group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Duty {
    places: HashMap<String, Place>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    /// `(now + offset) % cycle` is where the group is in its cycle.
    offset_us: u64,
    cycle_us: u64,
}

/// What a group does at a moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phase {
    pub runs: bool,
    /// When that next changes (µs on the clock given); never, for a share of 0.
    pub next_us: Option<u64>,
}

/// Where in its cycle a group starts (of 1000): its id's own, so groups first paced together
/// don't all run at once.
fn place(id: &str) -> u64 {
    id.bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b))) % 1000
}

impl Duty {
    /// Whether group `id`, held to `share` percent (whole, below [`SHARE_STEPS`]) of a cycle of
    /// `cycle_us`, runs at `now_us`, and when it next switches. A changed cycle keeps the
    /// group's place as a fraction of the cycle (a position that jumped would switch it for nothing).
    pub fn phase(&mut self, id: &str, share: u8, now_us: u64, cycle_us: u64) -> Phase {
        if share == 0 {
            self.places.remove(id);
            return Phase { runs: false, next_us: None };
        }
        let cycle = cycle_us.max(1);
        let at = |offset: u64, cycle: u64| (u128::from(now_us) + u128::from(offset)) % u128::from(cycle);
        let aligned = |pos: u128| ((pos + u128::from(cycle) - u128::from(now_us) % u128::from(cycle)) % u128::from(cycle)) as u64;
        let place = match self.places.get(id).copied() {
            Some(p) if p.cycle_us == cycle => p,
            Some(p) => {
                // The same place in the new cycle.
                let pos = at(p.offset_us, p.cycle_us.max(1)) * u128::from(cycle) / u128::from(p.cycle_us.max(1));
                Place { offset_us: aligned(pos), cycle_us: cycle }
            }
            None => Place { offset_us: aligned(u128::from(place(id)) * u128::from(cycle) / 1000), cycle_us: cycle },
        };
        self.places.insert(id.to_owned(), place);
        // The slice ends the cycle: the stop first, then the run, as far as both can be told apart.
        let run = (u128::from(cycle) * u128::from(share.min(99)) / SHARE_STEPS as u128).clamp(1, u128::from(cycle));
        let stop = u128::from(cycle) - run;
        let pos = at(place.offset_us, cycle);
        let (runs, left) = if pos >= stop { (true, u128::from(cycle) - pos) } else { (false, stop - pos) };
        Phase { runs, next_us: Some(now_us.saturating_add(u64::try_from(left).unwrap_or(u64::MAX))) }
    }

    /// Forgets the groups that aren't paced.
    pub fn keep<'a>(&mut self, paced: impl Iterator<Item = &'a str>) {
        let paced: BTreeSet<&str> = paced.collect();
        self.places.retain(|id, _| paced.contains(id.as_str()));
    }
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

/// One resource's shares: `budget` water-filled class by class over `demand` (none: no limit).
fn fill(budget: Option<f64>, groups: &[Running], demand: impl Fn(&Running) -> f64) -> Vec<f64> {
    let wanted: f64 = groups.iter().map(|g| demand(g).max(0.0)).sum();
    let Some(budget) = budget.filter(|b| wanted > *b) else { return vec![1.0; groups.len()] };
    let exempt = |g: &Running| g.protected || g.class == Class::Person || demand(g) <= 0.0;
    let mut left = budget - groups.iter().filter(|g| exempt(g)).map(|g| demand(g).max(0.0)).sum::<f64>();
    let mut levels: Vec<(Class, f64)> = Vec::new();
    let mut classes: Vec<Class> = groups.iter().filter(|g| !exempt(g)).map(|g| g.class).collect();
    classes.sort();
    classes.dedup();
    for c in classes.into_iter().rev() {
        let demands: Vec<f64> = groups.iter().filter(|g| !exempt(g) && g.class == c).map(&demand).collect();
        let l = level(&demands, left);
        left -= demands.iter().map(|d| d.min(l)).sum::<f64>();
        levels.push((c, l));
    }
    groups
        .iter()
        .map(|g| {
            if exempt(g) {
                return 1.0;
            }
            let l = levels.iter().find(|(c, _)| *c == g.class).map_or(0.0, |(_, l)| *l);
            (demand(g).min(l) / demand(g)).clamp(0.0, 1.0)
        })
        .collect()
}

/// Each running group's share under the write budget and the CPU budget (none: no limit): the
/// smaller of what each resource leaves it.
pub fn plan(write_budget: Option<f64>, cpu_budget: Option<f64>, groups: &[Running]) -> Vec<Pace> {
    let w = fill(write_budget, groups, |g| g.demand_bps);
    let c = fill(cpu_budget, groups, |g| g.demand_cpu);
    groups
        .iter()
        .zip(w.iter().zip(&c))
        .map(|(g, (sw, sc))| {
            let share = sw.min(*sc);
            let why = if g.protected {
                "a protected target's: never slowed".to_owned()
            } else if g.class == Class::Person {
                "a person's: never slowed".to_owned()
            } else if share >= 1.0 {
                "within its fair part of every budget".to_owned()
            } else if share <= 0.0 {
                "stopped: higher classes take the whole budget it needs".to_owned()
            } else if sw <= sc {
                format!("slowed to {:.0}%: wants {}/s of writes, its fair part of the {}/s budget is {}/s", share * 100.0, mb(g.demand_bps), mb(write_budget.unwrap_or_default()), mb(g.demand_bps * share))
            } else {
                format!("slowed to {:.0}%: wants {:.1} CPU cores, its fair part of the {:.1}-core budget is {:.1}", share * 100.0, g.demand_cpu, cpu_budget.unwrap_or_default(), g.demand_cpu * share)
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
        Running { id: id.into(), class, demand_bps: d, demand_cpu: 0.0, protected: false }
    }

    #[test]
    fn the_lowest_class_is_slowed_first_and_people_and_readers_never() {
        let groups = [g("person", Class::Person, 500e6), g("a1", Class::Agent, 100e6), g("a2", Class::Agent, 400e6), g("bench", Class::Background, 300e6), g("reader", Class::Agent, 0.0)];
        let p = plan(Some(800e6), None, &groups);
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
        assert!(plan(Some(0.0), None, &groups).iter().all(|p| p.share == 0.0));
        assert!(plan(None, None, &groups).iter().all(|p| p.share == 1.0));
    }

    #[test]
    fn a_cpu_budget_slows_only_what_uses_cpu_and_the_smaller_share_wins() {
        let mut busy = g("compile", Class::Agent, 0.0);
        busy.demand_cpu = 16.0;
        let mut linker = g("link", Class::Agent, 400e6);
        linker.demand_cpu = 1.0;
        let reader = g("idle", Class::Agent, 0.0);
        let p = plan(Some(200e6), Some(8.0), &[busy, linker, reader]);
        let share = |id: &str| p.iter().find(|x| x.id == id).map_or(-1.0, |x| x.share);
        // CPU: 8 cores over 16 + 1: the linker's 1 fits, the compile gets 7 of 16.
        assert!((share("compile") - 7.0 / 16.0).abs() < 1e-9);
        // Writes: 200 of 400 MB/s; its CPU share is whole, the smaller is the write share.
        assert!((share("link") - 0.5).abs() < 1e-9);
        assert_eq!(share("idle"), 1.0);
    }

    /// A group held to `share` percent of a `cycle_us` cycle, followed by its due times alone for
    /// `secs`: the part of the time it ran, and its longest stop (µs).
    fn simulate(share: u8, cycle_us: u64, secs: u64) -> (f64, u64) {
        let mut duty = Duty::default();
        let (mut now, mut ran, mut stopped, mut longest) = (0u64, 0u64, 0u64, 0u64);
        while now < secs * 1_000_000 {
            let p = duty.phase("session:a", share, now, cycle_us);
            let Some(next) = p.next_us else { break };
            let dt = next - now;
            if p.runs {
                ran += dt;
                stopped = 0;
            } else {
                stopped += dt;
                longest = longest.max(stopped);
            }
            now = next;
        }
        (ran as f64 / now.max(1) as f64, longest)
    }

    #[test]
    fn g11_a_share_above_zero_runs_whatever_cycle_it_has_and_keeps_its_share() {
        // A 5 ms cycle and a share under 20%: the pacer once truncated these to no time at all.
        for cycle in [5_000u64, 172_000, 1_000_000, 3_000_000] {
            for share in [1u8, 2, 5, 14, 19, 20, 33, 50, 75, 99] {
                let (ran, longest) = simulate(share, cycle, 900);
                let want = f64::from(share) / 100.0;
                assert!((ran - want).abs() <= 0.002, "{share}% of a {cycle} us cycle ran {ran}");
                // The stop is the rest of the cycle, and no longer: there's no catch-up to pay.
                let stop = cycle - cycle * u64::from(share) / 100;
                assert!(longest <= stop + 1, "{share}% of {cycle} us: stopped {longest} us at a stretch, its stop is {stop}");
            }
        }
    }

    #[test]
    fn g11_a_share_of_zero_never_runs_and_is_never_due() {
        let mut duty = Duty::default();
        assert_eq!(duty.phase("a", 0, 5, 1000), Phase { runs: false, next_us: None });
    }

    #[test]
    fn g11_a_switch_is_due_exactly_when_the_group_changes() {
        let mut duty = Duty::default();
        let mut now = 0u64;
        for _ in 0..1000 {
            let p = duty.phase("a", 40, now, 200_000);
            let next = p.next_us.unwrap_or(now + 1);
            // Up to the moment it's due it stays as it is; at the moment it's due it has changed.
            if next > now + 1 {
                assert_eq!(duty.phase("a", 40, next - 1, 200_000).runs, p.runs, "{now}");
            }
            let after = duty.phase("a", 40, next, 200_000);
            assert_ne!(after.runs, p.runs, "due at {next} and no change");
            now = next;
        }
    }

    #[test]
    fn g11_groups_at_one_share_do_not_all_run_at_once() {
        let mut duty = Duty::default();
        let mut seed = 11u64;
        let ids: Vec<String> = (0..20)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                format!("session:{:016x}{:016x}", seed, seed.rotate_left(29))
            })
            .collect();
        for i in 0..4000u64 {
            let now = i * 5_000;
            let running = ids.iter().filter(|d| duty.phase(d, 50, now, 1_000_000).runs).count();
            assert!((3..=17).contains(&running), "{running} of 20 run at {now} us");
        }
    }

    #[test]
    fn g11_a_group_first_paced_at_a_high_share_is_stopped_for_less_than_its_stop() {
        let mut duty = Duty::default();
        for i in 0..200 {
            let id = format!("session:{i}");
            let first = duty.phase(&id, 99, 5_000_000, 1_000_000);
            if !first.runs {
                // Its stop of the cycle (1%, 10 ms) is all it waits; then it runs.
                let at = first.next_us.unwrap_or(0);
                assert!(at <= 5_000_000 + 10_000, "{id}: stopped until {at}");
                assert!(duty.phase(&id, 99, at, 1_000_000).runs, "{id}");
            }
        }
    }

    #[test]
    fn g11_a_changed_cycle_keeps_a_groups_place() {
        // Not by the next due: a group mid-slice stays running when the cycle changes under it, days
        // into the clock as much as at its start.
        for start in [0u64, 1_000_000_000_000] {
            for i in 0..50 {
                let id = format!("session:{i}");
                let mut duty = Duty::default();
                let cycle = 1_000_000u64;
                // Find a moment it runs.
                let mut t = start;
                let until = loop {
                    let p = duty.phase(&id, 50, t, cycle);
                    if p.runs {
                        break p.next_us.unwrap_or(t);
                    }
                    t = p.next_us.unwrap_or(t + 1);
                };
                let mid = t + (until - t) / 2;
                // 0.1% longer (a lag's jitter) or 30% (a lag that grew): the same place in either.
                for longer in [1_001_000, 1_300_000] {
                    assert!(duty.clone().phase(&id, 50, mid, longer).runs, "{id} at {mid}: a cycle of {longer} us switched it off in the middle of its slice");
                }
            }
        }
    }

    #[test]
    fn g11_a_group_no_longer_paced_is_forgotten() {
        let mut duty = Duty::default();
        duty.phase("a", 50, 0, 1000);
        duty.phase("b", 50, 0, 1000);
        duty.keep(["a"].into_iter());
        assert!(duty.places.contains_key("a") && !duty.places.contains_key("b"));
    }

    #[test]
    fn lb05_the_least_cycle_is_a_lag_for_each_step_of_a_share() {
        // A lag of 2 ms: a 1% slice (a step) of a 200 ms cycle is its lag long.
        assert_eq!(cycle_us(5_000, Some(2_000.0)), 200_000);
        // Asked for more than that, what was asked.
        assert_eq!(cycle_us(1_500_000, Some(2_000.0)), 1_500_000);
        // Nothing measured yet: what was asked. The first switch measures it.
        assert_eq!(cycle_us(5_000, None), 5_000);
        // A lag that can't be taken as one floors nothing.
        assert_eq!(cycle_us(5_000, Some(f64::NAN)), 5_000);
        assert_eq!(cycle_us(0, None), 1);
    }

    #[test]
    fn lb05_at_the_least_cycle_every_share_has_a_slice_as_long_as_the_lag() {
        let lag = 1_337.0;
        let cycle = cycle_us(1, Some(lag));
        for share in 1u8..=99 {
            let (ran, longest) = simulate(share, cycle, 60);
            let slice = (cycle as f64) * f64::from(share) / 100.0;
            assert!(slice >= lag - 1.0, "{share}%: a slice of {slice} us under a lag of {lag} us");
            assert!(ran > 0.0 && (longest as f64) < cycle as f64, "{share}%");
        }
    }

    proptest! {
        #[test]
        fn shares_spend_at_most_the_budget_fairly_within_a_class_and_higher_classes_first(
            budget in 0f64..2e9,
            groups in prop::collection::vec((0u8..3, 0f64..1e9, any::<bool>()), 1..16)
        ) {
            let classes = [Class::Background, Class::Agent, Class::Person];
            let gs: Vec<Running> = groups.iter().enumerate().map(|(i, (c, d, p))| Running {
                id: format!("g{i}"), class: classes.get(usize::from(*c)).copied().unwrap_or(Class::Agent), demand_bps: *d, demand_cpu: 0.0, protected: *p,
            }).collect();
            let p = plan(Some(budget), None, &gs);
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
