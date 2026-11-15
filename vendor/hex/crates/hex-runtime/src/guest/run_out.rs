//! One thread for the process that tells a managed VM ask's run-out past its W* while the ask goes on (decision 9; the
//! review's "one run-out watcher", goal 4's design that ROOT approved on 2026-11-13). Each ask once spawned and joined a
//! watching thread of its own, twice a window for each VM on macOS and Windows (a rate's ask and a use's). Here every
//! armed ask waits in one table by the instant its W* passes; the watcher, started by the first ask armed, sleeps until
//! the earliest of them and tells each one past it, once, out of the table. An ask answered first takes its own entry
//! back, told nothing. The ask itself is the caller's, on the caller's thread: the watcher only tells.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hex_api::{BoxId, Refusal};
use hex_core::patience::{Patience, Piece};

/// Most asks armed at once: each of a node's boxes' VMs ([`crate::MAX_BOXES`]) with its use's ask and its rate's in
/// flight together, the two asks a VM is made ([`super::managed::rate`], [`super::managed::guest_time`]). Past it an
/// ask is refused, typed, before it is made.
pub(super) const MOST: usize = crate::MAX_BOXES.saturating_mul(2);

/// An ask armed: whose run-out is told past `w_star` from `began`, and the box whose VM it asks.
struct Armed {
    wait: &'static Patience,
    w_star: Duration,
    began: Instant,
    id: String,
}

struct State {
    /// Every armed ask by the instant its W* passes, then its ticket, the order they were armed in.
    armed: BTreeMap<(Instant, u64), Armed>,
    next: u64,
    /// Whether the watcher's thread has started: by the first ask armed, for the process's life.
    started: bool,
}

/// The table and the watcher's wake: an ask armed sooner than every other wakes it to sleep less.
struct Watch {
    state: hex_wait::Mutex<State>,
    changed: hex_wait::Condvar,
}

static WATCH: Watch = Watch {
    state: hex_wait::Mutex::new(State {
        armed: BTreeMap::new(),
        next: 0,
        started: false,
    }),
    changed: hex_wait::Condvar::new(),
};

fn poisoned() -> Refusal {
    Refusal::Broken("the managed VM asks' run-out watch is poisoned".to_owned())
}

/// `ask`'s answer, box `id`'s VM's: armed in the watch while it goes on, its run-out told once past `w_star` from now
/// as `wait`'s (decision 9: the ask follows its party past W*, never ended there), and taken back once it answers.
pub(super) fn told_past<T>(
    wait: &'static Patience,
    w_star: Duration,
    id: &BoxId,
    ask: impl FnOnce() -> T,
) -> Result<T, Refusal> {
    let armed = arm(wait, w_star, id)?;
    let answer = ask();
    drop(armed);
    Ok(answer)
}

/// An ask's place in the watch, taken back as it drops.
struct Ticket {
    key: (Instant, u64),
}

impl Drop for Ticket {
    fn drop(&mut self) {
        // A poisoned watch tells nothing more: the entry goes with it.
        if let Ok(mut state) = WATCH.state.lock() {
            state.armed.remove(&self.key);
        }
    }
}

fn arm(wait: &'static Patience, w_star: Duration, id: &BoxId) -> Result<Ticket, Refusal> {
    let began = Instant::now();
    let at = began.checked_add(w_star).ok_or_else(|| Refusal::Invalid {
        what: "a managed VM ask's W*",
        why: "it passes what a monotonic instant holds".to_owned(),
    })?;
    let mut name = String::new();
    name.try_reserve_exact(id.as_str().len())
        .map_err(|_| Refusal::Exhausted {
            what: "a managed VM ask's box name",
            bound: id.as_str().len(),
        })?;
    name.push_str(id.as_str());
    let mut state = WATCH.state.lock().map_err(|_| poisoned())?;
    if state.armed.len() >= MOST {
        return Err(Refusal::Exhausted {
            what: "managed VM asks armed at once",
            bound: MOST,
        });
    }
    if !state.started {
        std::thread::Builder::new()
            .name("hex-run-out".to_owned())
            .spawn(watch)
            .map_err(|why| Refusal::os("starting the managed VM asks' run-out watcher", why))?;
        state.started = true;
    }
    let key = (at, state.next);
    state.next = state.next.wrapping_add(1);
    let soonest = state.armed.keys().next().is_none_or(|first| key < *first);
    state.armed.insert(
        key,
        Armed {
            wait,
            w_star,
            began,
            id: name,
        },
    );
    drop(state);
    if soonest {
        WATCH.changed.notify_one();
    }
    Ok(Ticket { key })
}

/// The watcher: sleeps until the earliest armed ask's W* passes, or one sooner is armed, then tells every ask past its
/// W*, once, out of the table, outside its lock. A poisoned watch ends it: no ask waits on it, and each tells nothing.
fn watch() {
    let Ok(mut state) = WATCH.state.lock() else {
        return;
    };
    loop {
        let now = Instant::now();
        // Every ask whose W* has passed is the table's first, in its order: split them off whole.
        let later = state.armed.split_off(&(now, u64::MAX));
        let due = std::mem::replace(&mut state.armed, later);
        if !due.is_empty() {
            drop(state);
            for armed in due.into_values() {
                armed.wait.ran_out(
                    armed.w_star,
                    armed.began.elapsed(),
                    &[
                        Piece::Kind("the managed VM process of box "),
                        Piece::Name(&armed.id),
                        Piece::Kind(", its ask on its vsock socket"),
                    ],
                );
            }
            state = match WATCH.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            continue;
        }
        let waited = match state.armed.keys().next().map(|(at, _)| *at) {
            None => WATCH.changed.wait(state),
            Some(at) => WATCH
                .changed
                .wait_timeout(state, at.saturating_duration_since(now))
                .map(|(state, _)| state),
        };
        state = match waited {
            Ok(state) => state,
            Err(_) => return,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watch's bound's basis (Ada's rule, 2026-11-06: every number derived): two asks for each box a node holds.
    #[test]
    fn its_bound_is_two_asks_for_each_box() {
        assert_eq!(MOST, crate::MAX_BOXES * 2);
    }

    /// Decision 9: an ask past its W* is told once while it goes on, and an ask answered within it is told nothing,
    /// its entry taken back. Do: arm one ask answered at once, and one with a W* that passes while it is held, held until
    /// its wait follows its party (a run-out's mark). Expect: the held one told, the answered one not, and neither's
    /// entry left in the watch.
    #[test]
    fn a_held_ask_past_its_w_star_is_told_and_an_answered_one_never() {
        use hex_testkit::waits::follow::{self, Party};
        static TOLD: Patience =
            Patience::seeded("test.hex_runtime.run_out.held", Duration::from_secs(1));
        static QUIET: Patience =
            Patience::seeded("test.hex_runtime.run_out.answered", Duration::from_secs(1));
        static RAN_OUT: Patience = Patience::measuring("test.hex_runtime.run_out.told");
        let id = BoxId::new("told").unwrap();
        told_past(&QUIET, Duration::from_secs(3_600), &id, || ()).unwrap();
        told_past(&TOLD, Duration::from_millis(1), &id, || {
            follow::until(
                &RAN_OUT,
                "the held ask's run-out, told by the watcher",
                || Party::Live,
                || TOLD.deadline().is_none(),
            )
        })
        .unwrap()
        .unwrap();
        assert!(
            QUIET.deadline().is_some(),
            "an answered ask was told a run-out"
        );
        let state = WATCH.state.lock().unwrap();
        assert!(
            !state
                .armed
                .values()
                .any(|armed| std::ptr::eq(armed.wait, &TOLD) || std::ptr::eq(armed.wait, &QUIET)),
            "an ask's entry outlived it"
        );
    }
}
