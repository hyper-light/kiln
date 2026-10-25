//! The tracer (docs/ci.md W24-R05): what a unit whose tools keep no record of their inputs reads,
//! observed as it runs. On Linux, unprivileged: seccomp user notification. The command runs with
//! `no_new_privs` and a filter that notifies on the calls that take a path; the supervisor reads
//! each path from the tracee's memory, notes it, and lets the call go on as it would untraced
//! (`SECCOMP_USER_NOTIF_FLAG_CONTINUE`): it observes, never emulates. Every process the command
//! starts inherits the filter. Statically linked programs (Go's) are seen as any other, since the
//! kernel, not a library, is asked. The records are raw ([`Record`]); what each means for a key
//! is decided apart.
#![allow(unsafe_code)]

use serde::{Deserialize, Serialize};

/// One call a traced process made with a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: i32,
    /// The call: `openat`, `newfstatat`, `execve`, `getdents64`, `connect`, …
    pub call: String,
    /// The path it named, made absolute against the process's folder or the folder its
    /// descriptor names; for `getdents64`, the folder listed; empty for `connect`.
    pub path: String,
}

#[cfg(target_os = "linux")]
pub use linux::trace;

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus};

    use nix::libc;

    use super::Record;

    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JEQ_K: u16 = 0x15;
    const BPF_RET_K: u16 = 0x06;
    /// `seccomp_data`'s fields: the call's number, then the architecture.
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;

    /// The calls traced: each one's number, name, the argument holding its path, and the one
    /// holding the folder that path is relative to (an `*at` call's).
    fn calls() -> Vec<(i64, &'static str, usize, Option<usize>)> {
        let mut out = vec![
            (libc::SYS_openat, "openat", 1, Some(0)),
            (libc::SYS_openat2, "openat2", 1, Some(0)),
            (libc::SYS_newfstatat, "newfstatat", 1, Some(0)),
            (libc::SYS_statx, "statx", 1, Some(0)),
            (libc::SYS_faccessat, "faccessat", 1, Some(0)),
            (libc::SYS_faccessat2, "faccessat2", 1, Some(0)),
            (libc::SYS_readlinkat, "readlinkat", 1, Some(0)),
            (libc::SYS_execve, "execve", 0, None),
            (libc::SYS_execveat, "execveat", 1, Some(0)),
            (libc::SYS_chdir, "chdir", 0, None),
            (libc::SYS_getdents64, "getdents64", 0, None),
            (libc::SYS_connect, "connect", 0, None),
        ];
        #[cfg(target_arch = "x86_64")]
        out.extend([
            (libc::SYS_open, "open", 0, None),
            (libc::SYS_stat, "stat", 0, None),
            (libc::SYS_lstat, "lstat", 0, None),
            (libc::SYS_access, "access", 0, None),
            (libc::SYS_readlink, "readlink", 0, None),
        ]);
        out
    }

    fn stmt(code: u16, k: u32) -> libc::sock_filter {
        libc::sock_filter { code, jt: 0, jf: 0, k }
    }

    /// The filter: another architecture's calls go on; the traced calls notify; the rest go on.
    fn program() -> Vec<libc::sock_filter> {
        let nrs: Vec<u32> = calls().iter().filter_map(|(nr, ..)| u32::try_from(*nr).ok()).collect();
        let mut p = vec![stmt(BPF_LD_W_ABS, ARCH), libc::sock_filter { code: BPF_JEQ_K, jt: 1, jf: 0, k: AUDIT_ARCH }, stmt(BPF_RET_K, libc::SECCOMP_RET_ALLOW), stmt(BPF_LD_W_ABS, NR)];
        let n = nrs.len();
        for (i, nr) in nrs.iter().enumerate() {
            // To the notifying return: past the rest of the comparisons and the allowing one.
            p.push(libc::sock_filter { code: BPF_JEQ_K, jt: u8::try_from(n - i).unwrap_or(u8::MAX), jf: 0, k: *nr });
        }
        p.push(stmt(BPF_RET_K, libc::SECCOMP_RET_ALLOW));
        p.push(stmt(BPF_RET_K, libc::SECCOMP_RET_USER_NOTIF));
        p
    }

    /// Runs `cmd` under the tracer: how it exited, and the paths it and every process it started
    /// named, in the order the calls were made.
    pub fn trace(cmd: &mut Command) -> anyhow::Result<(ExitStatus, Vec<Record>)> {
        let filter = program();
        let (filter_at, filter_len) = (filter.as_ptr() as usize, u16::try_from(filter.len())?);
        let (parent, child) = UnixStream::pair()?;
        let child_fd = child.as_raw_fd();
        // Between fork and exec, the child makes system calls only, on what's made here. It
        // installs the filter, says which descriptor is its listener, and waits to be told the
        // supervisor has it before it execs (an exec notifies, which only a listener answers).
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let prog = libc::sock_fprog { len: filter_len, filter: filter_at as *mut libc::sock_filter };
                let fd = libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, libc::SECCOMP_FILTER_FLAG_NEW_LISTENER, &raw const prog);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let said: [i32; 2] = [libc::getpid(), fd as i32];
                if libc::write(child_fd, said.as_ptr().cast(), std::mem::size_of_val(&said)) != 8 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut ack = 0u8;
                if libc::read(child_fd, (&raw mut ack).cast(), 1) != 1 {
                    return Err(std::io::Error::other("the tracer's supervisor went away"));
                }
                libc::close(fd as i32);
                Ok(())
            });
        }
        let supervisor = std::thread::spawn(move || supervise(parent));
        let spawned = cmd.spawn();
        drop(child);
        let status = spawned.and_then(|mut c| c.wait());
        let records = supervisor.join().map_err(|_| anyhow::anyhow!("the tracer's supervisor stopped"))?;
        drop(filter);
        Ok((status?, records?))
    }

    /// The supervisor: takes the child's listener, then answers every notification until no
    /// process holds the filter.
    fn supervise(mut sock: UnixStream) -> anyhow::Result<Vec<Record>> {
        let mut said = [0u8; 8];
        if sock.read_exact(&mut said).is_err() {
            return Ok(Vec::new());
        }
        let word = |at: usize| said.get(at..at + 4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(i32::from_ne_bytes).unwrap_or(-1);
        let (pid, fd) = (word(0), word(4));
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        anyhow::ensure!(pidfd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as i32) };
        let listener = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd, 0) };
        anyhow::ensure!(listener >= 0, "pidfd_getfd: {}", std::io::Error::last_os_error());
        let listener = unsafe { OwnedFd::from_raw_fd(listener as i32) };
        sock.write_all(&[1])?;
        let table = calls();
        let mut out = Vec::new();
        loop {
            let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&raw mut pfd, 1, 1000) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("poll: {}", std::io::Error::last_os_error());
            }
            if pfd.revents & libc::POLLIN == 0 {
                if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    break;
                }
                continue;
            }
            let mut req: libc::seccomp_notif = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(listener.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_RECV, &raw mut req) } != 0 {
                // Its process went away between the poll and the read.
                continue;
            }
            let rec = record(&table, &req);
            let mut id = req.id;
            let valid = unsafe { libc::ioctl(listener.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_ID_VALID, &raw mut id) } == 0;
            let mut resp = libc::seccomp_notif_resp { id: req.id, val: 0, error: 0, flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32 };
            unsafe { libc::ioctl(listener.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_SEND, &raw mut resp) };
            if valid && let Some(r) = rec {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// What a notification says: its call, and the path it names, absolute.
    fn record(table: &[(i64, &'static str, usize, Option<usize>)], req: &libc::seccomp_notif) -> Option<Record> {
        let (_, call, path_arg, dir_arg) = table.iter().find(|(nr, ..)| i64::from(req.data.nr) == *nr)?;
        let pid = i32::try_from(req.pid).ok()?;
        let arg = |i: usize| req.data.args.get(i).copied().unwrap_or(0);
        let path = match *call {
            "connect" => String::new(),
            "getdents64" => link(pid, &format!("fd/{}", arg(0) as i32)).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            _ => {
                let given = read_str(pid, arg(*path_arg))?;
                let p = Path::new(&given);
                if p.is_absolute() {
                    given
                } else {
                    let base = match dir_arg.map(|i| arg(i) as i32) {
                        Some(d) if d != libc::AT_FDCWD => link(pid, &format!("fd/{d}")),
                        _ => link(pid, "cwd"),
                    };
                    base.map_or(given.clone(), |b| b.join(p).to_string_lossy().into_owned())
                }
            }
        };
        Some(Record { pid, call: (*call).to_owned(), path })
    }

    /// What `/proc/<pid>/<what>` links to.
    fn link(pid: i32, what: &str) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/{what}")).ok()
    }

    /// The NUL-ended string at `addr` in `pid`'s memory, up to 4,096 bytes.
    fn read_str(pid: i32, addr: u64) -> Option<String> {
        if addr == 0 {
            return None;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut at = addr;
        while out.len() < 4096 {
            let in_page = 4096 - usize::try_from(at % 4096).unwrap_or(0);
            let mut buf = vec![0u8; in_page.min(4096 - out.len())];
            let local = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
            let remote = libc::iovec { iov_base: at as *mut libc::c_void, iov_len: buf.len() };
            let n = unsafe { libc::process_vm_readv(pid, &raw const local, 1, &raw const remote, 1, 0) };
            let Ok(n) = usize::try_from(n) else { break };
            if n == 0 {
                break;
            }
            let got = buf.get(..n)?;
            if let Some(end) = got.iter().position(|b| *b == 0) {
                out.extend_from_slice(got.get(..end)?);
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            out.extend_from_slice(got);
            at = at.saturating_add(n as u64);
        }
        (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn on_path(name: &str) -> Option<PathBuf> {
            std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
        }

        /// W24-R05: a statically linked Go program and a dynamically linked rustc, each reading a
        /// file under the tracer: both reads are recorded, and each runs as it would untraced.
        #[test]
        fn w24_r05_a_static_go_binary_and_a_dynamic_rustc_read_under_the_tracer() -> anyhow::Result<()> {
            let d = tempfile::tempdir()?;
            let data = d.path().join("data.txt");
            std::fs::write(&data, "seen\n")?;
            match on_path("go") {
                Some(go) => {
                    std::fs::write(d.path().join("main.go"), "package main\n\nimport \"os\"\n\nfunc main() {\n\tb, err := os.ReadFile(os.Args[1])\n\tif err != nil {\n\t\tos.Exit(3)\n\t}\n\tos.Stdout.Write(b)\n}\n")?;
                    let built = Command::new(go).args(["build", "-o", "reader", "main.go"]).env("CGO_ENABLED", "0").env("GOCACHE", d.path().join("go-cache")).env("GOPATH", d.path().join("go-path")).env("GOFLAGS", "-mod=mod").current_dir(d.path()).output()?;
                    anyhow::ensure!(built.status.success(), "go build: {}", String::from_utf8_lossy(&built.stderr));
                    let file = Command::new("file").arg(d.path().join("reader")).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
                    anyhow::ensure!(file.is_empty() || file.contains("statically linked"), "{file}");
                    let (status, recs) = trace(Command::new(d.path().join("reader")).arg(&data).stdout(std::process::Stdio::null()))?;
                    anyhow::ensure!(status.success(), "the Go reader under the tracer: {status}");
                    anyhow::ensure!(recs.iter().any(|r| r.call.starts_with("open") && Path::new(&r.path) == data), "Go's read: {recs:?}");
                }
                None => eprintln!("w24_r05: no `go` on PATH: the static binary's half is skipped"),
            }
            let src = d.path().join("lib.rs");
            std::fs::write(&src, "pub fn f() -> u32 { 1 }\n")?;
            let mut rustc = Command::new("rustc");
            rustc.args(["--crate-type", "lib", "--emit", "metadata", "--crate-name", "traced", "-o"]).arg(d.path().join("traced.rmeta")).arg("lib.rs").current_dir(d.path());
            let (status, recs) = trace(&mut rustc)?;
            anyhow::ensure!(status.success(), "rustc under the tracer: {status}");
            anyhow::ensure!(recs.iter().any(|r| r.call.starts_with("open") && Path::new(&r.path) == src), "rustc's read of its source: {:?}", recs.iter().filter(|r| r.path.contains("lib.rs")).collect::<Vec<_>>());
            anyhow::ensure!(recs.iter().any(|r| r.call == "execve"), "the exec is seen too");
            Ok(())
        }
    }
}
