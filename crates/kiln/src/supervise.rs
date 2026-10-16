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

use futures::FutureExt as _;
use futures::channel::oneshot;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::rt::{Interval, Signals};

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

/// Wait for the child without a thread and report how it ended. Normally that is its pidfd. As
/// PID 1 (containers) every orphan re-parented to us must be reaped too, so a `waitpid(-1,
/// WNOHANG)` sweep runs whenever the child's pidfd fires and at least once a second; only our own
/// child's status is reported. One reaper for everything means no two waits race for a pid.
fn spawn_waiter(child: std::process::Child, pid1: bool) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    crate::rt::spawn(async move {
        let status = if pid1 { wait_as_pid1(child).await } else { wait_plain(child).await };
        let _ = tx.send(status);
    });
    rx
}

async fn wait_plain(mut child: std::process::Child) -> String {
    let pid = child.id();
    match crate::proc::wait_pid_exit(pid, || child.try_wait()).await {
        Ok(status) => status.to_string(),
        Err(e) => format!("wait error: {e}"),
    }
}

async fn wait_as_pid1(child: std::process::Child) -> String {
    let raw = i32::try_from(child.id()).unwrap_or(0);
    let pid = Pid::from_raw(raw);
    let pidfd = rustix::process::Pid::from_raw(raw)
        .and_then(|p| rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()).ok());
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

fn lock_path(state_dir: &Path, args: &[String]) -> PathBuf {
    let key = crate::util::sha256_hex(args.join(" ").as_bytes());
    state_dir.join(format!("supervise-{}.lock", key.get(..12).unwrap_or("x")))
}

/// Hold an exclusive lock for this (state dir, args) pair so boot hooks, cron and a manual start
/// can never run two supervisors for the same service. The lock is the returned file's (`flock`
/// on Unix, `LockFileEx` on Windows) and lasts until it is dropped.
fn single_instance(state_dir: &Path, args: &[String]) -> anyhow::Result<Option<std::fs::File>> {
    std::fs::create_dir_all(state_dir)?;
    let path = lock_path(state_dir, args);
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::anyhow!("lock {}: {e}", path.display())),
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
    let signals = Signals::new(&[crate::rt::SIGTERM, crate::rt::SIGINT, crate::rt::SIGHUP])?;
    let is_pid1 = std::process::id() == 1;
    let mut backoff = MIN_BACKOFF;
    let mut generation: u64 = 0;

    loop {
        generation += 1;
        let started_mtime = exe_mtime(&exe);
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&opts.args).env("KILN_SUPERVISED", "1").env("KILN_GENERATION", generation.to_string());
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, exe = %exe.display(), "spawn failed");
                crash_log(&opts.state_dir, &format!("spawn failed: {e}"));
                crate::rt::sleep(crate::sup::jitter(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let pid = i32::try_from(child.id()).ok().map(Pid::from_raw);
        let mut exited = spawn_waiter(child, is_pid1);
        tracing::info!(generation, pid = ?pid, "child started");
        let started = Instant::now();
        let mut upgrade_check = Interval::new(Duration::from_secs(10));

        enum Exit {
            Child(String),
            Shutdown(Signal),
            Upgrade,
        }
        enum Ev {
            Exited(String),
            Signal(Option<i32>),
            UpgradeCheck,
        }
        let exit = loop {
            let ev = futures::select_biased! {
                st = (&mut exited).fuse() => Ev::Exited(st.unwrap_or_else(|_| "waiter lost".to_owned())),
                sig = signals.next().fuse() => Ev::Signal(sig),
                () = upgrade_check.tick().fuse() => Ev::UpgradeCheck,
            };
            match ev {
                Ev::Exited(status) => break Exit::Child(status),
                Ev::Signal(Some(crate::rt::SIGHUP)) => {
                    // HUP = reload: forward to the child, which re-reads its config.
                    if let Some(p) = pid {
                        let _ = kill(p, Signal::SIGHUP);
                    }
                }
                Ev::Signal(Some(crate::rt::SIGINT)) => break Exit::Shutdown(Signal::SIGINT),
                Ev::Signal(_) => break Exit::Shutdown(Signal::SIGTERM),
                Ev::UpgradeCheck => {
                    if exe_mtime(&exe) != started_mtime && started.elapsed() > Duration::from_secs(5) {
                        break Exit::Upgrade;
                    }
                }
            }
        };

        match exit {
            Exit::Shutdown(sig) => {
                tracing::info!(?sig, "shutdown requested; stopping child");
                stop_child(&mut exited, pid).await;
                return Ok(());
            }
            Exit::Upgrade => {
                tracing::info!("binary changed on disk; restarting child into new version");
                stop_child(&mut exited, pid).await;
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
                let pause = crate::rt::sleep(crate::sup::jitter(backoff)).fuse();
                futures::pin_mut!(pause);
                loop {
                    futures::select_biased! {
                        sig = signals.next().fuse() => match sig {
                            Some(crate::rt::SIGHUP) => {}
                            _ => return Ok(()),
                        },
                        () = pause => break,
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn stop_child(exited: &mut oneshot::Receiver<String>, pid: Option<Pid>) {
    if let Some(p) = pid {
        let _ = kill(p, Signal::SIGTERM);
    }
    if crate::rt::timeout(GRACE, &mut *exited).await.is_err() {
        tracing::warn!("child ignored SIGTERM for {GRACE:?}; killing");
        if let Some(p) = pid {
            let _ = kill(p, Signal::SIGKILL);
        }
        let _ = crate::rt::timeout(Duration::from_secs(5), exited).await;
    }
}
