//! Boxes made by a run ([`hex_api::Runtime::run`]): each watched through its program's pidfd, in the
//! supervisor's epoll set, and removed once the program has ended (decision 7 of
//! docs/design/runtime-architecture.md). Whoever meets the end first removes the box: the caller's own
//! wait, which spares the hop through the supervising thread, the supervising thread when no caller
//! waits, or a delete. Each run is claimed once, under the supervisor's lock, and the others wait for what
//! the claimer publishes, so a box is removed once and never after another box has taken its id. A run's
//! pidfd is one of the supervisor's, held to its budget with the inits' ([`Supervisor::reserve`]).

use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Proc, Refusal};
use hex_core::record::ProcId;
use hex_runtime::linux::cgroup::{BoxCgroup, Names};
use rustix::event::epoll::{self, EventData, EventFlags};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions};

use super::{Slot, Stat, Supervisor, exit_of, owned, poisoned};
use crate::store::Store;

/// The runs a node watches, by box and by the epoll token each program's pidfd carries.
#[derive(Default)]
pub(super) struct Runs {
    by_box: BTreeMap<BoxId, Arc<Run>>,
    by_token: BTreeMap<u64, Arc<Run>>,
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
    pid: u32,
    token: u64,
    pidfd: OwnedFd,
    cgroup: BoxCgroup,
    /// The names of the root its cgroup is under, which its name goes from once the cgroup is removed.
    names: Names,
    /// What the run's claimer published once the box was gone: the program's end, or why the box
    /// couldn't be removed.
    end: Mutex<Option<Result<Exit, Refusal>>>,
    published: Condvar,
}

impl Run {
    fn end(&self) -> Result<MutexGuard<'_, Option<Result<Exit, Refusal>>>, Refusal> {
        self.end.lock().map_err(poisoned)
    }

    fn publish(&self, end: Result<Exit, Refusal>) -> Result<(), Refusal> {
        *self.end()? = Some(end);
        self.published.notify_all();
        Ok(())
    }
}

impl Supervisor {
    /// Watches box `id`'s run, its program `pid` by `pidfd`, until it ends and the box `cgroup` (under the root
    /// whose names `names` are) is removed with its record. Takes `slot`'s place in the budget. The program is
    /// owned from here, so no sweep reaps it first: the caller holds [`Supervisor::spawning`] from its spawn to
    /// this. A program that ended already turns its pidfd readable, which the epoll set reports at once. Refused,
    /// the pidfd comes back, for the caller to end and reap the program by.
    pub fn watch_run(
        self: &Arc<Self>,
        mut slot: Slot,
        id: &BoxId,
        pid: u32,
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
            Ok(mut owned) => owned.insert(pid),
            Err(e) => {
                let _ = epoll::delete(self.epoll.as_fd(), pidfd.as_fd());
                return Err((e, pidfd));
            }
        };
        let run = Arc::new(Run {
            id: id.clone(),
            pid,
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
                .watch_run(slot, id, program.pid, pidfd, cgroup, names)
                .map(drop)
                .map_err(|(e, _)| e),
            None => remove(id, &cgroup, &names, &self.store),
        }
    }

    /// Takes run `run` out of the watched runs: `true` for the one caller that does, which then removes
    /// its box ([`Supervisor::finish_run`]); `false` when another has.
    fn claim_run(&self, run: &Arc<Run>) -> Result<bool, Refusal> {
        let mut inits = self.inits()?;
        if !inits
            .runs
            .by_box
            .get(&run.id)
            .is_some_and(|r| Arc::ptr_eq(r, run))
        {
            return Ok(false);
        }
        inits.runs.by_box.remove(&run.id);
        inits.runs.by_token.remove(&run.token);
        epoll::delete(self.epoll.as_fd(), run.pidfd.as_fd())
            .map_err(|e| Refusal::os("unwatching a run's program", e))?;
        Ok(true)
    }

    /// Box `id`'s run, taken for its delete, which ends the box and then [`Supervisor::ended_run`]s it.
    /// `None` for a box no run watches, or one whose run another is removing.
    pub fn take_run(&self, id: &BoxId) -> Result<Option<Arc<Run>>, Refusal> {
        let Some(run) = self.inits()?.runs.by_box.get(id).cloned() else {
            return Ok(None);
        };
        if self.claim_run(&run)? {
            Ok(Some(run))
        } else {
            Ok(None)
        }
    }

    /// A run its delete has ended, `removed` saying whether it removed the box: the program reaped once
    /// the delete emptied the box, and the end published, or why the delete failed.
    pub fn ended_run(&self, run: &Run, removed: Result<(), Refusal>) -> Result<(), Refusal> {
        let end = match removed {
            Ok(()) => reap(run, WaitIdOptions::EXITED),
            // The program may run on in the box the delete left: no longer this run's, it's the
            // sweep's to reap once it ends, as any box process orphaned to the agent is.
            Err(e) => {
                owned()?.remove(&run.pid);
                Err(e)
            }
        };
        run.publish(end)
    }

    /// Removes a claimed run's box once its program has ended: the program reaped, whatever it left in
    /// the box ended (as `hex box run` ends it), the cgroup and the record removed; then publishes
    /// the program's end, or why the box couldn't be removed, which it also returns.
    fn finish_run(&self, run: &Run) -> Result<(), Refusal> {
        let removed = reap(run, WaitIdOptions::EXITED | WaitIdOptions::NOHANG)
            .and_then(|exit| remove(&run.id, &run.cgroup, &run.names, &self.store).map(|()| exit));
        let failed = removed.as_ref().err().cloned();
        run.publish(removed)?;
        failed.map_or(Ok(()), Err)
    }

    /// The supervising thread's pass over the runs whose programs ended.
    pub(super) fn runs_ready(&self, ready: &[Arc<Run>]) -> Result<(), Refusal> {
        let mut first_error = None;
        for run in ready {
            let done = match self.claim_run(run) {
                Ok(true) => self.finish_run(run),
                Ok(false) => Ok(()),
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
    owned()?.remove(&run.pid);
    exit
}

/// Ends what a run's program left in its box `id`, then removes the box's cgroup, its name from its root's
/// `names`, and its record: as a delete ends and removes a `process` box, `cgroup.kill` and the node's learned
/// wait for the box to empty, what still holds it named past that.
fn remove(id: &BoxId, cgroup: &BoxCgroup, names: &Names, store: &Store) -> Result<(), Refusal> {
    if cgroup.populated()? {
        let killed = Instant::now();
        cgroup.kill()?;
        let (emptied, patience) = crate::node::wait_empty(cgroup, killed)?;
        if !emptied {
            let op = "removing a run's box";
            let mut why = format!("box {id} still holds processes {patience:?} after cgroup.kill:");
            why.push_str(&crate::node::survivors(
                cgroup,
                crate::node::why_room(op, &why),
            ));
            return Err(Refusal::Os { op, why });
        }
    }
    cgroup.remove()?;
    names.forget(id.as_str());
    store.remove(id)
}

/// The caller's [`Proc`] for a run: its end is reported once the box is gone. Its own wait removes the
/// box when it meets the end first; dropped, it leaves that to the supervising thread.
pub struct RunProc {
    supervisor: Arc<Supervisor>,
    run: Arc<Run>,
}

impl RunProc {
    /// The published end, if there is one.
    fn published(&self) -> Result<Option<Exit>, Refusal> {
        self.run.end()?.clone().transpose()
    }

    /// Once the program has ended: removes the box if this caller claims it, else waits at most `left`
    /// for the claimer to publish.
    fn ended(&self, left: Duration) -> Result<Option<Exit>, Refusal> {
        if self.supervisor.claim_run(&self.run)? {
            // The removal's failure is the caller's to see, and published for any other waiter.
            self.supervisor.finish_run(&self.run)?;
            return self.published();
        }
        let end = self.run.end()?;
        let (end, _) = self
            .run
            .published
            .wait_timeout_while(end, left, |end| end.is_none())
            .map_err(poisoned)?;
        end.clone().transpose()
    }
}

impl Proc for RunProc {
    fn pid(&self) -> u32 {
        self.run.pid
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
