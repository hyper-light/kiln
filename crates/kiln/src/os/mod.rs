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

/// Child processes: [`crate::proc`]'s implementation.
#[cfg(unix)]
#[path = "proc_unix.rs"]
pub mod proc;
#[cfg(windows)]
#[path = "proc_windows.rs"]
pub mod proc;

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
