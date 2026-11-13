//! LB03: cancellation owns Child handles, never guessed PIDs. A configured peer has one weak slot;
//! the connection keeps the strong handle. Kill/reap never holds the registry mutex, so pipe I/O
//! and connection Drop can finish while service shutdown or an explicit peer interrupt cancels it.

#[cfg(target_os = "macos")]
use hex_runtime::macos::command::Child;
use std::collections::BTreeMap;
use std::io;
#[cfg(not(target_os = "macos"))]
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

pub type OwnedChild = Arc<Mutex<Child>>;

#[derive(Default)]
struct State {
    stopped: bool,
    children: BTreeMap<u32, Weak<Mutex<Child>>>,
    first_error: Option<io::Error>,
}

pub struct Stopping {
    state: Mutex<State>,
    wake: Condvar,
}

impl Default for Stopping {
    fn default() -> Self {
        Self::new(std::iter::empty())
    }
}

fn poisoned(what: &str) -> io::Error {
    io::Error::other(format!("{what} lock is poisoned"))
}

/// Kill only this exact owned child and reap it. A cached status is already reaped. Recovering a
/// poisoned lock permits cleanup but still reports that failure; a failed kill is not followed by
/// an unbounded wait on a process that has not been ended.
fn reap(child: &OwnedChild) -> io::Result<()> {
    let (mut child, mut failure) = match child.lock() {
        Ok(child) => (child, None),
        Err(e) => (e.into_inner(), Some(poisoned("SSH child"))),
    };
    match child.try_wait() {
        Ok(Some(_)) => return failure.map_or(Ok(()), Err),
        Ok(None) => {}
        // If another reaper already consumed this child, its numeric PID could be reused.
        // An unsuccessful ownership check must never be followed by a signal to that PID.
        Err(e) => return Err(failure.unwrap_or(e)),
    }
    if let Err(e) = child.kill() {
        // Ending between try_wait and kill is ordinary; try_wait also reaps that child.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return failure.map_or(Ok(()), Err);
        }
        return Err(failure.unwrap_or(e));
    }
    if let Err(e) = child.wait() {
        failure.get_or_insert(e);
    }
    failure.map_or(Ok(()), Err)
}

impl Stopping {
    /// The registry's bound is exactly the configured peer set, one weak child each. Unknown
    /// peers and overlapping live connections are refused instead of growing a child history.
    pub fn new(peers: impl IntoIterator<Item = u32>) -> Self {
        Self {
            state: Mutex::new(State {
                children: peers.into_iter().map(|node| (node, Weak::new())).collect(),
                ..State::default()
            }),
            wake: Condvar::new(),
        }
    }

    pub fn register(&self, node: u32, child: &OwnedChild) -> io::Result<()> {
        let result = match self.state.lock() {
            Err(_) => Err(poisoned("SSH registry")),
            Ok(state) if state.stopped => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "scheduler stopped",
            )),
            Ok(mut state) => match state.children.get_mut(&node) {
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unconfigured SSH peer",
                )),
                Some(slot) => {
                    if slot.upgrade().is_some_and(|old| !Arc::ptr_eq(&old, child)) {
                        Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "peer already owns an SSH child",
                        ))
                    } else {
                        *slot = Arc::downgrade(child);
                        Ok(())
                    }
                }
            },
        };
        if let Err(error) = result {
            // A child spawned concurrently with stop is owned too: end it before any hello can block.
            return match reap(child) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::new(
                    error.kind(),
                    format!("{error}; closing rejected SSH child: {cleanup}"),
                )),
            };
        }
        Ok(())
    }

    pub fn is_stopped(&self) -> bool {
        // A poisoned registry cannot authorize more I/O; stop() recovers it to end every child and reports it.
        self.state.lock().map_or(true, |state| state.stopped)
    }

    /// Wait at most the caller's measured retry duration, returning true on service stop. An
    /// interrupt or spurious wake returns early; the worker rechecks its durable control queue.
    pub fn wait_retry(&self, duration: Duration) -> io::Result<bool> {
        let state = self.state.lock().map_err(|_| poisoned("SSH registry"))?;
        if state.stopped {
            return Ok(true);
        }
        let (state, _) = self
            .wake
            .wait_timeout(state, duration)
            .map_err(|_| poisoned("SSH retry"))?;
        Ok(state.stopped)
    }

    pub fn interrupt_peer(&self, node: u32) -> io::Result<()> {
        let child = {
            let mut state = self.state.lock().map_err(|_| poisoned("SSH registry"))?;
            let slot = state.children.get_mut(&node).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unconfigured SSH peer")
            })?;
            std::mem::take(slot).upgrade()
        };
        self.wake.notify_all();
        child.as_ref().map_or(Ok(()), reap)
    }

    /// Drop cannot return an I/O failure. Retain the first cleanup failure for the service's
    /// explicit stop result, while always attempting to end only this connection's own child.
    pub(crate) fn finish_child(&self, child: &OwnedChild) {
        if let Err(error) = reap(child) {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(e) => e.into_inner(),
            };
            state.first_error.get_or_insert(error);
        }
    }

    pub fn stop(&self) -> io::Result<()> {
        let (children, mut failure) = {
            let (mut state, poison) = match self.state.lock() {
                Ok(state) => (state, None),
                Err(e) => (e.into_inner(), Some(poisoned("SSH registry"))),
            };
            state.stopped = true;
            let failure = poison.or_else(|| state.first_error.take());
            let children: Vec<OwnedChild> = state
                .children
                .values_mut()
                .filter_map(|slot| std::mem::take(slot).upgrade())
                .collect();
            (children, failure)
        };
        self.wake.notify_all();
        for child in children {
            if let Err(e) = reap(&child) {
                failure.get_or_insert(e);
            }
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(e) => e.into_inner(),
        };
        if let Some(e) = state.first_error.take() {
            failure.get_or_insert(e);
        }
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use hex_runtime::macos::command::{ChildStdin, ChildStdout, Command, Stdio};
    use std::io::{BufRead, BufReader, Read, Write};
    #[cfg(not(target_os = "macos"))]
    use std::process::{ChildStdin, ChildStdout, Command, Stdio};

    #[test]
    fn child_waits_for_owned_pipe() {
        if std::env::var_os("HEX_SCHEDULER_STOPPING_CHILD").is_none() {
            return;
        }
        std::io::stdout()
            .write_all(b"HEX_STOPPING_READY\n")
            .unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_exact(&mut [0]).unwrap();
    }

    pub(in crate::scheduler) fn waiting_child() -> (OwnedChild, ChildStdin, BufReader<ChildStdout>)
    {
        let full = concat!(module_path!(), "::child_waits_for_owned_pipe");
        let test = full.split_once("::").map_or(full, |(_, test)| test);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("HEX_SCHEDULER_STOPPING_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
            if line.ends_with("HEX_STOPPING_READY\n") {
                break;
            }
        }
        (Arc::new(Mutex::new(child)), input, output)
    }

    /// LB03: stop during a blocked peer read kills/reaps the registered child, unblocks the pipe,
    /// and leaves another child this test owns but never registered running until its own cleanup.
    #[test]
    fn stop_unblocks_only_the_owned_registered_child() {
        let stopping = Stopping::new([7]);
        let (owned, _input, mut output) = waiting_child();
        let (other, _other_input, _other_output) = waiting_child();
        stopping.register(7, &owned).unwrap();
        let blocked = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            output.read_to_end(&mut bytes).unwrap();
        });
        stopping.stop().unwrap();
        blocked.join().unwrap();
        assert!(owned.lock().unwrap().try_wait().unwrap().is_some());
        assert!(other.lock().unwrap().try_wait().unwrap().is_none());
        reap(&other).unwrap();
        assert!(stopping.is_stopped());
        assert!(stopping.wait_retry(Duration::ZERO).unwrap());
    }

    /// LB03: interrupt affects only the current connection. An old connection's later Drop must
    /// not end its replacement; stop also catches a spawn that raced after cancellation.
    #[test]
    fn peer_interrupt_preserves_replacement_child_ownership() {
        let stopping = Stopping::new([7]);
        let (old, _old_input, _old_output) = waiting_child();
        stopping.register(7, &old).unwrap();
        stopping.interrupt_peer(7).unwrap();
        assert!(!stopping.is_stopped());
        let (next, _next_input, _next_output) = waiting_child();
        stopping.register(7, &next).unwrap();
        stopping.finish_child(&old);
        assert!(next.lock().unwrap().try_wait().unwrap().is_none());
        stopping.stop().unwrap();
        assert!(next.lock().unwrap().try_wait().unwrap().is_some());
        let (raced, _raced_input, _raced_output) = waiting_child();
        assert!(stopping.register(7, &raced).is_err());
        assert!(raced.lock().unwrap().try_wait().unwrap().is_some());
    }
}
