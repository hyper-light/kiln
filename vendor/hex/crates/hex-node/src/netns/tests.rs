//! The node's network-namespace shell (decision 4, goal 4 plan step 4): its refill thread against the
//! kernel this test runs on, privileged or not.

use super::*;
/// The slowest recorded make of a network namespace by the pool's refill thread, in these tests (the lead's rule
/// of 2026-11-06 for test patiences).
/// The tests run debug builds, so their record is the debug one: the `netns.make` row of
/// `bench/competitive/probes/waits_census.md` run 3's debug arm, 120.814 ms of 47,210 (a release-build record ran these
/// tests' waits out).
const MAKE_RECORDED: Option<Duration> = Some(Duration::from_nanos(120_814_130));

/// Longest a test waits for the refill thread's fact: twice [`MAKE_RECORDED`]; unbounded while measuring.
const PATIENCE: Duration = match MAKE_RECORDED {
    Some(slowest) => slowest.saturating_mul(2),
    None => Duration::MAX,
};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

use crate::sizes::{Host, Sizes};

/// The most descriptors a process on this host may raise its limit to (`/proc/sys/fs/nr_open`; the kernel's
/// Documentation/admin-guide/sysctl/fs.rst), so a limit the files' bound is read under is one this host allows.
fn nr_open() -> u64 {
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The pools' bounds on fedora, the bench host, as its bench units run the node: 32 hardware threads, about 7.5 GiB
/// of memory to hold, and a thread limit past the socket's need (`crate::sizes`).
fn bench() -> Sizes {
    Sizes::of(
        Host {
            memory: 7_500 << 20,
            parallelism: std::num::NonZeroUsize::new(32).unwrap(),
            threads: 500_000,
            descriptors: 524_288,
        },
        &hex_core::demand::Peaks::default(),
    )
}

/// Ada's rule and the lead's calls, 2026-11-06 and 2026-11-11 (CLAUDE.md §2; goal 6 step 6's installer reads it): the
/// descriptors a root node holds at its learned pools' bounds on fedora's figures, a first start's, its state on one
/// disk. Do: bound the files under no limit at the bench host's sizes, nothing held beside its bounds. Expect: the
/// node's own 941,655. Of it 406,027 as counted before the lead's rulings of 2026-11-11 (the pools' shares a seventh
/// each with the device programs among them, 440 seeds and 1,439 idle namespaces; the programs held, 65,536 at their
/// bound; the create seeds' one pruned mount namespace, `goal6-mntns.md` §12); and now the 633 idle cgroups' parked
/// inits, four each (an init's pidfd and its socket's end, its proc's pid namespace and tree), 2,532; the 440 cgroups
/// made ahead, twenty each (those four, the cgroup's directory, its `memory.peak` and the fourteen files it opens ahead
/// over one disk), 8,800; the socket's connections, 65,536 (the inits' budget, under its 524,288 descriptors and
/// 500,000 threads) with the one it refuses past them and its own three, 65,540; the 65,536 program places' streams,
/// six each (a pipe of two for each of three), and the message read past them, three, 393,219; the exec'd programs'
/// pidfds the supervisor keeps, one a place, 65,536; and the supervisor's epoll set, one. Every kept namespace there
/// may be holding its files four each past that, 1,215,311 in all.
#[test]
fn the_nodes_own_descriptors_on_fedoras_figures() {
    let unlimited = rustix::process::Rlimit {
        current: None,
        maximum: None,
    };
    let b = FilesBound::of(&unlimited, root_pools(), 0);
    assert_eq!(b.own, 941_655, "{:?}", bench());
    assert_eq!(
        b.own - 406_027,
        633 * 4 + 440 * 20 + 65_540 + (65_536 * 6 + 3) + 65_536 + 1
    );
    assert_eq!(b.own + b.namespaces * FILES_A_KEPT, 1_215_311);
}

/// Goal 6 step 6's HEX-INST-5 (Ada's 2026-11-06 constants rule): what a node needs of its soft descriptor limit is the
/// node's to say, from its own bounds. Do: ask what a root node and a rootless one need on fedora's figures, nothing
/// held beside. Expect: the root node the bound's own figures under no limit, its own and four files for each of its
/// kept namespaces, 1,215,311 in all; the rootless node its own alone, keeping no namespaces.
#[test]
fn a_node_says_the_descriptors_it_needs() {
    let unlimited = rustix::process::Rlimit {
        current: None,
        maximum: None,
    };
    let bound = FilesBound::of(&unlimited, root_pools(), 0);
    let root = FilesBound::needed(root_pools(), 0);
    assert_eq!(
        root,
        Needed {
            own: bound.own,
            files: bound.namespaces * FILES_A_KEPT
        }
    );
    assert_eq!(root.total(), 1_215_311);
    let rootless = FilesBound::needed(Pools::rootless(bench(), files_ahead()), 0);
    assert_eq!(rootless.files, 0, "a rootless node keeps no namespaces");
    assert_eq!(rootless.total(), rootless.own);
}

/// The lead's ruling of 2026-11-11 (AC-T7.1.1): a node's own fit its hard descriptor limit by construction. Do: fit a
/// root node's pools on fedora's figures to a hard limit holding its own at those bounds, and to one past it. Expect:
/// its bounds as given, its socket's bound on connections lowered to what its programs' and calls' budgets admit (the
/// inits', 65,536, under its 524,288 descriptors and 500,000 threads), and its own and every namespace's files within
/// the limit.
#[test]
fn a_node_whose_own_fit_its_hard_limit_keeps_its_bounds() {
    use rustix::process::Rlimit;

    let pools = root_pools();
    let whole = descriptors_of_the_node(pools, 3);
    for hard in [whole, whole + 1_000] {
        let fitted = fit(pools, hard, 3).unwrap();
        assert_eq!(fitted.sizes.descriptors, 65_536, "at {hard}");
        assert_eq!(
            fitted,
            Pools {
                sizes: Sizes {
                    descriptors: 65_536,
                    ..pools.sizes
                },
                ..pools
            },
            "at {hard}"
        );
        let files = FilesBound::of(
            &Rlimit {
                current: Some(hard),
                maximum: Some(hard),
            },
            fitted,
            3,
        );
        assert!(
            files.own + files.namespaces * FILES_A_KEPT <= hard,
            "{files:?} at {hard}"
        );
    }
}

/// The lead's ruling of 2026-11-11 (AC-T7.1.1; the FC lane's guests, whose init started `hex node` under the kernel's
/// 1,024 and 4,096): a node under a hard limit short of its own at its host's bounds starts with every bound that
/// holds descriptors fitted to it. Do: fit a root node's pools and a rootless one's on fedora's figures, 40 held
/// beside, to the least node's descriptors and one past, to 1,024, to a guest's 4,096 and to 65,536. Expect: the node's
/// own, and its namespaces' files beside, within each limit; the share the largest that fits; at least one box, init
/// and connection; no bound past its host's or an init past the boxes; and no bound lower under a longer limit.
#[test]
fn a_node_under_a_short_hard_limit_fits_every_bound_that_holds_descriptors_to_it() {
    use rustix::process::Rlimit;

    let beside = 40;
    for pools in [root_pools(), Pools::rootless(bench(), files_ahead())] {
        let whole = descriptors_of_the_node(pools, beside);
        let least = descriptors_of_the_node(shrunk(pools, 0, 1), beside);
        let mut shorter: Option<Sizes> = None;
        for hard in [least, least + 1, 1_024, 4_096, 65_536] {
            assert!(hard < whole, "{hard} is short of the node's own, {whole}");
            let fitted = fit(pools, hard, beside).unwrap();
            let own = descriptors_of_the_node(fitted, beside);
            assert!(own <= hard, "its own {own} within {hard}");
            let at = share(pools, hard, beside);
            assert_eq!(
                Pools {
                    sizes: Sizes {
                        descriptors: fitted.sizes.descriptors,
                        ..shrunk(pools, at, whole).sizes
                    },
                    ..shrunk(pools, at, whole)
                },
                fitted
            );
            assert!(
                descriptors_of_the_node(shrunk(pools, at + 1, whole), beside) > hard,
                "the share past {at} doesn't fit {hard}"
            );
            let files = FilesBound::of(
                &Rlimit {
                    current: Some(hard),
                    maximum: Some(hard),
                },
                fitted,
                beside,
            );
            assert!(
                files.own + files.namespaces * FILES_A_KEPT <= hard,
                "{files:?} at {hard}"
            );
            let s = fitted.sizes;
            let given = pools.sizes;
            assert!(s.boxes >= 1 && s.inits >= 1 && s.descriptors >= 1, "{s:?}");
            assert!(s.inits <= s.boxes && s.programs <= s.boxes, "{s:?}");
            assert_eq!(s.descriptors, connections(&s), "{s:?}");
            let bounds = |s: Sizes| {
                [
                    s.boxes,
                    s.inits,
                    s.programs,
                    s.seeds.get(),
                    s.idle_cgroups,
                    s.ahead,
                    s.idle_netns,
                    s.netns,
                    s.ipc_uts,
                    s.descriptors,
                ]
            };
            for (fitted, given) in bounds(s).into_iter().zip(bounds(given)) {
                assert!(fitted <= given, "{s:?} within {given:?}");
            }
            if let Some(shorter) = shorter {
                for (longer, shorter) in bounds(s).into_iter().zip(bounds(shorter)) {
                    assert!(longer >= shorter, "{s:?} at {hard} under {shorter:?}");
                }
            }
            shorter = Some(s);
        }
    }
}

/// The lead's ruling of 2026-11-11 (AC-T7.1.1): a node's start is refused only where its hard limit can't hold the
/// least node, one box and one connection, and what refuses it carries that node's descriptors. Do: fit a root node's
/// pools and a rootless one's on fedora's figures, 40 held beside, to the least node's descriptors and to one fewer.
/// Expect: at them, one box, init, device program, seed and connection, and nothing made ahead; one fewer, refused
/// with the least node's count.
#[test]
fn a_node_under_a_hard_limit_short_of_the_least_node_is_refused_with_its_count() {
    let beside = 40;
    for pools in [root_pools(), Pools::rootless(bench(), files_ahead())] {
        let least = descriptors_of_the_node(shrunk(pools, 0, 1), beside);
        assert_eq!(fit(pools, least - 1, beside), Err(least));
        let fitted = fit(pools, least, beside).unwrap();
        let s = fitted.sizes;
        assert_eq!(
            (s.boxes, s.inits, s.programs, s.seeds.get(), s.descriptors),
            (1, 1, 1, 1, 1),
            "{s:?}"
        );
        assert_eq!(
            (
                s.idle_cgroups,
                s.ahead,
                s.idle_netns,
                s.netns,
                s.ipc_uts,
                fitted.netns,
                fitted.ipc_uts
            ),
            (0, 0, 0, 0, 0, 0, 0),
            "{fitted:?}"
        );
    }
}

/// The lead's ruling of 2026-11-11 (AC-T7.1.1): a node at the bounds its start fitted holds, with every admission at its
/// most, no more than its hard limit, so its own count holds every program place's streams, the message read past the
/// places, and every exec'd program's pidfd the supervisor keeps; and where it runs VM boxes, each one's past a box's
/// and their root's. Do: fit a root node's pools and a rootless one's on fedora's figures, 40 held beside, to a guest's
/// 4,096 and to 65,536, running VM boxes and not; count those at the fitted admission bounds (`serve::limits`, the
/// wire's three descriptors a message). Expect: the node's own at least those, and within the limit; running VM boxes,
/// its own counts seven more a box (each an init's row) and two for their root, at the same bounds.
#[test]
fn a_node_at_its_fitted_bounds_counts_every_program_place_within_its_hard_limit() {
    let beside = 40;
    let streams = u64::try_from(hex_core::wire::MAX_FDS).unwrap();
    let n = |count: usize| u64::try_from(count).unwrap();
    for pools in [root_pools(), Pools::rootless(bench(), files_ahead())] {
        for hard in [4_096, 65_536] {
            let fitted = fit(pools, hard, beside).unwrap();
            let places = n(crate::serve::limits(&fitted.sizes.platform()).programs);
            let own = descriptors_of_the_node(fitted, beside);
            let socket_and_supervisor = places * (1 + 2 * streams + 1) + streams;
            assert!(
                own >= socket_and_supervisor && own <= hard,
                "{own} counts {socket_and_supervisor} for {places} places within {hard}"
            );
            let vms = fitted.running_vms(true);
            assert_eq!(
                descriptors_of_the_node(vms, beside) - own,
                n(fitted.sizes.inits) * 7 + 2,
                "at {hard}"
            );
            let with_vms = fit(pools.running_vms(true), hard, beside).unwrap();
            assert!(
                descriptors_of_the_node(with_vms, beside) <= hard,
                "running VM boxes, fitted within {hard}: {with_vms:?}"
            );
            assert!(with_vms.sizes.inits <= fitted.sizes.inits, "at {hard}");
        }
    }
}

/// A root node's pools on the bench host.
fn root_pools() -> Pools {
    Pools::root(bench(), files_ahead())
}

/// The files each cgroup made ahead opens on the bench host, its state on one disk (its `io.latency` and `io.max`
/// among them; `hex_runtime::linux::cgroup::ahead_files`).
fn files_ahead() -> usize {
    hex_runtime::linux::cgroup::ahead_files(&[hex_core::cgroup::Dev {
        major: 259,
        minor: 0,
    }])
    .len()
}

/// Waits for `fact`, giving the CPU up between looks rather than sleeping a picked period, for at most
/// [`PATIENCE`], the longest a case waits for a fact: a thread that never makes it true fails the test
/// instead of hanging it.
fn wait_for(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = Instant::now().checked_add(PATIENCE);
    while deadline.is_none_or(|d| Instant::now() < d) {
        if fact() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("no {what} within {PATIENCE:?}");
}

/// Whether loopback is up in network namespace `ns`: a thread of its own joins it, then listens on
/// 127.0.0.1 and connects, which a namespace whose loopback is down refuses (net.rs).
fn loopback_up_in(ns: &OwnedFd) -> bool {
    let ns = ns.try_clone().unwrap();
    std::thread::spawn(move || {
        move_into_link_name_space(ns.as_fd(), Some(LinkNameSpaceType::Network)).unwrap();
        // The listener lives until the connect is answered.
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| std::net::TcpStream::connect(listener.local_addr()?))
            .is_ok()
    })
    .join()
    .unwrap()
}

/// The step at which this process's make ahead (`net::make_ahead`) fails, if one does: unshare(2)
/// takes `CLONE_NEWNET` only from a caller with CAP_SYS_ADMIN in its user namespace, and rtnetlink
/// takes loopback's RTM_NEWLINK only with CAP_NET_ADMIN (rtnetlink(7)). uid 0 alone gives neither: a
/// container runtime's default set, a Kubernetes pod's root's (kiln CI's), holds neither.
fn failing_step() -> Option<(Step, hex_core::netns::Missing)> {
    use rustix::thread::CapabilitySet;
    let held = rustix::thread::capabilities(None).unwrap().effective;
    let (sys_admin, net_admin) = (
        held.contains(CapabilitySet::SYS_ADMIN),
        held.contains(CapabilitySet::NET_ADMIN),
    );
    let step = if !sys_admin {
        Step::Unshare
    } else {
        Step::Loopback
    };
    hex_core::netns::Missing::of(sys_admin, net_admin).map(|missing| (step, missing))
}

/// HEX-NETNS-14: the agent's stderr line for a failed make names the step and why, errno and text.
#[test]
fn a_failed_make_is_said_with_its_step_and_reason() {
    let reason = Reason::new(
        Some(1),
        "unshare failed: Operation not permitted (os error 1)",
    );
    assert_eq!(
        failed_line(Step::Unshare, &reason),
        "hex node: a network namespace made ahead failed at unshare: errno 1: unshare failed: \
         Operation not permitted (os error 1)\n"
    );
}

/// HEX-NETNS-11: the refill thread hands the pool each namespace it makes, or counts each make that
/// fails, and a create is served either way. Do: start a pool of 2 and wait for its thread's first
/// outcome. Expect, where the agent may make network namespaces (CAP_SYS_ADMIN and CAP_NET_ADMIN):
/// the pool fills to its bound; a take hands over a namespace that isn't this process's, its loopback
/// up, and the thread makes another in its place. Expect, where it may not: one failed make, counted
/// under the step the missing capability fails (HEX-NETNS-12) and nothing made; then a take answered
/// on the path, counted, which asks the thread for one more, and that make fails and is counted too.
#[test]
fn the_refill_thread_fills_the_pool_or_counts_each_make_that_fails() {
    let pool = Netns::start(2, FilesBound::now(root_pools()).unwrap(), bench()).unwrap();
    wait_for("first make", || {
        let c = pool.counts().unwrap();
        c.made_ahead + c.failed > 0
    });
    let failing = failing_step();
    assert_eq!(
        pool.counts().unwrap().failed > 0,
        failing.is_some(),
        "a make fails exactly where CAP_SYS_ADMIN or CAP_NET_ADMIN is missing"
    );
    if let Some((step, missing)) = failing {
        hex_testkit::skip(
            "netns::tests::the_refill_thread_fills_the_pool_or_counts_each_make_that_fails",
            &missing.to_string(),
            "its full pool (made ahead, taken, loopback up) isn't tested here; its failed makes are",
        )
        .unwrap();
        let c = pool.counts().unwrap();
        assert_eq!((c.made_ahead, c.failed), (0, 1), "{c:?}");
        let mut by = hex_core::netns::FailedBy::default();
        let last = match step {
            Step::Unshare => {
                by.unshare = 1;
                &c.last_failed.unshare
            }
            _ => {
                by.loopback = 1;
                &c.last_failed.loopback
            }
        };
        assert_eq!(c.failed_by, by, "counted under the step that failed");
        // HEX-NETNS-14: the kernel's refusal is kept, errno and text: EPERM, a capability missing.
        let reason = last.as_ref().expect("the step's reason is kept");
        let eperm = rustix::io::Errno::PERM.raw_os_error();
        assert_eq!(reason.errno(), Some(eperm), "{reason}");
        assert!(reason.text().contains("not permitted"), "{reason}");
        assert!(pool.take().unwrap().is_none(), "a dry take");
        assert_eq!(pool.counts().unwrap().made_on_path, 1);
        wait_for("second failed make", || pool.counts().unwrap().failed == 2);
        return;
    }
    wait_for("the pool's bound", || {
        pool.counts().unwrap().made_ahead == 2
    });
    let ns = pool.take().unwrap().expect("a namespace made ahead");
    assert_ne!(
        std::fs::read_link(format!("/proc/self/fd/{}", ns.as_fd().as_raw_fd())).unwrap(),
        std::fs::read_link("/proc/self/ns/net").unwrap(),
        "a namespace of its own"
    );
    assert!(loopback_up_in(&ns), "its loopback is up");
    wait_for("a make in the taken one's place", || {
        pool.counts().unwrap().made_ahead == 3
    });
    let c = pool.counts().unwrap();
    assert_eq!((c.taken, c.made_on_path, c.failed), (1, 0, 0), "{c:?}");
}

/// Goal 6 (`goal6-mntns.md` §12): a root node holds its create seeds' one pruned mount namespace for its life, so
/// descriptors_of_the_node counts it. Do: read the node's named descriptor terms under a root node's pools. Expect the
/// pruned namespace among them, one descriptor, and the total the terms' sum.
#[test]
fn the_seeds_pruned_namespace_is_counted_in_what_a_node_may_hold() {
    let root = descriptor_terms(root_pools(), 0);
    assert!(
        root.contains(&("the create seeds' pruned mount namespace", 1)),
        "{root:?}"
    );
    let sum = root.iter().map(|(_, n)| *n).fold(0, u64::saturating_add);
    assert_eq!(descriptors_of_the_node(root_pools(), 0), sum);
}

/// Goal 5 extended step 3 (AC-e's reader; T-T7.1.4): a create of an owner's box that finds the pool dry is
/// given a namespace the node makes on its path, which the box's init joins as it joins one from the pool,
/// and the node counts it apart from the pool's other takes made on the path, whose containers make their
/// own. Do: a node keeping namespaces, its pool empty and never refilled; take for an owner's box that may
/// share. Expect, where the agent may make network namespaces (as root): a namespace that isn't this
/// process's, its loopback up, kept for the owner, counted once by the pool as made on the path and once by
/// the node; where it may not: the take refused, the pool's count risen and the node's not.
#[test]
fn a_dry_take_for_an_owners_box_is_made_by_the_node_and_counted() {
    let state = Arc::new(KeptState {
        idle: Mutex::new(Idle::new(bench().starts.owner_netns, bench().idle_netns)),
        held: Mutex::new(BTreeMap::new()),
        given_back: AtomicU64::new(0),
        checked: AtomicU64::new(0),
        unchecked: AtomicU64::new(0),
        lost_home: AtomicU64::new(0),
        made_on_path: AtomicU64::new(0),
        protocols: AtomicU64::new(0),
        through_files: AtomicU64::new(0),
        files_failed: AtomicU64::new(0),
        files_bounded: AtomicU64::new(0),
        files_bound: FilesBound::now(root_pools()).unwrap(),
        files_held: Arc::new(AtomicU64::new(0)),
        check_ns: AtomicU64::new(0),
    });
    let netns = Netns {
        shared: Arc::new(Shared {
            pool: Mutex::new(Pool::new(1)),
            wake: Condvar::new(),
        }),
        refill: None,
        kept: Some(Keeping {
            state,
            returns: None,
            checkers: Vec::new(),
        }),
    };
    let owner = BoxOwner::named("dry-owner").unwrap();
    let choice = netns.choice(Some(&owner));
    let taken = choice.take_for(Ok(()));
    let pool = netns.counts().unwrap();
    let node = netns.reuse_counts().unwrap().unwrap();
    assert_eq!(
        (pool.taken, pool.made_on_path),
        (0, 1),
        "the pool counts the dry take: {pool:?}"
    );
    match taken {
        Ok(Some(ns)) => {
            assert_eq!(node.made_on_path, 1, "the node counts its make: {node:?}");
            assert_ne!(
                std::fs::read_link(format!("/proc/self/fd/{}", ns.as_fd().as_raw_fd())).unwrap(),
                std::fs::read_link("/proc/self/ns/net").unwrap(),
                "a namespace of its own"
            );
            assert!(loopback_up_in(&ns), "its loopback is up");
            assert!(choice.kept().is_some(), "kept for the owner's next box");
        }
        Ok(None) => panic!("a take for an owner's box that may share is never left to its clone"),
        Err(e) => {
            assert_ne!(
                rustix::process::geteuid().as_raw(),
                0,
                "root makes namespaces: {e}"
            );
            assert_eq!(
                node.made_on_path, 0,
                "nothing made, nothing counted: {node:?}"
            );
        }
    }
}

/// systemd's default soft limit on a service's descriptors: `DefaultLimitNOFILE=` is `1024:524288`
/// (systemd-system.conf(5), "DefaultLimitNOFILE=", since v240), the soft half the one a unit gets unless set.
const SYSTEMD_SOFT_NOFILE: u64 = 1_024;

/// Ada's 2026-11-06 rule (CLAUDE.md §2): the descriptors the agent holds past its bounds are counted, not
/// stated. Do: hold eight files open and count; count under a soft limit of none. Expect: at least the eight;
/// the count refused by type, naming the limit, since more are listed than it allows.
#[test]
fn the_descriptors_beside_the_bounds_are_counted() {
    use rustix::process::Rlimit;

    let held: Vec<std::fs::File> = (0..8)
        .map(|_| std::fs::File::open("/proc/self/stat").unwrap())
        .collect();
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let counted = open_descriptors(&limit).unwrap();
    assert!(counted >= 8, "{counted} counted with eight held");
    let none = Rlimit {
        current: Some(0),
        maximum: Some(0),
    };
    assert!(
        matches!(open_descriptors(&none), Err(Refusal::Os { why, .. }) if why.contains("soft limit's 0")),
        "past the soft limit the count is refused by type"
    );
    drop(held);
}

/// Goal 5 extended step 6, the lead's second condition for a kept namespace's files as he restated it
/// (2026-11-03T23:07Z, 2026-11-04T09:15Z; CLAUDE.md §2, §3): the limit the node was given stays as it is, and what
/// it leaves past the node's own descriptors at its bounds is how many namespaces may hold their four files. Do: ask
/// of a soft limit at systemd's default, of one just past the node's own, of one leaving room for two namespaces
/// and a little more, and of none. Expect: none hold files at the default; two at the room for two; every namespace
/// there may be, under no limit; the node's own the same each time and past 65,536 boxes' four descriptors.
#[test]
fn the_files_bound_is_what_the_soft_limit_leaves_past_the_node_four_a_namespace() {
    use rustix::process::Rlimit;

    let beside = open_descriptors(&rustix::process::getrlimit(
        rustix::process::Resource::Nofile,
    ))
    .unwrap();

    let at = |soft: Option<u64>| {
        FilesBound::of(
            &Rlimit {
                current: soft,
                maximum: soft,
            },
            root_pools(),
            beside,
        )
    };
    let own = at(Some(SYSTEMD_SOFT_NOFILE)).own;
    let boxes = u64::try_from(hex_runtime::MAX_BOXES).unwrap();
    assert!(
        own >= boxes * 4,
        "each live box's own descriptors and its kept namespace's are in it: {own}"
    );
    assert_eq!(
        at(Some(SYSTEMD_SOFT_NOFILE)).namespaces,
        0,
        "systemd's default holds none"
    );
    assert_eq!(at(Some(own)).namespaces, 0, "just the node's own");
    assert_eq!(
        at(Some(own + 3)).namespaces,
        0,
        "short of one namespace's four"
    );
    assert_eq!(at(Some(own + 2 * 4 + 3)).namespaces, 2);
    let unlimited = at(None);
    assert_eq!(unlimited.soft, None);
    assert_eq!(unlimited.own, own);
    assert!(unlimited.namespaces >= boxes, "{unlimited:?}");
    assert_eq!(
        at(Some(u64::MAX)).namespaces,
        unlimited.namespaces,
        "never more than there may be namespaces"
    );
}

/// Goal 5 extended step 6, the files' bound past the node's own descriptors (CLAUDE.md §2; the lead's finding of
/// 2026-11-05T07:50Z on the composed tree): decision 4's network namespaces and step 4's ipc and uts pairs made ahead
/// are the node's own too. Do: bound the files under one soft limit with no pools, with each pool alone, with both at
/// a root node's bounds, and with each pool's bound raised. Expect: the node's own grows by each pool's held
/// descriptors, one a network namespace and two a pair, with its refill thread's and each call's takes beside, and
/// the namespaces holding files fall to what that leaves.
#[test]
fn the_files_bound_counts_the_namespaces_both_pools_make_ahead() {
    use rustix::process::Rlimit;

    let beside = open_descriptors(&rustix::process::getrlimit(
        rustix::process::Resource::Nofile,
    ))
    .unwrap();

    let at = |netns: usize, ipc_uts: usize| {
        FilesBound::of(
            &Rlimit {
                current: Some(nr_open()),
                maximum: Some(nr_open()),
            },
            Pools {
                netns,
                ipc_uts,
                devfs: true,
                sizes: bench(),
                files_ahead: files_ahead(),
                vms: false,
            },
            beside,
        )
    };
    let none = at(0, 0);
    let n = |count: usize| u64::try_from(count).unwrap();
    let takers = n(bench().seeds.get());
    assert_eq!(
        at(bench().netns, 0).own - none.own,
        n(bench().netns) + 2 + takers
    );
    assert_eq!(
        at(0, bench().ipc_uts).own - none.own,
        2 * (n(bench().ipc_uts) + 2) + 2 * takers
    );
    let root = at(bench().netns, bench().ipc_uts);
    assert_eq!(
        root.own - none.own,
        n(bench().netns) + 2 + 2 * (n(bench().ipc_uts) + 2) + 3 * takers
    );
    assert_eq!(
        root,
        FilesBound::of(
            &Rlimit {
                current: Some(nr_open()),
                maximum: Some(nr_open())
            },
            root_pools(),
            beside
        )
    );
    let raised = at(4_096, 4_096);
    assert_eq!(
        raised.own - root.own,
        (4_096 - n(bench().netns)) + 2 * (4_096 - n(bench().ipc_uts))
    );
    let soft = raised.own + 10 * FILES_A_KEPT + 3;
    let under = |pools| {
        FilesBound::of(
            &Rlimit {
                current: Some(soft),
                maximum: Some(soft),
            },
            pools,
            beside,
        )
        .namespaces
    };
    assert_eq!(
        under(Pools {
            netns: 4_096,
            ipc_uts: 4_096,
            devfs: true,
            sizes: bench(),
            files_ahead: files_ahead(),
            vms: false,
        }),
        10,
        "what the limit leaves past the pools, four a namespace"
    );
    assert_eq!(
        under(root_pools()),
        10 + (raised.own - root.own + 3) / FILES_A_KEPT
    );
}

/// Goal 5 of record, the files' bound past the node's own descriptors (CLAUDE.md §2; the lead, 2026-11-05T11:45Z):
/// the mounts made ahead are the node's own too, step 3's prepared `/dev` pool and goal 6's prepared proc pool,
/// with their mask sources. Do: bound the files under one soft limit with the namespace pools at a root node's
/// bounds, with and without the mounts made ahead. Expect: the node's own grows by both pools held at their bounds
/// (the seeds' bound each, one descriptor a `/dev`, two a proc: its pid namespace and its tree), the one maker's peak
/// while it makes a proc (six), the two mask sources, each call's takes of a `/dev` and a proc beside, and the proc of
/// every init parked in an idle cgroup or one made ahead, two each (decision 8, parked procs, which the pool makes); a
/// rootless node keeps none.
#[test]
fn the_files_bound_counts_the_mounts_made_ahead() {
    use rustix::process::Rlimit;

    let beside = open_descriptors(&rustix::process::getrlimit(
        rustix::process::Resource::Nofile,
    ))
    .unwrap();

    let at = |devfs: bool| {
        FilesBound::of(
            &Rlimit {
                current: Some(1 << 22),
                maximum: Some(1 << 22),
            },
            Pools {
                devfs,
                ..root_pools()
            },
            beside,
        )
    };
    let n = |count: usize| u64::try_from(count).unwrap();
    let held = n(bench().seeds.get());
    assert_eq!(
        at(true).own - at(false).own,
        held + 2 * held + 6 + 2 + 3 * n(bench().seeds.get()) + 2 * n(bench().procs_alive())
    );
    assert!(
        root_pools().devfs,
        "a root node keeps the mounts made ahead"
    );
}

/// Goal 5 extended step 6, the files' bound past the node's own descriptors (CLAUDE.md §2; the lead, 2026-11-05T11:58Z):
/// step 3's prepared `/dev` mounts are the node's own too. Do: bound the files under one soft limit with the namespace
/// pools at a root node's bounds, with and without the mounts made ahead. Expect: the node's own grows by at least the
/// `/dev` pool held at its bound (the seeds' bound, one descriptor a `/dev`), its maker's peak (two: its context and
/// its tree) and each call's take of a `/dev`; here the proc pool and mask sources are counted beside them
/// ([`the_files_bound_counts_the_mounts_made_ahead`]), and the one maker's peak is a proc's.
#[test]
fn the_files_bound_counts_the_prepared_dev_mounts() {
    use rustix::process::Rlimit;

    let beside = open_descriptors(&rustix::process::getrlimit(
        rustix::process::Resource::Nofile,
    ))
    .unwrap();

    let at = |devfs: bool| {
        FilesBound::of(
            &Rlimit {
                current: Some(nr_open()),
                maximum: Some(nr_open()),
            },
            Pools {
                devfs,
                ..root_pools()
            },
            beside,
        )
    };
    let n = |count: usize| u64::try_from(count).unwrap();
    let dev = n(bench().seeds.get()) + 2 + n(bench().seeds.get());
    let grown = at(true).own - at(false).own;
    assert!(
        grown >= dev,
        "the /dev pool's part ({dev}) is within the mounts made ahead's count ({grown})"
    );
}

/// Goal 5 extended step 6, the bound on namespaces holding files (CLAUDE.md §2): a place under it is taken before a
/// check opens files and given back as it drops. Do: take places under a bound of two until one is refused, drop one,
/// take again. Expect: two taken, the third refused; one dropped frees one; the count back to none at the end.
#[test]
fn a_files_slot_is_taken_up_to_the_bound_and_given_back_as_it_drops() {
    let held = Arc::new(AtomicU64::new(0));
    let first = FilesSlot::take(&held, 2).expect("the first of two");
    let second = FilesSlot::take(&held, 2).expect("the second of two");
    assert!(FilesSlot::take(&held, 2).is_none(), "past the bound");
    assert_eq!(held.load(Ordering::Acquire), 2);
    drop(first);
    let third = FilesSlot::take(&held, 2).expect("the place the first gave back");
    assert!(
        FilesSlot::take(&held, 0).is_none(),
        "a bound of none takes none"
    );
    drop((second, third));
    assert_eq!(held.load(Ordering::Acquire), 0);
}

/// Goal 5 extended step 6, the lead's conditions for the check's own socket (2026-11-03T17:36Z; CLAUDE.md §2): the
/// netlink socket a check keeps in a namespace is held with it inside the idle namespaces' bound, and closed with
/// it when the bound puts it out. Do: give one owner its bound + 1 kept namespaces, each with a stand-in for its
/// socket, the write end of a pipe whose read end the test keeps, and drop what the bound puts out, as the checker
/// does. Expect: one put out, the first given, its pipe at end-of-file; each kept one's still open. A process another
/// test spawns holds a copy of every descriptor from its fork to its exec, so the end-of-file is waited for, the fact
/// itself, as each such copy closes.
#[test]
fn a_kept_namespaces_socket_is_held_in_its_bound_and_closed_when_the_bound_puts_it_out() {
    let mut idle = Idle::new(bench().starts.owner_netns, bench().idle_netns);
    let mut ends = Vec::new();
    let mut out = Vec::new();
    for _ in 0..=bench().starts.owner_netns {
        let (read, write) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .unwrap();
        let ns = rustix::fs::open(
            "/dev/null",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        out.extend(idle.give(
            "o",
            KeptNet {
                ns,
                socket: Some(write),
                files: None,
                slot: None,
            },
        ));
        ends.push(read);
    }
    assert_eq!(out.len(), 1, "one past the owner's bound is put out");
    drop(out);
    let mut byte = [0u8; 1];
    wait_for(
        "the end-of-file of the socket of the first given, put out",
        || rustix::io::read(&ends[0], &mut byte) == Ok(0),
    );
    for end in &ends[1..] {
        assert_eq!(
            rustix::io::read(end, &mut byte),
            Err(rustix::io::Errno::AGAIN),
            "each kept namespace holds its socket open"
        );
    }
}
