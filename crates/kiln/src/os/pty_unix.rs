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
        Ok(Self { fd: Arc::new(fd) })
    }

    /// Pump PTY output (then the exit status) into `out`, and `input` into the PTY.
    pub fn start(self, mut child: Box<dyn Child + Send + Sync>, out: async_channel::Sender<PtyOut>, input: async_channel::Receiver<Vec<u8>>) {
        // PTY → channel. Ends at EOF/EIO (the shell exited, or the master closed).
        {
            let fd = Arc::clone(&self.fd);
            let out = out.clone();
            crate::rt::spawn(async move {
                let mut buf = vec![0u8; 32 * 1024];
                while let Ok(Some(n)) = read_chunk(&fd, &mut buf).await {
                    if out.send(PtyOut::Data(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default())).await.is_err() {
                        break;
                    }
                }
            });
        }
        // The exit status, after the last output has had a moment to drain.
        {
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
        // Channel → PTY.
        {
            let fd = Arc::clone(&self.fd);
            crate::rt::spawn(async move {
                while let Ok(data) = input.recv().await {
                    if write_all(&fd, &data).await.is_err() {
                        break;
                    }
                }
            });
        }
    }
}

/// Close a PTY master (quick on Unix).
pub fn close_master(master: Box<dyn MasterPty + Send>) {
    drop(master);
}
