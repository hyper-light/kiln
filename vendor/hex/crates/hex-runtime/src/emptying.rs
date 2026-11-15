//! A killed `process` box's emptying on macOS and Windows, waited for as decision 9 of
//! docs/design/runtime-architecture.md waits (the review's E-12): learned, its run-out told, the box followed by its
//! members' own ends past it, never refused at a time. Each OS's shell says what the box's emptying is and how its
//! members' ends are seen (macOS: its process group, by kqueue; Windows: its job, by its members' process handles); this
//! is the wait they share.

use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_core::patience::{Patience, Piece};

/// A killed `process` box's emptying, from its kill to its last member's end (decision 9; the person's rulings of
/// 2026-11-06 and 2026-11-13): W*, the slowest emptying this process has seen, from every delete and every recovery's
/// end of a box, since all wait on one kernel; past it, and before the first, and from a run-out to the next answer, the
/// box followed by its members' own ends to its emptying, never refused at a time. It was `hex_core::drain`'s bound,
/// one machine's kill-to-empty record times a margin of 2, refused past it while the group was listed again and again
/// (the review's E-12). No census run has its rows yet: MEASURING.
pub(crate) static EMPTYING: Patience = Patience::measuring("process.emptying");

/// Waits for a box killed at `killed` to empty, given each slice of the wait as a time (`Duration::MAX`: until it does),
/// as decision 9 waits on `wait`: within W*, its emptying; past W*, its run-out told once, naming `party`, and the box
/// followed by its members' ends to its emptying, however late; with no W* (before the first emptying, and from a
/// run-out to the next) followed from the kill. Its emptying, however late, is the sample W* rises to.
pub(crate) fn followed(
    wait: &Patience,
    killed: Instant,
    party: &[Piece<'_>],
    mut empties_within: impl FnMut(Duration) -> Result<bool, Refusal>,
) -> Result<(), Refusal> {
    if let Some(window) = wait.deadline() {
        if empties_within(window.saturating_sub(killed.elapsed()))? {
            wait.answered(killed.elapsed());
            return Ok(());
        }
        wait.ran_out(window, killed.elapsed(), party);
    }
    loop {
        if empties_within(Duration::MAX)? {
            wait.answered(killed.elapsed());
            return Ok(());
        }
    }
}

/// The review's E-12 test control: a hold on one killed box's emptying. While it stands for a box, the box reads as not
/// yet empty, as a box whose last member's teardown runs long reads, so a test meets a wait on it past any bound: a
/// slice's wait ends empty-handed, and a following wait stops at the hold, counted, until it goes. Keyed by what names
/// the box to its OS's wait (a macOS box's process group, by its id; a Windows box's id, or its job's name where a
/// recovery ends it), so tests side by side hold their own boxes alone.
#[cfg(test)]
pub(crate) mod hold {
    use std::collections::BTreeMap;
    use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    /// The holds standing, each with how many waits are stopped at it.
    static HOLDS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    static GONE: Condvar = Condvar::new();

    fn holds() -> MutexGuard<'static, BTreeMap<String, u64>> {
        HOLDS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The hold on box `key`'s emptying, standing until it goes.
    pub(crate) fn hold(key: &str) -> Hold {
        holds().insert(key.to_owned(), 0);
        Hold(key.to_owned())
    }

    /// A hold, standing until this goes.
    pub(crate) struct Hold(String);

    impl Hold {
        /// Whether a wait is stopped at the hold now, following the box past any bound.
        pub(crate) fn following(&self) -> bool {
            holds().get(&self.0).is_some_and(|stopped| *stopped > 0)
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            holds().remove(&self.0);
            GONE.notify_all();
        }
    }

    /// Whether box `key` may be waited for as it is: not while its emptying is held, a slice's wait (`slice`) then spent
    /// and a following wait (`Duration::MAX`) stopped, counted, until the hold goes.
    pub(crate) fn lets_empty(key: &str, slice: Duration) -> bool {
        let mut holds = holds();
        if !holds.contains_key(key) {
            return true;
        }
        if slice != Duration::MAX {
            let (holds, _) = GONE
                .wait_timeout_while(holds, slice, |holds| holds.contains_key(key))
                .unwrap_or_else(PoisonError::into_inner);
            return !holds.contains_key(key);
        }
        if let Some(stopped) = holds.get_mut(key) {
            *stopped = stopped.saturating_add(1);
        }
        let _gone = GONE
            .wait_while(holds, |holds| holds.contains_key(key))
            .unwrap_or_else(PoisonError::into_inner);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use hex_core::patience::{Patience, Piece};

    use super::followed;

    /// The review's E-12 (decision 9), AC-T7.4.1: a killed box's emptying past W* tells its run-out once, before it follows
    /// the box, and the emptying, however late, is the sample W* rises to. Do: a wait whose W* is 1 ns; a box that empties
    /// only for a wait that follows it (`Duration::MAX`), each look noting whether the run-out was told by then. Expect:
    /// one look within W*, then one following, the run-out told by then; Ok; and W* risen to the emptying.
    #[test]
    fn e12_an_emptying_past_its_w_star_tells_its_run_out_and_follows_the_box() {
        let wait = Patience::measuring("test.hex_runtime.emptying_past");
        wait.answered(Duration::from_nanos(1));
        let mut looks = Vec::new();
        let ended = followed(
            &wait,
            Instant::now(),
            &[Piece::Kind("the test's box")],
            |slice| {
                looks.push((slice == Duration::MAX, wait.deadline().is_none()));
                Ok(slice == Duration::MAX)
            },
        );
        assert_eq!(ended, Ok(()));
        assert_eq!(looks, [(false, false), (true, true)]);
        assert!(
            wait.slowest()
                .is_some_and(|slowest| slowest > Duration::from_nanos(1))
        );
    }

    /// Decision 9's cold start at a killed box's emptying (the review's E-12), AC-T7.4.1: before any emptying is seen there
    /// is no bound, so the box is followed from its kill, no run-out told, and its emptying is W*'s first sample. Do: a wait
    /// with no answer yet; a box that empties at its first look. Expect: one look, a following one (`Duration::MAX`); Ok;
    /// W* set.
    #[test]
    fn e12_a_first_emptying_is_followed_from_the_kill_and_is_w_stars_first_sample() {
        let wait = Patience::measuring("test.hex_runtime.emptying_first");
        let mut looks = Vec::new();
        let ended = followed(
            &wait,
            Instant::now(),
            &[Piece::Kind("the test's box")],
            |slice| {
                looks.push(slice);
                Ok(true)
            },
        );
        assert_eq!(ended, Ok(()));
        assert_eq!(looks, [Duration::MAX]);
        assert!(wait.slowest().is_some());
    }
}
