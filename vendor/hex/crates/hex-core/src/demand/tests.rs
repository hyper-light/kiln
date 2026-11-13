//! The learned sizes against plain models (CLAUDE.md §6: model-based tests with non-vacuity counters; Ada's rule of
//! 2026-11-06, AC-T7.1.3 and AC-T7.1.5): a size never falls, never passes its bound, never starts under its start,
//! and covers every demand it was shown; the host's facts parse as the kernel writes them and hostile text is
//! refused.

use proptest::prelude::*;

use super::*;

proptest! {
    /// Do: show a size random demands. Expect: it is the larger of its start and the largest demand, each held at
    /// the bound, and it never falls.
    #[test]
    fn a_learned_size_is_the_most_demand_seen_within_its_bound(
        start in 0usize..64,
        cap in 0usize..64,
        seen in proptest::collection::vec(0usize..128, 0..64),
    ) {
        let mut l = Learned::new(start, cap);
        let mut want = start.min(cap);
        for n in seen {
            let before = l.size();
            let rose = l.observe(n);
            want = want.max(n.min(cap));
            prop_assert_eq!(l.size(), want);
            prop_assert!(l.size() >= before);
            prop_assert_eq!(rose, l.size() > before);
            prop_assert!(l.size() <= cap);
        }
    }

    /// Do: drive a pool made ahead whose shell refills it fully between bursts and not at all within one, each
    /// burst a random number of takes. Expect: once a burst found the pool short, every later burst no deeper finds
    /// it full (no cold take), the size never falls nor passes the bound, and it covers the deepest burst with one
    /// to spare for each take that found it short.
    #[test]
    fn a_pool_that_ran_short_never_runs_short_again_at_that_depth(
        start in 0usize..16,
        cap in 0usize..96,
        bursts in proptest::collection::vec(0usize..48, 1..40),
    ) {
        let mut d = Depth::new(start, cap);
        let mut deepest_short = 0usize;
        for burst in bursts {
            let held_at_start = d.size();
            let before = d.size();
            let mut cold_in_burst = 0usize;
            for i in 0..burst {
                let cold = i >= held_at_start;
                if cold {
                    cold_in_burst += 1;
                }
                d.took(cold);
            }
            if burst <= deepest_short.min(cap) {
                prop_assert_eq!(cold_in_burst, 0, "a burst of {} ran short again", burst);
            }
            if cold_in_burst > 0 {
                deepest_short = deepest_short.max(burst);
                prop_assert_eq!(d.size(), (burst + cold_in_burst).min(cap).max(before));
            }
            prop_assert!(d.size() >= before);
            prop_assert!(d.size() <= cap);
            d.full();
        }
    }

    /// Do: evict and miss for random owners. Expect: the bound rises by one exactly when the owner missing has a
    /// ghost left, never past the cap, and no more owners keep ghosts than the cap.
    #[test]
    fn a_ghost_spent_raises_the_bound_by_one(
        start in 0usize..8,
        cap in 0usize..12,
        ops in proptest::collection::vec((any::<bool>(), 0u8..6), 0..200),
    ) {
        let mut g = Ghosts::new(start, cap);
        let mut model: std::collections::BTreeMap<u8, (u64, usize)> = Default::default();
        let mut seq = 0u64;
        let mut bound = start.min(cap);
        for (evict, o) in ops {
            let name = format!("o{o}");
            if evict {
                if cap > 0 {
                    seq += 1;
                    let count = model.remove(&o).map_or(1, |(_, c)| (c + 1).min(cap));
                    model.insert(o, (seq, count));
                    if model.len() > cap {
                        let oldest = *model.iter().min_by_key(|(_, (s, _))| *s).unwrap().0;
                        model.remove(&oldest);
                    }
                }
                g.evicted(&name);
            } else {
                let had = model.get_mut(&o).map(|(_, c)| {
                    *c -= 1;
                    *c
                });
                if had == Some(0) {
                    model.remove(&o);
                }
                let rose = g.missed(&name);
                if had.is_some() && bound < cap {
                    bound += 1;
                    prop_assert!(rose);
                } else {
                    prop_assert!(!rose);
                }
            }
            prop_assert_eq!(g.bound(), bound);
            prop_assert_eq!(g.owners(), model.len());
            prop_assert!(g.owners() <= cap);
        }
    }

    /// Hostile input (CLAUDE.md §6): no text makes the parsers panic.
    #[test]
    fn the_host_facts_parsers_take_any_text(a in ".{0,200}", b in ".{0,40}") {
        let _ = mem_available(&a);
        let _ = cgroup_headroom(&a, &b);
    }
}

/// Non-vacuity: a burst deeper than the start finds the pool short, and the next as deep doesn't.
#[test]
fn a_burst_past_the_start_teaches_the_pool_the_burst() {
    let mut d = Depth::new(4, 1_000);
    let mut cold = 0;
    for i in 0..10 {
        if d.took(i >= 4) {
            cold += 1;
        }
    }
    assert!(cold > 0);
    assert_eq!(d.size(), 16, "ten drawn and six found short");
    d.full();
    for _ in 0..10 {
        assert!(!d.took(false));
    }
}

#[test]
fn mem_available_reads_the_kernels_line_and_refuses_the_rest() {
    let meminfo =
        "MemTotal:       65536000 kB\nMemFree:         1000 kB\nMemAvailable:   32768000 kB\n";
    assert_eq!(mem_available(meminfo), Some(32_768_000 * 1024));
    for bad in [
        "",
        "MemAvailable: 12",
        "MemAvailable: 12 MB",
        "MemAvailable: x kB",
        "MemAvailable: 12 kB extra",
        "MemAvailable: 18446744073709551615 kB",
        "memavailable: 12 kB",
    ] {
        assert_eq!(mem_available(bad), None, "{bad:?}");
    }
}

#[test]
fn headroom_is_what_memory_max_leaves_and_none_without_a_bound() {
    assert_eq!(
        cgroup_headroom("8589934592\n", "1073741824\n"),
        Some(7_516_192_768)
    );
    assert_eq!(cgroup_headroom("100\n", "200\n"), Some(0));
    assert_eq!(cgroup_headroom("max\n", "200\n"), None);
    assert_eq!(cgroup_headroom("100\n", ""), None);
}

#[test]
fn a_share_splits_memory_evenly_and_names_no_item_of_no_size() {
    assert_eq!(share(1_000, 2, 100), 5);
    assert_eq!(share(1_000, 0, 100), 0);
    assert_eq!(share(1_000, 2, 0), 0);
    assert_eq!(
        share(u64::MAX, 1, 1),
        usize::try_from(u64::MAX).unwrap_or(usize::MAX)
    );
}

/// The lead's call, 2026-11-06 (AC-T7.1.3): a node's learned peaks outlive it. Do: encode peaks and read them back;
/// read torn, hostile and foreign text; read a record a later node wrote with a key this one doesn't know. Expect:
/// the same peaks; each bad record refused by type (a first start, for the shell to count), never a panic; an
/// unknown key passed over and a missing one read as none seen (the record changes append-only).
#[test]
fn peaks_outlive_a_node_and_a_bad_record_is_a_first_start() {
    let p = Peaks {
        owner_cgroups: 40,
        owner_netns: 41,
        netns: 70,
        ipc_uts: 71,
        ahead: 12,
        devfs_keys: 5,
        devfs_per_key: 33,
        seeds: 48,
        spare: 3,
        takes: [1, 2, 3, 4, 5, 6, u64::MAX],
    };
    let text = p.encode();
    assert!(text.len() <= Peaks::most_bytes(), "{}", text.len());
    assert_eq!(Peaks::decode(&text), Ok(p));
    let torn = text.get(..text.len() / 2).unwrap();
    assert!(Peaks::decode(torn).is_err(), "a torn record is no record");
    for bad in [
        "",
        "hex-peaks 2\n",
        "hex-sizes 1\nseeds 4\n",
        "hex-peaks 1\nseeds x\n",
        "hex-peaks 1\nseeds 4 4\n",
        "hex-peaks 1\nseeds 4\nseeds 5\n",
    ] {
        assert!(Peaks::decode(bad).is_err(), "{bad:?}");
    }
    let later = format!("{text}a-later-key 9\n");
    assert_eq!(Peaks::decode(&later), Ok(p));
    assert_eq!(
        Peaks::decode("hex-peaks 1\nend\n"),
        Ok(Peaks::default()),
        "a record of no keys: nothing seen"
    );
}

proptest! {
    /// Hostile input (CLAUDE.md §6): no text makes the record's reader panic, and what it accepts it writes back the
    /// same.
    #[test]
    fn the_peaks_reader_takes_any_text(t in ".{0,400}") {
        if let Ok(p) = Peaks::decode(&t) {
            prop_assert_eq!(Peaks::decode(&p.encode()), Ok(p));
        }
    }

    /// The lead's call, 2026-11-06: shares follow demand. Do: split memory among pools of random take counts and
    /// item costs. Expect: the shares never sum past the memory; a pool that took nothing gets nothing while others
    /// took; with no takes at all the split is even; and a pool's share grows with its takes times its cost.
    #[test]
    fn shares_are_weighted_by_takes_times_cost(
        memory in 0u64..(1 << 40),
        pools in proptest::collection::vec((0u64..1_000_000, 1u64..10_000_000), 1..8),
    ) {
        let shares = weighted(memory, &pools);
        prop_assert_eq!(shares.len(), pools.len());
        let total: u128 = shares.iter().map(|&s| u128::from(s)).sum();
        prop_assert!(total <= u128::from(memory));
        let any = pools.iter().any(|&(t, _)| t > 0);
        for (i, &(t, _)) in pools.iter().enumerate() {
            if any && t == 0 {
                prop_assert_eq!(shares[i], 0);
            }
            if !any {
                prop_assert_eq!(shares[i], memory / pools.len() as u64);
            }
        }
        for i in 0..pools.len() {
            for j in 0..pools.len() {
                let (wi, wj) = (
                    u128::from(pools[i].0) * u128::from(pools[i].1),
                    u128::from(pools[j].0) * u128::from(pools[j].1),
                );
                if wi > wj {
                    prop_assert!(shares[i] >= shares[j]);
                }
            }
        }
    }
}
