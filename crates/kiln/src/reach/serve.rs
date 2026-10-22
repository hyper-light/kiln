//! Running a command another runner asked for (`docs/reach.md` §4.2).
//!
//! A command is shell text, run by bash (sh where there is none) in its own process group, with
//! the environment a login shell of this machine's user has (their PATH: cargo, go, node...),
//! resolved once and kept a few minutes. A named shell carries its working folder and exported
//! variables from one run to the next, the way one terminal would.
//!
//! What a run says comes back as [`Event`]s in order: started, output (stdout and stderr each in
//! order, interleaved as they came), then its exit once both pipes are closed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::wire::{RunSpec, Signal, Stream};

/// What a run says, in order.
#[derive(Debug)]
pub enum Event {
    Started { pid: u32, cwd: String },
    Output { stream: Stream, data: Vec<u8> },
    Exit { code: Option<i32>, error: Option<String> },
}

/// How long the resolved login environment is kept before it is asked for again.
const ENV_TTL: Duration = Duration::from_secs(600);
/// How long asking a login shell for its environment may take.
const ENV_TIMEOUT: Duration = Duration::from_secs(8);

static LOGIN_ENV: OnceLock<Mutex<Option<(Instant, Vec<(String, String)>)>>> = OnceLock::new();

/// The user's login shell: `$SHELL`, else the passwd entry's, else bash.
fn login_shell() -> String {
    #[cfg(unix)]
    {
        if let Some(s) = std::env::var("SHELL").ok().filter(|s| Path::new(s).exists()) {
            return s;
        }
        if let Ok(Some(u)) = nix::unistd::User::from_uid(nix::unistd::getuid()) {
            let s = u.shell.to_string_lossy().into_owned();
            if Path::new(&s).exists() {
                return s;
            }
        }
    }
    crate::terminal::default_shell(None)
}

/// Parse `env` output: `NAME=value` lines; a line that does not start a variable continues the
/// previous value (a value with newlines).
fn parse_env(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let starts = line.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit()));
        match (starts, line.split_once('=')) {
            (true, Some((k, v))) => out.push((k.to_owned(), v.to_owned())),
            _ => {
                if let Some((_, v)) = out.last_mut() {
                    v.push('\n');
                    v.push_str(line);
                }
            }
        }
    }
    out
}

/// Variables a login shell sets that make no sense to hand on (they describe that shell).
const SKIP_ENV: &[&str] = &["_", "SHLVL", "PWD", "OLDPWD", "PS1", "PS2", "TERM_SESSION_ID", "KILN_TERM_SESSION"];

/// The ssh agent a login shell's environment names (`SSH_AUTH_SOCK`, `SSH_AGENT_PID`), while its
/// socket is there.
fn agent_of(env: &[(String, String)]) -> Vec<(String, String)> {
    let alive = env.iter().any(|(k, v)| k == "SSH_AUTH_SOCK" && Path::new(v).exists());
    env.iter().filter(|(k, _)| alive && (k == "SSH_AUTH_SOCK" || k == "SSH_AGENT_PID")).cloned().collect()
}

/// Ask the user's login shell for its environment (blocking), handing it `agent`, the ssh agent an
/// earlier answer named: a startup file that starts an agent unless one is set (fish's and bash's
/// often do) starts none again, where each answer would otherwise leave one more running.
fn ask_login_shell(shell: &str, agent: Vec<(String, String)>) -> Option<Vec<(String, String)>> {
    if cfg!(windows) {
        return None;
    }
    const BEGIN: &str = "__KILN_REACH_ENV_BEGIN__";
    const END: &str = "__KILN_REACH_ENV_END__";
    let script = format!("printf '\\n{BEGIN}\\n'; env; printf '\\n{END}\\n'");
    let mut cmd = std::process::Command::new(shell);
    cmd.args(["-l", "-i", "-c", &script]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    cmd.current_dir(crate::os::home_dir()).envs(agent);
    // A second early: process start times are whole seconds.
    let since = unix_s().saturating_sub(1);
    let mut child = cmd.spawn().ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > ENV_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(%shell, "the login shell took too long to say its environment; using the runner's");
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    let body = out.split_once(&format!("\n{BEGIN}\n"))?.1.split_once(&format!("\n{END}\n"))?.0;
    let env: Vec<(String, String)> = parse_env(body).into_iter().filter(|(k, _)| !SKIP_ENV.contains(&k.as_str())).collect();
    (!env.is_empty()).then(|| without_its_agent(env, since))
}

/// Seconds since the epoch, as process start times are given.
fn unix_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The environment a login shell said, without the `ssh-agent` its startup files started for it
/// alone. `eval "$(ssh-agent -s)"` (fish: `eval (ssh-agent -c)`) starts one in every shell that
/// reads the file, or every one given none, and it runs on after the shell: asked every few
/// minutes, the shell left one each time, for good (hundreds on a machine). It's stopped, and runs
/// get the agent this process has, or none, rather than the stopped one's socket. An agent the
/// shell found running (started before `since`, kept in a file) is left as it is.
fn without_its_agent(mut env: Vec<(String, String)>, since: u64) -> Vec<(String, String)> {
    let pid_of = |v: &str| v.trim().parse::<u32>().ok();
    let theirs = env.iter().find(|(k, _)| k == "SSH_AGENT_PID").and_then(|(_, v)| pid_of(v));
    let ours = std::env::var("SSH_AGENT_PID").ok().and_then(|v| pid_of(&v));
    let Some(pid) = theirs.filter(|p| Some(*p) != ours && started_agent(*p, since)) else { return env };
    crate::harness::survivors::stop(&[pid]);
    env.retain(|(k, _)| k != "SSH_AGENT_PID" && k != "SSH_AUTH_SOCK");
    env.extend(["SSH_AUTH_SOCK", "SSH_AGENT_PID"].into_iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_owned(), v))));
    env
}

/// Whether `pid` is an `ssh-agent` that started at `since` or later, and runs.
fn started_agent(pid: u32, since: u64) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), true, ProcessRefreshKind::nothing());
    sys.process(p).is_some_and(|p| p.status() != sysinfo::ProcessStatus::Zombie && p.name().to_string_lossy().starts_with("ssh-agent") && p.start_time() >= since)
}

/// The environment runs get: the login shell's (kept [`ENV_TTL`]), else this process's. One shell
/// is asked at a time: runs that start while it answers take its answer.
pub async fn login_env() -> Vec<(String, String)> {
    static ASKING: OnceLock<futures::lock::Mutex<()>> = OnceLock::new();
    let cell = LOGIN_ENV.get_or_init(|| Mutex::new(None));
    let kept = || cell.lock().as_ref().filter(|(at, _)| at.elapsed() < ENV_TTL).map(|(_, env)| env.clone());
    if let Some(env) = kept() {
        return env;
    }
    let _asking = ASKING.get_or_init(|| futures::lock::Mutex::new(())).lock().await;
    if let Some(env) = kept() {
        return env;
    }
    let agent = cell.lock().as_ref().map(|(_, env)| agent_of(env)).unwrap_or_default();
    let env = crate::rt::blocking(move || ask_login_shell(&login_shell(), agent)).await.ok().flatten().unwrap_or_else(|| std::env::vars().collect());
    *cell.lock() = Some((Instant::now(), env.clone()));
    env
}

/// Where a run starts. `mirror_root`: the synced copy it runs in, when it names one.
pub fn resolve_cwd(spec: &RunSpec, mirror_root: Option<&Path>) -> anyhow::Result<PathBuf> {
    let home = crate::os::home_dir();
    let cwd = spec.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let path = match (mirror_root, cwd) {
        (Some(root), None) => root.to_path_buf(),
        (Some(root), Some(rel)) => {
            let rel = Path::new(rel);
            anyhow::ensure!(rel.is_relative() && !rel.components().any(|c| matches!(c, std::path::Component::ParentDir)), "a folder inside a synced copy is relative and stays inside it");
            root.join(rel)
        }
        (None, None) => home,
        (None, Some("~")) => home,
        (None, Some(c)) => match c.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(c),
        },
    };
    anyhow::ensure!(path.is_dir(), "{} is not a folder on this machine", path.display());
    Ok(path)
}

/// bash where there is one, else sh. On Windows, never WSL's `bash.exe` (System32's, or the Store
/// app's alias): it runs the command in Linux, not in this machine's folder, and with no Linux
/// installed it hangs or fails. Git for Windows' bash is used instead, found on PATH, beside the
/// `git` on PATH or where it installs; without one, the run fails at once and says so.
fn bash() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        windows_bash().ok_or_else(|| "this Windows machine has no bash kiln can run commands with: install Git for Windows (WSL's bash.exe would run them in Linux, not here)".to_owned())
    }
    #[cfg(not(windows))]
    {
        Ok(crate::os::which("bash").unwrap_or_else(|| PathBuf::from("sh")))
    }
}

/// Whether `path` is WSL's launcher rather than a bash of this machine: `bash.exe` in System32 (or
/// SysWOW64), or the Store app's alias under WindowsApps.
#[cfg(any(windows, test))]
fn wsl_launcher(path: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase().replace('/', "\\");
    ["\\windows\\system32\\", "\\windows\\syswow64\\", "\\windows\\sysnative\\", "\\windowsapps\\"].iter().any(|d| p.contains(d))
}

/// Where Git for Windows' bash may be, given the `git` on PATH (`<Git>\cmd\git.exe`,
/// `<Git>\bin\git.exe`, `<Git>\mingw64\bin\git.exe`) and the folders Git installs under.
#[cfg(any(windows, test))]
fn git_bash_places(git: Option<&Path>, install_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut places: Vec<PathBuf> = Vec::new();
    for up in git.into_iter().flat_map(|g| g.ancestors().skip(1).take(3)) {
        places.push(up.join("bin").join("bash.exe"));
        places.push(up.join("usr").join("bin").join("bash.exe"));
    }
    for root in install_roots {
        places.push(root.join("Git").join("bin").join("bash.exe"));
    }
    places
}

#[cfg(windows)]
fn windows_bash() -> Option<PathBuf> {
    if let Some(p) = crate::os::which("bash").filter(|p| !wsl_launcher(p)) {
        return Some(p);
    }
    let mut roots: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs"));
    }
    git_bash_places(crate::os::which("git").as_deref(), &roots).into_iter().find(|p| p.is_file() && !wsl_launcher(p))
}

/// The script a run's shell is given: the command, and for a named shell, its folder and
/// variables put back first and saved again on the way out (whatever way out).
fn script(command: &str, shell_state: Option<&Path>, start_in_saved: bool) -> String {
    match shell_state {
        None => command.to_owned(),
        Some(_) => {
            let restore_cwd = if start_in_saved { "if [ -f \"$KILN_REACH_SHELL/cwd\" ]; then cd \"$(cat \"$KILN_REACH_SHELL/cwd\")\" 2>/dev/null || true; fi\n" } else { "" };
            format!(
                "{restore_cwd}if [ -f \"$KILN_REACH_SHELL/env\" ]; then . \"$KILN_REACH_SHELL/env\" 2>/dev/null || true; fi\n\
                 trap 'pwd > \"$KILN_REACH_SHELL/cwd\"; export -p | grep -v \" KILN_REACH_SHELL=\" > \"$KILN_REACH_SHELL/env\"' EXIT\n\
                 {command}"
            )
        }
    }
}

/// Send `signal` to a run's process group.
pub fn signal_group(pid: u32, signal: Signal) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal as S, killpg};
        let Ok(pgid) = i32::try_from(pid) else { return };
        if pgid <= 0 {
            return;
        }
        let s = match signal {
            Signal::Interrupt => S::SIGINT,
            Signal::Terminate => S::SIGTERM,
            Signal::Kill => S::SIGKILL,
        };
        let _ = killpg(nix::unistd::Pid::from_raw(pgid), s);
    }
    #[cfg(windows)]
    {
        let _ = signal;
        let _ = std::process::Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).output();
    }
}

/// How long what a finished run left running gets to end, once asked, before it's killed.
const LEFTOVER_GRACE: Duration = Duration::from_millis(500);

/// What a finished run left running ends: asked to, then killed if it hasn't within
/// [`LEFTOVER_GRACE`]. Where the machine holds the run's processes together, that's everything in
/// its group, however parented (a daemon, `setsid`), and nothing moved out of it: a unit of the
/// machine's own (`systemd-run --user`) is a job, and stays. Elsewhere, its process group. A run
/// that left nothing ends at once.
fn end_leftovers(pgid: u32, group: Option<crate::os::contain::Group>) {
    match group {
        Some(g) => {
            if g.pids().is_empty() {
                g.remove();
                return;
            }
            #[cfg(unix)]
            for p in g.pids() {
                if let Ok(p) = i32::try_from(p) {
                    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGTERM);
                }
            }
            let until = Instant::now() + LEFTOVER_GRACE;
            while !g.pids().is_empty() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            g.kill();
            for _ in 0..20 {
                if g.pids().is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            g.remove();
        }
        None => {
            if !group_alive(pgid) {
                return;
            }
            signal_group(pgid, Signal::Terminate);
            let until = Instant::now() + LEFTOVER_GRACE;
            while group_alive(pgid) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            signal_group(pgid, Signal::Kill);
        }
    }
}

/// Whether anything is still in process group `pgid`.
fn group_alive(pgid: u32) -> bool {
    #[cfg(unix)]
    {
        i32::try_from(pgid).is_ok_and(|p| p > 0 && nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), None).is_ok())
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// A run its asker went away from: all of it ends at once.
fn end_now(pgid: u32, group: Option<crate::os::contain::Group>) {
    signal_group(pgid, Signal::Kill);
    if let Some(g) = group {
        crate::rt::spawn(async move {
            let _ = crate::rt::blocking(move || {
                g.kill();
                std::thread::sleep(Duration::from_millis(200));
                g.remove();
            })
            .await;
        });
    }
}

/// Start `spec`; its events go to `events` in order, ending with Exit. `extra_env` comes last
/// (who asked, the shell's state folder). Returns at once; a failure to start is an Exit with
/// an error.
pub fn start(spec: RunSpec, cwd: PathBuf, shell_state: Option<PathBuf>, extra_env: Vec<(String, String)>, events: async_channel::Sender<Event>) -> async_channel::Sender<Vec<u8>> {
    let (stdin_tx, stdin_rx) = async_channel::unbounded::<Vec<u8>>();
    crate::rt::spawn(async move {
        let env = login_env().await;
        if let Some(dir) = &shell_state
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            let _ = events.send(Event::Exit { code: None, error: Some(format!("cannot keep the shell's state in {}: {e}", dir.display())) }).await;
            return;
        }
        // A named shell starts where it was left, unless the run says where.
        let start_in_saved = shell_state.is_some() && spec.cwd.is_none();
        let shell = match bash() {
            Ok(b) => b,
            Err(e) => {
                let _ = events.send(Event::Exit { code: None, error: Some(e) }).await;
                return;
            }
        };
        let mut cmd = std::process::Command::new(shell);
        cmd.arg("-c").arg(script(&spec.command, shell_state.as_deref(), start_in_saved));
        cmd.current_dir(&cwd);
        cmd.env_clear();
        cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if let Some(dir) = &shell_state {
            cmd.env("KILN_REACH_SHELL", dir);
        }
        // A group of its own where this machine holds processes together (`crate::os::contain`):
        // what it leaves running ends with it, however that's parented (a daemon, `setsid`).
        let group = crate::os::contain::Group::of(&format!("run-{}", uuid::Uuid::now_v7().simple()));
        let duplex = match crate::os::proc::spawn_duplex_in(cmd, group.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                if let Some(g) = group {
                    g.remove();
                }
                let _ = events.send(Event::Exit { code: None, error: Some(format!("{e:#}")) }).await;
                return;
            }
        };
        // (Windows' has more: its job object.)
        let crate::os::proc::Duplex { pid, stdin, stdout, stderr, exit, .. } = duplex;
        if events.send(Event::Started { pid, cwd: cwd.to_string_lossy().into_owned() }).await.is_err() {
            end_now(pid, group);
            return;
        }
        // Input from the asker until it says the end (or the run ends).
        if spec.stdin {
            crate::rt::spawn(async move {
                while let Ok(data) = stdin_rx.recv().await {
                    if data.is_empty() || stdin.send(data).await.is_err() {
                        break;
                    }
                }
            });
        } else {
            drop(stdin);
            drop(stdin_rx);
        }
        let (mut out_open, mut err_open) = (true, true);
        while out_open || err_open {
            use futures::FutureExt as _;
            let next = futures::select_biased! {
                d = stdout.recv().fuse() => (Stream::Out, d.ok()),
                d = stderr.recv().fuse() => (Stream::Err, d.ok()),
            };
            match next {
                (Stream::Out, None) => {
                    out_open = false;
                    // Only stderr is left: wait on it alone.
                    if err_open {
                        while let Ok(data) = stderr.recv().await {
                            if events.send(Event::Output { stream: Stream::Err, data }).await.is_err() {
                                end_now(pid, group);
                                return;
                            }
                        }
                        err_open = false;
                    }
                }
                (Stream::Err, None) => {
                    err_open = false;
                    if out_open {
                        while let Ok(data) = stdout.recv().await {
                            if events.send(Event::Output { stream: Stream::Out, data }).await.is_err() {
                                end_now(pid, group);
                                return;
                            }
                        }
                        out_open = false;
                    }
                }
                (stream, Some(data)) => {
                    if events.send(Event::Output { stream, data }).await.is_err() {
                        end_now(pid, group);
                        return;
                    }
                }
            }
        }
        let code = exit.await.ok().flatten();
        // What it left running (`nohup … &`, a daemon) ends with it, before it's said to have ended.
        let _ = crate::rt::blocking(move || end_leftovers(pid, group)).await;
        let _ = events.send(Event::Exit { code, error: None }).await;
    });
    stdin_tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn what_a_run_leaves_running_ends_with_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pid_file = dir.path().join("left.pid");
            let spec = RunSpec { id: 9, command: format!("sleep 300 >/dev/null 2>&1 & echo $! > {}; echo started", pid_file.display()), ..RunSpec::default() };
            let (out, _, code, error) = run(spec, dir.path(), None).await?;
            assert_eq!((String::from_utf8_lossy(&out).trim(), code, error), ("started", Some(0), None));
            let left: i32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
            // Gone by the time the run is said to have ended (or a moment after, reaped).
            for _ in 0..50 {
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(left), None).is_err() {
                    return Ok(());
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            anyhow::bail!("the run left {left} running")
        })
    }

    #[cfg(unix)]
    #[test]
    fn the_ssh_agent_a_login_shell_started_is_stopped_and_not_handed_on() -> anyhow::Result<()> {
        let Ok(out) = std::process::Command::new("ssh-agent").arg("-s").stdin(std::process::Stdio::null()).output() else {
            eprintln!("skipped: no ssh-agent here");
            return Ok(());
        };
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        let value = |k: &str| said.split([';', '\n']).find_map(|p| p.trim().strip_prefix(&format!("{k}=")).map(str::to_owned));
        let (Some(sock), Some(pid)) = (value("SSH_AUTH_SOCK"), value("SSH_AGENT_PID")) else { anyhow::bail!("ssh-agent said {said:?}") };
        let n: u32 = pid.parse()?;
        let env = vec![("PATH".to_owned(), "/bin".to_owned()), ("SSH_AUTH_SOCK".to_owned(), sock.clone()), ("SSH_AGENT_PID".to_owned(), pid.clone())];
        // One that ran before the shell was asked: the shell found it, and it stays.
        assert_eq!(without_its_agent(env.clone(), u64::MAX), env);
        assert!(started_agent(n, 0), "it still runs");
        // One the shell started: stopped, and neither its socket nor its pid handed on.
        let kept = without_its_agent(env, unix_s().saturating_sub(60));
        assert!(!started_agent(n, 0), "the agent still runs");
        assert!(!kept.iter().any(|(_, v)| *v == sock || *v == pid), "{kept:?}");
        assert!(kept.iter().any(|(k, v)| k == "PATH" && v == "/bin"));
        Ok(())
    }

    #[cfg(unix)]
    async fn run(spec: RunSpec, cwd: &Path, shell: Option<PathBuf>) -> anyhow::Result<(Vec<u8>, Vec<u8>, Option<i32>, Option<String>)> {
        let (tx, rx) = async_channel::unbounded();
        let stdin = start(spec, cwd.to_path_buf(), shell, vec![("KILN_REACH_FROM".into(), "mac".into())], tx);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let _keep = stdin;
        loop {
            match crate::rt::timeout(Duration::from_secs(20), rx.recv()).await {
                Ok(Ok(Event::Started { pid, .. })) => anyhow::ensure!(pid > 0),
                Ok(Ok(Event::Output { stream: Stream::Out, data })) => out.extend(data),
                Ok(Ok(Event::Output { stream: Stream::Err, data })) => err.extend(data),
                Ok(Ok(Event::Exit { code, error })) => return Ok((out, err, code, error)),
                Ok(Err(_)) => anyhow::bail!("events ended without an exit"),
                Err(_) => anyhow::bail!("timed out"),
            }
        }
    }

    /// On Windows a run's bash is never WSL's launcher (it would run the command in Linux, and with
    /// no Linux installed it hangs): Git for Windows' bash is looked for beside the git on PATH
    /// and where it installs.
    #[test]
    fn windows_runs_never_go_to_wsl_s_bash() {
        for wsl in [r"C:\Windows\System32\bash.exe", r"c:\windows\system32\bash.exe", r"C:\Windows\SysWOW64\bash.exe", r"C:\Users\ada\AppData\Local\Microsoft\WindowsApps\bash.exe"] {
            assert!(wsl_launcher(Path::new(wsl)), "{wsl}");
        }
        for git_bash in [r"C:\Program Files\Git\bin\bash.exe", r"C:\Program Files\Git\usr\bin\bash.exe", r"C:\msys64\usr\bin\bash.exe", "/usr/bin/bash"] {
            assert!(!wsl_launcher(Path::new(git_bash)), "{git_bash}");
        }
        // Paths made of parts, so they read the same on every platform.
        let git = PathBuf::from("Program Files").join("Git").join("cmd").join("git.exe");
        let places = git_bash_places(Some(&git), &[PathBuf::from("Apps")]);
        let shown: Vec<String> = places.iter().map(|p| p.to_string_lossy().replace('/', "\\")).collect();
        assert!(shown.iter().any(|p| p == r"Program Files\Git\bin\bash.exe"), "beside git's cmd folder: {shown:?}");
        assert!(shown.iter().any(|p| p == r"Apps\Git\bin\bash.exe"), "where it installs: {shown:?}");
    }

    /// The ssh agent a login shell's startup file starts goes on to the next ask, so the file
    /// starts none again: one an answer, every ten minutes, had piled up to 56 on one machine.
    #[cfg(unix)]
    #[test]
    fn a_login_shells_agent_is_handed_on() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        if std::env::var_os("SSH_AUTH_SOCK").is_some() {
            eprintln!("a_login_shells_agent_is_handed_on: skipped, this process has an agent already");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let (marks, sock) = (dir.path().join("started"), dir.path().join("agent.sock"));
        std::fs::write(&sock, "")?;
        // As fish's or bash's startup file often does: an agent, unless one is set.
        let shell = dir.path().join("shell");
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\n[ -n \"$SSH_AUTH_SOCK\" ] || {{ echo started >> '{}'; SSH_AUTH_SOCK='{}'; SSH_AGENT_PID=4242; export SSH_AUTH_SOCK SSH_AGENT_PID; }}\nexec /bin/sh -c \"$4\"\n",
                marks.display(),
                sock.display()
            ),
        )?;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))?;
        let shell = shell.to_string_lossy().into_owned();
        let first = ask_login_shell(&shell, Vec::new()).ok_or_else(|| anyhow::anyhow!("no answer"))?;
        let agent = agent_of(&first);
        assert_eq!(agent.len(), 2, "its socket and pid: {first:?}");
        let second = ask_login_shell(&shell, agent).ok_or_else(|| anyhow::anyhow!("no second answer"))?;
        assert_eq!(std::fs::read_to_string(&marks)?.lines().count(), 1, "one agent, not one an answer");
        assert!(second.iter().any(|(k, v)| k == "SSH_AUTH_SOCK" && Path::new(v) == sock), "{second:?}");
        // Its socket gone, it's not handed on.
        std::fs::remove_file(&sock)?;
        assert!(agent_of(&second).is_empty());
        Ok(())
    }

    #[test]
    fn env_output_parses_with_multiline_values() {
        let env = parse_env("A=1\nB=two\nlines\nPATH=/usr/bin:/bin\n_=x\n");
        assert_eq!(env, vec![("A".into(), "1".into()), ("B".into(), "two\nlines".into()), ("PATH".into(), "/usr/bin:/bin".into()), ("_".into(), "x".into())]);
    }

    #[test]
    fn cwd_inside_a_mirror_stays_inside() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("crates/kiln"))?;
        let spec = |cwd: Option<&str>| RunSpec { cwd: cwd.map(str::to_owned), ..RunSpec::default() };
        assert_eq!(resolve_cwd(&spec(Some("crates/kiln")), Some(dir.path()))?, dir.path().join("crates/kiln"));
        assert_eq!(resolve_cwd(&spec(None), Some(dir.path()))?, dir.path());
        assert!(resolve_cwd(&spec(Some("../..")), Some(dir.path())).is_err());
        assert!(resolve_cwd(&spec(Some("/etc")), Some(dir.path())).is_err());
        assert!(resolve_cwd(&spec(Some("missing")), Some(dir.path())).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_run_reports_output_in_order_and_its_exit_code() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let spec = RunSpec { id: 1, command: "echo out1; echo err1 >&2; printf '%s' \"$KILN_REACH_FROM\"; exit 3".into(), ..RunSpec::default() };
            let (out, err, code, error) = run(spec, dir.path(), None).await?;
            assert_eq!(String::from_utf8_lossy(&out), "out1\nmac");
            assert_eq!(String::from_utf8_lossy(&err), "err1\n");
            assert_eq!((code, error), (Some(3), None));
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_named_shell_keeps_its_folder_and_exports() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            std::fs::create_dir_all(dir.path().join("sub"))?;
            let state = dir.path().join("state");
            let first = RunSpec { id: 1, command: "cd sub && export GREETING=hi".into(), ..RunSpec::default() };
            let (_, _, code, _) = run(first, dir.path(), Some(state.clone())).await?;
            assert_eq!(code, Some(0));
            let second = RunSpec { id: 2, command: "pwd; echo \"$GREETING\"; exit 4".into(), ..RunSpec::default() };
            let (out, _, code, _) = run(second, dir.path(), Some(state.clone())).await?;
            let text = String::from_utf8_lossy(&out).into_owned();
            // bash says where it is as the kernel has it (macOS's /var is /private/var).
            let there = std::fs::canonicalize(text.lines().next().unwrap_or_default())?;
            assert_eq!(there, std::fs::canonicalize(dir.path().join("sub"))?, "{text}");
            assert!(text.ends_with("hi\n"), "{text}");
            // An exit still saves the state.
            assert_eq!(code, Some(4));
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn stdin_reaches_the_command_and_a_missing_folder_fails_cleanly() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let (tx, rx) = async_channel::unbounded();
            let stdin = start(RunSpec { id: 1, command: "tr a-z A-Z".into(), stdin: true, ..RunSpec::default() }, dir.path().to_path_buf(), None, vec![], tx);
            stdin.send(b"hello reach".to_vec()).await?;
            stdin.send(Vec::new()).await?;
            let mut out = Vec::new();
            loop {
                match crate::rt::timeout(Duration::from_secs(20), rx.recv()).await {
                    Ok(Ok(Event::Output { data, .. })) => out.extend(data),
                    Ok(Ok(Event::Exit { code, .. })) => {
                        assert_eq!(code, Some(0));
                        break;
                    }
                    Ok(Ok(Event::Started { .. })) => {}
                    _ => anyhow::bail!("no exit"),
                }
            }
            assert_eq!(out, b"HELLO REACH");
            let (_, _, code, error) = run(RunSpec { id: 2, command: "true".into(), ..RunSpec::default() }, &dir.path().join("gone"), None).await?;
            assert!(code.is_none() && error.is_some());
            Ok(())
        })
    }
}
