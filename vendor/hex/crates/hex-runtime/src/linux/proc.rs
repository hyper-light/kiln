//! A started process watched through its pidfd (Linux 5.3): readable when it ends, and waited on
//! with `waitid(P_PIDFD)`, so its pid can't be confused with a later process's.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use hex_api::{Exit, Proc, Refusal};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions};

/// A child of this process, by pidfd.
#[derive(Debug)]
pub struct PidfdProc {
    pid: u32,
    pidfd: OwnedFd,
    exit: Option<Exit>,
}

impl PidfdProc {
    /// Watches child `pid`. It must be this process's child and not yet waited for, so its pid
    /// can't have been given to another process in between.
    pub fn of_child(pid: u32) -> Result<Self, Refusal> {
        let raw = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or(Refusal::Invalid {
                what: "pid",
                why: format!("{pid} isn't a process id"),
            })?;
        let pidfd = rustix::process::pidfd_open(raw, PidfdFlags::empty())
            .map_err(|e| Refusal::os("pidfd_open", e))?;
        Ok(Self {
            pid,
            pidfd,
            exit: None,
        })
    }

    /// Watches child `pid` through the pidfd its clone made (`CLONE_PIDFD`).
    pub fn of_clone(pid: u32, pidfd: OwnedFd) -> Self {
        Self {
            pid,
            pidfd,
            exit: None,
        }
    }

    /// The pidfd: readable once the process has ended, for an event loop to wait on.
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }

    /// Its pid and pidfd, for a watcher of its own (the node agent's of a run, decision 7).
    pub fn into_parts(self) -> (u32, OwnedFd) {
        (self.pid, self.pidfd)
    }

    fn reap(&mut self) -> Result<Option<Exit>, Refusal> {
        if self.exit.is_some() {
            return Ok(self.exit);
        }
        let status = rustix::process::waitid(
            WaitId::PidFd(self.pidfd.as_fd()),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        )
        .map_err(|e| Refusal::os("waitid", e))?;
        self.exit = status.map(|s| match (s.exit_status(), s.terminating_signal()) {
            (_, Some(sig)) => Exit::Signal(sig),
            (Some(code), None) => Exit::Code(code),
            (None, None) => Exit::Unknown,
        });
        Ok(self.exit)
    }
}

impl Proc for PidfdProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.reap()
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(exit) = self.reap()? {
                return Ok(Some(exit));
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => timeout,
            };
            if left.is_zero() {
                return Ok(None);
            }
            let ts = Timespec {
                tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(left.subsec_nanos()),
            };
            let mut fds = [PollFd::new(&self.pidfd, PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(Refusal::os("poll on a pidfd", e)),
            }
        }
    }

    /// KILL through the pidfd, which names this process alone, so it can't reach one given its pid since.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.exit.is_some() {
            return Ok(());
        }
        match rustix::process::pidfd_send_signal(&self.pidfd, rustix::process::Signal::KILL) {
            // ESRCH: it has ended already (pidfd_send_signal(2)).
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(Refusal::os("pidfd_send_signal", e)),
        }
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}
