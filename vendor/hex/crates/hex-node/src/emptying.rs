//! How long a killed box takes to empty, learned (decision 9 of docs/design/runtime-architecture.md): one wait for
//! every OS's node, since the socket's core waits on its programs' ends by it too ([`crate::serve`]), and Linux's node
//! on its boxes' emptying, their programs' reaping and their inits' ends.

use std::time::{Duration, Instant};

use hex_core::patience::Patience;

/// How long a killed box takes to empty, from its kill (decision 9; Ada's 2026-11-06 rule, the lead's
/// failure-bound rule): twice the slowest emptying recorded by this process's nodes, from every delete, scrub
/// and removal, since all wait on one kernel. A wait that runs out is a fault to report for that kill, yet a
/// slow emptying, not a wedged kernel (run 3's debug seed ran out at 255.505 ms where run 5 answered in up to
/// 904.705 ms), so the next wait backs off (Karn's point, `Patience::ran_out`). Its programs' reaping and its init's end, which a box's
/// emptying waits on, share the wait, as do the socket's waits for the programs it killed. Seeded by build profile
/// from the `node.emptying` rows of `bench/competitive/probes/waits_census.md` (a box's kill to `populated 0`):
/// release, run 4's largest max of the release arms, 19.771 ms (adacachy2, its tmpfs arm, of 17,569); debug, run 5's
/// largest answer of the debug-build arms, the debug arm and the suites' (adacachy2), for it and the tests' waits seeded
/// from it, never under a bound one ran out at: 904.705 ms (the suites', of 14,977, where run 3's seed ran out at
/// 255.505 ms). Another host class (a macOS node's) starts from these and learns its own (decision 9).
pub(crate) static EMPTYING: Patience = Patience::by_profile(
    "node.emptying",
    Duration::from_nanos(19_770_961),
    Some(Duration::from_nanos(904_705_253)),
);

/// What is left at the time of asking of the wait for a box killed at `killed` to empty ([`EMPTYING`]):
/// `Duration::MAX`, unbounded, while it is measuring with no emptying seen.
pub(crate) fn empty_patience(killed: Instant) -> Duration {
    EMPTYING
        .deadline()
        .map_or(Duration::MAX, |wait| wait.saturating_sub(killed.elapsed()))
}
