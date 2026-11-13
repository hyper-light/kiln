//! The pools' bounds and starts (Ada's rule and the lead's calls of 2026-11-06; AC-T7.1.3, AC-T7.1.5): together the
//! pools hold no more than the memory the node may hold but for what a pool was seen to need; on the bench host none
//! starts below the constant it replaced; a node starts where its last start's peaks were, and a record it can't read
//! is a first start, counted.

use std::num::NonZeroUsize;

use hex_core::demand::Peaks;

use super::*;

fn host(memory: u64, parallelism: usize) -> Host {
    Host {
        memory,
        parallelism: NonZeroUsize::new(parallelism).unwrap(),
        threads: FEDORA_THREADS_MAX,
        descriptors: 524_288,
    }
}

/// A thread limit past the socket's need, as a large host's is: Linux sets `threads-max` from memory, one thread's
/// worth for every eight of its kernel stacks' (kernel/fork.c, `set_max_threads`), so a 64 GiB host has hundreds of
/// thousands.
const FEDORA_THREADS_MAX: u64 = 500_000;

fn first(h: Host) -> Sizes {
    Sizes::of(h, &Peaks::default())
}

/// Do: size the pools on hosts of any memory, a first start's. Expect: the pools at their bounds hold at most that
/// memory, but for the floor of the host's parallelism each keeps.
#[test]
fn the_pools_at_their_bounds_hold_no_more_than_the_node_may() {
    for memory in [
        0,
        1,
        1 << 20,
        100 << 20,
        1 << 30,
        7_500 << 20,
        64 << 30,
        u64::MAX / 2,
    ] {
        let s = first(host(memory, 8));
        let floor = first(host(0, 8)).bytes();
        assert!(
            s.bytes() <= memory.saturating_add(floor),
            "{s:?} holds {} of {memory}",
            s.bytes()
        );
    }
}

/// Do: size the pools on fedora, the bench host, as its bench units run the node (32 hardware threads, 8 GiB of
/// `MemoryMax` with about 7.5 GiB left), a first start's. Expect: no bound or start under the constant it replaces
/// (256 idle cgroups and namespaces, 32 seeds, pairs and namespaces made ahead, 8 cgroups made ahead, each owner's
/// 32: the lead's condition, 2026-11-06), and the spare at its floor of one.
#[test]
fn on_the_bench_host_no_pool_is_smaller_than_the_constant_it_replaced() {
    let s = first(host(7_500 << 20, 32));
    assert_eq!(s.start, 32);
    assert!(s.idle_cgroups >= 256, "{s:?}");
    assert!(s.idle_netns >= 256, "{s:?}");
    assert!(s.seeds.get() >= 32, "{s:?}");
    assert!(s.ipc_uts >= 32, "{s:?}");
    assert!(s.ahead >= 32, "{s:?}");
    assert!(s.netns >= 32, "decision 4's pool: {s:?}");
    assert!(s.workers >= 128, "the socket's workers: {s:?}");
    assert_eq!(s.starts.owner_cgroups, 32);
    assert_eq!(s.starts.spare, 1, "a first start's spare");
    assert_eq!(s.procs_alive(), s.idle_cgroups + s.ahead);
}

/// Do: size the pools on a host with no memory to spare. Expect: each at the host's parallelism, the floor a first
/// start starts at, and nothing past it.
#[test]
fn a_host_with_no_memory_to_spare_keeps_each_pool_at_its_floor() {
    let s = first(host(0, 4));
    assert_eq!(s.seeds.get(), 4);
    assert_eq!(
        (s.idle_cgroups, s.idle_netns, s.ahead, s.ipc_uts, s.netns),
        (4, 4, 4, 4, 4)
    );
}

/// The lead's call, 2026-11-06: the socket's calls in flight are bounded by the thread limit. Do: size the socket's
/// workers on hosts of many and of few threads. Expect: the thread limit, one at least; its descriptors the hard
/// limit's.
#[test]
fn the_socket_has_a_worker_for_every_thread_the_kernel_allows() {
    assert_eq!(first(host(7_500 << 20, 32)).workers, 500_000);
    let few = Host {
        threads: 100,
        ..host(7_500 << 20, 32)
    };
    assert_eq!(first(few).workers, 100);
    let none = Host {
        threads: 0,
        ..host(7_500 << 20, 32)
    };
    assert_eq!(first(none).workers, 1, "one worker, or no call runs");
    assert_eq!(first(host(1 << 30, 4)).descriptors, 524_288);
}

/// The lead's call, 2026-11-06 (AC-T7.1.3): a restart takes no cold miss its last start learned past. Do: size a node
/// whose last peaks were past the host's parallelism, and one whose were under it. Expect: each learned size starts
/// at its peak, never under the parallelism; the spare at its peak; and each bound at least its peak.
#[test]
fn a_node_starts_where_its_last_start_peaked() {
    let peaks = Peaks {
        owner_cgroups: 900,
        owner_netns: 40,
        netns: 700,
        ipc_uts: 50,
        ahead: 600,
        devfs_keys: 9,
        devfs_per_key: 64,
        seeds: 800,
        spare: 5,
        takes: [0; MEMORY_POOLS],
    };
    let s = Sizes::of(host(7_500 << 20, 32), &peaks);
    assert_eq!(
        s.starts,
        Starts {
            owner_cgroups: 900,
            owner_netns: 40,
            netns: 700,
            ipc_uts: 50,
            ahead: 600,
            devfs_keys: 32,
            devfs_per_key: 64,
            seeds: 800,
            spare: 5,
        }
    );
    assert!(
        s.idle_cgroups >= 900 && s.netns >= 700 && s.ahead >= 600,
        "{s:?}"
    );
    assert!(s.seeds.get() >= 800, "{s:?}");
}

/// The lead's call, 2026-11-06: shares follow demand. Do: size the pools after a start whose takes were all seeds'.
/// Expect: the seeds' bound holds the whole memory's worth of seeds, and every other pool its floor.
#[test]
fn a_pool_that_was_taken_from_gets_the_memory() {
    let mut peaks = Peaks::default();
    peaks.takes[4] = 1_000;
    let memory = 7_500 << 20;
    let s = Sizes::of(host(memory, 32), &peaks);
    assert_eq!(s.seeds.get(), usize::try_from(memory / SEED_BYTES).unwrap());
    assert_eq!((s.idle_cgroups, s.idle_netns), (32, 32));
}

/// The lead's call, 2026-11-06: peaks are kept in the node's state, crash-safe. Do: keep peaks in a state directory,
/// read them back; tear the record, write garbage, write past the bound. Expect: the same peaks; each bad record a
/// first start with its reason; none read at all a first start without one.
#[test]
fn peaks_kept_in_the_state_are_read_back_and_a_bad_record_is_a_first_start() {
    let cache =
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/sizes");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("peaks-")
        .tempdir_in(&cache)
        .unwrap();
    assert_eq!(read_peaks(dir.path()), Last::First);
    let peaks = Peaks {
        seeds: 77,
        spare: 4,
        takes: [1, 2, 3, 4, 5, 6, 7],
        ..Peaks::default()
    };
    write_peaks(dir.path(), &peaks).unwrap();
    assert_eq!(read_peaks(dir.path()), Last::Peaks(peaks));
    let path = dir.path().join("peaks");
    let text = peaks.encode();
    for bad in [
        text.get(..text.len() / 2).unwrap().to_owned(),
        "not a record\n".to_owned(),
        "x".repeat(Peaks::most_bytes() * 2 + 1),
    ] {
        std::fs::write(&path, &bad).unwrap();
        assert!(
            matches!(read_peaks(dir.path()), Last::Unread(_)),
            "{:?}",
            bad.get(..bad.len().min(40)).unwrap()
        );
    }
}

/// The lead's call, 2026-11-06: the keeper keeps the most a node learned and adds its takes. Do: open a keeper over a
/// record, offer this start's peaks, finish. Expect: the record holds each peak at its larger, the takes summed; a
/// keeper over an unreadable record starts from none and says why.
#[test]
fn the_keeper_keeps_the_larger_of_each_peak_and_sums_the_takes() {
    let cache =
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/sizes");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("keeper-")
        .tempdir_in(&cache)
        .unwrap();
    let before = Peaks {
        seeds: 50,
        spare: 2,
        takes: [10; MEMORY_POOLS],
        ..Peaks::default()
    };
    write_peaks(dir.path(), &before).unwrap();
    let mut keeper = Keeper::open(dir.path()).unwrap();
    assert_eq!(keeper.base(), before);
    let now = Peaks {
        seeds: 40,
        spare: 6,
        netns: 70,
        takes: [1; MEMORY_POOLS],
        ..Peaks::default()
    };
    keeper.finish(&now).unwrap();
    assert_eq!(
        read_peaks(dir.path()),
        Last::Peaks(Peaks {
            seeds: 50,
            spare: 6,
            netns: 70,
            takes: [11; MEMORY_POOLS],
            ..Peaks::default()
        })
    );
    std::fs::write(dir.path().join("peaks"), "torn").unwrap();
    let keeper = Keeper::open(dir.path()).unwrap();
    assert_eq!(keeper.base(), Peaks::default());
    assert!(keeper.unread().is_some(), "a first start, with why");
}

/// The device programs a node holds, one per policy a create used (Ada's 2026-11-06 constants rule; AC-T7.1.5,
/// CLAUDE.md §2): a memory pool as the others are, what fits of [`PROGRAM_BYTES`] each in its demand-weighted share,
/// within the descriptors, never under the host's parallelism; no longer a fixed 64. Do: size a node on fedora's
/// figures and on a host of 1 GiB and 4 threads, a first start's (an even share of seven pools); then after a start
/// that loaded programs and took nothing else; on a host whose descriptors are few, and fit its start to them; and on
/// one with no memory to spare. Expect: the even share over a program's cost; all the memory's worth after the start
/// that took only programs; where the descriptors are fewer, the memory's share as sized and fewer as the start fits
/// every bound to the hard limit (the lead, 2026-11-11), the node's own within it; and the parallelism at the least.
#[test]
fn the_programs_held_are_bounded_by_their_memory_share_and_the_descriptors() {
    let share = |memory: u64| usize::try_from(memory / 7 / PROGRAM_BYTES).unwrap();
    assert_eq!(
        first(host(7_500 << 20, 32)).programs,
        hex_runtime::MAX_BOXES,
        "fedora's share holds 210,981, past a policy for each box the node may hold"
    );
    assert_eq!(first(host(1 << 30, 4)).programs, share(1 << 30));
    let only_programs = Peaks {
        takes: [0, 0, 0, 0, 0, 0, 9],
        ..Peaks::default()
    };
    assert_eq!(
        Sizes::of(host(256 << 20, 4), &only_programs).programs,
        usize::try_from((256u64 << 20) / PROGRAM_BYTES).unwrap(),
        "the programs' demand takes the whole memory"
    );
    let few = first(Host {
        descriptors: 1_000,
        ..host(7_500 << 20, 32)
    });
    assert_eq!(few.programs, hex_runtime::MAX_BOXES);
    let fitted = crate::netns::fit(crate::netns::Pools::rootless(few, 14), 1_000, 0).unwrap();
    assert!(fitted.sizes.programs < few.programs, "{fitted:?}");
    assert!(
        crate::netns::FilesBound::needed(fitted, 0).own <= 1_000,
        "{fitted:?}"
    );
    assert_eq!(first(host(0, 4)).programs, 4, "never under the parallelism");
}
