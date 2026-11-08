//! Pseudo-terminal sessions: a real PTY (so `isatty()` is true, resize reaches the program, and
//! full-screen TUIs such as k9s, htop and vim work) bridged to lock-free queues (`crate::memory::queue`;
//! goal 5, plan 01a1a59c2aae: an async-channel's wait took event-listener's `Mutex`, its reader's shard
//! and the terminal's relay taking it for each piece of output). Shared by the server's "local" terminal
//! and every agent. Unix PTYs and Windows ConPTY both come from portable-pty; the I/O plumbing is the
//! platform layer's ([`crate::os::pty`]).

use anyhow::Context as _;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

#[derive(Debug)]
pub enum PtyOut {
    Data(Vec<u8>),
    Exit(Option<i32>),
}

pub struct PtySession {
    input: crate::memory::queue::Sender<Vec<u8>>,
    /// portable-pty's side, for sessions this process started (None once adopted across an exec).
    master: Option<Box<dyn MasterPty + Send>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// The master and shell pid: resizes and hang-ups for adopted sessions, and the handover.
    handle: Option<crate::os::pty::Handle>,
    /// The shell's pid, to tell which programs run inside this terminal.
    pid: Option<u32>,
}

/// What a re-executed kiln needs to take a session over: the master's descriptor number (kept
/// open across the exec) and the shell's pid (still our child: exec keeps the pid).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Handover {
    pub fd: i32,
    pub pid: u32,
    /// The shell is another process's child (handed over by another runner of this machine,
    /// blue/green), not this one's: its exit is watched, not reaped.
    #[serde(default)]
    pub foreign: bool,
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

/// Environment Claude Code sets for the programs a session runs.
const CLAUDE_SESSION_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
];

/// Start `command` (or an interactive login shell) on a new PTY.
pub fn spawn(
    shell: Option<&str>,
    command: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<(PtySession, crate::memory::queue::Receiver<PtyOut>)> {
    spawn_with(shell, command, cols, rows, &[])
}

/// [`spawn`], with extra environment for the shell (e.g. how `kiln spawn` reaches its kiln).
pub fn spawn_with(
    shell: Option<&str>,
    command: Option<&str>,
    cols: u16,
    rows: u16,
    env: &[(&str, String)],
) -> anyhow::Result<(PtySession, crate::memory::queue::Receiver<PtyOut>)> {
    spawn_at(shell, command, None, None, cols, rows, env)
}

/// [`spawn_with`], starting in `cwd` (else the home directory), or running `program` (its path
/// and arguments) instead of a shell.
pub fn spawn_at(
    shell: Option<&str>,
    command: Option<&str>,
    program: Option<&[String]>,
    cwd: Option<&std::path::Path>,
    cols: u16,
    rows: u16,
    env: &[(&str, String)],
) -> anyhow::Result<(PtySession, crate::memory::queue::Receiver<PtyOut>)> {
    let pty = native_pty_system().openpty(size(cols, rows))?;
    let shell = default_shell(shell);
    let mut cmd = match program.and_then(|p| p.split_first()) {
        Some((prog, args)) => {
            let mut c = CommandBuilder::new(prog);
            c.args(args);
            c
        }
        None => {
            let mut c = CommandBuilder::new(&shell);
            c.args(shell_args(&shell, command));
            c
        }
    };
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("KILN_TERMINAL", "1");
    // A kiln terminal is a session of its own, even when kiln was started from inside a Claude
    // Code session: drop that session's markers (and its messaging token), or a `claude` run
    // here believes it is nested and neither saves its transcript nor registers itself.
    for marker in CLAUDE_SESSION_MARKERS {
        cmd.env_remove(marker);
    }
    // Programs started here can rely on it (`exec $SHELL -l` after an agent exits).
    if cfg!(unix) {
        cmd.env("SHELL", &shell);
    }
    if cfg!(unix) && std::env::var_os("LANG").is_none() {
        cmd.env("LANG", "C.UTF-8");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.cwd(cwd.map_or_else(crate::os::home_dir, std::path::Path::to_path_buf));
    let io = crate::os::pty::PtyIo::new(pty.master.as_ref()).context("PTY master")?;
    let child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let killer = child.clone_killer();
    let pid = child.process_id();
    let handle = pid.map(|pid| io.handle(pid));
    let ((tx, rx), (in_tx, in_rx)) = (output(), input());
    io.start(child, tx, in_rx);
    crate::metrics::global().terminal_sessions.add(1);
    Ok((PtySession { input: in_tx, master: Some(pty.master), killer: Some(killer), handle, pid }, rx))
}

/// A terminal's output on its way out, in a pipe whose room its pace sets (`crate::memory::queue::pipe`: what its
/// reader's socket takes while the terminal writes the next piece, a piece past it; consts-audit B4: 256 pieces, as
/// the channel it was held), its reader waiting for room past it.
fn output() -> (crate::memory::queue::Sender<PtyOut>, crate::memory::queue::Receiver<PtyOut>) {
    crate::memory::queue::pipe(&crate::memory::owners::CHANNELS)
}

/// A piece of output weighs its bytes; an exit nothing.
impl crate::memory::queue::Weigh for PtyOut {
    fn weight(&self) -> usize {
        match self {
            PtyOut::Data(data) => data.len(),
            PtyOut::Exit(_) => 0,
        }
    }
}

/// A terminal's keystrokes on their way in: unbounded, as the channel it was (a write never waits:
/// [`PtySession::write`]), refused only once its writer has gone.
fn input() -> (crate::memory::queue::Sender<Vec<u8>>, crate::memory::queue::Receiver<Vec<u8>>) {
    crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS)
}

/// Take over a session handed across this process's re-exec ([`PtySession::handover`]). `None`
/// when its descriptor did not survive (the session is then gone).
pub fn adopt(h: Handover) -> Option<(PtySession, crate::memory::queue::Receiver<PtyOut>)> {
    let ((tx, rx), (in_tx, in_rx)) = (output(), input());
    // One expression: on Windows the descriptor and the handle are both `()`.
    let session = PtySession {
        input: in_tx,
        master: None,
        killer: None,
        handle: Some(crate::os::pty::adopt(crate::os::reclaim_fd(h.fd)?, h.pid, h.foreign, tx, in_rx)),
        pid: Some(h.pid),
    };
    crate::metrics::global().terminal_sessions.add(1);
    Some((session, rx))
}

impl PtySession {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input.try_send(data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(master) = &self.master {
            if let Err(e) = master.resize(size(cols, rows)) {
                tracing::debug!(error = %e, "pty resize failed");
            }
        } else if let Some(h) = &self.handle {
            h.resize(cols, rows);
        }
    }

    /// Prepare this session to survive an exec of this process: keep its master open and say how
    /// the new image finds it. `None` where that is impossible (Windows; no shell pid).
    pub fn handover(&self) -> Option<Handover> {
        let h = self.handle.as_ref()?;
        if !crate::os::IN_PLACE_UPGRADE || h.keep_across_exec().is_err() {
            return None;
        }
        Some(Handover { fd: h.raw_fd(), pid: h.pid(), foreign: false })
    }

    /// Let this session go to another runner process of this machine (a blue/green upgrade): its
    /// shell keeps running, nothing more is read here, and this side lets go of it without
    /// hanging it up. A copy of its master for the other process, and the shell's pid; `None`
    /// (the session is left as it is) where it can't be.
    #[cfg(unix)]
    pub fn hand_off(mut self) -> Result<(std::os::fd::OwnedFd, u32), Self> {
        let Some(h) = self.handle.as_ref() else { return Err(self) };
        let Ok(fd) = h.hand_off() else { return Err(self) };
        let pid = h.pid();
        // Neither killed nor hung up when this side is dropped.
        self.handle = None;
        if let Some(k) = self.killer.take() {
            std::mem::forget(k);
        }
        Ok((fd, pid))
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        match (&mut self.killer, &self.handle) {
            (Some(k), _) => {
                let _ = k.kill();
            }
            (None, Some(h)) => h.hangup(),
            (None, None) => {}
        }
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

    /// Goal 5, plan 01a1a59c2aae (its top goal), the runner's side; consts-audit B4: a terminal's output is a lock-free
    /// pipe whose room its pace sets (it was 256 pieces, as its channel was). Not taken while its program writes far more,
    /// it holds no more than its room, two pieces while nothing has taken any (its pace unlearned), and its reader waits
    /// for room; then taken, every byte comes once, in order, its exit with it; and it ends once its program has.
    #[cfg(unix)]
    #[test]
    fn g5_b4_a_terminals_output_holds_its_room_keeps_its_order_and_ends() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            // Far past what its room holds of its reader's largest reads (32 KiB). Every number in full (macOS's `seq`
            // writes 1000000 on as 1e+06).
            let lines = 1_100_000u32;
            let (s, rx) = spawn(Some("/bin/sh"), Some(&format!("seq -f %.0f 1 {lines}")), 80, 24)?;
            let came = crate::rt::timeout(std::time::Duration::from_secs(30), async {
                while rx.is_empty() {
                    crate::rt::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
            anyhow::ensure!(came.is_ok(), "its output never came");
            crate::rt::sleep(std::time::Duration::from_millis(200)).await;
            anyhow::ensure!((1..=2).contains(&rx.len()), "{} pieces held ({} bytes, its room {}), past the two an unlearned pace holds", rx.len(), rx.held(), rx.bound());
            let (mut out, mut code) = (Vec::new(), None);
            loop {
                match crate::rt::timeout(std::time::Duration::from_secs(30), rx.recv()).await.map_err(|_| anyhow::anyhow!("its output never ended"))? {
                    Ok(PtyOut::Data(d)) => out.extend(d.into_iter().filter(|b| *b != b'\r')),
                    Ok(PtyOut::Exit(c)) => {
                        anyhow::ensure!(code.is_none(), "two exits");
                        code = Some(c);
                    }
                    Err(_) => break,
                }
            }
            let want: String = (1..=lines).map(|n| format!("{n}\n")).collect();
            anyhow::ensure!(out == want.as_bytes(), "{} bytes of {}, in order: {}", out.len(), want.len(), out.starts_with(want.as_bytes()));
            anyhow::ensure!(code == Some(Some(0)), "{code:?}");
            drop(s);
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), the runner's side: what's typed into a terminal goes in
    /// on a lock-free queue, each write once, in the order written.
    #[cfg(unix)]
    #[test]
    fn g5_a_terminals_keystrokes_go_in_once_in_order() -> anyhow::Result<()> {
        /// What the terminal says, `\r`s left out, onto `out` until `done` holds of it.
        async fn read_until(rx: &crate::memory::queue::Receiver<PtyOut>, out: &mut String, done: impl Fn(&str) -> bool) -> anyhow::Result<()> {
            while !done(out) {
                match crate::rt::timeout(std::time::Duration::from_secs(10), rx.recv()).await.map_err(|_| anyhow::anyhow!("no more output: {out:?}"))? {
                    Ok(PtyOut::Data(d)) => out.push_str(&String::from_utf8_lossy(&d).replace('\r', "")),
                    other => anyhow::bail!("{other:?} before {out:?}"),
                }
            }
            Ok(())
        }
        crate::rt::block_on(async move {
            let (s, rx) = spawn(Some("/bin/sh"), Some("stty -echo; echo ready; cat"), 80, 24)?;
            let mut out = String::new();
            read_until(&rx, &mut out, |o| o.contains("ready\n")).await?;
            let n = 300;
            for i in 0..n {
                s.write(format!("line-{i}\n").into_bytes());
            }
            read_until(&rx, &mut out, |o| o.contains(&format!("line-{}\n", n - 1))).await?;
            let after = out.split_once("ready\n").map(|(_, rest)| rest.to_owned()).unwrap_or_default();
            let want: String = (0..n).map(|i| format!("line-{i}\n")).collect();
            anyhow::ensure!(after == want, "{after:?}");
            // Its end typed in: the program ends.
            s.write(vec![4]);
            let code = loop {
                match crate::rt::timeout(std::time::Duration::from_secs(10), rx.recv()).await.map_err(|_| anyhow::anyhow!("never ended"))? {
                    Ok(PtyOut::Data(_)) => {}
                    Ok(PtyOut::Exit(c)) => break c,
                    Err(_) => break None,
                }
            };
            anyhow::ensure!(code == Some(0), "{code:?}");
            Ok(())
        })
    }

    /// A program a signal ends has no exit code (portable-pty would say 1: a build killed from
    /// outside read as a failed build); one that exits keeps its code.
    #[cfg(unix)]
    #[test]
    fn a_program_ended_by_a_signal_has_no_exit_code() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            for (command, want) in [("echo bye; kill -TERM $$; sleep 5", None), ("exit 3", Some(3))] {
                let (_s, rx) = spawn(Some("/bin/sh"), Some(command), 80, 24)?;
                let code = loop {
                    match crate::rt::timeout(std::time::Duration::from_secs(10), rx.recv()).await.map_err(|_| anyhow::anyhow!("pty timed out"))? {
                        Ok(PtyOut::Data(_)) => {}
                        Ok(PtyOut::Exit(c)) => break c,
                        Err(_) => anyhow::bail!("no exit for {command}"),
                    }
                };
                assert_eq!(code, want, "{command}");
            }
            Ok(())
        })
    }

    /// A shell handed over by the runner process before this one is not this process's child
    /// (macOS upgrades): its exit is still seen, without a status.
    #[cfg(unix)]
    #[test]
    fn an_adopted_shell_that_is_not_our_child_is_seen_to_exit() -> anyhow::Result<()> {
        // Handed to a fresh process (macOS), it is reaped by no one here; handed to another
        // runner process of the machine (blue/green, `foreign`), its exit is only watched.
        for foreign in [false, true] {
            crate::rt::block_on(async move {
                // Started by a shell that exits at once: not this process's child.
                let out = std::process::Command::new("/bin/sh").args(["-c", "sleep 1 >/dev/null 2>&1 & echo $!"]).output()?;
                let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse()?;
                // Its "master": a pipe's read end, non-blocking as a handed-over master is.
                let (r, w) = std::io::pipe()?;
                let r = std::os::fd::OwnedFd::from(r);
                crate::os::proc::nonblocking(&r)?;
                let ((tx, rx), (_input, input_rx)) = (output(), input());
                let _session = crate::os::pty::adopt(r, pid, foreign, tx, input_rx);
                drop(w);
                let code = loop {
                    match crate::rt::timeout(std::time::Duration::from_secs(15), rx.recv()).await {
                        Ok(Ok(PtyOut::Exit(code))) => break code,
                        Ok(Ok(PtyOut::Data(_))) => {}
                        _ => anyhow::bail!("the adopted shell's exit was never seen (foreign: {foreign})"),
                    }
                };
                assert_eq!(code, None, "no status for a process that is not our child");
                assert!(!crate::os::pid_alive(pid));
                Ok::<(), anyhow::Error>(())
            })?;
        }
        Ok(())
    }
}
