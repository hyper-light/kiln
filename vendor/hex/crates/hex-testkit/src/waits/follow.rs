//! The tests' waits as decision 9 waits them (the person's ruling of 2026-11-13; `docs/research/runtime/learned-waits.md`
//! "Owed", 3): each follows its fact's party, what makes the fact true as the test can look at it. W* is a bound only
//! while answers come within it:
//! - **Within W***, the slowest answer the wait has seen, the fact is the answer.
//! - **Past W*** the run-out is told to the census, once and with no sample, and never fails the test: the wait goes on
//!   while its party lives, and fails ([`Unmet`]) only at its party's end. No stall of the party's progress is judged:
//!   one judged over W*, or at the slowest gap seen, refuses healthy parties (learned-waits.md, decision 9's bound).
//! - **With no W*** (before the fact's first answer, and from a run-out to the next) it follows its party from its
//!   start.
//! - **Its answer**, however long it took, is the sample W* rises to.
//!
//! A party ([`Party`]) is [`Party::Ended`] once nothing can make the fact true (the thread that would has finished,
//! the child is reaped, the connection closed), and [`Party::Live`] while it can: the test's own subject, what it drives
//! and what makes the fact (the review's C-5). [`Thread`] gives a thread's. A party that stays alive and never makes its
//! fact holds its wait, as decision 9 holds a production wait: libtest names such a test once it has run a minute
//! (`test <name> has been running for over 60 seconds`), and its job's own end stops it.
//!
//! Three forms, by how the fact comes:
//! - [`within`]: a call that waits on its fact's own event (a read, an accept, a process's end, a channel), sliced at
//!   W*'s end within W* so a party that ended is seen, and given no deadline past it or with none, when it must end at
//!   its party's end itself. Each slice may end the call and make it again, so the call is one that may be made again.
//! - [`whole`]: a call that can't be cut and made again (a dial, whose cut abandons its handshake): given no deadline,
//!   it ends at its own answer or its party's end.
//! - [`until`]: a fact with no event to wait on, looked at, resting between looks as long as the last look took, so a
//!   wait spends at most half its time looking.

use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

pub use hex_core::patience::Party;
use hex_core::patience::{Patience, Piece};

/// A wait's fact that didn't come: its party ended without it, or the rest between its looks was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmet {
    /// The fact waited for.
    pub what: String,
    /// How long it was waited for.
    pub after: Duration,
    /// Why the wait ended without it, where it wasn't its party's end: the rest between its looks refused.
    pub rest: Option<String>,
}

impl fmt::Display for Unmet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.rest {
            None => write!(
                f,
                "no {} after {:?}: its party ended without it",
                self.what, self.after
            ),
            Some(why) => write!(
                f,
                "no {} after {:?}: its rest between looks was refused: {why}",
                self.what, self.after
            ),
        }
    }
}

/// Waits for `fact`, one with no event to wait on, as the [module](self) says: `party` is looked at beside it, and the
/// looks it took are returned. Between looks it rests as long as the last look took (the fact's and the party's), so it
/// spends at most half its time looking, at whatever a look costs here: a rest derived from the look itself, never a
/// picked period. A party that ends is taken for ended only once a last look at the fact, as it ends, hasn't found it.
pub fn until(
    wait: &Patience,
    what: &str,
    mut party: impl FnMut() -> Party,
    mut fact: impl FnMut() -> bool,
) -> Result<u64, Unmet> {
    super::census();
    let mut waiting = Waiting::new(wait);
    let mut looks = 0u64;
    loop {
        let look = Instant::now();
        looks = looks.saturating_add(1);
        if fact() {
            wait.answered(waiting.began.elapsed());
            return Ok(looks);
        }
        if waiting.look(wait, what, party()).is_err() {
            if fact() {
                wait.answered(waiting.began.elapsed());
                return Ok(looks.saturating_add(1));
            }
            return Err(waiting.unmet(what));
        }
        let rest = Instant::now()
            .checked_add(look.elapsed())
            .unwrap_or_else(Instant::now);
        if let Err(why) = hex_wait::rest_until(rest) {
            let mut unmet = waiting.unmet(what);
            unmet.rest = Some(why.to_string());
            return Err(unmet);
        }
    }
}

/// Waits for an answer through `call`, one that can't be cut and made again (a dial, whose cut would abandon its
/// handshake and make another), as the [module](self) says: given no deadline, it ends at its own answer or at its
/// party's end, which the call must see itself (a dialing endpoint's end ends its dial). Its answer, however long it
/// took, is the sample W* rises to, its run-out told first where it took past W*.
pub fn whole<T>(wait: &Patience, what: &str, call: impl FnOnce() -> T) -> T {
    super::census();
    let waiting = Waiting::new(wait);
    let answer = call();
    let took = waiting.began.elapsed();
    if let Some(bound) = waiting.bound
        && took >= bound
    {
        // No sample (Karn's point), but the run-out is told, after the call that ran past W* came back.
        wait.ran_out(
            bound,
            took,
            &[
                Piece::Kind("the call that can't be cut, for "),
                Piece::Name(what),
            ],
        );
    }
    wait.answered(took);
    answer
}

/// Waits for an answer through `call`, a wait that takes its own deadline (a read's, a channel's), as the
/// [module](self) says: `call` is given the end of its slice (W*'s end within it; `None` past it and with no W*, when
/// its own answer or its party's end, as `call` sees it, ends it) and gives its answer, or `None` where the slice
/// ended first; `party` is looked at between slices. A party that ends is taken for ended only once a last slice, with
/// no time left, hasn't given the answer.
pub fn within<T>(
    wait: &Patience,
    what: &str,
    mut party: impl FnMut() -> Party,
    mut call: impl FnMut(Option<Instant>) -> Option<T>,
) -> Result<T, Unmet> {
    super::census();
    let mut waiting = Waiting::new(wait);
    let mut by = waiting.slice();
    loop {
        if let Some(answer) = call(by) {
            wait.answered(waiting.began.elapsed());
            return Ok(answer);
        }
        match waiting.look(wait, what, party()) {
            Ok(next) => by = next,
            Err(Ended) => {
                // The answer may have come as its party ended: one more look, with no time left.
                if let Some(answer) = call(Some(Instant::now())) {
                    wait.answered(waiting.began.elapsed());
                    return Ok(answer);
                }
                return Err(waiting.unmet(what));
            }
        }
    }
}

/// A wait's party, looked at, has ended.
struct Ended;

/// A wait under way: within its W* until that passes, then following its party by its life alone.
struct Waiting {
    began: Instant,
    /// W*, where the wait has one.
    bound: Option<Duration>,
    /// Whether W* has passed, its run-out told.
    past: bool,
}

impl Waiting {
    fn new(wait: &Patience) -> Self {
        Self {
            began: Instant::now(),
            bound: wait.deadline(),
            past: false,
        }
    }

    /// When the next look comes at the latest: W*'s end while within it; none past it or with no W*.
    fn slice(&self) -> Option<Instant> {
        if self.past {
            return None;
        }
        self.bound.and_then(|bound| self.began.checked_add(bound))
    }

    /// The party as looked at now: when to look next at the latest ([`Waiting::slice`]), or its end. Past W* its
    /// run-out is told, once, naming `what` it waits for.
    fn look(
        &mut self,
        wait: &Patience,
        what: &str,
        party: Party,
    ) -> Result<Option<Instant>, Ended> {
        if !self.past
            && let Some(bound) = self.bound
            && self.began.elapsed() >= bound
        {
            // No sample (Karn's point), but the run-out is told, and the wait goes on.
            wait.ran_out(
                bound,
                self.began.elapsed(),
                &[Piece::Kind("the party of "), Piece::Name(what)],
            );
            self.past = true;
        }
        match party {
            Party::Ended => Err(Ended),
            Party::Live => Ok(self.slice()),
        }
    }

    fn unmet(&self, what: &str) -> Unmet {
        Unmet {
            what: what.to_owned(),
            after: self.began.elapsed(),
            rest: None,
        }
    }
}

/// A thread a test follows: made on the thread itself ([`Thread::current`]) and looked at from any other
/// ([`Thread::party`]), on every OS alike. It holds a weak reference to a token that the thread's own thread-locals hold,
/// so it names no thread id a later thread could reuse, nor any other process's thread.
#[derive(Debug, Clone)]
pub struct Thread(Weak<()>);

thread_local! {
    /// The calling thread's token: its thread-locals hold the one strong reference, which std drops as the thread ends.
    static TOKEN: Arc<()> = Arc::new(());
}

impl Thread {
    /// The calling thread. Refused only while its thread-locals are being dropped, as it ends.
    pub fn current() -> std::io::Result<Self> {
        TOKEN
            .try_with(Arc::downgrade)
            .map(Self)
            .map_err(std::io::Error::other)
    }

    /// The thread as a party: ended once std has dropped its thread-locals, as the thread ends, after its code has run
    /// (a panic's unwinding included) and before its join returns; live until then. std drops a thread's locals as it
    /// ends on every OS but for a Unix process's main thread, and on Windows the other threads of a process that exits
    /// (`std::thread::LocalKey`'s platform notes): neither is a thread a test follows. Its other thread-locals may be
    /// dropped after the token, so a wait takes one last look at its fact once the party reads ended ([`until`],
    /// [`within`]).
    pub fn party(&self) -> Party {
        if self.0.strong_count() == 0 {
            Party::Ended
        } else {
            Party::Live
        }
    }
}

#[cfg(test)]
mod tests;
