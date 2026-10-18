//! PTY plumbing on Unix, without threads: the master is duplicated, made non-blocking and read
//! and written as hyper-rt readiness fires (epoll on Linux, kqueue on macOS; a refused wait
//! degrades to a short poll); the shell's exit is awaited through its pidfd (Linux) or a poll.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use portable_pty::{Child, MasterPty};

use crate::os::proc::{nonblocking, read_chunk, wait_pid_exit, write_all};
use crate::terminal::PtyOut;

/// A raw descriptor number, so `filedescriptor` can duplicate it into an owned handle (the
/// duplicate is ours to close; the original stays owned by portable-pty).
struct RawNumber(RawFd);

impl AsRawFd for RawNumber {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// The I/O side of a PTY session: an owned, non-blocking duplicate of the master.
pub struct PtyIo {
    fd: Arc<OwnedFd>,
    /// Stops reading its output (the session went to another process).
    stop: (async_channel::Sender<()>, async_channel::Receiver<()>),
}

impl PtyIo {
    /// Take hold of the master's I/O. Called before the child is spawned, so a failure here
    /// leaves nothing running.
    pub fn new(master: &dyn MasterPty) -> anyhow::Result<Self> {
        let raw = master.as_raw_fd().context("the PTY master has no file descriptor")?;
        let dup = filedescriptor::FileDescriptor::dup(&RawNumber(raw)).map_err(|e| anyhow::anyhow!("dup PTY master: {e}"))?;
        let file = dup.as_file().map_err(|e| anyhow::anyhow!("PTY master as file: {e}"))?;
        let fd = OwnedFd::from(file);
        nonblocking(&fd).context("non-blocking PTY master")?;
        Ok(Self { fd: Arc::new(fd), stop: async_channel::bounded(1) })
    }

    /// What it takes to hand this session to a re-executed kiln: the master and the shell's pid.
    pub fn handle(&self, pid: u32) -> Handle {
        Handle { fd: Arc::clone(&self.fd), pid, stop: self.stop.0.clone() }
    }

    /// Pump PTY output (then the exit status) into `out`, and `input` into the PTY.
    pub fn start(self, mut child: Box<dyn Child + Send + Sync>, out: async_channel::Sender<PtyOut>, input: async_channel::Receiver<Vec<u8>>) {
        pump(&self.fd, &out, input, self.stop.1.clone());
        // The exit status, after the last output has had a moment to drain.
        let pid = child.process_id();
        crate::rt::spawn(async move {
            let code = match pid {
                Some(pid) => wait_pid_exit(pid, || child.try_wait())
                    .await
                    .ok()
                    .and_then(|st| i32::try_from(st.exit_code()).ok()),
                None => None,
            };
            crate::rt::sleep(Duration::from_millis(50)).await;
            let _ = out.send(PtyOut::Exit(code)).await;
        });
    }
}

/// PTY → `out` (until EOF/EIO: the shell exited or the master closed, or `stop`: the session went
/// to another process, which reads on) and `input` → PTY.
fn pump(fd: &Arc<OwnedFd>, out: &async_channel::Sender<PtyOut>, input: async_channel::Receiver<Vec<u8>>, stop: async_channel::Receiver<()>) {
    {
        let fd = Arc::clone(fd);
        let out = out.clone();
        crate::rt::spawn(async move {
            use futures::FutureExt as _;
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                let read = futures::select_biased! {
                    _ = stop.recv().fuse() => return,
                    r = read_chunk(&fd, &mut buf).fuse() => r,
                };
                let Ok(Some(n)) = read else { return };
                if out.send(PtyOut::Data(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default())).await.is_err() {
                    return;
                }
            }
        });
    }
    {
        let fd = Arc::clone(fd);
        crate::rt::spawn(async move {
            while let Ok(data) = input.recv().await {
                if write_all(&fd, &data).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// A session's master and shell pid: enough to resize it, end it, or hand it across an exec.
pub struct Handle {
    fd: Arc<OwnedFd>,
    pid: u32,
    stop: async_channel::Sender<()>,
}

impl Handle {
    /// Let the session go to another process (a blue/green upgrade): nothing more is read from it
    /// here, and a copy of its master is returned for that process.
    pub fn hand_off(&self) -> std::io::Result<OwnedFd> {
        let _ = self.stop.try_send(());
        rustix::io::fcntl_dupfd_cloexec(&*self.fd, 3).map_err(std::io::Error::from)
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Keep the master open across this process's next exec.
    pub fn keep_across_exec(&self) -> std::io::Result<()> {
        crate::os::keep_across_exec(&self.fd)
    }

    /// Set the window size (the kernel signals the foreground program, as for any terminal).
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = rustix::termios::Winsize { ws_row: rows.clamp(2, 1000), ws_col: cols.clamp(2, 1000), ws_xpixel: 0, ws_ypixel: 0 };
        if let Err(e) = rustix::termios::tcsetwinsize(&*self.fd, ws) {
            tracing::debug!(error = %e, "pty resize failed");
        }
    }

    /// End the session: hang up the shell's process group, as closing a terminal does.
    pub fn hangup(&self) {
        if let Ok(p) = i32::try_from(self.pid) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGHUP);
        }
    }
}

/// Take over a session handed over before this process started, or by another runner process of
/// this machine: its master (already non-blocking) and its shell. After a re-exec the shell is
/// still our child; after a handover to a fresh process (macOS) it is not (its parent was the
/// runner before this one), so its exit is seen without a status. `foreign`: another runner of
/// this machine still reaps it (a blue/green handover), so its exit is only watched. Output and
/// the exit flow as for a spawned session.
pub fn adopt(fd: OwnedFd, pid: u32, foreign: bool, out: async_channel::Sender<PtyOut>, input: async_channel::Receiver<Vec<u8>>) -> Handle {
    // Nothing this process starts (a job, another shell) may inherit this terminal.
    let _ = crate::os::close_on_exec(&fd);
    let fd = Arc::new(fd);
    let stop = async_channel::bounded(1);
    pump(&fd, &out, input, stop.1.clone());
    crate::rt::spawn(async move {
        let code = if foreign {
            crate::os::proc::wait_foreign_exit(pid).await;
            None
        } else {
            wait_pid_exit(pid, || match crate::os::try_reap(pid) {
                Ok(st) => Ok(st.map(|(code, _)| code)),
                Err(e) if e.raw_os_error() == Some(nix::libc::ECHILD) => Ok((!crate::os::pid_alive(pid)).then_some(None)),
                Err(e) => Err(e),
            })
            .await
            .ok()
            .flatten()
        };
        crate::rt::sleep(Duration::from_millis(50)).await;
        let _ = out.send(PtyOut::Exit(code)).await;
    });
    Handle { fd, pid, stop: stop.0 }
}

/// Close a PTY master (quick on Unix).
pub fn close_master(master: Box<dyn MasterPty + Send>) {
    drop(master);
}
