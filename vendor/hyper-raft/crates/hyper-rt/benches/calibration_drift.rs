//! What a calibration costs and how far its measurement moves with time on one machine
//! (docs/runtime.md §15 item 10): `cargo bench -p hyper-rt --bench calibration_drift`.
//!
//! A short command reuses a stored calibration record while its key matches and its age is under the
//! consumer's bound (§10.2); the bound weighs what a fresh record costs the command that measures it
//! against what a stale one costs the commands that reuse it. This measures both inputs: each round runs
//! `Calibration::measure` at the probes' budget the runtime's own callers give (40 ms), and prints its
//! wall time, the process CPU it took, the one-minute load average before it, and what it measured (the
//! wake's mean with its 95 % interval, p50 and p99, and the null system call's median). Rounds are
//! `HYPER_RT_DRIFT_ROUNDS` apart by `HYPER_RT_DRIFT_GAP_S` seconds (defaults 30 and 60), so the run spans
//! the host's load as it moves.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use std::time::{Duration, Instant};

use hyper_rt::machine::calibration::Calibration;

/// The probes' wall budget the runtime's own callers measure with (`benches/echo.rs`, hyper-rt-compare).
const BUDGET: Duration = Duration::from_millis(40);

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let rounds = env("HYPER_RT_DRIFT_ROUNDS", 30);
    let gap = Duration::from_secs(env("HYPER_RT_DRIFT_GAP_S", 60));
    let began = Instant::now();
    println!(
        "round,at_s,load,wall_ms,cpu_ms,wake_mean_ns,wake_lower_ns,wake_upper_ns,wake_p50_ns,wake_p99_ns,syscall_ns"
    );
    for round in 0..rounds {
        let load = hyper_measure::usage::load().unwrap_or(f64::NAN);
        let before = hyper_measure::usage::this().unwrap();
        let start = Instant::now();
        let calibration = Calibration::measure(BUDGET, 0).unwrap();
        let wall = start.elapsed();
        let cpu = hyper_measure::usage::this()
            .unwrap()
            .since(&before)
            .cpu_ns();
        let wake = &calibration.wake;
        println!(
            "{round},{:.1},{load:.2},{:.1},{:.1},{},{},{},{},{},{}",
            began.elapsed().as_secs_f64(),
            wall.as_secs_f64() * 1e3,
            cpu as f64 / 1e6,
            wake.mean_ns,
            wake.mean_lower_ns,
            wake.mean_upper_ns,
            wake.p50_ns,
            wake.p99_ns,
            calibration.syscall.median_ns(),
        );
        if round + 1 < rounds {
            std::thread::sleep(gap.saturating_sub(start.elapsed()));
        }
    }
}
