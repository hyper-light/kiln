//! Boxes made by a run ([`hex_api::Runtime::run`]): each watched through its program's pidfd, in the
//! supervisor's epoll set, and removed once the program has ended (decision 7 of
//! docs/design/runtime-architecture.md). Whoever meets the end first removes the box: the caller's own
//! wait, which spares the hop through the supervising thread, the supervising thread when no caller
//! waits, or a delete. The supervising thread, which every box shares, waits for a box's emptying within W*
//! alone and hands one that hasn't emptied by then to a thread of its own (the review's E-3). Each run is
//! claimed once, under the supervisor's lock, and the others wait for what
//! the claimer says, so a box is removed once and never after another box has taken its id: a claimed run
//! stays in view until its claimer has said how the removal went, and a delete of its box waits for that
//! word rather than remove the box beside it (runtime goal 7). The word comes on every path, the claim's
//! drop saying the claimer stopped where it unwound. A run's pidfd is one of the supervisor's, held to its
//! budget with the inits' ([`Supervisor::reserve`]).

use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Proc, Refusal};
use hex_core::patience::{Patience, Piece};
use hex_core::record::ProcId;
use hex_runtime::linux::cgroup::{BoxCgroup, Names};
use rustix::event::epoll::{self, EventData, EventFlags};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions};

use super::{Slot, Stat, Supervisor, exit_of, owned, poisoned};
use crate::emptying::Within;
use crate::store::Store;

/// A delete's wait for the word of the run's claimer, whose removal is under way (decision 9; the person's ruling of
/// 2026-11-13): within the slowest word seen, the word; past it, its run-out told once and the claimer followed, its
/// word coming on every path ([`Claimed`]'s and [`Following`]'s drops saying it where they unwound). Its word, however
/// late, is the sample W* rises to.
static SAID: Patience = Patience::measuring("node.run_said");

/// The runs a node watches, by box and by the epoll token each program's pidfd carries, and those claimed and
/// not yet done with, by box: each claimer's removal under way, which a delete of the same box waits for
/// ([`Supervisor::take_run`]). A run is in one of them at a time, watched, then claimed, then neither once its
/// claimer has said how the removal went, so `claimed` holds at most the runs `by_box` held.
#[derive(Default)]
pub(super) struct Runs {
    by_box: BTreeMap<BoxId, Arc<Run>>,
    by_token: BTreeMap<u64, Arc<Run>>,
    claimed: BTreeMap<BoxId, Arc<Run>>,
}

impl Runs {
    /// How many it watches, a pidfd each.
    pub(super) fn len(&self) -> usize {
        self.by_box.len()
    }

    /// The run whose program's pidfd carries `token`.
    pub(super) fn by_token(&self, token: u64) -> Option<Arc<Run>> {
        self.by_token.get(&token).cloned()
    }
}

/// One box made by a run, and its program.
pub struct Run {
    id: BoxId,
    /// Its program, by pid and start time, as the box's record names it.
    program: ProcId,
    token: u64,
    pidfd: OwnedFd,
    cgroup: BoxCgroup,
    /// The names of the root its cgroup is under, which its name goes from once the cgroup is removed.
    names: Names,
    /// What the run's claimer said once done with the box.
    end: Mutex<Option<Said>>,
    published: Condvar,
}

/// What a run's claimer says once done with its box (R04): the box gone, with how its program ended; why the box
/// couldn't be removed; or that the claimer stopped before it said (its thread unwound), the box as it left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    Removed(Exit),
    Failed(Refusal),
    Abandoned,
}

impl Said {
    /// The run's end as its [`Proc`] reports it to its caller.
    fn end(&self, id: &BoxId) -> Result<Exit, Refusal> {
        match self {
            Self::Removed(exit) => Ok(*exit),
            Self::Failed(why) => Err(why.clone()),
            Self::Abandoned => Err(Refusal::Broken(format!(
                "box {id}'s run's claimer stopped before it said how the box's removal went"
            ))),
        }
    }
}

impl Run {
    fn end(&self) -> Result<MutexGuard<'_, Option<Said>>, Refusal> {
        self.end.lock().map_err(poisoned)
    }

    /// Its program, by pid and start time.
    pub fn program(&self) -> ProcId {
        self.program
    }

    /// What its claimer said once done with the box, waited for as decision 9 waits ([`SAID`]): within the slowest
    /// word seen; past it, its run-out told once, the claimer followed to its word, which comes on every path, its
    /// [`Claimed`]'s or [`Following`]'s drop saying [`Said::Abandoned`] where it unwound, so the wait needs no timer.
    pub fn said(&self) -> Result<Said, Refusal> {
        let asked = Instant::now();
        let mut end = self.end()?;
        if let Some(window) = SAID.deadline() {
            let (within, _) = self
                .published
                .wait_timeout_while(end, window, |end| end.is_none())
                .map_err(poisoned)?;
            end = within;
            if end.is_none() {
                SAID.ran_out(
                    window,
                    asked.elapsed(),
                    &[
                        Piece::Kind("the claimer removing the box of program pid "),
                        Piece::Pid(i64::from(self.program.pid)),
                        Piece::Kind(", its word"),
                    ],
                );
            }
        }
        let end = self
            .published
            .wait_while(end, |end| end.is_none())
            .map_err(poisoned)?;
        SAID.answered(asked.elapsed());
        end.clone().ok_or_else(|| {
            Refusal::Broken(format!(
                "box {}'s run's claimer was waited for and said nothing",
                self.id
            ))
        })
    }
}

/// A run its one claimer took out of the watched runs to remove its box: what it says goes to every waiter, and
/// where it stops without a word (its thread unwound) its drop says [`Said::Abandoned`], so no waiter waits on a
/// claimer gone.
pub struct Claimed<'a> {
    supervisor: &'a Supervisor,
    run: Arc<Run>,
    said: bool,
}

impl Claimed<'_> {
    /// Says how the removal went.
    fn say(mut self, said: Said) -> Result<(), Refusal> {
        self.said = true;
        self.supervisor.done_with(&self.run, said)
    }

    /// Its delete has ended the box ([`crate::Node`]'s), `removed` saying whether it removed it: the program reaped
    /// once the delete emptied the box, and that said, or why the delete failed.
    pub fn deleted(self, removed: Result<(), Refusal>) -> Result<(), Refusal> {
        let said = match removed {
            Ok(()) => match reap(&self.run, WaitIdOptions::EXITED) {
                Ok(exit) => Said::Removed(exit),
                Err(why) => Said::Failed(why),
            },
            // The program may run on in the box the delete left: no longer this run's, it's the
            // sweep's to reap once it ends, as any box process orphaned to the agent is.
            Err(why) => {
                owned()?.remove(&self.run.program.pid);
                Said::Failed(why)
            }
        };
        self.say(said)
    }

    /// Removes the box once its program has ended: the program reaped, whatever it left in the box ended (as
    /// `hex box run` ends it), the cgroup and the record removed; then says the program's end, or why the box
    /// couldn't be removed, which it also returns.
    fn finish(self) -> Result<(), Refusal> {
        #[cfg(all(debug_assertions, feature = "test-controls"))]
        self.supervisor.removals.stop_here()?;
        let run = &self.run;
        let removed = reap(run, WaitIdOptions::EXITED | WaitIdOptions::NOHANG).and_then(|exit| {
            remove(&run.id, &run.cgroup, &run.names, &self.supervisor.store).map(|()| exit)
        });
        let failed = removed.as_ref().err().cloned();
        self.say(match removed {
            Ok(exit) => Said::Removed(exit),
            Err(why) => Said::Failed(why),
        })?;
        failed.map_or(Ok(()), Err)
    }

    /// [`Claimed::finish`] on the supervising thread, which every other box's end and removal shares: the box waited
    /// for within W* alone; one that hasn't emptied by then, its run-out told once, is followed and taken apart by a
    /// thread of its own, which then says how the removal went (decision 9 follows a party by its life, never on a
    /// thread other parties share: the review's E-3, 2026-11-14). Those threads are at most the boxes the node holds:
    /// a followed box's cgroup stays under its root until it's taken apart, and a root holds at most its bound of them,
    /// past which a create is refused (`BoxRoot::count`, [`hex_runtime::MAX_BOXES`] or fewer).
    fn finish_supervised(self, supervisor: &Arc<Supervisor>) -> Result<(), Refusal> {
        #[cfg(all(debug_assertions, feature = "test-controls"))]
        self.supervisor.removals.stop_here()?;
        let run = Arc::clone(&self.run);
        let exit = match reap(&run, WaitIdOptions::EXITED | WaitIdOptions::NOHANG) {
            Ok(exit) => exit,
            Err(why) => return self.failed(why),
        };
        let killed = match kill_left(&run.cgroup) {
            Ok(None) => return self.taken_apart(exit),
            Ok(Some(killed)) => killed,
            Err(why) => return self.failed(why),
        };
        match crate::node::empties_within_bound(&run.cgroup, killed) {
            Ok(Within::Came) => self.taken_apart(exit),
            Ok(Within::Follow) => self.follow_apart(supervisor, exit, killed),
            Err(why) => self.failed(why),
        }
    }

    /// Says the removal failed for `why`, which it also returns.
    fn failed(self, why: Refusal) -> Result<(), Refusal> {
        self.say(Said::Failed(why.clone()))?;
        Err(why)
    }

    /// The emptied box taken apart, and that said with the program's end `exit`.
    fn taken_apart(self, exit: Exit) -> Result<(), Refusal> {
        let run = Arc::clone(&self.run);
        match take_apart(&run.id, &run.cgroup, &run.names, &self.supervisor.store) {
            Ok(()) => self.say(Said::Removed(exit)),
            Err(why) => self.failed(why),
        }
    }

    /// The box, killed at `killed` and not empty within W*, handed with the claim to a thread of its own
    /// ([`Following`]). A thread that can't be started drops the claim it was handed, which says the claimer stopped,
    /// so a delete removes the box itself; the refusal is the supervising thread's to note.
    fn follow_apart(
        mut self,
        supervisor: &Arc<Supervisor>,
        exit: Exit,
        killed: Instant,
    ) -> Result<(), Refusal> {
        let following = Following {
            supervisor: Arc::clone(supervisor),
            run: Arc::clone(&self.run),
            said: false,
        };
        // The word is the follower's to say from here.
        self.said = true;
        drop(self);
        std::thread::Builder::new()
            .name("hex-run-follow".to_owned())
            .spawn(move || following.finish(exit, killed))
            .map(drop)
            .map_err(|e| Refusal::os("starting a run's follower", e))
    }
}

/// A claimed run whose box didn't empty within W*, followed to its emptying and taken apart by a thread of its own
/// ([`Claimed::finish_supervised`]): its word goes to every waiter as a claim's does, and where it stops without one
/// its drop says [`Said::Abandoned`].
struct Following {
    supervisor: Arc<Supervisor>,
    run: Arc<Run>,
    said: bool,
}

impl Following {
    /// Follows the box, killed at `killed`, by its life alone to its emptying, then takes it apart and says the
    /// program's end `exit`, or why the box couldn't be removed, which the supervisor also keeps as its fault (§3).
    fn finish(mut self, exit: Exit, killed: Instant) {
        let run = Arc::clone(&self.run);
        let removed = crate::node::follow_to_empty(&run.cgroup, killed).and_then(|emptied| {
            if emptied {
                take_apart(&run.id, &run.cgroup, &run.names, &self.supervisor.store)
            } else {
                Err(still_holds(&run.id, &run.cgroup, killed.elapsed()))
            }
        });
        let said = match removed {
            Ok(()) => Said::Removed(exit),
            Err(why) => {
                self.supervisor.note(Err(why.clone()));
                Said::Failed(why)
            }
        };
        self.said = true;
        if let Err(why) = self.supervisor.done_with(&run, said) {
            self.supervisor.note(Err(why));
        }
    }
}

impl Drop for Following {
    fn drop(&mut self) {
        if !self.said {
            let _ = self.supervisor.done_with(&self.run, Said::Abandoned);
        }
    }
}

impl Drop for Claimed<'_> {
    fn drop(&mut self) {
        if !self.said {
            let _ = self.supervisor.done_with(&self.run, Said::Abandoned);
        }
    }
}

/// What a delete finds of its box's run ([`Supervisor::take_run`]).
pub enum Taken<'a> {
    /// Watched: claimed for the delete, which removes the box and then says so ([`Claimed::deleted`]).
    Mine(Claimed<'a>),
    /// Claimed by another, its removal under way: the delete waits for its word ([`Run::said`]) where the box's
    /// record still names this run's program.
    Theirs(Arc<Run>),
    /// No run of the box is watched or being removed.
    NoRun,
}

impl Supervisor {
    /// Watches box `id`'s run, its program `program` by `pidfd`, until it ends and the box `cgroup` (under the root
    /// whose names `names` are) is removed with its record. Takes `slot`'s place in the budget. The program is
    /// owned from here, so no sweep reaps it first: the caller holds [`Supervisor::spawning`] from its spawn to
    /// this. A program that ended already turns its pidfd readable, which the epoll set reports at once. Refused,
    /// the pidfd comes back, for the caller to end and reap the program by.
    pub fn watch_run(
        self: &Arc<Self>,
        mut slot: Slot,
        id: &BoxId,
        program: ProcId,
        pidfd: OwnedFd,
        cgroup: BoxCgroup,
        names: Names,
    ) -> Result<RunProc, (Refusal, OwnedFd)> {
        let mut inits = match self.inits() {
            Ok(inits) => inits,
            Err(e) => return Err((e, pidfd)),
        };
        let token = match inits.token() {
            Ok(token) => token,
            Err(e) => return Err((e, pidfd)),
        };
        if let Err(e) = epoll::add(
            self.epoll.as_fd(),
            pidfd.as_fd(),
            EventData::new_u64(token),
            EventFlags::IN,
        ) {
            return Err((Refusal::os("watching a run's program", e), pidfd));
        }
        match owned() {
            Ok(mut owned) => owned.insert(program.pid),
            Err(e) => {
                let _ = epoll::delete(self.epoll.as_fd(), pidfd.as_fd());
                return Err((e, pidfd));
            }
        };
        let run = Arc::new(Run {
            id: id.clone(),
            program,
            token,
            pidfd,
            cgroup,
            names,
            end: Mutex::new(None),
            published: Condvar::new(),
        });
        inits.runs.by_box.insert(id.clone(), Arc::clone(&run));
        inits.runs.by_token.insert(token, Arc::clone(&run));
        inits.reserved = inits.reserved.saturating_sub(1);
        slot.held = false;
        Ok(RunProc {
            supervisor: Arc::clone(self),
            run,
        })
    }

    /// Watches a run taken back after a restart, in `slot`'s place in the budget: its program, which ran when
    /// recovery looked, by a pidfd opened now and checked against the start time its record holds, so a later
    /// process given its pid is never watched for it. Once it ends its box is removed as any run's is, its end
    /// [`Exit::Unknown`]: the agent that started it, its parent, is gone. One that ended since is removed now.
    pub fn adopt_run(
        self: &Arc<Self>,
        slot: Slot,
        id: &BoxId,
        program: ProcId,
        cgroup: BoxCgroup,
        names: Names,
    ) -> Result<(), Refusal> {
        let pidfd = i32::try_from(program.pid)
            .ok()
            .and_then(Pid::from_raw)
            .and_then(|pid| rustix::process::pidfd_open(pid, PidfdFlags::empty()).ok());
        let same = |_: &OwnedFd| {
            i32::try_from(program.pid)
                .ok()
                .and_then(Stat::of)
                .is_some_and(|stat| stat.start_ticks == program.start_ticks)
        };
        match pidfd.filter(same) {
            Some(pidfd) => self
                .watch_run(slot, id, program, pidfd, cgroup, names)
                .map(drop)
                .map_err(|(e, _)| e),
            None => remove(id, &cgroup, &names, &self.store),
        }
    }

    /// Takes run `run` out of the watched runs into the claimed ones, for the one caller that does, which then
    /// removes its box ([`Claimed`]); `None` when another has.
    fn claim_run(&self, run: &Arc<Run>) -> Result<Option<Claimed<'_>>, Refusal> {
        let mut inits = self.inits()?;
        self.claim_in(&mut inits.runs, run)
    }

    /// [`Supervisor::claim_run`] under the supervisor's lock. Unwatched first, so a run that can't be is still
    /// watched rather than claimed by no one.
    fn claim_in(&self, runs: &mut Runs, run: &Arc<Run>) -> Result<Option<Claimed<'_>>, Refusal> {
        if !runs
            .by_box
            .get(&run.id)
            .is_some_and(|r| Arc::ptr_eq(r, run))
        {
            return Ok(None);
        }
        epoll::delete(self.epoll.as_fd(), run.pidfd.as_fd())
            .map_err(|e| Refusal::os("unwatching a run's program", e))?;
        runs.by_box.remove(&run.id);
        runs.by_token.remove(&run.token);
        runs.claimed.insert(run.id.clone(), Arc::clone(run));
        Ok(Some(Claimed {
            supervisor: self,
            run: Arc::clone(run),
            said: false,
        }))
    }

    /// What a delete of box `id` finds of its run: watched, claimed here for the delete ([`Taken::Mine`]); claimed by
    /// another, whose removal is under way ([`Taken::Theirs`]); or none.
    pub fn take_run(&self, id: &BoxId) -> Result<Taken<'_>, Refusal> {
        let mut inits = self.inits()?;
        if let Some(run) = inits.runs.by_box.get(id).cloned()
            && let Some(claimed) = self.claim_in(&mut inits.runs, &run)?
        {
            return Ok(Taken::Mine(claimed));
        }
        Ok(match inits.runs.claimed.get(id) {
            Some(run) => Taken::Theirs(Arc::clone(run)),
            None => Taken::NoRun,
        })
    }

    /// A claimer's word on its run: to every waiter, then the run out of the claimed ones, where its entry is still
    /// this run's (a box made again under the id may have been claimed there since).
    fn done_with(&self, run: &Arc<Run>, said: Said) -> Result<(), Refusal> {
        *run.end()? = Some(said);
        run.published.notify_all();
        let mut inits = self.inits()?;
        if inits
            .runs
            .claimed
            .get(&run.id)
            .is_some_and(|r| Arc::ptr_eq(r, run))
        {
            inits.runs.claimed.remove(&run.id);
        }
        Ok(())
    }

    /// Holds the runs' removals at their start until the guard goes ([`Removals`]).
    #[cfg(all(debug_assertions, feature = "test-controls"))]
    pub fn hold_removals(&self) -> Result<RemovalHold<'_>, Refusal> {
        self.removals.hold()
    }

    /// A delete waits on a removal another claimer has under way ([`Removals`]).
    #[cfg(all(debug_assertions, feature = "test-controls"))]
    pub fn waiting_on_removal(&self) -> Result<(), Refusal> {
        self.removals.waiting()
    }

    /// The supervising thread's pass over the runs whose programs ended.
    pub(super) fn runs_ready(self: &Arc<Self>, ready: &[Arc<Run>]) -> Result<(), Refusal> {
        let mut first_error = None;
        for run in ready {
            let done = match self.claim_run(run) {
                Ok(Some(claimed)) => claimed.finish_supervised(self),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            };
            if let Err(e) = done {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Reaps the run's program: how it ended, or [`Exit::Unknown`] where it isn't the agent's child (one
/// taken back after a restart, which its old agent started).
fn reap(run: &Run, options: WaitIdOptions) -> Result<Exit, Refusal> {
    let exit = match rustix::process::waitid(WaitId::PidFd(run.pidfd.as_fd()), options) {
        Ok(Some(status)) => Ok(exit_of(&status)),
        // Not ended: the claim follows its pidfd turning readable, or a delete's kill.
        Ok(None) => Err(Refusal::Broken(format!(
            "box {}'s run was claimed before its program ended",
            run.id
        ))),
        Err(Errno::CHILD) => Ok(Exit::Unknown),
        Err(e) => Err(Refusal::os("reaping a run's program", e)),
    };
    owned()?.remove(&run.program.pid);
    exit
}

/// Ends what a run's program left in its box `id`, then removes the box's cgroup, its name from its root's
/// `names`, and its record: as a delete ends and removes a `process` box, `cgroup.kill` and the node's learned
/// wait for the box to empty, what still holds it named past that.
/// On the caller's own thread, which follows the box past W* there.
fn remove(id: &BoxId, cgroup: &BoxCgroup, names: &Names, store: &Store) -> Result<(), Refusal> {
    if let Some(killed) = kill_left(cgroup)? {
        let (emptied, patience) = crate::node::wait_empty(cgroup, killed)?;
        if !emptied {
            return Err(still_holds(id, cgroup, patience));
        }
    }
    take_apart(id, cgroup, names, store)
}

/// What a run's program left in its box `cgroup`, ended: when it was killed, or none where nothing was left.
fn kill_left(cgroup: &BoxCgroup) -> Result<Option<Instant>, Refusal> {
    if !cgroup.populated()? {
        return Ok(None);
    }
    let killed = Instant::now();
    cgroup.kill()?;
    Ok(Some(killed))
}

/// Box `id`'s emptied `cgroup` removed, its name from its root's `names`, and its record.
fn take_apart(id: &BoxId, cgroup: &BoxCgroup, names: &Names, store: &Store) -> Result<(), Refusal> {
    cgroup.remove()?;
    names.forget(id.as_str());
    store.remove(id)
}

/// Box `id`'s cgroup still held processes `waited` after `cgroup.kill`: what still holds it, named.
fn still_holds(id: &BoxId, cgroup: &BoxCgroup, waited: Duration) -> Refusal {
    let op = "removing a run's box";
    let mut why = format!("box {id} still holds processes {waited:?} after cgroup.kill:");
    why.push_str(&crate::node::survivors(
        cgroup,
        crate::node::why_room(op, &why),
    ));
    Refusal::Os { op, why }
}

/// A debug build's hold on the runs' removals (runtime goal 7), armed by the node's caller alone
/// ([`crate::Node::hold_run_removals`]): while it stands, a claimer that has taken a run stops before it reaps the
/// program or touches the box, as one descheduled there would, so a test meets a delete beside a removal under way.
/// Absent from release builds, and from any build without the test controls.
#[cfg(all(debug_assertions, feature = "test-controls"))]
#[derive(Debug, Default)]
pub(super) struct Removals {
    state: Mutex<Holding>,
    changed: Condvar,
}

#[cfg(all(debug_assertions, feature = "test-controls"))]
#[derive(Debug, Default)]
struct Holding {
    held: bool,
    /// Claimers stopped at the hold now.
    stopped: u64,
    /// Deletes that waited on a removal another claimer had under way.
    waiting: u64,
}

#[cfg(all(debug_assertions, feature = "test-controls"))]
impl Removals {
    fn state(&self) -> Result<MutexGuard<'_, Holding>, Refusal> {
        self.state.lock().map_err(poisoned)
    }

    /// The hold, standing until the guard goes.
    pub(super) fn hold(&self) -> Result<RemovalHold<'_>, Refusal> {
        self.state()?.held = true;
        Ok(RemovalHold { removals: self })
    }

    /// A claimer at the start of its removal: stopped there while the hold stands.
    fn stop_here(&self) -> Result<(), Refusal> {
        let mut state = self.state()?;
        if !state.held {
            return Ok(());
        }
        state.stopped = state.stopped.saturating_add(1);
        self.changed.notify_all();
        let mut state = self
            .changed
            .wait_while(state, |state| state.held)
            .map_err(poisoned)?;
        state.stopped = state.stopped.saturating_sub(1);
        Ok(())
    }

    /// A delete waits on a removal another claimer has under way.
    pub(super) fn waiting(&self) -> Result<(), Refusal> {
        let mut state = self.state()?;
        state.waiting = state.waiting.saturating_add(1);
        self.changed.notify_all();
        Ok(())
    }
}

/// The hold on the runs' removals ([`Removals`]): it stands until this goes.
#[cfg(all(debug_assertions, feature = "test-controls"))]
pub struct RemovalHold<'a> {
    removals: &'a Removals,
}

#[cfg(all(debug_assertions, feature = "test-controls"))]
impl RemovalHold<'_> {
    /// The claimers stopped at the hold now.
    pub fn stopped(&self) -> Result<u64, Refusal> {
        Ok(self.removals.state()?.stopped)
    }

    /// The deletes that waited on a removal another claimer had under way.
    pub fn waiting(&self) -> Result<u64, Refusal> {
        Ok(self.removals.state()?.waiting)
    }
}

#[cfg(all(debug_assertions, feature = "test-controls"))]
impl Drop for RemovalHold<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.removals.state() {
            state.held = false;
        }
        self.removals.changed.notify_all();
    }
}

/// The caller's [`Proc`] for a run: its end is reported once the box is gone. Its own wait removes the
/// box when it meets the end first; dropped, it leaves that to the supervising thread.
pub struct RunProc {
    supervisor: Arc<Supervisor>,
    run: Arc<Run>,
}

impl RunProc {
    /// The end its claimer said, if it has.
    fn published(&self) -> Result<Option<Exit>, Refusal> {
        self.run
            .end()?
            .as_ref()
            .map(|said| said.end(&self.run.id))
            .transpose()
    }

    /// Once the program has ended: removes the box if this caller claims it, else waits at most `left`
    /// for the claimer's word.
    fn ended(&self, left: Duration) -> Result<Option<Exit>, Refusal> {
        if let Some(claimed) = self.supervisor.claim_run(&self.run)? {
            // The removal's failure is the caller's to see, and said to any other waiter.
            claimed.finish()?;
            return self.published();
        }
        let end = self.run.end()?;
        let (end, _) = self
            .run
            .published
            .wait_timeout_while(end, left, |end| end.is_none())
            .map_err(poisoned)?;
        end.as_ref().map(|said| said.end(&self.run.id)).transpose()
    }
}

impl Proc for RunProc {
    fn pid(&self) -> u32 {
        self.run.program.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        if let Some(exit) = self.published()? {
            return Ok(Some(exit));
        }
        if super::ended(self.run.pidfd.as_fd())? {
            return self.ended(Duration::ZERO);
        }
        Ok(None)
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        let left = || deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
        loop {
            if let Some(exit) = self.published()? {
                return Ok(Some(exit));
            }
            let mut fds = [PollFd::new(&self.run.pidfd, PollFlags::IN)];
            let wait = left();
            let ts = Timespec {
                tv_sec: i64::try_from(wait.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(wait.subsec_nanos()),
            };
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(e) => return Err(Refusal::os("poll on a run's pidfd", e)),
            }
            if fds.iter().any(|f| f.revents().contains(PollFlags::IN)) {
                return self.ended(left());
            }
            if left().is_zero() {
                return Ok(None);
            }
        }
    }

    /// KILL to the run's program alone, through its pidfd, so it never reaches a process given its pid since; its
    /// end then removes its box, as any end of a run's program does. `Ok` once sent, or when it has ended already.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.published()?.is_some() {
            return Ok(());
        }
        match rustix::process::pidfd_send_signal(&self.run.pidfd, rustix::process::Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(e) => Err(Refusal::os("killing a run's program", e)),
        }
    }

    /// A run's streams asked for are its node's to give ([`hex_api::Asked`]); this one holds none.
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}
