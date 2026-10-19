//! The platform layer. Everything kiln does differently on Linux, macOS and Windows lives behind
//! this module, so the rest of the code has no `cfg`s.
//!
//! * **Unix** ([`unix`], Linux and macOS): child pipes and PTY masters are non-blocking and driven
//!   by hyper-rt readiness (epoll on Linux, kqueue on macOS); process trees are process groups.
//!   Linux additionally awaits exits through a pidfd and reaps orphans as PID 1; macOS has no
//!   pidfd, so exits are a short `try_wait` poll.
//! * **Windows** ([`windows`]): hyper-rt readiness covers sockets only (AFD), so child pipes and
//!   the ConPTY are read and written on dedicated threads, exits are a short `try_wait` poll, and
//!   process trees are job objects (killed as one, and with the agent if it dies).
//!
//! Both implementations export the same items; the portable helpers below sit on top of them.

use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// kiln's own code signature on a Mac, so macOS knows every upgrade as one program.
pub mod macsign;

/// Child processes: [`crate::proc`]'s implementation.
#[cfg(unix)]
#[path = "proc_unix.rs"]
pub mod proc;
#[cfg(windows)]
#[path = "proc_windows.rs"]
pub mod proc;

/// `kiln supervise`'s child control.
#[cfg(unix)]
#[path = "supervise_unix.rs"]
pub mod supervise;
#[cfg(windows)]
#[path = "supervise_windows.rs"]
pub mod supervise;

/// PTY session I/O: [`crate::terminal`]'s plumbing.
#[cfg(unix)]
#[path = "pty_unix.rs"]
pub mod pty;
#[cfg(windows)]
#[path = "pty_windows.rs"]
pub mod pty;

/// Host and process metrics: [`crate::host`]'s implementation (procfs on Linux, sysinfo elsewhere).
#[cfg(target_os = "linux")]
#[path = "metrics_procfs.rs"]
pub mod metrics;
#[cfg(not(target_os = "linux"))]
#[path = "metrics_sysinfo.rs"]
pub mod metrics;

/// The first executable called `name` on `PATH` (on Windows also `name` + each `PATHEXT`).
/// Lowers the calling thread's scheduling priority, ten nice levels below its own (19 at the
/// most): work that may wait (a query's helper threads) yields to what may not (the writes the
/// server takes). Linux's threads each have their own nice value (`setpriority(PRIO_PROCESS, tid)`);
/// elsewhere the thread runs as it was.
pub fn lower_thread_priority() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let me = rustix::thread::gettid();
        if let Ok(now) = rustix::process::getpriority_process(Some(me)) {
            let _ = rustix::process::setpriority_process(Some(me), (now + 10).min(19));
        }
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let candidates = exe_names(name);
    std::env::split_paths(&paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates.iter().map(move |n| dir.join(n)).collect::<Vec<_>>())
        .find(|p| p.is_file())
}

/// Whether `name` is an executable on `PATH`.
pub fn have(name: &str) -> bool {
    which(name).is_some()
}

/// Keep git from ever prompting (terminal, askpass helper, or Git Credential Manager's window):
/// a missing credential fails fast instead of hanging a job. On Windows there is no
/// `/bin/true` to answer askpass, and git without one simply fails the credential.
pub fn git_no_prompts(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never");
    if cfg!(unix) {
        cmd.env("GIT_ASKPASS", "/bin/true").env("SSH_ASKPASS", "/bin/true");
    }
    cmd
}

/// How a job step runs `script` by default: `bash -eo pipefail` (else `sh -e`) on Unix;
/// PowerShell 7 (else Windows PowerShell) on Windows, as GitHub Actions does there.
pub fn default_step_shell(script: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        let ps = if have("pwsh") { "pwsh" } else { "powershell" };
        return (ps.to_owned(), script_args(ps, script));
    }
    if have("bash") {
        let args = ["--noprofile", "--norc", "-eo", "pipefail", "-c", script];
        return ("bash".to_owned(), args.map(str::to_owned).to_vec());
    }
    ("sh".to_owned(), vec!["-ec".to_owned(), script.to_owned()])
}

/// The arguments that make shell `program` run `script`: `-c` for Unix shells, `/D /S /C` for
/// cmd, and for PowerShell `-Command` with errors made fatal and the last native exit code
/// passed on (GitHub Actions' wrapper).
pub fn script_args(program: &str, script: &str) -> Vec<String> {
    match program_stem(program).as_str() {
        "cmd" => ["/D", "/S", "/C", script].map(str::to_owned).to_vec(),
        "pwsh" | "powershell" => {
            let wrapped = format!(
                "$ErrorActionPreference = 'Stop'\n{script}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}"
            );
            vec!["-NoLogo".into(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), wrapped]
        }
        _ => vec!["-c".into(), script.to_owned()],
    }
}

/// Open `url` in the desktop's browser (best effort, never waits).
pub fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    let _ = cmd.arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// hyper-rt's signal kinds that stand for kiln's `SIGINT` / `SIGTERM` / `SIGHUP` here. Unix has
/// them as such; Windows delivers console control events, of which Ctrl-Break, closing the
/// console and system shutdown mean "terminate", and nothing means "reload".
pub fn signal_kinds(sig: i32) -> &'static [hyper_rt::signal::Signal] {
    use hyper_rt::signal::Signal;
    match sig {
        crate::rt::SIGINT => &[Signal::Interrupt],
        crate::rt::SIGTERM if cfg!(windows) => &[Signal::Break, Signal::Close, Signal::Shutdown],
        crate::rt::SIGTERM => &[Signal::Terminate],
        crate::rt::SIGHUP if cfg!(windows) => &[],
        crate::rt::SIGHUP => &[Signal::Hangup],
        _ => &[],
    }
}

/// kiln's signal number for a delivered hyper-rt signal kind.
pub fn signal_number(kind: hyper_rt::signal::Signal) -> i32 {
    use hyper_rt::signal::Signal;
    match kind {
        Signal::Interrupt => crate::rt::SIGINT,
        Signal::Hangup => crate::rt::SIGHUP,
        Signal::Terminate | Signal::Break | Signal::Close | Signal::Logoff | Signal::Shutdown => crate::rt::SIGTERM,
    }
}

/// The names `name` may have on disk: itself, plus the executable extensions on Windows.
fn exe_names(name: &str) -> Vec<String> {
    let mut names = vec![name.to_owned()];
    if cfg!(windows) && Path::new(name).extension().is_none() {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
        names.extend(exts.split(';').filter(|e| !e.is_empty()).map(|e| format!("{name}{}", e.to_ascii_lowercase())));
    }
    names
}

/// The file name of a program, without directory or extension, lowercased: what to compare when
/// choosing how to talk to a shell (`C:\Program Files\PowerShell\7\pwsh.exe` → `pwsh`).
pub fn program_stem(program: &str) -> String {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ["exe", "com", "bat", "cmd"].contains(&ext.to_ascii_lowercase().as_str()) => stem,
        _ => name,
    };
    stem.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_path() {
        let sh = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(have(sh), "{sh} not on PATH");
        assert!(!have("kiln-surely-not-a-real-program"));
        assert_eq!(program_stem("/usr/bin/bash"), "bash");
        assert_eq!(program_stem(r"C:\Program Files\PowerShell\7\pwsh.exe"), "pwsh");
        assert_eq!(program_stem("CMD.EXE"), "cmd");
    }

    #[test]
    fn shells_get_the_right_flags() {
        assert_eq!(script_args("/bin/bash", "echo hi"), ["-c", "echo hi"]);
        assert_eq!(script_args(r"C:\Windows\System32\cmd.exe", "dir"), ["/D", "/S", "/C", "dir"]);
        let ps = script_args("pwsh", "Write-Output hi");
        assert_eq!(ps.get(..4), Some(&["-NoLogo".to_owned(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into()][..]));
        assert!(ps.last().is_some_and(|s| s.contains("Write-Output hi") && s.contains("exit $LASTEXITCODE")));
        let (prog, args) = default_step_shell("true");
        if cfg!(windows) {
            assert!(prog == "pwsh" || prog == "powershell");
        } else {
            assert!(prog == "bash" || prog == "sh");
            assert_eq!(args.last().map(String::as_str), Some("true"));
        }
    }

    #[test]
    fn positional_io_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("f");
        let f = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path)?;
        write_all_at(&f, b"world", 6)?;
        write_all_at(&f, b"hello ", 0)?;
        let mut buf = [0u8; 11];
        let n = read_at(&f, &mut buf, 0)?;
        assert_eq!(buf.get(..n), Some(&b"hello world"[..]));
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod priority_tests {
    /// A thread lowered runs ten nice levels below the process; the rest keep theirs.
    #[test]
    fn a_lowered_thread_runs_ten_nice_levels_below_the_rest() {
        let me = rustix::thread::gettid();
        let before = rustix::process::getpriority_process(Some(me)).unwrap_or(0);
        let lowered = std::thread::spawn(|| {
            super::lower_thread_priority();
            rustix::process::getpriority_process(Some(rustix::thread::gettid())).unwrap_or(-100)
        })
        .join()
        .unwrap_or(-100);
        assert_eq!(lowered, (before + 10).min(19));
        assert_eq!(rustix::process::getpriority_process(Some(me)).unwrap_or(-100), before);
    }
}
