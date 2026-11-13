//! A program started in a box, watched for its end. The node owns the member's process handle (from
//! `CreateProcessW`), so it waits on it directly (`WaitForSingleObject`, the fact, never a wall-clock
//! guess, CLAUDE.md §4) and reads its exit code. A member Hex ended by signalling its box's job reads
//! as that signal ([`hex_core::job::exit`] against the box's recorded signals); any other code reads as
//! itself.

use std::time::{Duration, Instant};
use triomphe::Arc;

use hex_api::{Exit, Proc, Refusal, Signal};

use super::process::State;
use super::sys::{self, Handle};

/// The same native observation serves Proc and the managed VM receipt. Preserve the actual OS
/// failure until its caller chooses the Hex or Shards API representation.
pub(crate) enum ObserveError {
    Unpublished,
    DeadlineOverflow,
    UnexpectedWait(u32),
    Os {
        operation: Operation,
        source: std::io::Error,
    },
    State(Refusal),
}

pub(crate) enum Operation {
    Wait,
    ExitCode,
    StopScope,
    QueryScope,
}

impl ObserveError {
    pub(crate) fn refusal(self) -> Refusal {
        match self {
            Self::Unpublished => super::pending("the native job handle is not published"),
            Self::DeadlineOverflow => {
                super::pending("the finite process wait deadline is not representable")
            }
            Self::UnexpectedWait(_) => {
                super::pending("an infinite native process wait returned without a terminal event")
            }
            Self::Os { operation, source } => Refusal::os(
                match operation {
                    Operation::Wait => "waiting on a program",
                    Operation::ExitCode => "reading a program's exit code",
                    Operation::StopScope => "ending a managed native job",
                    Operation::QueryScope => "reading the native job's scope",
                },
                source,
            ),
            Self::State(why) => why,
        }
    }
}

/// A program the node started in a box, by its process handle. The handle is dropped once the program
/// has ended and its exit is read, so the terminated process is reaped and leaves the box's job (a
/// terminated process a handle is still open on stays in the job's live process list, which the box
/// reads for whether a member runs).
pub struct BoxProc {
    process: Option<Handle>,
    /// The box it runs in, so its exit reads against the signals Hex ended the box's job for.
    state: Arc<State>,
    pid: u32,
    exit: Option<Exit>,
    /// Terminated by its own `kill`, with KILL's code: that code then reads as KILL whatever the box's job
    /// was sent.
    killed: bool,
}

impl BoxProc {
    pub fn new(process: Handle, pid: u32, state: Arc<State>) -> Self {
        Self {
            process: Some(process),
            state,
            pid,
            exit: None,
            killed: false,
        }
    }

    /// Reads the program's exit if it has ended within `timeout_ms` (`u32::MAX` waits for it), caching
    /// it and closing its handle; `Ok(None)` while it still runs.
    pub(crate) fn poll_raw(&mut self, timeout_ms: u32) -> Result<Option<Exit>, ObserveError> {
        if let Some(exit) = self.exit {
            return Ok(Some(exit));
        }
        let Some(process) = self.process.as_ref() else {
            return Ok(self.exit);
        };
        if !sys::wait(process, timeout_ms).map_err(|source| ObserveError::Os {
            operation: Operation::Wait,
            source,
        })? {
            return Ok(None);
        }
        let code = sys::exit_code(process).map_err(|source| ObserveError::Os {
            operation: Operation::ExitCode,
            source,
        })?;
        let mut sent = self.state.sent().map_err(ObserveError::State)?;
        if self.killed {
            sent.add(Signal::KILL);
        }
        let exit = hex_core::job::exit(code, sent);
        self.exit = Some(exit);
        // Close the handle: the ended process is reaped and leaves the box's job.
        self.process = None;
        Ok(Some(exit))
    }

    fn poll(&mut self, timeout_ms: u32) -> Result<Option<Exit>, Refusal> {
        self.poll_raw(timeout_ms).map_err(ObserveError::refusal)
    }

    pub(crate) fn wait_raw(&mut self, timeout: Duration) -> Result<Option<Exit>, ObserveError> {
        // One kernel event wait covers the caller's budget. A spurious early wake consumes only
        // the remaining time; the same receipt caches the one terminal observation.
        let deadline = Instant::now().checked_add(timeout);
        if timeout != Duration::MAX && deadline.is_none() {
            return Err(ObserveError::DeadlineOverflow);
        }
        if self.poll_raw(millis(timeout))?.is_some() {
            return Ok(self.exit);
        }
        if timeout == Duration::MAX {
            return Err(ObserveError::UnexpectedWait(
                windows_sys::Win32::Foundation::WAIT_TIMEOUT,
            ));
        }
        loop {
            let left = deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return Ok(None);
            }
            if self.poll_raw(millis(left))?.is_some() {
                return Ok(self.exit);
            }
        }
    }
}

/// Duration::MAX requests an event-only wait (Win32 INFINITE). Every finite caller budget remains
/// finite, capped below that sentinel. WaitForSingleObject's documented DWORD milliseconds define
/// both constants; a finite kernel timeout alone is never scope release evidence.
pub(super) fn millis(d: Duration) -> u32 {
    if d == Duration::MAX {
        return u32::MAX;
    }
    u32::try_from(d.as_millis())
        .unwrap_or(0xFFFF_FFFE)
        .min(0xFFFF_FFFE)
}

impl Proc for BoxProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.poll(0)
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        self.wait_raw(timeout).map_err(ObserveError::refusal)
    }

    /// Terminates this process alone, with the code Hex ends a job with for KILL, so its end reads as
    /// KILL's.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.poll(0)?.is_some() {
            return Ok(());
        }
        let Some(process) = self.process.as_ref() else {
            return Ok(());
        };
        self.killed = true;
        match sys::terminate_process(process, hex_core::job::terminated_code(Signal::KILL)) {
            Ok(()) => Ok(()),
            // Refused: it ended between the poll and the call, which its exit then says.
            Err(_) if self.poll(0)?.is_some() => Ok(()),
            Err(e) => Err(Refusal::os("TerminateProcess", e)),
        }
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}
