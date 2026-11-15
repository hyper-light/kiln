//! A run's box removed once its program ends (decision 7 of docs/design/runtime-architecture.md), on the
//! runtimes whose boxes nothing else watches: macOS's and Windows' `process` classes. (Linux's node agent's
//! supervisor removes a run's box there, watching the program's pidfd.) A thread of the run's own takes its
//! program, waits for its end, removes the box, and only then gives the caller the end, so a caller that saw
//! the end finds the box gone. It does so whether or not the caller still holds the run: a caller that lets
//! the run go leaves no box behind.
//!
//! A run's box carries its [`Claim`] from its making, and is removed once, by whichever of the run's end and a delete
//! claims it first, never after another box has taken its id (runtime goal 7; the review's E-11, the guarantee
//! 016cbe5b2 gave Linux). The run's setup holds the box until it is whole and its program watched, as Linux's create
//! turn does; a claimer's removal touches the box that carries its claim alone, which stays in view until that removal
//! ends, so no box takes its id meanwhile; and one that meets the setup or a removal under way waits for it, answering
//! as the removal went. The word comes on every path, a claim's drop saying its holder stopped where it unwound.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Given, Proc, Refusal};
use hex_core::patience::{Patience, Piece};
use hex_wait::{Condvar, Mutex, MutexGuard};

/// A wait for a run's setup to make its box whole and watch its program, as a delete or the run's end waits on it
/// (decision 9): within the slowest seen; past it, its run-out told once and the setup followed to its end, which comes
/// on every path ([`Making`]'s drop saying it stopped where it unwound). Its end, however late, is the sample W* rises
/// to.
static MADE: Patience = Patience::measuring("runtime.run_made");

/// A wait for the word of the claimer removing a run's box, as a delete or the run's end waits on it (decision 9; the
/// person's ruling of 2026-11-13): within the slowest word seen; past it, its run-out told once and the claimer followed
/// to its word, which comes on every path ([`Claimed`]'s drop saying it stopped where it unwound). Its word, however
/// late, is the sample W* rises to.
static SAID: Patience = Patience::measuring("runtime.run_said");

/// What ends a run's program: its box's KILL, which a run's box, made for that program alone, holds.
pub(crate) type Kill = Box<dyn FnMut() -> Result<(), Refusal> + Send>;

/// What removes a run's box once its program has ended: the runtime's own removal of the box that carries the run's
/// claim, which touches no other.
pub(crate) type Remove = Box<dyn FnOnce() -> Result<(), Refusal> + Send>;

/// The run's end as its thread leaves it: the program's exit, or why the end or the removal failed.
struct Ended {
    end: Mutex<Option<Result<Exit, Refusal>>>,
    came: Condvar,
}

fn poisoned<T>(_: PoisonError<T>) -> Refusal {
    Refusal::Broken("a run's end lock was poisoned".to_owned())
}

/// A wait for a run's end that couldn't wait: its lock poisoned, or the OS refused it (macOS's kqueue).
fn wait_refused(why: hex_wait::Failed) -> Refusal {
    match why {
        hex_wait::Failed::Poisoned => Refusal::Broken("a run's end lock was poisoned".to_owned()),
        hex_wait::Failed::Os(why) => Refusal::Os {
            op: "waiting for a run's end",
            why: why.to_string(),
        },
    }
}

/// Who removes a run's box (R04): its setup holds it while it makes it; then whichever of the run's end and a delete
/// claims it first removes it and says how that went, the others waiting for the word.
pub(crate) struct Claim {
    state: Mutex<Claiming>,
    moved: Condvar,
}

/// A run box's claim as it stands (R04): its setup making it; made, no one removing it; one claimer removing it; or
/// that claimer's word. It moves only forward, so each wait on it ends with one move.
enum Claiming {
    Making,
    Open,
    Claimed,
    Said(Said),
}

/// What a run box's claimer says once done with the box (R04): it is gone; why it couldn't be removed; or that the
/// claimer stopped before it said (its thread unwound), the box as it left it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Said {
    Removed,
    Failed(Refusal),
    Abandoned,
}

/// What one who would remove a run's box finds of its claim, its making waited out (R04).
enum Taken {
    /// The removal is this caller's, which then says how it went.
    Mine(Claimed),
    /// Another's removal is under way: its word is waited for.
    Theirs,
    /// A removal was done with, its word this.
    Done(Said),
}

fn poisoned_claim<T>(_: PoisonError<T>) -> Refusal {
    Refusal::Broken("a run box's claim lock was poisoned".to_owned())
}

impl Claim {
    /// A run box's claim, and its setup's hold on it from the box's making: the box is made carrying the claim, and the
    /// setup lets it go once the box is whole and its program watched ([`Making::made`]).
    pub(crate) fn making() -> (Arc<Self>, Making) {
        let claim = Arc::new(Self {
            state: Mutex::new(Claiming::Making),
            moved: Condvar::new(),
        });
        let making = Making {
            claim: Arc::clone(&claim),
            made: false,
        };
        (claim, making)
    }

    fn state(&self) -> Result<MutexGuard<'_, Claiming>, Refusal> {
        self.state.lock().map_err(poisoned_claim)
    }

    /// Moves the claim on to `to` and tells every waiter, its lock taken even where poisoned, so a move always reaches
    /// them (decision 9: the word comes on every path).
    fn moves(&self, to: Claiming) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = to;
        self.moved.notify_all();
    }

    /// Takes the box's removal, for the one caller that does, once the box is made: its making waited out as decision 9
    /// waits ([`MADE`]).
    fn take(self: &Arc<Self>, id: &BoxId) -> Result<Taken, Refusal> {
        let state = self.wait(
            &MADE,
            id,
            |state| matches!(state, Claiming::Making),
            &[
                Piece::Kind("the setup making run box "),
                Piece::Name(id.as_str()),
            ],
        )?;
        self.taken(state, id)
    }

    fn taken(
        self: &Arc<Self>,
        mut state: MutexGuard<'_, Claiming>,
        id: &BoxId,
    ) -> Result<Taken, Refusal> {
        Ok(match &*state {
            Claiming::Making => {
                return Err(Refusal::Broken(format!(
                    "run box {id}'s claim was waited out of its making and is making still"
                )));
            }
            Claiming::Open => {
                *state = Claiming::Claimed;
                Taken::Mine(Claimed {
                    claim: Arc::clone(self),
                    said: false,
                })
            }
            Claiming::Claimed => Taken::Theirs,
            Claiming::Said(said) => Taken::Done(said.clone()),
        })
    }

    /// The word of the claimer removing the box, waited for as decision 9 waits ([`SAID`]).
    fn said(&self, id: &BoxId) -> Result<Said, Refusal> {
        let claimer = [
            Piece::Kind("the claimer removing run box "),
            Piece::Name(id.as_str()),
            Piece::Kind(", its word"),
        ];
        match &*self.wait(
            &SAID,
            id,
            |state| matches!(state, Claiming::Claimed),
            &claimer,
        )? {
            Claiming::Said(said) => Ok(said.clone()),
            Claiming::Making | Claiming::Open | Claiming::Claimed => Err(Refusal::Broken(format!(
                "run box {id}'s claimer was waited for and said nothing"
            ))),
        }
    }

    /// Waits while `busy` holds of the claim, as decision 9 waits on `patience`: within W*; past it, its run-out told
    /// once, naming the claim's `holder`, which it follows to its move, which comes on every path. The move, however
    /// late, is the sample W* rises to.
    fn wait(
        &self,
        patience: &Patience,
        id: &BoxId,
        busy: fn(&Claiming) -> bool,
        holder: &[Piece<'_>],
    ) -> Result<MutexGuard<'_, Claiming>, Refusal> {
        let asked = Instant::now();
        let mut state = self.state()?;
        if !busy(&state) {
            return Ok(state);
        }
        #[cfg(test)]
        hold::waiting(id);
        if let Some(window) = patience.deadline() {
            let (within, _) = self
                .moved
                .wait_timeout_while(state, window, |state| busy(state))
                .map_err(|why| claim_wait_refused(why, id))?;
            state = within;
            if busy(&state) {
                patience.ran_out(window, asked.elapsed(), holder);
            }
        }
        let state = self
            .moved
            .wait_while(state, |state| busy(state))
            .map_err(|why| claim_wait_refused(why, id))?;
        patience.answered(asked.elapsed());
        Ok(state)
    }

    /// Runs `act` on the box while no one removes it, the claim held through it, so no removal takes the box meanwhile
    /// and no box made again under its id since is ever acted on (a run's KILL): Ok without it once the box is being
    /// removed or gone, its program ended or ending with it.
    fn unremoved(&self, act: &mut Kill) -> Result<(), Refusal> {
        let state = self.state()?;
        match &*state {
            Claiming::Making | Claiming::Open => act(),
            Claiming::Claimed | Claiming::Said(_) => Ok(()),
        }
    }
}

fn claim_wait_refused(why: hex_wait::Failed, id: &BoxId) -> Refusal {
    match why {
        hex_wait::Failed::Poisoned => poisoned_claim(PoisonError::new(())),
        hex_wait::Failed::Os(why) => Refusal::Os {
            op: "waiting on a run box's claim",
            why: format!("box {id}: {why}"),
        },
    }
}

/// A run's setup's hold on its box's claim (R04): while it stands, a delete or the run's end waits for the box to be
/// made whole and its program watched. Let go ([`Making::made`]), the box is a delete's or the run's end's to take;
/// dropped unmade (the setup unwound, or made no box), it says so ([`Said::Abandoned`]).
pub(crate) struct Making {
    claim: Arc<Claim>,
    made: bool,
}

impl Making {
    /// The setup is done with the box, its program watched or not: a delete or the run's end may take it from here, the
    /// setup's own removal of a run that couldn't start among them.
    pub(crate) fn made(mut self) {
        self.made = true;
        self.claim.moves(Claiming::Open);
    }
}

impl Drop for Making {
    fn drop(&mut self) {
        if !self.made {
            self.claim.moves(Claiming::Said(Said::Abandoned));
        }
    }
}

/// A run box's removal its one claimer holds (R04): what it says goes to every waiter, and where it stops without a
/// word (its thread unwound) its drop says [`Said::Abandoned`], so no waiter waits on a claimer gone.
struct Claimed {
    claim: Arc<Claim>,
    said: bool,
}

impl Claimed {
    /// Removes the box with `remove`, says how that went, and gives it.
    fn remove(mut self, remove: impl FnOnce() -> Result<(), Refusal>) -> Result<(), Refusal> {
        let removed = remove();
        self.said = true;
        self.claim.moves(Claiming::Said(match &removed {
            Ok(()) => Said::Removed,
            Err(why) => Said::Failed(why.clone()),
        }));
        removed
    }
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if !self.said {
            self.claim.moves(Claiming::Said(Said::Abandoned));
        }
    }
}

/// A delete of run box `id`, which carries `claim`: removed with `remove`, which touches the box that carries the claim
/// alone, where the delete takes it first (its making waited out); else the word of the claimer whose removal is under
/// way waited for, the delete answering as that removal went and removing the box itself only where that claimer
/// stopped without a word. A removal done with but not done leaves the box a delete's, as any box is (runtime goal 7).
pub(crate) fn delete(
    claim: &Arc<Claim>,
    id: &BoxId,
    remove: impl FnOnce() -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    match claim.take(id)? {
        Taken::Mine(mine) => mine.remove(remove),
        Taken::Theirs => match claim.said(id)? {
            Said::Removed => Ok(()),
            Said::Failed(why) => Err(why),
            Said::Abandoned => remove(),
        },
        Taken::Done(Said::Removed) => Ok(()),
        Taken::Done(Said::Failed(_) | Said::Abandoned) => remove(),
    }
}

/// The run's end, its program having ended at `exit`: the box removed with `remove` where the run claims it first (its
/// making waited out), the end then the program's or why the box wasn't removed; else the word of the delete removing
/// it, waited for, so a caller that sees the end finds the box gone.
fn end(
    claim: &Arc<Claim>,
    id: &BoxId,
    exit: Result<Exit, Refusal>,
    remove: Remove,
) -> Result<Exit, Refusal> {
    let said = match claim.take(id)? {
        Taken::Mine(mine) => {
            #[cfg(test)]
            hold::stop_here(id, hold::At::Removal);
            // A removal that fails is the run's to report, never dropped (CLAUDE.md §3).
            return match (exit, mine.remove(remove)) {
                (Ok(exit), Ok(())) => Ok(exit),
                (Err(why), _) => Err(why),
                (Ok(exit), Err(left)) => Err(Refusal::Broken(format!(
                    "run box {id}'s program ended ({exit:?}) but its box wasn't removed: {left}"
                ))),
            };
        }
        Taken::Theirs => claim.said(id)?,
        Taken::Done(said) => said,
    };
    match (exit, said) {
        (Err(why), _) => Err(why),
        (Ok(exit), Said::Removed) => Ok(exit),
        (Ok(exit), Said::Failed(left)) => Err(Refusal::Broken(format!(
            "run box {id}'s program ended ({exit:?}) but the delete that took its box couldn't remove it: {left}"
        ))),
        (Ok(exit), Said::Abandoned) => Err(Refusal::Broken(format!(
            "run box {id}'s program ended ({exit:?}) but the delete that took its box stopped before it said how its removal went"
        ))),
    }
}

/// A run's program as its caller holds it ([`hex_api::Runtime::run`]): its end comes from the run's thread.
pub(crate) struct Run {
    pid: u32,
    ended: Arc<Ended>,
    given: Option<Given>,
    kill: Kill,
    /// Its box's claim, which its KILL holds through ([`Claim::unremoved`]).
    claim: Arc<Claim>,
}

impl Run {
    /// Hands `program`, started in run box `id`, which carries `claim`, to a thread of its own, which waits for its
    /// end, removes the box with `remove` where it claims it first, then gives the end to the caller. The caller's
    /// ends of the streams the run asked for are taken here, before the program goes to the thread.
    pub(crate) fn start(
        mut program: Box<dyn Proc>,
        id: &BoxId,
        claim: &Arc<Claim>,
        kill: Kill,
        remove: Remove,
    ) -> Result<Self, Refusal> {
        #[cfg(test)]
        hold::stop_here(id, hold::At::Making);
        let given = program.streams()?;
        let pid = program.pid();
        let ended = Arc::new(Ended {
            end: Mutex::new(None),
            came: Condvar::new(),
        });
        let theirs = Arc::clone(&ended);
        let named = id.clone();
        let claimed = Arc::clone(claim);
        #[cfg(test)]
        let token = hold::token(id);
        std::thread::Builder::new()
            .name(format!("hex-run-{id}"))
            .spawn(move || {
                #[cfg(test)]
                let _token = token;
                // Waits as long as the program runs: each wait returns at its end, or with none yet.
                let exit = loop {
                    match program.wait_for(Duration::MAX) {
                        Ok(Some(exit)) => break Ok(exit),
                        Ok(None) => {}
                        Err(why) => break Err(why),
                    }
                };
                drop(program);
                #[cfg(test)]
                hold::stop_here(&named, hold::At::End);
                // The box goes before the end is given, so a caller that saw the end finds it gone.
                let end = end(&claimed, &named, exit, remove);
                if let Ok(mut slot) = theirs.end.lock() {
                    *slot = Some(end);
                }
                theirs.came.notify_all();
            })
            .map_err(|e| Refusal::os("starting a run's watch", e))?;
        Ok(Self {
            pid,
            ended,
            given: Some(given),
            kill,
            claim: Arc::clone(claim),
        })
    }
}

/// The refusal a run that couldn't start gives: `why`, and what removing the box it made then left
/// (`cleanup`), so neither is dropped.
pub(crate) fn undone(why: Refusal, cleanup: Result<(), Refusal>, id: &BoxId) -> Refusal {
    match cleanup {
        Ok(()) => why,
        Err(left) => Refusal::Broken(format!(
            "{why}; and run box {id} it made couldn't be removed: {left}"
        )),
    }
}

impl Proc for Run {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        let end = self.ended.end.lock().map_err(poisoned)?;
        (*end).clone().transpose()
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        let mut end = self.ended.end.lock().map_err(poisoned)?;
        loop {
            if let Some(got) = end.as_ref() {
                return got.clone().map(Some);
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => timeout,
            };
            if left.is_zero() {
                return Ok(None);
            }
            end = self
                .ended
                .came
                .wait_timeout(end, left)
                .map_err(wait_refused)?
                .0;
        }
    }

    /// Ends the run's program through its box's KILL: the box holds that program alone. A program that ended
    /// already is `Ok`, its box perhaps gone; so is one whose box a delete or the run's end is removing, and the KILL
    /// holds the box's claim through it, so it never reaches a box made again under the id since.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        match self.claim.unremoved(&mut self.kill) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
            Err(why) => Err(why),
        }
    }

    fn streams(&mut self) -> Result<Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }
}

/// The review's E-11 test control (runtime goal 7): a hold on one run box's setup or thread. While it stands for box
/// `id`, the run stops where the hold says, as a thread descheduled there would: its setup once the box is made and its
/// program started, before the watch starts ([`hold::At::Making`]); its thread once its program has ended, before it
/// claims the box's removal ([`hold::At::End`]), or once it has claimed it, before it removes the box
/// ([`hold::At::Removal`]). It counts the waits that began on the run's setup or a claimer's word, and gives the run's
/// thread as a party to follow. Keyed by box id, so tests side by side hold their own boxes alone.
#[cfg(test)]
pub(crate) mod hold {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};

    use hex_api::BoxId;
    use hex_core::patience::Party;

    /// Where a held run's setup or thread stops.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum At {
        /// The run's setup, once its box is made and its program started, before its watch starts.
        Making,
        /// Once its program has ended, before it claims the box's removal.
        End,
        /// Once it has claimed the box's removal, before it removes the box.
        Removal,
    }

    struct Held {
        at: At,
        /// Whether the run's setup or thread is stopped at the hold now.
        stopped: bool,
        /// The waits on the run's setup or the word of the box's claimer that began.
        waiting: u64,
        /// The run's thread's token, which the thread holds for its life.
        thread: Weak<()>,
    }

    static HOLDS: Mutex<BTreeMap<BoxId, Held>> = Mutex::new(BTreeMap::new());
    static GONE: Condvar = Condvar::new();

    fn holds() -> MutexGuard<'static, BTreeMap<BoxId, Held>> {
        HOLDS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The hold on box `id`'s run at `at`, standing until it goes. Taken before the run starts.
    pub(crate) fn hold(id: &BoxId, at: At) -> Hold {
        holds().insert(
            id.clone(),
            Held {
                at,
                stopped: false,
                waiting: 0,
                thread: Weak::new(),
            },
        );
        Hold(id.clone())
    }

    /// A hold, standing until this goes.
    pub(crate) struct Hold(BoxId);

    impl Hold {
        /// Whether the run's setup or thread is stopped at the hold now.
        pub(crate) fn stopped(&self) -> bool {
            holds().get(&self.0).is_some_and(|held| held.stopped)
        }

        /// The waits on the run's setup or the word of the box's claimer that began.
        pub(crate) fn waiting(&self) -> u64 {
            holds().get(&self.0).map_or(0, |held| held.waiting)
        }

        /// The run's thread as a party: live while it runs, ended once it has gone.
        pub(crate) fn thread(&self) -> Party {
            match holds().get(&self.0) {
                Some(held) if held.thread.strong_count() > 0 => Party::Live,
                _ => Party::Ended,
            }
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            holds().remove(&self.0);
            GONE.notify_all();
        }
    }

    /// The token box `id`'s run's thread holds for its life, where a hold stands for the box.
    pub(super) fn token(id: &BoxId) -> Option<Arc<()>> {
        let token = Arc::new(());
        holds().get_mut(id)?.thread = Arc::downgrade(&token);
        Some(token)
    }

    /// Box `id`'s run's setup or thread at `at`: stopped there while a hold at `at` stands for the box.
    pub(super) fn stop_here(id: &BoxId, at: At) {
        let mut holds = holds();
        match holds.get_mut(id) {
            Some(held) if held.at == at => held.stopped = true,
            _ => return,
        }
        let _gone = GONE
            .wait_while(holds, |holds| holds.contains_key(id))
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// A wait on box `id`'s run's setup or the word of its claimer begins, a delete's or the run's end's.
    pub(super) fn waiting(id: &BoxId) {
        if let Some(held) = holds().get_mut(id) {
            held.waiting = held.waiting.saturating_add(1);
        }
    }
}
