//! Who starts next (docs/headroom.md, "Admission"): work waiting to start (a session's next turn,
//! a heavy command) has a class, how long it has waited, and a demand learned from its own past
//! (writes a second, CPU cores, peak memory, how long it runs) with the mean over everything as the
//! prior. Writes and CPU aren't held for: the pacer shares the budgets among what runs (holding
//! work the pacer can slow protects nothing and only delays it; sharing also keeps mean response
//! time insensitive to how job sizes vary, where first-come-first-served's grows with it). Memory
//! can't be slowed, so work starts when its peak memory fits what's free less what is promised. Order: class, then highest response
//! ratio ((wait + run time) / run time), so short work goes first and long work never starves.
//! Work behind one that doesn't fit starts only if it delays no waiting work (conservative
//! backfilling). With nothing learned at all, one unknown at a time starts, until its demand shows.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Who yields first: the lowest class is held and slowed first, and a person is never held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Benches, sync.
    Background,
    /// What an agent runs on its own: its turns, its tools' commands.
    Agent,
    /// A person's own message or command: never held.
    Person,
}

/// What a piece of work needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Demand {
    pub write_bps: f64,
    pub mem_bytes: f64,
    pub duration_us: f64,
    #[serde(default)]
    pub cpu_cores: f64,
}

/// What's been seen of one kind of work (a session, a command's shape): sums for exact means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    pub runs: u64,
    pub write_bps: f64,
    pub mem_bytes: f64,
    pub duration_us: f64,
    #[serde(default)]
    pub cpu_cores: f64,
}

/// Demands learned per kind of work.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub by_key: HashMap<String, Seen>,
    pub all: Seen,
}

impl Learned {
    /// One finished run of `key`: its peak write rate, peak memory and duration.
    pub fn record(&mut self, key: &str, d: Demand) {
        for s in [self.by_key.entry(key.to_owned()).or_default(), &mut self.all] {
            s.runs += 1;
            s.write_bps += d.write_bps;
            s.mem_bytes += d.mem_bytes;
            s.duration_us += d.duration_us;
            s.cpu_cores += d.cpu_cores;
        }
    }

    /// What `key` is expected to need: its own mean, else the mean over everything, else
    /// unknown (nothing learned yet).
    /// Held to what the machine has: no kind of work is learned to want more than its `cores` or
    /// to write more a second than its `disks` take (work measured at a tiny share could be
    /// learned to want more than the whole machine).
    pub fn within(&mut self, cores: Option<f64>, disks: Option<f64>) {
        for s in self.by_key.values_mut().chain(std::iter::once(&mut self.all)) {
            let n = s.runs as f64;
            if let Some(c) = cores {
                s.cpu_cores = s.cpu_cores.min(c * n);
            }
            if let Some(d) = disks {
                s.write_bps = s.write_bps.min(d * n);
            }
        }
    }

    pub fn demand(&self, key: &str) -> Option<Demand> {
        let s = self.by_key.get(key).filter(|s| s.runs > 0).copied().or_else(|| (self.all.runs > 0).then_some(self.all))?;
        let n = s.runs as f64;
        Some(Demand { write_bps: s.write_bps / n, mem_bytes: s.mem_bytes / n, duration_us: s.duration_us / n, cpu_cores: s.cpu_cores / n })
    }
}

/// Work waiting to start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Waiting {
    pub id: String,
    /// Its kind, for what's learned (a session's id, a command's shape).
    pub key: String,
    pub class: Class,
    pub since_us: u64,
}

/// Work let in whose demand hasn't shown yet: held for it until it does, or its expected run
/// time passes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Promise {
    pub id: String,
    pub demand: Option<Demand>,
    pub until_us: u64,
}

/// Room that frees when running or promised work is expected to end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Free {
    pub at_us: u64,
    pub write_bps: f64,
    pub mem_bytes: f64,
    #[serde(default)]
    pub cpu_cores: f64,
}

/// What there is to share now, and when more frees.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Room {
    /// The write budget (none: no limit).
    pub write_budget: Option<f64>,
    /// What kiln's running work writes a second (unslowed).
    pub write_used: f64,
    pub mem_free: f64,
    /// What running work gives back as it's expected to end.
    pub frees: Vec<Free>,
    /// The CPU budget in cores (none: no limit), and the cores kiln's running work wants.
    #[serde(default)]
    pub cpu_budget: Option<f64>,
    #[serde(default)]
    pub cpu_used: f64,
}

/// A decision about one waiting piece of work.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub id: String,
    pub start: bool,
    /// When it's expected to start (its reservation).
    pub at_us: Option<u64>,
    pub why: String,
}

fn ratio(now_us: u64, w: &Waiting, d: Option<Demand>) -> f64 {
    let wait = now_us.saturating_sub(w.since_us) as f64;
    match d.map(|d| d.duration_us).filter(|r| *r > 0.0) {
        Some(run) => (wait + run) / run,
        // Nothing to say how long it runs: by its wait alone, first come first served.
        None => wait,
    }
}

fn mb(b: f64) -> String {
    format!("{:.0} MB", b / 1e6)
}

/// What's free from each time on: a step function, its steps by time.
struct Profile {
    /// (from, writes, memory, cores) free.
    steps: Vec<(u64, f64, f64, f64)>,
}

impl Profile {
    fn new(now_us: u64, write: f64, mem: f64, cpu: f64, frees: &[Free]) -> Self {
        let mut frees: Vec<Free> = frees.iter().copied().filter(|f| f.at_us > now_us).collect();
        frees.sort_by_key(|f| f.at_us);
        let mut steps = vec![(now_us, write, mem, cpu)];
        let (mut w, mut m, mut c) = (write, mem, cpu);
        for f in frees {
            w += f.write_bps;
            m += f.mem_bytes;
            c += f.cpu_cores;
            match steps.last_mut() {
                Some(last) if last.0 == f.at_us => *last = (f.at_us, w, m, c),
                _ => steps.push((f.at_us, w, m, c)),
            }
        }
        Self { steps }
    }

    /// The earliest step `d` fits from, for all of `run` (µs, none: for good).
    fn earliest(&self, d: Demand, run: Option<u64>) -> Option<u64> {
        self.steps.iter().map(|s| s.0).find(|&start| {
            let end = run.map(|r| start.saturating_add(r));
            // The step in force at `start`, and every one that begins before `end`.
            let at = self.steps.iter().rposition(|s| s.0 <= start).unwrap_or_default();
            self.steps.iter().skip(at).take_while(|s| s.0 == start || s.0 > start && end.is_none_or(|e| s.0 < e)).all(|s| d.mem_bytes <= s.2 + 1e-9)
        })
    }

    fn split(&mut self, t: u64) {
        if let Some(i) = self.steps.iter().rposition(|s| s.0 <= t)
            && self.steps.get(i).is_some_and(|s| s.0 != t)
            && let Some(&(_, w, m, c)) = self.steps.get(i)
        {
            self.steps.insert(i + 1, (t, w, m, c));
        }
    }

    /// `d` reserved from `start` for `run`.
    fn take(&mut self, d: Demand, start: u64, run: Option<u64>) {
        self.split(start);
        let end = run.map(|r| start.saturating_add(r));
        if let Some(e) = end {
            self.split(e);
        }
        for s in self.steps.iter_mut().filter(|s| s.0 >= start && end.is_none_or(|e| s.0 < e)) {
            s.1 -= d.write_bps;
            s.2 -= d.mem_bytes;
            s.3 -= d.cpu_cores;
        }
    }
}

/// Each waiting piece of work: starts now, or waits and why, with its reservation. `promised`:
/// work let in whose demand hasn't shown yet.
pub fn decide(now_us: u64, room: &Room, promised: &[Promise], waiting: &[Waiting], learned: &Learned) -> Vec<Decision> {
    let held: f64 = promised.iter().filter_map(|p| p.demand).map(|d| d.write_bps).sum();
    let held_mem: f64 = promised.iter().filter_map(|p| p.demand).map(|d| d.mem_bytes).sum();
    let held_cpu: f64 = promised.iter().filter_map(|p| p.demand).map(|d| d.cpu_cores).sum();
    let write_now = room.write_budget.map_or(f64::INFINITY, |b| b - room.write_used - held);
    let cpu_now = room.cpu_budget.map_or(f64::INFINITY, |b| b - room.cpu_used - held_cpu);
    let mut profile = Profile::new(now_us, write_now, room.mem_free - held_mem, cpu_now, &room.frees);
    // With nothing learned anywhere, one unknown at a time.
    let mut unknown_out = promised.iter().any(|p| p.demand.is_none());

    let mut order: Vec<(&Waiting, Option<Demand>)> = waiting.iter().map(|w| (w, learned.demand(&w.key))).collect();
    order.sort_by(|(a, da), (b, db)| b.class.cmp(&a.class).then(ratio(now_us, b, *db).total_cmp(&ratio(now_us, a, *da))).then(a.since_us.cmp(&b.since_us)));

    let mut out = Vec::with_capacity(order.len());
    for (w, d) in order {
        if w.class == Class::Person {
            out.push(Decision { id: w.id.clone(), start: true, at_us: Some(now_us), why: "a person's: never held".into() });
            continue;
        }
        let Some(d) = d else {
            if unknown_out {
                out.push(Decision { id: w.id.clone(), start: false, at_us: None, why: "nothing learned yet of what work needs: one starts at a time until its demand shows".into() });
                continue;
            }
            unknown_out = true;
            out.push(Decision { id: w.id.clone(), start: true, at_us: Some(now_us), why: "nothing learned yet: starts alone, to learn what it needs".into() });
            continue;
        };
        // What it needs of what can be paced is at most the budget: past that it is slowed, so it
        // starts once kiln's other work leaves it the whole budget, and never starves.
        let d = Demand { write_bps: room.write_budget.map_or(d.write_bps, |b| d.write_bps.min(b)), cpu_cores: room.cpu_budget.map_or(d.cpu_cores, |b| d.cpu_cores.min(b)), ..d };
        // Its run, as learned (none known: it holds its room for good, which delays no one).
        let run = (d.duration_us > 0.0).then_some(d.duration_us as u64);
        match profile.earliest(d, run) {
            Some(at) => {
                profile.take(d, at, run);
                let start = at <= now_us;
                let why = if start {
                    "its memory fits without delaying any work waiting ahead of it (its writes and CPU are paced)".to_owned()
                } else {
                    format!("needs {} of memory, held by work running or ahead of it; expected to start in {:.0} s", mb(d.mem_bytes), at.saturating_sub(now_us) as f64 / 1e6)
                };
                out.push(Decision { id: w.id.clone(), start, at_us: Some(at), why });
            }
            None => out.push(Decision {
                id: w.id.clone(),
                start: false,
                at_us: None,
                why: format!("needs more memory than this machine has free even empty: {}", mb(d.mem_bytes)),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn w(id: &str, key: &str, class: Class, since: u64) -> Waiting {
        Waiting { id: id.into(), key: key.into(), class, since_us: since }
    }

    fn learned(items: &[(&str, f64, f64, f64)]) -> Learned {
        let mut l = Learned::default();
        for (k, wr, mem, dur) in items {
            l.record(k, Demand { write_bps: *wr, mem_bytes: *mem, duration_us: *dur, cpu_cores: 0.0 });
        }
        l
    }

    #[test]
    fn short_work_goes_first_people_always_and_long_work_ages_past_it() {
        let l = learned(&[("short", 10e6, 1e9, 1e6), ("long", 10e6, 1e9, 1e9)]);
        // Memory for one at a time (writes are paced, not held for).
        let room = Room { write_budget: Some(10e6), write_used: 0.0, mem_free: 1.5e9, frees: Vec::new(), cpu_budget: None, cpu_used: 0.0 };
        let q = [w("L", "long", Class::Agent, 0), w("S", "short", Class::Agent, 0), w("P", "short", Class::Person, 0)];
        let d = decide(1_000_000, &room, &[], &q, &l);
        let started: Vec<&str> = d.iter().filter(|d| d.start).map(|d| d.id.as_str()).collect();
        assert_eq!(started, vec!["P", "S"]);
        // Long has waited 10x its run time: it goes before a fresh short one.
        let q = [w("L", "long", Class::Agent, 0), w("S2", "short", Class::Agent, 9_999_000_000)];
        let d = decide(10_000_000_000, &room, &[], &q, &l);
        assert_eq!(d.iter().find(|d| d.start).map(|d| d.id.as_str()), Some("L"));
    }

    #[test]
    fn with_nothing_learned_one_starts_at_a_time_in_arrival_order() {
        let l = Learned::default();
        let q = [w("b", "k2", Class::Agent, 5), w("a", "k1", Class::Agent, 1)];
        let open = Room { write_budget: None, write_used: 0.0, mem_free: 1e12, frees: Vec::new(), cpu_budget: None, cpu_used: 0.0 };
        let d = decide(10, &open, &[], &q, &l);
        assert_eq!(d.iter().filter(|d| d.start).map(|d| d.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
        let out = [Promise { id: "a".into(), demand: None, until_us: 100 }];
        let d = decide(10, &open, &out, q.get(..1).unwrap_or_default(), &l);
        assert!(!d.iter().any(|d| d.start));
    }

    proptest! {
        #[test]
        fn what_starts_fits_now_and_no_reservation_is_pushed_back_by_work_behind_it(
            budget in 1e6f64..2e9, used in 0f64..1e9, mem in 1e9f64..1e11,
            frees in prop::collection::vec((1u64..5_000_000_000, 0f64..5e8, 0f64..3e10), 0..6),
            items in prop::collection::vec((1e5f64..5e8, 1e7f64..3e10, 1e5f64..1e10, 0u64..1_000_000), 1..14)
        ) {
            let mut l = Learned::default();
            let mut q = Vec::new();
            for (i, (wr, m, dur, since)) in items.iter().enumerate() {
                let key = format!("k{i}");
                l.record(&key, Demand { write_bps: *wr, mem_bytes: *m, duration_us: *dur, cpu_cores: 0.0 });
                q.push(w(&format!("w{i}"), &key, Class::Agent, *since));
            }
            let room = Room {
                write_budget: Some(budget),
                write_used: used,
                mem_free: mem,
                frees: frees.iter().map(|(at, wr, m)| Free { at_us: 2_000_000 + at, write_bps: *wr, mem_bytes: *m, cpu_cores: 0.0 }).collect(),
                cpu_budget: None,
                cpu_used: 0.0,
            };
            let d = decide(2_000_000, &room, &[], &q, &l);
            let demand = |id: &str| q.iter().find(|x| x.id == id).and_then(|x| l.demand(&x.key)).unwrap_or_default();
            let started: Vec<Demand> = d.iter().filter(|d| d.start).map(|d| demand(&d.id)).collect();
            // Writes are paced to the budget, not held for: only memory decides who starts.
            let _ = (budget, used);
            prop_assert!(started.iter().map(|d| d.mem_bytes).sum::<f64>() <= mem + 1e-6);
            // Each one's reservation is the one it gets with only the work ahead of it waiting.
            for (i, x) in d.iter().enumerate() {
                let ahead: Vec<Waiting> = d.iter().take(i + 1).filter_map(|y| q.iter().find(|w| w.id == y.id).cloned()).collect();
                let alone = decide(2_000_000, &room, &[], &ahead, &l);
                prop_assert_eq!(alone.iter().find(|y| y.id == x.id).and_then(|y| y.at_us), x.at_us);
            }
        }

        #[test]
        fn every_waiting_piece_of_work_starts_eventually(durs in prop::collection::vec(1u64..50, 1..12)) {
            // One slot's worth of budget; each finishes after its run; all arrive at 0.
            let mut l = Learned::default();
            let mut q: Vec<Waiting> = Vec::new();
            for (i, dur) in durs.iter().enumerate() {
                let key = format!("k{i}");
                l.record(&key, Demand { write_bps: 1.0, mem_bytes: 1.0, duration_us: *dur as f64 * 1e6, cpu_cores: 1.0 });
                q.push(w(&format!("w{i}"), &key, Class::Agent, 0));
            }
            let mut running: Option<(String, u64)> = None;
            let mut t = 0u64;
            while !q.is_empty() && t < 10_000_000_000 {
                if running.as_ref().is_some_and(|(_, until)| t >= *until) {
                    running = None;
                }
                let room = Room {
                    write_budget: Some(1.0),
                    write_used: if running.is_some() { 1.0 } else { 0.0 },
                    mem_free: if running.is_some() { 0.0 } else { 1.0 },
                    frees: running.iter().map(|(_, until)| Free { at_us: *until, write_bps: 1.0, mem_bytes: 1.0, cpu_cores: 1.0 }).collect(),
                    cpu_budget: Some(1.0),
                    cpu_used: if running.is_some() { 1.0 } else { 0.0 },
                };
                let d = decide(t, &room, &[], &q, &l);
                if let Some(s) = d.iter().find(|d| d.start) {
                    let i = q.iter().position(|x| x.id == s.id).unwrap_or_default();
                    let x = q.remove(i);
                    let run = l.demand(&x.key).map_or(0, |d| d.duration_us as u64);
                    running = Some((x.id, t + run));
                }
                t += 1_000_000;
            }
            prop_assert!(q.is_empty(), "{} never started", q.len());
        }
    }
}
