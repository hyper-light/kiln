//! `kiln supervise -- <subcommand...>`: a tiny parent process that keeps a kiln child alive without
//! systemd or Kubernetes. It
//!
//! * restarts the child on any exit that was not requested, with jittered exponential backoff
//!   (reset once the child has been healthy for a minute),
//! * forwards SIGTERM / SIGINT / SIGHUP and gives the child a grace period before SIGKILL (on
//!   Windows: closes the child's stdin to ask it to stop, then kills its process tree),
//! * reaps orphaned zombies so it is safe as PID 1 inside a container,
//! * takes up the binary on disk when it changes: on Linux it re-executes itself and the runner
//!   does the same (in-place upgrades); on macOS, where a process that re-executed itself loses
//!   the local network, the runner hands its terminals over and exits, and the supervisor starts
//!   the new binary as a fresh process ([`HANDOVER_EXIT`]); other children are restarted, and
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

/// The exit status of a runner that stopped to take up a new binary, its terminals handed to
/// this supervisor first: start the binary on disk at once, giving it the terminals.
pub const HANDOVER_EXIT: u8 = 75;

pub struct Options {
    pub args: Vec<String>,
    pub state_dir: PathBuf,
    /// The binary children run (`--exe`), when it is not this one: a launchd job's launcher (a
    /// copy of kiln upgrades leave alone, so macOS keeps knowing the job) runs the `kiln` that
    /// upgrades replace.
    pub exe: Option<PathBuf>,
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

/// How a child ended: its exit code, if it exited, and a description for the log.
type Ended = (Option<i32>, String);

/// Wait for a child this supervisor adopted across its own re-exec (no `Child` handle, only a pid).
fn spawn_pid_waiter(pid: u32) -> oneshot::Receiver<Ended> {
    let (tx, rx) = oneshot::channel();
    crate::rt::spawn(async move {
        let st = crate::os::proc::wait_pid_exit(pid, || crate::os::try_reap(pid)).await;
        let _ = tx.send(st.unwrap_or_else(|e| (None, format!("wait error: {e}"))));
    });
    rx
}

/// The supervised subcommand (the first argument that is not an option).
fn subcommand(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| !a.starts_with('-'))
}

/// How a child takes up a new binary on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takes {
    /// The runner re-executes itself once idle, keeping its terminals; so does this supervisor.
    InPlace,
    /// The runner hands its terminals here once idle and exits ([`HANDOVER_EXIT`]); this
    /// supervisor starts the new binary as a fresh process with them, and never re-executes.
    Successor,
    /// This supervisor restarts the child.
    Restart,
}

fn takes_upgrade(args: &[String]) -> Takes {
    if subcommand(args) != Some("agent") {
        Takes::Restart
    } else if crate::os::UPGRADE_BY_SUCCESSOR {
        Takes::Successor
    } else if crate::os::IN_PLACE_UPGRADE {
        Takes::InPlace
    } else {
        Takes::Restart
    }
}

/// Wait for the child (in the platform layer: pidfd / poll, plus orphan reaping as PID 1).
fn spawn_waiter(child: std::process::Child) -> oneshot::Receiver<Ended> {
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

/// Start `kiln supervise [--exe <exe>] -- <args>` in its own session, detached from the
/// terminal, with output appended to `<state>/kiln.log` (rotated at 64 MiB). Returns immediately.
pub fn detach(args: &[String], state_dir: &Path, child_exe: Option<&Path>) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let log = state_dir.join("kiln.log");
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > 64 << 20) {
        let _ = std::fs::rename(&log, state_dir.join("kiln.log.1"));
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let err = out.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("supervise");
    if let Some(e) = child_exe {
        cmd.arg("--exe").arg(e);
    }
    cmd.arg("--").args(args).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    crate::os::supervise::spawn_detached(&mut cmd)?;
    tracing::info!(log = %log.display(), "supervisor started in the background");
    Ok(())
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let Some(_lock) = single_instance(&opts.state_dir, &opts.args)? else {
        tracing::info!(args = ?opts.args, "already running under another supervisor; nothing to do");
        return Ok(());
    };
    // The binary children run: `--exe` (a launcher keeps itself), else this one.
    let exe = opts.exe.clone().unwrap_or_else(|| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kiln")));
    let takes = takes_upgrade(&opts.args);
    let signals = Signals::new(&[crate::rt::SIGTERM, crate::rt::SIGINT, crate::rt::SIGHUP])?;
    let mut backoff = MIN_BACKOFF;
    let mut generation: u64 = 0;
    // A previous image of this supervisor re-executed itself into a new binary and left us its
    // running child ("<pid>:<generation>"): take it over instead of starting another.
    let mut inherited: Option<(u32, u64)> = std::env::var("KILN_SUPERVISE_CHILD").ok().and_then(|v| {
        let (pid, generation) = v.split_once(':')?;
        Some((pid.parse().ok()?, generation.parse().ok()?))
    });
    if inherited.is_some() && takes == Takes::Successor {
        // An older kiln re-executed this supervisor in place, which on macOS can cut its whole
        // process tree off from the local network; the runner has launchd start it afresh once
        // it is idle.
        tracing::warn!("an older kiln re-executed this supervisor in place; the runner has launchd start it afresh once it is idle");
    }
    // Terminals the last runner process handed over, for the next one.
    let mut handed: Option<crate::os::supervise::Terminals> = None;

    loop {
        generation += 1;
        let started_mtime = exe_mtime(&exe);
        let adopted = inherited
            .take()
            .filter(|(pid, _)| crate::os::pid_alive(*pid))
            .and_then(|(pid, g)| ChildControl::adopt(pid).map(|c| (pid, g, c)));
        let mut handover = None;
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
            if takes == Takes::Successor {
                handover = crate::os::supervise::Handover::for_child(&mut cmd);
                if let Some(t) = &handed
                    && let Err(e) = t.pass_to(&mut cmd)
                {
                    tracing::warn!(error = %e, "cannot pass the terminals on; they end with this upgrade");
                }
            }
            match ChildControl::spawn(&mut cmd) {
                Ok((child, control)) => {
                    if let Some(h) = &mut handover {
                        h.started();
                    }
                    // The child holds them now: close this process's copies.
                    if let Some(t) = handed.take()
                        && !t.is_empty()
                    {
                        tracing::info!(terminals = t.len(), "terminals handed to the new runner process");
                    }
                    (spawn_waiter(child), control)
                }
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
            Child(Ended),
            Shutdown(&'static str),
            Upgrade,
        }
        enum Ev {
            Exited(Ended),
            Signal(Option<i32>),
            UpgradeCheck,
        }
        let mut noticed = false;
        let exit = loop {
            let ev = futures::select_biased! {
                st = (&mut exited).fuse() => Ev::Exited(st.unwrap_or_else(|_| (None, "waiter lost".to_owned()))),
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
                        match takes {
                            // The runner takes it up once idle, as a fresh process started here.
                            Takes::Successor => {
                                if !noticed {
                                    noticed = true;
                                    tracing::info!("binary changed on disk; the runner takes it up as a fresh process once it is idle");
                                }
                            }
                            Takes::InPlace => {
                                if let Some(pid) = control.pid() {
                                    // The child takes up the new binary itself (keeping its
                                    // terminals); this supervisor does the same and keeps
                                    // supervising it.
                                    tracing::info!(pid, "binary changed on disk; the child upgrades itself in place, re-executing the supervisor");
                                    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
                                    let err = crate::os::reexec(&exe, &args, &[("KILN_SUPERVISE_CHILD", format!("{pid}:{generation}"))]);
                                    tracing::error!(error = %err, "supervisor re-exec failed; restarting the child instead");
                                }
                                break Exit::Upgrade;
                            }
                            Takes::Restart => break Exit::Upgrade,
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
            Exit::Child((code, _)) if takes == Takes::Successor && code == Some(i32::from(HANDOVER_EXIT)) => {
                // The runner stopped to take up the new binary, its terminals handed over.
                handed = match handover {
                    Some(h) => h.finish().await,
                    None => None,
                };
                tracing::info!(
                    generation,
                    terminals = handed.as_ref().map_or(0, crate::os::supervise::Terminals::len),
                    "the runner stopped to take up the new binary; starting it as a fresh process"
                );
                backoff = MIN_BACKOFF;
            }
            Exit::Child((_, status)) => {
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

async fn stop_child(exited: &mut oneshot::Receiver<Ended>, control: &mut ChildControl) {
    control.stop();
    if crate::rt::timeout(GRACE, &mut *exited).await.is_err() {
        tracing::warn!("child ignored the stop request for {GRACE:?}; killing");
        control.kill().await;
        let _ = crate::rt::timeout(Duration::from_secs(5), exited).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_children_take_up_a_new_binary() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(takes_upgrade(&args(&["server"])), Takes::Restart);
        // A runner keeps its terminals: in place on Linux, as a fresh process on macOS (where a
        // process that re-executed itself loses the local network), restarted on Windows.
        let runner = if cfg!(target_os = "macos") {
            Takes::Successor
        } else if cfg!(unix) {
            Takes::InPlace
        } else {
            Takes::Restart
        };
        assert_eq!(takes_upgrade(&args(&["--config=/c.toml", "agent", "--name", "x"])), runner);
    }
}
