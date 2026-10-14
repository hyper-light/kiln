//! `kiln supervise -- <subcommand...>`: a tiny parent process that keeps a kiln child alive without
//! systemd or Kubernetes. It
//!
//! * restarts the child on any exit that was not requested, with jittered exponential backoff
//!   (reset once the child has been healthy for a minute),
//! * forwards SIGTERM / SIGINT / SIGHUP and gives the child a grace period before SIGKILL,
//! * reaps orphaned zombies so it is safe as PID 1 inside a container,
//! * re-executes the binary on disk when it changes (in-place upgrades), and
//! * appends every crash to `<state>/crash.log` for the UI.
//!
//! The supervisor itself does almost nothing, so there is little that can fail; every error it
//! does hit is logged and retried rather than propagated.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const HEALTHY: Duration = Duration::from_secs(60);
const GRACE: Duration = Duration::from_secs(20);

pub struct Options {
    pub args: Vec<String>,
    pub state_dir: PathBuf,
}

fn exe_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn crash_log(dir: &Path, line: &str) {
    let path = dir.join("crash.log");
    let res = std::fs::create_dir_all(dir).and_then(|()| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{} {line}", crate::util::now_rfc3339())
    });
    if let Err(e) = res {
        tracing::warn!(error = %e, path = %path.display(), "cannot write crash log");
    }
}

/// Reap any zombie that is not our direct child (we are PID 1 in containers).
fn reap_orphans(child: Option<Pid>) {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => return,
            Ok(st) => {
                if st.pid().is_some() && st.pid() == child {
                    // Our child was reaped here; tokio's waiter will report ECHILD, handled below.
                    return;
                }
            }
        }
    }
}

fn lock_path(state_dir: &Path, args: &[String]) -> PathBuf {
    let key = crate::util::sha256_hex(args.join(" ").as_bytes());
    state_dir.join(format!("supervise-{}.lock", key.get(..12).unwrap_or("x")))
}

/// Hold an exclusive lock for this (state dir, args) pair so boot hooks, cron and a manual start
/// can never run two supervisors for the same service.
fn single_instance(state_dir: &Path, args: &[String]) -> anyhow::Result<Option<nix::fcntl::Flock<std::fs::File>>> {
    std::fs::create_dir_all(state_dir)?;
    let path = lock_path(state_dir, args);
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    match nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(l) => Ok(Some(l)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, e)) => Err(anyhow::anyhow!("lock {}: {e}", path.display())),
    }
}

/// Start `kiln supervise -- <args>` in its own session, detached from the terminal, with output
/// appended to `<state>/kiln.log` (rotated at 64 MiB). Returns immediately.
pub fn detach(args: &[String], state_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let log = state_dir.join("kiln.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > 64 << 20) {
        let _ = std::fs::rename(&log, state_dir.join("kiln.log.1"));
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let err = out.try_clone()?;
    let exe = std::env::current_exe()?;
    use std::os::unix::process::CommandExt as _;
    std::process::Command::new(exe)
        .arg("supervise")
        .arg("--")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()?;
    tracing::info!(log = %log.display(), "supervisor started in the background");
    Ok(())
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let Some(_lock) = single_instance(&opts.state_dir, &opts.args)? else {
        tracing::info!(args = ?opts.args, "already running under another supervisor; nothing to do");
        return Ok(());
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kiln"));
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigchld = signal(SignalKind::child())?;
    let is_pid1 = std::process::id() == 1;
    let mut backoff = MIN_BACKOFF;
    let mut generation: u64 = 0;

    loop {
        generation += 1;
        let started_mtime = exe_mtime(&exe);
        let mut cmd = Command::new(&exe);
        cmd.args(&opts.args)
            .env("KILN_SUPERVISED", "1")
            .env("KILN_GENERATION", generation.to_string())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, exe = %exe.display(), "spawn failed");
                crash_log(&opts.state_dir, &format!("spawn failed: {e}"));
                tokio::time::sleep(crate::sup::jitter(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let pid = child.id().and_then(|p| i32::try_from(p).ok()).map(Pid::from_raw);
        tracing::info!(generation, pid = ?pid, "child started");
        let started = Instant::now();
        let mut upgrade_check = tokio::time::interval(Duration::from_secs(10));
        upgrade_check.tick().await;

        enum Exit {
            Child(String),
            Shutdown(Signal),
            Upgrade,
        }
        let exit = loop {
            tokio::select! {
                st = child.wait() => {
                    break Exit::Child(match st {
                        Ok(s) => s.to_string(),
                        Err(e) => format!("wait error: {e}"),
                    });
                }
                _ = sigterm.recv() => break Exit::Shutdown(Signal::SIGTERM),
                _ = sigint.recv() => break Exit::Shutdown(Signal::SIGINT),
                _ = sighup.recv() => {
                    // HUP = reload: forward to the child, which re-reads its config.
                    if let Some(p) = pid { let _ = kill(p, Signal::SIGHUP); }
                }
                _ = sigchld.recv() => if is_pid1 { reap_orphans(pid) },
                _ = upgrade_check.tick() => {
                    if exe_mtime(&exe) != started_mtime && started.elapsed() > Duration::from_secs(5) {
                        break Exit::Upgrade;
                    }
                }
            }
        };

        match exit {
            Exit::Shutdown(sig) => {
                tracing::info!(?sig, "shutdown requested; stopping child");
                stop_child(&mut child, pid).await;
                return Ok(());
            }
            Exit::Upgrade => {
                tracing::info!("binary changed on disk; restarting child into new version");
                stop_child(&mut child, pid).await;
                backoff = MIN_BACKOFF;
                continue;
            }
            Exit::Child(status) => {
                if started.elapsed() >= HEALTHY {
                    backoff = MIN_BACKOFF;
                }
                let msg = format!(
                    "generation {generation} exited ({status}) after {:?}; restarting in ~{:?}",
                    started.elapsed(),
                    backoff
                );
                tracing::error!("{msg}");
                crash_log(&opts.state_dir, &msg);
                tokio::select! {
                    _ = sigterm.recv() => return Ok(()),
                    _ = sigint.recv() => return Ok(()),
                    () = tokio::time::sleep(crate::sup::jitter(backoff)) => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn stop_child(child: &mut tokio::process::Child, pid: Option<Pid>) {
    if let Some(p) = pid {
        let _ = kill(p, Signal::SIGTERM);
    }
    if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
        tracing::warn!("child ignored SIGTERM for {GRACE:?}; killing");
        let _ = child.kill().await;
    }
}
