//! `kiln supervise -- <subcommand...>`: a tiny parent process that keeps a kiln child alive without
//! systemd or Kubernetes. It
//!
//! * restarts the child on any exit that was not requested, with jittered exponential backoff
//!   (reset once the child has been healthy for a minute),
//! * forwards SIGTERM / SIGINT / SIGHUP and gives the child a grace period before SIGKILL (on
//!   Windows: closes the child's stdin to ask it to stop, then kills its process tree),
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

use crate::os::supervise::ChildControl;
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

/// Wait for a child this supervisor adopted across its own re-exec (no `Child` handle, only a pid).
fn spawn_pid_waiter(pid: u32) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    crate::rt::spawn(async move {
        let st = crate::os::proc::wait_pid_exit(pid, || crate::os::try_reap(pid).map(|st| st.map(|(_, d)| d))).await;
        let _ = tx.send(st.unwrap_or_else(|e| format!("wait error: {e}")));
    });
    rx
}

/// The supervised subcommand (the first argument that is not an option).
fn subcommand(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| !a.starts_with('-'))
}

/// Children that take up a new binary themselves, in place, keeping their terminals alive: the
/// supervisor must not restart them for an upgrade.
fn upgrades_itself(args: &[String]) -> bool {
    crate::os::IN_PLACE_UPGRADE && subcommand(args) == Some("agent")
}

/// Wait for the child (in the platform layer: pidfd / poll, plus orphan reaping as PID 1).
fn spawn_waiter(child: std::process::Child) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    crate::rt::spawn(async move {
        let _ = tx.send(crate::os::supervise::wait_child(child).await);
    });
    rx
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
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("supervise").arg("--").args(args).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    crate::os::supervise::spawn_detached(&mut cmd)?;
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
    let mut backoff = MIN_BACKOFF;
    let mut generation: u64 = 0;
    // A previous image of this supervisor re-executed itself into a new binary and left us its
    // running child ("<pid>:<generation>"): take it over instead of starting another.
    let mut inherited: Option<(u32, u64)> = std::env::var("KILN_SUPERVISE_CHILD").ok().and_then(|v| {
        let (pid, generation) = v.split_once(':')?;
        Some((pid.parse().ok()?, generation.parse().ok()?))
    });

    loop {
        generation += 1;
        let started_mtime = exe_mtime(&exe);
        let adopted = inherited
            .take()
            .filter(|(pid, _)| crate::os::pid_alive(*pid))
            .and_then(|(pid, g)| ChildControl::adopt(pid).map(|c| (pid, g, c)));
        let (mut exited, mut control) = if let Some((pid, g, control)) = adopted {
            generation = g;
            tracing::info!(generation, pid, "supervising the child carried over from the previous binary");
            (spawn_pid_waiter(pid), control)
        } else {
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(&opts.args)
                .env("KILN_SUPERVISED", "1")
                .env("KILN_GENERATION", generation.to_string())
                .env_remove("KILN_SUPERVISE_CHILD");
            match ChildControl::spawn(&mut cmd) {
                Ok((child, control)) => (spawn_waiter(child), control),
                Err(e) => {
                    tracing::error!(error = %e, exe = %exe.display(), "spawn failed");
                    crash_log(&opts.state_dir, &format!("spawn failed: {e}"));
                    crate::rt::sleep(crate::sup::jitter(backoff)).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        };
        tracing::info!(generation, pid = ?control.pid(), "child started");
        let started = Instant::now();
        let mut upgrade_check = Interval::new(Duration::from_secs(10));

        enum Exit {
            Child(String),
            Shutdown(&'static str),
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
                    control.reload();
                }
                Ev::Signal(Some(crate::rt::SIGINT)) => break Exit::Shutdown("interrupt"),
                Ev::Signal(_) => break Exit::Shutdown("terminate"),
                Ev::UpgradeCheck => {
                    if exe_mtime(&exe) != started_mtime && started.elapsed() > Duration::from_secs(5) {
                        if upgrades_itself(&opts.args)
                            && let Some(pid) = control.pid()
                        {
                            // The child takes up the new binary itself (keeping its terminals);
                            // this supervisor does the same and keeps supervising it.
                            tracing::info!(pid, "binary changed on disk; the child upgrades itself in place, re-executing the supervisor");
                            let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
                            let err = crate::os::reexec(&exe, &args, &[("KILN_SUPERVISE_CHILD", format!("{pid}:{generation}"))]);
                            tracing::error!(error = %err, "supervisor re-exec failed; restarting the child instead");
                        }
                        break Exit::Upgrade;
                    }
                }
            }
        };

        match exit {
            Exit::Shutdown(sig) => {
                tracing::info!(signal = sig, "shutdown requested; stopping child");
                stop_child(&mut exited, &mut control).await;
                return Ok(());
            }
            Exit::Upgrade => {
                tracing::info!("binary changed on disk; restarting child into new version");
                stop_child(&mut exited, &mut control).await;
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

async fn stop_child(exited: &mut oneshot::Receiver<String>, control: &mut ChildControl) {
    control.stop();
    if crate::rt::timeout(GRACE, &mut *exited).await.is_err() {
        tracing::warn!("child ignored the stop request for {GRACE:?}; killing");
        control.kill().await;
        let _ = crate::rt::timeout(Duration::from_secs(5), exited).await;
    }
}
