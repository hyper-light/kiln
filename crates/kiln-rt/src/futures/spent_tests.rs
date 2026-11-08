//! What a piece of work spent ([`super::spent`], [`super::Spent::cost_ns`]; kiln's goal 9, plan 01a1a87b2cec step 5).

use super::{Spent, spent};

/// Do: the cost of work by what's known of it. Expect: its time on the CPU where it's known it didn't wait inside a call
/// (a host's preemption swelled its wall time); its wall time where it waited, or where that or its CPU time isn't known;
/// never more than its wall time (a coarse CPU clock).
#[test]
fn g9_s5_works_cost_is_its_cpu_time_unless_it_waited() {
    let spent = |wall_ns, cpu_ns, blocked| Spent { wall_ns, cpu_ns, blocked };
    assert_eq!(spent(300_000, Some(40_000), Some(false)).cost_ns(), 40_000, "preempted: its CPU time");
    assert_eq!(spent(300_000, Some(40_000), Some(true)).cost_ns(), 300_000, "it waited: its wall time");
    assert_eq!(spent(300_000, Some(40_000), None).cost_ns(), 300_000, "not known whether it waited: its wall time");
    assert_eq!(spent(300_000, None, Some(false)).cost_ns(), 300_000, "no CPU clock: its wall time");
    assert_eq!(spent(40_000, Some(41_000), Some(false)).cost_ns(), 40_000, "never past its wall time");
    // Computation on memory, which makes no call that waits: its CPU time unless it's known it waited.
    assert_eq!(spent(300_000, Some(40_000), None).compute_cost_ns(), 40_000, "not known whether it waited: its CPU time");
    assert_eq!(spent(300_000, Some(40_000), Some(true)).compute_cost_ns(), 300_000, "it waited (a page from swap): its wall time");
    assert_eq!(spent(300_000, None, None).compute_cost_ns(), 300_000, "no CPU clock: its wall time");
}

/// Waits `ns` on this thread, as a call that waits does: a channel nobody sends on.
fn wait(ns: u64) {
    let (_hold, rx) = std::sync::mpsc::channel::<()>();
    let _ = rx.recv_timeout(std::time::Duration::from_nanos(ns));
}

/// Do: measure work that spins on the CPU, and work that waits inside a call. Expect: both timed by the wall clock; where
/// the thread's CPU clock tells it, the spin's CPU time near its wall time and the wait's far under; where the OS counts
/// a thread's switches (Linux), the wait said to have waited, its cost its wall time.
#[test]
fn g9_s5_spent_tells_a_spin_from_a_wait() {
    /// Shape: long against the clocks' reads (a microsecond or less) and a scheduler's tick, short for a test.
    const SPAN_NS: u64 = 5_000_000;
    let (_, spun) = spent(|| {
        let began = crate::machine::clock::shard_clock_ns();
        while crate::machine::clock::shard_clock_ns().saturating_sub(began) < SPAN_NS {
            std::hint::spin_loop();
        }
    });
    let (_, waited) = spent(|| wait(SPAN_NS));
    assert!(spun.wall_ns >= SPAN_NS && waited.wall_ns >= SPAN_NS, "{spun:?} {waited:?}");
    if let (Some(spin_cpu), Some(wait_cpu)) = (spun.cpu_ns, waited.cpu_ns) {
        assert!(spin_cpu > wait_cpu.saturating_mul(4), "the spin ran, the wait didn't: {spun:?} {waited:?}");
    }
    if cfg!(target_os = "linux") {
        assert_eq!(waited.blocked, Some(true), "{waited:?}");
        assert_eq!(waited.cost_ns(), waited.wall_ns);
    }
}
