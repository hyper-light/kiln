//! A harness its boundary holds, as the `Duplex` an agent session drives it through (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 17): the `Sandbox` seam's run of a microVM's member, no
//! process of this machine's. The session's adapter speaks to its pipes as it does to a process's
//! on the machine: what it writes goes to the run's stdin in order, the end of its stdin is the
//! run's, what the run says fills its stdout and stderr, and its end is the run's exit.
//!
//! Stopping it is its boundary's ([`Held::terminate`]): a signal to end where the boundary delivers
//! one, else the end of its input (every harness kiln drives ends at it: a switch relies on that,
//! `harness::session::drive`), then its kill (a microVM's stop) once its grace has passed without
//! its end. A run whose session let go of its output is killed: nobody reads what it does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::channel::oneshot;
use kiln_seams::sandbox::{Exit, Output, Refusal, Run, Signal};

/// Most of a run's stdout chunks waiting to be read, and of its stderr's: a host process's pipes'
/// (`os::proc`'s readers' channels), whose bounds the session's loop was measured against.
const STDOUT_PLACES: usize = 256;
const STDERR_PLACES: usize = 64;

/// A boundary's run, and whether its end has come.
pub struct Held {
    run: Arc<dyn Run>,
    ended: AtomicBool,
    changed: crate::rt::Changed,
}

/// The pipes and end of a held run, for a `Duplex`.
pub struct Parts {
    pub stdin: crate::memory::queue::Sender<Vec<u8>>,
    pub stdout: crate::memory::queue::Receiver<Vec<u8>>,
    pub stderr: crate::memory::queue::Receiver<Vec<u8>>,
    pub exit: oneshot::Receiver<Option<i32>>,
    pub held: Arc<Held>,
}

/// `run`'s stdio and end as a `Duplex`'s: one task writes what's sent to its stdin, in order, and
/// closes its stdin once nothing more can be sent; another hands on what it says and its end.
pub fn pump(run: Box<dyn Run>) -> Parts {
    let held = Arc::new(Held { run: Arc::from(run), ended: AtomicBool::new(false), changed: crate::rt::Changed::new() });
    let (in_tx, in_rx) = crate::memory::queue::unbounded::<Vec<u8>>(&crate::memory::owners::CHANNELS);
    let (out_tx, stdout) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, STDOUT_PLACES);
    let (err_tx, stderr) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, STDERR_PLACES);
    let (exit_tx, exit) = oneshot::channel();
    let writer = Arc::clone(&held);
    drop(crate::rt::spawn(async move {
        while let Ok(bytes) = in_rx.recv().await {
            if writer.run.stdin(bytes).await.is_err() {
                break;
            }
        }
        writer.run.close_stdin();
    }));
    let reader = Arc::clone(&held);
    drop(crate::rt::spawn(async move {
        let (mut out, mut err, mut exit_tx) = (Some(out_tx), Some(err_tx), Some(exit_tx));
        while let Some(said) = reader.run.next().await {
            match said {
                Output::Stdout(bytes) => {
                    let gone = match &out {
                        Some(tx) => tx.send(bytes).await.is_err(),
                        None => false,
                    };
                    if gone {
                        // Its session let go of it: nobody reads what it does any more.
                        out = None;
                        reader.run.kill();
                    }
                }
                Output::Stderr(bytes) => {
                    if let Some(tx) = &err
                        && tx.send(bytes).await.is_err()
                    {
                        err = None;
                    }
                }
                Output::Exited(how) => {
                    // Its pipes close before its end is said, as a process's do.
                    (out, err) = (None, None);
                    if let Some(tx) = exit_tx.take() {
                        let _ = tx.send(code(how));
                    }
                }
            }
        }
        drop((out, err, exit_tx));
        reader.ended.store(true, Ordering::Release);
        reader.changed.notify();
    }));
    Parts { stdin: in_tx, stdout, stderr, exit, held }
}

/// A run's end as a process's exit code: its code, or none where its boundary ended it (killed, or
/// over a limit), as a process a signal ended has none.
fn code(how: Exit) -> Option<i32> {
    match how {
        Exit::Code(c) => Some(c),
        Exit::Killed | Exit::OverLimit(_) => None,
    }
}

impl Held {
    /// Ends it in its own time, then by force: a signal to end where its boundary delivers one, else
    /// the end of its input; its kill once `grace` passes without its end.
    pub fn terminate(self: &Arc<Self>, grace: Duration) {
        match self.run.signal(Signal::Terminate) {
            Ok(()) | Err(Refusal::Ended) => {}
            // No signal reaches it there (a microVM's member, until its control lane lands: steps
            // 11 and 12): the end of its input is its own way out.
            Err(_) => self.run.close_stdin(),
        }
        let me = Arc::clone(self);
        drop(crate::rt::spawn(async move {
            let ended = me.changed.listen();
            if me.ended.load(Ordering::Acquire) {
                return;
            }
            if crate::rt::timeout(grace, ended).await.is_err() {
                me.run.kill();
            }
        }));
    }

    /// Ends it at once (a microVM's stop).
    pub fn kill(&self) {
        self.run.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::sandbox::{Holds, Launch, Sandbox as _, SimSandbox};

    fn launched(s: &SimSandbox, program: &str) -> anyhow::Result<Parts> {
        let run = crate::rt::block_on(s.launch(Launch::of(program, &[]))).map_err(anyhow::Error::msg)?;
        Ok(pump(run))
    }

    /// Step 17: what a session writes reaches the run in order and comes back on its stdout; the
    /// end of its input is the run's; its stdout closes before its end, which is its code.
    #[test]
    fn s17_a_held_run_takes_its_input_in_order_and_ends_at_its_end() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let p = launched(&SimSandbox::new(), "echo")?;
            for line in ["one\n", "two\n", "three\n"] {
                p.stdin.send(line.as_bytes().to_vec()).await.map_err(|_| anyhow::anyhow!("its stdin closed"))?;
            }
            p.stdin.close();
            let mut got = Vec::new();
            while let Ok(chunk) = crate::rt::timeout(Duration::from_secs(10), p.stdout.recv()).await? {
                got.extend(chunk);
            }
            assert_eq!(got, b"one\ntwo\nthree\n");
            assert_eq!(crate::rt::timeout(Duration::from_secs(10), p.exit).await?.ok().flatten(), Some(0));
            Ok(())
        })
    }

    /// Step 17: terminated where its boundary delivers signals, a run ends by its signal, and no
    /// kill follows; where it delivers none, its input ends, and once its grace passes without its
    /// end it's killed (a microVM's stop).
    #[test]
    fn s17_terminate_signals_where_it_can_else_ends_its_input_then_kills() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let signalled = launched(&SimSandbox::new(), "wait")?;
            signalled.held.terminate(Duration::from_secs(30));
            assert_eq!(crate::rt::timeout(Duration::from_secs(10), signalled.exit).await?.ok().flatten(), Some(Signal::Terminate.ended_code()));
            // An echo ends at its input's end: unsignalled, terminate ends it so, before its grace.
            let unsignalled = SimSandbox::holding(Holds { signals: false, ..Holds::ALL });
            let echo = launched(&unsignalled, "echo")?;
            echo.held.terminate(Duration::from_secs(30));
            assert_eq!(crate::rt::timeout(Duration::from_secs(10), echo.exit).await?.ok().flatten(), Some(0), "its input's end ended it");
            // One that ignores its input's end is killed once its grace is over: no code.
            let deaf = launched(&unsignalled, "wait")?;
            deaf.held.terminate(Duration::from_millis(50));
            assert_eq!(crate::rt::timeout(Duration::from_secs(10), deaf.exit).await?.ok().flatten(), None, "killed past its grace");
            Ok(())
        })
    }

    /// Step 17: a run whose session let go of its output is killed, so no microVM outlives the
    /// session that held it.
    #[test]
    fn s17_a_held_run_let_go_of_is_killed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let s = SimSandbox::new();
            let run = s.launch(Launch::of("echo", &[])).await.map_err(anyhow::Error::msg)?;
            let p = pump(run);
            let held = Arc::clone(&p.held);
            let Parts { stdin, stdout, .. } = p;
            drop(stdout);
            stdin.send(b"said to nobody\n".to_vec()).await.map_err(|_| anyhow::anyhow!("its stdin closed"))?;
            crate::rt::timeout(Duration::from_secs(10), async {
                loop {
                    let next = held.changed.listen();
                    if held.ended.load(Ordering::Acquire) {
                        return;
                    }
                    next.await;
                }
            })
            .await?;
            assert_eq!(held.run.signal(Signal::Terminate), Err(Refusal::Ended), "it was ended");
            Ok(())
        })
    }
}
