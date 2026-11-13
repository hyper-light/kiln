//! Joint plan step 14 (AC-T7.4.1, AC-T7.5.2; `docs/design/learned-charge.md`, "Measured in hex-sim"): the learned
//! charge and its contention rule on one simulated node, window by window, over a bounded set of seeds. What each
//! run measured is printed for the record; ROOT measures the same on hardware.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::time::Duration;

use hex_api::CapacityResources;
use hex_sim::charge::ChargeError;
use hex_sim::charge::sim::{Report, Scenario, run};
use hex_sim::charge::trace::{self, Memory, Step, Trace};

const GIB: u64 = 1 << 30;
/// Linux's `cpu.max` period, the window a Linux node enforces a VM's CPU over (`hex_core::cgroup::CPU_PERIOD_US`).
const WINDOW: Duration = Duration::from_millis(100);
/// A bounded set of seeds, not a sweep.
const SEEDS: [u64; 4] = [1, 2, 3, 4];
/// Every run's length: 20 minutes of windows.
const WINDOWS: u64 = 12_000;

/// A Linux node of 16 cores and 64 GiB whose room is all Hex's, every VM a default Shards VM: every host CPU
/// (D191) and 8 GiB. 48 VMs in line, each living 5 to 20 minutes, over 20 minutes.
fn node(seed: u64) -> Scenario {
    Scenario {
        seed,
        window: WINDOW,
        capacity: CapacityResources {
            cpu_milli: 16_000,
            mem_bytes: 64 * GIB,
            disk_bytes: 0,
        },
        envelope: CapacityResources {
            cpu_milli: 16_000,
            mem_bytes: 8 * GIB,
            disk_bytes: 0,
        },
        vcpus: 16,
        vms: 48,
        windows: WINDOWS,
        life: (3_000, 12_000),
        rooms: true,
        riser: None,
        traces: Vec::new(),
    }
}

/// The riser's node: the same, but each VM may grow to 48 GiB, so a VM that grows to its whole envelope passes the
/// memory its neighbours leave it (about ten agents at their peaks beside it on 64 GiB).
fn riser(seed: u64, rooms: bool) -> Scenario {
    let base = node(seed);
    Scenario {
        envelope: CapacityResources {
            mem_bytes: 48 * GIB,
            ..base.envelope
        },
        rooms,
        riser: Some((6_000, 0)),
        ..base
    }
}

fn said(name: &str, seed: u64, r: &Report) {
    println!(
        "{name} seed {seed}: admitted {}, refused {}, peak {} VMs at once (envelope charge: {}), mean {:.2}; \
         CPU given {:.4} of asked; contended windows {}, Jain's index least {:.4} mean {:.4}; guarantee misses {}; \
         memory held back {} VM-windows; ended {} ({} within budget); riser over {} windows, refused while rising {}, \
         neighbours short {}, riser ended {}",
        r.admitted,
        r.refused,
        r.peak_live,
        r.envelope_density,
        r.live_windows as f64 / WINDOWS as f64,
        r.given_milli as f64 / r.asked_milli.max(1) as f64,
        r.contended,
        r.jain_least_ppm as f64 / 1e6,
        r.jain_mean_ppm() as f64 / 1e6,
        r.guarantee_misses,
        r.held_back,
        r.ended,
        r.ended_within,
        r.riser_over,
        r.refused_while_rising,
        r.neighbours_short,
        r.riser_ended,
    );
}

/// Joint plan step 14 (AC-T7.4.1): density and fairness under the rule. Do run agents through a node whose VMs each
/// have every host CPU; expect today's envelope charge to hold one at once and the rule more on every seed, starts
/// refused while the node is full (its back-off), no VM ever given less CPU than `min(its ask, its booking)`, and no
/// VM ended within its budget.
#[test]
fn w14_density_and_fairness_under_the_rule() {
    for seed in SEEDS {
        let r = run(&node(seed)).unwrap();
        said("density", seed, &r);
        assert_eq!(r.envelope_density, 1);
        assert!(r.peak_live > 1, "seed {seed}: {r:?}");
        assert!(r.refused > 0, "seed {seed}: the node never filled: {r:?}");
        assert_eq!(r.guarantee_misses, 0, "seed {seed}: {r:?}");
        assert_eq!(r.ended_within, 0, "seed {seed}: {r:?}");
    }
}

/// Joint plan step 14 (AC-T7.4.1): a VM whose use rises past its charge, its memory held at its budget as Linux's
/// `memory.high` holds it. Do let the first VM to start, of 48 GiB, ask for its whole envelope from minute 10; expect its charge
/// past its booking, the node refusing starts while it rises (backing off), it held at its budget, its neighbours
/// never given less CPU than `min(ask, booking)`, contention to have happened, and nothing ended.
#[test]
fn w14_a_riser_is_held_without_starving_its_neighbours() {
    for seed in SEEDS {
        let r = run(&riser(seed, true)).unwrap();
        said("riser, rooms held", seed, &r);
        assert!(r.riser_over > 0, "seed {seed}: {r:?}");
        assert!(r.refused_while_rising > 0, "seed {seed}: {r:?}");
        assert!(r.held_back > 0, "seed {seed}: {r:?}");
        assert!(r.contended > 0, "seed {seed}: {r:?}");
        assert_eq!(r.neighbours_short, 0, "seed {seed}: {r:?}");
        assert_eq!(r.guarantee_misses, 0, "seed {seed}: {r:?}");
        assert_eq!(r.ended, 0, "seed {seed}: {r:?}");
    }
}

/// Joint plan step 14 (AC-T7.4.1): the same riser where nothing holds a VM's memory at its budget yet (a guest's room
/// awaits Shards, `Awaits(GuestRoom)`). Expect the riser ended by the last resort, nothing within its budget ended,
/// and its neighbours never short of `min(ask, booking)`.
#[test]
fn w14_a_riser_with_no_room_held_is_ended_last_and_alone() {
    for seed in SEEDS {
        let r = run(&riser(seed, false)).unwrap();
        said("riser, no room held", seed, &r);
        assert!(r.riser_ended, "seed {seed}: {r:?}");
        assert_eq!(r.ended_within, 0, "seed {seed}: {r:?}");
        assert_eq!(r.neighbours_short, 0, "seed {seed}: {r:?}");
        assert_eq!(r.guarantee_misses, 0, "seed {seed}: {r:?}");
    }
}

/// Joint plan step 14 (item 3; `bench/charge/record_trace.py`): a recorded trace's text is its `millis threads mib`
/// steps, `#` comments and blank lines aside, its header naming where its memory came from. Do read a recorder's
/// header naming its processes' resident sets, a blank line and three steps, one with a comment after it; then a
/// trace whose header names its cgroup, one that names none (the recorder's only source before it named one, its
/// cgroup's), a line of two numbers, and a trace with no step. Expect the three steps and `ProcsRss`; `Cgroup` twice;
/// the short line and the empty trace refused, typed.
#[test]
fn w14_a_recorded_trace_reads_as_its_steps_and_its_memorys_source() {
    let header = "# window: 100000 us, the CFS period of the nearest cpu.max on its path\n";
    let text = format!(
        "{header}# memory: procs-rss, the sum over its cgroup.procs\n# lines: millis threads mib\n\n30 3 512\n\
         70 2 512 # the rest\n100 0 640\n"
    );
    let step = |millis, threads, mib| Step {
        millis,
        threads,
        mib,
    };
    assert_eq!(
        trace::parse(&text).unwrap(),
        Trace {
            memory: Memory::ProcsRss,
            steps: vec![step(30, 3, 512), step(70, 2, 512), step(100, 0, 640)],
        }
    );
    let cgroup = format!("{header}# memory: cgroup, its memory.current\n100 1 64\n");
    assert_eq!(trace::parse(&cgroup).unwrap().memory, Memory::Cgroup);
    assert_eq!(
        trace::parse(&format!("{header}100 1 64\n")).unwrap().memory,
        Memory::Cgroup
    );
    assert!(matches!(
        trace::parse("30 3\n"),
        Err(ChargeError::Invalid { .. })
    ));
    assert!(matches!(
        trace::parse("# memory: cgroup\n"),
        Err(ChargeError::Invalid { .. })
    ));
}

/// Joint plan step 14 (item 3): the sim replays a recorded trace in place of the generator's draws, as hex-cli's
/// charge_fleet replays it on hardware: each VM plays one trace, window by window, and ends once it has played. Do run
/// one VM over a trace of a window at 3 busy threads holding 512 MiB, half a window at 1, then a window idle holding
/// 640 MiB, on a node with room for it. Expect it admitted once and live three windows, asking 3 CPUs, then 0.5 (half a
/// window at one thread), then none: 3,500 milli-CPU-windows asked, and given.
#[test]
fn w14_the_sim_replays_a_recorded_trace() {
    let played = trace::parse("100 3 512\n50 1 512\n100 0 640\n").unwrap();
    let scenario = Scenario {
        vms: 1,
        windows: 5,
        life: (10, 10),
        traces: vec![played],
        ..node(1)
    };
    let r = run(&scenario).unwrap();
    assert_eq!(r.admitted, 1, "{r:?}");
    assert_eq!(r.live_windows, 3, "{r:?}");
    assert_eq!(r.asked_milli, 3_500, "{r:?}");
    assert_eq!(r.given_milli, 3_500, "{r:?}");
}
