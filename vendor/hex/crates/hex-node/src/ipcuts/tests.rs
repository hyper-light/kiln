//! The node's ipc and uts namespace shell (goal 5 extended, step 4): its refill thread against the kernel this
//! test runs on, privileged or not.

use super::*;
use hex_core::patience::Patience;
use hex_testkit::waits::follow::{self, Party};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::PathBuf;

/// Waits for `fact`, one the refill thread makes on its own, measuring, its answer recorded under `name` for the census
/// to come, by the refill thread's life (the review's C-5): ended once no `hex-ipcuts` task of this process runs. A
/// look: the pool tells no event as its thread makes a pair or fails one, its counts and namespaces read.
fn wait_for(name: &'static str, what: &str, fact: impl FnMut() -> bool) {
    follow::until(&Patience::measuring(name), what, refill_thread, fact).unwrap();
}

/// The pool's refill thread as a party: live while a `hex-ipcuts` task of this process runs, read from procfs.
fn refill_thread() -> Party {
    let running = std::fs::read_dir("/proc/self/task").is_ok_and(|tasks| {
        tasks.flatten().any(|task| {
            let path = task.path();
            std::fs::read_to_string(path.join("comm")).is_ok_and(|c| c.trim() == "hex-ipcuts")
                && std::fs::read_to_string(path.join("stat")).is_ok_and(|stat| {
                    !matches!(
                        stat.rsplit_once(')')
                            .and_then(|(_, fields)| fields.split_whitespace().next()),
                        Some("Z" | "X" | "x")
                    )
                })
        })
    });
    if running { Party::Live } else { Party::Ended }
}

fn ns_of(fd: &OwnedFd) -> PathBuf {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd())).unwrap()
}

fn own(kind: &str) -> PathBuf {
    std::fs::read_link(format!("/proc/self/ns/{kind}")).unwrap()
}

/// The namespace of kind `kind` that this process's `hex-ipcuts` thread is in.
fn refill_thread_in(kind: &str) -> PathBuf {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|task| task.unwrap().path())
        .find(|task| {
            std::fs::read_to_string(task.join("comm")).is_ok_and(|c| c.trim() == "hex-ipcuts")
        })
        .map(|task| std::fs::read_link(task.join("ns").join(kind)).unwrap())
        .expect("the refill thread")
}

/// HEX-IPCUTS-1: the refill thread hands the pool each pair it makes, or counts each make that fails, and a create
/// is served either way. Do: start a pool of 2 and wait for its thread's first outcome. Expect, where the agent may
/// make these namespaces (as root): the pool fills to its bound; two takes hand over pairs that are neither this
/// process's nor each other's; the thread makes more in their place and ends up back in this process's own
/// namespaces, holding none it made. Expect, where it may not: one failed make, counted and nothing made; then a
/// take answered on the path, counted, which asks the thread for one more, and that make fails and is counted too.
#[test]
fn the_refill_thread_fills_the_pool_or_counts_each_make_that_fails() {
    let pool = IpcUtsPool::start(2).unwrap();
    wait_for("test.ipcuts.first_outcome", "first make", || {
        let c = pool.counts().unwrap();
        c.made_ahead + c.failed > 0
    });
    if pool.counts().unwrap().failed > 0 {
        assert_ne!(
            rustix::process::geteuid().as_raw(),
            0,
            "root makes namespaces ahead"
        );
        let c = pool.counts().unwrap();
        assert_eq!((c.made_ahead, c.failed), (0, 1), "{c:?}");
        assert!(pool.take().unwrap().is_none(), "a dry take");
        assert_eq!(pool.counts().unwrap().made_on_path, 1);
        wait_for(
            "test.ipcuts.second_failed_make",
            "second failed make",
            || pool.counts().unwrap().failed == 2,
        );
        return;
    }
    wait_for("test.ipcuts.filled", "the pool's bound", || {
        pool.counts().unwrap().made_ahead == 2
    });
    let first = pool.take().unwrap().expect("a pair made ahead");
    let second = pool.take().unwrap().expect("a second pair made ahead");
    for (kind, a, b) in [
        ("ipc", &first.ipc, &second.ipc),
        ("uts", &first.uts, &second.uts),
    ] {
        assert_ne!(ns_of(a), own(kind), "a {kind} namespace of its own");
        assert_ne!(ns_of(b), own(kind), "a {kind} namespace of its own");
        assert_ne!(ns_of(a), ns_of(b), "no two pairs share a {kind} namespace");
    }
    wait_for(
        "test.ipcuts.refilled",
        "makes in the taken ones' places",
        || pool.counts().unwrap().made_ahead == 4,
    );
    for kind in ["ipc", "uts"] {
        wait_for(
            "test.ipcuts.back_home",
            "the refill thread back home",
            || refill_thread_in(kind) == own(kind),
        );
    }
    let c = pool.counts().unwrap();
    assert_eq!((c.taken, c.made_on_path, c.failed), (2, 0, 0), "{c:?}");
}

/// HEX-IPCUTS-2: a pool with a bound of 0 starts no thread and serves nothing. Do: take from it, then say a container
/// makes its own. Expect: the take answered on the path; both counted as made on the path, nothing made or taken.
#[test]
fn a_bound_of_0_makes_none_and_counts_every_container() {
    let pool = IpcUtsPool::start(0).unwrap();
    assert!(pool.take().unwrap().is_none());
    pool.on_path().unwrap();
    let c = pool.counts().unwrap();
    assert_eq!(
        (c.made_ahead, c.taken, c.made_on_path, c.failed),
        (0, 0, 2, 0),
        "{c:?}"
    );
}

/// Ada's rule, 2026-11-06 (AC-T7.1.3): the mark the refill is woken at is derived from the recorded rates, not half
/// the bound. Do: the mark for sizes 0 to 4,096. Expect: the refill, making at its slowest recorded rate, makes the
/// pairs above the mark before the creates, taking at their fastest recorded rate, take the pairs at or under it;
/// and a pool of any size holds at least one at the mark.
#[test]
fn the_refill_is_woken_while_what_is_left_outlasts_its_refill() {
    assert_eq!(low(0), 0);
    for size in 1..=4_096usize {
        let mark = low(size);
        assert!((1..=size).contains(&mark), "{size}: {mark}");
        let (above, at) = (
            u64::try_from(size - mark).unwrap(),
            u64::try_from(mark).unwrap(),
        );
        // Time to make what's above the mark against time to take what's at or under it, in common units.
        assert!(
            above * TAKES_PER_SECOND <= at * MAKES_PER_SECOND,
            "{size}: the refill of {above} outlasts the {at} left"
        );
    }
}
