//! Pseudo-terminal sessions: a real PTY (so `isatty()` is true, resize reaches the program, and
//! full-screen TUIs such as k9s, htop and vim work) bridged to async channels. Shared by the
//! server's "local" terminal and every agent. Unix PTYs and Windows ConPTY both come from
//! portable-pty; the I/O plumbing is the platform layer's ([`crate::os::pty`]).

use anyhow::Context as _;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

#[derive(Debug)]
pub enum PtyOut {
    Data(Vec<u8>),
    Exit(Option<i32>),
}

pub struct PtySession {
    input: async_channel::Sender<Vec<u8>>,
    master: Option<Box<dyn MasterPty + Send>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// The shell for terminals: the configured one, else `$SHELL`, bash or sh on Unix; PowerShell 7,
/// Windows PowerShell or `%COMSPEC%` on Windows.
pub fn default_shell(configured: Option<&str>) -> String {
    if let Some(s) = configured {
        return s.to_owned();
    }
    if cfg!(windows) {
        for s in ["pwsh", "powershell"] {
            if let Some(p) = crate::os::which(s) {
                return p.to_string_lossy().into_owned();
            }
        }
        return std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned());
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

/// How to ask `shell` for an interactive session (`command` = None) or to run one command:
/// `-l` / `-c` for Unix shells, `-NoLogo` / `-Command` for PowerShell, `/C` for cmd.
pub fn shell_args(shell: &str, command: Option<&str>) -> Vec<String> {
    let stem = crate::os::program_stem(shell);
    let command = command.filter(|c| !c.trim().is_empty());
    match (stem.as_str(), command) {
        ("cmd", Some(c)) => vec!["/D".into(), "/S".into(), "/C".into(), c.to_owned()],
        ("cmd", None) => Vec::new(),
        ("pwsh" | "powershell", Some(c)) => vec!["-NoLogo".into(), "-Command".into(), c.to_owned()],
        ("pwsh" | "powershell", None) => vec!["-NoLogo".into()],
        (_, Some(c)) => vec!["-c".into(), c.to_owned()],
        (_, None) => vec!["-l".into()],
    }
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
) -> anyhow::Result<(PtySession, async_channel::Receiver<PtyOut>)> {
    let pty = native_pty_system().openpty(size(cols, rows))?;
    let shell = default_shell(shell);
    let mut cmd = CommandBuilder::new(&shell);
    cmd.args(shell_args(&shell, command));
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("KILN_TERMINAL", "1");
    if cfg!(unix) && std::env::var_os("LANG").is_none() {
        cmd.env("LANG", "C.UTF-8");
    }
    cmd.cwd(crate::os::home_dir());
    let io = crate::os::pty::PtyIo::new(pty.master.as_ref()).context("PTY master")?;
    let child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let killer = child.clone_killer();
    let (tx, rx) = async_channel::bounded::<PtyOut>(256);
    let (in_tx, in_rx) = async_channel::unbounded::<Vec<u8>>();
    io.start(child, tx, in_rx);
    crate::metrics::global().terminal_sessions.add(1);
    Ok((PtySession { input: in_tx, master: Some(pty.master), killer }, rx))
}

impl PtySession {
    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.try_send(data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(master) = &self.master
            && let Err(e) = master.resize(size(cols, rows))
        {
            tracing::debug!(error = %e, "pty resize failed");
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
        if let Some(master) = self.master.take() {
            crate::os::pty::close_master(master);
        }
        crate::metrics::global().terminal_sessions.add(-1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_arguments() {
        assert_eq!(shell_args("/bin/bash", None), ["-l"]);
        assert_eq!(shell_args("/bin/sh", Some("ls")), ["-c", "ls"]);
        assert_eq!(shell_args(r"C:\Windows\System32\cmd.exe", Some("dir")), ["/D", "/S", "/C", "dir"]);
        assert_eq!(shell_args("pwsh.exe", Some("ls")), ["-NoLogo", "-Command", "ls"]);
        assert_eq!(shell_args("powershell", None), ["-NoLogo"]);
    }

    #[cfg(unix)]
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
