//! Eviction under quota pressure (docs/ci.md W24-R15, R21, R28, AC12): what the ledger and the
//! content store keep when a domain's entries outgrow its quota. Each entry's worth is the time it
//! saves (W24-R28: its unit's duration times the hits it will have, counting the one to come) per
//! byte it keeps; each domain (W24-R21) keeps its most worthwhile entries that fit its quota, in
//! that order, and evicts the rest, so one domain's entries never evict another's. Pure: the server
//! gives the entries and the quotas, and removes what [`evict`] names.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// An entry, as eviction weighs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Its trust domain (W24-R21).
    pub domain: String,
    /// The bytes it keeps (its outputs' in the content store).
    pub bytes: u64,
    /// What its unit took to run, in milliseconds: what each hit saves.
    pub ms: u64,
    /// How often it was taken.
    pub hits: u64,
    /// When it was last taken (or made), in seconds: the tie-break, newest kept first.
    pub at: i64,
}

impl Entry {
    /// The time it saves: its unit's duration for each hit it had, and the next.
    pub fn saved_ms(&self) -> u128 {
        u128::from(self.ms).saturating_mul(u128::from(self.hits) + 1)
    }
}

/// How `a`'s time saved per byte compares with `b`'s, cross-multiplied rather than divided (it
/// saturates only past any real duration); an entry of no bytes is worth more than any that keeps
/// some.
fn denser(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (sa, sb) = (a.saved_ms(), b.saved_ms());
    match (a.bytes, b.bytes) {
        (0, 0) => sa.cmp(&sb),
        (0, _) => Ordering::Greater,
        (_, 0) => Ordering::Less,
        (ba, bb) => sa.saturating_mul(u128::from(bb)).cmp(&sb.saturating_mul(u128::from(ba))),
    }
}

/// The order a domain keeps its entries in: the most time saved per byte first, then the most
/// recently taken, then by id.
fn order(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    denser(b, a).then_with(|| b.at.cmp(&a.at)).then_with(|| a.id.cmp(&b.id))
}

/// The entries to evict (W24-AC12): in each domain over its quota (`quotas`, else `default`
/// bytes), those past the longest run of its most worthwhile entries that fits. A domain within its
/// quota evicts nothing, whatever the others hold.
pub fn evict(entries: &[Entry], quotas: &BTreeMap<String, u64>, default: u64) -> BTreeSet<String> {
    let mut by: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for e in entries {
        by.entry(e.domain.as_str()).or_default().push(e);
    }
    let mut out = BTreeSet::new();
    for (domain, mut list) in by {
        let quota = quotas.get(domain).copied().unwrap_or(default);
        if list.iter().map(|e| e.bytes).fold(0u64, u64::saturating_add) <= quota {
            continue;
        }
        list.sort_by(|a, b| order(a, b));
        let mut kept = 0u64;
        let mut full = false;
        for e in list {
            full = full || kept.saturating_add(e.bytes) > quota;
            if full {
                out.insert(e.id.clone());
            } else {
                kept += e.bytes;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn entry(id: &str, domain: &str, bytes: u64, ms: u64, hits: u64) -> Entry {
        Entry { id: id.into(), domain: domain.into(), bytes, ms, hits, at: 0 }
    }

    #[test]
    fn w24_ac12_the_most_time_saved_per_byte_stays() {
        let entries = [
            entry("rlib", "trusted", 10, 60_000, 9), // 60 s saved per byte
            entry("image", "trusted", 1_000, 300_000, 1), // 0.6 s per byte
            entry("tests", "trusted", 50, 30_000, 4), // 3 s per byte
            entry("theirs", "untrusted", 5_000, 1, 0),
        ];
        let quotas = BTreeMap::from([("trusted".to_owned(), 100), ("untrusted".to_owned(), 10_000)]);
        assert_eq!(evict(&entries, &quotas, 0), BTreeSet::from(["image".to_owned()]));
        // A domain's pressure never reaches another's entries, however little they're worth.
        let tight = BTreeMap::from([("trusted".to_owned(), 59), ("untrusted".to_owned(), 10_000)]);
        assert_eq!(evict(&entries, &tight, 0), BTreeSet::from(["image".to_owned(), "tests".to_owned()]));
        assert!(evict(&entries, &BTreeMap::new(), u64::MAX).is_empty(), "within quota: nothing goes");
    }

    fn arb_entries() -> impl Strategy<Value = Vec<Entry>> {
        let one = (0u64..1 << 20, 0u64..600_000, 0u64..50, 0i64..1_000, prop_oneof![Just("trusted"), Just("untrusted"), Just("person:ada")]);
        proptest::collection::vec(one, 0..60).prop_map(|v| v.into_iter().enumerate().map(|(i, (bytes, ms, hits, at, d))| Entry { id: format!("e{i}"), domain: d.into(), bytes, ms, hits, at }).collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// W24-AC12: under quota pressure every domain fits its quota, keeps entries with at least
        /// the time saved per byte of every entry it evicts, and evicts no other domain's; and what
        /// it keeps would save, at each entry's hits so far, at least what least-recently-used
        /// eviction keeps in the same bytes, short of the most one entry it evicted saves.
        #[test]
        fn w24_ac12_eviction_keeps_the_most_time_saved_per_byte(entries in arb_entries(), quota in 0u64..(8 << 20)) {
            let quotas = BTreeMap::from([("trusted".to_owned(), quota), ("person:ada".to_owned(), quota / 2)]);
            let default = quota / 4;
            let gone = evict(&entries, &quotas, default);
            for domain in ["trusted", "untrusted", "person:ada"] {
                let mine: Vec<&Entry> = entries.iter().filter(|e| e.domain == domain).collect();
                let q = quotas.get(domain).copied().unwrap_or(default);
                let (kept, out): (Vec<&Entry>, Vec<&Entry>) = mine.iter().partition(|e| !gone.contains(&e.id));
                prop_assert!(kept.iter().map(|e| e.bytes).sum::<u64>() <= q);
                for k in &kept {
                    for o in &out {
                        prop_assert!(denser(k, o) != std::cmp::Ordering::Less, "{} kept over {}", k.id, o.id);
                    }
                }
                let total: u64 = mine.iter().map(|e| e.bytes).sum();
                prop_assert!(total > q || out.is_empty(), "a domain within its quota evicts nothing");
                // Least recently used in the same bytes: the newest that fit, in order.
                let mut lru = mine.clone();
                lru.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
                let (mut used, mut lru_saved, mut stop) = (0u64, 0u128, false);
                for e in lru {
                    stop = stop || used.saturating_add(e.bytes) > q;
                    if !stop {
                        used += e.bytes;
                        lru_saved += e.saved_ms();
                    }
                }
                let saved: u128 = kept.iter().map(|e| e.saved_ms()).sum();
                // Density's prefix can lose to another choice by at most the next entry it left out.
                let lost = out.iter().map(|e| e.saved_ms()).max().unwrap_or(0);
                prop_assert!(saved + lost >= lru_saved, "kept {saved} ms saved, least recently used {lru_saved}");
            }
            prop_assert!(gone.iter().all(|id| entries.iter().any(|e| &e.id == id)));
        }
    }

    /// A generator of the simulation's own (xorshift64*), so it runs alike everywhere.
    struct Gen(u64);

    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Uniform in [0, 1).
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Between `lo` and `hi`, uniform in their logarithms.
        fn spread(&mut self, lo: f64, hi: f64) -> f64 {
            (lo.ln() + self.unit() * (hi.ln() - lo.ln())).exp()
        }
    }

    /// A unit of work the simulation looks up: the bytes its entry keeps, what running it takes,
    /// its domain, and how much it's wanted.
    struct Work {
        bytes: u64,
        ms: u64,
        domain: &'static str,
        weight: f64,
    }

    /// Least-recently-used eviction, to measure against: in each domain over its quota, the entries
    /// taken longest ago go first, until it fits.
    fn lru(entries: &[Entry], quotas: &BTreeMap<String, u64>, default: u64) -> BTreeSet<String> {
        let mut by: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        for e in entries {
            by.entry(e.domain.as_str()).or_default().push(e);
        }
        let mut out = BTreeSet::new();
        for (domain, mut list) in by {
            let quota = quotas.get(domain).copied().unwrap_or(default);
            let mut total = list.iter().map(|e| e.bytes).fold(0u64, u64::saturating_add);
            list.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
            for e in list {
                if total <= quota {
                    break;
                }
                total = total.saturating_sub(e.bytes);
                out.insert(e.id.clone());
            }
        }
        out
    }

    type Policy = fn(&[Entry], &BTreeMap<String, u64>, u64) -> BTreeSet<String>;

    /// A store under quota over `lookups` lookups of `works`, each drawn by its weight: a lookup
    /// whose entry is kept saves its unit's duration; a miss runs the unit and keeps its entry, and
    /// then `policy` evicts. An entry's hits are its unit's lookups so far, which the ledger keeps
    /// beside the bytes eviction removes. Returns the time saved, and how many entries went while
    /// their own domain was within its quota (another's pressure).
    fn simulate(works: &[Work], lookups: usize, quotas: &BTreeMap<String, u64>, seed: u64, policy: Policy) -> (u128, usize) {
        let mut g = Gen(seed);
        let mut upto = 0.0;
        let cumulative: Vec<f64> = works
            .iter()
            .map(|w| {
                upto += w.weight;
                upto
            })
            .collect();
        let mut kept: Vec<Option<Entry>> = works.iter().map(|_| None).collect();
        let mut wanted = vec![0u64; works.len()];
        let (mut saved, mut crossed) = (0u128, 0usize);
        for t in 0..lookups {
            let draw = g.unit() * upto;
            let i = cumulative.partition_point(|c| *c <= draw).min(works.len().saturating_sub(1));
            let Some(w) = works.get(i) else { continue };
            let seen = wanted.get(i).copied().unwrap_or(0);
            if let Some(n) = wanted.get_mut(i) {
                *n += 1;
            }
            if let Some(Some(e)) = kept.get_mut(i) {
                saved += u128::from(w.ms);
                (e.hits, e.at) = (seen, t as i64);
                continue;
            }
            if let Some(slot) = kept.get_mut(i) {
                *slot = Some(Entry { id: i.to_string(), domain: w.domain.into(), bytes: w.bytes, ms: w.ms, hits: seen, at: t as i64 });
            }
            let all: Vec<Entry> = kept.iter().flatten().cloned().collect();
            let mut used: BTreeMap<&str, u64> = BTreeMap::new();
            for e in &all {
                *used.entry(e.domain.as_str()).or_default() += e.bytes;
            }
            let over = |d: &str| used.get(d).copied().unwrap_or(0) > quotas.get(d).copied().unwrap_or(0);
            for id in policy(&all, quotas, 0) {
                let Some(slot) = id.parse::<usize>().ok().and_then(|j| kept.get_mut(j)) else { continue };
                if slot.as_ref().is_some_and(|e| !over(&e.domain)) {
                    crossed += 1;
                }
                *slot = None;
            }
        }
        (saved, crossed)
    }

    /// W24-AC12, simulated: a store under quota over a stream of lookups, in two domains each held
    /// to a tenth of what its units' entries would take. The units are small and large (1 MiB to
    /// 4 GiB), quick and slow (2 s to 30 min), and wanted by a Zipf law with ranks unrelated to
    /// either. For every seed, evicting by time saved per byte saves more time than evicting the
    /// least recently used in the same bytes, and no domain's entries go for another's pressure.
    #[test]
    fn w24_ac12_a_simulated_store_saves_more_than_least_recently_used() {
        const MIB: f64 = 1_048_576.0;
        for seed in [1u64, 7, 42, 2_024, 0x5eed] {
            let mut g = Gen(seed);
            let n = 240usize;
            let mut ranks: Vec<usize> = (1..=n).collect();
            for i in (1..ranks.len()).rev() {
                let j = (g.next() % (i as u64 + 1)) as usize;
                ranks.swap(i, j);
            }
            let works: Vec<Work> = ranks
                .iter()
                .enumerate()
                .map(|(i, rank)| Work { bytes: g.spread(MIB, 4_096.0 * MIB) as u64, ms: g.spread(2_000.0, 1_800_000.0) as u64, domain: if i % 4 == 0 { "person:ada" } else { "trusted" }, weight: 1.0 / *rank as f64 })
                .collect();
            let tenth = |d: &str| works.iter().filter(|w| w.domain == d).map(|w| w.bytes).sum::<u64>() / 10;
            let quotas = BTreeMap::from([("trusted".to_owned(), tenth("trusted")), ("person:ada".to_owned(), tenth("person:ada"))]);
            let (dense, crossed) = simulate(&works, 6_000, &quotas, seed, evict);
            let (recent, _) = simulate(&works, 6_000, &quotas, seed, lru);
            println!("w24_ac12: seed {seed}: by time saved per byte {} s saved, least recently used {} s ({:.2}x)", dense / 1_000, recent / 1_000, dense as f64 / (recent.max(1) as f64));
            assert_eq!(crossed, 0, "seed {seed}: a domain's entries went for another's pressure");
            assert!(dense >= recent, "seed {seed}: by time saved per byte {dense} ms, least recently used {recent} ms");
        }
    }
}
