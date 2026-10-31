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
/// The Win32 calls installing needs (elevation, the user's `Path`, UAC) and a volume's room,
/// declared by hand.
#[cfg(windows)]
mod win32;

pub mod battery;
pub mod cgroup;
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

/// Each agent session's processes held together by the system, so everything an agent started is
/// known (however it was parented since), whose a process is takes one look, and ending them all
/// is one call: a cgroup of its own on Linux, a job object on Windows, followed fork by fork on
/// macOS. Where there's none, `Group::of` is `None` and an agent's processes are found by their
/// mark (`crate::harness::survivors`).
#[cfg(target_os = "linux")]
#[path = "contain_linux.rs"]
pub mod contain;
#[cfg(windows)]
#[path = "contain_windows.rs"]
pub mod contain;
#[cfg(target_os = "macos")]
#[path = "contain_macos.rs"]
pub mod contain;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[path = "contain_none.rs"]
pub mod contain;

/// A CI step's processes in its job's group ([`contain::job_group`]), let go of once the step ends
/// (dropped): a kiln the step starts (a test's or a lab's runner) takes that group as its own
/// cgroup, never the runner's, where the runner's agents are.
pub struct JobGroup(Option<contain::Group>);

impl JobGroup {
    /// `cmd` joins job `job`'s group before it runs anything, where the machine has one (Linux).
    pub fn enter(job: &str, cmd: &mut std::process::Command) -> Self {
        let group = contain::job_group(job);
        #[cfg(not(windows))]
        if let Some(g) = &group {
            g.enter(cmd);
        }
        #[cfg(windows)]
        let _ = cmd;
        Self(group)
    }
}

impl Drop for JobGroup {
    fn drop(&mut self) {
        if let Some(g) = &self.0 {
            g.release();
        }
    }
}

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

/// The first executable called `name` on `PATH`. On Windows a name without a program's extension
/// is looked for as cmd.exe looks for it, as `name` + each `PATHEXT` extension a process starts
/// from, never bare: npm puts a shell script `codex`, which Windows can't start, beside `codex.cmd`.
pub fn which(name: &str) -> Option<PathBuf> {
    which_in(name, &std::env::var_os("PATH")?)
}

/// [`which`] over `paths`, a `PATH` value.
pub fn which_in(name: &str, paths: &std::ffi::OsStr) -> Option<PathBuf> {
    let pathext = cfg!(windows).then(|| std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned()));
    find_in(name, paths, pathext.as_deref())
}

/// [`which_in`] with Windows' `PATHEXT` (None: Unix, where a program's name is its file's).
fn find_in(name: &str, paths: &std::ffi::OsStr, pathext: Option<&str>) -> Option<PathBuf> {
    let candidates = exe_names(name, pathext);
    std::env::split_paths(paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates.iter().map(move |n| dir.join(n)).collect::<Vec<_>>())
        .find(|p| p.is_file())
}

/// Whether `name` is an executable on `PATH`.
pub fn have(name: &str) -> bool {
    which(name).is_some()
}

/// This kiln's binary, as a path another process can start. On Linux, once an upgrade has put a new
/// binary in its place (and until this process takes it up, once its jobs are done), the path
/// reads `<path> (deleted)`: the kiln at `<path>` is the one to start.
pub fn this_kiln() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(replaced(&exe).unwrap_or(exe))
}

/// The binary now at a running binary's path that was replaced (`<path> (deleted)`).
fn replaced(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?.strip_suffix(" (deleted)").map(PathBuf::from).filter(|p| p.is_file())
}

/// Where [`add_to_user_path`] put a folder on the user's PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAdded {
    /// The shell startup file that puts it there (Unix); none where the system keeps the user's
    /// PATH itself (Windows).
    pub file: Option<PathBuf>,
    /// What puts it on this terminal's PATH too: only new terminals read that file.
    pub now: Option<String>,
    /// Whether that file was made for it.
    pub created: bool,
}

/// A Windows `Path` value (`;`-separated, as the registry keeps it) with `dir` added at its end,
/// or `None` when one of its entries is `dir` already. Entries are compared as Windows compares
/// folders: without case, quotes or a trailing `\`, and with `%VARIABLES%` expanded by `var`.
pub fn path_list_with(list: &str, dir: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let norm = |entry: &str| {
        let expanded = expand_percent_vars(entry.trim().trim_matches('"'), &var);
        expanded.trim_end_matches(['\\', '/']).to_lowercase()
    };
    let want = norm(dir);
    if list.split(';').any(|e| norm(e) == want) {
        return None;
    }
    let kept = list.trim_end_matches(';');
    Some(if kept.is_empty() { dir.to_owned() } else { format!("{kept};{dir}") })
}

/// A Windows `Path` value without its entries that are `dir` (compared as [`path_list_with`]
/// does), or `None` when none is.
pub fn path_list_without(list: &str, dir: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let norm = |entry: &str| expand_percent_vars(entry.trim().trim_matches('"'), &var).trim_end_matches(['\\', '/']).to_lowercase();
    let want = norm(dir);
    let kept: Vec<&str> = list.split(';').filter(|e| e.is_empty() || norm(e) != want).collect();
    (kept.len() != list.split(';').count()).then(|| kept.join(";"))
}

/// `%NAME%` replaced by `var(NAME)`; a name it doesn't know, and a lone `%`, stay as they are.
fn expand_percent_vars(s: &str, var: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        let after = from.get(1..).unwrap_or_default();
        match after.find('%') {
            Some(end) => {
                let name = after.get(..end).unwrap_or_default();
                match var(name).filter(|_| !name.is_empty()) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(from.get(..end + 2).unwrap_or_default()),
                }
                rest = after.get(end + 1..).unwrap_or_default();
            }
            None => {
                out.push_str(from);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The size of the volume holding `path` and the bytes this user may still write on it (None:
/// unknown).
// The statvfs fields' types differ by platform: a conversion that is one elsewhere is none here.
#[allow(clippy::useless_conversion)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let s = nix::sys::statvfs::statvfs(path).ok()?;
        let frag = u64::from(s.fragment_size());
        Some((u64::from(s.blocks()).saturating_mul(frag), u64::from(s.blocks_available()).saturating_mul(frag)))
    }
    // Asked of the volume itself: `sysinfo`'s mount points (`C:\`) don't prefix the `\\?\` path
    // `canonicalize` gives.
    #[cfg(windows)]
    {
        win32::disk_space(path).ok()
    }
}

/// Bytes this user may still write on the volume holding `path` (None: unknown).
pub fn disk_available(path: &Path) -> Option<u64> {
    disk_space(path).map(|(_, available)| available)
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

/// The extensions of the files Windows starts as programs: binaries, and batch files (which the
/// standard library runs through cmd.exe). `PATHEXT`'s others (`.js`, `.vbs`) open in a program.
const PROGRAM_EXTENSIONS: [&str; 4] = ["exe", "com", "bat", "cmd"];

/// The names `name` may have on disk: itself on Unix; on Windows (`pathext` given) itself when
/// it has a program's extension, else it with each of `pathext`'s that is one, in that order.
fn exe_names(name: &str, pathext: Option<&str>) -> Vec<String> {
    let is_program = |ext: &str| PROGRAM_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str());
    let Some(pathext) = pathext else { return vec![name.to_owned()] };
    if Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(is_program) {
        return vec![name.to_owned()];
    }
    pathext.split(';').map(|e| e.trim().trim_start_matches('.')).filter(|e| is_program(e)).map(|e| format!("{name}.{}", e.to_ascii_lowercase())).collect()
}

/// The file name of a program, without directory or extension, lowercased: what to compare when
/// choosing how to talk to a shell (`C:\Program Files\PowerShell\7\pwsh.exe` → `pwsh`).
pub fn program_stem(program: &str) -> String {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && PROGRAM_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) => stem,
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
    fn a_replaced_binary_s_path_names_the_new_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let kiln = dir.path().join("kiln");
        std::fs::write(&kiln, b"new")?;
        let gone = |p: &Path| PathBuf::from(format!("{} (deleted)", p.display()));
        assert_eq!(replaced(&gone(&kiln)), Some(kiln.clone()));
        assert_eq!(replaced(&kiln), None, "not replaced");
        assert_eq!(replaced(&gone(&dir.path().join("uninstalled"))), None, "nothing in its place");
        assert!(this_kiln()?.is_file());
        Ok(())
    }

    /// As an upgrade does it: a running binary's path given a new file, the process names the new
    /// one (its own `/proc/self/exe` reads `<path> (deleted)`). The running binary is this test's,
    /// linked into a folder of its own (a copy would be gigabytes).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_binary_replaced_on_disk_names_the_one_in_its_place() -> anyhow::Result<()> {
        const GO: &str = "KILN_TEST_REPLACED_GO";
        // The run in the replaced binary: once told, what it names.
        if let Some(go) = std::env::var_os(GO) {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !Path::new(&go).exists() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("exe={}", std::env::current_exe()?.display());
            println!("this_kiln={}", this_kiln()?.display());
            return Ok(());
        }
        let me = std::env::current_exe()?;
        let dir = tempfile::tempdir_in(me.parent().unwrap_or(Path::new(".")))?;
        let bin = dir.path().join("kiln");
        std::fs::hard_link(&me, &bin)?;
        let go = dir.path().join("go");
        let child = std::process::Command::new(&bin)
            .args(["--exact", "os::tests::a_running_binary_replaced_on_disk_names_the_one_in_its_place", "--nocapture", "--test-threads=1"])
            .env(GO, &go)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let new = dir.path().join("kiln.new");
        std::fs::write(&new, b"the next kiln")?;
        std::fs::rename(&new, &bin)?;
        std::fs::write(&go, b"")?;
        let said = String::from_utf8_lossy(&child.wait_with_output()?.stdout).into_owned();
        println!("{said}");
        assert!(said.contains(&format!("exe={} (deleted)\n", bin.display())), "the replaced binary's own path: {said}");
        assert!(said.contains(&format!("this_kiln={}\n", bin.display())), "{said}");
        Ok(())
    }

    #[test]
    fn a_windows_program_is_one_cmd_would_start() -> anyhow::Result<()> {
        let pathext = Some(".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC");
        assert_eq!(exe_names("codex", pathext), ["codex.com", "codex.exe", "codex.bat", "codex.cmd"]);
        assert_eq!(exe_names("codex.CMD", pathext), ["codex.CMD"]);
        assert_eq!(exe_names("codex", None), ["codex"]);
        // What `npm install -g @openai/codex` leaves in Node's folder: a shell script for Git Bash,
        // a batch file for cmd, a script for PowerShell.
        let npm = tempfile::tempdir()?;
        for f in ["codex", "codex.cmd", "codex.ps1"] {
            std::fs::write(npm.path().join(f), "")?;
        }
        let path = std::env::join_paths([npm.path()])?;
        assert_eq!(find_in("codex", &path, pathext), Some(npm.path().join("codex.cmd")));
        assert_eq!(find_in("codex.cmd", &path, pathext), Some(npm.path().join("codex.cmd")));
        assert_eq!(find_in("codex", &path, None), Some(npm.path().join("codex")), "Unix starts the script");
        // A file Windows opens rather than starts is no program, even first on PATH.
        let (scripts, node) = (tempfile::tempdir()?, tempfile::tempdir()?);
        std::fs::write(scripts.path().join("node.js"), "")?;
        std::fs::write(scripts.path().join("gemini"), "")?;
        std::fs::write(node.path().join("node.exe"), "")?;
        let path = std::env::join_paths([scripts.path(), node.path()])?;
        assert_eq!(find_in("node", &path, pathext), Some(node.path().join("node.exe")));
        assert_eq!(find_in("gemini", &path, pathext), None, "a shell script alone is not installed");
        Ok(())
    }

    #[test]
    fn a_folder_goes_on_a_windows_path_once() {
        let var = |name: &str| (name == "LOCALAPPDATA").then(|| r"C:\Users\me\AppData\Local".to_owned());
        let kiln = r"C:\Users\me\AppData\Local\Programs\kiln";
        assert_eq!(path_list_with("", kiln, var).as_deref(), Some(kiln));
        assert_eq!(path_list_with(r"C:\a;C:\b", kiln, var), Some(format!(r"C:\a;C:\b;{kiln}")));
        assert_eq!(path_list_with(r"C:\a;;C:\b;", kiln, var), Some(format!(r"C:\a;;C:\b;{kiln}")), "the user's list kept as it is");
        // Already there: another case, quoted, a trailing backslash, or through a variable.
        assert_eq!(path_list_with(r"C:\a;c:\users\ME\appdata\local\programs\KILN", kiln, var), None);
        assert_eq!(path_list_with(r#"C:\a;"C:\Users\me\AppData\Local\Programs\kiln\""#, kiln, var), None);
        assert_eq!(path_list_with(r"%LOCALAPPDATA%\Programs\kiln;C:\a", kiln, var), None);
        // A variable it can't expand stays as written, and is no match.
        assert!(path_list_with(r"%NOPE%\Programs\kiln", kiln, var).is_some());
        assert_eq!(expand_percent_vars(r"%NOPE%\x;100%;%%", &var), r"%NOPE%\x;100%;%%");
    }

    #[test]
    fn a_folder_comes_off_a_windows_path_as_it_went_on() {
        let var = |name: &str| (name == "LOCALAPPDATA").then(|| r"C:\Users\me\AppData\Local".to_owned());
        let kiln = r"C:\Users\me\AppData\Local\Programs\kiln";
        let list = r"C:\a;C:\b";
        let added = path_list_with(list, kiln, var).unwrap_or_default();
        assert_eq!(path_list_without(&added, kiln, var).as_deref(), Some(list));
        assert_eq!(path_list_without(r"C:\a;%LOCALAPPDATA%\Programs\kiln\;;C:\b", kiln, var).as_deref(), Some(r"C:\a;;C:\b"));
        assert_eq!(path_list_without(list, kiln, var), None, "not there: nothing to change");
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
