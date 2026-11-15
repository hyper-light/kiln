//! A program started in a box, watched for its end. macOS has no pidfd, so a [`BoxProc`] waits on its
//! child through a kqueue `EVFILT_PROC`/`NOTE_EXIT` (the fact, never a wall-clock guess, CLAUDE.md §4) and
//! reaps it with `waitpid`. The node is the child's parent (posix_spawn makes a direct child), so its pid
//! can't be confused with another process's until this reaps it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Proc, Refusal, Signal};
use rustix::process::{Pid, WaitId, WaitIdOptions};

use super::helper_client::Helper;
use super::process::Members;
use super::sys::{Event, Kqueue};

pub(super) fn process_failure(
    operation: shards_api::ProcessOperation,
    error: std::io::Error,
) -> shards_api::ProcessFailure {
    match error.raw_os_error() {
        Some(code) => shards_api::ProcessFailure::Os { operation, code },
        None => shards_api::ProcessFailure::UnrepresentedIo {
            operation,
            kind: error.kind(),
        },
    }
}

/// A program the node spawned into a box, by pid.
pub struct BoxProc {
    pid: i32,
    kq: Kqueue,
    /// The process box this program runs in (none for a box-user box, whose helper counts its ends), so reaping a kill at the fatal cap counts it on the box at once
    /// (native plan step 1's `_ext` backstop) — before a caller that waited on this reads the box's usage.
    members: Option<Members>,
    exit: Option<Exit>,
    /// Whether its end is under way: the kernel refused its watch as already exiting (`ESRCH`), or a wait
    /// took its exit event. Its reap then waits for the zombie the same exit path makes.
    exiting: bool,
    /// Whether its end came to be under way since its last reap: the next reap is the one made on it.
    on_exit: bool,
    /// Whether the reap made on its end's coming found nothing, so an end found after it is seen late
    /// ([`super::process::Status::late_ends`]).
    missed: bool,
    events: Vec<Event>,
    reaping: Reaping,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reaping {
    Ordinary,
    Retained,
    Borrowed { started: u64 },
}

pub(super) struct Prepared {
    kq: Kqueue,
    events: Vec<Event>,
}

impl Prepared {
    pub(super) fn new() -> Result<Self, Refusal> {
        let mut events = Vec::new();
        events
            .try_reserve_exact(super::sys::EVENTS)
            .map_err(|_| Refusal::Exhausted {
                what: "native controller events",
                bound: super::sys::EVENTS,
            })?;
        let kq = Kqueue::new()
            .map_err(|error| Refusal::os("preparing the native controller observer", error))?;
        kq.child_completions()
            .map_err(|error| Refusal::os("observing waitable native child completion", error))?;
        Ok(Self { kq, events })
    }

    pub(super) fn bind(
        self,
        pid: i32,
        members: Option<Members>,
        reaping: Reaping,
    ) -> (BoxProc, std::io::Result<()>) {
        let observed = self
            .kq
            .watch_one(pid, 0, !matches!(reaping, Reaping::Borrowed { .. }));
        let exiting = matches!(observed, Ok(true)) && !matches!(reaping, Reaping::Borrowed { .. });
        let process = BoxProc {
            pid,
            kq: self.kq,
            members,
            exit: None,
            exiting,
            on_exit: exiting,
            missed: false,
            events: self.events,
            reaping,
        };
        (process, observed.map(|_| ()))
    }
}

impl BoxProc {
    /// Watches child `pid` for its end, in process box `members` (none for a box-user box). It must be this process's child and not yet
    /// reaped, so its pid is this program's until [`BoxProc`] reaps it.
    pub fn watch(pid: i32, members: Option<Members>) -> Result<Self, Refusal> {
        let kq = Kqueue::new().map_err(|e| Refusal::os("making a program's kqueue", e))?;
        // Already exiting (or reaped by a box delete that beat this): no exit event will come, so its end is
        // read by a reap that waits for it, or is no longer knowable.
        let exiting = kq
            .watch(&[pid], 0)
            .map_err(|e| Refusal::os("watching a program", e))?
            .contains(&pid);
        Ok(Self {
            pid,
            kq,
            members,
            exit: None,
            exiting,
            on_exit: exiting,
            missed: false,
            events: Vec::with_capacity(1),
            reaping: Reaping::Ordinary,
        })
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid
    }

    /// Reaps the program if it has ended, caching its exit; `Ok(None)` while it still runs. With `exiting`
    /// (its exit event was delivered) it waits for the reap: XNU posts `NOTE_EXIT` before it makes the child
    /// a zombie (`bsd/kern/kern_exit.c:2594-2595`, `p_stat = SZOMB` at :2664, the same exit path), and the
    /// event fires once, so a reap that found nothing on it would wait out the caller's whole budget. An
    /// end its node can't collect (reaped elsewhere, e.g. by a box delete) is [`Exit::Unknown`].
    fn reap(&mut self, exiting: bool) -> Result<Option<Exit>, Refusal> {
        if self.reaping != Reaping::Ordinary {
            return self.observe_retained(exiting);
        }
        if self.exit.is_some() {
            return Ok(self.exit);
        }
        // The reap made on its end's coming, if this is it: one that finds nothing leaves the end to a later
        // wakeup, which sees it late.
        let on_exit = std::mem::take(&mut self.on_exit);
        let Some(pid) = Pid::from_raw(self.pid) else {
            return Err(Refusal::Invalid {
                what: "pid",
                why: format!("{} isn't a process id", self.pid),
            });
        };
        let options = if exiting {
            WaitIdOptions::EXITED
        } else {
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG
        };
        // macOS answers WEXITED with a stopped or continued child's report too (measured, tests.rs
        // a_stopped_program_is_running_to_its_waiter_not_ended): not its end, so the report is taken and the
        // child read again. Beside its exit a child holds at most one stop and one continue report (XNU
        // keeps one of each a process, `bsd/kern/kern_exit.c`), so a reap reads past at most two.
        for _ in 0..REPORTS {
            // Peek first (NOWAIT): the zombie's use is readable only until the reap: its lifetime peak, which tells a
            // cap kill from another hand's SIGKILL (State::killed_by_cap), and its exit's own time.
            let zombie =
                match rustix::process::waitid(WaitId::Pid(pid), options | WaitIdOptions::NOWAIT) {
                    Ok(Some(report)) if ended(&report) => {
                        super::sys::rusage(self.pid).ok().flatten()
                    }
                    _ => None,
                };
            let peak = zombie.map(|r| r.peak);
            match rustix::process::waitid(WaitId::Pid(pid), options) {
                Ok(Some(status)) if !ended(&status) => {}
                Ok(Some(status)) => {
                    let exit = exit_of(&status);
                    // A SIGKILL of a box member is the kernel's fatal cap (or memory pressure on it): tell the
                    // box before this reap returns, so the count is there when a caller that waited reads
                    // usage. The box ignores it unless the member was armed and it wasn't the box's own kill.
                    let sigkill = matches!(exit, Exit::Signal(s) if s == Signal::KILL.number());
                    self.exit = Some(exit);
                    if let Some(members) = &self.members {
                        members.ended(self.pid, sigkill, peak)?;
                        // How long after its exit its end was seen: the time since the exit's own time, on its
                        // clock, as a life is reckoned.
                        let after_exit = zombie
                            .map(|r| r.exited)
                            .filter(|at| *at != 0)
                            .and_then(|at| super::process_life(at).ok());
                        members.seen(after_exit, self.missed)?;
                    }
                    return Ok(self.exit);
                }
                Ok(None) => {
                    self.missed = self.missed || on_exit;
                    return Ok(None);
                }
                // No such child: it was reaped elsewhere (a box delete). Its end is no longer knowable here.
                Err(rustix::io::Errno::CHILD) => {
                    self.exit = Some(Exit::Unknown);
                    return Ok(self.exit);
                }
                Err(e) => return Err(Refusal::os("waitid", e)),
            }
        }
        // Stopped or continued again past the reports a child holds at once: it still runs.
        self.missed = self.missed || on_exit;
        Ok(None)
    }

    fn observation_refusal(&self) -> Refusal {
        if let Some(members) = &self.members {
            return members.uncertain("its controller's end couldn't be observed");
        }
        let text = "native controller observation has no canonical member owner";
        let mut why = String::new();
        if why.try_reserve_exact(text.len()).is_err() {
            return Refusal::Exhausted {
                what: "native controller refusal bytes",
                bound: text.len(),
            };
        }
        why.push_str(text);
        Refusal::Invalid {
            what: "native controller ownership",
            why,
        }
    }

    fn observe_retained(&mut self, exiting: bool) -> Result<Option<Exit>, Refusal> {
        self.observe_retained_raw(exiting)
            .map_err(|_| self.observation_refusal())
    }

    fn observe_retained_raw(
        &mut self,
        exiting: bool,
    ) -> Result<Option<Exit>, shards_api::ProcessFailure> {
        use shards_api::{ProcessFailure, ProcessOperation};
        if self.exit.is_some() {
            return Ok(self.exit);
        }
        if let Reaping::Borrowed { started } = self.reaping {
            let actual = super::sys::rusage(self.pid)
                .map_err(|error| process_failure(ProcessOperation::ProcPidRusage, error))?;
            return match actual {
                Some(actual) if actual.started != started => Err(ProcessFailure::DifferentStart {
                    observed: actual.started,
                }),
                Some(actual) if !actual.ended && !exiting => Ok(None),
                // Ended, or no process at its pid at all: its parent reaped it since (launchd reaps a helper whose
                // node ended), so its end is no longer knowable here, as a Retained child reaped elsewhere
                // (ECHILD) reads (AC-T7.4.1).
                _ => {
                    self.exit = Some(Exit::Unknown);
                    Ok(self.exit)
                }
            };
        }
        let pid = Pid::from_raw(self.pid).ok_or(ProcessFailure::PidOutOfRange {
            operation: ProcessOperation::PollExit,
        })?;
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        for _ in 0..REPORTS {
            match rustix::process::waitid(WaitId::Pid(pid), options) {
                Ok(Some(report)) if ended(&report) => {
                    let exit = exit_of(&report);
                    let peak = super::sys::rusage(self.pid)
                        .map_err(|error| process_failure(ProcessOperation::ProcPidRusage, error))?
                        .map(|usage| usage.peak);
                    if let Some(members) = &self.members {
                        members.ended(self.pid, matches!(exit, Exit::Signal(signal) if signal == Signal::KILL.number()), peak)
                            .map_err(|_| ProcessFailure::Poisoned { what: "native controller accounting" })?;
                    }
                    self.exit = Some(exit);
                    return Ok(self.exit);
                }
                Ok(Some(_)) => {
                    // XNU only reaps SZOMB with WEXITED. Consume other reports without WEXITED,
                    // so a concurrent exit cannot release this native incarnation.
                    rustix::process::waitid(
                        WaitId::Pid(pid),
                        WaitIdOptions::STOPPED | WaitIdOptions::CONTINUED | WaitIdOptions::NOHANG,
                    )
                    .map_err(|error| process_failure(ProcessOperation::PollExit, error.into()))?;
                }
                Ok(None) => return Ok(None),
                Err(error) => {
                    return Err(process_failure(ProcessOperation::PollExit, error.into()));
                }
            }
        }
        Ok(None)
    }

    pub(super) fn native_poll(&mut self) -> Result<Option<Exit>, shards_api::ProcessFailure> {
        if !self.exiting && self.exit.is_none() {
            self.kq
                .wait(None, Some(Duration::ZERO), &mut self.events)
                .map_err(|error| {
                    process_failure(shards_api::ProcessOperation::KeventExit, error)
                })?;
            self.exiting = ended_in(&self.events, self.pid);
        }
        self.observe_retained_raw(self.exiting)
    }

    pub(super) fn wait_scope(
        &mut self,
        pids: &[i32],
        patience: Duration,
    ) -> Result<bool, shards_api::ProcessFailure> {
        use shards_api::{ProcessFailure, ProcessOperation};
        for &pid in pids {
            let gone = self
                .kq
                .watch_one(pid, 0, false)
                .map_err(|error| process_failure(ProcessOperation::RegisterExit, error))?;
            match super::sys::rusage(pid)
                .map_err(|error| process_failure(ProcessOperation::ProcPidRusage, error))?
            {
                Some(usage) if usage.ended => return Ok(true),
                Some(_) if !gone => {}
                _ => return Err(ProcessFailure::InvalidStartRecord),
            }
        }
        let timeout = if patience == Duration::MAX {
            None
        } else {
            Some(patience)
        };
        self.kq
            .wait(None, timeout, &mut self.events)
            .map_err(|error| process_failure(ProcessOperation::KeventExit, error))?;
        self.exiting |= ended_in(&self.events, self.pid);
        if self.events.is_empty() && patience == Duration::MAX {
            return Err(ProcessFailure::UnexpectedWait { returned: 0 });
        }
        Ok(!self.events.is_empty())
    }

    pub(super) fn finalize_retained(&mut self) -> Result<(), shards_api::ProcessFailure> {
        use shards_api::{ProcessFailure, ProcessOperation};
        if self.exit.is_none() {
            return Err(ProcessFailure::InvalidStartRecord);
        }
        if self.reaping != Reaping::Retained {
            return Ok(());
        }
        let pid = Pid::from_raw(self.pid).ok_or(ProcessFailure::PidOutOfRange {
            operation: ProcessOperation::PollExit,
        })?;
        let report = rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        )
        .map_err(|error| process_failure(ProcessOperation::PollExit, error.into()))?;
        if !report.as_ref().is_some_and(ended) {
            return Err(ProcessFailure::InvalidStartRecord);
        }
        self.reaping = Reaping::Ordinary;
        Ok(())
    }
}

impl Proc for BoxProc {
    fn pid(&self) -> u32 {
        u32::try_from(self.pid).unwrap_or(0)
    }

    /// Takes the exit event its watch holds, if any, then reaps: at once either way, since an exit event
    /// means the zombie is being made on the same exit path.
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        if !self.exiting && self.exit.is_none() {
            self.kq
                .wait(None, Some(Duration::ZERO), &mut self.events)
                .map_err(|e| Refusal::os("waiting on a program", e))?;
            self.exiting = ended_in(&self.events, self.pid);
            self.on_exit = self.exiting;
        }
        self.reap(self.exiting)
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(exit) = self.reap(self.exiting)? {
                return Ok(Some(exit));
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => timeout,
            };
            if left.is_zero() {
                return Ok(None);
            }
            self.kq
                .wait(None, Some(left), &mut self.events)
                .map_err(|e| Refusal::os("waiting on a program", e))?;
            if !self.exiting && ended_in(&self.events, self.pid) {
                self.exiting = true;
                self.on_exit = true;
            }
        }
    }

    /// KILL by pid: the program is this node's child and not yet reaped (a reaped one has its exit cached),
    /// so its pid is still its own, a zombie's at worst. Its box's watcher is told first, so the kill isn't
    /// counted as its bound's (native plan step 8).
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.reap(self.exiting)?.is_some() {
            return Ok(());
        }
        let Some(pid) = Pid::from_raw(self.pid) else {
            return Err(Refusal::Invalid {
                what: "pid",
                why: format!("{} isn't a process id", self.pid),
            });
        };
        if let Some(members) = &self.members {
            members.killing(self.pid)?;
        }
        match rustix::process::kill_process(pid, rustix::process::Signal::KILL) {
            // ESRCH: reaped elsewhere since (a box delete); it has ended.
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(Refusal::os("kill", e)),
        }
    }

    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// Whether `events` carry `pid`'s end: its exit, or a fork coalesced with it.
fn ended_in(events: &[Event], pid: i32) -> bool {
    events.iter().any(|e| match e {
        Event::Exit { pid: p, .. }
        | Event::Fork {
            pid: p,
            exited: true,
            ..
        } => *p == pid,
        _ => false,
    })
}

/// A program the box helper started in a box-user box (native plan step 2). The helper is its parent, so
/// only the helper can reap it: `waitid` answers `ECHILD` to any other caller
/// (`bsd/kern/kern_exit.c:3194-3361`), which a [`BoxProc`] would read as an end at once. Any process may
/// watch another's `NOTE_EXIT` (`filt_procattach` checks only `NOTE_EXITSTATUS`,
/// `bsd/kern/kern_event.c:1116-1133`), so the node waits on the fact on a kqueue of its own, then asks the
/// helper for the end it reaped. A watch attached once the program is past its `NOTE_EXIT` is refused
/// (`ESRCH`): the exit path hides it from `proc_find` (`P_REF_DEAD`, `kern_exit.c:2127-2129`, in
/// `proc_prepareexit`) before `proc_exit` posts the event (:2561), so no end is missed between the two.
pub struct UserProc {
    pid: i32,
    box_id: BoxId,
    helper: Arc<Helper>,
    kq: Kqueue,
    exit: Option<Exit>,
    /// Whether its end is under way: its exit event was taken, or its watch was refused as exiting.
    exiting: bool,
    events: Vec<Event>,
}

impl UserProc {
    /// Watches `pid`, which the helper just started in `box_id`, for its end.
    pub fn watch(pid: i32, box_id: BoxId, helper: Arc<Helper>) -> Result<Self, Refusal> {
        let kq = Kqueue::new().map_err(|e| Refusal::os("making a program's kqueue", e))?;
        // NOTE_EXIT alone: the node isn't its parent, and XNU refuses another's NOTE_EXITSTATUS.
        let exiting = kq
            .watch_end(pid)
            .map_err(|e| Refusal::os("watching a program", e))?;
        Ok(Self {
            pid,
            box_id,
            helper,
            kq,
            exit: None,
            exiting,
            events: Vec::with_capacity(1),
        })
    }

    /// Its end from the helper once it is under way; `None` before.
    fn ask(&mut self) -> Result<Option<Exit>, Refusal> {
        if self.exit.is_some() || !self.exiting {
            return Ok(self.exit);
        }
        let exit = match self.helper.wait(&self.box_id, self.pid) {
            Ok(Some(exit)) => exit,
            Ok(None) => {
                return Err(Refusal::Broken(format!(
                    "the helper holds program {} as running after its exit event",
                    self.pid
                )));
            }
            // Its box and the ends it kept were dropped (a delete long past): no one holds its end now.
            Err(Refusal::NoSuchBox(_)) => Exit::Unknown,
            Err(e) => return Err(e),
        };
        self.exit = Some(exit);
        Ok(self.exit)
    }

    /// Takes the events waiting on its kqueue for at most `timeout`, noting its end among them.
    fn take(&mut self, timeout: Duration) -> Result<(), Refusal> {
        self.kq
            .wait(None, Some(timeout), &mut self.events)
            .map_err(|e| Refusal::os("waiting on a program", e))?;
        self.exiting = self.exiting || ended_in(&self.events, self.pid);
        Ok(())
    }
}

impl Proc for UserProc {
    fn pid(&self) -> u32 {
        u32::try_from(self.pid).unwrap_or(0)
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        if !self.exiting {
            self.take(Duration::ZERO)?;
        }
        self.ask()
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(exit) = self.ask()? {
                return Ok(Some(exit));
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => timeout,
            };
            if left.is_zero() {
                return Ok(None);
            }
            self.take(left)?;
        }
    }

    /// KILL by the helper's hand (HEX-RT-16): the program runs as its box's pool user, which this node may
    /// not signal, and the helper, which started it, ends it alone, the box's other programs running on. An
    /// end already read is done, and so is a box the helper no longer holds (deleted, its programs ended
    /// with it), as a program's own kill after its box's delete is on every class.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.exit.is_some() {
            return Ok(());
        }
        match self.helper.kill(&self.box_id, self.pid) {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// A reap reads past at most this many reports before the child's own: its exit, and the one stop and one
/// continue report a process holds at once (`bsd/kern/kern_exit.c`).
const REPORTS: usize = 3;

/// Whether wait report `status` is the child's end (it exited, or a signal ended it), not a stop or a
/// continue.
fn ended(status: &rustix::process::WaitIdStatus) -> bool {
    status.exited() || status.killed() || status.dumped()
}

/// How a program ended, from its wait status, with any terminating signal carried as Linux's number so a
/// caller reads every OS's boxes alike (CLAUDE.md §8).
fn exit_of(status: &rustix::process::WaitIdStatus) -> Exit {
    if let Some(sig) = status.terminating_signal() {
        Exit::Signal(hex_core::darwin::signal_from_macos(sig).unwrap_or(sig))
    } else if let Some(code) = status.exit_status() {
        Exit::Code(code)
    } else {
        Exit::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1 (the original of macos_pool_restart's release refused "observing exact helper: process stat has
    /// no valid starttime"): a Borrowed observer, as a restarted node holds of the helper it isn't the parent
    /// of, polls a process whose parent reaped it after the observer was bound (launchd reaps the helper as
    /// the node polls it). Do: bind one to a live child, then end and reap the child here, and poll. Expect its
    /// end read, Unknown (its reaper took its status), never an invalid start record: no process at its pid is
    /// the end the poll waits for.
    #[test]
    fn a_borrowed_observer_reads_a_process_reaped_since_as_ended() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("100000")
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let started = super::super::sys::rusage(pid).unwrap().unwrap().started;
        let (mut observer, registered) =
            Prepared::new()
                .unwrap()
                .bind(pid, None, Reaping::Borrowed { started });
        registered.unwrap();
        assert_eq!(
            observer.native_poll(),
            Ok(None),
            "a live process read as ended"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            super::super::sys::rusage(pid).unwrap().is_none(),
            "the reaped child's pid is another process's"
        );
        assert_eq!(observer.native_poll(), Ok(Some(Exit::Unknown)));
    }
}
