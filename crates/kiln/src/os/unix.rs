//! Unix (Linux and macOS).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------------- identity and paths

/// Running as root (system-wide install, `/etc/kiln`, `/var/lib/kiln`).
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Running with full rights: as root.
pub fn elevated() -> bool {
    is_root()
}

/// A terminal here takes escapes as it is (Windows' console needs them turned on).
pub fn console_takes_escapes() -> bool {
    true
}

/// Windows only (its UAC prompt): elsewhere installs ask through `sudo`.
pub fn run_elevated(_program: &Path, _parameters: &str, _timeout: std::time::Duration) -> io::Result<u32> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "only Windows asks for administrator rights this way"))
}

/// Put `dir` on PATH for this user's new terminals when it isn't on this one's (`~/.local/bin`
/// isn't on a Mac's, nor on some Linux desktops'): a line in the startup file their shell
/// (`$SHELL`) reads for each ([`shell_path_line`]), which adds it unless it's there. `Ok(None)`
/// when it's on PATH, or that line is in the file already. Never as root: a system-wide kiln is in
/// `/usr/local/bin`, which every PATH has.
pub fn add_to_user_path(dir: &Path) -> io::Result<Option<super::PathAdded>> {
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|e| e == dir));
    if on_path || is_root() {
        return Ok(None);
    }
    let shell = std::env::var("SHELL").unwrap_or_default();
    let zdotdir = std::env::var_os("ZDOTDIR").map(PathBuf::from);
    let line = shell_path_line(&shell, cfg!(target_os = "macos"), &home_dir(), zdotdir.as_deref(), dir, Path::exists);
    let created = !line.file.exists();
    Ok(add_marked_line(&line.file, &line.line)?.then_some(super::PathAdded { file: Some(line.file), now: Some(line.now), created }))
}

/// Take back what [`add_to_user_path`] added: its marked line (and the blank line set before it)
/// out of the startup file, and the file itself when it was made for it and holds nothing else.
pub fn remove_from_user_path(_dir: &Path, added: &super::PathAdded) -> io::Result<()> {
    let Some(file) = &added.file else { return Ok(()) };
    let text = std::fs::read_to_string(file)?;
    let kept = without_marked_line(&text);
    if added.created && kept.trim().is_empty() {
        return std::fs::remove_file(file);
    }
    if kept != text {
        std::fs::write(file, kept)?;
    }
    Ok(())
}

/// `text` without [`add_marked_line`]'s block: the mark, the line after it, and a blank line
/// just before the mark.
fn without_marked_line(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(at) = lines.iter().position(|l| l.trim() == PATH_MARK) else { return text.to_owned() };
    let from = if at > 0 && lines.get(at - 1).is_some_and(|l| l.trim().is_empty()) { at - 1 } else { at };
    let rest: Vec<&str> = lines.iter().take(from).chain(lines.iter().skip(at + 2)).copied().collect();
    if rest.is_empty() { String::new() } else { format!("{}\n", rest.join("\n")) }
}

/// What marks the line [`add_to_user_path`] adds to a shell's startup file.
const PATH_MARK: &str = "# kiln: its folder on PATH (added by `kiln install`)";

/// Append `line` to shell startup file `file` (made if it isn't there), under [`PATH_MARK`] and
/// apart from what's above it, unless the mark is in it already: whether it was added.
fn add_marked_line(file: &Path, line: &str) -> io::Result<bool> {
    use std::io::Write as _;
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if text.lines().any(|l| l.trim() == PATH_MARK) {
        return Ok(false);
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let gap = if text.is_empty() { "" } else if text.ends_with('\n') { "\n" } else { "\n\n" };
    OpenOptions::new().create(true).append(true).open(file)?.write_all(format!("{gap}{PATH_MARK}\n{line}\n").as_bytes())?;
    Ok(true)
}

/// How a shell gets a folder on its PATH.
#[derive(Debug, PartialEq, Eq)]
struct ShellPathLine {
    /// The startup file every new terminal of that shell reads.
    file: PathBuf,
    /// What goes in it: adds the folder unless PATH has it.
    line: String,
    /// What adds it in this terminal.
    now: String,
}

/// For login shell `shell` (`$SHELL`), the startup file new terminals read, and what in it puts
/// `dir` on PATH: zsh's `.zshrc` (in `zdotdir` when set); bash's `.bashrc`, but on macOS, whose
/// terminals start login shells, the first of `.bash_profile`, `.bash_login` and `.profile` there
/// is (`.bash_profile` when none is); fish's `conf.d/kiln.fish`; any other shell's `.profile`.
/// `dir` is written `"$HOME/…"` when it's in `home`, quoted as it is otherwise.
fn shell_path_line(shell: &str, macos: bool, home: &Path, zdotdir: Option<&Path>, dir: &Path, exists: impl Fn(&Path) -> bool) -> ShellPathLine {
    let fish = shell.rsplit('/').next() == Some("fish");
    let in_home = dir.strip_prefix(home).ok().map(|rest| rest.display().to_string()).filter(|rest| !rest.contains(['"', '$', '`', '\\']));
    let quoted = match in_home {
        Some(rest) => format!("\"$HOME/{rest}\""),
        None if fish => format!("'{}'", dir.display().to_string().replace('\\', "\\\\").replace('\'', "\\'")),
        None => format!("'{}'", dir.display().to_string().replace('\'', "'\\''")),
    };
    if fish {
        return ShellPathLine {
            file: home.join(".config/fish/conf.d/kiln.fish"),
            line: format!("contains -- {quoted} $PATH; or set -gx PATH {quoted} $PATH"),
            now: format!("set -gx PATH {quoted} $PATH"),
        };
    }
    let file = match shell.rsplit('/').next().unwrap_or_default() {
        "zsh" => zdotdir.unwrap_or(home).join(".zshrc"),
        "bash" if macos => [".bash_profile", ".bash_login", ".profile"].map(|f| home.join(f)).into_iter().find(|f| exists(f)).unwrap_or_else(|| home.join(".bash_profile")),
        "bash" => home.join(".bashrc"),
        _ => home.join(".profile"),
    };
    ShellPathLine {
        file,
        line: format!("case \":$PATH:\" in *:{quoted}:*) ;; *) export PATH={quoted}:\"$PATH\" ;; esac"),
        now: format!("export PATH={quoted}:\"$PATH\""),
    }
}

/// The real user id (launchd's per-user domains are `gui/<uid>` and `user/<uid>`).
pub fn uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

/// The current user's login name.
pub fn user_principal() -> String {
    std::env::var("USER").unwrap_or_default()
}

pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "localhost".to_owned())
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// System-wide data directory and config file (used when running as root).
pub fn system_dirs() -> (PathBuf, PathBuf) {
    (PathBuf::from("/var/lib/kiln"), PathBuf::from("/etc/kiln/kiln.toml"))
}

/// Per-user data (`$XDG_DATA_HOME`, else `~/.local/share`).
pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user config (`$XDG_CONFIG_HOME`, else `~/.config`).
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user state (`$XDG_STATE_HOME` when absolute, else `~/.local/state`).
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"))
}

// ---------------------------------------------------------------------------------------- files

/// Create files with Unix permission bits `mode` (e.g. `0o600` for secrets).
pub fn with_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    opts.mode(mode)
}

/// Mark a file executable (`0755`).
pub fn make_executable(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Open `path` to read it. (Windows keeps others from changing it meanwhile; Unix has no share
/// modes, and nothing here hands a file to a more privileged process.)
pub fn open_unchangeable(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Read at `offset` without moving a shared cursor.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(buf, offset)
}

/// Write all of `buf` at `offset` without moving a shared cursor.
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    file.write_all_at(buf, offset)
}

// ------------------------------------------------------------------- in-place re-execution

/// This platform can replace a running kiln with a new binary in place (same pid), handing open
/// PTY masters to the new image so shells survive the upgrade.
pub const IN_PLACE_UPGRADE: bool = true;

/// macOS's local network privacy stops recognising a process that replaced its own image: after
/// an `exec`, the runner's connections to the server fail with "No route to host" until kiln is
/// started afresh. So on macOS nothing re-executes itself: a runner takes up a new binary as a
/// fresh process its supervisor starts, handing its terminals over first
/// ([`crate::os::supervise::Handover`]).
pub const UPGRADE_BY_SUCCESSOR: bool = cfg!(target_os = "macos");

/// Keep `fd` open across the next exec (clear close-on-exec).
pub fn keep_across_exec(fd: &std::os::fd::OwnedFd) -> io::Result<()> {
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty()).map_err(io::Error::from)
}

/// Take ownership of a descriptor this process was handed at start-up: carried across its own
/// re-exec (its number comes from the handoff file the previous image wrote), or inherited from
/// the supervisor that started it (the number in an environment variable it set). `None` when
/// that number is not open.
#[allow(unsafe_code)]
pub fn reclaim_fd(raw: i32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{BorrowedFd, FromRawFd as _};
    if raw < 3 {
        return None;
    }
    // SAFETY: the number was written by this same process's previous image just before it
    // re-executed itself, or by the parent that started this process, for a descriptor it had
    // cleared close-on-exec on; each is read once at start-up, and nothing else in this image has
    // claimed that number yet. The borrow only lives for the validity check.
    let open = rustix::io::fcntl_getfd(unsafe { BorrowedFd::borrow_raw(raw) }).is_ok();
    // SAFETY: as above; the descriptor is open and owned by nobody else in this image.
    open.then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
}

/// Close `fd` at the next exec again (after [`reclaim_fd`]: nothing this process starts inherits
/// it).
pub fn close_on_exec(fd: &std::os::fd::OwnedFd) -> io::Result<()> {
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).map_err(io::Error::from)
}

/// Replace this process image with `exe` (same pid, same children). Returns only on failure.
pub fn reexec(exe: &Path, args: &[std::ffi::OsString], env: &[(&str, String)]) -> io::Error {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.exec()
}

/// Stop these processes and the process groups they lead or belong to (never `keep`'s, nor this
/// process's own): SIGTERM now, SIGKILL to whatever is left after `grace`. Blocking.
pub fn stop_processes(pids: &[u32], keep: Option<u32>, grace: std::time::Duration) {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::{Pid, getpgid};
    let ours = getpgid(None).ok();
    let keep = keep.and_then(|k| i32::try_from(k).ok()).map(Pid::from_raw);
    let mut groups = std::collections::HashSet::new();
    let pids: Vec<Pid> = pids.iter().filter_map(|p| i32::try_from(*p).ok()).filter(|p| *p > 1).map(Pid::from_raw).collect();
    for p in &pids {
        if let Ok(g) = getpgid(Some(*p))
            && Some(g) != ours
            && Some(g) != keep
        {
            groups.insert(g);
        }
        let _ = kill(*p, Signal::SIGTERM);
    }
    for g in &groups {
        let _ = killpg(*g, Signal::SIGTERM);
    }
    let end = std::time::Instant::now() + grace;
    while std::time::Instant::now() < end && pids.iter().any(|p| kill(*p, None).is_ok()) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for p in &pids {
        let _ = kill(*p, Signal::SIGKILL);
    }
    for g in &groups {
        let _ = killpg(*g, Signal::SIGKILL);
    }
}

/// The process that started this one (1 once that has exited).
pub fn parent_pid() -> u32 {
    std::os::unix::process::parent_id()
}

/// While `on`, this process's memory is kept from its user's other processes (docs/ci.md W27-R02,
/// a CI leg's key in it, out of its steps' reach): on Linux it isn't dumpable meanwhile, so neither
/// ptrace nor `/proc/<pid>/mem` (nor `environ`, `fd` or `exe` there) reaches it, whatever Yama's
/// ptrace scope. Off, it's dumpable again, unless it wasn't before. macOS keeps a process's memory
/// from its user's others already: `task_for_pid` is refused them.
#[cfg(target_os = "linux")]
pub fn private_memory(on: bool) -> io::Result<()> {
    use rustix::process::{DumpableBehavior, dumpable_behavior, set_dumpable_behavior};
    use std::sync::atomic::{AtomicBool, Ordering};
    /// Whether this made it undumpable (so may make it dumpable again).
    static MADE: AtomicBool = AtomicBool::new(false);
    if on {
        if dumpable_behavior()? == DumpableBehavior::Dumpable {
            set_dumpable_behavior(DumpableBehavior::NotDumpable)?;
            MADE.store(true, Ordering::SeqCst);
        }
    } else if MADE.swap(false, Ordering::SeqCst) {
        set_dumpable_behavior(DumpableBehavior::Dumpable)?;
    }
    Ok(())
}

/// While `on`, this process's memory is kept from its user's other processes: macOS does so
/// already (`task_for_pid` is refused them).
#[cfg(not(target_os = "linux"))]
pub fn private_memory(_on: bool) -> io::Result<()> {
    Ok(())
}

/// Whether `pid` is still running (and signalable by us). A zombie (dead, not yet reaped) is not.
pub fn pid_alive(pid: u32) -> bool {
    i32::try_from(pid).is_ok_and(|p| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None).is_ok()) && !is_zombie(pid)
}

/// Whether `pid` has died and not been reaped yet.
#[cfg(target_os = "linux")]
pub fn is_zombie(pid: u32) -> bool {
    proc_stat(pid).is_some_and(|(state, _)| state == 'Z')
}

/// Whether `pid` has died and not been reaped yet (only Linux can tell cheaply).
#[cfg(not(target_os = "linux"))]
pub fn is_zombie(_pid: u32) -> bool {
    false
}

/// A process's state letter and parent, from `/proc/<pid>/stat` (its name may hold spaces and
/// parentheses, so the fields after its last `)` are read).
#[cfg(target_os = "linux")]
fn proc_stat(pid: u32) -> Option<(char, u32)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.get(s.rfind(')')? + 1..)?;
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    Some((state, ppid))
}

/// This process's children that have died and not been reaped.
#[cfg(target_os = "linux")]
fn zombie_children() -> Vec<u32> {
    let me = std::process::id();
    // Each thread's children, where the kernel lists them; else every process, by its parent.
    let mut pids: Vec<u32> = Vec::new();
    let mut listed = false;
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for t in tasks.flatten() {
            if let Ok(s) = std::fs::read_to_string(t.path().join("children")) {
                listed = true;
                pids.extend(s.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
            }
        }
    }
    if !listed {
        pids = std::fs::read_dir("/proc").map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok()).collect()).unwrap_or_default();
    }
    pids.sort_unstable();
    pids.dedup();
    pids.into_iter().filter(|&pid| proc_stat(pid).is_some_and(|(state, ppid)| state == 'Z' && ppid == me)).collect()
}

/// This process's children that have died and not been reaped.
#[cfg(not(target_os = "linux"))]
fn zombie_children() -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};
    let me = sysinfo::Pid::from_u32(std::process::id());
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    sys.processes().values().filter(|p| p.parent() == Some(me) && p.status() == ProcessStatus::Zombie).map(|p| p.pid().as_u32()).collect()
}

/// Of these dead children, reaps the ones first seen dead (in `seen`) at least `grace` before
/// `now`, and returns them. `seen` forgets children that are no longer there.
fn reap_old(zombies: &[u32], seen: &mut std::collections::HashMap<u32, std::time::Instant>, grace: std::time::Duration, now: std::time::Instant) -> Vec<u32> {
    seen.retain(|pid, _| zombies.contains(pid));
    let mut reaped = Vec::new();
    for &pid in zombies {
        let first = *seen.entry(pid).or_insert(now);
        if now.saturating_duration_since(first) >= grace && matches!(try_reap(pid), Ok(Some(_))) {
            seen.remove(&pid);
            reaped.push(pid);
        }
    }
    reaped
}

/// How long a child may lie dead before the reaper takes it. Whoever waits for a child reaps it
/// within a second of its death (`wait_pid_exit`, `Command::output`), so one dead this long has
/// nobody waiting, and reaping it takes nothing from anyone.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Reaps, every ten seconds for as long as this process runs, children that died with nobody
/// waiting for them, so dead processes never pile up whatever left them: a wait that missed its
/// wake-up, a handle dropped unwaited, a child an earlier image of this process started before an
/// in-place upgrade. Started once per process.
pub fn start_reaper() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        crate::rt::spawn(async {
            let seen: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<u32, std::time::Instant>>> = std::sync::Arc::default();
            loop {
                crate::rt::sleep(std::time::Duration::from_secs(10)).await;
                let s = seen.clone();
                let reaped = crate::rt::blocking(move || reap_old(&zombie_children(), &mut s.lock(), REAP_GRACE, std::time::Instant::now())).await.unwrap_or_default();
                if !reaped.is_empty() {
                    tracing::info!(count = reaped.len(), pids = ?reaped, "reaped dead child processes nobody waited for");
                }
            }
        });
    });
}

/// Reap `pid` if it has exited: `Ok(Some(description))` once it has, `Ok(None)` while it runs.
pub fn try_reap(pid: u32) -> io::Result<Option<(Option<i32>, String)>> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    let p = nix::unistd::Pid::from_raw(i32::try_from(pid).map_err(|_| io::Error::other("bad pid"))?);
    match waitpid(p, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => Ok(None),
        Ok(WaitStatus::Exited(_, code)) => Ok(Some((Some(code), format!("exit status: {code}")))),
        Ok(WaitStatus::Signaled(_, sig, _)) => Ok(Some((None, format!("signal: {sig}")))),
        Ok(other) => Ok(Some((None, format!("{other:?}")))),
        Err(e) => Err(io::Error::from(e)),
    }
}

/// How many PTY masters process `pid` holds open (its terminals), where the platform can tell.
pub fn pty_masters(pid: u32) -> Option<usize> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(
        fds.flatten()
            .filter(|e| std::fs::read_link(e.path()).is_ok_and(|t| t == Path::new("/dev/ptmx") || t == Path::new("/dev/pts/ptmx")))
            .count(),
    )
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn each_shell_gets_its_startup_file_and_line() {
        let home = Path::new("/home/me");
        let dir = home.join(".local/bin");
        let none = |_: &Path| false;
        let zsh = shell_path_line("/bin/zsh", true, home, None, &dir, none);
        assert_eq!(zsh.file, home.join(".zshrc"));
        assert_eq!(zsh.line, r#"case ":$PATH:" in *:"$HOME/.local/bin":*) ;; *) export PATH="$HOME/.local/bin":"$PATH" ;; esac"#);
        assert_eq!(zsh.now, r#"export PATH="$HOME/.local/bin":"$PATH""#);
        assert_eq!(shell_path_line("/usr/bin/zsh", false, home, Some(Path::new("/home/me/.zsh")), &dir, none).file, Path::new("/home/me/.zsh/.zshrc"));
        assert_eq!(shell_path_line("/bin/bash", false, home, None, &dir, none).file, home.join(".bashrc"));
        // macOS terminals start login shells: bash reads the first of these there is.
        assert_eq!(shell_path_line("/bin/bash", true, home, None, &dir, none).file, home.join(".bash_profile"));
        let profile = home.join(".profile");
        assert_eq!(shell_path_line("/bin/bash", true, home, None, &dir, |p: &Path| p == profile).file, profile);
        let fish = shell_path_line("/opt/homebrew/bin/fish", true, home, None, &dir, none);
        assert_eq!(fish.file, home.join(".config/fish/conf.d/kiln.fish"));
        assert_eq!(fish.line, r#"contains -- "$HOME/.local/bin" $PATH; or set -gx PATH "$HOME/.local/bin" $PATH"#);
        assert_eq!(shell_path_line("", false, home, None, &dir, none).file, profile);
        // Outside the home folder: quoted as it is.
        assert_eq!(shell_path_line("/bin/sh", false, home, None, Path::new("/opt/it's/bin"), none).now, r#"export PATH='/opt/it'\''s/bin':"$PATH""#);
    }

    #[test]
    fn the_line_goes_in_a_startup_file_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // A file that isn't there yet, in a folder that isn't either (fish's conf.d).
        let fresh = dir.path().join("conf.d/kiln.fish");
        assert!(add_marked_line(&fresh, "the line")?);
        assert_eq!(std::fs::read_to_string(&fresh)?, format!("{PATH_MARK}\nthe line\n"));
        assert!(!add_marked_line(&fresh, "the line")?, "once");
        // Someone's own file: kept, and the line set apart from it.
        let theirs = dir.path().join(".zshrc");
        std::fs::write(&theirs, "alias ll='ls -l'")?;
        assert!(add_marked_line(&theirs, "the line")?);
        assert_eq!(std::fs::read_to_string(&theirs)?, format!("alias ll='ls -l'\n\n{PATH_MARK}\nthe line\n"));
        assert_eq!(without_marked_line(&std::fs::read_to_string(&theirs)?), "alias ll='ls -l'\n", "taken back, the file is as it was");
        assert_eq!(without_marked_line(&std::fs::read_to_string(&fresh)?), "");
        Ok(())
    }

    /// The line itself, run by `sh`: the folder goes on PATH once, however often it runs.
    #[test]
    fn the_line_puts_the_folder_on_path_once() -> anyhow::Result<()> {
        let home = Path::new("/home/me");
        let line = shell_path_line("/bin/sh", false, home, None, &home.join(".local/bin"), |_| false).line;
        let script = format!("{line}\n{line}\nprintf %s \"$PATH\"");
        let out = std::process::Command::new("sh").arg("-c").arg(script).env("HOME", home).env("PATH", "/usr/bin:/bin").output()?;
        assert_eq!(String::from_utf8_lossy(&out.stdout), "/home/me/.local/bin:/usr/bin:/bin");
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_dead_child_nobody_waits_for_is_reaped_once_its_grace_is_over() -> anyhow::Result<()> {
        let child = std::process::Command::new("true").spawn()?;
        let pid = child.id();
        // Nobody waits for it.
        drop(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !is_zombie(pid) {
            anyhow::ensure!(Instant::now() < deadline, "the child never died");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!pid_alive(pid), "a zombie is not alive");
        assert!(zombie_children().contains(&pid));
        let mut seen = std::collections::HashMap::new();
        let t0 = Instant::now();
        // Within its grace it is left for a waiter; past it, reaped.
        assert!(reap_old(&[pid], &mut seen, Duration::from_secs(30), t0).is_empty());
        assert!(is_zombie(pid));
        assert_eq!(reap_old(&[pid], &mut seen, Duration::from_secs(30), t0 + Duration::from_secs(31)), [pid]);
        assert!(proc_stat(pid).is_none(), "gone from the process table");
        assert!(seen.is_empty());
        Ok(())
    }
}
