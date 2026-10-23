//! The `firecracker` executor's jail (docs/ci.md W13-R05): each VMM confined as Firecracker's
//! jailer confines one, without root. `kiln vmm-jail <spec>`, run fresh (so with one thread),
//! makes namespaces of its own (user, mount, PID, network, IPC and UTS) and a root of its own on a
//! tmpfs, holding only what its microVM needs: Firecracker, the guest's kernel, its disks, its
//! vsock sockets' directory, `/dev/kvm`, `/dev/urandom` and `/dev/null`. It moves into that root
//! and runs Firecracker there, as PID 1 of its PID namespace, with:
//!
//! - no network at all, not even the host's loopback;
//! - none of the host's processes, and none of its files but those;
//! - no new privileges;
//! - an end with its jail's: the runner's handle is on the jail, and a jail that goes takes its
//!   VMM with it.
//!
//! Root in its user namespace is the runner's user outside it, so what it writes (its sockets, its
//! scratch disk) is the runner's.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Context as _;
use nix::mount::{MntFlags, MsFlags, mount};
use serde::{Deserialize, Serialize};

/// A jail, as the runner asks for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spec {
    /// An empty directory of the host's: the jail's root is a tmpfs mounted on it.
    pub root: PathBuf,
    /// What's in there.
    pub binds: Vec<Bind>,
    /// The program (a path in the jail) and its arguments; none to only make the jail, which
    /// says whether jails can be made here.
    pub argv: Vec<String>,
}

/// A host path, where it is in the jail, and whether the jail may write it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bind {
    pub from: PathBuf,
    pub to: String,
    pub rw: bool,
}

/// `kiln vmm-jail <spec>`: a jail of its own, then the program in it; its exit code is the
/// program's.
pub fn main() -> std::process::ExitCode {
    let spec = std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("`kiln vmm-jail <spec>`: the jail's spec, a file")).and_then(|p| Ok(serde_json::from_slice::<Spec>(&std::fs::read(&p)?)?));
    match spec.and_then(|s| enter(&s)) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            eprintln!("kiln vmm-jail: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The namespaces made, and the jail's first process forked into its PID namespace; this one
/// waits for it, and says how it ended.
fn enter(spec: &Spec) -> anyhow::Result<u8> {
    use nix::sched::{CloneFlags, unshare};
    use nix::sys::wait::{WaitStatus, waitpid};
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    let flags = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNET | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS;
    // A refusal that passes (EINVAL while another thread of this process is still ending) is
    // tried again a few times.
    let mut tries = 0;
    loop {
        match unshare(flags) {
            Ok(()) => break,
            Err(nix::errno::Errno::EINVAL) if tries < 10 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                let threads = std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count);
                anyhow::bail!("namespaces of its own (this machine may not allow unprivileged user namespaces): {e} ({threads} threads)");
            }
        }
    }
    // Root in its user namespace, the runner's user outside it.
    std::fs::write("/proc/self/setgroups", "deny").context("its groups")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1")).context("its user")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1")).context("its group")?;
    #[allow(unsafe_code)]
    // SAFETY: `kiln vmm-jail` runs before anything starts a thread, so this process has one, and
    // the child may go on as this program does.
    let forked = unsafe { nix::unistd::fork() }.context("forking into the jail")?;
    match forked {
        nix::unistd::ForkResult::Child => {
            let e = inside(spec);
            eprintln!("kiln vmm-jail: {e:#}");
            #[allow(clippy::exit)]
            std::process::exit(1)
        }
        nix::unistd::ForkResult::Parent { child } => loop {
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => return Ok(u8::try_from(code).unwrap_or(1)),
                Ok(WaitStatus::Signaled(..)) => return Ok(1),
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(e) => return Err(anyhow::anyhow!("waiting for the jail's program: {e}")),
            }
        },
    }
}

/// PID 1 of the jail's PID namespace: its root made and moved into, then the program run (or,
/// for a probe, its end). Only what goes wrong comes back.
fn inside(spec: &Spec) -> anyhow::Error {
    if let Err(e) = make_root(spec) {
        return e;
    }
    let Some((program, args)) = spec.argv.split_first() else {
        #[allow(clippy::exit)]
        std::process::exit(0)
    };
    use std::os::unix::process::CommandExt as _;
    let e = std::process::Command::new(program).args(args).env_clear().exec();
    anyhow::Error::from(e).context(format!("running {program} in the jail"))
}

/// The jail's root: a tmpfs on `spec.root` with only its binds, a `/proc` of its PID namespace, the
/// host's root detached, and no new privileges from here.
fn make_root(spec: &Spec) -> anyhow::Result<()> {
    // Gone with the jail's first process outside it: the runner's handle is on that one.
    nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL).context("ending with its jail")?;
    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>).context("its mounts its own")?;
    let root = &spec.root;
    mount(Some("tmpfs"), root, Some("tmpfs"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=755,size=4m")).context("its root")?;
    for b in &spec.binds {
        let to = root.join(b.to.trim_start_matches('/'));
        anyhow::ensure!(to.starts_with(root) && !b.to.split('/').any(|p| p == ".."), "`{}` isn't a path in the jail", b.to);
        if std::fs::metadata(&b.from).with_context(|| format!("{}", b.from.display()))?.is_dir() {
            std::fs::create_dir_all(&to)?;
        } else {
            if let Some(d) = to.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::File::create(&to)?;
        }
        mount(Some(&b.from), &to, None::<&str>, MsFlags::MS_BIND, None::<&str>).with_context(|| format!("{} at {} in the jail", b.from.display(), b.to))?;
        if !b.rw {
            read_only(&to).with_context(|| format!("{} read-only", b.to))?;
        }
    }
    let proc = root.join("proc");
    std::fs::create_dir_all(&proc)?;
    mount(Some("proc"), &proc, Some("proc"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC, None::<&str>).context("its /proc")?;
    // Into it, the host's root detached.
    nix::unistd::chdir(root)?;
    nix::unistd::pivot_root(".", ".").context("moving into its root")?;
    nix::mount::umount2(".", MntFlags::MNT_DETACH).context("leaving the host's root")?;
    nix::unistd::chdir("/")?;
    // Nothing more is made in its root: what the program writes goes to its binds.
    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV, None::<&str>).context("its root read-only")?;
    let _ = nix::unistd::sethostname("kiln-vmm");
    nix::sys::prctl::set_no_new_privs().context("no new privileges")?;
    Ok(())
}

/// A bind mount made read-only, keeping the flags its source's mount has: in a user namespace
/// they're locked, and a remount without them is refused.
fn read_only(at: &Path) -> anyhow::Result<()> {
    use nix::sys::statvfs::{FsFlags, statvfs};
    let has = statvfs(at)?.flags();
    let mut flags = MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY;
    for (f, m) in [
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        // Linux's ST_RELATIME, which nix names on glibc but not on musl (kiln's static builds).
        (FsFlags::from_bits_retain(4096), MsFlags::MS_RELATIME),
    ] {
        if has.contains(f) {
            flags |= m;
        }
    }
    mount(None::<&str>, at, None::<&str>, flags, None::<&str>)?;
    Ok(())
}

/// Whether jails can be made here: a jail made and left at once, once per process. `false`, and
/// microVMs run unjailed, where this machine allows no unprivileged user namespaces, or
/// `KILN_FIRECRACKER_JAIL=off` says not to.
pub fn available() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| {
        if std::env::var("KILN_FIRECRACKER_JAIL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off")) {
            return false;
        }
        let dir = std::env::temp_dir().join(format!("kiln-jail-probe-{}", std::process::id()));
        let probe = || -> anyhow::Result<bool> {
            let root = dir.join("root");
            std::fs::create_dir_all(&root)?;
            let spec = dir.join("jail.json");
            std::fs::write(&spec, serde_json::to_vec(&Spec { root, binds: Vec::new(), argv: Vec::new() })?)?;
            let out = std::process::Command::new(std::env::current_exe()?).arg("vmm-jail").arg(&spec).stdin(std::process::Stdio::null()).output()?;
            if !out.status.success() {
                tracing::info!(why = %String::from_utf8_lossy(&out.stderr).trim(), "microVMs run unjailed here: no jail could be made");
            }
            Ok(out.status.success())
        };
        let works = probe().unwrap_or(false);
        let _ = std::fs::remove_dir_all(&dir);
        works
    })
}
