//! The harness that measures the three ways to reach a cached page against each other (G7 step 3):
//! point reads and scans at each client count asked for (the plan's 1 and 32), with every page of
//! the file cached and with a pool a tenth of the file (eviction-heavy: plan step 4's bound, a pool
//! a tenth of the data's size). Each run is one record: the design, the work, the pool, the clients,
//! accesses, nanoseconds per access, and the pool's counts. Running it, and recording what it
//! measures in `bench/` with the hardware, commit and load, is the coordinator's (`amphora-bench
//! --reach`, `bench/src/reach.rs`).
//!
//! Sizes come from the file and the machine: each client makes as many accesses as the file has
//! pages, the cached pool has a frame per page, the evicting pool a tenth of that, and the tables'
//! shards follow the machine's threads ([`super::shards_for_machine`]).

use super::direct::Direct;
use super::swizzle::Swizzle;
use super::table::Table;
use super::{shards_for_machine, Reach, Stats, Store};
use crate::error::LimboError;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::Result;
use std::sync::Arc;
use std::time::Instant;

/// What each client does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// A random page, and a binary search's probes within it.
    Point,
    /// Pages in order from a start of its own, every word of each.
    Scan,
}

/// How big the pool is against the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// A frame for every page.
    Cached,
    /// A tenth of the pages.
    Evicting,
}

/// One run's record.
#[derive(Clone, Debug)]
pub struct Record {
    pub design: &'static str,
    pub work: Work,
    pub fit: Fit,
    pub clients: usize,
    pub pages: u64,
    pub frames: usize,
    pub shards: usize,
    pub accesses: u64,
    pub nanos: u128,
    pub stats: Stats,
    /// What the reads summed, so none is optimized away.
    pub checksum: u64,
}

impl Record {
    /// Nanoseconds per access.
    pub fn ns_per_access(&self) -> f64 {
        let a = self.accesses.max(1) as f64;
        self.nanos as f64 / a
    }

    /// The record as one JSON line.
    pub fn json(&self) -> String {
        format!(
            "{{\"bench\":\"reach\",\"design\":\"{}\",\"work\":\"{:?}\",\"fit\":\"{:?}\",\"clients\":{},\"pages\":{},\"frames\":{},\"shards\":{},\"accesses\":{},\"nanos\":{},\"ns_per_access\":{:.3},\"misses\":{},\"evictions\":{},\"retries\":{},\"checksum\":{}}}",
            self.design,
            self.work,
            self.fit,
            self.clients,
            self.pages,
            self.frames,
            self.shards,
            self.accesses,
            self.nanos,
            self.ns_per_access(),
            self.stats.misses,
            self.stats.evictions,
            self.stats.retries,
            self.checksum
        )
    }
}

/// SplitMix64's step (Steele et al., OOPSLA 2014): the clients' random pages.
fn mix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ z.wrapping_shr(31)
}

/// A binary search's probes over a page's words for `key`: the words it would compare.
fn probe(words: &[AtomicU64], key: u64) -> u64 {
    let (mut lo, mut hi) = (0usize, words.len());
    let mut sum = 0u64;
    while lo < hi {
        let mid = lo.saturating_add(hi.saturating_sub(lo).wrapping_shr(1));
        let w = words.get(mid).map_or(0, |w| w.load(Ordering::Relaxed));
        sum = sum.wrapping_add(w);
        if w ^ key < key {
            lo = mid.saturating_add(1);
        } else {
            hi = mid;
        }
    }
    sum
}

fn scan(words: &[AtomicU64]) -> u64 {
    words
        .iter()
        .fold(0u64, |s, w| s.wrapping_add(w.load(Ordering::Relaxed)))
}

/// One run of `work` by `clients` clients on `pool` over a file of `pages` pages.
pub fn measure<P: Reach>(
    pool: &P,
    work: Work,
    fit: Fit,
    clients: usize,
    pages: u64,
    frames: usize,
) -> Result<Record> {
    if pages == 0 || clients == 0 {
        return Err(LimboError::InvalidArgument(
            "a run needs pages and clients".into(),
        ));
    }
    // Every page read once first, by one client: a cached pool's hits are what's measured.
    let mut c = pool.client();
    for no in 1..=pages {
        pool.read(&mut c, no, |w| {
            w.first().map_or(0, |w| w.load(Ordering::Relaxed))
        })?;
    }
    pool.release(&mut c);
    let before = pool.stats();
    let began = Instant::now();
    let sums: Vec<Result<u64>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..clients)
            .map(|k| {
                s.spawn(move || -> Result<u64> {
                    let mut c = pool.client();
                    let mut seed = u64::try_from(k).unwrap_or(0);
                    let mut sum = 0u64;
                    let mut at = mix(&mut seed).checked_rem(pages).unwrap_or(0);
                    for _ in 0..pages {
                        let r = match work {
                            Work::Point => {
                                let no = mix(&mut seed)
                                    .checked_rem(pages)
                                    .unwrap_or(0)
                                    .saturating_add(1);
                                let key = mix(&mut seed);
                                pool.read(&mut c, no, |w| probe(w, key))
                            }
                            Work::Scan => {
                                at = at.saturating_add(1).checked_rem(pages).unwrap_or(0);
                                pool.read(&mut c, at.saturating_add(1), scan)
                            }
                        };
                        match r {
                            Ok(v) => sum = sum.wrapping_add(v),
                            Err(e) => {
                                pool.release(&mut c);
                                return Err(e);
                            }
                        }
                    }
                    pool.release(&mut c);
                    Ok(sum)
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(LimboError::InternalError(
                        "a client thread ended early".into(),
                    ))
                })
            })
            .collect()
    });
    let nanos = began.elapsed().as_nanos();
    let mut checksum = 0u64;
    for s in sums {
        checksum = checksum.wrapping_add(s?);
    }
    let after = pool.stats();
    let accesses = pages.saturating_mul(u64::try_from(clients).unwrap_or(u64::MAX));
    Ok(Record {
        design: pool.name(),
        work,
        fit,
        clients,
        pages,
        frames,
        shards: shards_for_machine(),
        accesses,
        nanos,
        stats: Stats {
            misses: after.misses.saturating_sub(before.misses),
            evictions: after.evictions.saturating_sub(before.evictions),
            retries: after.retries.saturating_sub(before.retries),
        },
        checksum,
    })
}

/// Every design, work and fit at each client count, over a file of `pages` pages: each record as
/// `out` is handed it.
pub fn run_all(pages: u64, clients: &[usize], mut out: impl FnMut(&Record)) -> Result<()> {
    let store = Arc::new(Store::new(pages, |no, w| {
        let mut s = no;
        for x in w.iter_mut() {
            *x = mix(&mut s);
        }
    })?);
    let all = usize::try_from(pages).map_err(|_| {
        LimboError::InvalidArgument("the file's pages don't fit an array here".into())
    })?;
    for fit in [Fit::Cached, Fit::Evicting] {
        let frames = match fit {
            Fit::Cached => all,
            // A tenth, but never fewer than a frame for each client's pin ((a) keeps one) and one
            // to load into, or (a) would refuse for want of a free frame.
            Fit::Evicting => all
                .checked_div(10)
                .unwrap_or(0)
                .max(clients.iter().copied().max().unwrap_or(1).saturating_add(1)),
        };
        for work in [Work::Point, Work::Scan] {
            for &k in clients {
                out(&measure(
                    &Table::new(store.clone(), frames)?,
                    work,
                    fit,
                    k,
                    pages,
                    frames,
                )?);
                out(&measure(
                    &Swizzle::new(store.clone(), frames)?,
                    work,
                    fit,
                    k,
                    pages,
                    frames,
                )?);
                out(&measure(
                    &Direct::new(store.clone(), frames)?,
                    work,
                    fit,
                    k,
                    pages,
                    frames,
                )?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-NS.3.1: G7 step 3's harness, one tiny run: every design, work and fit at 1 and 4 clients returns a
    /// record with every access counted, and the evicting runs evict.
    #[test]
    fn g7_s3_harness_runs_every_design() {
        let mut records = Vec::new();
        run_all(200, &[1, 4], |r| records.push(r.clone())).unwrap();
        assert_eq!(records.len(), 2 * 2 * 2 * 3);
        for r in &records {
            assert_eq!(r.accesses, 200 * r.clients as u64, "{}", r.json());
            if r.fit == Fit::Evicting && r.work == Work::Point {
                assert!(r.stats.evictions > 0, "{}", r.json());
            }
            if r.fit == Fit::Cached {
                assert_eq!(r.stats.misses, 0, "{}", r.json());
            }
        }
        // The same work over the same file sums the same, whichever design read it.
        for chunk in records.chunks(3) {
            assert!(
                chunk.iter().all(|r| r.checksum == chunk[0].checksum),
                "{:?}",
                chunk.iter().map(Record::json).collect::<Vec<_>>()
            );
        }
    }
}
