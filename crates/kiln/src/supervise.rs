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

/// What the supervisor runs as its child: a binary and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ChildCmd {
    exe: PathBuf,
    args: Vec<String>,
}

/// A new build started beside the child for a blue/green upgrade (`crate::bluegreen`): it becomes
/// the child once promoted and the old one has ended.
struct Candidate {
    pid: u32,
    control: ChildControl,
    exited: oneshot::Receiver<String>,
    cmd: ChildCmd,
    promoted: bool,
}

fn key_of(args: &[String]) -> String {
    let key = crate::util::sha256_hex(args.join(" ").as_bytes());
    key.get(..12).unwrap_or("x").to_owned()
}

/// Where the build a runner was last promoted to is kept: started again after this supervisor is.
fn current_path(state_dir: &Path, args: &[String]) -> PathBuf {
    state_dir.join(format!("supervise-{}.json", key_of(args)))
}

/// Where the child asks this supervisor to start, promote or stop a new build: in the user's
/// runtime directory (Unix socket paths are short), named after the state directory and child.
pub fn control_path(state_dir: &Path, args: &[String]) -> PathBuf {
    let tag = crate::util::sha256_hex(format!("{}\n{}", state_dir.display(), args.join(" ")).as_bytes());
    crate::harness::socket::runtime_path(&format!("supervise-{}.sock", tag.get(..16).unwrap_or("0")))
}

fn spawn_cmd(c: &ChildCmd, generation: u64, control: Option<&Path>, env: &[(String, String)]) -> std::process::Command {
    let mut cmd = std::process::Command::new(&c.exe);
    cmd.args(&c.args).env("KILN_SUPERVISED", "1").env("KILN_GENERATION", generation.to_string()).env_remove("KILN_SUPERVISE_CHILD");
    if let Some(p) = control {
        cmd.env(crate::bluegreen::SUPERVISOR_ENV, p);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
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
    // The child it runs: its own binary, or the build a runner was last promoted to.
    let own = ChildCmd { exe: exe.clone(), args: opts.args.clone() };
    let saved = current_path(&opts.state_dir, &opts.args);
    let mut current = std::fs::read(&saved).ok().and_then(|b| serde_json::from_slice::<ChildCmd>(&b).ok()).filter(|c| c.exe.is_file()).unwrap_or_else(|| own.clone());
    let control_at = control_path(&opts.state_dir, &opts.args);
    let ctl = control::listen(&control_at).map_err(|e| tracing::warn!(error = %format!("{e:#}"), "no control socket: upgrades of the child aren't blue/green")).ok();
    let ctl_path = ctl.as_ref().map(|_| control_at.as_path());
    let mut candidate: Option<Candidate> = None;
    // When a promoted build that died was last started again, and how many times in a row the
    // build the child was promoted to ended soon after it started.
    let mut respawned: Option<Instant> = None;
    let mut quick_ends = 0u32;

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
            let mut cmd = spawn_cmd(&current, generation, ctl_path, &[]);
            match ChildControl::spawn(&mut cmd) {
                Ok((child, control)) => (spawn_waiter(child), control),
                Err(e) => {
                    tracing::error!(error = %e, exe = %current.exe.display(), "spawn failed");
                    crash_log(&opts.state_dir, &format!("spawn failed: {e}"));
                    // A promoted build that can't start: back to this supervisor's own.
                    if current != own {
                        current = own.clone();
                        let _ = std::fs::remove_file(&saved);
                    }
                    crate::rt::sleep(crate::sup::jitter(backoff)).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        };
        tracing::info!(generation, pid = ?control.pid(), exe = %current.exe.display(), "child started");
        let mut started = Instant::now();
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
            CandidateExited(String),
            Control(Option<control::Stream>),
        }
        let exit = loop {
            let ev = futures::select_biased! {
                st = (&mut exited).fuse() => Ev::Exited(st.unwrap_or_else(|_| "waiter lost".to_owned())),
                sig = signals.next().fuse() => Ev::Signal(sig),
                st = candidate_exit(&mut candidate).fuse() => Ev::CandidateExited(st),
                c = control::accept(ctl.as_ref()).fuse() => Ev::Control(c),
                () = upgrade_check.tick().fuse() => Ev::UpgradeCheck,
            };
            match ev {
                Ev::Exited(status) => {
                    // The old runner ended after handing everything to the new build: it is the
                    // child from now on.
                    if candidate.as_ref().is_some_and(|c| c.promoted)
                        && let Some(c) = candidate.take()
                    {
                        tracing::info!(pid = c.pid, %status, "the old runner ended; the new build is the runner now");
                        control = c.control;
                        exited = c.exited;
                        current = c.cmd;
                        started = Instant::now();
                        stale_since = None;
                        continue;
                    }
                    // It ended mid-upgrade: the new build, not ready, goes too.
                    if let Some(mut c) = candidate.take() {
                        tracing::warn!(pid = c.pid, "the runner ended before its new build took over: stopping that");
                        stop_child(&mut c.exited, &mut c.control).await;
                    }
                    break Exit::Child(status);
                }
                Ev::CandidateExited(status) => {
                    if let Some(c) = candidate.take() {
                        let msg = format!("the new build's runner (pid {}) exited ({status}){}", c.pid, if c.promoted { " after it took over" } else { " before it took over" });
                        tracing::warn!("{msg}");
                        crash_log(&opts.state_dir, &msg);
                        // Promoted, it holds the machine's sessions: started again at once (it
                        // takes them up from its own records), unless it already was within the
                        // minute. Then it is started once the old runner ends, as the child.
                        if c.promoted {
                            let again = respawned.is_none_or(|t: Instant| t.elapsed() >= Duration::from_secs(60));
                            match again.then(|| ChildControl::spawn(&mut spawn_cmd(&c.cmd, generation, ctl_path, &[]))) {
                                Some(Ok((child, control))) => {
                                    respawned = Some(Instant::now());
                                    let pid = child.id();
                                    tracing::info!(pid, "the promoted build started again");
                                    candidate = Some(Candidate { pid, control, exited: spawn_waiter(child), cmd: c.cmd, promoted: true });
                                }
                                other => {
                                    if let Some(Err(e)) = other {
                                        tracing::error!(error = %e, "the promoted build couldn't start again");
                                    }
                                    current = c.cmd;
                                }
                            }
                        }
                    }
                }
                Ev::Control(Some(stream)) => {
                    let req = stream.request().await;
                    let answer = match req {
                        Ok(r) => handle(r, &mut candidate, &saved, generation, ctl_path).await,
                        Err(e) => serde_json::json!({ "ok": false, "error": format!("{e:#}") }),
                    };
                    stream.answer(&answer).await;
                }
                Ev::Control(None) => {}
                Ev::Signal(Some(crate::rt::SIGHUP)) => {
                    // HUP = reload: forward to the child, which re-reads its config.
                    control.reload();
                }
                Ev::Signal(Some(crate::rt::SIGINT)) => break Exit::Shutdown("interrupt"),
                Ev::Signal(_) => break Exit::Shutdown("terminate"),
                // Runs a promoted build (blue/green): its own binary changing on disk is nothing to it.
                Ev::UpgradeCheck if current != own => {}
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
                if let Some(mut c) = candidate.take() {
                    stop_child(&mut c.exited, &mut c.control).await;
                }
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
                    quick_ends = 0;
                } else if current != own {
                    quick_ends += 1;
                    if quick_ends >= 3 {
                        let msg = format!("the build the runner was upgraded to ({}) ended {quick_ends} times soon after it started: back to {}", current.exe.display(), own.exe.display());
                        tracing::error!("{msg}");
                        crash_log(&opts.state_dir, &msg);
                        current = own.clone();
                        let _ = std::fs::remove_file(&saved);
                        quick_ends = 0;
                    }
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

/// The candidate's end, when there is one (never, else).
async fn candidate_exit(c: &mut Option<Candidate>) -> String {
    match c {
        Some(c) => (&mut c.exited).await.unwrap_or_else(|_| "waiter lost".to_owned()),
        None => std::future::pending().await,
    }
}

/// What the child asks of its supervisor (`crate::bluegreen::Supervisor`).
#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Start {
        exe: PathBuf,
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    Promote {
        pid: u32,
    },
    Stop {
        pid: u32,
        /// At once (SIGKILL), not asked to shut down: a rollback.
        #[serde(default)]
        kill: bool,
    },
}

async fn handle(r: Request, candidate: &mut Option<Candidate>, saved: &Path, generation: u64, ctl: Option<&Path>) -> serde_json::Value {
    use serde_json::json;
    match r {
        Request::Start { exe, args, env } => {
            if let Some(c) = candidate {
                return json!({ "ok": false, "error": format!("a new build is already starting (pid {})", c.pid) });
            }
            let cmd = ChildCmd { exe, args };
            let mut spawn = spawn_cmd(&cmd, generation, ctl, &env);
            match ChildControl::spawn(&mut spawn) {
                Ok((child, control)) => {
                    let pid = child.id();
                    tracing::info!(pid, exe = %cmd.exe.display(), "a new build started beside the runner");
                    *candidate = Some(Candidate { pid, control, exited: spawn_waiter(child), cmd, promoted: false });
                    json!({ "ok": true, "pid": pid })
                }
                Err(e) => json!({ "ok": false, "error": format!("couldn't start {}: {e}", cmd.exe.display()) }),
            }
        }
        Request::Promote { pid } => match candidate {
            Some(c) if c.pid == pid => {
                c.promoted = true;
                let written = serde_json::to_vec(&c.cmd).map_err(std::io::Error::other).and_then(|b| crate::util::write_secret(saved, &b, 0o600));
                if let Err(e) = written {
                    tracing::warn!(error = %e, "couldn't remember the promoted build: a restart of the supervisor starts its own");
                }
                tracing::info!(pid, "the new build is promoted: it is the runner once the old one ends");
                json!({ "ok": true })
            }
            _ => json!({ "ok": false, "error": format!("no new build with pid {pid}") }),
        },
        Request::Stop { pid, kill } => match candidate.take() {
            Some(mut c) if c.pid == pid => {
                if kill {
                    c.control.kill().await;
                    let _ = crate::rt::timeout(Duration::from_secs(5), &mut c.exited).await;
                } else {
                    stop_child(&mut c.exited, &mut c.control).await;
                }
                tracing::info!(pid, kill, "the new build was stopped: the runner goes on as it was");
                json!({ "ok": true })
            }
            other => {
                *candidate = other;
                json!({ "ok": false, "error": format!("no new build with pid {pid}") })
            }
        },
    }
}

/// The control socket (Unix only: elsewhere upgrades of the child aren't blue/green).
mod control {
    use std::path::Path;

    #[cfg(unix)]
    pub struct Listener(std::os::unix::net::UnixListener);
    #[cfg(not(unix))]
    pub struct Listener;

    #[cfg(unix)]
    pub struct Stream(std::os::unix::net::UnixStream);
    #[cfg(not(unix))]
    pub struct Stream;

    #[cfg(unix)]
    pub fn listen(path: &Path) -> anyhow::Result<Listener> {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::remove_file(path);
        let l = std::os::unix::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        l.set_nonblocking(true)?;
        Ok(Listener(l))
    }

    #[cfg(not(unix))]
    pub fn listen(_path: &Path) -> anyhow::Result<Listener> {
        anyhow::bail!("no Unix sockets here")
    }

    /// The next connection (`None` on a failed accept); never, without a listener.
    #[cfg(unix)]
    pub async fn accept(l: Option<&Listener>) -> Option<Stream> {
        use std::os::fd::AsRawFd as _;
        let Some(Listener(l)) = l else { return std::future::pending().await };
        loop {
            match l.accept() {
                Ok((s, _)) => return Some(Stream(s)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if hyper_rt::readiness::readable(l.as_raw_fd()).await.is_err() {
                        crate::rt::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "supervisor control accept failed");
                    crate::rt::sleep(std::time::Duration::from_millis(500)).await;
                    return None;
                }
            }
        }
    }

    #[cfg(not(unix))]
    pub async fn accept(_l: Option<&Listener>) -> Option<Stream> {
        std::future::pending().await
    }

    impl Stream {
        /// Its one request: a JSON line.
        #[cfg(unix)]
        pub async fn request(&self) -> anyhow::Result<super::Request> {
            let s = self.0.try_clone()?;
            let line = crate::rt::blocking(move || -> anyhow::Result<String> {
                use std::io::BufRead as _;
                s.set_nonblocking(false)?;
                s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
                let mut line = String::new();
                std::io::BufReader::new(s).read_line(&mut line)?;
                Ok(line)
            })
            .await??;
            Ok(serde_json::from_str(&line)?)
        }

        #[cfg(not(unix))]
        pub async fn request(&self) -> anyhow::Result<super::Request> {
            anyhow::bail!("no Unix sockets here")
        }

        #[cfg(unix)]
        pub async fn answer(self, v: &serde_json::Value) {
            let mut line = v.to_string();
            line.push('\n');
            let _ = crate::rt::blocking(move || {
                use std::io::Write as _;
                let mut s = self.0;
                let _ = s.set_nonblocking(false);
                s.write_all(line.as_bytes())
            })
            .await;
        }

        #[cfg(not(unix))]
        pub async fn answer(self, _v: &serde_json::Value) {}
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
    use anyhow::Context as _;

    #[test]
    fn a_new_build_is_started_promoted_and_killed_as_the_runner_asks() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let saved = dir.path().join("supervise-x.json");
            let mut candidate = None;
            let ask = |json: &str| serde_json::from_str::<Request>(json).map_err(anyhow::Error::from);
            let start = ask(r#"{"op":"start","exe":"/bin/sleep","args":["60"],"env":[["KILN_TAKE_OVER_FROM","/x.sock"]]}"#)?;
            let v = handle(start, &mut candidate, &saved, 1, None).await;
            let pid = v.get("pid").and_then(serde_json::Value::as_u64).and_then(|p| u32::try_from(p).ok()).context("no pid")?;
            assert!(crate::os::pid_alive(pid));
            // One at a time.
            let again = handle(ask(r#"{"op":"start","exe":"/bin/sleep","args":["60"]}"#)?, &mut candidate, &saved, 1, None).await;
            assert_eq!(again.get("ok"), Some(&serde_json::Value::Bool(false)));
            // Only the process it started is promoted; that is remembered for the supervisor's next start.
            let wrong = handle(ask(&format!(r#"{{"op":"promote","pid":{}}}"#, pid + 1))?, &mut candidate, &saved, 1, None).await;
            assert_eq!(wrong.get("ok"), Some(&serde_json::Value::Bool(false)));
            let ok = handle(ask(&format!(r#"{{"op":"promote","pid":{pid}}}"#))?, &mut candidate, &saved, 1, None).await;
            assert_eq!(ok.get("ok"), Some(&serde_json::Value::Bool(true)));
            let kept: ChildCmd = serde_json::from_slice(&std::fs::read(&saved)?)?;
            assert_eq!(kept, ChildCmd { exe: "/bin/sleep".into(), args: vec!["60".into()] });
            // A rollback kills it outright.
            let stop = ask(&format!(r#"{{"op":"stop","pid":{pid},"kill":true}}"#))?;
            assert!(matches!(stop, Request::Stop { kill: true, .. }));
            let v = handle(stop, &mut candidate, &saved, 1, None).await;
            assert_eq!(v.get("ok"), Some(&serde_json::Value::Bool(true)));
            assert!(candidate.is_none() && !crate::os::pid_alive(pid));
            assert!(matches!(ask(r#"{"op":"stop","pid":1}"#)?, Request::Stop { kill: false, .. }));
            Ok(())
        })
    }

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
