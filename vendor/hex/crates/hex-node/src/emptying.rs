//! How long a killed box takes to empty, learned (decision 9 of docs/design/runtime-architecture.md): one wait for
//! every OS's node, since the socket's core waits on its programs' ends by it too ([`crate::serve`]), and Linux's node
//! on its boxes' emptying, their programs' reaping and their inits' ends.

use std::time::{Duration, Instant};

use hex_core::patience::{Patience, Piece};

/// How long a killed box takes to empty, from its kill (decision 9; Ada's rules of 2026-11-06 and 2026-11-13): W*, the
/// slowest emptying this process's nodes have seen, from every delete, scrub and removal, since all wait on one kernel,
/// a bound only while emptyings come within it. Past it, and before the first, and from a run-out to the next answer,
/// the wait follows the box by its life alone, to its emptying ([`wait_killed`]; run 3's census seed ran out at 255.505
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
        #[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
        if !HOLD.lets_empty(self.dir(), timeout)? {
            return Ok(false);
        }
        self.wait_empty(timeout)
    }
}

/// Waits for `fact`, given each slice of the wait as a time, about a box killed at `killed`, as decision 9 waits (the
/// person's ruling of 2026-11-13) on `wait`, from the kill, on the caller's own thread: whether it came.
/// - **within W*** the fact is the answer ([`within_kill`]);
/// - **past W*** its run-out is told as W* passes, once and with no sample, and the wait follows the box by its life
///   alone, to the fact ([`follow_rest`]): a SIGKILL'd process's teardown is the kernel's to finish, and no count of its
///   work is fine enough to tell a teardown that pauses for a moment from a wedge at W*'s scale, microseconds (with its
///   cgroup's CPU time as the count, HEX-RT-1's delete was refused "init still runs 304µs after SIGKILL", run r511), so
///   none is taken for stalled;
/// - **with no W*** it waits for the fact alone.
///
/// The fact, however late, is the sample W* rises to. A thread other boxes share takes the halves apart, the follow
/// handed to a thread of the box's own (the review's E-3).
#[cfg(target_os = "linux")]
pub(crate) fn wait_killed(
    wait: &Patience,
    killed: Instant,
    party: &[Piece<'_>],
    mut fact: impl FnMut(Duration) -> Result<bool, hex_api::Refusal>,
) -> Result<bool, hex_api::Refusal> {
    match within_kill(wait, killed, party, &mut fact)? {
        Within::Came => Ok(true),
        Within::Follow => follow_rest(wait, killed, fact),
    }
}

/// How a wait on a killed box went within its W* ([`within_kill`]).
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Within {
    /// Its fact came within W*, the answer told.
    Came,
    /// It didn't: past W*, its run-out told once (no W* yet: from the kill), the box is to be followed by its life
    /// alone ([`follow_rest`]).
    Follow,
}

/// [`wait_killed`]'s bounded half, for a thread other boxes share (the supervising thread): the wait within W* alone,
/// so it holds the thread no longer than the slowest answer seen, its answer told; past W*, its run-out told once,
/// naming `party`, and the follow ([`follow_rest`]) is the caller's to hand to a thread of the box's own (decision 9: a
/// wait follows its party, but not on a thread other parties share; the review's E-3, 2026-11-14).
#[cfg(target_os = "linux")]
pub(crate) fn within_kill(
    wait: &Patience,
    killed: Instant,
    party: &[Piece<'_>],
    mut fact: impl FnMut(Duration) -> Result<bool, hex_api::Refusal>,
) -> Result<Within, hex_api::Refusal> {
    let Some(window) = wait.deadline() else {
        return Ok(Within::Follow);
    };
    if fact(window.saturating_sub(killed.elapsed()))? {
        wait.answered(killed.elapsed());
        return Ok(Within::Came);
    }
    wait.ran_out(window, killed.elapsed(), party);
    Ok(Within::Follow)
}

/// [`wait_killed`]'s following half: the box followed by its life alone to the fact, however late, its answer the sample
/// W* rises to; whether it came. Never on a thread other boxes share: there its caller hands it to a thread of the
/// box's own ([`within_kill`]).
#[cfg(target_os = "linux")]
pub(crate) fn follow_rest(
    wait: &Patience,
    killed: Instant,
    mut fact: impl FnMut(Duration) -> Result<bool, hex_api::Refusal>,
) -> Result<bool, hex_api::Refusal> {
    let came = fact(Duration::MAX)?;
    if came {
        wait.answered(killed.elapsed());
    }
    Ok(came)
}

/// A debug build's hold on one box's emptying, armed by a test alone ([`crate::Node::hold_emptying`]): while it stands,
/// the box's cgroup reads as not yet empty, a slice's wait ending empty-handed and a following wait waiting until the
/// hold goes, as a box whose last process sleeps uninterruptibly in the kernel past W* empties (the review's E-3). Absent
/// from release builds, and from any build without the test controls.
#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
pub(crate) static HOLD: Hold = Hold {
    state: std::sync::Mutex::new(Holding {
        held: None,
        following: Vec::new(),
    }),
    changed: std::sync::Condvar::new(),
};

#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
pub(crate) struct Hold {
    state: std::sync::Mutex<Holding>,
    changed: std::sync::Condvar,
}

#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
struct Holding {
    /// The cgroup directory's last name whose emptying is held: its box's id.
    held: Option<String>,
    /// The threads whose following waits are stopped at the hold now, by name.
    following: Vec<String>,
}

#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
fn held_poisoned<T>(_: std::sync::PoisonError<T>) -> hex_api::Refusal {
    hex_api::Refusal::Broken("the emptying hold's lock was poisoned".to_owned())
}

#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
impl Hold {
    /// Whether the cgroup at `dir` may be waited for as it is: not while its box's emptying is held, a slice's wait then
    /// spent and a following wait (`timeout` `Duration::MAX`) stopped until the hold goes.
    fn lets_empty(
        &self,
        dir: &std::path::Path,
        timeout: Duration,
    ) -> Result<bool, hex_api::Refusal> {
        let name = dir.file_name().and_then(|n| n.to_str());
        let state = self.state.lock().map_err(held_poisoned)?;
        if state.held.is_none() || state.held.as_deref() != name {
            return Ok(true);
        }
        if timeout != Duration::MAX {
            let (state, _) = self
                .changed
                .wait_timeout_while(state, timeout, |state| state.held.as_deref() == name)
                .map_err(held_poisoned)?;
            return Ok(state.held.as_deref() != name);
        }
        let thread = std::thread::current()
            .name()
            .unwrap_or("an unnamed thread")
            .to_owned();
        let mut state = state;
        state.following.push(thread.clone());
        self.changed.notify_all();
        let mut state = self
            .changed
            .wait_while(state, |state| state.held.as_deref() == name)
            .map_err(held_poisoned)?;
        if let Some(at) = state.following.iter().position(|t| *t == thread) {
            state.following.swap_remove(at);
        }
        Ok(true)
    }

    /// Holds box `id`'s emptying until the guard goes.
    pub(crate) fn hold(&'static self, id: &str) -> Result<EmptyingHold, hex_api::Refusal> {
        self.state.lock().map_err(held_poisoned)?.held = Some(id.to_owned());
        Ok(EmptyingHold { hold: self })
    }
}

/// The hold on one box's emptying ([`HOLD`]): it stands until this goes.
#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
pub struct EmptyingHold {
    hold: &'static Hold,
}

#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
impl EmptyingHold {
    /// The threads whose following waits are stopped at the hold now, by name: whose thread follows the box.
    pub fn following(&self) -> Result<Vec<String>, hex_api::Refusal> {
        Ok(self
            .hold
            .state
            .lock()
            .map_err(held_poisoned)?
            .following
            .clone())
    }
}

#[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
impl Drop for EmptyingHold {
    fn drop(&mut self) {
        if let Ok(mut state) = self.hold.state.lock() {
            state.held = None;
        }
        self.hold.changed.notify_all();
    }
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
        EMPTYING.ran_out(
            window,
            killed.elapsed(),
            &[
                Piece::Kind("the killed program pid "),
                Piece::Pid(i64::from(proc.pid())),
                Piece::Kind(if proc.on_this_host() {
                    ", by its own events"
                } else {
                    " in its guest, by its host's events"
                }),
            ],
        );
    }
    loop {
        if let Some(exit) = proc.wait_for(Duration::MAX)? {
            EMPTYING.answered(killed.elapsed());
            return Ok(exit);
        }
    }
}
