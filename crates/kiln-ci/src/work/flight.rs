//! Steps in flight, shared (docs/ci.md W28-R72 to R74): when two legs reach the same memo step at
//! once, in the same pipeline or in different ones, one executes it and the others wait for it,
//! then take its recorded success from the ledger (W24-R07), as Go's `singleflight` and
//! Buildbarn's in-flight deduplication do.
//!
//! Pure: time comes in (unix ms), nothing is read or written here. The table is keyed by a step's
//! key (its weak key, within its repository) and the trust domain that executes it. An ask is
//! answered [`Answer::Execute`] (the asker holds the key's lease and runs the step) or
//! [`Answer::Wait`] (another holds it, or the asker is behind other waiters). The ledger is asked
//! first, by the caller: a recorded success is a hit and never reaches this table.
//!
//! - **One execution per key and domain.** A lease is held by one leg's step at a time; it's
//!   renewed while the step runs ([`Flight::renew`]) and lapses [`LEASE_TTL_MS`] after its last
//!   renewal, as when its runner is lost.
//! - **Waiters in order.** A waiter is kept in its own domain's queue while it keeps asking (a
//!   waiter not heard from in [`WAITER_TTL_MS`] has left). When the lease is let go, by success,
//!   failure or lapse, the first waiter in the queue takes it on its next ask; the rest wait on it.
//!   A success is recorded before the lease goes, so a waiter's next lookup is a hit: a failure is
//!   never anyone's result, only the next waiter's turn to execute.
//! - **Domains** (W24-R21, W28-R41): a waiter waits on an execution of its own domain, or of a
//!   domain it may read ([`Domain::may_read`]: an untrusted or person's leg on a trusted one), but
//!   only takes leases in its own. Protected legs share nothing: they always execute, alone.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::trust::Domain;

/// How long a lease lives past its last renewal: a runner renews every quarter of it.
pub const LEASE_TTL_MS: i64 = 60_000;
/// How long a waiter stays in its queue past its last ask.
pub const WAITER_TTL_MS: i64 = 90_000;

/// Who asks: one attempt of one leg's step, and what a waiter is told of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct Holder {
    pub run: String,
    /// The run's number, as people know it (`run #12`).
    pub number: i64,
    /// Its workflow's name or file.
    pub workflow: String,
    pub leg: String,
    pub step: u32,
    pub attempt: u32,
    pub runner: String,
}

impl Holder {
    /// The same step of the same attempt asks as the same holder.
    fn same(&self, other: &Holder) -> bool {
        self.leg == other.leg && self.step == other.step && self.attempt == other.attempt
    }

    /// `step 2 of run #12 (release, on fedora)`, for a waiter's log.
    pub fn said(&self) -> String {
        let runner = if self.runner.is_empty() { String::new() } else { format!(", on {}", self.runner) };
        let workflow = if self.workflow.is_empty() { String::new() } else { format!("{}{}", self.workflow, runner) };
        let around = if workflow.is_empty() { String::new() } else { format!(" ({workflow})") };
        format!("step {} of run #{}{around}", self.step, self.number)
    }
}

/// A key's lease: who executes it, since when, renewed when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub key: String,
    pub domain: Domain,
    pub holder: Holder,
    pub since: i64,
    pub renewed: i64,
}

/// What an ask gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    /// Execute the step under `lease`, renewing it while the step runs.
    Execute { lease: String },
    /// Wait: `holder` executes it (or, with `turn`, is next to), in `domain`, since `since`; `ahead`
    /// waiters are before the asker in its own queue.
    Wait { holder: Holder, domain: Domain, since: i64, ahead: usize, turn: bool },
    /// Protected legs share nothing (W24 decision 4, W28-R41): execute, without a lease.
    Alone,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Slot {
    lease: Option<Lease>,
    /// Waiters for a turn in this domain, oldest first, each with when it last asked.
    queue: VecDeque<(Holder, i64)>,
    /// Waiters from domains below that wait on this slot's execution, with when they last asked
    /// (they never take its lease).
    readers: Vec<(Holder, i64)>,
}

impl Slot {
    fn idle(&self) -> bool {
        self.lease.is_none() && self.queue.is_empty() && self.readers.is_empty()
    }
}

/// The table of steps in flight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flight {
    slots: BTreeMap<(String, Domain), Slot>,
    /// Leases made, for their ids.
    seq: u64,
}

impl Flight {
    pub fn new() -> Flight {
        Flight::default()
    }

    /// A lease kept before a restart, put back as it was (its holder renews it, or it lapses).
    pub fn restore(&mut self, lease: Lease) {
        if let Some(n) = lease.id.rsplit('-').next().and_then(|n| n.parse::<u64>().ok()) {
            self.seq = self.seq.max(n);
        }
        let slot = self.slots.entry((lease.key.clone(), lease.domain.clone())).or_default();
        slot.lease = Some(lease);
    }

    /// Every lease held, for keeping across a restart.
    pub fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.slots.values().filter_map(|s| s.lease.as_ref())
    }

    /// The lease `id`, if it's still held.
    pub fn lease(&self, id: &str) -> Option<&Lease> {
        self.leases().find(|l| l.id == id)
    }

    /// `who` asks to execute `key` in `domain`, the ledger having nothing for it.
    pub fn ask(&mut self, key: &str, domain: &Domain, who: &Holder, now: i64) -> Answer {
        self.expire(now);
        if domain.needs_two_runners() {
            return Answer::Alone;
        }
        // Its own domain first: a lease it holds, or one another holds.
        let own = (key.to_owned(), domain.clone());
        if let Some(slot) = self.slots.get_mut(&own)
            && let Some(l) = &slot.lease
        {
            if l.holder.same(who) {
                return Answer::Execute { lease: l.id.clone() };
            }
            let (holder, since) = (l.holder.clone(), l.since);
            let ahead = Self::queue_up(&mut slot.queue, who, now);
            return Answer::Wait { holder, domain: domain.clone(), since, ahead, turn: false };
        }
        // Then an execution of a domain above it that it may read.
        let above: Option<(String, Domain)> = self
            .slots
            .iter()
            .filter(|((k, d), s)| k == key && d != domain && s.lease.is_some() && domain.may_read(d))
            .max_by_key(|((_, d), _)| d.clone() == Domain::Trusted)
            .map(|(k, _)| k.clone());
        if let Some(at) = above
            && let Some(slot) = self.slots.get_mut(&at)
            && let Some(l) = &slot.lease
        {
            let (holder, since) = (l.holder.clone(), l.since);
            match slot.readers.iter_mut().find(|(h, _)| h.same(who)) {
                Some(r) => r.1 = now,
                None => slot.readers.push((who.clone(), now)),
            }
            // A reader that waited in its own queue keeps its place there.
            let ahead = self.slots.get(&own).and_then(|s| s.queue.iter().position(|(h, _)| h.same(who))).unwrap_or(0);
            return Answer::Wait { holder, domain: at.1, since, ahead, turn: false };
        }
        // Nobody executes it: the first waiter of its own domain takes the lease.
        let slot = self.slots.entry(own).or_default();
        if let Some(r) = slot.readers.iter().position(|(h, _)| h.same(who)) {
            slot.readers.remove(r);
        }
        match slot.queue.front() {
            Some((first, _)) if !first.same(who) => {
                let first = first.clone();
                let ahead = Self::queue_up(&mut slot.queue, who, now);
                Answer::Wait { holder: first, domain: domain.clone(), since: now, ahead, turn: true }
            }
            _ => {
                if slot.queue.front().is_some_and(|(h, _)| h.same(who)) {
                    slot.queue.pop_front();
                }
                self.seq += 1;
                let id = format!("lease-{}", self.seq);
                slot.lease = Some(Lease { id: id.clone(), key: key.to_owned(), domain: domain.clone(), holder: who.clone(), since: now, renewed: now });
                Answer::Execute { lease: id }
            }
        }
    }

    /// Puts `who` in `queue` (or notes it asked again): how many are before it.
    fn queue_up(queue: &mut VecDeque<(Holder, i64)>, who: &Holder, now: i64) -> usize {
        match queue.iter().position(|(h, _)| h.same(who)) {
            Some(i) => {
                if let Some(w) = queue.get_mut(i) {
                    w.1 = now;
                }
                i
            }
            None => {
                queue.push_back((who.clone(), now));
                queue.len().saturating_sub(1)
            }
        }
    }

    /// The holder of `lease` says it's still at it: whether it still holds it.
    pub fn renew(&mut self, lease: &str, now: i64) -> bool {
        self.expire(now);
        match self.slots.values_mut().find_map(|s| s.lease.as_mut().filter(|l| l.id == lease)) {
            Some(l) => {
                l.renewed = now;
                true
            }
            None => false,
        }
    }

    /// The holder of `lease` is done with it, however its step ended (a success recorded first):
    /// the lease it let go, if it held one.
    pub fn end(&mut self, lease: &str, now: i64) -> Option<Lease> {
        let slot = self.slots.values_mut().find(|s| s.lease.as_ref().is_some_and(|l| l.id == lease))?;
        let gone = slot.lease.take();
        self.expire(now);
        gone
    }

    /// `who` stopped waiting or executing (its leg ended): it leaves every queue, and lets go any
    /// lease it held.
    pub fn leave(&mut self, who: &Holder, now: i64) -> Vec<Lease> {
        let mut gone = Vec::new();
        for slot in self.slots.values_mut() {
            slot.queue.retain(|(h, _)| !h.same(who));
            slot.readers.retain(|(h, _)| !h.same(who));
            if slot.lease.as_ref().is_some_and(|l| l.holder.same(who)) {
                gone.extend(slot.lease.take());
            }
        }
        self.expire(now);
        gone
    }

    /// Leases not renewed and waiters not heard from in time are let go; empty slots forgotten.
    fn expire(&mut self, now: i64) {
        for slot in self.slots.values_mut() {
            if slot.lease.as_ref().is_some_and(|l| now.saturating_sub(l.renewed) > LEASE_TTL_MS) {
                slot.lease = None;
            }
            slot.queue.retain(|(_, at)| now.saturating_sub(*at) <= WAITER_TTL_MS);
            slot.readers.retain(|(_, at)| now.saturating_sub(*at) <= WAITER_TTL_MS);
        }
        self.slots.retain(|_, s| !s.idle());
    }

    /// The leases that lapsed by `now` (for the caller to forget them where it keeps them).
    pub fn lapsed(&self, now: i64) -> Vec<Lease> {
        self.leases().filter(|l| now.saturating_sub(l.renewed) > LEASE_TTL_MS).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn who(leg: &str, step: u32) -> Holder {
        Holder { run: format!("run-{leg}"), number: 1, workflow: "ci".into(), leg: leg.into(), step, attempt: 1, runner: "r".into() }
    }

    fn lease_of(a: &Answer) -> Option<String> {
        if let Answer::Execute { lease } = a { Some(lease.clone()) } else { None }
    }

    #[test]
    fn w28_r72_one_executes_the_other_waits_then_the_ledger_answers() {
        let mut f = Flight::new();
        let a = f.ask("k", &Domain::Trusted, &who("a", 0), 0);
        let lease = lease_of(&a).unwrap_or_default();
        assert!(!lease.is_empty(), "{a:?}");
        let b = f.ask("k", &Domain::Trusted, &who("b", 0), 1_000);
        assert!(matches!(&b, Answer::Wait { holder, ahead: 0, turn: false, .. } if holder.leg == "a"), "{b:?}");
        assert_eq!(f.ask("k", &Domain::Trusted, &who("a", 0), 2_000), a, "asking again keeps the lease");
        assert!(f.renew(&lease, 30_000));
        // Success: recorded first, so b's next lookup is a hit and never asks here again.
        assert!(f.end(&lease, 40_000).is_some());
        assert_eq!(f.leases().count(), 0);
    }

    #[test]
    fn w28_r73_a_failure_is_the_next_waiters_turn_in_order() {
        let mut f = Flight::new();
        let lease = lease_of(&f.ask("k", &Domain::Trusted, &who("a", 0), 0)).unwrap_or_default();
        f.ask("k", &Domain::Trusted, &who("b", 0), 1);
        f.ask("k", &Domain::Trusted, &who("c", 0), 2);
        f.end(&lease, 10);
        // c asks first, but b was first in line.
        let c = f.ask("k", &Domain::Trusted, &who("c", 0), 11);
        assert!(matches!(&c, Answer::Wait { holder, turn: true, ahead: 1, .. } if holder.leg == "b"), "{c:?}");
        let b = f.ask("k", &Domain::Trusted, &who("b", 0), 12);
        assert!(lease_of(&b).is_some(), "{b:?}");
        let c = f.ask("k", &Domain::Trusted, &who("c", 0), 13);
        assert!(matches!(&c, Answer::Wait { holder, ahead: 0, turn: false, .. } if holder.leg == "b"), "{c:?}");
    }

    #[test]
    fn w28_r73_a_lost_runner_lets_its_lease_lapse() {
        let mut f = Flight::new();
        f.ask("k", &Domain::Trusted, &who("a", 0), 0);
        f.ask("k", &Domain::Trusted, &who("b", 0), 1_000);
        // a's runner is lost: no renewal. b keeps asking.
        f.ask("k", &Domain::Trusted, &who("b", 0), 50_000);
        let b = f.ask("k", &Domain::Trusted, &who("b", 0), LEASE_TTL_MS + 1);
        assert!(lease_of(&b).is_some(), "{b:?}");
    }

    #[test]
    fn w28_r74_domains_wait_down_never_up_and_protected_shares_nothing() {
        let mut f = Flight::new();
        lease_of(&f.ask("k", &Domain::Trusted, &who("t", 0), 0)).unwrap_or_default();
        let u = f.ask("k", &Domain::Untrusted, &who("u", 0), 1);
        assert!(matches!(&u, Answer::Wait { domain: Domain::Trusted, .. }), "an untrusted leg waits on a trusted execution: {u:?}");
        let mut g = Flight::new();
        lease_of(&g.ask("k", &Domain::Untrusted, &who("u", 0), 0)).unwrap_or_default();
        let t = g.ask("k", &Domain::Trusted, &who("t", 0), 1);
        assert!(lease_of(&t).is_some(), "a trusted leg never waits on an untrusted one: {t:?}");
        assert_eq!(g.ask("k", &Domain::Protected, &who("p", 0), 2), Answer::Alone);
    }

    #[test]
    fn w28_r72_a_restart_keeps_leases() {
        let mut f = Flight::new();
        let lease = lease_of(&f.ask("k", &Domain::Trusted, &who("a", 0), 0)).unwrap_or_default();
        let kept: Vec<Lease> = f.leases().cloned().collect();
        let mut g = Flight::new();
        kept.into_iter().for_each(|l| g.restore(l));
        assert!(matches!(g.ask("k", &Domain::Trusted, &who("b", 0), 5), Answer::Wait { .. }));
        assert_eq!(lease_of(&g.ask("k", &Domain::Trusted, &who("a", 0), 6)), Some(lease.clone()));
        let next = lease_of(&g.ask("k2", &Domain::Trusted, &who("c", 0), 7)).unwrap_or_default();
        assert_ne!(next, lease, "lease ids go on from those restored");
    }

    #[derive(Debug, Clone)]
    enum Op {
        Ask { key: u8, domain: u8, leg: u8 },
        Renew { leg: u8 },
        End { leg: u8 },
        Leave { leg: u8 },
        Tick { ms: u32 },
        Restart,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0u8..3, 0u8..4, 0u8..6).prop_map(|(key, domain, leg)| Op::Ask { key, domain, leg }),
            2 => (0u8..6).prop_map(|leg| Op::Renew { leg }),
            1 => (0u8..6).prop_map(|leg| Op::End { leg }),
            1 => (0u8..6).prop_map(|leg| Op::Leave { leg }),
            2 => (0u32..40_000).prop_map(|ms| Op::Tick { ms }),
            1 => Just(Op::Restart),
        ]
    }

    fn domain(n: u8) -> Domain {
        match n {
            0 => Domain::Trusted,
            1 => Domain::Untrusted,
            2 => Domain::Person("ada".into()),
            _ => Domain::Protected,
        }
    }

    /// Runs `ops`, checking at every answer: one execution per key and domain at a time, an
    /// execution only under a lease the table holds for that holder, waits only on domains the
    /// waiter may read. Returns every answer, for determinism.
    fn run(ops: &[Op]) -> Result<Vec<String>, TestCaseError> {
        let mut f = Flight::new();
        let mut now = 0i64;
        // What each leg executes under: (key, domain, lease).
        let mut executing: BTreeMap<u8, (String, Domain, String)> = BTreeMap::new();
        let mut out = Vec::new();
        for o in ops {
            match o {
                Op::Ask { key, domain: d, leg } => {
                    let (k, d, h) = (format!("k{key}"), domain(*d), who(&format!("l{leg}"), 0));
                    if executing.get(leg).is_some_and(|(ek, ed, _)| *ek != k || *ed != d) {
                        continue;
                    }
                    let a = f.ask(&k, &d, &h, now);
                    match &a {
                        Answer::Execute { lease } => {
                            let held = f.lease(lease).cloned();
                            prop_assert!(held.as_ref().is_some_and(|l| l.holder.same(&h) && l.key == k && l.domain == d), "{a:?} {held:?}");
                            // Nobody else executes this key in this domain under a lease still held.
                            for (other, (ok, od, ol)) in &executing {
                                prop_assert!(!(other != leg && *ok == k && *od == d && f.lease(ol).is_some()), "two executions of {k} in {d:?}");
                            }
                            executing.insert(*leg, (k, d, lease.clone()));
                        }
                        Answer::Wait { domain: wd, holder, .. } => {
                            prop_assert!(*wd == d || d.may_read(wd), "{d:?} waits on {wd:?}");
                            prop_assert!(!holder.same(&h));
                        }
                        Answer::Alone => prop_assert!(d.needs_two_runners()),
                    }
                    out.push(format!("{a:?}"));
                }
                Op::Renew { leg } => {
                    if let Some((_, _, l)) = executing.get(leg) {
                        let held = f.renew(l, now);
                        if !held {
                            executing.remove(leg);
                        }
                    }
                }
                Op::End { leg } => {
                    if let Some((_, _, l)) = executing.remove(leg) {
                        f.end(&l, now);
                    }
                }
                Op::Leave { leg } => {
                    executing.remove(leg);
                    f.leave(&who(&format!("l{leg}"), 0), now);
                }
                Op::Tick { ms } => {
                    now += i64::from(*ms);
                    let live: Vec<String> = f.leases().filter(|l| now.saturating_sub(l.renewed) <= LEASE_TTL_MS).map(|l| l.id.clone()).collect();
                    executing.retain(|_, (_, _, l)| live.contains(l));
                }
                Op::Restart => {
                    let kept: Vec<Lease> = f.leases().cloned().collect();
                    f = Flight::new();
                    kept.into_iter().for_each(|l| f.restore(l));
                }
            }
            // At most one lease per key and domain, always.
            let mut seen = std::collections::BTreeSet::new();
            for l in f.leases() {
                prop_assert!(seen.insert((l.key.clone(), l.domain.clone())), "two leases for {} in {:?}", l.key, l.domain);
            }
        }
        Ok(out)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]
        /// W28-AC72: single-flight within domains, waits only on readable domains, deterministic.
        #[test]
        fn w28_ac72_single_flight_within_domains(ops in proptest::collection::vec(op(), 1..60)) {
            let first = run(&ops)?;
            let again = run(&ops)?;
            prop_assert_eq!(first, again);
        }
    }

    #[test]
    fn w28_ac72_the_first_waiter_executes_once_the_lease_is_let_go() {
        let mut f = Flight::new();
        let l = lease_of(&f.ask("k", &Domain::Trusted, &who("a", 0), 0)).unwrap_or_default();
        for (i, leg) in ["b", "c", "d"].iter().enumerate() {
            f.ask("k", &Domain::Trusted, &who(leg, 0), 1 + i as i64);
        }
        f.leave(&who("a", 0), 10);
        assert!(f.lease(&l).is_none());
        // Whoever asks first, b goes first, then c, then d.
        for (n, leg) in ["b", "c", "d"].iter().enumerate() {
            for other in ["d", "c"] {
                if other != *leg {
                    let _ = f.ask("k", &Domain::Trusted, &who(other, 0), 20 + n as i64);
                }
            }
            let mine = f.ask("k", &Domain::Trusted, &who(leg, 0), 21 + n as i64);
            let lease = lease_of(&mine).unwrap_or_default();
            assert!(!lease.is_empty(), "{leg}: {mine:?}");
            f.end(&lease, 22 + n as i64);
        }
    }
}
