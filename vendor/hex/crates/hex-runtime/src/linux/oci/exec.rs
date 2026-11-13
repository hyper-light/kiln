//! `hex oci exec`: a program run in the foreground in a created or running container, the runc and
//! crun `exec` analog (docs/design/runtime.md, "the `hex` executor runs a leg's steps in a persistent
//! container via `hex oci exec`").
//!
//! The runtime execs itself as `hex __oci-exec`, so the helper is single-threaded, as joining a user or
//! mount namespace needs. It joins each of the container init's namespaces (`/proc/<pid>/ns/*`) that
//! differ from its own, user first, but the cgroup namespace. Then it clones a child into the
//! container's cgroup, as `create` does (`clone3(CLONE_INTO_CGROUP)` where it can), and through the
//! pid namespace it joined into the container's pid namespace. The child joins the cgroup namespace
//! from inside the cgroup and becomes the container's process, as its spec gives it and the
//! request overrides (environment, working directory, user), and execs the program on the caller's
//! stdin, stdout and stderr. The helper passes SIGHUP, SIGINT, SIGQUIT and SIGTERM on to it, and exits
//! with its status once it ends: its exit code, or 128 plus the signal that ended it, as a shell
//! reports one. The wait lasts as long as the program does, which the caller bounds by signalling.
//! Run by the node agent (seam mode), the program dies with the helper, so the agent ending the helper
//! ends the program too.

use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::pipe::PipeFlags;
use serde::{Deserialize, Serialize};

use crate::linux::oci::spec::{Process, Seccomp};
use crate::linux::oci::{OciError, init, sys};

/// The request the runtime hands `hex __oci-exec`, as JSON.
pub const ENV_REQUEST: &str = "HEX_OCI_EXEC";

/// What `hex oci exec` runs, and in which container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The container init's pid, whose namespaces the program joins.
    pub pid: i32,
    /// The container's bundle, whose `config.json` gives the process the program becomes.
    pub bundle: String,
    /// The container's cgroup directory, which the program is born in.
    pub cgroup: Option<String>,
    pub args: Vec<String>,
    /// Overrides the process's working directory.
    pub cwd: Option<String>,
    /// `K=V` entries, each overriding the process's entry for `K` or adding one.
    pub env: Vec<String>,
    /// Overrides the process's user: a uid, and a gid (the process's own when absent).
    pub user: Option<(u32, Option<u32>)>,
    /// Seam mode: the helper is spawned by the node agent and watched by its pidfd, not a shell, so
    /// a program ended by a signal makes the helper die by that same signal (the node reports
    /// `Signal(n)`), rather than exiting 128 + n as a shell reports. A program that exits normally
    /// exits the helper with its code either way. Absent in a request from the command line.
    #[serde(default)]
    pub seam: bool,
    /// The container's state directory, where its create kept the spec it checked: the program's process settings
    /// come from that spec, never the bundle's `config.json` as it is now (AC-T7.1.1). Absent in a request from
    /// before it was named; the bundle's spec is then checked again, with the same checks.
    #[serde(default)]
    pub dir: Option<String>,
}

/// A helper that fails before the program can start exits as a shell does for a program it can't run
/// (126), and for one that doesn't exist (127).
const CANNOT_RUN: u8 = 126;
const NOT_FOUND: u8 = 127;

/// The namespaces joined, in this order: user first, since it grants the capabilities joining the
/// rest needs; mount last, since it changes what every path names. The cgroup namespace is the child's
/// to join, once it's in the container's cgroup: where cgroup2 is mounted `nsdelegate`, a clone into a
/// cgroup needs the caller's own cgroup reachable from the caller's cgroup namespace
/// (kernel/cgroup/cgroup.c, `cgroup_procs_write_permission`), and from the container's, rooted at its
/// cgroup, the helper's isn't: joining it first made `clone3` fail with ENOENT.
const NAMESPACES: [(&str, i32); 7] = [
    ("user", libc::CLONE_NEWUSER),
    ("ipc", libc::CLONE_NEWIPC),
    ("uts", libc::CLONE_NEWUTS),
    ("net", libc::CLONE_NEWNET),
    ("pid", libc::CLONE_NEWPID),
    ("cgroup", libc::CLONE_NEWCGROUP),
    ("mnt", libc::CLONE_NEWNS),
];

/// The signals the helper passes on to the program.
const FORWARDED: [i32; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// The `hex __oci-exec` entry: the program's status, or 126 or 127 with why on stderr.
pub fn run() -> u8 {
    match helper() {
        Ok(status) => status,
        Err((status, e)) => {
            let _ = writeln!(std::io::stderr().lock(), "hex oci exec: {e}");
            status
        }
    }
}

fn helper() -> Result<u8, (u8, OciError)> {
    let cannot = |e| (CANNOT_RUN, e);
    let raw = std::env::var(ENV_REQUEST)
        .map_err(|e| cannot(OciError::os("reading the exec request", e)))?;
    let req: Request = serde_json::from_str(&raw)
        .map_err(|e| cannot(OciError::Broken(format!("the exec request: {e}"))))?;
    let spec = crate::linux::oci::checked::spec_of(
        req.dir.as_deref().map(Path::new),
        Path::new(&req.bundle),
    )
    .map_err(cannot)?
    .ok_or_else(|| {
        cannot(OciError::Config(
            "the container's spec is neither kept with its state nor in its bundle".to_owned(),
        ))
    })?;
    let process = process_for(spec.process.unwrap_or_default(), &req);

    // The cgroup is opened before any namespace changes, as the shim opens it (init.rs): its path
    // names nothing once the mount namespace is the container's.
    let cgroup = req
        .cgroup
        .as_deref()
        .map(|dir| open_cgroup(Path::new(dir)))
        .transpose()
        .map_err(cannot)?;
    let joins = namespaces_to_join(req.pid).map_err(cannot)?;
    // A birth in the cgroup is checked against the credentials the clone runs with, which a joined
    // user namespace has changed; there the program joins by migration (init.rs, the shim).
    let in_userns = joins.iter().any(|(kind, _, _)| *kind == "user");
    let mut cgroup_ns = None;
    for (kind, flag, fd) in joins {
        if kind == "cgroup" {
            cgroup_ns = Some(fd);
            continue;
        }
        sys::setns(fd.as_fd(), flag).map_err(|f| cannot(f.into()))?;
    }

    let signals = sys::forward_signals(&FORWARDED).map_err(cannot)?;
    // Seam mode: the node agent watches the helper as the program's stand-in and, when the caller
    // watching it goes away, ends it (decision 3's disconnect rule); the program dies with it
    // (`die_with_helper`), so none outlives that unwatched. The lifeline's write end stays with the
    // helper until it ends. Step 2 of goal 3 supersedes this: its seed makes the program the agent's
    // own child, ended by its own pidfd.
    let lifeline = if req.seam {
        Some(
            rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
                .map_err(|e| cannot(OciError::os("making the exec helper's lifeline", e)))?,
        )
    } else {
        None
    };
    let born = match &cgroup {
        Some((dir, _)) if !in_userns => sys::fork_into_cgroup(dir.as_fd()).map_err(cannot)?,
        _ => None,
    };
    let (forked, procs) = match (born, cgroup) {
        (Some(forked), _) => (forked, None),
        (None, cgroup) => (sys::fork().map_err(cannot)?, cgroup.map(|(_, procs)| procs)),
    };
    match forked {
        // The child returns only if it couldn't exec; its status leaves through `main`, as the
        // init's does. The container's seccomp profile binds an exec'd program too (runc and crun
        // apply it on exec): setns does not inherit the init's filter, so Hex reapplies it here.
        sys::Fork::Child => Err(child(
            &process,
            spec.linux.seccomp.as_ref(),
            procs,
            cgroup_ns,
            &signals,
            lifeline.map(|(read, _write)| read),
        )),
        sys::Fork::Parent(pid) => {
            let _lifeline = lifeline.map(|(_read, write)| write);
            drop((procs, cgroup_ns));
            wait_forwarding(pid, &signals, req.seam).map_err(cannot)
        }
    }
}

/// The container's process with the request's overrides: its program, working directory, user and
/// environment entries.
pub fn process_for(mut process: Process, req: &Request) -> Process {
    process.args = req.args.clone();
    if let Some(cwd) = &req.cwd {
        process.cwd = cwd.clone();
    }
    if let Some((uid, gid)) = req.user {
        process.user.uid = uid;
        process.user.gid = gid.unwrap_or(process.user.gid);
        // Supplementary groups belong to the user they were given for.
        process.user.additional_gids.clear();
    }
    for entry in &req.env {
        let key = entry.split_once('=').map_or(entry.as_str(), |(k, _)| k);
        let same_key = |e: &String| e.split_once('=').map_or(e.as_str(), |(k, _)| k) == key;
        match process.env.iter_mut().find(|e| same_key(e)) {
            Some(existing) => existing.clone_from(entry),
            None => process.env.push(entry.clone()),
        }
    }
    process
}

/// The cgroup's directory (for `clone3`) and its `cgroup.procs` (to join by migration where a birth
/// there can't be), opened now.
pub(crate) fn open_cgroup(dir: &Path) -> Result<(OwnedFd, OwnedFd), OciError> {
    let open = |path: &Path, flags: OFlags| {
        rustix::fs::open(path, flags | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| OciError::os("opening the container's cgroup", e))
    };
    Ok((
        open(dir, OFlags::RDONLY | OFlags::DIRECTORY)?,
        open(&dir.join("cgroup.procs"), OFlags::WRONLY)?,
    ))
}

/// Each of the init's namespaces that isn't this process's own, opened before any is joined: once the
/// mount namespace is joined, `/proc` is the container's.
pub(crate) fn namespaces_to_join(pid: i32) -> Result<Vec<(&'static str, i32, OwnedFd)>, OciError> {
    let mut out = Vec::new();
    for (kind, flag) in NAMESPACES {
        let theirs = rustix::fs::open(
            format!("/proc/{pid}/ns/{kind}"),
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| OciError::os("opening the container's namespace", e))?;
        let a = rustix::fs::fstat(&theirs).map_err(|e| OciError::os("stat of a namespace", e))?;
        let b = rustix::fs::stat(format!("/proc/self/ns/{kind}"))
            .map_err(|e| OciError::os("stat of a namespace", e))?;
        if (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino) {
            out.push((kind, flag, theirs));
        }
    }
    Ok(out)
}

/// The child: into its cgroup by migration when it wasn't born there, then into the container's cgroup
/// namespace (NAMESPACES says why only now), the signals unblocked, the container's process become,
/// bound to die with the helper when it has a `lifeline` (seam mode), then the program. Returns only
/// when one of those failed: the status to exit with, and why.
fn child(
    process: &Process,
    seccomp: Option<&Seccomp>,
    procs: Option<OwnedFd>,
    cgroup_ns: Option<OwnedFd>,
    signals: &sys::ForwardedSignals,
    lifeline: Option<OwnedFd>,
) -> (u8, OciError) {
    let setup = || -> Result<(), OciError> {
        if let Some(procs) = procs {
            let n = rustix::io::write(&procs, b"0")
                .map_err(|e| OciError::os("joining the container's cgroup", e))?;
            if n != 1 {
                return Err(OciError::os(
                    "joining the container's cgroup",
                    "a short write to cgroup.procs",
                ));
            }
        }
        if let Some(ns) = cgroup_ns {
            sys::setns(ns.as_fd(), libc::CLONE_NEWCGROUP)?;
        }
        signals.restore()?;
        init::become_process(process, seccomp)?;
        lifeline.as_ref().map_or(Ok(()), die_with_helper)
    };
    if let Err(e) = setup() {
        return (CANNOT_RUN, e);
    }
    let program = match process.args.first() {
        Some(p) => p,
        None => {
            return (
                CANNOT_RUN,
                OciError::Config("exec needs a program".to_owned()),
            );
        }
    };
    match init::resolve_program(program, &process.env) {
        Ok(path) if path.exists() => {}
        Ok(path) => {
            return (
                NOT_FOUND,
                OciError::Config(format!("{} doesn't exist in the container", path.display())),
            );
        }
        Err(e) => return (NOT_FOUND, e),
    }
    match init::exec(process) {
        Err(e) => (CANNOT_RUN, e),
        Ok(()) => (
            CANNOT_RUN,
            OciError::Broken("execve returned without an error".to_owned()),
        ),
    }
}

/// Makes the child die with its helper: KILL once the helper ends (PR_SET_PDEATHSIG, prctl(2)). Set
/// after the child has become the container's process, since a change of its user clears it (as does
/// exec'ing a set-user-ID program, a gap step 2's seed closes). A helper that ended before it was set
/// has closed the lifeline's last write end, so the child ends too, rather than outlive it.
fn die_with_helper(lifeline: &OwnedFd) -> Result<(), OciError> {
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
        .map_err(|e| OciError::os("binding the program to its exec helper", e))?;
    let mut fds = [PollFd::new(lifeline, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&now))
        .map_err(|e| OciError::os("looking for the exec helper", e))?;
    if fds
        .iter()
        .any(|f| f.revents().intersects(PollFlags::HUP | PollFlags::ERR))
    {
        return Err(OciError::Broken(
            "the exec helper ended before its program started".to_owned(),
        ));
    }
    Ok(())
}

/// Waits for the program, passing on each forwarded signal the helper receives, then reports its
/// status: a shell's (the exit code, or 128 + the signal), or in `seam` mode by ending the helper
/// the same way the program ended, so the node's pidfd watch sees the program's own outcome.
fn wait_forwarding(pid: i32, signals: &sys::ForwardedSignals, seam: bool) -> Result<u8, OciError> {
    let raw = rustix::process::Pid::from_raw(pid)
        .ok_or_else(|| OciError::os("clone", "the child's pid isn't a pid"))?;
    let pidfd = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty())
        .map_err(|e| OciError::os("opening the program's pidfd", e))?;
    loop {
        let mut fds = [
            PollFd::new(&pidfd, PollFlags::IN),
            PollFd::from_borrowed_fd(signals.fd(), PollFlags::IN),
        ];
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(OciError::os("waiting for the program", e)),
        }
        let [ended, signalled] = &fds;
        if signalled.revents().contains(PollFlags::IN) {
            let sig = signals.next()?;
            // A program that has just exited can't take it; the pidfd says so next.
            let _ = sys::kill(pid, sig, false);
        }
        if ended.revents().contains(PollFlags::IN) {
            let status = sys::waitpid(pid)?;
            if seam && let Some(sig) = status.term_signal() {
                // End the helper by the same signal, so the node's pidfd watch reads the program's
                // outcome and reports `Signal(sig)`. `raise_default` sets that signal to its default
                // and unblocks only it before raising, so a different forwarded signal left pending
                // can't be delivered first; a signal that can't be raised falls through to the shell
                // mapping below.
                let _ = sys::raise_default(sig);
            }
            return status_of(&status);
        }
    }
}

/// A shell's status for an ended process: its exit code, or 128 plus the signal that ended it.
pub fn status_of(status: &sys::WaitStatus) -> Result<u8, OciError> {
    let code = match (status.exit_code(), status.term_signal()) {
        (Some(code), _) => code,
        (None, Some(sig)) => sig
            .checked_add(128)
            .ok_or_else(|| OciError::os("the program's status", "a signal out of range"))?,
        (None, None) => {
            return Err(OciError::os(
                "the program's status",
                "neither exited nor signalled",
            ));
        }
    };
    u8::try_from(code).map_err(|_| OciError::os("the program's status", "a status out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(env: &[&str], cwd: Option<&str>, user: Option<(u32, Option<u32>)>) -> Request {
        Request {
            pid: 1,
            bundle: "/b".to_owned(),
            cgroup: None,
            args: vec!["sh".to_owned(), "-c".to_owned(), "true".to_owned()],
            cwd: cwd.map(str::to_owned),
            env: env.iter().map(|s| (*s).to_owned()).collect(),
            user,
            seam: false,
            dir: None,
        }
    }

    fn base() -> Process {
        let mut p = Process {
            cwd: "/".to_owned(),
            args: vec!["sleep".to_owned(), "infinity".to_owned()],
            env: vec!["PATH=/usr/bin".to_owned(), "HOME=/root".to_owned()],
            ..Process::default()
        };
        p.user.uid = 0;
        p.user.gid = 5;
        p.user.additional_gids = vec![10, 11];
        p
    }

    /// Exec takes the container's process and overrides only what the request names (docs/design/
    /// runtime.md, `hex oci exec`; AC-T7.1.4).
    #[test]
    fn exec_overrides_only_what_it_names() {
        let p = process_for(base(), &req(&[], None, None));
        assert_eq!(p.args, ["sh", "-c", "true"]);
        assert_eq!(p.cwd, "/");
        assert_eq!(p.env, ["PATH=/usr/bin", "HOME=/root"]);
        assert_eq!((p.user.uid, p.user.gid), (0, 5));
        assert_eq!(p.user.additional_gids, [10, 11]);
    }

    /// An env entry replaces the process's entry for its key, in place, or is added at the end; a
    /// working directory and a user replace the process's.
    #[test]
    fn exec_env_replaces_by_key_and_user_drops_supplementary_groups() {
        let p = process_for(
            base(),
            &req(&["HOME=/tmp", "FOO=bar"], Some("/work"), Some((1000, None))),
        );
        assert_eq!(p.env, ["PATH=/usr/bin", "HOME=/tmp", "FOO=bar"]);
        assert_eq!(p.cwd, "/work");
        assert_eq!((p.user.uid, p.user.gid), (1000, 5));
        assert!(p.user.additional_gids.is_empty());
        let p = process_for(base(), &req(&[], None, Some((1000, Some(1000)))));
        assert_eq!((p.user.uid, p.user.gid), (1000, 1000));
    }

    /// The request survives the trip to the helper unchanged.
    #[test]
    fn exec_request_round_trips_as_json() {
        let r = req(&["A=b"], Some("/x"), Some((7, Some(8))));
        let back: Request = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    /// Append-only (AC-T7.1.1): a request from before the state directory was named, as an older node writes it,
    /// reads as one naming none, which checks the bundle's spec again; one naming it reads back with it.
    #[test]
    fn an_older_request_reads_without_a_state_directory() {
        let older = r#"{"pid":1,"bundle":"/b","cgroup":null,"args":["true"],"cwd":null,"env":[],"user":null,"seam":true}"#;
        let read: Request = serde_json::from_str(older).unwrap();
        assert_eq!(read.dir, None);
        let named = Request {
            dir: Some("/run/hex-node/k-c".to_owned()),
            ..req(&[], None, None)
        };
        let back: Request = serde_json::from_str(&serde_json::to_string(&named).unwrap()).unwrap();
        assert_eq!(back, named);
    }
}
