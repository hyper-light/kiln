//! Sizes the node learns from the demand it sees, and the bounds over them (Ada's rule, 2026-11-06: "We do not
//! support arbitrary constants. We support learned and data derived values"; docs/design/runtime-architecture.md,
//! "Pool sizes learned from demand"). Latency and throughput come before memory, so a learned size starts at no
//! less than the host's own parallelism, grows ahead of the demand seen, and never shrinks: no rule that shrinks
//! a pool can be shown not to cause a take to find it short under the demand already seen. Each size stops at
//! its bound, derived from the memory the node may hold (CLAUDE.md §2); past it a take finds the pool short and
//! is counted, as every pool's cold take is.
//!
//! Pure (CLAUDE.md §9): the node's shell reads the host's facts ([`mem_available`], [`cgroup_headroom`]) and
//! reports what its pools see; this decides the sizes.

use std::collections::BTreeMap;

/// A size learned from demand: at least its start, at most its bound, raised to each demand reported, never
/// lowered. The default holds none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Learned {
    size: usize,
    cap: usize,
}

impl Learned {
    /// A size starting at `start`, held at or under `cap`.
    pub fn new(start: usize, cap: usize) -> Self {
        Self {
            size: start.min(cap),
            cap,
        }
    }

    /// Demand of `n` seen: the size is raised to it, up to the bound. Whether it rose.
    pub fn observe(&mut self, n: usize) -> bool {
        let to = n.min(self.cap);
        let rose = to > self.size;
        self.size = self.size.max(to);
        rose
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn cap(&self) -> usize {
        self.cap
    }
}

/// A pool made ahead's size, learned from its draws: each take since the pool last held its size is one drawn,
/// and a take that finds the pool empty (a cold take) raises the size to what was drawn plus every cold take of
/// that draw, so the next draw as deep finds the pool full with that many to spare. The headroom grows with each
/// take found short, as the lead's condition of 2026-11-06 asks, and the size never falls. The default holds none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Depth {
    size: Learned,
    drawn: usize,
    cold: usize,
}

impl Depth {
    pub fn new(start: usize, cap: usize) -> Self {
        Self {
            size: Learned::new(start, cap),
            drawn: 0,
            cold: 0,
        }
    }

    /// A take: `cold` when it found the pool empty. Whether the size rose.
    pub fn took(&mut self, cold: bool) -> bool {
        self.drawn = self.drawn.saturating_add(1);
        if !cold {
            return false;
        }
        self.cold = self.cold.saturating_add(1);
        self.size.observe(self.drawn.saturating_add(self.cold))
    }

    /// The pool holds its size again: the next take starts a new draw.
    pub fn full(&mut self) {
        self.drawn = 0;
        self.cold = 0;
    }

    pub fn size(&self) -> usize {
        self.size.size()
    }

    pub fn cap(&self) -> usize {
        self.size.cap()
    }
}

/// The most idle items any one owner keeps, learned from regret, as ARC adapts its lists' target (Megiddo and
/// Modha, "ARC: A Self-Tuning, Low Overhead Replacement Cache", FAST 2003, §4.3: a hit in a list's ghost
/// directory raises that list's target). Each item an owner's bound put out is remembered as a ghost of that
/// owner; a take that then finds the owner with none idle spends one and raises the bound by one, so a burst
/// one round lost to the bound is kept whole the next. The ghosts are bounded: at most `cap` owners, the one
/// whose ghost came longest ago forgotten past it, and at most `cap` ghosts an owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ghosts {
    bound: Learned,
    /// Each owner's ghosts and when its last came.
    owners: BTreeMap<String, (u64, usize)>,
    /// The owners by when their last ghost came, the oldest first.
    order: BTreeMap<u64, String>,
    seq: u64,
}

impl Ghosts {
    pub fn new(start: usize, cap: usize) -> Self {
        Self {
            bound: Learned::new(start, cap),
            owners: BTreeMap::new(),
            order: BTreeMap::new(),
            seq: 0,
        }
    }

    /// The bound now.
    pub fn bound(&self) -> usize {
        self.bound.size()
    }

    pub fn cap(&self) -> usize {
        self.bound.cap()
    }

    /// One of `owner`'s idle items was put out by the bound.
    pub fn evicted(&mut self, owner: &str) {
        let cap = self.bound.cap();
        if cap == 0 {
            return;
        }
        self.seq = self.seq.saturating_add(1);
        let count = match self.owners.remove(owner) {
            Some((seq, count)) => {
                self.order.remove(&seq);
                count.saturating_add(1).min(cap)
            }
            None => 1,
        };
        self.owners.insert(owner.to_owned(), (self.seq, count));
        self.order.insert(self.seq, owner.to_owned());
        // One insert a call, so at most one owner past the bound.
        if self.owners.len() > cap
            && let Some((_, oldest)) = self.order.pop_first()
        {
            self.owners.remove(&oldest);
        }
    }

    /// A take of `owner`'s found none of its items idle: one of its ghosts, if any, is spent and the bound
    /// raised by one. Whether it rose.
    pub fn missed(&mut self, owner: &str) -> bool {
        let Some((seq, count)) = self.owners.get_mut(owner) else {
            return false;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            let seq = *seq;
            self.owners.remove(owner);
            self.order.remove(&seq);
        }
        self.bound.observe(self.bound.size().saturating_add(1))
    }

    /// How many owners have ghosts.
    pub fn owners(&self) -> usize {
        self.owners.len()
    }
}

/// `MemAvailable` from `/proc/meminfo`, in bytes: the kernel's estimate of the memory available for starting new
/// work without swapping (Linux Documentation/filesystems/proc.rst, "meminfo"). `None` when the text has no such
/// line in kB or its number doesn't fit.
pub fn mem_available(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let mut words = rest.split_whitespace();
        let kib: u64 = words.next()?.parse().ok()?;
        if words.next()? != "kB" || words.next().is_some() {
            return None;
        }
        kib.checked_mul(1024)
    })
}

/// What a cgroup's `memory.max` leaves past its `memory.current`, in bytes (Linux
/// Documentation/admin-guide/cgroup-v2.rst, "memory.max"): `None` where it sets no bound (`max`) or a file
/// doesn't parse; 0 where it is reached.
pub fn cgroup_headroom(max: &str, current: &str) -> Option<u64> {
    let max: u64 = max.trim().parse().ok()?;
    let current: u64 = current.trim().parse().ok()?;
    Some(max.saturating_sub(current))
}

/// How many items of `each` bytes fit in an even share of `memory` among `sharers` pools: the pools that hold
/// the node's memory split it evenly, so together they never hold past it. 0 for no sharers or items of no
/// size, which nothing should name.
pub fn share(memory: u64, sharers: usize, each: u64) -> usize {
    let Ok(sharers) = u64::try_from(sharers) else {
        return 0;
    };
    memory
        .checked_div(sharers)
        .and_then(|part| part.checked_div(each))
        .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX))
}

/// How many of a node's pools hold its memory, whose shares follow their demand ([`weighted`]).
pub const MEMORY_POOLS: usize = 7;

/// What a node learned, kept in its state so its next start begins there and takes no cold miss the last one had
/// learned past (the lead's call, 2026-11-06): each learned size's peak, the seeds' spare, and the takes of each pool
/// that holds memory, which weight their shares at the next start ([`weighted`]). The record is text, a key and a
/// number a line between a version line and an `end` line, so a torn one has no end and reads as none; it changes
/// append-only: a key a later node wrote is passed over, and one an earlier node didn't write reads as none seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Peaks {
    pub owner_cgroups: usize,
    pub owner_netns: usize,
    pub netns: usize,
    pub ipc_uts: usize,
    pub ahead: usize,
    pub devfs_keys: usize,
    pub devfs_per_key: usize,
    pub seeds: usize,
    pub spare: usize,
    /// Each memory pool's takes, in the order the node's shell lists them.
    pub takes: [u64; MEMORY_POOLS],
}

/// Why a record of peaks wasn't read: the shell counts it and starts as a node's first start does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unread {
    #[error("it isn't a version 1 record of peaks")]
    Version,
    #[error("it has no end: torn")]
    Torn,
    #[error("line {0} isn't a key and a number")]
    Line(usize),
    #[error("key {0} is there twice")]
    Twice(String),
}

const HEADER: &str = "hex-peaks 1";
const END: &str = "end";
/// The keys a record holds, in the order it writes them.
const KEYS: [&str; 9 + MEMORY_POOLS] = [
    "owner-cgroups",
    "owner-netns",
    "netns",
    "ipc-uts",
    "ahead",
    "devfs-keys",
    "devfs-per-key",
    "seeds",
    "spare",
    "takes-0",
    "takes-1",
    "takes-2",
    "takes-3",
    "takes-4",
    "takes-5",
    "takes-6",
];

impl Peaks {
    /// The most bytes a record this node writes holds: its version line, each key with the longest number a `u64`
    /// writes (20 digits), a space and a newline, and its end. A reader takes no more than twice this, room for a
    /// later node's as many keys again, before it refuses the record.
    pub fn most_bytes() -> usize {
        KEYS.iter()
            .map(|k| k.len().saturating_add(1 + 20 + 1))
            .fold(
                HEADER.len().saturating_add(END.len()).saturating_add(2),
                usize::saturating_add,
            )
    }

    fn values(&self) -> [u64; KEYS.len()] {
        let u = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let [t0, t1, t2, t3, t4, t5, t6] = self.takes;
        [
            u(self.owner_cgroups),
            u(self.owner_netns),
            u(self.netns),
            u(self.ipc_uts),
            u(self.ahead),
            u(self.devfs_keys),
            u(self.devfs_per_key),
            u(self.seeds),
            u(self.spare),
            t0,
            t1,
            t2,
            t3,
            t4,
            t5,
            t6,
        ]
    }

    /// The record's text.
    pub fn encode(&self) -> String {
        let mut out = format!("{HEADER}\n");
        for (key, value) in KEYS.iter().zip(self.values()) {
            out.push_str(&format!("{key} {value}\n"));
        }
        out.push_str(END);
        out.push('\n');
        out
    }

    /// The peaks a record's text holds, or why it holds none.
    pub fn decode(text: &str) -> Result<Self, Unread> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(Unread::Version);
        }
        let mut values: [Option<u64>; KEYS.len()] = [None; KEYS.len()];
        let mut ended = false;
        for (n, line) in lines.enumerate() {
            if line == END {
                ended = true;
                break;
            }
            let mut words = line.split(' ');
            let (Some(key), Some(value), None) = (words.next(), words.next(), words.next()) else {
                return Err(Unread::Line(n.saturating_add(2)));
            };
            let value: u64 = value
                .parse()
                .map_err(|_| Unread::Line(n.saturating_add(2)))?;
            if let Some(at) = KEYS.iter().position(|k| *k == key)
                && let Some(slot) = values.get_mut(at)
                && slot.replace(value).is_some()
            {
                return Err(Unread::Twice(key.to_owned()));
            }
        }
        if !ended {
            return Err(Unread::Torn);
        }
        let v = |i: usize| values.get(i).copied().flatten().unwrap_or(0);
        let z = |i: usize| usize::try_from(v(i)).unwrap_or(usize::MAX);
        Ok(Self {
            owner_cgroups: z(0),
            owner_netns: z(1),
            netns: z(2),
            ipc_uts: z(3),
            ahead: z(4),
            devfs_keys: z(5),
            devfs_per_key: z(6),
            seeds: z(7),
            spare: z(8),
            takes: [v(9), v(10), v(11), v(12), v(13), v(14), v(15)],
        })
    }
}

/// `memory` split among pools by their demand, each `(takes, cost)`: a pool's share is the memory times its takes
/// times its item's cost over the sum of those (the lead's call, 2026-11-06), so the pools that were used hold the
/// memory in proportion to what they used. With no takes at all, as on a node's first start, the split is even.
/// The shares never sum past `memory`.
pub fn weighted(memory: u64, pools: &[(u64, u64)]) -> Vec<u64> {
    // Each weight is under 2^128; shifted together until their sum fits in 64 bits, so the memory times one can't
    // pass 2^128. Their ratios, which the shares follow, keep.
    let mut weights: Vec<u128> = pools
        .iter()
        .map(|&(takes, cost)| u128::from(takes).saturating_mul(u128::from(cost)))
        .collect();
    let sum = |w: &[u128]| w.iter().fold(0u128, |a, &b| a.saturating_add(b));
    let mut total = sum(&weights);
    while total > u128::from(u64::MAX) {
        for w in &mut weights {
            *w >>= 1;
        }
        total = sum(&weights);
    }
    if total == 0 {
        let even = u64::try_from(pools.len())
            .ok()
            .and_then(|n| memory.checked_div(n))
            .unwrap_or(0);
        return vec![even; pools.len()];
    }
    weights
        .iter()
        .map(|&w| {
            let share = u128::from(memory)
                .saturating_mul(w)
                .checked_div(total)
                .unwrap_or(0);
            u64::try_from(share).unwrap_or(u64::MAX)
        })
        .collect()
}

#[cfg(test)]
mod tests;
