//! `kiln supervise` on Unix: the child is signalled by pid (SIGHUP reload, SIGTERM stop,
//! SIGKILL), awaited through its pidfd (Linux) or a poll, and as PID 1 every orphan is reaped.

use std::process::{Child, Command};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

/// How the supervisor talks to its running child.
pub struct ChildControl {
    pid: Option<Pid>,
}

impl ChildControl {
    /// Spawn the supervised child (stdio inherited).
    pub fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Self)> {
        let child = cmd.spawn()?;
        let pid = i32::try_from(child.id()).ok().map(Pid::from_raw);
        Ok((child, Self { pid }))
    }

    pub fn pid(&self) -> Option<i32> {
        self.pid.map(Pid::as_raw)
    }

    /// Ask the child to reload its configuration (SIGHUP).
    pub fn reload(&self) {
        if let Some(p) = self.pid {
            let _ = kill(p, Signal::SIGHUP);
        }
    }

    /// Ask the child to shut down gracefully (SIGTERM).
    pub fn stop(&mut self) {
        if let Some(p) = self.pid {
            let _ = kill(p, Signal::SIGTERM);
        }
    }

    /// Kill the child outright (SIGKILL).
    pub async fn kill(&mut self) {
        if let Some(p) = self.pid {
            let _ = kill(p, Signal::SIGKILL);
        }
    }
}

/// Wait for the child without a thread and report how it ended. Normally that is its pidfd. As
/// PID 1 (containers) every orphan re-parented to us must be reaped too, so a `waitpid(-1,
/// WNOHANG)` sweep runs whenever the child's pidfd fires and at least once a second; only our own
/// child's status is reported. One reaper for everything means no two waits race for a pid.
pub async fn wait_child(child: Child) -> String {
    if std::process::id() == 1 { wait_as_pid1(child).await } else { wait_plain(child).await }
}

async fn wait_plain(mut child: Child) -> String {
    let pid = child.id();
    match crate::os::proc::wait_pid_exit(pid, || child.try_wait()).await {
        Ok(status) => status.to_string(),
        Err(e) => format!("wait error: {e}"),
    }
}

async fn wait_as_pid1(child: Child) -> String {
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(0));
    let pidfd = crate::os::proc::pidfd(child.id());
    loop {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(st) if st.pid() == Some(pid) => return describe(st),
                Ok(_) | Err(Errno::EINTR) => {}
                Err(Errno::ECHILD) => return "child vanished".to_owned(),
                Err(e) => return format!("wait error: {e}"),
            }
        }
        match &pidfd {
            Some(fd) => {
                let _ = crate::rt::timeout(Duration::from_secs(1), crate::os::proc::wait_readable(fd)).await;
            }
            None => crate::rt::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn describe(st: WaitStatus) -> String {
    match st {
        WaitStatus::Exited(_, code) => format!("exit status: {code}"),
        WaitStatus::Signaled(_, sig, _) => format!("signal: {sig}"),
        other => format!("{other:?}"),
    }
}

/// Spawn `cmd` detached from the terminal: its own process group.
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0).spawn()
}

/// A supervised child on Unix is stopped by signals, which `main` already handles.
pub fn watch_supervisor(_shutdown: crate::rt::Shutdown) {}

/// `kiln uninstall` on Unix leaves running supervisors to the boot manager that started them
/// (`systemctl disable --now`, `launchctl bootout`); nothing to find here.
pub fn stop_supervisors(_subcommand: &str) -> usize {
    0
}
