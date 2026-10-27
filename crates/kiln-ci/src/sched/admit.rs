//! Admission (docs/ci.md W28-R60 to R63, R68): which demands go into the scheduler's queues now,
//! which wait and why, and which are skipped for a newer one of their lane. Pure: time comes in,
//! decisions and timers go out, in the caller's unit (the scheduler's unix milliseconds, the
//! replay's seconds), and what is saturated is the caller's to say.
//!
//! A demand is one unit of work (a job, with its matrix values) of a lane (a repository's
//! workflow on a branch), at its run's place in the lane (`order`: the newest is the highest).
//!
//! - **Coalescing** (`coalesce:`, R61): a demand whose resources are saturated is held for a quiet
//!   period, which each newer demand of its lane and unit restarts, never past `max_wait` from the
//!   burst's start; it is released at once when its resources stop being saturated. A newer demand
//!   skips the older one held, so a burst runs its newest commit. One released is never skipped.
//! - **Cadence** (`cadence:`, R62): at most one demand of a lane and unit waits to start, the
//!   newest; `free` lets it start as soon as its resource frees (the scheduler keeps that resource
//!   from taking other work meanwhile), `every` at most once per interval.
//! - **Speculation window** (R68): a lane's speculative demands (bisection steps, intermediate
//!   commits kept for direct verdicts, land candidates past the head) are admitted only while
//!   fewer than its AIMD window are in flight: a pass widens it by one, a failure halves it.
//!
//! Every hold says why ([`Held`]), and every skip names the demand that took its place.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::wf::ir::YVal;

/// `coalesce: { quiet, max-wait, when }` (W28-R61). Times in the caller's unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coalesce {
    pub quiet: i64,
    pub max_wait: i64,
    /// `when: always`: held whether or not its resources are saturated (`when: busy`, the default,
    /// holds only while they are).
    pub always: bool,
}

/// `cadence:` (W28-R62).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// `on: free`: the newest pending demand starts as soon as its resource frees.
    Free,
    /// `every: <duration>`: at most one start per interval, the newest demand then.
    Every(i64),
}

/// A lane's speculation window (W28-R68): AIMD, between `floor` and `cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub initial: u32,
    pub floor: u32,
    pub cap: u32,
}

impl Default for Window {
    fn default() -> Self {
        Window { initial: WINDOW_INITIAL, floor: WINDOW_FLOOR, cap: WINDOW_CAP }
    }
}

/// The defaults W28 takes from replaying the recorded week (ci/sim/README.md): a quiet period of
/// a minute while busy, bursts held ten minutes at most (W25 step 1's sweeps), and a speculation
/// window of 4, at least 1, at most 20 (step 4's sweep).
pub const QUIET_S: i64 = 60;
pub const MAX_WAIT_S: i64 = 600;
pub const WINDOW_INITIAL: u32 = 4;
pub const WINDOW_FLOOR: u32 = 1;
pub const WINDOW_CAP: u32 = 20;

/// A workflow's `coalesce:` and `cadence:` (W28-R61, R62), in seconds as written; off when
/// neither is declared (§0.3: what kiln decides on its own is off until declared).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spec {
    pub coalesce: Option<Coalesce>,
    pub cadence: Option<Cadence>,
}

impl Spec {
    /// From the workflow's keys: `coalesce: true` or `{ quiet, max-wait, when: busy | always }`
    /// (durations like `60s`, `10m`, or seconds; defaults a quiet minute, ten minutes at most, while
    /// busy); `cadence: true`, `free` or `{ on: free }`, or `{ every: 30m }`.
    pub fn parse(coalesce: Option<&YVal>, cadence: Option<&YVal>) -> Result<Spec, String> {
        let coalesce = match coalesce {
            None | Some(YVal::Null | YVal::Bool(false)) => None,
            Some(YVal::Bool(true)) => Some(Coalesce { quiet: QUIET_S, max_wait: MAX_WAIT_S, always: false }),
            Some(m @ YVal::Map(entries)) => {
                if let Some((k, _)) = entries.iter().find(|(k, _)| !matches!(k.as_str(), "quiet" | "max-wait" | "when")) {
                    return Err(format!("`coalesce` takes `quiet`, `max-wait` and `when`, not `{k}`"));
                }
                let quiet = m.get("quiet").map(|v| duration(v, "coalesce.quiet")).transpose()?.unwrap_or(QUIET_S);
                let max_wait = m.get("max-wait").map(|v| duration(v, "coalesce.max-wait")).transpose()?.unwrap_or(MAX_WAIT_S);
                let always = match m.get("when") {
                    None => false,
                    Some(YVal::Str(t)) if t.src.trim() == "busy" => false,
                    Some(YVal::Str(t)) if t.src.trim() == "always" => true,
                    Some(_) => return Err("`coalesce.when` is `busy` (the default: only while the runners it needs are busy) or `always`".into()),
                };
                Some(Coalesce { quiet, max_wait, always })
            }
            Some(_) => return Err("`coalesce` is `true` or `{ quiet, max-wait, when }`".into()),
        };
        let cadence = match cadence {
            None | Some(YVal::Null | YVal::Bool(false)) => None,
            Some(YVal::Bool(true)) => Some(Cadence::Free),
            Some(YVal::Str(t)) if t.src.trim() == "free" => Some(Cadence::Free),
            Some(m @ YVal::Map(entries)) => {
                if let Some((k, _)) = entries.iter().find(|(k, _)| !matches!(k.as_str(), "on" | "every")) {
                    return Err(format!("`cadence` takes `on: free` or `every`, not `{k}`"));
                }
                match (m.get("on"), m.get("every")) {
                    (Some(YVal::Str(t)), None) if t.src.trim() == "free" => Some(Cadence::Free),
                    (None, Some(v)) => Some(Cadence::Every(duration(v, "cadence.every")?.max(1))),
                    _ => return Err("`cadence` is `{ on: free }` or `{ every: <duration> }`".into()),
                }
            }
            Some(_) => return Err("`cadence` is `true`, `free`, `{ on: free }` or `{ every: <duration> }`".into()),
        };
        Ok(Spec { coalesce, cadence })
    }

    /// Whether the workflow asks for admission at all.
    pub fn any(&self) -> bool {
        self.coalesce.is_some() || self.cadence.is_some()
    }

    /// In milliseconds, the scheduler's unit.
    pub fn millis(self) -> Spec {
        let ms = |s: i64| s.saturating_mul(1_000);
        Spec {
            coalesce: self.coalesce.map(|c| Coalesce { quiet: ms(c.quiet), max_wait: ms(c.max_wait), always: c.always }),
            cadence: self.cadence.map(|c| match c {
                Cadence::Free => Cadence::Free,
                Cadence::Every(s) => Cadence::Every(ms(s)),
            }),
        }
    }
}

/// A duration in seconds: a number, or `90s`, `15m`, `2h`, `1d`.
fn duration(v: &YVal, key: &str) -> Result<i64, String> {
    let secs = match v {
        YVal::Num(n) if *n >= 0.0 => Some(n.round() as u64),
        YVal::Str(t) => crate::trigger::cron::seconds(&t.src),
        _ => None,
    };
    secs.and_then(|s| i64::try_from(s).ok()).ok_or_else(|| format!("`{key}` is a duration: `60s`, `10m`, `2h`, or seconds"))
}

/// What admission knows of a demand.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Demand {
    /// Its lane: repository, workflow and branch.
    pub lane: String,
    /// Its unit within the lane: the job, with its matrix values.
    pub unit: String,
    /// Its run's place in the lane: the newest is the highest.
    pub order: u64,
    pub coalesce: Option<Coalesce>,
    pub cadence: Option<Cadence>,
    /// It assumes what is ahead of it in its lane passes (W28-R68).
    pub speculative: bool,
}

/// Why a demand is held (W05-R13's list gains these, W28-R60).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum Held {
    /// Coalescing a burst while what it needs is saturated: until its quiet period ends, and never
    /// past `deadline`.
    Coalescing { lane: String, until: i64, deadline: i64 },
    /// On a cadence of `every`: not before `next`.
    Cadence { lane: String, next: i64 },
    /// Its lane's speculation window is full.
    Speculation { lane: String, size: u32, in_flight: u32 },
}

/// Why a demand was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// A newer demand of its lane and unit arrived while it was held to coalesce.
    Coalesced,
    /// A newer demand of its lane and unit took its place on the cadence.
    Cadence,
}

/// What admission decides.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Into the queues now.
    Admit { id: String },
    /// Held, and why.
    Hold { id: String, held: Held },
    /// Never started: `by`, newer, takes its place.
    Skip { id: String, by: String, why: Skipped },
    /// Wake admission at `at` ([`Admit::review`]).
    Timer { at: i64 },
}

#[derive(Debug, Clone, PartialEq)]
enum St {
    Held(Held),
    /// Admitted, not started.
    Pending,
    Running,
}

#[derive(Debug, Clone)]
struct Entry {
    d: Demand,
    st: St,
}

#[derive(Debug, Clone, Default)]
struct Slot {
    /// When the burst held now began.
    burst: Option<i64>,
    /// The demand held to coalesce.
    held: Option<String>,
    /// The demand on the cadence that hasn't started (held for `every`, or admitted).
    pending: Option<String>,
    last_start: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct Lane {
    size: u32,
    in_flight: u32,
}

/// Admission's state.
#[derive(Debug, Clone, Default)]
pub struct Admit {
    window: Window,
    demands: BTreeMap<String, Entry>,
    /// The demands held now.
    holds: BTreeSet<String>,
    slots: BTreeMap<(String, String), Slot>,
    lanes: BTreeMap<String, Lane>,
}

impl Admit {
    pub fn new(window: Window) -> Admit {
        Admit { window, ..Admit::default() }
    }

    /// Whether nothing is held: nothing to review.
    pub fn idle(&self) -> bool {
        self.holds.is_empty()
    }

    /// The demands held to coalesce while busy: the caller says which of them are saturated now.
    pub fn coalescing(&self) -> impl Iterator<Item = &str> {
        self.holds
            .iter()
            .filter(|k| self.demands.get(*k).is_some_and(|e| matches!(e.st, St::Held(Held::Coalescing { .. })) && !e.d.coalesce.is_some_and(|c| c.always)))
            .map(String::as_str)
    }

    /// Why a demand is held, if it is.
    pub fn held(&self, id: &str) -> Option<&Held> {
        match &self.demands.get(id)?.st {
            St::Held(h) => Some(h),
            _ => None,
        }
    }

    /// A lane's speculation window now, and how many of its speculative demands are in flight.
    pub fn window(&self, lane: &str) -> (u32, u32) {
        self.lanes.get(lane).map_or((self.window.initial.clamp(self.window.floor, self.window.cap), 0), |l| (l.size, l.in_flight))
    }

    /// A demand arrives; `saturated`: its resources would make it wait now.
    pub fn arrive(&mut self, id: &str, d: Demand, saturated: bool, now: i64) -> Vec<Decision> {
        let mut out = Vec::new();
        let key = (d.lane.clone(), d.unit.clone());
        if d.cadence.is_some() {
            let pending = self.slots.get(&key).and_then(|s| s.pending.clone()).filter(|p| p != id);
            if let Some(p) = pending {
                let older = self.demands.get(&p).is_some_and(|e| e.d.order < d.order);
                if older {
                    self.remove(&p);
                    out.push(Decision::Skip { id: p, by: id.to_owned(), why: Skipped::Cadence });
                } else {
                    // A newer one waits already: this one is skipped for it.
                    out.push(Decision::Skip { id: id.to_owned(), by: p, why: Skipped::Cadence });
                    return out;
                }
            }
        }
        self.demands.insert(id.to_owned(), Entry { d: d.clone(), st: St::Pending });
        self.holds.remove(id);
        if d.cadence.is_some() {
            self.slots.entry(key.clone()).or_default().pending = Some(id.to_owned());
        }
        if let Some(c) = d.coalesce {
            let held = self.slots.get(&key).and_then(|s| s.held.clone()).filter(|h| h != id);
            let mut joins = true;
            if let Some(h) = held {
                if self.demands.get(&h).is_some_and(|e| e.d.order < d.order) {
                    self.remove(&h);
                    out.push(Decision::Skip { id: h, by: id.to_owned(), why: Skipped::Coalesced });
                } else {
                    // Older than the one held (asked for since): it goes on its own.
                    joins = false;
                }
            }
            if joins && (saturated || c.always) {
                let slot = self.slots.entry(key.clone()).or_default();
                let start = slot.burst.unwrap_or(now);
                let deadline = start.saturating_add(c.max_wait);
                let until = now.saturating_add(c.quiet).min(deadline);
                if until > now {
                    slot.burst = Some(start);
                    slot.held = Some(id.to_owned());
                    let held = Held::Coalescing { lane: d.lane.clone(), until, deadline };
                    self.set(id, St::Held(held.clone()));
                    out.push(Decision::Hold { id: id.to_owned(), held });
                    out.push(Decision::Timer { at: until });
                    return out;
                }
            }
            if joins && let Some(slot) = self.slots.get_mut(&key) {
                slot.burst = None;
                slot.held = None;
            }
        }
        self.after_coalescing(id, now, &mut out);
        out
    }

    /// Releases what may go now: holds whose time came, coalescing holds whose resources aren't
    /// saturated any more (`unsaturated`), and speculative demands the window has room for.
    pub fn review(&mut self, now: i64, unsaturated: &dyn Fn(&str) -> bool) -> Vec<Decision> {
        let mut out = Vec::new();
        let mut held: Vec<(u64, String)> = self.holds.iter().filter_map(|k| self.demands.get(k).map(|e| (e.d.order, k.clone()))).collect();
        held.sort();
        for (_, id) in held {
            let Some(e) = self.demands.get(&id) else { continue };
            match e.st.clone() {
                St::Held(Held::Coalescing { until, .. }) => {
                    let busy_only = !e.d.coalesce.is_some_and(|c| c.always);
                    if until <= now || (busy_only && unsaturated(&id)) {
                        let key = (e.d.lane.clone(), e.d.unit.clone());
                        if let Some(slot) = self.slots.get_mut(&key)
                            && slot.held.as_deref() == Some(id.as_str())
                        {
                            slot.burst = None;
                            slot.held = None;
                        }
                        self.set(&id, St::Pending);
                        self.after_coalescing(&id, now, &mut out);
                    }
                }
                St::Held(Held::Cadence { next, .. }) => {
                    if next <= now {
                        self.set(&id, St::Pending);
                        self.after_cadence(&id, &mut out);
                    }
                }
                St::Held(Held::Speculation { lane, .. }) => {
                    let (size, in_flight) = self.window(&lane);
                    if in_flight < size {
                        self.set(&id, St::Pending);
                        self.after_cadence(&id, &mut out);
                    }
                }
                St::Pending | St::Running => {}
            }
        }
        out
    }

    /// A demand got its runner.
    pub fn started(&mut self, id: &str, now: i64) {
        self.holds.remove(id);
        let Some(e) = self.demands.get_mut(id) else { return };
        e.st = St::Running;
        let key = (e.d.lane.clone(), e.d.unit.clone());
        if e.d.cadence.is_some()
            && let Some(slot) = self.slots.get_mut(&key)
        {
            if slot.pending.as_deref() == Some(id) {
                slot.pending = None;
            }
            slot.last_start = Some(now);
        }
    }

    /// A demand ended: passed (`Some(true)`), failed (`Some(false)`), or neither (cancelled,
    /// skipped, gone). Its lane's window learns from a speculative one's outcome, and what it held
    /// back may go.
    pub fn ended(&mut self, id: &str, passed: Option<bool>, now: i64) -> Vec<Decision> {
        let Some(e) = self.demands.get(id) else { return Vec::new() };
        let (speculative, lane, admitted) = (e.d.speculative, e.d.lane.clone(), matches!(e.st, St::Pending | St::Running));
        self.remove(id);
        if speculative {
            let w = self.window;
            let initial = w.initial.clamp(w.floor, w.cap);
            let l = self.lanes.entry(lane).or_insert(Lane { size: initial, in_flight: 0 });
            if admitted {
                l.in_flight = l.in_flight.saturating_sub(1);
            }
            match passed {
                Some(true) => l.size = l.size.saturating_add(1).min(w.cap),
                Some(false) => l.size = (l.size / 2).max(w.floor),
                None => {}
            }
        }
        self.review(now, &|_| false)
    }

    /// Goes on from coalescing: the cadence's interval, then the window.
    fn after_coalescing(&mut self, id: &str, now: i64, out: &mut Vec<Decision>) {
        let Some(e) = self.demands.get(id) else { return };
        if let Some(Cadence::Every(gap)) = e.d.cadence {
            let key = (e.d.lane.clone(), e.d.unit.clone());
            let lane = e.d.lane.clone();
            if let Some(last) = self.slots.get(&key).and_then(|s| s.last_start) {
                let next = last.saturating_add(gap);
                if next > now {
                    let held = Held::Cadence { lane, next };
                    self.set(id, St::Held(held.clone()));
                    out.push(Decision::Hold { id: id.to_owned(), held });
                    out.push(Decision::Timer { at: next });
                    return;
                }
            }
        }
        self.after_cadence(id, out);
    }

    /// The window, then admitted.
    fn after_cadence(&mut self, id: &str, out: &mut Vec<Decision>) {
        let Some(e) = self.demands.get(id) else { return };
        if e.d.speculative {
            let lane = e.d.lane.clone();
            let (size, in_flight) = self.window(&lane);
            if in_flight >= size {
                let held = Held::Speculation { lane, size, in_flight };
                self.set(id, St::Held(held.clone()));
                out.push(Decision::Hold { id: id.to_owned(), held });
                return;
            }
            let w = self.window;
            let initial = w.initial.clamp(w.floor, w.cap);
            self.lanes.entry(lane).or_insert(Lane { size: initial, in_flight: 0 }).in_flight += 1;
        }
        self.set(id, St::Pending);
        out.push(Decision::Admit { id: id.to_owned() });
    }

    fn set(&mut self, id: &str, st: St) {
        if let Some(e) = self.demands.get_mut(id) {
            if matches!(st, St::Held(_)) {
                self.holds.insert(id.to_owned());
            } else {
                self.holds.remove(id);
            }
            e.st = st;
        }
    }

    fn remove(&mut self, id: &str) {
        self.holds.remove(id);
        let Some(e) = self.demands.remove(id) else { return };
        let key = (e.d.lane, e.d.unit);
        if let Some(slot) = self.slots.get_mut(&key) {
            if slot.held.as_deref() == Some(id) {
                slot.held = None;
            }
            if slot.pending.as_deref() == Some(id) {
                slot.pending = None;
            }
            if slot.held.is_none() && slot.pending.is_none() && slot.last_start.is_none() && slot.burst.is_none() {
                self.slots.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests;
