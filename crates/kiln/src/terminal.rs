//! Pseudo-terminal sessions: a real PTY (so `isatty()` is true, resize sends SIGWINCH, and
//! full-screen TUIs such as k9s, htop and vim work) bridged to async channels. Shared by the
//! server's "local" terminal and every agent.
//!
//! No threads: the PTY master is non-blocking and read and written as hyper-rt readiness fires;
//! the shell's exit is awaited through its pidfd.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

#[derive(Debug)]
pub enum PtyOut {
    Data(Vec<u8>),
    Exit(Option<i32>),
}

pub struct PtySession {
    input: async_channel::Sender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

pub fn default_shell(configured: Option<&str>) -> String {
    if let Some(s) = configured {
        return s.to_owned();
    }
    if let Some(s) = std::env::var("SHELL").ok().filter(|s| std::path::Path::new(s).exists()) {
        return s;
    }
    for s in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        if std::path::Path::new(s).exists() {
            return s.to_owned();
        }
    }
    "sh".to_owned()
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize { rows: rows.clamp(2, 1000), cols: cols.clamp(2, 1000), pixel_width: 0, pixel_height: 0 }
}

/// A raw descriptor number, so `filedescriptor` can duplicate it into an owned handle (the
/// duplicate is ours to close; the original stays owned by portable-pty).
struct RawNumber(RawFd);

impl AsRawFd for RawNumber {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// An owned duplicate of the PTY master, non-blocking.
fn master_fd(master: &dyn MasterPty) -> anyhow::Result<OwnedFd> {
    let raw = master.as_raw_fd().context("the PTY master has no file descriptor")?;
    let dup = filedescriptor::FileDescriptor::dup(&RawNumber(raw)).map_err(|e| anyhow::anyhow!("dup PTY master: {e}"))?;
    let file = dup.as_file().map_err(|e| anyhow::anyhow!("PTY master as file: {e}"))?;
    let fd = OwnedFd::from(file);
    crate::proc::nonblocking(&fd).context("non-blocking PTY master")?;
    Ok(fd)
}

/// Start `command` (or an interactive login shell) on a new PTY.
pub fn spawn(
    shell: Option<&str>,
    command: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<(PtySession, async_channel::Receiver<PtyOut>)> {
    let pty = native_pty_system().openpty(size(cols, rows))?;
    let shell = default_shell(shell);
    let mut cmd = CommandBuilder::new(&shell);
    match command {
        Some(c) if !c.trim().is_empty() => {
            cmd.arg("-c");
            cmd.arg(c);
        }
        _ => {
            cmd.arg("-l");
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("KILN_TERMINAL", "1");
    if std::env::var_os("LANG").is_none() {
        cmd.env("LANG", "C.UTF-8");
    }
    if let Some(home) = std::env::var_os("HOME") {
        cmd.cwd(home);
    }
    let fd = Arc::new(master_fd(pty.master.as_ref())?);
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let killer = child.clone_killer();

    let (tx, rx) = async_channel::bounded::<PtyOut>(256);
    // PTY → channel. Ends at EOF/EIO (the shell exited, or the master closed).
    {
        let fd = Arc::clone(&fd);
        let out = tx.clone();
        crate::rt::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            while let Ok(Some(n)) = crate::proc::read_chunk(&fd, &mut buf).await {
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
                Some(pid) => crate::proc::wait_pid_exit(pid, || child.try_wait())
                    .await
                    .ok()
                    .and_then(|st| i32::try_from(st.exit_code()).ok()),
                None => None,
            };
            crate::rt::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(PtyOut::Exit(code)).await;
        });
    }
    // Channel → PTY.
    let (in_tx, in_rx) = async_channel::unbounded::<Vec<u8>>();
    {
        let fd = Arc::clone(&fd);
        crate::rt::spawn(async move {
            while let Ok(data) = in_rx.recv().await {
                if crate::proc::write_all(&fd, &data).await.is_err() {
                    break;
                }
            }
        });
    }
    crate::metrics::global().terminal_sessions.add(1);
    Ok((PtySession { input: in_tx, master: pty.master, killer }, rx))
}

impl PtySession {
    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.try_send(data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Err(e) = self.master.resize(size(cols, rows)) {
            tracing::debug!(error = %e, "pty resize failed");
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
        crate::metrics::global().terminal_sessions.add(-1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_is_a_tty_and_resizes() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
        let (s, rx) = spawn(Some("/bin/sh"), Some("stty size; test -t 1 && echo TTY"), 100, 30)?;
        let mut out = Vec::new();
        let code = loop {
            match crate::rt::timeout(std::time::Duration::from_secs(10), rx.recv()).await.map_err(|_| anyhow::anyhow!("pty timed out"))? {
                Ok(PtyOut::Data(d)) => out.extend(d),
                Ok(PtyOut::Exit(c)) => break c,
                Err(_) => break None,
            }
        };
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("30 100"), "{text}");
        assert!(text.contains("TTY"), "{text}");
        assert_eq!(code, Some(0));
        s.resize(80, 24);
        Ok(())
    })
    }
}
