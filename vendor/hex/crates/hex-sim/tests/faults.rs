//! Scheduler implementation goal 5 (`docs/design/failure-isolation.md`, S8): every fault class the
//! nemesis injects is detected and isolated within the bound the design states for it, on every one of
//! 1,000 seeds, with no verdict against what wasn't at fault; and each safeguard's mutant breaks its
//! bound (non-vacuity). The crash class is `tests/membership.rs`'s.

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

use hex_health::agent::REPEAT_LIMIT;
use hex_health::grade::WINDOW_NS;
use hex_sim::danger::{Isolation, Refusal};
use hex_sim::faults::{self, MS, Mutant, S};
use hex_sim::model::Class;

const SEEDS: u64 = 1_000;

fn p(xs: &mut [u64], q: usize) -> u64 {
    xs.sort_unstable();
    xs[(xs.len() - 1) * q / 100]
}

/// S8 gray/fail-slow (F2): a node 2.5-8x slower than its peers is suspected within 2 windows and
/// drained within 4, both under Panorama's 7 s, and no healthy node is suspected.
#[test]
fn a_slow_node_is_suspected_in_2s_and_drained_in_4s() {
    let (mut det, mut iso) = (Vec::new(), Vec::new());
    for seed in 0..SEEDS {
        let (m, factor) = faults::gray(seed).unwrap();
        let d = m
            .detected_after_ns
            .unwrap_or_else(|| panic!("seed {seed} (x{factor}) undetected"));
        let i = m
            .isolated_after_ns
            .unwrap_or_else(|| panic!("seed {seed} (x{factor}) not drained"));
        assert!(d <= 2 * WINDOW_NS, "seed {seed}: suspected after {d} ns");
        assert!(i <= 4 * WINDOW_NS, "seed {seed}: drained after {i} ns");
        assert!(i < 7 * S);
        assert_eq!(m.wrong_verdicts, 0, "seed {seed}");
        det.push(d);
        iso.push(i);
    }
    println!(
        "gray: suspect p50 {} p99 {} max {} ms; drain p50 {} p99 {} max {} ms",
        p(&mut det, 50) / MS,
        p(&mut det, 99) / MS,
        p(&mut det, 100) / MS,
        p(&mut iso, 50) / MS,
        p(&mut iso, 99) / MS,
        p(&mut iso, 100) / MS,
    );
}

/// AC-T7.10.3 in the model (F3), Ada's cost rule: do stall a disk whose probe holds a record of its
/// healthy answers and a measured re-placement time (an OCI start or a Shards boot); expect it marked
/// within the earlier of the slowest answer and the drain age, plus a period, never before, and drained
/// within its drain age and a period, at either price.
#[test]
fn a_stalled_disk_is_suspected_then_drained_within_its_bound() {
    let mut by_price = [0u64; 2];
    for seed in 0..SEEDS {
        let (m, j) = faults::disk_stall(seed).unwrap();
        let (suspect, drain) = faults::disk_stall_bounds(&j);
        let d = m.detected_after_ns.unwrap();
        let i = m.isolated_after_ns.unwrap();
        assert!(d <= suspect, "seed {seed}: {d} > {suspect}");
        assert!(i <= drain, "seed {seed}: {i} > {drain}");
        assert!(
            d + j.period_ns > j.slowest_ns.min(j.drain_age_ns),
            "seed {seed}: marked before either age"
        );
        by_price[usize::from(j.replace_ns == faults::REPLACE_NS[1])] += 1;
    }
    assert!(by_price.iter().all(|n| *n > 0), "{by_price:?}");
}

/// F2 owns fail-slow (the lead, 2026-11-06): do slow one node's disk gradually, 0.5-5% a window
/// from 1x to 4x; expect the peer grade to drain it, against its healthy peers, by the time its work
/// runs 3x theirs, with no verdict on a peer. (Its own probe drains it too: the cost rule takes an
/// answer only within its drain age, and a slowing disk outruns it; F2 doesn't lean on that.)
#[test]
fn a_gradually_slowing_disk_is_caught_by_its_peers() {
    let mut probe_drained = 0u64;
    for seed in 0..SEEDS {
        let f = faults::fail_slow_disk(seed).unwrap();
        assert_eq!(f.measured.wrong_verdicts, 0, "seed {seed}: {f:?}");
        let at = f
            .slowdown_at_drain_milli
            .unwrap_or_else(|| panic!("seed {seed}: never drained: {f:?}"));
        assert!(at <= 3_000, "seed {seed}: drained at {at}");
        probe_drained += u64::from(f.probe_drained);
    }
    println!("fail-slow disk: its own probe drained {probe_drained} of {SEEDS} too");
}

/// S8 antagonist (F4): an antagonist is capped within 3 CPI samples (CPI²'s one a minute) and a second
/// of its onset, and no bystander is.
#[test]
fn an_antagonist_is_capped_within_three_samples() {
    let mut caught = Vec::new();
    for seed in 0..SEEDS {
        let (m, factor) = faults::antagonist(seed).unwrap();
        let i = m
            .isolated_after_ns
            .unwrap_or_else(|| panic!("seed {seed} (x{factor}): not capped"));
        assert!(
            i <= 3 * 60 * S + S,
            "seed {seed} (x{factor}): capped after {} s",
            i / S
        );
        assert_eq!(m.wrong_verdicts, 0, "seed {seed}");
        caught.push(i / S);
    }
    println!(
        "antagonist: capped p50 {} s, max {} s",
        p(&mut caught, 50),
        p(&mut caught, 100)
    );
}

/// A4 stuck loop (F5): a looping session is stopped by its third identical call, and no healthy
/// session is.
#[test]
fn a_stuck_loop_is_stopped_by_its_third_identical_call() {
    for seed in 0..SEEDS {
        let (m, calls) = faults::stuck_loop(seed).unwrap();
        assert!(m.isolated_after_ns.is_some(), "seed {seed}: not stopped");
        assert!(
            (1..=REPEAT_LIMIT).contains(&calls),
            "seed {seed}: {calls} loop calls"
        );
        assert_eq!(m.wrong_verdicts, 0, "seed {seed}");
    }
}

/// A4 quota shown as slowness (F5): the account is held at its first typed error and no node is
/// suspected or charged.
#[test]
fn quota_holds_the_account_at_once_and_charges_no_node() {
    for seed in 0..SEEDS {
        let m = faults::quota(seed, Mutant::None).unwrap();
        assert!(m.detected_after_ns.unwrap() < 100 * MS, "seed {seed}");
        assert_eq!(m.wrong_verdicts, 0, "seed {seed}");
    }
}

/// A4 partial provider outage (F5): the provider is marked degraded within 2 windows and no node is
/// suspected or charged.
#[test]
fn a_provider_outage_is_marked_in_2s_and_charges_no_node() {
    for seed in 0..SEEDS {
        let m = faults::provider_outage(seed, Mutant::None).unwrap();
        let d = m
            .detected_after_ns
            .unwrap_or_else(|| panic!("seed {seed}: not marked"));
        assert!(d <= 2 * WINDOW_NS, "seed {seed}: marked after {d} ns");
        assert_eq!(m.wrong_verdicts, 0, "seed {seed}");
    }
}

/// Non-vacuity of F5's separation: fed to the node grade as slowness, provider errors do suspect
/// nodes.
#[test]
fn the_mutant_that_reads_provider_errors_as_slowness_suspects_nodes() {
    let wrong: u64 = (0..100)
        .map(|seed| {
            faults::quota(seed, Mutant::ProviderErrorsAsNodeSlowness)
                .unwrap()
                .wrong_verdicts
                + faults::provider_outage(seed, Mutant::ProviderErrorsAsNodeSlowness)
                    .unwrap()
                    .wrong_verdicts
        })
        .sum();
    assert!(wrong > 0);
}

/// S8 dangerous (F7), per kind Hex places: do 1,000 trips of each kind in a seeded cell; expect each
/// isolated (stopped) at its signal and re-placed or refused by the rule, against the oracle (no
/// wrong verdict); an OCI container re-placed as Firecracker and as Shards, on its own node and on
/// another, and refused where none holds it (non-vacuity; every node model of G2's runs Shards, so
/// no cell is without a microVM); every trip in a Firecracker or Shards microVM refused.
#[test]
fn a_dangerous_workload_is_re_placed_in_a_stronger_kind_or_refused_at_its_signal() {
    for kind in [Class::Oci, Class::Firecracker, Class::Shards] {
        let (mut to_fc, mut to_shards, mut elsewhere, mut none_in_cell, mut no_fit, mut in_vm) =
            (0, 0, 0, 0, 0, 0);
        let mut rescued = 0u64;
        let mut scanned = Vec::new();
        for seed in 0..SEEDS {
            let d = faults::dangerous(seed, kind).unwrap();
            assert_eq!(d.measured.wrong_verdicts, 0, "{kind:?} seed {seed}: {d:?}");
            assert_eq!(d.measured.isolated_after_ns, Some(0));
            elsewhere += u64::from(d.elsewhere);
            rescued += u64::from(d.rescued);
            match d.isolation {
                Isolation::Move { to, scanned: s, .. } => {
                    scanned.push(s as u64);
                    if to == Class::Firecracker {
                        to_fc += 1;
                    } else {
                        to_shards += 1;
                    }
                }
                Isolation::Refuse {
                    why: Refusal::NoMicroVmInCell,
                    ..
                } => none_in_cell += 1,
                Isolation::Refuse {
                    why: Refusal::NoneHolds,
                    ..
                } => no_fit += 1,
                Isolation::Refuse {
                    why: Refusal::InMicroVm,
                    ..
                } => in_vm += 1,
                Isolation::None => panic!("{kind:?} seed {seed}"),
            }
        }
        let span = if scanned.is_empty() {
            (0, 0)
        } else {
            (p(&mut scanned, 50), p(&mut scanned, 100))
        };
        println!(
            "dangerous {kind:?}: re-placed as Firecracker {to_fc}, as Shards {to_shards} ({elsewhere} on another node, {rescued} the on-node rule would have refused; nodes scanned p50 {} max {}); refused: no microVM in the cell {none_in_cell}, none holds it {no_fit}, in a microVM {in_vm}",
            span.0, span.1
        );
        if kind == Class::Oci {
            // Every one of G2's node models runs Shards, so no cell lacks a microVM; that refusal is
            // `danger`'s unit test's, on an OCI-only cell.
            assert!(
                to_fc > 0 && to_shards > 0 && no_fit > 0,
                "{to_fc} {to_shards} {no_fit}"
            );
            assert_eq!(none_in_cell, 0);
            assert!(rescued > 0 && rescued <= elsewhere);
            assert!(elsewhere > 0 && elsewhere < to_fc + to_shards);
        } else {
            assert_eq!(in_vm, SEEDS);
        }
    }
}

/// S8 ejection and mass loss (F6, the ejection criterion restated 2026-11-06 under Ada's rule that no
/// constant is arbitrary): do turn a third to two fifths of a loaded cell of G2's node models slow at
/// once; expect no ejection to leave work the cell can't re-place (every ejected node's workloads
/// placed elsewhere, `admitted_anywhere` checked at the moment of ejection), drains refused where the
/// rest of the cell is full (non-vacuity), some ejected; and a partition to move work no faster than
/// the eviction rate.
#[test]
fn a_correlated_fault_strands_no_work_and_never_passes_the_rate() {
    let (mut ejected, mut refused, mut re_placed) = (0, 0, 0);
    for seed in 0..SEEDS {
        let c = faults::correlated(seed, Mutant::None).unwrap();
        assert_eq!(c.stranded, 0, "seed {seed}: {c:?}");
        assert!(
            c.max_moved_in_window <= c.allowed_in_window,
            "seed {seed}: {c:?}"
        );
        assert!(c.moved >= 1, "seed {seed}");
        ejected += c.ejected;
        refused += c.refused;
        re_placed += c.re_placed;
    }
    assert!(
        ejected > 0 && refused > 0 && re_placed > 0,
        "{ejected} {refused} {re_placed}"
    );
}

/// Non-vacuity of F2: with the gray detector off, the same slow nodes are never suspected or drained,
/// so the bound fails on every seed.
#[test]
fn without_the_detector_a_slow_node_is_never_caught() {
    for seed in 0..100 {
        let (m, _) = faults::gray_with(seed, Mutant::NoDetector).unwrap();
        assert_eq!(m.detected_after_ns, None, "seed {seed}");
        assert_eq!(m.isolated_after_ns, None, "seed {seed}");
    }
}

/// Non-vacuity of F6: with the re-placement check or the rate switched off, the same faults strand
/// work or pass the rate.
#[test]
fn without_the_check_or_the_rate_a_correlated_fault_breaks_them() {
    let stranded = (0..100).any(|seed| {
        let c = faults::correlated(seed, Mutant::NoReplacementCheck).unwrap();
        c.stranded > 0
    });
    let over_rate = (0..100).any(|seed| {
        let c = faults::correlated(seed, Mutant::NoEvictionRate).unwrap();
        c.max_moved_in_window > c.allowed_in_window
    });
    assert!(stranded && over_rate, "{stranded} {over_rate}");
}

/// H1: a fault run is a function of its seed.
#[test]
fn a_fault_run_is_a_function_of_its_seed() {
    assert_eq!(faults::gray(7).unwrap(), faults::gray(7).unwrap());
    assert_eq!(
        faults::correlated(7, Mutant::None).unwrap(),
        faults::correlated(7, Mutant::None).unwrap()
    );
}
