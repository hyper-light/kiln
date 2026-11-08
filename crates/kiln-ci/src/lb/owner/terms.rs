//! The per-ask terms (docs/lb.md "Per-ask terms"), W24-R18: an ask's expected finish on a
//! runner is its queue wait, the transfer of what isn't there, its CPU time at the runner's
//! per-core speed and slowness, its I/O at the disk's rate and its network bytes at the runner's
//! network rate, and, for a run that streams to a person, the link's latency and what its drops
//! are expected to cost. The choice reads the orders the ask's terms make relevant, a member each
//! in turn, and the shell's short list of runners it knows more of, until no member left unread
//! can beat the soonest seen by more than 1/64 (Fagin, Lotem and Naor's threshold): within 1/64
//! of the soonest of all.

use super::orders::{DISK, LINK, M, NARROW, NET, mutant};
use super::*;
use crate::lb::order::{class_hi, class_of};

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

/// Which terms are compared: a term counts only where every member that could be chosen has its
/// rate measured (every member with a whole core free, in its order's classes above 0, and every
/// listed candidate), so a runner not yet measured is neither favoured nor shut out by it, and
/// what is compared is the same whatever the walk has seen.
#[derive(Debug, Clone, Copy)]
struct On {
    disk: bool,
    net: bool,
    link: bool,
}

/// Where a candidate came from: the index (its bucket), an order, or the shell's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Index(u32),
    Order(usize),
    Listed { wait: u64, saved: u64, missing: u64 },
}

/// A candidate the threshold walk has seen: its expected finish, its runner, where it came from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seen {
    e: u128,
    r: u32,
    src: Source,
}

/// The index, as a list the threshold walk reads (its place in the walk's lists).
const IX: usize = usize::MAX;

/// One list the threshold walk reads in its order, member by member: an order (by its class) or
/// the index (by its bucket, down to 1: members with a whole core free). `at` is the class or
/// bucket of the next member not yet seen (NIL: none left), `next` that member, `first` the first
/// one this class or bucket gave, so its round is known; `took` the class or bucket of the member
/// it gave last.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    list: usize,
    at: u32,
    first: u32,
    next: u32,
    took: u32,
}

/// The latest finish a candidate may have and be within 1/64 of every member's, seen or not:
/// those not seen are at least the threshold `tau`, the soonest seen is `best`.
#[inline]
fn within_bound(tau: u128, best: u128) -> u128 {
    let b = tau.min(best);
    b.saturating_add(b >> ROUND)
}

/// Keeps a candidate the walk has seen: the soonest so far, and every one when a shared owner
/// draws among them.
#[inline]
fn keep(best: &mut Seen, seen: &mut Vec<Seen>, spread: bool, x: Seen) {
    if x.e < best.e {
        *best = x;
    }
    if spread {
        seen.push(x);
    }
}

/// The threshold's rounding, 2^-6: it stops once the best seen is within 1/64 of what no member
/// not yet seen can beat ([`Owner::ask_with`]). 1/64 is the orders' resolution: a class is a
/// rate's top bit and the six below it ([`class_of`]), so a class's rates are within 1/64 of each
/// other ((65 + m) / (64 + m) at most) and its members aren't ordered within it.
const ROUND: u32 = 6;

impl Owner {
    /// Decides an ask with what bears on its finish (docs/lb.md "Per-ask terms"). The expected
    /// finish is a sum of terms, each the ask's volume times a runner's rate (or its CPU time on
    /// its cores), and each order lists the members by one of those rates: Fagin, Lotem and
    /// Naor's threshold algorithm ("Optimal Aggregation Algorithms for Middleware", JCSS 66(4),
    /// 2003, §4, §6.2) reads the orders the ask's terms make relevant one member each in turn
    /// (by CPU, the narrow order and for a wide ask the index; by disk for I/O; by this owner's
    /// link for bytes to move; by the runner's network for network bytes), works out each
    /// member's finish, and stops once the soonest seen is no later than the threshold, the sum
    /// of what each term can be at least for a member no order has reached yet, times 1 + 1/64:
    /// within 1/64 of the soonest of every member with a whole core free (Theorem 6.6), and no
    /// algorithm that reads the orders reads asymptotically fewer (Theorem 6.7). 1/64 is the
    /// orders' own resolution: within a class rates aren't ordered, so a finer bound would read
    /// classes whole. No fixed count from each order bounds it: a member second in two orders,
    /// first in none, can beat both orders' firsts by any factor. The shell's list (`warm`) is
    /// read first, each with its saving, its missing bytes, and without room its wait; a run
    /// whose soonest is a listed runner without room waits there alone. With no term but its
    /// parallelism the ask is decided by its order alone; with none at all it is [`Owner::ask`],
    /// the same instructions.
    pub fn ask_with(&mut self, sig: SigId, need: Amounts, terms: &Terms, out: &mut Out) -> Result<Asked, Refused> {
        let cpu = need[0];
        let p = if terms.parallel == 0 { cpu } else { terms.parallel };
        let wide = p > cpu || mutant(M::NoNarrow);
        if !terms.costs() {
            if !wide || !self.ordered {
                return self.ask(sig, need, out);
            }
            return self.ask_most(sig.0, need, out);
        }
        self.flush();
        self.clean();
        let si = sig.0;
        let others = !mutant(M::OneCandidate);
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
        // The terms compared: those every member with a core free (none in its order's class 0,
        // a rate below 2 bytes a second) and every listed candidate has measured.
        let on = {
            let order = |o: usize| self.orders_on && self.sigs.get(si as usize).and_then(|s| s.orders.get(o)).is_some_and(|x| !x.has(0));
            let mut on = On { disk: order(DISK), net: order(NET), link: order(LINK) };
            for (r, _) in listed.iter().filter_map(|w| listed_at(self, w)) {
                let c = self.cold.get(r as usize).copied().unwrap_or_default();
                on.disk &= class_of(c.disk) != 0;
                on.net &= class_of(c.net) != 0;
                on.link &= class_of(c.link.rate) != 0;
            }
            on
        };
        let spread = self.width > 1;
        let mut seen = std::mem::take(&mut self.seen);
        seen.clear();
        let mut best = Seen { e: u128::MAX, r: NIL, src: Source::Index(0) };
        for w in listed {
            if let Some((r, f)) = listed_at(self, w)
                && let Source::Listed { wait, saved, missing } = f
            {
                keep(&mut best, &mut seen, spread, Seen { e: self.finish(r, terms, cpu, p, wide, on, wait, saved, missing), r, src: f });
            }
        }
        // The lists: by CPU, the narrow order on measured runners and, for a wide ask (or none
        // measured), the index; then the orders of the byte terms compared. On an unmeasured
        // fleet a narrow ask's CPU time is the same everywhere: the index lists the members only
        // when no other order does.
        let mut lists = [Cursor { list: 0, at: NIL, first: NIL, next: NIL, took: NIL }; 5];
        let mut n = 0usize;
        let byte_orders = [(DISK, terms.io_bytes, on.disk), (LINK, terms.bytes, on.link), (NET, terms.net_bytes, on.net)];
        let any_bytes = others && byte_orders.iter().any(|(_, v, on)| *v != 0 && *on);
        let wanted = [(IX, wide || (!self.ordered && !any_bytes)), (NARROW, self.ordered)].into_iter().chain(byte_orders.iter().map(|(o, v, on)| (*o, others && *v != 0 && *on)));
        for (list, want) in wanted {
            if want && let Some(x) = lists.get_mut(n) {
                *x = self.cursor(si, list);
                n += 1;
            }
        }
        let lists = lists.get_mut(..n).unwrap_or(&mut []);
        let (mut walked, mut read) = (0u64, 0u64);
        let mut tau: u128;
        loop {
            tau = self.threshold(lists, terms, p, wide);
            if (best.e <= tau.saturating_add(tau >> ROUND) && (!spread || self.drawn_from(&seen, tau, best.e))) || (!others && best.r != NIL) {
                break;
            }
            let mut any = false;
            for c in lists.iter_mut() {
                let Some(r) = self.take(si, c) else { continue };
                any = true;
                read += 1;
                if !self.fits_at(r, &need) {
                    walked += 1;
                    continue;
                }
                let src = if c.list == IX { Source::Index(c.took) } else { Source::Order(c.list) };
                keep(&mut best, &mut seen, spread, Seen { e: self.finish(r, terms, cpu, p, wide, on, 0, 0, terms.bytes), r, src });
                if !others {
                    break;
                }
            }
            if !any {
                break;
            }
        }
        self.counts.walked = self.counts.walked.saturating_add(walked);
        self.counts.read = self.counts.read.saturating_add(read);
        if best.r == NIL {
            self.seen = seen;
            // No member with a core free has room (nor a listed one): a need below a core may
            // still fit one with less.
            let r = self.pick_cpu(si, &need, !multi(&need));
            return if r == NIL { self.ask_waits(si, need, out) } else { self.start_at(si, need, r) };
        }
        // A shared owner draws among the seen within the bound of the soonest: each is within
        // 1/64 of every member's finish, seen or not.
        let chosen = if spread {
            let bound = within_bound(tau, best.e);
            let m = seen.iter().filter(|x| x.e <= bound).count();
            let k = self.draw_below(u32::try_from(m).unwrap_or(u32::MAX).min(self.width).max(1));
            seen.iter().filter(|x| x.e <= bound).nth(k as usize).copied().unwrap_or(best)
        } else {
            best
        };
        self.seen = seen;
        let Seen { r, src: f, .. } = chosen;
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

    /// Whether a shared owner has seen enough to draw: `width` candidates within the bound of the
    /// soonest, or none not yet seen can be (every one is at least the threshold, past 1 + 1/64
    /// of the soonest seen).
    fn drawn_from(&self, seen: &[Seen], tau: u128, best: u128) -> bool {
        if tau > best.saturating_add(best >> ROUND) {
            return true;
        }
        let bound = within_bound(tau, best);
        seen.iter().filter(|x| x.e <= bound).count() >= self.width as usize
    }

    /// A list's cursor at its first member: an order's highest class, or the index's highest
    /// bucket above 0.
    fn cursor(&self, si: u32, list: usize) -> Cursor {
        let none = Cursor { list, at: NIL, first: NIL, next: NIL, took: NIL };
        let Some(s) = self.sigs.get(si as usize) else { return none };
        if list == IX {
            return match s.ix.top().filter(|b| *b != 0) {
                Some(b) => {
                    let r = s.ix.next_in(b);
                    Cursor { list, at: b, first: r, next: r, took: NIL }
                }
                None => none,
            };
        }
        match s.orders.get(list).and_then(|o| o.top().map(|c| (c, o.head(c)))) {
            Some((c, r)) => Cursor { list, at: c, first: r, next: r, took: NIL },
            None => none,
        }
    }

    /// The next member a list gives (None: none left), and its cursor on to the one after: the
    /// rest of its class's ring or its bucket's turn, then the next class or bucket down. The
    /// class or bucket it gave it from is left in `took`.
    fn take(&self, si: u32, c: &mut Cursor) -> Option<u32> {
        if c.at == NIL {
            return None;
        }
        let r = c.next;
        let s = self.sigs.get(si as usize)?;
        c.took = c.at;
        if c.list == IX {
            let n = s.ix.after_in(c.at, r);
            if n == c.first || n == NIL {
                match s.ix.below(c.at).filter(|b| *b != 0) {
                    Some(b) => {
                        let m = s.ix.next_in(b);
                        *c = Cursor { at: b, first: m, next: m, ..*c };
                    }
                    None => c.at = NIL,
                }
            } else {
                c.next = n;
            }
        } else {
            let o = s.orders.get(c.list)?;
            let n = o.after(r);
            if n == c.first || n == NIL {
                match o.below(c.at) {
                    Some(k) => {
                        let m = o.head(k);
                        *c = Cursor { at: k, first: m, next: m, ..*c };
                    }
                    None => c.at = NIL,
                }
            } else {
                c.next = n;
            }
        }
        Some(r)
    }

    /// The threshold: the least expected finish a member no list has reached yet can have (its
    /// terms' sum, each at the bound its list's place gives; latency, stability and waits at 0).
    /// u128::MAX when a list has none left: every member with a core free has been seen.
    fn threshold(&self, lists: &[Cursor], t: &Terms, p: u64, wide: bool) -> u128 {
        let p = p.max(1);
        // The cores a run gets times its runner's key multiplier, at most: its parallelism times
        // the narrow order's bound (every multiplier is the reference's when none is measured),
        // and for a wide one the core-equivalents the index's bucket bounds ((b + 1) << 40, the
        // key's shift; the top bucket bounds nothing when keys may pass it).
        let mut cap = if self.ordered { u128::MAX } else { u128::from(p) * u128::from(SCALE) };
        let mut tau = 0u128;
        for c in lists {
            if c.at == NIL {
                return u128::MAX;
            }
            match c.list {
                IX => {
                    if wide && (c.at < self.last || self.buckets < BUCKETS) {
                        cap = cap.min((u128::from(c.at) + 1) << 40);
                    }
                }
                NARROW => cap = cap.min(u128::from(p) * u128::from(class_hi(c.at).min(u64::from(u32::MAX)))),
                o => {
                    let v = match o {
                        DISK => t.io_bytes,
                        LINK => t.bytes,
                        _ => t.net_bytes,
                    };
                    tau = tau.saturating_add(mul_q32(u128::from(v), per_byte_q32(class_hi(c.at))));
                }
            }
        }
        let work = u128::from(t.cpu_ns) * u128::from(CPU) * u128::from(SCALE);
        tau.saturating_add(work / cap.max(1))
    }

    /// The expected finish on runner `r`, ns: its wait, the transfer of `missing` bytes over the
    /// owner's link, the run's CPU time less `saved` on the cores it gets there (its parallelism,
    /// and for a wide run no more than are free) at the runner's per-core speed and slowness, its
    /// I/O at the disk's rate and its network bytes at the runner's network rate; for a run that
    /// streams to a person, the link's latency and the expected cost of its drops too. Each byte
    /// rate is a multiplier worked out when it was measured ([`Rates`]): a multiply a term; the CPU
    /// time divides once, by the cores the run gets times the runner's key multiplier.
    #[allow(clippy::too_many_arguments)]
    fn finish(&self, r: u32, t: &Terms, cpu: u64, p: u64, wide: bool, on: On, wait: u64, saved: u64, missing: u64) -> u128 {
        let Some(pe) = self.peers.get(r as usize) else { return u128::MAX };
        let x = self.cold.get(r as usize).map_or(Rates::NONE, |c| c.rates);
        let k = if wide { p.min(u64::from(pe.view).max(cpu)) } else { p };
        let scale = if mutant(M::NoSlowness) { self.cold.get(r as usize).map_or(SCALE, |c| scale_of(c.speed, 0)) } else { pe.scale };
        let saved = if mutant(M::NoWarm) { 0 } else { saved };
        let t_cpu = cpu_time(t.cpu_ns.saturating_sub(saved), k, scale);
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

    /// A plain wide ask on measured runners: one lookup in the core-equivalents order, the
    /// index, whose keys fold each runner's speed and slowness: its top's next in turn, checked
    /// exactly (CPU alone on the fast path), a walk when that one is short.
    fn ask_most(&mut self, si: u32, need: Amounts, out: &mut Out) -> Result<Asked, Refused> {
        #[cfg(test)]
        let planted = crate::lb::any_planted();
        #[cfg(not(test))]
        let planted = false;
        if !multi(&need)
            && !planted
            && let Ok(cpu) = u32::try_from(need[0])
            && let Some((r, run, epoch)) = self.ask_top(si, cpu)
        {
            return Ok(Asked { run, send: Some((RunnerId(r), ToRunner::Start(Start { owner: self.id, run, epoch, need }))) });
        }
        self.flush();
        let r = self.pick(si, &need, !multi(&need));
        if r == NIL {
            return self.ask_waits(si, need, out);
        }
        self.start_at(si, need, r)
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

/// What a decision multiplies by for each of a runner's byte rates, worked out when one is
/// measured (as the key's multiplier is), so a decision divides by none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rates {
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
    /// A runner nothing is measured of.
    pub const NONE: Rates = Rates { disk: 0, net: 0, link: 0, drops: 0 };

    pub(crate) fn of(disk: u64, net: u64, link: &Link) -> Rates {
        let drops = (1u128 << 96).checked_div(u128::from(link.between_drops_ns)).unwrap_or(0);
        Rates { disk: per_byte_q32(disk), net: per_byte_q32(net), link: per_byte_q32(link.rate), drops }
    }
}

/// A run's CPU time on a runner, ns: `work` ns of a reference core on `cores` thousandths of a
/// core at key multiplier `scale` (SCALE x speed / slowness), W x CPU x SCALE / (cores x scale).
/// One divide, monotone in the cores and the multiplier, so a bound on their product bounds it.
#[inline]
pub(crate) fn cpu_time(work: u64, cores: u64, scale: u32) -> u128 {
    (u128::from(work) * u128::from(CPU) * u128::from(SCALE)) / (u128::from(cores.max(1)) * u128::from(scale.max(1)))
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
