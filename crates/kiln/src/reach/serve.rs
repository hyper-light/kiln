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

/// Ask the user's login shell for its environment (blocking).
fn ask_login_shell() -> Option<Vec<(String, String)>> {
    if cfg!(windows) {
        return None;
    }
    let shell = login_shell();
    const BEGIN: &str = "__KILN_REACH_ENV_BEGIN__";
    const END: &str = "__KILN_REACH_ENV_END__";
    let script = format!("printf '\\n{BEGIN}\\n'; env; printf '\\n{END}\\n'");
    let mut cmd = std::process::Command::new(&shell);
    cmd.args(["-l", "-i", "-c", &script]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    cmd.current_dir(crate::os::home_dir());
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
    (!env.is_empty()).then_some(env)
}

/// The environment runs get: the login shell's (kept [`ENV_TTL`]), else this process's.
pub async fn login_env() -> Vec<(String, String)> {
    let cell = LOGIN_ENV.get_or_init(|| Mutex::new(None));
    if let Some((at, env)) = cell.lock().as_ref()
        && at.elapsed() < ENV_TTL
    {
        return env.clone();
    }
    let env = crate::rt::blocking(ask_login_shell).await.ok().flatten().unwrap_or_else(|| std::env::vars().collect());
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

/// bash where there is one, else sh.
fn bash() -> String {
    crate::os::which("bash").map_or_else(|| "sh".to_owned(), |p| p.to_string_lossy().into_owned())
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
        let mut cmd = std::process::Command::new(bash());
        cmd.arg("-c").arg(script(&spec.command, shell_state.as_deref(), start_in_saved));
        cmd.current_dir(&cwd);
        cmd.env_clear();
        cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if let Some(dir) = &shell_state {
            cmd.env("KILN_REACH_SHELL", dir);
        }
        let duplex = match crate::os::proc::spawn_duplex(cmd) {
            Ok(d) => d,
            Err(e) => {
                let _ = events.send(Event::Exit { code: None, error: Some(format!("{e:#}")) }).await;
                return;
            }
        };
        // (Windows' has more: its job object.)
        let crate::os::proc::Duplex { pid, stdin, stdout, stderr, exit, .. } = duplex;
        if events.send(Event::Started { pid, cwd: cwd.to_string_lossy().into_owned() }).await.is_err() {
            signal_group(pid, Signal::Kill);
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
                                signal_group(pid, Signal::Kill);
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
                                signal_group(pid, Signal::Kill);
                                return;
                            }
                        }
                        out_open = false;
                    }
                }
                (stream, Some(data)) => {
                    if events.send(Event::Output { stream, data }).await.is_err() {
                        signal_group(pid, Signal::Kill);
                        return;
                    }
                }
            }
        }
        let code = exit.await.ok().flatten();
        let _ = events.send(Event::Exit { code, error: None }).await;
    });
    stdin_tx
}

#[cfg(test)]
mod tests {
    use super::*;

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
