//! The orders by measured rates (docs/lb.md "Rank", "Orders"): a signature's members by per-core
//! speed under contention (narrow asks), by disk, by this owner's link and by the runner's own
//! network, each a class a rate, kept as runners measure and links are measured; and the picks
//! from them.

use super::terms::Rates;
use super::*;
use crate::lb::order::class_of;

/// The orders a signature keeps beyond its index, by these rates: per-core speed under
/// contention (speed over slowness: narrow asks), the disk's, this owner's link's, and the
/// runner's own network's.
pub(super) const NARROW: usize = 0;
pub(super) const DISK: usize = 1;
pub(super) const LINK: usize = 2;
pub(super) const NET: usize = 3;
const ORDERS: usize = 4;
pub(super) const BYTES: u8 = (1 << DISK) | (1 << LINK) | (1 << NET);

/// A signature's orders, empty.
pub(super) fn orders(kept: u8) -> Vec<Order> {
    (0..(u8::BITS - kept.leading_zeros()) as usize).map(|_| Order::new()).collect()
}

#[cfg(test)]
mod cold_tests {
    use super::*;

    /// W05-R26: CPU-only measured ranking should not construct byte-rate indexes. An index
    /// becomes populated when a request actually needs it, without a TTL or size heuristic.
    #[test]
    fn w05_r26_cpu_measure_does_not_build_unused_byte_orders() -> Result<(), String> {
        let (mut owner, mut runners, sig) = crate::lb::tests::fleet(4, [8 * CPU, 0, 0, 0]);
        let runner = runners.first_mut().ok_or("runner")?;
        runner.measure(768, 1024);
        owner.hear(ToOwner::Hint { runner: RunnerId(0), word: runner.word() }, &mut Out::default());
        let orders = &owner.sigs.get(sig.0 as usize).ok_or("signature")?.orders;
        if orders.get(NARROW).and_then(Order::top).is_none() { return Err("CPU order missing".into()); }
        for index in [DISK, LINK, NET] {
            if orders.get(index).and_then(Order::top).is_some() { return Err(format!("unused byte order built: {index}")); }
        }
        let terms = Terms { io_bytes: 1, cpu_ns: 1, ..Terms::default() };
        owner.ask_with(sig, [CPU, 0, 0, 0], &terms, &mut Out::default()).map_err(|e| format!("I/O ask: {e:?}"))?;
        let orders = &owner.sigs.get(sig.0 as usize).ok_or("signature")?.orders;
        if orders.get(DISK).and_then(Order::top).is_none() { return Err("requested disk order missing".into()); }
        for index in [LINK, NET] {
            if orders.get(index).and_then(Order::top).is_some() { return Err(format!("unrequested byte order built: {index}")); }
        }
        Ok(())
    }

    /// W05-R26: first use reads current facts; later measurements and joins update the active
    /// order even when the CPU order is activated by the same word. Unused kinds stay empty.
    #[test]
    fn w05_r26_lazy_orders_follow_measurements_and_new_members() -> Result<(), String> {
        let budget = [8 * CPU, 0, 0, 0];
        let (mut owner, mut runners, sig) = crate::lb::tests::fleet(2, budget);
        let mut out = Out::default();
        let a = owner.ask_with(sig, [CPU, 0, 0, 0], &Terms { bytes: 1, ..Terms::default() }, &mut out).map_err(|e| format!("ask: {e:?}"))?;
        let (to, start) = a.send.ok_or("no start")?;
        let reply = runners.get_mut(to.0 as usize).ok_or("runner")?.hear(start, &mut RunnerOut::new()).ok_or("no reply")?;
        owner.hear(reply, &mut out);
        owner.set_link(RunnerId(1), Link { rate: 1 << 20, ..Link::default() });
        let link = owner.sigs.get(sig.0 as usize).and_then(|s| s.orders.get(LINK)).ok_or("link order")?;
        for r in 0..2 {
            if link.class(r) != class_of(1 << 20) { return Err(format!("new link measurement missing for {r}")); }
        }
        let r = runners.get_mut(1).ok_or("runner")?;
        r.measure(768, 1024);
        r.measure_rates(1 << 21, 1 << 20);
        owner.hear(ToOwner::Hint { runner: RunnerId(1), word: r.word() }, &mut out);
        let mut added = Runner::new(RunnerId(2), budget, 1);
        added.watch(OwnerId(0));
        owner.join(RunnerId(2), budget, added.word(), &mut out);
        owner.add(sig, RunnerId(2), &mut out);
        let fresh = owner.sig();
        owner.add(fresh, RunnerId(2), &mut out);
        for id in [sig, fresh] {
            let orders = &owner.sigs.get(id.0 as usize).ok_or("signature")?.orders;
            if orders.get(LINK).map(|o| o.class(2)) != Some(class_of(1 << 20)) { return Err("new member's link order missing".into()); }
            if orders.get(NARROW).map(|o| o.class(2)) != Some(class_of(u64::from(SCALE))) { return Err("new member's CPU order missing".into()); }
            for index in [DISK, NET] {
                if orders.get(index).and_then(Order::top).is_some() { return Err("unrequested byte order populated".into()); }
            }
        }
        Ok(())
    }
}

/// What the owner knows of its link to a runner: the shell's measure of its own transfers there
/// and of the link's drops (lb/wire). Owner-side, soft: set it when it is measured again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    /// Bytes a second the owner's transfers there get (0: not measured; then none of the link's
    /// terms is compared: docs/lb.md "Per-ask terms").
    pub rate: u64,
    /// A round trip, ns.
    pub latency_ns: u64,
    /// The mean time between its drops, ns: the reciprocal of their measured rate, so a rare
    /// drop keeps its resolution (0: none seen).
    pub between_drops_ns: u64,
    /// An outage's mean length, ns.
    pub outage_ns: u64,
}

#[cfg(test)]
pub(super) fn mutant(m: crate::lb::Mutant) -> bool {
    crate::lb::planted(m)
}

#[cfg(not(test))]
#[inline(always)]
pub(super) fn mutant(_: ()) -> bool {
    false
}

#[cfg(test)]
pub(super) use crate::lb::Mutant as M;

/// The mutants' names where they aren't compiled: each is `()`, never planted.
#[cfg(not(test))]
#[allow(non_snake_case, non_upper_case_globals)]
pub(super) mod M {
    pub const NoNarrow: () = ();
    pub const NoSlowness: () = ();
    pub const NoWarm: () = ();
    pub const NoTransfer: () = ();
    pub const NoDisk: () = ();
    pub const NoNet: () = ();
    pub const NoLatency: () = ();
    pub const NoStability: () = ();
    pub const NoQueueWait: () = ();
    pub const OneCandidate: () = ();
    pub const NoPrior: () = ();
    pub const NoGate: () = ();
}

impl Owner {
    /// The owner's link to runner `r`, as the shell measures its own transfers there and the
    /// link's drops (docs/lb.md "Per-ask terms"). Its rate orders the runners for asks with bytes
    /// to move.
    pub fn set_link(&mut self, r: RunnerId, link: Link) {
        self.flush();
        let i = r.0 as usize;
        if self.cold.len() <= i {
            // Before it joins: kept for when it does.
            self.peers.resize(i + 1, PEER);
            self.cold.resize(i + 1, Cold::default());
            self.more.resize(i + 1, Vec::new());
        }
        if let Some(c) = self.cold.get_mut(i) {
            c.link = link;
        }
        self.rerate(r.0);
        if self.orders_kept & (1 << LINK) != 0 {
            self.set_orders(r.0, [false, false, true, false]);
        }
    }

    /// Works out runner `r`'s multipliers again from what it measured and its link.
    pub(super) fn rerate(&mut self, r: u32) {
        let i = r as usize;
        let Some(c) = self.cold.get_mut(i) else { return };
        let (was, now) = (c.rates, Rates::of(c.disk, c.net, &c.link));
        c.rates = now;
        let before = self.prior.classes();
        self.prior.disk.moved(was.disk, now.disk);
        self.prior.net.moved(was.net, now.net);
        self.prior.link.moved(was.link, now.link);
        // A runner not measured sits where the prior does: its class moves with it.
        if self.orders_kept & BYTES != 0 && self.prior.classes() != before {
            self.refile_unmeasured(r);
        }
    }

    /// The prior's class moved: the runners that take it (all but `r`, which its caller syncs) are
    /// filed again. O(runners), only when a mean crosses a class.
    #[cold]
    #[inline(never)]
    fn refile_unmeasured(&mut self, r: u32) {
        for x in 0..u32::try_from(self.cold.len()).unwrap_or(NIL) {
            let c = self.cold.get(x as usize).copied().unwrap_or_default();
            if x != r && (c.disk == 0 || c.net == 0 || c.link.rate == 0) {
                self.set_orders(x, [false, c.disk == 0, c.link.rate == 0, c.net == 0]);
            }
        }
    }

    /// The owner's link to runner `r` as last set.
    pub fn link(&self, r: RunnerId) -> Link {
        self.cold.get(r.0 as usize).map_or_else(Link::default, |c| c.link)
    }

    /// W05-R26: activate only the requested order kinds, from current membership and facts.
    /// One bit per real order kind, no eviction, timeout or size threshold. New kinds are built
    /// once; subsequent words maintain those kinds through the existing event paths.
    pub(super) fn keep_orders(&mut self, wanted: [bool; ORDERS]) {
        let mask = wanted.iter().enumerate().fold(0u8, |mask, (i, wanted)| mask | (u8::from(*wanted) << i));
        let missing = mask & !self.orders_kept;
        if missing == 0 { return; }
        self.flush();
        self.orders_kept |= missing;
        let len = (u8::BITS - self.orders_kept.leading_zeros()) as usize;
        for s in &mut self.sigs {
            s.orders.resize_with(len, Order::new);
        }
        let only = std::array::from_fn(|i| missing & (1 << i) != 0);
        for r in 0..u32::try_from(self.peers.len()).unwrap_or(NIL) {
            self.set_orders(r, only);
        }
    }

    /// Runner `r`'s word said something it measured changed: its classes move, and the first
    /// non-reference CPU measure activates the narrow order; byte orders follow their first ask.
    pub(super) fn measures_heard(&mut self, r: u32) {
        self.rerate(r);
        let cpu = self.peers.get(r as usize).is_some_and(|p| p.scale != SCALE);
        if cpu && !self.ordered {
            self.order_narrow();
            // The same word may also change a byte rate whose index was already requested.
            self.set_orders(r, [false, true, true, true]);
        } else if self.orders_kept != 0 {
            self.sync_orders(r);
        }
    }

    /// Narrow asks' order is not the wide one from now on: a plain ask (its parallelism its CPU
    /// need: narrow) is decided by the narrow order (`WIDE` sends the fast entry to
    /// [`Owner::ask_narrow`]).
    fn order_narrow(&mut self) {
        self.flush();
        self.ordered = true;
        self.pend.r = WIDE;
        self.keep_orders([true, false, false, false]);
    }

    /// Runner `r`'s class in each order: the narrow one by its key multiplier (per-core speed over
    /// slowness, as its key folds them), the rest by their rates.
    fn classes(&self, r: u32, only: [bool; ORDERS]) -> [u32; ORDERS] {
        let scale = self.peers.get(r as usize).map_or(SCALE, |p| p.scale);
        let narrow = if only[NARROW] { class_of(u64::from(scale)) } else { NIL };
        if !only[DISK] && !only[LINK] && !only[NET] {
            // CPU-only updates never load the separate, much larger byte-rate facts.
            return [narrow, NIL, NIL, NIL];
        }
        let c = self.cold.get(r as usize);
        [narrow,
         if only[DISK] { class_of(self.prior.disk.or(c.map_or(0, |c| c.disk))) } else { NIL },
         if only[LINK] { class_of(self.prior.link.or(c.map_or(0, |c| c.link.rate))) } else { NIL },
         if only[NET] { class_of(self.prior.net.or(c.map_or(0, |c| c.net))) } else { NIL }]
    }

    /// The classes runner `r` belongs in, order by order (NIL: out): its own while its link is up
    /// and it has a whole core free (its key not 0), so a pick walks no full runner; the narrow
    /// order only once that is kept. `only`: the orders to set (the rest are left as they are).
    fn set_orders(&mut self, r: u32, only: [bool; ORDERS]) {
        let only = std::array::from_fn(|i| only.get(i).copied().unwrap_or(false) && self.orders_kept & (1 << i) != 0);
        if !only.into_iter().any(|x| x) { return; }
        let Some(p) = self.peers.get(r as usize).copied() else { return };
        let up = p.flags & (KNOWN | UP) == KNOWN | UP;
        let core = key(p.view, p.scale, self.last) != 0;
        let classes = if up && core { self.classes(r, only) } else { [NIL; ORDERS] };
        let narrow = self.ordered;
        let set = |s: &mut Sig| {
            for (o, order) in s.orders.iter_mut().enumerate() {
                if only.get(o).copied().unwrap_or(false) {
                    order.set(r, if o == NARROW && !narrow { NIL } else { classes.get(o).copied().unwrap_or(NIL) });
                }
            }
        };
        if let Some(s) = self.sigs.get_mut(p.sig0 as usize) {
            set(s);
        }
        let n = self.more.get(r as usize).map_or(0, Vec::len);
        for i in 0..n {
            let si = self.more.get(r as usize).and_then(|m| m.get(i)).copied().unwrap_or(NIL);
            if let Some(s) = self.sigs.get_mut(si as usize) {
                set(s);
            }
        }
    }

    /// Puts runner `r` where it belongs in every order of its signatures.
    pub(super) fn sync_orders(&mut self, r: u32) {
        self.set_orders(r, [true; ORDERS]);
    }

    /// Runner `r`'s key went to or from 0: into or out of its orders. The narrow order follows
    /// now (plain asks read it); the disk, link and network orders, read only by asks with those
    /// terms, when one of those reads them ([`Owner::clean`]).
    #[cold]
    #[inline(never)]
    pub(super) fn gate(&mut self, r: u32) {
        if self.ordered && !mutant(M::NoGate) {
            let mut only = [false; ORDERS];
            if let Some(x) = only.get_mut(NARROW) {
                *x = true;
            }
            self.set_orders(r, only);
        }
        if self.orders_kept & BYTES != 0
            && let Some(p) = self.peers.get_mut(r as usize)
        {
            mark(&mut p.flags, &mut self.dirty, r);
        }
    }

    /// The disk, link and network orders' places put off ([`Owner::gate`]), applied: before an
    /// ask reads those orders. O(runners put off).
    pub(super) fn clean(&mut self) {
        let mut list = std::mem::take(&mut self.dirty);
        let mut only = [true; ORDERS];
        if let Some(x) = only.get_mut(NARROW) {
            *x = false;
        }
        for &r in &list {
            if let Some(p) = self.peers.get_mut(r as usize) {
                p.flags &= !DIRTY;
            }
            self.set_orders(r, only);
        }
        list.clear();
        self.dirty = list;
    }

    /// Whether `need` fits runner `r` at this owner's view, its link up.
    pub(super) fn fits_at(&self, r: u32, need: &Amounts) -> bool {
        self.peers.get(r as usize).is_some_and(|p| p.flags & UP != 0 && u64::from(p.view) >= need[0]) && (!multi(need) || self.rest_fits_at(r, need))
    }

    /// One draw of the spread pick's generator among `width` (as [`Owner::ask_wide`]'s).
    pub(super) fn draw(&mut self) -> u32 {
        self.draw_below(self.width)
    }

    /// One draw of the spread pick's generator, uniform below `n` (SplitMix64, multiply-reduced).
    pub(super) fn draw_below(&mut self, n: u32) -> u32 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let z = z ^ (z >> 31);
        ((u128::from(z) * u128::from(n)) >> 64) as u32
    }

    /// The pick a plain ask takes by CPU: on measured runners its order is the narrow one (its
    /// parallelism is its CPU need), the member of the highest class with room, and of those the
    /// index's most free when that one is in it; else the index's pick.
    pub(super) fn pick_cpu(&mut self, si: u32, need: &Amounts, cpu_only: bool) -> u32 {
        if !self.ordered || mutant(M::NoNarrow) {
            return self.pick(si, need, cpu_only);
        }
        let skip = if self.width > 1 { self.draw() } else { 0 };
        let m = self.find_in(si, NARROW, need, skip);
        if m == NIL {
            return self.pick(si, need, cpu_only);
        }
        if skip == 0
            && let Some(s) = self.sigs.get(si as usize)
            && let Some(top) = s.ix.top()
        {
            let w = s.ix.next_in(top);
            let order = s.orders.get(NARROW);
            let (wc, mc) = order.map_or((NIL, NIL), |o| (o.class(w), o.class(m)));
            if w != m && wc != NIL && wc == mc && self.fits_at(w, need) {
                if let Some(s) = self.sigs.get_mut(si as usize) {
                    s.ix.took(top, w);
                }
                return w;
            }
        }
        if let Some(o) = self.sigs.get_mut(si as usize).and_then(|s| s.orders.get_mut(NARROW)) {
            o.took(m);
        }
        m
    }

    /// The member of order `o` a need goes to: from the highest class down, members in turn,
    /// the first that fits after `skip` that fit (the last that fits when fewer do). Members
    /// passed are counted ([`Counts::walked`]). NIL: none fits.
    pub(super) fn find_in(&mut self, si: u32, o: usize, need: &Amounts, skip: u32) -> u32 {
        let Some(order) = self.sigs.get(si as usize).and_then(|s| s.orders.get(o)) else { return NIL };
        let (mut walked, mut left, mut found) = (0u64, skip, NIL);
        let mut c = order.top();
        'classes: while let Some(cc) = c {
            for r in order.ring(cc) {
                if self.fits_at(r, need) {
                    found = r;
                    if left == 0 {
                        break 'classes;
                    }
                    left -= 1;
                } else {
                    walked += 1;
                }
            }
            c = order.below(cc);
        }
        self.counts.walked = self.counts.walked.saturating_add(walked);
        found
    }
}
