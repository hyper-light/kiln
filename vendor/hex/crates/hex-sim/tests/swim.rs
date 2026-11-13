//! S8's crash class on hyper-swim's own failure detector (`hex_sim::swim`, F1): a crashed member is
//! condemned within the bound its condemner states, and no live member is condemned.

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

use hex_sim::rng::Rng;
use hex_sim::swim::{self, Config};
use hex_sim::trace::Latencies;

const SECOND: u64 = 1_000_000_000;

/// The two budgets the crash class is measured at: zero, each period as short as its probe allows;
/// and memberlist's pace, one probe a member a second (`(2(2m − 1) + 1)` seconds for `m` peers).
#[derive(Debug, Clone, Copy)]
enum Pace {
    AtNeed,
    OneASecond,
}

fn crash_case(seed: u64, pace: Pace) -> Config {
    let mut r = Rng::new(seed ^ 0x5717);
    let n = u32::try_from(r.range(8, 128)).unwrap();
    let m = u64::from(n - 1);
    let budget = match pace {
        Pace::AtNeed => Duration::ZERO,
        Pace::OneASecond => Duration::from_secs(2 * (2 * m - 1) + 1),
    };
    let mut config = Config::lan(n, budget);
    config.victim = u32::try_from(r.range(0, m)).unwrap();
    // Warm-up: every member has run several rounds before the crash.
    config.crash_ns = match pace {
        Pace::AtNeed => r.range(2 * SECOND, 4 * SECOND),
        Pace::OneASecond => r.range(4 * m * SECOND, 5 * m * SECOND),
    };
    config
}

/// Do `seeds` crashes at `pace`; expect each victim condemned, within the bound its condemner stated
/// (from its last answer), and no live member condemned. Prints the distributions.
fn crashes(seeds: u64, pace: Pace) {
    let (mut suspected, mut condemned, mut bounds) = (Vec::new(), Vec::new(), Vec::new());
    let mut pings_per_member_s = Vec::new();
    for seed in 0..seeds {
        let config = crash_case(seed, pace);
        let report = swim::run(&config, seed).unwrap();
        assert_eq!(report.false_condemnations, 0, "seed {seed}: {report:?}");
        let s = report
            .first_suspected_ns
            .unwrap_or_else(|| panic!("seed {seed}: never suspected: {report:?}"));
        let c = report
            .first_condemned
            .unwrap_or_else(|| panic!("seed {seed}: never condemned: {report:?}"));
        let within = c
            .within
            .unwrap_or_else(|| panic!("seed {seed}: no bound stated: {c:?}"));
        assert!(
            c.after <= within,
            "seed {seed}, n {}: condemned {:?} after the last answer, past its bound {within:?}",
            config.members,
            c.after
        );
        suspected.push(s - config.crash_ns);
        condemned.push(c.at_ns - config.crash_ns);
        bounds.push(u64::try_from(within.as_nanos()).unwrap());
        pings_per_member_s
            .push(report.pings * 1000 / (u64::from(config.members) * c.at_ns / 1_000_000).max(1));
    }
    let q = |v: Vec<u64>| Latencies::new(v).quantiles().unwrap();
    let ms = |ns: u64| ns as f64 / 1e6;
    let (s, c, b, p) = (q(suspected), q(condemned), q(bounds), q(pings_per_member_s));
    println!(
        "{pace:?} over {seeds} seeds: suspected p50 {:.1} p99 {:.1} max {:.1} ms; condemned p50 {:.1} p99 {:.1} max {:.1} ms; stated bound p50 {:.1} max {:.1} ms; pings per member-second p50 {} max {}",
        ms(s.p50),
        ms(s.p99),
        ms(s.max),
        ms(c.p50),
        ms(c.p99),
        ms(c.max),
        ms(b.p50),
        ms(b.max),
        p.p50,
        p.max,
    );
}

/// S8 crash class on hyper-swim (F1): do 10 crashes at each pace; expect every victim suspected and
/// condemned, each condemnation within the bound its condemner stated, and no live member condemned.
#[test]
fn a_crashed_member_is_condemned_within_its_stated_bound() {
    crashes(10, Pace::AtNeed);
    crashes(10, Pace::OneASecond);
}

/// The same over 1,000 seeds at each pace, in release (`cargo test -p hex-sim --release --test swim
/// -- --ignored`): the record run of F1's evidence.
#[test]
#[ignore = "1,000 seeds at each pace: the record run, in release"]
fn a_crashed_member_is_condemned_within_its_stated_bound_over_1000_seeds() {
    crashes(1_000, Pace::AtNeed);
    crashes(1_000, Pace::OneASecond);
}
