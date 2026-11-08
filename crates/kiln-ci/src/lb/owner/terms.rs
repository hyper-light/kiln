//! The per-ask terms (docs/lb.md "Per-ask terms"), W24-R18: an ask's expected finish on a
//! runner is its queue wait, the transfer of what isn't there, its CPU time at the runner's
//! per-core speed and slowness, its I/O at the disk's rate and its network bytes at the runner's
//! network rate, and, for a run that streams to a person, the link's latency and what its drops
//! are expected to cost. Candidates come from the top of each order the ask's terms make
//! relevant, and from the shell's short list of runners it knows more of: O(orders + listed),
//! each compared on its expected finish.

use super::orders::{DISK, LINK, M, NARROW, NET, mutant};
use super::*;

/// Nanoseconds a second: rates are bytes a second, times are ns.
pub(crate) const NS: u128 = 1_000_000_000;

/// A runner the shell knows something particular of for one ask (W24-R18).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Warm {
    pub runner: RunnerId,
    /// The CPU time its warm state saves the run there (build outputs, images, a checkout), in
    /// ns of a reference core: the run's own unit, so it scales as the rest of its CPU time.
    pub saved_ns: u64,
    /// Bytes not there yet (a runner not listed lacks all of the ask's `bytes`).
    pub missing: u64,
    /// With no room there now: how long until it has room, ns. 0: a candidate only with room
    /// now. A run that waits for it waits there alone (W24-R18's wait for a busy warm runner).
    pub wait_ns: u64,
}

/// What bears on one ask's finish beyond its runners' words (the shell computes each from
/// history and its own measures; the kernel takes integers). The default has none: the ask is
/// a plain one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Terms<'a> {
    /// Its learned parallelism in thousandths of a core (0: not learned, its CPU need is). More
    /// than its CPU need makes it wide: it runs faster with free cores past what it asks.
    pub parallel: u64,
    /// Its learned CPU time, ns of a reference core uncontended.
    pub cpu_ns: u64,
    /// Bytes it reads and writes on disk, learned.
    pub io_bytes: u64,
    /// Bytes it moves over the runner's own network (fetches), learned.
    pub net_bytes: u64,
    /// Bytes to move over the owner's link to a runner that has none of them.
    pub bytes: u64,
    /// It streams to a person (a session, a live log): the link's latency and its stability
    /// count. Batch and background work doesn't wait on the link once it runs.
    pub interactive: bool,
    /// The runners with a known advantage or their own transfer: a short list.
    pub warm: &'a [Warm],
}

impl Terms<'_> {
    /// Whether any term bears on its finish (parallelism alone only chooses its order).
    pub fn costs(&self) -> bool {
        (self.cpu_ns | self.io_bytes | self.net_bytes | self.bytes) != 0 || self.interactive || !self.warm.is_empty()
    }
}

/// Which terms every compared candidate has measured: a term is compared only where it is known
/// for all of them, so a runner not yet measured is neither favoured nor shut out by it.
#[derive(Debug, Clone, Copy)]
struct On {
    disk: bool,
    net: bool,
    link: bool,
}

/// Where a candidate came from: the index (its bucket), an order, or the shell's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Index(u32),
    Order(usize),
    Listed { wait: u64, saved: u64, missing: u64 },
}

impl Owner {
    /// Decides an ask with what bears on its finish (docs/lb.md "Per-ask terms"): from the top
    /// of each order its terms make relevant (by CPU, wide or narrow; by disk for I/O; by this
    /// owner's link for bytes to move; by the runner's network for network bytes) and from the
    /// shell's list, the candidate whose expected finish is soonest, the first among equals in
    /// that order. A listed runner without room now is a candidate with its wait, and a run that
    /// chooses it waits there alone. With no term but its parallelism it is decided by its order
    /// alone; with none at all it is [`Owner::ask`], the same instructions.
    pub fn ask_with(&mut self, sig: SigId, need: Amounts, terms: &Terms, out: &mut Out) -> Result<Asked, Refused> {
        let cpu = need[0];
        let p = if terms.parallel == 0 { cpu } else { terms.parallel };
        let wide = p > cpu || mutant(M::NoNarrow);
        if !terms.costs() {
            if !wide || !self.ordered {
                return self.ask(sig, need, out);
            }
            self.flush();
            let (r, b) = self.find_wide(sig.0, &need, 0);
            if r == NIL {
                return self.ask_waits(sig.0, need, out);
            }
            if let Some(s) = self.sigs.get_mut(sig.0 as usize) {
                s.ix.took(b, r);
            }
            return self.start_at(sig.0, need, r);
        }
        self.flush();
        let si = sig.0;
        let skip = if self.width > 1 { self.draw() } else { 0 };
        let mut cands = [(NIL, Source::Index(0)); 6];
        let mut n = 0usize;
        let mut add = |c: &mut [(u32, Source); 6], r: u32, f: Source| {
            if r != NIL
                && let Some(x) = c.get_mut(n)
            {
                *x = (r, f);
                n += 1;
            }
        };
        // By CPU: a wide ask by the index (most core-equivalents) and, on measured runners, by
        // the narrow order for its parallelism (as fast a core as any with that many free); a
        // narrow one by the narrow order, the index when nothing is measured or none is in it.
        if wide || !self.ordered {
            let (r, b) = self.find_wide(si, &need, skip);
            add(&mut cands, r, Source::Index(b));
        }
        if self.ordered {
            let [_, m, d, g] = need;
            let r = self.find_in(si, NARROW, &[p.max(cpu), m, d, g], skip);
            if r != NIL {
                add(&mut cands, r, Source::Order(NARROW));
            } else if !wide {
                let (r, b) = self.find_wide(si, &need, skip);
                add(&mut cands, r, Source::Index(b));
            }
        }
        let others = !mutant(M::OneCandidate);
        if others && self.orders_on {
            for (o, bytes) in [(DISK, terms.io_bytes), (LINK, terms.bytes), (NET, terms.net_bytes)] {
                if bytes != 0 {
                    let r = self.find_in(si, o, &need, skip);
                    add(&mut cands, r, Source::Order(o));
                }
            }
        }
        let listed = if others { terms.warm } else { &[] };
        let listed_at = |o: &Owner, w: &Warm| -> Option<(u32, Source)> {
            let r = w.runner.0;
            let member = o.sigs.get(si as usize).is_some_and(|s| s.ix.contains(r));
            let covers = o.cold.get(r as usize).is_some_and(|c| need.iter().zip(&c.budget).all(|(n, b)| n <= b));
            if !member || !covers || o.peers.get(r as usize).is_none_or(|p| p.flags & UP == 0) {
                return None;
            }
            let wait = if o.fits_at(r, &need) || mutant(M::NoQueueWait) { 0 } else if w.wait_ns != 0 { w.wait_ns } else { return None };
            Some((r, Source::Listed { wait, saved: w.saved_ns, missing: w.missing }))
        };
        // The terms compared: those every candidate has measured.
        let mut on = On { disk: true, net: true, link: true };
        let mut any = false;
        let order_cands = cands.get(..n).unwrap_or(&[]);
        for r in order_cands.iter().map(|c| c.0).chain(listed.iter().filter_map(|w| listed_at(self, w).map(|c| c.0))) {
            any = true;
            let c = self.cold.get(r as usize).copied().unwrap_or_default();
            on.disk &= c.disk != 0;
            on.net &= c.net != 0;
            on.link &= c.link.rate != 0;
        }
        if !any {
            return self.ask_waits(si, need, out);
        }
        let mut best = (u128::MAX, NIL, Source::Index(0));
        for &(r, f) in order_cands {
            let e = self.finish(r, terms, cpu, p, wide, on, 0, 0, terms.bytes);
            if e < best.0 {
                best = (e, r, f);
            }
        }
        for w in listed {
            if let Some((r, f)) = listed_at(self, w)
                && let Source::Listed { wait, saved, missing } = f
            {
                let e = self.finish(r, terms, cpu, p, wide, on, wait, saved, missing);
                if e < best.0 {
                    best = (e, r, f);
                }
            }
        }
        let (_, r, f) = best;
        match f {
            Source::Index(b) => {
                if let Some(s) = self.sigs.get_mut(si as usize) {
                    s.ix.took(b, r);
                }
            }
            Source::Order(o) => {
                if let Some(x) = self.sigs.get_mut(si as usize).and_then(|s| s.orders.get_mut(o)) {
                    x.took(r);
                }
            }
            Source::Listed { wait, .. } if wait != 0 => return self.wait_at(si, need, r, out),
            Source::Listed { .. } => {}
        }
        self.start_at(si, need, r)
    }

    /// The expected finish on runner `r`, ns: its wait, the transfer of `missing` bytes over the
    /// owner's link, the run's CPU time less `saved` on the cores it gets there (its parallelism,
    /// and for a wide run no more than are free) at the runner's per-core speed and slowness, its
    /// I/O at the disk's rate and its network bytes at the runner's network rate; for a run that
    /// streams to a person, the link's latency and the expected cost of its drops too. Each rate
    /// is a multiplier worked out when it was measured ([`Rates`]): a multiply a term, and one
    /// divide by the cores the run gets.
    #[allow(clippy::too_many_arguments)]
    fn finish(&self, r: u32, t: &Terms, cpu: u64, p: u64, wide: bool, on: On, wait: u64, saved: u64, missing: u64) -> u128 {
        let Some(pe) = self.peers.get(r as usize) else { return u128::MAX };
        let x = self.cold.get(r as usize).map_or(Rates::NONE, |c| c.rates);
        let k = if wide { p.min(u64::from(pe.view).max(cpu)) } else { p }.max(1);
        let core = if mutant(M::NoSlowness) { self.cold.get(r as usize).map_or(ONE, |c| core_q32(c.speed, 0)) } else { x.core };
        let saved = if mutant(M::NoWarm) { 0 } else { saved };
        let work = t.cpu_ns.saturating_sub(saved);
        let per_core = match work.checked_mul(CPU) {
            Some(w) => u128::from(w / k),
            None => u128::from(work) * u128::from(CPU) / u128::from(k),
        };
        let t_cpu = mul_q32(per_core, core);
        let t_io = if on.disk && !mutant(M::NoDisk) { mul_q32(u128::from(t.io_bytes), x.disk) } else { 0 };
        let t_net = if on.net && !mutant(M::NoNet) { mul_q32(u128::from(t.net_bytes), x.net) } else { 0 };
        let t_move = if on.link && !mutant(M::NoTransfer) { mul_q32(u128::from(missing), x.link) } else { 0 };
        let wait = if mutant(M::NoQueueWait) { 0 } else { u128::from(wait) };
        let run = t_cpu.saturating_add(t_io).saturating_add(t_net);
        let mut e = wait.saturating_add(t_move).saturating_add(run);
        if t.interactive && on.link {
            let l = self.cold.get(r as usize).map_or_else(Link::default, |c| c.link);
            if !mutant(M::NoLatency) {
                e = e.saturating_add(u128::from(l.latency_ns));
            }
            if !mutant(M::NoStability) {
                e = e.saturating_add(stability(run, x.drops, l.outage_ns));
            }
        }
        e
    }

    /// Starts an ask on runner `r` (the pick made, its turn taken): as [`Owner::ask`]'s general
    /// path does once it has picked.
    fn start_at(&mut self, si: u32, need: Amounts, r: u32) -> Result<Asked, Refused> {
        let cpu_only = !multi(&need);
        let slow = if cpu_only { NIL } else { self.slow_for(&need)? };
        let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: si, runner: r, state: STARTING, slow }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        let epoch = self.deduct(r, &need, cpu_only);
        Ok(Asked { run, send: Some((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need }))) })
    }

    /// Sends an ask to wait at runner `r` alone, its expected finish the soonest though others
    /// have room: not in its signature's waiting runs, so another runner's room doesn't take it;
    /// if `r` declines it, it is placed again as any run is.
    fn wait_at(&mut self, si: u32, need: Amounts, r: u32, out: &mut Out) -> Result<Asked, Refused> {
        let epoch = self.peers.get(r as usize).map_or(0, |p| p.epoch());
        let mut sl = Slow::of(&need);
        let mut places = [NO_PLACE; PLACES];
        if let Some(x) = places.first_mut() {
            *x = Place { runner: r, at: Handle::NONE, epoch };
        }
        sl.places = places;
        let slow = self.slow.insert(sl).ok_or(Refused::Full)?.slot;
        let Some(run) = self.runs.insert(Run { cpu: need[0], at: Handle::NONE, sig: si, runner: NIL, state: WAITING, slow }) else {
            self.free_slow(slow);
            return Err(Refused::Full);
        };
        out.to.push((RunnerId(r), ToRunner::Wait(Start { owner: self.id, run, epoch, need })));
        self.counts.waited = self.counts.waited.saturating_add(1);
        Ok(Asked { run, send: None })
    }
}

/// 1 in units of 2^-32: the multipliers' unit.
pub(crate) const ONE: u64 = 1 << 32;

/// What a decision multiplies by for each of a runner's rates, worked out when one is measured
/// (as the key's multiplier is), so a decision divides by none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rates {
    /// ns there per ns of a reference core: slowness over speed, in 2^-32.
    pub core: u64,
    /// ns a byte at its disk's, its own network's and this owner's link's rates, in 2^-32 (0:
    /// not measured).
    pub disk: u64,
    pub net: u64,
    pub link: u64,
    /// Its link's drops a ns, in 2^-96 (0: none seen): 41 bits or more for any mean time
    /// between drops below a year (2^55 ns).
    pub drops: u128,
}

impl Default for Rates {
    fn default() -> Rates {
        Rates::NONE
    }
}

impl Rates {
    /// A runner nothing is measured of: the reference core.
    pub const NONE: Rates = Rates { core: ONE, disk: 0, net: 0, link: 0, drops: 0 };

    pub(crate) fn of(speed: u32, slowness: u32, disk: u64, net: u64, link: &Link) -> Rates {
        let drops = (1u128 << 96).checked_div(u128::from(link.between_drops_ns)).unwrap_or(0);
        Rates { core: core_q32(speed, slowness), disk: per_byte_q32(disk), net: per_byte_q32(net), link: per_byte_q32(link.rate), drops }
    }
}

/// ns there per ns of a reference core, in 2^-32: slowness over speed (1024 each, unmeasured).
pub(crate) fn core_q32(speed: u32, slowness: u32) -> u64 {
    let s = if speed == 0 { SPEED_ONE } else { speed };
    let w = if slowness == 0 { SPEED_ONE } else { slowness };
    u64::try_from((u128::from(w) << 32) / u128::from(s)).unwrap_or(u64::MAX)
}

/// ns a byte at `rate` bytes a second, in 2^-32 (0: not measured).
pub(crate) fn per_byte_q32(rate: u64) -> u64 {
    if rate == 0 { 0 } else { u64::try_from((NS << 32) / u128::from(rate)).unwrap_or(u64::MAX) }
}

/// `x` times a multiplier in 2^-32 (the most a u128 holds, past it).
pub(crate) fn mul_q32(x: u128, q: u64) -> u128 {
    x.checked_mul(u128::from(q)).map_or(u128::MAX, |v| v >> 32)
}

/// What an interactive run's person is expected to lose to its link's drops, ns: the chance of a
/// drop during the run times the mean outage. Drops are taken as a Poisson process at the
/// measured rate 1/`between` (Ross, Introduction to Probability Models, §5.3: the count in a
/// span T is Poisson with mean T/between), so the chance of one or more is 1 - e^(-T/between).
/// `drops` is that rate a ns in 2^-96 ([`Rates::drops`]).
pub(crate) fn stability(run: u128, drops: u128, outage: u64) -> u128 {
    if drops == 0 || outage == 0 {
        return 0;
    }
    // T / between in 2^-32 (a product past 2^128 is a run 2^32 times the mean time between
    // drops); past 64 the chance is 1 to within e^-64.
    let x = run.checked_mul(drops).map_or(u128::MAX, |v| v >> 64);
    let Some(x) = u64::try_from(x).ok().filter(|x| *x < 64 * ONE) else { return u128::from(outage) };
    let chance = ONE.saturating_sub(exp_neg_q32(x));
    (u128::from(outage) * u128::from(chance)) >> 32
}

/// e^(-x / 2^32) in units of 2^-32, within 2^-25 of it: e^-n by n factors of e^-1, times e^-f
/// for the fraction f as (e^-(f/16))^16. For g = f/16 below 1/16 the series of e^-g to its g^6
/// term is off by less than g^7/7! < 2^-39, summed by Horner's rule with each 1/k a multiplier;
/// four squarings take it to e^-f and multiply its error by 16. No divide.
pub(crate) fn exp_neg_q32(x: u64) -> u64 {
    /// e^-1 = 0.36787944117144233 in units of 2^-32, rounded.
    const E_INV: u64 = 1_580_030_169;
    /// 2^32 / k rounded up, for k = 2 to 6.
    const INV: [u64; 5] = [2_147_483_648, 1_431_655_766, 1_073_741_824, 858_993_460, 715_827_883];
    let (n, f) = (x >> 32, x & (ONE - 1));
    let g = f >> 4;
    let mut h = ONE;
    for inv in INV.iter().rev() {
        h = ONE - ((((g * inv) >> 32) * h) >> 32);
    }
    h = ONE - ((g * h) >> 32);
    let mut e = u128::from(h);
    for _ in 0..4 {
        e = (e * e) >> 32;
    }
    let mut e = u64::try_from(e).unwrap_or(ONE);
    for _ in 0..n {
        e = (e * E_INV) >> 32;
        if e == 0 {
            break;
        }
    }
    e
}
