//! Pseudo-terminal sessions: a real PTY (so `isatty()` is true, resize sends SIGWINCH, and
//! full-screen TUIs such as k9s, htop and vim work) bridged to async channels. Shared by the
//! server's "local" terminal and every agent.

use std::io::{Read as _, Write as _};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum PtyOut {
    Data(Vec<u8>),
    Exit(Option<i32>),
}

pub struct PtySession {
    input: std::sync::mpsc::Sender<Vec<u8>>,
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

/// Start `command` (or an interactive login shell) on a new PTY.
pub fn spawn(
    shell: Option<&str>,
    command: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<(PtySession, mpsc::Receiver<PtyOut>)> {
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
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let killer = child.clone_killer();
    let mut reader = pty.master.try_clone_reader()?;
    let mut writer = pty.master.take_writer()?;

    let (tx, rx) = mpsc::channel::<PtyOut>(256);
    // Reader thread: PTY → channel. Ends at EOF (child exited, or master closed).
    let out = tx.clone();
    std::thread::Builder::new().name("kiln-pty-read".into()).spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default();
                    if out.blocking_send(PtyOut::Data(chunk)).is_err() {
                        break;
                    }
                }
            }
        }
    })?;
    // Waiter thread: reports the exit status after output drains.
    std::thread::Builder::new().name("kiln-pty-wait".into()).spawn(move || {
        let code = child.wait().ok().and_then(|s| i32::try_from(s.exit_code()).ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = tx.blocking_send(PtyOut::Exit(code));
    })?;
    // Writer thread: channel → PTY. Keeps blocking writes off the async runtime.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new().name("kiln-pty-write".into()).spawn(move || {
        while let Ok(data) = in_rx.recv() {
            if writer.write_all(&data).and_then(|()| writer.flush()).is_err() {
                break;
            }
        }
    })?;
    crate::metrics::global().terminal_sessions.add(1);
    Ok((PtySession { input: in_tx, master: pty.master, killer }, rx))
}

impl PtySession {
    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.send(data);
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

    #[tokio::test]
    async fn pty_is_a_tty_and_resizes() -> anyhow::Result<()> {
        let (s, mut rx) = spawn(Some("/bin/sh"), Some("stty size; test -t 1 && echo TTY"), 100, 30)?;
        let mut out = Vec::new();
        let code = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await? {
                Some(PtyOut::Data(d)) => out.extend(d),
                Some(PtyOut::Exit(c)) => break c,
                None => break None,
            }
        };
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("30 100"), "{text}");
        assert!(text.contains("TTY"), "{text}");
        assert_eq!(code, Some(0));
        s.resize(80, 24);
        Ok(())
    }
}
