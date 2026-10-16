//! `kiln supervise` on Windows. There is no SIGTERM or SIGHUP, and console control events
//! reach a whole console rather than one process, so the supervisor hands its child a stdin pipe
//! and asks it to stop by closing that pipe: the child (with `KILN_SUPERVISED=1`) watches its
//! stdin and shuts down gracefully at EOF. That also stops the child cleanly if the supervisor
//! itself dies. A child that ignores the request is killed with its tree after the grace period.

use std::io::Read as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

/// `CREATE_NO_WINDOW`: a hidden console that the supervisor's children inherit (no window pops
/// up at logon). `CREATE_NEW_PROCESS_GROUP`: Ctrl-C elsewhere does not reach it.
/// `CREATE_BREAKAWAY_FROM_JOB`: outlive the Scheduled Task (or shell) that launched it.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// How the supervisor talks to its running child.
pub struct ChildControl {
    pid: u32,
    stdin: Option<ChildStdin>,
}

impl ChildControl {
    /// Spawn the supervised child with a stdin pipe the supervisor keeps (its stop request).
    pub fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Self)> {
        let mut child = cmd.stdin(Stdio::piped()).spawn()?;
        let stdin = child.stdin.take();
        let pid = child.id();
        Ok((child, Self { pid, stdin }))
    }

    pub fn pid(&self) -> Option<i32> {
        i32::try_from(self.pid).ok()
    }

    /// Windows has no reload signal; restart the service to apply new configuration.
    pub fn reload(&self) {}

    /// Ask the child to shut down gracefully: close its stdin.
    pub fn stop(&mut self) {
        drop(self.stdin.take());
    }

    /// Kill the child and everything it started.
    pub async fn kill(&mut self) {
        let pid = self.pid;
        let _ = crate::rt::blocking(move || crate::os::proc::taskkill_tree(pid)).await;
    }
}

/// Wait for the child (a 100 ms `try_wait` poll) and report how it ended.
pub async fn wait_child(mut child: Child) -> String {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.to_string(),
            Ok(None) => crate::rt::sleep(Duration::from_millis(100)).await,
            Err(e) => return format!("wait error: {e}"),
        }
    }
}

/// Spawn `cmd` detached: no console window, its own process group, and out of any job object
/// that allows breakaway (refused breakaway is retried without it).
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt as _;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(_) => {
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
        }
    }
}

/// Kill every running `kiln supervise … -- <subcommand> …` of this user (with its child and the
/// child's jobs): a detached supervisor outlives the Scheduled Task that launched it, so
/// `kiln uninstall` stops it here. Returns how many were stopped.
pub fn stop_supervisors(subcommand: &str) -> usize {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let me = std::process::id();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
    let mut stopped = 0;
    for (pid, p) in sys.processes() {
        let args: Vec<String> = p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let is_kiln = args.first().is_some_and(|a| crate::os::program_stem(a) == "kiln");
        let ours = is_kiln
            && args.iter().any(|a| a == "supervise")
            && args.iter().any(|a| a == subcommand)
            && (subcommand == "agent" || !args.iter().any(|a| a == "agent"));
        if ours && pid.as_u32() != me && crate::os::proc::taskkill_tree(pid.as_u32()) {
            stopped += 1;
        }
    }
    stopped
}

/// In a supervised child, treat EOF on stdin (the supervisor closed it, or died) as a shutdown
/// request.
pub fn watch_supervisor(shutdown: crate::rt::Shutdown) {
    if !std::env::var("KILN_SUPERVISED").is_ok_and(|v| v == "1") {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-supervisor-pipe".into()).spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        tracing::info!("the supervisor asked for a shutdown");
        shutdown.cancel();
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch the supervisor pipe");
    }
}
