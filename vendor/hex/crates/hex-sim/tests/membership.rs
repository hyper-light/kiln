//! S8's membership bars in the model (`hex_sim::membership`): a crash is found within SWIM's bounds,
//! and Lifeguard cuts the false positives slow members cause (arXiv:1707.00788v2).

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

use hex_sim::SimError;
use hex_sim::membership::{self, Config, Fault};
use hex_sim::rng::Rng;
use hex_sim::trace::Latencies;

const SECOND: u64 = 1_000_000_000;

fn crash_case(seed: u64) -> (u32, u32, u64) {
    let mut r = Rng::new(seed ^ 0xc7a5);
    let n = u32::try_from(r.range(8, 128)).unwrap();
    let member = u32::try_from(r.range(0, u64::from(n) - 1)).unwrap();
    // Warm-up: every member has probed for 5 s before the crash.
    let at_ns = r.range(5 * SECOND, 30 * SECOND);
    (n, member, at_ns)
}

/// S8 (crash detection, implementation goal 5): do 1,000 seeds of 8 to 128 members with one member
/// crashed after warm-up, with Lifeguard and as the SWIM baseline; expect the first suspicion within
/// 6 probe periods at the p99 (F1's derivation, ln(100) + 1, agreeing with the measured p99) and
/// within SWIM's round-robin bound of 2·(n−1) periods + the timeout at the max, the first death within
/// the first suspicion + the suspicion's max timeout + one period, and (non-vacuity) every crash
/// detected and dead at every live member.
#[test]
fn a_crashed_member_is_suspected_and_declared_dead_within_its_bound() {
    for lifeguard in [true, false] {
        let (mut suspected, mut dead, mut all_dead) = (Vec::new(), Vec::new(), Vec::new());
        for seed in 0..1_000 {
            let (n, member, at_ns) = crash_case(seed);
            let fault = Fault::Crash { member, at_ns };
            let mut config = Config::lan(n, lifeguard);
            config.health_gated_confirmations = lifeguard;
            let (_, max) = config.suspicion_bounds().unwrap();
            let period = config.probe_interval_ns;
            let timeout = config.probe_timeout_ns;
            let round_robin = 2 * (u64::from(n) - 1) * period + timeout;
            config.end_ns = at_ns + round_robin + max + 2 * period;
            config.until_detected = true;
            let report = membership::run(&config, &[fault], seed).unwrap();
            let d = report.crashes[0];
            let first = d
                .first_suspected
                .unwrap_or_else(|| panic!("seed {seed}: never suspected: {d:?}"));
            let first_dead = d
                .first_dead
                .unwrap_or_else(|| panic!("seed {seed}: never declared dead: {d:?}"));
            let everyone = d
                .all_dead
                .unwrap_or_else(|| panic!("seed {seed}: not dead everywhere: {d:?}"));
            assert!(
                first - at_ns <= round_robin,
                "seed {seed}, n {n}: suspected {} ns after the crash",
                first - at_ns
            );
            assert!(
                first_dead - first <= max + period,
                "seed {seed}, n {n}: dead {} ns after the first suspicion",
                first_dead - first
            );
            assert_eq!(report.false_positives, 0, "seed {seed}: {report:?}");
            suspected.push(first - at_ns);
            dead.push(first_dead - at_ns);
            all_dead.push(everyone - at_ns);
        }
        let suspected = Latencies::new(suspected).quantiles().unwrap();
        let dead = Latencies::new(dead).quantiles().unwrap();
        let all_dead = Latencies::new(all_dead).quantiles().unwrap();
        let ms = |ns: u64| ns as f64 / 1e6;
        println!(
            "lifeguard {lifeguard}: suspected p50 {:.1} ms p99 {:.1} ms max {:.1} ms; first dead p50 {:.1} p99 {:.1} max {:.1} ms; dead at all p50 {:.1} p99 {:.1} max {:.1} ms (after the crash)",
            ms(suspected.p50),
            ms(suspected.p99),
            ms(suspected.max),
            ms(dead.p50),
            ms(dead.p99),
            ms(dead.max),
            ms(all_dead.p50),
            ms(all_dead.p99),
            ms(all_dead.max),
        );
        // F1's derivation (docs/design/failure-isolation.md): a member goes unprobed in a period with
        // probability about e^-1 (SWIM §3.1), so in k periods with about e^-k, and its prober suspects
        // it a period after its probe began, when the ping-reqs' wait ends (memberlist's probeNode).
        // The p99 is then ln(100) + 1 periods. Do expect the measured p99 within the 6-period bound,
        // at most the derivation, and within half a period of it (the derivation isn't loose).
        let period_ns = Config::lan(8, lifeguard).probe_interval_ns;
        let period = period_ns as f64;
        let derived = (100f64.ln() + 1.0) * period;
        let p99 = suspected.p99 as f64;
        assert!(suspected.p99 <= 6 * period_ns);
        assert!(
            p99 <= derived && derived - p99 <= period / 2.0,
            "p99 first suspicion {p99} ns against the derived {derived} ns"
        );
    }
}

/// Lifeguard's scenario: `n` members, 4% of them (at least one) slow from 2 s to the end, each
/// processing every message it receives `delay` late, `delay` drawn in 1-30 s: seconds-long, past
/// SWIM's suspicion timeout (6.0-8.4 s at 32-128 members) so the baseline does accuse, and below
/// Lifeguard's maximum (36.1-50.6 s). Past that maximum dynamic suspicion can't outwait the slow
/// member and only the local health multiplier's slower probing helps: drawn in 1-60 s, Lifeguard's
/// false positives were 2.6% of SWIM's (622 of 23,834; 618 of the 622 with a delay of 30 s or more).
fn slow_case(seed: u64, lifeguard: bool) -> (Config, Vec<Fault>, u64) {
    let mut r = Rng::new(seed ^ 0x510e);
    let n = u32::try_from(r.range(32, 128)).unwrap();
    let slow = (n * 4 / 100).max(1);
    let mut config = Config::lan(n, lifeguard);
    config.end_ns = 120 * SECOND;
    let mut members: Vec<u32> = (0..n).collect();
    let mut faults = Vec::new();
    let delay_ns = r.range(SECOND, 30 * SECOND);
    for i in 0..usize::try_from(slow).unwrap() {
        let j = usize::try_from(r.range(u64::try_from(i).unwrap(), u64::from(n) - 1)).unwrap();
        members.swap(i, j);
        faults.push(Fault::Slow {
            member: members[i],
            from_ns: 2 * SECOND,
            until_ns: config.end_ns,
            delay_ns,
        });
    }
    (config, faults, delay_ns)
}

/// The 4% case over `seeds` seeds: false positives (healthy members declared dead) as the SWIM
/// baseline, with Lifeguard as published, and with its confirmations gated on the sender's health.
fn four_percent_slow(seeds: u64) -> [u64; 3] {
    let mut out = [0u64; 3];
    for seed in 0..seeds {
        for (k, (lifeguard, gated)) in [(false, false), (true, false), (true, true)]
            .into_iter()
            .enumerate()
        {
            let (mut config, faults, _) = slow_case(seed, lifeguard);
            config.health_gated_confirmations = gated;
            out[k] += membership::run(&config, &faults, seed)
                .unwrap()
                .false_positives;
        }
    }
    println!(
        "4% slow over {seeds} seeds: SWIM {}, Lifeguard as published {}, health-gated {}",
        out[0], out[1], out[2]
    );
    out
}

/// S8 (Lifeguard cuts slow-member false positives 50-100x, PF[9]), step 3's test: do 100 seeds of 32
/// to 128 members with 4% slow; expect SWIM's false positives above zero (non-vacuity), Lifeguard as
/// published (the competitor) at most 2% of SWIM's, and with confirmations counted only from members
/// in good local health, no healthy member declared dead by anyone. Lifeguard as published declares
/// a few: slow members, each suspecting healthy peers on its own, confirm each other's suspicions,
/// which shrinks the timeout past the refutation (F1).
#[test]
fn lifeguard_cuts_false_positives_and_health_gated_confirmations_end_them() {
    let [swim, published, gated] = four_percent_slow(100);
    assert!(swim > 0, "the SWIM baseline made no false positives");
    assert!(
        published * 50 <= swim,
        "Lifeguard {published} is more than 2% of SWIM's {swim}"
    );
    assert_eq!(gated, 0);
}

/// The same over 1,000 seeds, in release (`cargo test -p hex-sim --release --test membership --
/// --ignored`): the record run of F1's evidence.
#[test]
#[ignore = "1,000 seeds: the record run, in release"]
fn health_gated_confirmations_end_false_positives_over_1000_seeds() {
    let [swim, published, gated] = four_percent_slow(1_000);
    assert!(swim > 0 && published * 50 <= swim);
    assert_eq!(gated, 0);
}

/// S8, step 3's case (Lifeguard's, PF[9]): do 200 seeds of 32 to 128 members with one member slow
/// from 2 s, its delay drawn in 1-30 s as in the 4% case; expect no healthy member declared dead with
/// Lifeguard, and some with it off (non-vacuity). Every declaration against a healthy member is the
/// slow member's own: its timers fire on time but it hears the refutation a delay late. Lifeguard's
/// suspicion starts at its maximum (36.1-50.6 s here), past any such delay, and only independent
/// confirmations shrink it; with one slow member there are none. SWIM's fixed timeout, its minimum
/// (6.0-8.4 s), doesn't outwait it. (With several slow members, they confirm each other's false
/// suspicions: the 4% case above.)
#[test]
fn a_slow_member_makes_lifeguard_declare_no_healthy_member_dead() {
    let (mut swim, mut lifeguard) = (0u64, 0u64);
    for seed in 0..200 {
        let mut r = Rng::new(seed ^ 0x51_0e01);
        let n = u32::try_from(r.range(32, 128)).unwrap();
        let slow = u32::try_from(r.range(0, u64::from(n) - 1)).unwrap();
        let delay_ns = r.range(SECOND, 30 * SECOND);
        for on in [false, true] {
            let mut config = Config::lan(n, on);
            config.end_ns = 120 * SECOND;
            let fault = Fault::Slow {
                member: slow,
                from_ns: 2 * SECOND,
                until_ns: config.end_ns,
                delay_ns,
            };
            let report = membership::run(&config, &[fault], seed).unwrap();
            if on {
                lifeguard += report.false_positives;
            } else {
                swim += report.false_positives;
            }
        }
    }
    println!("one slow member, delay 1-30 s: SWIM {swim}, Lifeguard {lifeguard}");
    assert!(swim > 0, "the SWIM baseline made no false positives");
    assert_eq!(lifeguard, 0);
}

/// H1 (a run is a function of its seed), for S8's model: do one crash-and-slow run twice from one
/// seed and once from another; expect equal reports, and a different trace hash from the other seed.
#[test]
fn a_run_is_a_function_of_its_seed() {
    let (mut config, mut faults, _) = slow_case(7, true);
    config.end_ns = 40 * SECOND;
    faults.push(Fault::Crash {
        member: 0,
        at_ns: 10 * SECOND,
    });
    let a = membership::run(&config, &faults, 7).unwrap();
    let b = membership::run(&config, &faults, 7).unwrap();
    let c = membership::run(&config, &faults, 8).unwrap();
    assert_eq!(a, b);
    assert_ne!(a.trace_hash, c.trace_hash);
    assert!(a.messages > 0 && a.crashes[0].first_dead.is_some());
}

/// H5 (bounded everywhere), for S8's model: do a run past each bound; expect its typed refusal.
#[test]
fn bounds_refuse_typed() {
    let crash = [Fault::Crash {
        member: 1,
        at_ns: 5 * SECOND,
    }];
    let mut config = Config::lan(16, true);
    config.end_ns = 30 * SECOND;
    config.max_events = 8;
    assert_eq!(
        membership::run(&config, &crash, 1),
        Err(SimError::Full {
            what: "the event queue",
            bound: 8
        })
    );
    let mut config = Config::lan(16, true);
    config.end_ns = 30 * SECOND;
    config.max_steps = 100;
    assert_eq!(
        membership::run(&config, &crash, 1),
        Err(SimError::StepBudget(100))
    );
    let mut config = Config::lan(16, true);
    config.end_ns = 30 * SECOND;
    config.max_relayed = 0;
    assert_eq!(
        membership::run(&config, &crash, 1),
        Err(SimError::Full {
            what: "a member's relayed probes",
            bound: 0
        })
    );
    for members in [1, membership::MAX_MEMBERS + 1] {
        assert!(matches!(
            membership::run(&Config::lan(members, true), &[], 1),
            Err(SimError::Config(_))
        ));
    }
    assert!(matches!(
        membership::run(
            &Config::lan(4, true),
            &[Fault::Crash {
                member: 4,
                at_ns: 0
            }],
            1
        ),
        Err(SimError::Config(_))
    ));
}
