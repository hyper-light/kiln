//! How long a killed box takes to empty, learned (decision 9 of docs/design/runtime-architecture.md): one wait for
//! every OS's node, since the socket's core waits on its programs' ends by it too ([`crate::serve`]), and Linux's node
//! on its boxes' emptying, their programs' reaping and their inits' ends.

use std::time::{Duration, Instant};

use hex_core::patience::Patience;

/// How long a killed box takes to empty, from its kill (decision 9; Ada's rules of 2026-11-06 and 2026-11-13): W*, the
/// slowest emptying this process's nodes have seen, from every delete, scrub and removal, since all wait on one kernel,
/// a bound only while emptyings come within it. Past it, and before the first, and from a run-out to the next answer,
/// the wait follows the box by its life alone, to its emptying ([`follow_kill`]; run 3's census seed ran out at 255.505
/// ms where run 5 answered in up to 904.705 ms: a slow emptying, not a wedged kernel). Its programs' reaping and its
/// init's end, which a box's emptying waits on, share the wait, as do the socket's waits for the programs it killed.
/// The `node.emptying` rows of `bench/competitive/probes/waits_census.md` are the evidence the rule is held to, never a
/// seed.
pub(crate) static EMPTYING: Patience = Patience::measuring("node.emptying");

/// A killed box as its emptying's wait looks at it: whether it empties within a time (`Duration::MAX`: until it does).
#[cfg(target_os = "linux")]
pub(crate) trait Emptying {
    /// Waits until the box holds no process, at most `timeout`; whether it holds none.
    fn empties_within(&self, timeout: Duration) -> Result<bool, hex_api::Refusal>;
}

#[cfg(target_os = "linux")]
impl Emptying for hex_runtime::linux::cgroup::BoxCgroup {
    fn empties_within(&self, timeout: Duration) -> Result<bool, hex_api::Refusal> {
        self.wait_empty(timeout)
    }
}

/// How a wait on a killed box went: whether its fact came, and the W* it went past, where it did.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Followed {
    pub came: bool,
    pub past: Option<Duration>,
}

/// Waits for `fact`, given each slice of the wait as a time, about a box killed at `killed`, as decision 9 waits (the
/// person's ruling of 2026-11-13) on `wait`, from the kill:
/// - **within W*** the fact is the answer;
/// - **past W*** the wait follows the box by its life alone, to the fact: a SIGKILL'd process's teardown is the
///   kernel's to finish, and no count of its work is fine enough to tell a teardown that pauses for a moment from a
///   wedge at W*'s scale, microseconds (with its cgroup's CPU time as the count, HEX-RT-1's delete was refused "init
///   still runs 304µs after SIGKILL", run r511), so none is taken for stalled;
/// - **with no W*** it waits for the fact alone.
///
/// Its census is its caller's: the run-out past W* ([`Followed::past`]) and the answer.
#[cfg(target_os = "linux")]
pub(crate) fn follow_kill(
    wait: &Patience,
    killed: Instant,
    mut fact: impl FnMut(Duration) -> Result<bool, hex_api::Refusal>,
) -> Result<Followed, hex_api::Refusal> {
    let Some(window) = wait.deadline() else {
        return Ok(Followed {
            came: fact(Duration::MAX)?,
            past: None,
        });
    };
    if fact(window.saturating_sub(killed.elapsed()))? {
        return Ok(Followed {
            came: true,
            past: None,
        });
    }
    Ok(Followed {
        came: fact(Duration::MAX)?,
        past: Some(window),
    })
}

/// Program `proc`'s end, killed (or its box gone) at `killed`, waited for as a killed box's emptying is ([`EMPTYING`];
/// decision 9, the person's ruling of 2026-11-13): within W*, from the kill, and past it, its run-out told once and no
/// sample, by the program's life alone, on its own events (`Duration::MAX`: until its end), however long that takes;
/// with no W* by its life from the start. Its end, however late, is the sample W* rises to.
pub(crate) fn end_of_killed<P: hex_api::Proc + ?Sized>(
    proc: &mut P,
    killed: Instant,
) -> Result<hex_api::Exit, hex_api::Refusal> {
    if let Some(window) = EMPTYING.deadline() {
        if let Some(exit) = proc.wait_for(window.saturating_sub(killed.elapsed()))? {
            EMPTYING.answered(killed.elapsed());
            return Ok(exit);
        }
        EMPTYING.ran_out(window);
    }
    loop {
        if let Some(exit) = proc.wait_for(Duration::MAX)? {
            EMPTYING.answered(killed.elapsed());
            return Ok(exit);
        }
    }
}
