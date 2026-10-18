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
/// A runner that takes up a new binary itself which still runs the old one this long after: it
/// isn't going to (a build that can't). Restarted once nothing runs under it.
const STALE_LIMIT: Duration = Duration::from_secs(20 * 60);

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

/// Whether the process `pid` still runs a binary that is no longer the one at `exe` (it hasn't
/// taken up an upgrade). Linux only, where /proc says which file a process runs; elsewhere never.
fn runs_old_binary(pid: u32, exe: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt as _;
        let (Ok(running), Ok(on_disk)) = (std::fs::metadata(format!("/proc/{pid}/exe")), std::fs::metadata(exe)) else { return false };
        (running.dev(), running.ino()) != (on_disk.dev(), on_disk.ino())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, exe);
        false
    }
}

/// Whether anything still runs under `root` (zombies aside): what restarting it would end.
fn busy_under(root: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
    let root = Pid::from_u32(root);
    sys.processes().values().any(|p| {
        if p.status() == ProcessStatus::Zombie {
            return false;
        }
        let mut up = p.parent();
        for _ in 0..64 {
            match up {
                Some(x) if x == root => return true,
                Some(x) => up = sys.process(x).and_then(sysinfo::Process::parent),
                None => return false,
            }
        }
        false
    })
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
        // Since when the child has run a binary that isn't the one on disk, and whether that was said.
        let mut stale_since: Option<Instant> = None;
        let mut stale_said = false;

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
                    // A child that takes up new binaries itself but still runs an old one long
                    // after (a build that never does): restarted once nothing runs under it, so
                    // nothing it runs is cut short.
                    if upgrades_itself(&opts.args)
                        && let Some(pid) = control.pid().and_then(|p| u32::try_from(p).ok())
                    {
                        if runs_old_binary(pid, &exe) {
                            let since = *stale_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= STALE_LIMIT {
                                if !std::mem::replace(&mut stale_said, true) {
                                    let msg = format!("the runner (pid {pid}) still runs the binary it started with, {} minutes after a new one was installed: it restarts into the new one once nothing runs under it", STALE_LIMIT.as_secs() / 60);
                                    tracing::warn!("{msg}");
                                    crash_log(&opts.state_dir, &msg);
                                }
                                if !crate::rt::blocking(move || busy_under(pid)).await.unwrap_or(true) {
                                    break Exit::Upgrade;
                                }
                            }
                        } else {
                            stale_since = None;
                            stale_said = false;
                        }
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_child_still_on_a_replaced_binary_is_seen_and_so_is_what_runs_under_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let exe = dir.path().join("prog");
        std::fs::copy("/bin/sh", &exe)?;
        // A child running `exe` that starts a process of its own.
        use std::os::unix::process::CommandExt as _;
        let mut child = std::process::Command::new(&exe).args(["-c", "sleep 30 & wait"]).process_group(0).spawn()?;
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!runs_old_binary(pid, &exe), "it runs the binary on disk");
        assert!(busy_under(pid), "its sleep runs under it");
        // A new binary installed where it was (a new file, renamed into place): the child runs the old one.
        let next = dir.path().join("prog.next");
        std::fs::copy("/bin/sh", &next)?;
        std::fs::rename(&next, &exe)?;
        assert!(runs_old_binary(pid, &exe));
        // It and its sleep, as a group.
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(i32::try_from(pid)?), nix::sys::signal::Signal::SIGKILL);
        let _ = child.wait();
        assert!(!busy_under(pid), "nothing runs under a process that is gone");
        Ok(())
    }
}
