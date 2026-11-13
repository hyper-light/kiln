//! A run's box removed once its program ends (decision 7 of docs/design/runtime-architecture.md), on the
//! runtimes whose boxes nothing else watches: macOS's and Windows' `process` classes. (Linux's node agent's
//! supervisor removes a run's box there, watching the program's pidfd.) A thread of the run's own takes its
//! program, waits for its end, removes the box, and only then gives the caller the end, so a caller that saw
//! the end finds the box gone. It does so whether or not the caller still holds the run: a caller that lets
//! the run go leaves no box behind.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Given, Proc, Refusal};

/// What ends a run's program: its box's KILL, which a run's box, made for that program alone, holds.
pub(crate) type Kill = Box<dyn FnMut() -> Result<(), Refusal> + Send>;

/// What removes a run's box once its program has ended: the runtime's own removal, the one its delete takes.
pub(crate) type Remove = Box<dyn FnOnce() -> Result<(), Refusal> + Send>;

/// The run's end as its thread leaves it: the program's exit, or why the end or the removal failed.
struct Ended {
    end: Mutex<Option<Result<Exit, Refusal>>>,
    came: Condvar,
}

fn poisoned<T>(_: PoisonError<T>) -> Refusal {
    Refusal::Broken("a run's end lock was poisoned".to_owned())
}

/// A run's program as its caller holds it ([`hex_api::Runtime::run`]): its end comes from the run's thread.
pub(crate) struct Run {
    pid: u32,
    ended: Arc<Ended>,
    given: Option<Given>,
    kill: Kill,
}

impl Run {
    /// Hands `program`, started in run box `id`, to a thread of its own, which waits for its end, calls
    /// `remove`, then gives the end to the caller. The caller's ends of the streams the run asked for are
    /// taken here, before the program goes to the thread.
    pub(crate) fn start(
        mut program: Box<dyn Proc>,
        id: &BoxId,
        kill: Kill,
        remove: Remove,
    ) -> Result<Self, Refusal> {
        let given = program.streams()?;
        let pid = program.pid();
        let ended = Arc::new(Ended {
            end: Mutex::new(None),
            came: Condvar::new(),
        });
        let theirs = Arc::clone(&ended);
        let named = id.clone();
        std::thread::Builder::new()
            .name(format!("hex-run-{id}"))
            .spawn(move || {
                // Waits as long as the program runs: each wait returns at its end, or with none yet.
                let exit = loop {
                    match program.wait_for(Duration::MAX) {
                        Ok(Some(exit)) => break Ok(exit),
                        Ok(None) => {}
                        Err(why) => break Err(why),
                    }
                };
                drop(program);
                // The box goes before the end is given, so a caller that saw the end finds it gone; a
                // removal that fails is the run's to report, never dropped (CLAUDE.md §3).
                let end = match (exit, remove()) {
                    (Ok(exit), Ok(())) => Ok(exit),
                    (Err(why), _) => Err(why),
                    (Ok(exit), Err(left)) => Err(Refusal::Broken(format!(
                        "run box {named}'s program ended ({exit:?}) but its box wasn't removed: {left}"
                    ))),
                };
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
            end = self.ended.came.wait_timeout(end, left).map_err(poisoned)?.0;
        }
    }

    /// Ends the run's program through its box's KILL: the box holds that program alone. A program that ended
    /// already is `Ok`, its box perhaps gone.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        match (self.kill)() {
            Ok(()) | Err(Refusal::NoSuchBox(_)) => Ok(()),
            Err(why) => Err(why),
        }
    }

    fn streams(&mut self) -> Result<Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }
}
