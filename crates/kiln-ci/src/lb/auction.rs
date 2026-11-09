//! An integer auction for joint placements (docs/lb.md "Batches and gangs"; LB08, W05-R26): where
//! several runs go together at the least total expected finish, when the owner is told to
//! ([`super::Owner::set_auction`]; off until then: one at a time, each to its cheapest member).
//!
//! Bertsekas, "Auction Algorithms for Network Flow Problems: A Tutorial Introduction",
//! Computational Optimization and Applications 1:7-66, 1992:
//! - Persons (the runs) bid for objects (a runner's slots: as many as its view holds of the
//!   need), each raising its best object's price by its best value less its second-best plus ε;
//!   a person outbid bids again (the forward auction, one bidder at a time, §3).
//! - With integer benefits, an assignment satisfying ε-complementary slackness is within nε of
//!   optimal for n persons (Prop. 1), so ε < 1/n makes it optimal (Prop. 2); with similar objects
//!   ε < 1/m for m classes suffices (§5.1). Benefits here are costs scaled by min(n, m) + 1 and
//!   negated, and ε is 1: the bound is derived, not chosen.
//! - More objects than persons (§9): every price starts at zero, which is all the forward auction
//!   needs there (ε-scaling then isn't possible). A runner's slots are one similarity class
//!   (§5.1): a bid's increment is against the second-best *class*, and a class's price is its
//!   slots' lowest contention threshold, so persons don't bid one another up one ε at a time over
//!   identical slots (the price wars of its Figure 8).
//! - Feasibility is checked first (every person gets a slot at once, by augmenting paths), so
//!   the auction ends.
//!
//! Costs are expected finish in the caller's integer unit ([`expected_finish_ms`]): the run time
//! at the runner's measured speed and the transfer of its inputs from where they are (the
//! topology's [`crate::topo::Link::transfer_us`]); only slots with room now take part, so no wait.

use super::owner::SPEED_ONE;

/// An assignment: each person's class, the total cost, and how many bids it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub of: Vec<usize>,
    pub total: u64,
    pub bids: u64,
}

/// A run's expected finish at a runner, ms: its run time at the reference core (`run_ms`) at
/// the runner's per-core `speed` (1/1024 of the reference's; 0, not measured, is the reference),
/// and its inputs' transfer there (`transfer_us`).
pub fn expected_finish_ms(run_ms: u64, speed: u32, transfer_us: u64) -> u64 {
    let run = if speed == 0 { run_ms } else { u64::try_from(u128::from(run_ms) * u128::from(SPEED_ONE) / u128::from(speed)).unwrap_or(u64::MAX) };
    run.saturating_add(transfer_us.div_ceil(1000))
}

/// Persons placed in classes while feasibility is worked out: each class's holders.
struct Paths<'a, F> {
    slots: &'a [u32],
    cost: &'a F,
    holders: Vec<Vec<usize>>,
}

impl<F: Fn(usize, usize) -> Option<u64>> Paths<'_, F> {
    /// From person `i`, a path to a class with a free slot, moving holders along it.
    fn augment(&mut self, i: usize, seen: &mut [bool]) -> bool {
        for c in 0..self.slots.len() {
            if (self.cost)(i, c).is_none() || seen.get(c).copied().unwrap_or(true) {
                continue;
            }
            if let Some(s) = seen.get_mut(c) {
                *s = true;
            }
            let (held, k) = (self.holders.get(c).map_or(0, Vec::len), self.slots.get(c).copied().unwrap_or(0) as usize);
            if held < k {
                if let Some(h) = self.holders.get_mut(c) {
                    h.push(i);
                }
                return true;
            }
            for j in self.holders.get(c).cloned().unwrap_or_default() {
                if self.augment(j, seen) {
                    if let Some(h) = self.holders.get_mut(c) {
                        h.retain(|x| *x != j);
                        h.push(i);
                    }
                    return true;
                }
            }
        }
        false
    }
}

/// Whether every one of `n` persons can have a slot at once (`slots[c]` of class `c`, where
/// `cost(i, c)` is some): augmenting paths, O(n × edges).
fn feasible(n: usize, slots: &[u32], cost: &impl Fn(usize, usize) -> Option<u64>) -> bool {
    let mut p = Paths { slots, cost, holders: vec![Vec::new(); slots.len()] };
    (0..n).all(|i| p.augment(i, &mut vec![false; slots.len()]))
}

/// The least-cost assignment of `n` persons to classes, at most `slots[c]` to class `c`, person
/// `i` to class `c` only where `cost(i, c)` is some (Bertsekas's forward auction with similar
/// objects, ε = 1 on costs scaled by min(n, m) + 1). None: they can't all have a slot.
pub fn assign(n: usize, slots: &[u32], cost: impl Fn(usize, usize) -> Option<u64>) -> Option<Assignment> {
    let m = slots.len();
    if n == 0 {
        return Some(Assignment { of: Vec::new(), total: 0, bids: 0 });
    }
    if !feasible(n, slots, &cost) {
        return None;
    }
    #[cfg(test)]
    let scale: i128 = if super::planted(super::Mutant::EpsilonTooLarge) { 1 } else { i128::try_from(n.min(m)).unwrap_or(i128::MAX / 4) + 1 };
    #[cfg(not(test))]
    let scale: i128 = i128::try_from(n.min(m)).unwrap_or(i128::MAX / 4) + 1;
    let cost = &cost;
    let top = (0..n).flat_map(|i| (0..m).filter_map(move |c| cost(i, c))).max().unwrap_or(0);
    // A person with one class bids past any value another could have there.
    let past = (i128::from(top) + 1) * scale + 1;
    let mut theta: Vec<Vec<i128>> = slots.iter().map(|&k| vec![0i128; (k as usize).min(n)]).collect();
    let mut holder: Vec<Vec<Option<usize>>> = theta.iter().map(|t| vec![None; t.len()]).collect();
    let mut of: Vec<Option<usize>> = vec![None; n];
    let mut queue: std::collections::VecDeque<usize> = (0..n).collect();
    let mut bids = 0u64;
    while let Some(i) = queue.pop_front() {
        let mut best: Option<(usize, usize, i128)> = None;
        let mut second: Option<i128> = None;
        for c in 0..m {
            let Some(k) = cost(i, c) else { continue };
            let Some((j, price)) = theta.get(c).and_then(|t| t.iter().copied().enumerate().min_by_key(|(_, p)| *p)) else { continue };
            let v = -(i128::from(k) * scale) - price;
            match best {
                Some((_, _, bv)) if v <= bv => second = Some(second.map_or(v, |s| s.max(v))),
                Some((_, _, bv)) => {
                    second = Some(second.map_or(bv, |s| s.max(bv)));
                    best = Some((c, j, v));
                }
                None => best = Some((c, j, v)),
            }
            #[cfg(test)]
            if super::planted(super::Mutant::SecondInClass) {
                // The plain auction's second best: the next slot of the best class counts too.
                let t = theta.get(c).cloned().unwrap_or_default();
                let mut prices: Vec<i128> = t.clone();
                prices.sort_unstable();
                if let (Some((bc, _, _)), Some(p2)) = (best, prices.get(1))
                    && bc == c
                {
                    let v2 = -(i128::from(k) * scale) - p2;
                    second = Some(second.map_or(v2, |s| s.max(v2)));
                }
            }
        }
        let (c, j, v) = best?;
        let inc = second.map_or(past, |w| v - w + 1);
        if let Some(t) = theta.get_mut(c).and_then(|t| t.get_mut(j)) {
            *t = t.saturating_add(inc);
        }
        let h = holder.get_mut(c).and_then(|h| h.get_mut(j))?;
        if let Some(prev) = h.replace(i) {
            if let Some(o) = of.get_mut(prev) {
                *o = None;
            }
            queue.push_back(prev);
        }
        if let Some(o) = of.get_mut(i) {
            *o = Some(c);
        }
        bids += 1;
    }
    let of: Vec<usize> = of.into_iter().collect::<Option<Vec<usize>>>()?;
    let total = of.iter().enumerate().map(|(i, c)| cost(i, *c).unwrap_or(0)).fold(0u64, u64::saturating_add);
    Some(Assignment { of, total, bids })
}

/// One at a time, in order, each person to its cheapest class with a slot left (lowest class
/// among equals): the rule the auction is measured against. None: one found no slot.
pub fn greedy(n: usize, slots: &[u32], cost: impl Fn(usize, usize) -> Option<u64>) -> Option<Assignment> {
    let mut left: Vec<u32> = slots.to_vec();
    let mut of = Vec::with_capacity(n);
    let mut total = 0u64;
    for i in 0..n {
        let (c, k) = (0..left.len()).filter(|c| left.get(*c).is_some_and(|l| *l > 0)).filter_map(|c| cost(i, c).map(|k| (c, k))).min_by_key(|(c, k)| (*k, *c))?;
        if let Some(l) = left.get_mut(c) {
            *l -= 1;
        }
        of.push(c);
        total = total.saturating_add(k);
    }
    Some(Assignment { of, total, bids: 0 })
}
