//! `hex guest`: what runs in a Shards guest (docs/design/shards-guest.md §3 and §4).
//!
//!   hex guest relay --node <socket> (--vsock | --unix <dir>) [--staged <dir>]
//!
//! The guest's init is the `hex` binary itself as its initramfs's `/init`: run as PID 1, `hex` is the
//! guest's init whatever words the kernel passes it ([`init_main`]), as Shards appends its own to a
//! command line (` quiet` where no one reads the console, `crates/vmm/src/vm/mod.rs:1007-1018` at
//! `a0cced87`), and words after a `--` are init's. It first moves itself off the kernel's initramfs onto a tmpfs root, as
//! switch_root does, since a container's start pivots its root and pivot_root(2) refuses to leave the
//! initramfs ("EINVAL The current root directory is on the rootfs"). It then mounts what a Linux node
//! needs (`/proc`, `/sys`, cgroup2, `/dev`, a tmpfs `/run` and `/tmp`; the node's state is under `/run`,
//! as the guest has no disk: docs/design/shards-guest.md §4), mounts the Shards shares the kernel's
//! command line names (`hex.shares=rw,ro,…`, §4a) at `/run/hex/shares/<i>`, gives the node the descriptor
//! limit `hex node --descriptors` counts (soft and hard, `fs.nr_open` raised to it where lower), starts `hex node`
//! and then `hex guest relay --vsock`, each within the boot's deadline where its host gave one (Firecracker's
//! `hex.firecracker-ready-ms`, a Shards host's `hex.ready-ms`), which it hands them (`BOOT_READY_BY`), says
//! `hex guest ready` on the console, and reaps every process that ends,
//! as PID 1 must. When the node or the relay ends, it says why and powers the guest off, so the host sees
//! the guest end.
//!
//! Under shards-init (docs/design/node-under-shards-init.md), the same init is a Shards run's own command under
//! Shards' nesting grant (D234): PID 1 of the workload's own PID namespace, which it knows by `NOTIFY_SOCKET`. There
//! shards-init has made the root and its mounts, the run's shares and its descriptor limit. So init mounts a tmpfs for
//! its node's state and staged roots, takes the deadline shards-init gives (`SHARDS_READY_BY`), and starts the node and
//! the relay, which listens on AF_UNIX sockets in `/run/hex/channel` (its `--unix`). It then says READY=1 on
//! shards-init's socket (sd_notify(3)) and reaps as PID 1 of its namespace. When the node or the relay ends, it exits,
//! and its namespace and its run end with it.
//!
//! With `--vsock` it listens on the guest's vsock ports ([`hex_core::stream::CONTROL_PORT`],
//! [`hex_core::stream::STDIO_PORT`] and [`hex_core::stream::files::FILES_PORT`], any CID); with `--unix` on
//! `control.sock`, `stdio.sock` and `files.sock` in `<dir>`, which tests dial as the host would. It stages
//! boxes' roots under `--staged` (default `/run/hex/staged`, made if absent; §4a). Once all listen and its node
//! has told it its bounds, within the boot's deadline where init handed one, it says `ready` on stdout, and it
//! serves until a listener fails, when it says why on stderr and exits 1.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_client::guest::Duplex;
use hex_core::stream;
use hex_node::relay::{Conn, Limits, Listen, Relay, STAGED};
use socket2::{Domain, SockAddr, Socket, Type};

/// Runs `hex guest`'s arguments (everything after `guest`); returns the exit status.
pub fn main(args: &[String]) -> u8 {
    match relay(args) {
        Ok(()) => 0,
        Err((status, why)) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(format!("hex guest: {why}\n").as_bytes());
            status
        }
    }
}

const USAGE: &str =
    "usage: hex guest relay --node <socket> (--vsock | --unix <dir>) [--staged <dir>]";

/// Where the relay listens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum On {
    Vsock,
    Unix(PathBuf),
}

/// What `hex guest relay` was given.
struct Args {
    node: PathBuf,
    on: On,
    staged: PathBuf,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut rest = args.iter();
    if rest.next().map(String::as_str) != Some("relay") {
        return Err(USAGE.to_owned());
    }
    let (mut node, mut on, mut staged) = (None, None, None);
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        let given = match flag.as_str() {
            "--node" => node.replace(PathBuf::from(value()?)).is_some(),
            "--vsock" => on.replace(On::Vsock).is_some(),
            "--unix" => on.replace(On::Unix(PathBuf::from(value()?))).is_some(),
            "--staged" => staged.replace(PathBuf::from(value()?)).is_some(),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        };
        if given {
            return Err(format!(
                "{flag} is given twice, or with another place\n{USAGE}"
            ));
        }
    }
    Ok(Args {
        node: node.ok_or_else(|| format!("--node is needed\n{USAGE}"))?,
        on: on.ok_or_else(|| format!("--vsock or --unix is needed\n{USAGE}"))?,
        staged: staged.unwrap_or_else(|| PathBuf::from(STAGED)),
    })
}

fn relay(args: &[String]) -> Result<(), (u8, String)> {
    let Args { node, on, staged } = parse(args).map_err(|why| (2, why))?;
    std::fs::create_dir_all(&staged)
        .map_err(|e| (1, format!("making {}: {e}", staged.display())))?;
    type Three = (Box<dyn Listen>, Box<dyn Listen>, Box<dyn Listen>);
    let (control, stdio, files): Three = match &on {
        On::Vsock => (
            Box::new(vsock(stream::CONTROL_PORT).map_err(|e| (1, e))?),
            Box::new(vsock(stream::STDIO_PORT).map_err(|e| (1, e))?),
            Box::new(vsock(stream::files::FILES_PORT).map_err(|e| (1, e))?),
        ),
        On::Unix(dir) => (
            Box::new(unix(&dir.join("control.sock")).map_err(|e| (1, e))?),
            Box::new(unix(&dir.join("stdio.sock")).map_err(|e| (1, e))?),
            Box::new(unix(&dir.join("files.sock")).map_err(|e| (1, e))?),
        ),
    };
    let stamps: Box<dyn Listen> = match &on {
        On::Vsock => Box::new(vsock(stream::STAMPS_PORT).map_err(|e| (1, e))?),
        On::Unix(dir) => Box::new(unix(&dir.join("stamps.sock")).map_err(|e| (1, e))?),
    };
    let line = stamps_line(std::env::var(BOOT_STAMPS).ok().as_deref(), boottime_ns());
    std::thread::Builder::new()
        .name("hex-relay-stamps".to_owned())
        .spawn(move || serve_stamps(stamps, line))
        .map_err(|e| (1, format!("starting the stamps thread: {e}")))?;
    // Its bounds are its node's, which the guest's init started and saw ready before it: asked before it says ready,
    // within the boot's deadline where init handed one, so a node busy past its hello's learned wait (initializing
    // the guest's GPU) ends no boot that has time left (AC-T7.4.1).
    let now = Instant::now();
    let ready_by = boot_deadline(
        std::env::var_os(BOOT_READY_BY).as_deref(),
        now,
        boottime_ns(),
    )
    .map_err(|why| (1, why))?;
    let limits = Limits::asked(&node, ready_by)
        .map_err(|why| (1, format!("asking the node its bounds: {why}")))?;
    let mut out = std::io::stdout().lock();
    out.write_all(b"ready\n")
        .and_then(|()| out.flush())
        .map_err(|e| (1, format!("saying it is ready: {e}")))?;
    drop(out);
    Relay::new(&node, &staged, limits)
        .serve(control, stdio, files)
        .map_err(|why| (1, why.to_string()))
}

fn unix(path: &Path) -> Result<UnixListener, String> {
    UnixListener::bind(path).map_err(|e| format!("listening on {}: {e}", path.display()))
}

/// The guest's listener on vsock `port`, from any CID (the host's is the only other on its device).
fn vsock(port: u32) -> Result<VsockListener, String> {
    let fail = |e: io::Error| format!("listening on vsock port {port:#x}: {e}");
    let socket = Socket::new(Domain::VSOCK, Type::STREAM.cloexec(), None).map_err(fail)?;
    // VMADDR_CID_ANY (vsock(7)).
    socket
        .bind(&SockAddr::vsock(u32::MAX, port))
        .map_err(fail)?;
    // The most the kernel queues: listen(2) truncates a larger backlog to `net.core.somaxconn`
    // (net/socket.c, `__sys_listen_socket`), so asking for `i32::MAX` takes the guest's own.
    socket.listen(i32::MAX).map_err(fail)?;
    Ok(VsockListener(socket))
}

struct VsockListener(Socket);

impl Listen for VsockListener {
    fn accept(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(Vsock(self.0.accept()?.0)))
    }
}

/// A vsock connection, as the relay's connections are.
struct Vsock(Socket);

impl std::os::fd::AsFd for Vsock {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Read for Vsock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Vsock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Duplex for Vsock {
    fn try_clone(&self) -> io::Result<hex_client::guest::DuplexOwner> {
        hex_client::guest::own_duplex(Self(self.0.try_clone()?))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    fn handle(&self) -> hex_client::guest::Handle<'_> {
        std::os::fd::AsFd::as_fd(&self.0)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

/// The variables that carry init's boot stamps: its first entry's, across its re-exec from the initramfs,
/// and all three to the relay (its first entry, its mounts done, its node ready).
const BOOT_T0: &str = "HEX_BOOT_T0";
const BOOT_STAMPS: &str = "HEX_BOOT_STAMPS";
/// The variable that carries the boot's deadline from init to each child it starts before the guest is ready, the
/// node and the relay (AC-T7.4.1): the instant on the guest's own clock, `CLOCK_BOOTTIME` nanoseconds, by which its
/// host waits for it to be ready, its readiness bound ([`boot_budget`]) counted from the kernel's start, so the time
/// the boot has spent is what the clock reads. The relay's calls to its node take it as their caller's.
const BOOT_READY_BY: &str = "HEX_BOOT_READY_BY";

/// `ready_by`, an instant on the guest's clock in nanoseconds, as a monotonic one: `now`, and as much after it as the
/// clock, at `boot_ns` (read after `now`, so the conversion never lends time), has left to it; `now` itself once the
/// clock has passed it; none where that is past what an instant holds.
fn deadline_at(ready_by: u128, now: Instant, boot_ns: u128) -> Option<Instant> {
    // Nothing left once the guest's clock has passed it: the boot's deadline has gone by.
    let left = u64::try_from(ready_by.saturating_sub(boot_ns)).ok()?;
    now.checked_add(Duration::from_nanos(left))
}

/// The boot's deadline init handed the relay (`given`, [`BOOT_READY_BY`]) as a monotonic instant ([`deadline_at`]);
/// none where init handed none, as outside a guest.
fn boot_deadline(
    given: Option<&OsStr>,
    now: Instant,
    boot_ns: u128,
) -> Result<Option<Instant>, String> {
    let Some(given) = given else {
        return Ok(None);
    };
    let bad =
        || format!("{BOOT_READY_BY} is {given:?}, not a CLOCK_BOOTTIME instant in nanoseconds");
    let at = clock_ns(given).ok_or_else(bad)?;
    deadline_at(at, now, boot_ns).map(Some).ok_or_else(bad)
}

/// `text` as an instant on the guest's clock in nanoseconds: decimal digits alone, within a `u128`.
fn clock_ns(text: &OsStr) -> Option<u128> {
    text.to_str()
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u128>().ok())
}

/// Nanoseconds since the kernel began keeping time: `CLOCK_BOOTTIME` (clock_gettime(2)), the guest's own
/// clock from its boot, which the boot split reads phases by.
fn boottime_ns() -> u128 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let (secs, nanos) = (
        u64::try_from(t.tv_sec).unwrap_or_default(),
        u32::try_from(t.tv_nsec).unwrap_or_default(),
    );
    Duration::new(secs, nanos).as_nanos()
}

/// The relay's stamps line: init's three stamps as init handed them, then the relay's own; `-` for each of
/// init's where it handed none (a relay outside a guest) or handed them unreadable.
fn stamps_line(from_init: Option<&str>, relay_ns: u128) -> String {
    let init = from_init.filter(|given| {
        let words: Vec<&str> = given.split(' ').collect();
        words.len() == 3 && words.iter().all(|w| w.parse::<u128>().is_ok())
    });
    format!("{} {relay_ns}\n", init.unwrap_or("- - -"))
}

/// Serves the boot's stamps on `listener`: each connection is sent `line` and closed, one at a time.
fn serve_stamps(listener: Box<dyn Listen>, line: String) {
    loop {
        match listener.accept() {
            Ok(mut conn) => {
                let _ = conn.write_all(line.as_bytes());
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Where the guest keeps the node's socket, state and boxes.
const RUN: &str = "/run/hex";
const STATE: &str = "/run/hex/state";
const BOX_ROOT: &str = "/sys/fs/cgroup/hex";
/// The controllers the node's boxes are bounded by (`docs/design/runtime.md`), enabled for the root's
/// children.
const CONTROLLERS: &str = "+cpu +io +memory +pids";
/// Where the relay listens under a service manager: AF_UNIX sockets its host's channel reaches, as a workload under
/// Docker's default filter opens no vsock socket (docs/design/node-under-shards-init.md §2, §5; Shards' D234 f27).
const CHANNEL: &str = "/run/hex/channel";

/// The variable by which a service manager names the socket its service says it is ready on (sd_notify(3)):
/// shards-init's `/run/shards/notify` under Shards' nesting grant (D234). Only a service manager sets it, so it alone
/// says init's host is one (docs/design/node-under-shards-init.md §3).
const NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";
/// The boot's deadline a service manager gives (D234): an instant on the guest's `CLOCK_BOOTTIME` in nanoseconds, the
/// clock, unit and form of [`BOOT_READY_BY`], counted from its run's arrival; 0 for one that passed before it arrived.
const SHARDS_READY_BY: &str = "SHARDS_READY_BY";

/// Who started init (docs/design/node-under-shards-init.md §3), read once as it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    /// The VM's kernel: init is the VM's own init (Shards' direct boot, Firecracker), its host's words on the
    /// kernel's command line.
    Kernel,
    /// A service manager: shards-init, whose run's own command init is under Shards' nesting grant (D234), PID 1 of
    /// the workload's own PID namespace. It names the socket init says it is ready on, and the boot's deadline where
    /// its run gives one.
    Manager {
        notify: PathBuf,
        ready_by: Option<u128>,
    },
}

/// Init's host, from the words only each host sets (`var`, init's environment): a service manager where
/// [`NOTIFY_SOCKET`] names an absolute path (sd_notify(3)'s file-system form, the one shards-init gives), with
/// [`SHARDS_READY_BY`]'s deadline where it is given; the kernel where [`NOTIFY_SOCKET`] is absent. Refused, naming the
/// word: a socket that isn't an absolute path, a deadline that isn't nanoseconds on the guest's clock, or a deadline
/// with no service manager to have given it.
fn host(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Host, String> {
    let ready_by = var(SHARDS_READY_BY);
    let Some(notify) = var(NOTIFY_SOCKET) else {
        return match ready_by {
            None => Ok(Host::Kernel),
            Some(given) => Err(format!(
                "{SHARDS_READY_BY} is {given:?} with no {NOTIFY_SOCKET}: only a service manager gives that deadline"
            )),
        };
    };
    let notify = PathBuf::from(notify);
    if !notify.is_absolute() {
        return Err(format!(
            "{NOTIFY_SOCKET} is {notify:?}, not an absolute path"
        ));
    }
    let ready_by = ready_by
        .map(|given| {
            clock_ns(&given).ok_or_else(|| {
                format!(
                    "{SHARDS_READY_BY} is {given:?}, not a CLOCK_BOOTTIME instant in nanoseconds"
                )
            })
        })
        .transpose()?;
    Ok(Host::Manager { notify, ready_by })
}

/// One duty of init's boot, in the order init does them under its host (docs/design/node-under-shards-init.md §2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Duty {
    /// Move off the kernel's initramfs onto a tmpfs root and run again from there ([`leave_rootfs`]).
    LeaveInitramfs,
    /// Mount each file system, each on a directory made for it.
    Mount(Vec<Mount>),
    /// Make each directory the node's paths need.
    Dirs(&'static [&'static str]),
    /// Mount the shares the kernel's command line names ([`mount_shares`]).
    KernelShares,
    /// Take the boot's deadline where its host tells it.
    Deadline(Deadline),
    /// Enable the controllers the node's boxes are bounded by, at the root of the hierarchy init sees.
    Controllers,
    /// Find `/init` itself, which the node and the relay are run as, through `/proc/self/exe`.
    FindExe,
    /// Write the GPU catalog the kernel's command line grants ([`hex_runtime::guest::gpu::initialize`]).
    GpuAuthority,
    /// Give the node its descriptor limit ([`give_descriptors`]).
    Descriptors,
    /// Start the node, then the relay listening where it says, each within the boot's deadline.
    Start(On),
    /// Tell Firecracker the guest is ready, where the kernel's command line asks ([`notify_firecracker_ready`]).
    TellFirecracker,
    /// Tell the service manager the guest is ready: READY=1 on the socket it named (sd_notify(3)).
    Notify(PathBuf),
    /// Say the guest is ready, reap every process that ends until the node or the relay does, then end as it says.
    Serve(End),
}

#[cfg(test)]
impl Duty {
    /// Whether it reads the kernel's `/proc`, mounted before it: by init's own Mount duty as the VM's init, by its
    /// service manager under one. Leaving the initramfs reads `/proc/self/mountinfo` through a `/proc` it mounts and
    /// unmounts itself.
    fn reads_proc(&self) -> bool {
        matches!(
            self,
            Duty::KernelShares
                | Duty::Deadline(Deadline::Kernel)
                | Duty::FindExe
                | Duty::GpuAuthority
                | Duty::Descriptors
        )
    }

    /// Whether it mounts `/proc` for the duties after it.
    fn mounts_proc(&self) -> bool {
        matches!(self, Duty::Mount(mounts) if mounts.iter().any(|m| m.fs == "proc"))
    }
}

/// Where init takes the boot's deadline from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Deadline {
    /// The kernel's command line: `hex.ready-ms` or Firecracker's, counted from the kernel's start ([`boot_budget`]).
    Kernel,
    /// What its service manager told it ([`SHARDS_READY_BY`]), none where it told none.
    Told(Option<u128>),
}

/// How init ends once the node or the relay has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// Powers the VM off (reboot(2)), so its host sees it end.
    PowerOff,
    /// Exits: its PID namespace ends with it, every process left in it killed (pid_namespaces(7)), and its service
    /// manager's run with them.
    Exit,
}

/// What init does under `host`, in order (docs/design/node-under-shards-init.md §2). As the VM's init, everything its
/// node needs of a Linux machine. Under a service manager, what shards-init leaves it: shards-init has made the root,
/// mounted `/proc`, `/sys`, the delegated cgroup hierarchy and `/dev`, mounted the run's shares, set `fs.nr_open` and
/// the run's descriptor limit, and holds no GPU authority it could pass on (D234's `gpus=` isn't built).
fn duties(host: &Host) -> Vec<Duty> {
    match host {
        Host::Kernel => vec![
            Duty::LeaveInitramfs,
            Duty::Mount(mounts().to_vec()),
            Duty::Dirs(&[RUN, STATE, BOX_ROOT]),
            Duty::KernelShares,
            Duty::Deadline(Deadline::Kernel),
            Duty::Controllers,
            // `/proc/self/exe`: as the VM's init, only once the Mount duty above has mounted `/proc`.
            Duty::FindExe,
            Duty::GpuAuthority,
            Duty::Descriptors,
            Duty::Start(On::Vsock),
            Duty::TellFirecracker,
            Duty::Serve(End::PowerOff),
        ],
        Host::Manager { notify, ready_by } => vec![
            Duty::Mount(node_tmpfs().to_vec()),
            Duty::Dirs(&[CHANNEL, BOX_ROOT]),
            Duty::Deadline(Deadline::Told(*ready_by)),
            Duty::Controllers,
            Duty::FindExe,
            Duty::Start(On::Unix(PathBuf::from(CHANNEL))),
            Duty::Notify(notify.clone()),
            Duty::Serve(End::Exit),
        ],
    }
}

/// The tmpfs init mounts for its node under a service manager: on the node's state and on the relay's staged roots,
/// the two directories its boxes' overlays take their layers and upper directories from. The run's root is itself an
/// overlay, which overlayfs refuses as an upper directory (`ovl_mount_dir_check`, fs/overlayfs/params.c: "not supported
/// as upperdir"). Each is root's alone to write, 0755, as each is when made under the VM's own init's tmpfs `/run`
/// (tmpfs(5) would make its root 01777). shards-init's `/run/shards` and the shares beside them stay as it mounted them.
fn node_tmpfs() -> [Mount; 2] {
    let tmpfs = |target| Mount {
        source: "tmpfs",
        target,
        fs: "tmpfs",
        flags: rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NODEV,
        data: Some(c"mode=0755"),
        expedite: false,
    };
    [tmpfs(STATE), tmpfs(STAGED)]
}

/// How the kernel names the initramfs's first process: `rdinit`'s default, passed as its `argv[0]`
/// (init/main.c, `ramdisk_execute_command` and `run_init_process`).
const KERNEL_INIT: &str = "/init";

/// Whether `hex` runs as the guest's init: PID 1, named `/init`, whatever other words it is given. That is the
/// kernel's first process, or under shards-init the run's own command, which Hex's node image names `/init`
/// (docs/design/node-under-shards-init.md §4). PID 1 alone isn't enough: a container's first program is PID 1 of
/// its own PID namespace, and `/hex selftest-box` or the runtime's `__oci-init` there must run as themselves, so
/// its `argv[0]` tells them apart. A container that names its own program `/init` and is `hex` fails init's first
/// privileged step or stays inside its namespaces: it reaches nothing a container can't.
pub fn runs_as_init(pid: u32, argv0: Option<&std::ffi::OsStr>) -> bool {
    pid == 1 && argv0 == Some(std::ffi::OsStr::new(KERNEL_INIT))
}

/// Runs as the guest's init (§4); returns only if it fails, with the exit status.
pub fn init_main() -> u8 {
    let why = match init() {
        Ok(()) => "powering off returned".to_owned(),
        Err((_, why)) => why,
    };
    let _ = std::io::stderr()
        .lock()
        .write_all(format!("hex guest: {why}\n").as_bytes());
    1
}

fn init() -> Result<(), (u8, String)> {
    let fail = |why: String| (1, why);
    let t0 = std::env::var(BOOT_T0)
        .ok()
        .and_then(|t| t.parse::<u128>().ok())
        .unwrap_or_else(boottime_ns);
    let host = host(|name| std::env::var_os(name)).map_err(fail)?;
    let mut boot = Boot {
        exe: None,
        t0,
        mounted: t0,
        node_ready: t0,
        ready_by: None,
        firecracker: None,
        started: None,
    };
    for duty in duties(&host) {
        boot.perform(duty).map_err(fail)?;
    }
    Ok(())
}

/// What init's duties hand on to later ones as it boots.
struct Boot {
    /// `/init`, which runs the node and the relay, once its FindExe duty has found it.
    exe: Option<PathBuf>,
    /// The boot split's stamps on the guest's clock: init's first entry, its mounts done, its node ready.
    t0: u128,
    mounted: u128,
    node_ready: u128,
    /// The boot's deadline on the guest's clock, once its host has told it: the instant by which its host waits for
    /// it to be ready, within which init waits for each child's ready line and the relay's calls to its node take it
    /// as their caller's (AC-T7.4.1). None where its host told none.
    ready_by: Option<u128>,
    /// Firecracker's readiness budget, where the kernel's command line asked for its ready word.
    firecracker: Option<Duration>,
    /// The node's and the relay's process IDs, once both are started.
    started: Option<(u32, u32)>,
}

impl Boot {
    fn perform(&mut self, duty: Duty) -> Result<(), String> {
        match duty {
            Duty::LeaveInitramfs => leave_rootfs(self.t0),
            Duty::Mount(mounts) => mount_each(&mounts),
            Duty::Dirs(dirs) => dirs.iter().try_for_each(|dir| {
                std::fs::create_dir_all(dir).map_err(|e| format!("making {dir}: {e}"))
            }),
            Duty::KernelShares => mount_shares(),
            Duty::Deadline(Deadline::Kernel) => {
                let cmdline = kernel_cmdline()?;
                self.firecracker = firecracker_ready_budget(&cmdline)?;
                // AC-T7.4.1: its host's readiness bound from the kernel's start, an instant on the guest's clock.
                self.ready_by =
                    boot_budget(&cmdline, self.firecracker)?.map(|budget| budget.as_nanos());
                Ok(())
            }
            Duty::Deadline(Deadline::Told(ready_by)) => {
                self.ready_by = ready_by;
                Ok(())
            }
            Duty::Controllers => {
                std::fs::write("/sys/fs/cgroup/cgroup.subtree_control", CONTROLLERS)
                    .map_err(|e| format!("enabling the cgroup controllers {CONTROLLERS}: {e}"))?;
                self.mounted = boottime_ns();
                Ok(())
            }
            Duty::FindExe => {
                let exe = std::env::current_exe().map_err(|e| format!("finding /init: {e}"))?;
                self.exe = Some(exe);
                Ok(())
            }
            // AC-T7.4.1: only immutable host boot authority can materialize the selected renderer's
            // guest catalog; do this before the node reads its configured attachment routes.
            Duty::GpuAuthority => hex_runtime::guest::gpu::initialize(Path::new(STATE))
                .map(|_| ())
                .map_err(|why| format!("initializing guest GPU authority: {why}")),
            Duty::Descriptors => give_descriptors(Path::new(STATE))
                .map(|_| ())
                .map_err(|why| format!("giving the node its descriptor limit: {why}")),
            Duty::Start(on) => self.start_node_and_relay(&on),
            Duty::TellFirecracker => match self.firecracker {
                Some(budget) => notify_firecracker_ready(budget)
                    .map_err(|e| format!("notifying Firecracker that the guest is ready: {e}")),
                None => Ok(()),
            },
            Duty::Notify(socket) => {
                let by = self
                    .ready_by
                    .map(|at| {
                        deadline_at(at, Instant::now(), boottime_ns()).ok_or_else(|| {
                            format!("the boot's deadline, {at} ns on the guest's clock, is past a monotonic instant")
                        })
                    })
                    .transpose()?;
                notify_ready(&socket, by)
                    .map_err(|e| format!("telling {} the guest is ready: {e}", socket.display()))
            }
            Duty::Serve(end) => self.serve(end),
        }
    }

    /// Starts the node, then the relay listening `on` it, each within the boot's deadline.
    fn start_node_and_relay(&mut self, on: &On) -> Result<(), String> {
        let exe = self
            .exe
            .clone()
            .ok_or_else(|| "starting the node before init found itself".to_owned())?;
        let socket = Path::new(RUN).join("node.sock");
        let node = start(
            &exe,
            &[
                "node".as_ref(),
                "--socket".as_ref(),
                socket.as_os_str(),
                "--state".as_ref(),
                STATE.as_ref(),
                "--box-root".as_ref(),
                BOX_ROOT.as_ref(),
            ],
            &format!("ready {}\n", socket.display()),
            None,
            self.ready_by,
        )
        .map_err(|why| why.to_string())?;
        self.node_ready = boottime_ns();
        let mut args: Vec<&OsStr> = vec![
            "guest".as_ref(),
            "relay".as_ref(),
            "--node".as_ref(),
            socket.as_os_str(),
        ];
        match on {
            On::Vsock => args.push("--vsock".as_ref()),
            On::Unix(dir) => args.extend(["--unix".as_ref(), dir.as_os_str()]),
        }
        let relay = start(
            &exe,
            &args,
            "ready\n",
            Some(format!("{} {} {}", self.t0, self.mounted, self.node_ready)),
            self.ready_by,
        )
        .map_err(|why| why.to_string())?;
        self.started = Some((node, relay));
        Ok(())
    }

    /// Says the guest is ready, reaps until the node or the relay ends, and ends as `end` says.
    fn serve(&self, end: End) -> Result<(), String> {
        let (node, relay) = self
            .started
            .ok_or_else(|| "serving before the node and the relay started".to_owned())?;
        let mut out = std::io::stdout().lock();
        let _ = out
            .write_all(b"hex guest ready\n")
            .and_then(|()| out.flush());
        drop(out);
        let why = reap(node, relay);
        match end {
            End::PowerOff => {
                let _ = std::io::stderr()
                    .lock()
                    .write_all(format!("hex guest: {why}; powering off\n").as_bytes());
                rustix::system::reboot(rustix::system::RebootCommand::PowerOff)
                    .map_err(|e| format!("powering off: {e}"))
            }
            End::Exit => Err(format!("{why}; ending its run")),
        }
    }
}

/// What init gives the node of RLIMIT_NOFILE, as the guest's service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeDescriptors {
    /// The soft and hard limit init sets before it starts the node, which the node inherits.
    limit: u64,
    /// `fs.nr_open` raised to this first, where it was lower.
    nr_open: Option<u64>,
}

/// The kernel's ceiling on any process's descriptor limit: setrlimit(2) refuses a hard RLIMIT_NOFILE past it
/// (EPERM), and root may raise it (proc_sys_fs(5), `nr_open`).
const NR_OPEN: &str = "/proc/sys/fs/nr_open";

/// `fs.nr_open` now: a decimal count and its newline, read within a `u64`'s digits and that newline.
fn read_nr_open() -> Result<u64, Refusal> {
    let mut text = String::new();
    let bound = u64::try_from(u64::MAX.to_string().len().saturating_add(1)).unwrap_or(u64::MAX);
    std::fs::File::open(NR_OPEN)
        .and_then(|file| file.take(bound).read_to_string(&mut text))
        .map_err(|e| Refusal::os("reading fs.nr_open", e))?;
    text.trim_end().parse().map_err(|_| Refusal::Invalid {
        what: "fs.nr_open",
        why: format!("{text:?} isn't a count"),
    })
}

/// The node's descriptor limit, from its `need` (`hex_node::netns::needed_on_this_host`'s total, the one count, which
/// `hex node --descriptors` says), what init was `given` and the kernel's `nr_open` now: soft and hard alike the need,
/// or a hard limit init was given past it, kept; `fs.nr_open` raised to it first where lower, as no hard limit passes
/// it.
fn node_descriptors(need: u64, given: &rustix::process::Rlimit, nr_open: u64) -> NodeDescriptors {
    let limit = given.maximum.filter(|hard| *hard > need).unwrap_or(need);
    NodeDescriptors {
        limit,
        nr_open: if limit > nr_open { Some(limit) } else { None },
    }
}

/// Gives the node, its state under `state`, its descriptor limit before init starts it, as a service manager gives one
/// (`LimitNOFILE=`): init's own, which the node inherits, as [`node_descriptors`] says (AC-T7.4.1). Refused by type,
/// with the numbers, where `fs.nr_open` or the limit can't be set, so init says why and starts no node rather than one
/// that refuses silently.
fn give_descriptors(state: &Path) -> Result<NodeDescriptors, Refusal> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    // The guest's node runs containers alone, no VM boxes of its own.
    let need = hex_node::netns::needed_on_this_host(state, hex_node::Runs::default())?.total();
    let nr_open = read_nr_open()?;
    let given = node_descriptors(need, &getrlimit(Resource::Nofile), nr_open);
    let refused = |why: String| Refusal::Platform {
        what: "the guest node's descriptor limit",
        platform: "linux",
        why,
    };
    if let Some(raised) = given.nr_open {
        std::fs::write(NR_OPEN, raised.to_string()).map_err(|e| {
            refused(format!(
                "the node needs {need} descriptors, past fs.nr_open's {nr_open}, which init couldn't raise to {raised}: {e}"
            ))
        })?;
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(given.limit),
            maximum: Some(given.limit),
        },
    )
    .map_err(|e| {
        refused(format!(
            "the node needs {need} descriptors; init couldn't set RLIMIT_NOFILE's soft and hard limits to {}: {e}",
            given.limit
        ))
    })?;
    Ok(given)
}

/// One file system `init` mounts: its source, target, type and flags.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    source: &'static str,
    target: &'static str,
    fs: &'static str,
    flags: rustix::mount::MountFlags,
    /// Its file system's options; none for its defaults.
    data: Option<&'static std::ffi::CStr>,
    /// Made with RCU's grace periods expedited ([`RCU_EXPEDITED`]).
    expedite: bool,
}

/// What `init` mounts as the VM's init, in order.
fn mounts() -> [Mount; 6] {
    use rustix::mount::MountFlags;
    let quiet = MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NODEV;
    let mount = |source, target, fs, flags| Mount {
        source,
        target,
        fs,
        flags,
        data: None,
        expedite: false,
    };
    [
        mount("proc", "/proc", "proc", quiet),
        mount("sysfs", "/sys", "sysfs", quiet),
        // cgroup2's first mount waits out RCU grace periods, which the kernel expedites only until it starts
        // init (`rcu_end_inkernel_boot`): about 20 ms of an HVF boot, under 0.1 ms expedited
        // (bench/native/guest/boot-split/RECORD.md).
        Mount {
            expedite: true,
            ..mount("cgroup2", "/sys/fs/cgroup", "cgroup2", quiet)
        },
        mount("devtmpfs", "/dev", "devtmpfs", MountFlags::NOSUID),
        mount(
            "tmpfs",
            "/run",
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
        ),
        mount(
            "tmpfs",
            "/tmp",
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV,
        ),
    ]
}

/// Where `init` mounts the tmpfs that becomes the root.
const NEW_ROOT: &str = "/hex-root";

/// Moves `init` off the kernel's initramfs, whose `rootfs` pivot_root(2) can't leave, onto a tmpfs
/// mounted over `/` (switch_root's way: `MS_MOVE` and `chroot`), and runs it again from there. Returns
/// only when the root is already not the initramfs.
fn leave_rootfs(t0: u128) -> Result<(), String> {
    use rustix::mount::{MountFlags, UnmountFlags, mount, mount_move, unmount};
    let quiet = MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NODEV;
    std::fs::create_dir_all("/proc").map_err(|e| format!("making /proc: {e}"))?;
    mount("proc", "/proc", "proc", quiet, None).map_err(|e| format!("mounting /proc: {e}"))?;
    let root =
        std::fs::File::open("/proc/self/mountinfo").and_then(|f| root_fs(io::BufReader::new(f)));
    unmount("/proc", UnmountFlags::empty()).map_err(|e| format!("unmounting /proc: {e}"))?;
    let root = root.map_err(|e| format!("reading /proc/self/mountinfo: {e}"))?;
    if root.as_deref() != Some("rootfs") {
        return Ok(());
    }
    std::fs::create_dir_all(NEW_ROOT).map_err(|e| format!("making {NEW_ROOT}: {e}"))?;
    // 0755: search on `/` for every user, as resolving any path needs (path_resolution(7)), and
    // writes for root alone, the mode of ramfs's root, the kernel's rootfs without tmpfs
    // (fs/ramfs/inode.c `RAMFS_DEFAULT_MODE`); a tmpfs left to itself is 01777 (tmpfs(5) `mode=`),
    // which would let any user make names in `/`.
    mount(
        "tmpfs",
        NEW_ROOT,
        "tmpfs",
        MountFlags::empty(),
        c"mode=0755",
    )
    .map_err(|e| format!("mounting the root's tmpfs: {e}"))?;
    let init = Path::new(NEW_ROOT).join("init");
    std::fs::copy("/init", &init).map_err(|e| format!("copying /init: {e}"))?;
    // The initramfs is unreachable once the tmpfs covers it, so its copy goes now; this process keeps
    // its pages until the exec below.
    std::fs::remove_file("/init").map_err(|e| format!("removing the initramfs's /init: {e}"))?;
    std::env::set_current_dir(NEW_ROOT).map_err(|e| format!("entering {NEW_ROOT}: {e}"))?;
    mount_move(".", "/").map_err(|e| format!("moving {NEW_ROOT} to /: {e}"))?;
    rustix::process::chroot(".").map_err(|e| format!("entering the new root: {e}"))?;
    std::env::set_current_dir("/").map_err(|e| format!("entering /: {e}"))?;
    let e = std::os::unix::process::CommandExt::exec(
        std::process::Command::new("/init").env(BOOT_T0, t0.to_string()),
    );
    Err(format!("running /init from the new root: {e}"))
}

/// The file system mounted at `/` in `mountinfo` (proc(5)): the last mount on `/`, as each covers the
/// one before; `None` where no line names `/`. It is read to its end a line at a time, holding one line
/// and the answer: its length is the kernel's, a line per mount in the namespace, which the kernel bounds
/// (`fs.mount-max`, Documentation/admin-guide/sysctl/fs.rst), and here, before init mounts anything but
/// this `/proc`, it lists the initramfs and that.
fn root_fs(mut mountinfo: impl io::BufRead) -> io::Result<Option<String>> {
    let (mut line, mut root) = (String::new(), None);
    loop {
        line.clear();
        if mountinfo.read_line(&mut line)? == 0 {
            return Ok(root);
        }
        let fs = line.split_once(" - ").and_then(|(mount, fs)| {
            (mount.split(' ').nth(4)? == "/").then(|| fs.split([' ', '\n']).next())?
        });
        if let Some(fs) = fs {
            root = Some(fs.to_owned());
        }
    }
}

/// One of Shards' shares (docs/design/shards-guest.md §4a): whether the guest mounts it read-only.
#[derive(Debug, PartialEq, Eq)]
struct Share {
    read_only: bool,
}

// The most shares a guest mounts: the host's bound, one virtio-fs device each among Shards' slots
// (`hex_runtime::guest::shares::MAX_SHARES`); a longer list is refused by name.
use hex_runtime::guest::shares::MAX_SHARES;

/// The parameter on the kernel's command line that names the guest's shares: `hex.shares=` and a mode
/// per share, `rw` or `ro`, joined by commas, in the order of their tags (`shards0`, `shards1`, …).
/// Absent, the guest has none.
const SHARES_PARAMETER: &str = "hex.shares=";

/// Where share `i` is mounted: `/run/hex/shares/<i>`.
const SHARES: &str = "/run/hex/shares";

/// The most read of `/proc/cmdline`: the kernel's bound on its command line, `COMMAND_LINE_SIZE` with
/// its terminating zero, which `/proc/cmdline` prints as a newline (fs/proc/cmdline.c): 2048 on x86_64
/// (arch/x86/include/asm/setup.h:7) and arm64 (arch/arm64/include/uapi/asm/setup.h), the generic 512
/// elsewhere (include/uapi/asm-generic/setup.h:5).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const CMDLINE_BOUND: u64 = 2048;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const CMDLINE_BOUND: u64 = 512;

/// The host supplies its finite boot budget; neither a user environment variable nor
/// init arguments after `--` can enable a host notification.
fn firecracker_ready_budget(cmdline: &str) -> Result<Option<Duration>, String> {
    let (mut enabled, mut budget) = (false, None);
    for word in cmdline.split_whitespace().take_while(|word| *word != "--") {
        if word == "hex.firecracker-ready" {
            if enabled {
                return Err("hex.firecracker-ready is given twice".to_owned());
            }
            enabled = true;
        } else if let Some(value) = word.strip_prefix("hex.firecracker-ready-ms=") {
            let millis = value
                .parse::<u32>()
                .ok()
                .filter(|ms| *ms != 0)
                .filter(|_| value.bytes().all(|byte| byte.is_ascii_digit()))
                .ok_or_else(|| {
                    "hex.firecracker-ready-ms needs positive u32 milliseconds".to_owned()
                })?;
            if budget
                .replace(Duration::from_millis(u64::from(millis)))
                .is_some()
            {
                return Err("hex.firecracker-ready-ms is given twice".to_owned());
            }
        } else if word.starts_with("hex.firecracker-ready") {
            return Err("invalid Firecracker readiness parameter".to_owned());
        }
    }
    match (enabled, budget) {
        (false, None) => Ok(None),
        (true, Some(budget)) => Ok(Some(budget)),
        _ => Err("Firecracker readiness requires its flag and finite budget together".to_owned()),
    }
}

/// The boot's readiness bound its host told the guest on its kernel command line, counted from the kernel's start
/// (AC-T7.4.1): Firecracker's, `firecracker` ([`firecracker_ready_budget`]), or a Shards host's,
/// [`hex_runtime::guest::READY_MS`]; none where the host told none, whose guest keeps its learned bounds. Refused where
/// both are told, or the Shards word is malformed or told twice.
fn boot_budget(cmdline: &str, firecracker: Option<Duration>) -> Result<Option<Duration>, String> {
    use hex_runtime::guest::READY_MS;
    let mut told = None;
    for word in cmdline.split_whitespace().take_while(|word| *word != "--") {
        let Some(rest) = word.strip_prefix(READY_MS) else {
            continue;
        };
        let millis = rest
            .strip_prefix('=')
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|ms| *ms != 0)
            .ok_or_else(|| format!("{READY_MS} needs positive milliseconds, not {word:?}"))?;
        if told.replace(Duration::from_millis(millis)).is_some() {
            return Err(format!("{READY_MS} is given twice"));
        }
    }
    match (firecracker, told) {
        (Some(_), Some(_)) => Err(format!(
            "hex.firecracker-ready-ms and {READY_MS} both name the boot's deadline"
        )),
        (Some(budget), None) => Ok(Some(budget)),
        (None, told) => Ok(told),
    }
}

fn kernel_cmdline() -> Result<String, String> {
    let bound = usize::try_from(CMDLINE_BOUND)
        .map_err(|_| "kernel command line bound exceeds usize".to_owned())?;
    let mut cmdline = String::new();
    cmdline
        .try_reserve_exact(bound)
        .map_err(|_| "allocating the bounded kernel command line".to_owned())?;
    std::fs::File::open("/proc/cmdline")
        .and_then(|file| file.take(CMDLINE_BOUND).read_to_string(&mut cmdline))
        .map_err(|e| format!("reading /proc/cmdline: {e}"))?;
    if !cmdline.ends_with('\n') {
        return Err("kernel command line is truncated".to_owned());
    }
    Ok(cmdline)
}

/// Tells Firecracker's host the guest is ready, within what is left of the boot's `budget`, its readiness bound from
/// the kernel's start (AC-T7.4.1): timed out at once, with no connection tried, where the boot has spent it.
fn notify_firecracker_ready(budget: Duration) -> io::Result<()> {
    let expired = || io::Error::from(io::ErrorKind::TimedOut);
    let deadline = deadline_at(budget.as_nanos(), Instant::now(), boottime_ns())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(expired)?;
    let socket = Socket::new(Domain::VSOCK, Type::STREAM.cloexec(), None)?;
    // VMADDR_CID_HOST is 2 (vsock(7)); the host bound this VM's private UDS before boot.
    socket.connect_timeout(&SockAddr::vsock(2, stream::GUEST_READY_PORT), left)?;
    let mut pending = stream::GUEST_READY;
    while !pending.is_empty() {
        let left = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(expired)?;
        socket.set_write_timeout(Some(left))?;
        // Closing the host's readiness listener must return BrokenPipe, not kill PID1 by SIGPIPE.
        match rustix::net::send(&socket, pending, rustix::net::SendFlags::NOSIGNAL) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(sent) => {
                pending = pending
                    .get(sent..)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if std::time::Instant::now() >= deadline {
        return Err(expired());
    }
    Ok(())
}

/// sd_notify(3)'s word that "service startup is finished".
const READY: &[u8] = b"READY=1";

/// Tells the service manager listening on `socket` that the guest is ready (docs/design/node-under-shards-init.md §2):
/// [`READY`] in one datagram, as sd_notify(3) sends it ("a single datagram with the state string as payload"), from a
/// socket of init's own, unbound. Within what is left of the boot's deadline where init has one, timed out at once
/// where the boot has spent it, as Firecracker's notice is ([`notify_firecracker_ready`]). With none it waits on the
/// manager, the VM's init, whose end is the VM's. A datagram is sent whole or not at all (unix(7)), so a short send
/// is refused.
fn notify_ready(socket: &Path, by: Option<Instant>) -> io::Result<()> {
    let sender = std::os::unix::net::UnixDatagram::unbound()?;
    if let Some(by) = by {
        let left = by
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        sender.set_write_timeout(Some(left))?;
    }
    match sender.send_to(READY, socket)? {
        sent if sent == READY.len() => Ok(()),
        _ => Err(io::ErrorKind::WriteZero.into()),
    }
}

/// The shares the kernel's command line names, in tag order. Only the kernel's own parameters count:
/// what follows `--` is init's arguments.
fn shares(cmdline: &str) -> Result<Vec<Share>, String> {
    let mut named = cmdline
        .split_whitespace()
        .take_while(|word| *word != "--")
        .filter_map(|word| word.strip_prefix(SHARES_PARAMETER));
    let Some(modes) = named.next() else {
        return Ok(Vec::new());
    };
    if named.next().is_some() {
        return Err(format!("{SHARES_PARAMETER} is given twice"));
    }
    let mut out = Vec::new();
    for mode in modes.split(',') {
        if out.len() == MAX_SHARES {
            return Err(format!(
                "{SHARES_PARAMETER} names more than {MAX_SHARES} shares"
            ));
        }
        out.push(Share {
            read_only: match mode {
                "rw" => false,
                "ro" => true,
                other => {
                    return Err(format!(
                        "{SHARES_PARAMETER} names a share {other:?}, not rw or ro"
                    ));
                }
            },
        });
    }
    Ok(out)
}

/// Mounts each share the kernel's command line names, as `virtiofs` by its tag, at `/run/hex/shares/<i>`.
fn mount_shares() -> Result<(), String> {
    use rustix::mount::{MountFlags, mount};
    let mut cmdline = String::new();
    std::fs::File::open("/proc/cmdline")
        .and_then(|f| f.take(CMDLINE_BOUND).read_to_string(&mut cmdline))
        .map_err(|e| format!("reading /proc/cmdline: {e}"))?;
    for (i, share) in shares(&cmdline)?.iter().enumerate() {
        let (tag, target) = (format!("shards{i}"), Path::new(SHARES).join(i.to_string()));
        let mut flags = MountFlags::NOSUID | MountFlags::NODEV;
        if share.read_only {
            flags |= MountFlags::RDONLY;
        }
        std::fs::create_dir_all(&target)
            .map_err(|e| format!("making {}: {e}", target.display()))?;
        mount(&tag, &target, "virtiofs", flags, None)
            .map_err(|e| format!("mounting share {tag} on {}: {e}", target.display()))?;
    }
    Ok(())
}

/// The kernel's switch for expediting RCU's grace periods (kernel/ksysfs.c, `rcu_expedited_store`):
/// on, a grace period ends at once at the cost of an interrupt to every CPU; the kernel does so itself during
/// its boot, until it starts init.
const RCU_EXPEDITED: &str = "/sys/kernel/rcu_expedited";

/// Turns RCU's expedited grace periods on or off.
fn expedite_rcu(on: bool) -> Result<(), String> {
    std::fs::write(RCU_EXPEDITED, if on { "1" } else { "0" })
        .map_err(|e| format!("writing {on} to {RCU_EXPEDITED}: {e}"))
}

/// Mounts each of `mounts` in order, each on a directory made for it.
fn mount_each(mounts: &[Mount]) -> Result<(), String> {
    for m in mounts {
        let (target, fs) = (m.target, m.fs);
        std::fs::create_dir_all(target).map_err(|e| format!("making {target}: {e}"))?;
        if m.expedite {
            expedite_rcu(true)?;
        }
        let mounted = rustix::mount::mount(m.source, target, fs, m.flags, m.data)
            .map_err(|e| format!("mounting {fs} on {target}: {e}"));
        if m.expedite {
            expedite_rcu(false)?;
        }
        mounted?;
    }
    Ok(())
}

/// Starts `/init` with `args` and waits for its first line, `ready`: read as far as its length and no
/// further, as nothing else is that line, and, where init has the boot's deadline `ready_by`, within what is left of
/// it, refused past it with the child ended (AC-T7.4.1).
/// `stamps`, where given, goes to the child as init's boot stamps ([`BOOT_STAMPS`]), and `ready_by` as the boot's
/// deadline ([`BOOT_READY_BY`]), which no child takes from init's own environment.
fn start(
    exe: &Path,
    args: &[&std::ffi::OsStr],
    ready: &str,
    stamps: Option<String>,
    ready_by: Option<u128>,
) -> Result<u32, Refusal> {
    let what = args
        .iter()
        .take(2)
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let by = ready_by
        .map(|at| {
            deadline_at(at, Instant::now(), boottime_ns()).ok_or_else(|| Refusal::Invalid {
                what: "the boot's deadline",
                why: format!("{at} ns on the guest's clock is past a monotonic instant"),
            })
        })
        .transpose()?;
    let mut command = std::process::Command::new(exe);
    if let Some(stamps) = stamps {
        command.env(BOOT_STAMPS, stamps);
    }
    command.env_remove(BOOT_READY_BY);
    if let Some(at) = ready_by {
        command.env(BOOT_READY_BY, at.to_string());
    }
    let mut child = command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Refusal::Os {
            op: "starting a guest process",
            why: format!("hex {what}: {e}"),
        })?;
    let stdout = child.stdout.take().ok_or_else(|| Refusal::Os {
        op: "starting a guest process",
        why: format!("hex {what}'s stdout wasn't kept"),
    })?;
    let line = match ready_line(stdout, ready.len(), by, &what) {
        Ok(line) => line,
        Err(why) => {
            // Not ready within the boot's deadline, or not read: it ends, as the boot does.
            let _ = child.kill();
            let _ = child.wait();
            return Err(why);
        }
    };
    if line != ready.as_bytes() {
        return Err(Refusal::Invalid {
            what: "a guest process's ready line",
            why: format!(
                "hex {what} said {:?}, not {ready:?}",
                String::from_utf8_lossy(&line)
            ),
        });
    }
    Ok(child.id())
}

/// The first `want` bytes of `out`, a child's stdout, its ready line, or fewer where it ends first: within `by` where
/// init has the boot's deadline, refused past it, else as long as it takes.
fn ready_line(
    mut out: std::process::ChildStdout,
    want: usize,
    by: Option<Instant>,
    what: &str,
) -> Result<Vec<u8>, Refusal> {
    let failed = |e: &dyn std::fmt::Display| Refusal::Os {
        op: "reading a guest process's ready line",
        why: format!("hex {what}: {e}"),
    };
    let exhausted = || Refusal::Exhausted {
        what: "a guest process's ready line",
        bound: want,
    };
    let mut line = Vec::new();
    line.try_reserve_exact(want).map_err(|_| exhausted())?;
    let Some(by) = by else {
        let bound = u64::try_from(want).map_err(|e| failed(&e))?;
        out.take(bound)
            .read_to_end(&mut line)
            .map_err(|e| failed(&e))?;
        return Ok(line);
    };
    let flags = rustix::fs::fcntl_getfl(&out).map_err(|e| failed(&e))?;
    rustix::fs::fcntl_setfl(&out, flags | rustix::fs::OFlags::NONBLOCK).map_err(|e| failed(&e))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(want).map_err(|_| exhausted())?;
    buf.resize(want, 0);
    while line.len() < want {
        let left = by.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Refusal::Os {
                op: "waiting for a guest process to say it is ready",
                why: format!(
                    "hex {what} said {:?} by the boot's deadline",
                    String::from_utf8_lossy(&line)
                ),
            });
        }
        let timeout = rustix::event::Timespec {
            tv_sec: i64::try_from(left.as_secs()).map_err(|e| failed(&e))?,
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let mut ready = [rustix::event::PollFd::new(
            &out,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut ready, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(failed(&e)),
        }
        let room = want
            .checked_sub(line.len())
            .and_then(|room| buf.get_mut(..room))
            .ok_or_else(exhausted)?;
        match out.read(room) {
            Ok(0) => break,
            Ok(n) => line.extend_from_slice(room.get(..n).unwrap_or_default()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(failed(&e)),
        }
    }
    Ok(line)
}

/// Reaps every process that ends until the node or the relay does, and says which and how.
fn reap(node: u32, relay: u32) -> String {
    use rustix::process::{WaitOptions, wait};
    loop {
        match wait(WaitOptions::empty()) {
            Ok(Some((pid, status))) => {
                let pid = pid.as_raw_nonzero().get().unsigned_abs();
                let name = if pid == node {
                    "the node agent"
                } else if pid == relay {
                    "the relay"
                } else {
                    continue;
                };
                return format!("{name} ended ({status:?})");
            }
            Ok(None) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return format!("waiting for the guest's processes: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::mount::MountFlags;

    /// AC-T7.4.1 (original): the guest's kernel starts init under RLIMIT_NOFILE 1024 soft and 4096 hard (its
    /// INR_OPEN_CUR and INR_OPEN_MAX), under which the node refused to start: "its own 271062 descriptors at its pools'
    /// bounds exceed RLIMIT_NOFILE: the hard limit of 4096" (native_firecracker_smoke at c1f89439, the VMM's tail). Do:
    /// count the node's need as `hex node --descriptors` does, then what init gives the node from the guest kernel's
    /// limit, under this host's `fs.nr_open` and under one below the need. Expect soft and hard both the need, and
    /// `fs.nr_open` raised to it first only where it was lower.
    #[test]
    fn original_the_limit_init_gives_the_node_holds_its_need() {
        let need = hex_node::netns::needed_on_this_host(Path::new("/"), hex_node::Runs::default())
            .unwrap()
            .total();
        let guest = rustix::process::Rlimit {
            current: Some(1024),
            maximum: Some(4096),
        };
        let nr_open = read_nr_open().unwrap();
        assert_eq!(
            node_descriptors(need, &guest, nr_open),
            NodeDescriptors {
                limit: need,
                nr_open: if need > nr_open { Some(need) } else { None },
            }
        );
        assert_eq!(
            node_descriptors(need, &guest, need - 1),
            NodeDescriptors {
                limit: need,
                nr_open: Some(need),
            }
        );
    }

    /// AC-T7.4.1: only the host's paired kernel parameters authorize a bounded readiness
    /// notification; init arguments and environment defaults never enable it.
    #[test]
    fn firecracker_readiness_requires_paired_finite_kernel_parameters() {
        for text in [
            "",
            "quiet",
            "quiet -- hex.firecracker-ready hex.firecracker-ready-ms=1",
        ] {
            assert_eq!(firecracker_ready_budget(text).unwrap(), None);
        }
        for millis in [1u32, u32::MAX] {
            let text = format!(
                "quiet hex.firecracker-ready-ms={millis} hex.firecracker-ready -- hex.firecracker-ready-ms=bad"
            );
            assert_eq!(
                firecracker_ready_budget(&text).unwrap(),
                Some(Duration::from_millis(u64::from(millis)))
            );
        }
        for text in [
            "hex.firecracker-ready",
            "hex.firecracker-ready-ms=1",
            "hex.firecracker-ready hex.firecracker-ready-ms=0",
            "hex.firecracker-ready hex.firecracker-ready-ms=-1",
            "hex.firecracker-ready hex.firecracker-ready-ms=+1",
            "hex.firecracker-ready hex.firecracker-ready-ms=4294967296",
            "hex.firecracker-ready hex.firecracker-ready-ms=",
            "hex.firecracker-ready hex.firecracker-ready-ms=1 hex.firecracker-ready",
            "hex.firecracker-ready hex.firecracker-ready-ms=1 hex.firecracker-ready-ms=1",
            "hex.firecracker-ready=1 hex.firecracker-ready-ms=1",
        ] {
            assert!(firecracker_ready_budget(text).is_err(), "{text}");
        }
    }

    /// AC-T7.4.1: the boot's deadline init hands the relay is its host's readiness bound on the guest's clock, and the
    /// relay's calls to its node take what is left of it. Do: read the variable as the relay does with the clock
    /// before, at and past it, with none, and with hostile values. Expect: what is left from now, nothing once passed
    /// (a caller's deadline its call refuses at once), none where init handed none, and a refusal for the rest.
    #[test]
    fn the_relay_takes_what_is_left_of_the_boots_deadline() {
        use std::os::unix::ffi::OsStrExt;
        let now = Instant::now();
        let ready_by = |text: &str, boot_ns| boot_deadline(Some(OsStr::new(text)), now, boot_ns);
        assert_eq!(boot_deadline(None, now, 5), Ok(None));
        assert_eq!(
            ready_by("60000000000", 1_500_000_000),
            Ok(Some(now + Duration::from_nanos(58_500_000_000)))
        );
        assert_eq!(ready_by("60000000000", 60_000_000_000), Ok(Some(now)));
        assert_eq!(ready_by("60000000000", 61_000_000_000), Ok(Some(now)));
        let past = (u128::from(u64::MAX) + 1).to_string();
        for bad in ["", "x", "-1", "+1", "1.5", " 1", "1 ", past.as_str()] {
            assert!(ready_by(bad, 0).is_err(), "{bad:?}");
        }
        assert!(boot_deadline(Some(OsStr::from_bytes(b"\xff1")), now, 0).is_err());
    }

    /// AC-T7.4.1: the boot's deadline is its host's readiness bound, Firecracker's (`hex.firecracker-ready-ms`) or a
    /// Shards host's (`hex.ready-ms`), and a guest told none keeps its learned bounds. Do: read command lines that name
    /// either, neither or both, malformed or repeated Shards words, and one after `--`. Expect each bound as told, none
    /// for neither or init's own arguments, and a refusal for the rest.
    #[test]
    fn the_boots_deadline_is_its_hosts_readiness_bound_either_way() {
        let firecracker = Some(Duration::from_secs(60));
        assert_eq!(boot_budget("console=ttyS0 panic=-1", None), Ok(None));
        assert_eq!(boot_budget("console=ttyS0", firecracker), Ok(firecracker));
        assert_eq!(
            boot_budget(
                "console=ttyS0 panic=-1 hex.shares=rw hex.ready-ms=21056",
                None
            ),
            Ok(Some(Duration::from_millis(21_056)))
        );
        assert_eq!(boot_budget("quiet -- hex.ready-ms=5", None), Ok(None));
        for bad in [
            "hex.ready-ms",
            "hex.ready-ms=",
            "hex.ready-ms=0",
            "hex.ready-ms=-1",
            "hex.ready-ms=+1",
            "hex.ready-ms=1.5",
            "hex.ready-msx=1",
            "hex.ready-ms=18446744073709551616",
            "hex.ready-ms=1 hex.ready-ms=2",
        ] {
            assert!(boot_budget(bad, None).is_err(), "{bad}");
        }
        assert!(boot_budget("hex.ready-ms=5", firecracker).is_err());
    }

    /// AC-T7.4.1 (original): init waited for each child's ready line with no bound, so a node or relay that never
    /// said it was ready held the boot past its deadline. Do: start a child that says nothing and never ends on its
    /// own (`sleep infinity`), handed the boot's deadline as init hands it. Expect: refused at the deadline, naming
    /// it. Only the deadline can end the wait: a child that ended on its own after a chosen time (`sleep 1`, ten times
    /// the deadline) was read to its end first once on a loaded host, and the wait refused for its silence instead.
    #[test]
    fn original_init_waits_for_a_childs_ready_line_within_the_boots_deadline() {
        let within = Duration::from_millis(100);
        let ready_by = boottime_ns() + within.as_nanos();
        let began = Instant::now();
        let started = start(
            Path::new("sleep"),
            &[OsStr::new("infinity")],
            "ready\n",
            None,
            Some(ready_by),
        )
        .map_err(|why| why.to_string());
        eprintln!(
            "guest_init_ready_line within={within:?} took={:?} started={started:?}",
            began.elapsed()
        );
        assert!(
            matches!(&started, Err(why) if why.contains("the boot's deadline")),
            "{started:?}"
        );
    }

    /// AC-T7.4.1 (original): init's notice to Firecracker that the guest is ready took a whole fresh budget from its
    /// own start, not what was left of the boot's. Do: notify within a boot budget this host's clock has long spent
    /// (its boot is older than the budget). Expect: refused at once as timed out, before the budget it was handed.
    #[test]
    fn original_the_ready_notice_gets_what_is_left_of_the_boots_deadline() {
        let budget = Duration::from_secs(1);
        assert!(
            boottime_ns() > budget.as_nanos(),
            "this host booted within the budget"
        );
        let began = Instant::now();
        let notified = notify_firecracker_ready(budget);
        let took = began.elapsed();
        eprintln!("guest_ready_notice budget={budget:?} took={took:?} notified={notified:?}");
        assert!(
            matches!(&notified, Err(e) if e.kind() == io::ErrorKind::TimedOut),
            "{notified:?}"
        );
        assert!(took < budget, "{took:?}: the notice spent a fresh budget");
    }

    /// HEX-GUEST-9 (docs/design/shards-guest.md §4, amended 2026-11-06): `init` mounts `/proc`, `/sys`,
    /// cgroup2, `devtmpfs` and a tmpfs `/run`, and the node's state lives on that tmpfs, never a disk;
    /// `/run` may run programs, since the staged bundles under it are containers' roots.
    #[test]
    fn init_mounts_section_4s_file_systems_and_keeps_state_on_tmpfs() {
        let all = mounts();
        for (target, fs) in [
            ("/proc", "proc"),
            ("/sys", "sysfs"),
            ("/sys/fs/cgroup", "cgroup2"),
            ("/dev", "devtmpfs"),
            ("/run", "tmpfs"),
        ] {
            assert!(
                all.iter().any(|m| m.target == target && m.fs == fs),
                "{fs} on {target} is missing"
            );
        }
        let run = all.iter().find(|m| m.target == "/run").unwrap();
        assert!(!run.flags.contains(MountFlags::NOEXEC));
        for m in &all {
            assert!(
                ["proc", "sysfs", "cgroup2", "devtmpfs", "tmpfs"].contains(&m.fs),
                "{} on {} is a disk's file system",
                m.fs,
                m.target
            );
        }
        assert!(Path::new(STATE).starts_with(RUN), "the state is at {STATE}");
    }

    /// HEX-GUEST-38 (the boot split): read init's mounts: expect cgroup2's alone made with RCU's grace
    /// periods expedited (its first mount waits out grace periods, 20 ms of an HVF boot), after sysfs, whose
    /// /sys/kernel/rcu_expedited switches it, and every other mount made as the kernel leaves RCU.
    #[test]
    fn only_cgroup2_is_mounted_with_rcu_expedited_after_sysfs() {
        let all = mounts();
        let expedited: Vec<&str> = all.iter().filter(|m| m.expedite).map(|m| m.fs).collect();
        assert_eq!(expedited, ["cgroup2"]);
        let at = |fs| all.iter().position(|m| m.fs == fs).unwrap();
        assert!(at("sysfs") < at("cgroup2"));
    }

    /// HEX-GUEST-10 (§4): `init` reads the root's file system from mountinfo, so it leaves the
    /// initramfs (`rootfs`) once and never again; hostile or short lines name no root.
    #[test]
    fn the_root_file_system_is_read_from_mountinfo() {
        let root_fs = |text: &str| root_fs(text.as_bytes()).unwrap();
        let initramfs = "1 1 0:2 / / rw - rootfs rootfs rw,size=1000k\n";
        assert_eq!(root_fs(initramfs).as_deref(), Some("rootfs"));
        let moved = "1 1 0:2 / / rw - rootfs rootfs rw\n\
                     20 1 0:20 / / rw,relatime - tmpfs tmpfs rw,mode=755\n\
                     21 20 0:21 / /proc rw - proc proc rw\n";
        assert_eq!(root_fs(moved).as_deref(), Some("tmpfs"));
        for hostile in [
            "",
            "\n\n",
            "1 1 0:2 / /\n",
            "1 1 0:2 / /run rw - tmpfs tmpfs rw\n",
            " - - - - - \n",
            "1 1 0:2 / / rw -rootfs\n",
        ] {
            assert_eq!(root_fs(hostile), None, "{hostile:?}");
        }
        assert_eq!(root_fs("1 1 0:2 / / rw - \n").as_deref(), Some(""));
    }

    /// HEX-GUEST-31 (§4): mountinfo is read whole, a line at a time, as long as the kernel makes it: a
    /// root mount listed past 64 KiB of other mounts is still the one read.
    #[test]
    fn mountinfo_is_read_to_its_end() {
        let mut info = String::from("1 1 0:2 / / rw - tmpfs tmpfs rw\n");
        for i in 0..2_000 {
            info.push_str(&format!("{i} 1 0:3 / /mnt/{i:040} rw - tmpfs tmpfs rw\n"));
        }
        info.push_str("9 1 0:2 / / rw - rootfs rootfs rw\n");
        assert!(info.len() > 64 * 1024);
        assert_eq!(root_fs(info.as_bytes()).unwrap().as_deref(), Some("rootfs"));
    }

    /// HEX-GUEST-11 (§4a): `hex.shares=` on the kernel's command line names Shards' shares in tag order,
    /// `rw` or `ro` each, which init mounts at /run/hex/shares/<i>; none named is none; a bad mode, an
    /// empty one, the parameter twice or past its bound is refused by name; init's own arguments after
    /// `--` are not the kernel's.
    #[test]
    fn shares_are_read_from_the_kernel_command_line() {
        let (rw, ro) = (Share { read_only: false }, Share { read_only: true });
        assert_eq!(shares("console=ttyS0 panic=-1 -- guest init"), Ok(vec![]));
        assert_eq!(
            shares("console=ttyS0 hex.shares=rw,ro,rw -- guest init\n"),
            Ok(vec![rw, ro, Share { read_only: false }])
        );
        assert_eq!(shares("hex.shares=ro"), Ok(vec![Share { read_only: true }]));
        assert_eq!(shares("-- hex.shares=rw"), Ok(vec![]));
        let past = vec!["rw"; MAX_SHARES + 1].join(",");
        let at = vec!["rw"; MAX_SHARES].join(",");
        assert_eq!(
            shares(&format!("hex.shares={at}")).map(|s| s.len()),
            Ok(MAX_SHARES)
        );
        for bad in [
            "hex.shares=".to_owned(),
            "hex.shares=rw,,ro".to_owned(),
            "hex.shares=rx".to_owned(),
            "hex.shares=RW".to_owned(),
            "hex.shares=rw hex.shares=ro".to_owned(),
            format!("hex.shares={past}"),
        ] {
            let why = shares(&bad).unwrap_err();
            assert!(why.contains("hex.shares"), "{bad:?}: {why}");
        }
    }

    /// HEX-GUEST-12 (§4): as PID 1, `hex` is the guest's init, whatever words the kernel passes it
    /// (Shards appends `quiet` to a run with no console, which once reached init as an argument and
    /// ended the guest: a boot on adas-mac-studio-2, 2026-11-06, `Attempted to kill init!
    /// exitcode=0x00000200`); as any other process it is not, nor as PID 1 of a container's namespace.
    #[test]
    fn hex_is_the_guests_init_as_pid_1_alone() {
        let init = Some(std::ffi::OsStr::new("/init"));
        assert!(runs_as_init(1, init));
        for pid in [0, 2, 1000, u32::MAX] {
            assert!(!runs_as_init(pid, init), "{pid}");
        }
        // A container's first program is PID 1 of its namespace, and runs as itself.
        for argv0 in ["/hex", "/proc/self/exe", "hex", "/usr/bin/hex"] {
            assert!(
                !runs_as_init(1, Some(std::ffi::OsStr::new(argv0))),
                "{argv0}"
            );
        }
        assert!(!runs_as_init(1, None));
    }

    /// The environment shards-init gives its run's own command under Shards' nesting grant (D234), as `var` reads it.
    fn shards_inits(ready_by: Option<&str>) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| match name {
            NOTIFY_SOCKET => Some("/run/shards/notify".into()),
            SHARDS_READY_BY => ready_by.map(Into::into),
            _ => None,
        }
    }

    /// AC-T7.4.1, joint plan step 39 (original; docs/design/node-under-shards-init.md §2, §3): under shards-init,
    /// Hex's init is its run's own command under Shards' nesting grant (D234), PID 1 of the workload's own PID
    /// namespace, and shards-init has done the VM's part. Do: read init's host from shards-init's environment
    /// (`NOTIFY_SOCKET`, `SHARDS_READY_BY`), then its duties. Expect: no initramfs to leave and no kernel mount,
    /// share, GPU word or descriptor raise, which are shards-init's; a tmpfs for the node's state and its staged
    /// roots, whose overlays the run's own overlay root can't hold; the deadline shards-init told; the controllers;
    /// the node, then the relay on AF_UNIX sockets (no vsock under Docker's filter); READY=1 to shards-init's
    /// socket; and an exit at the end, not a power-off.
    #[test]
    fn original_under_shards_init_init_does_its_own_duties_alone() {
        let host = host(shards_inits(Some("4200000000"))).unwrap();
        assert_eq!(
            host,
            Host::Manager {
                notify: PathBuf::from("/run/shards/notify"),
                ready_by: Some(4_200_000_000),
            }
        );
        let tmpfs = |target| Mount {
            source: "tmpfs",
            target,
            fs: "tmpfs",
            flags: MountFlags::NOSUID | MountFlags::NODEV,
            data: Some(c"mode=0755"),
            expedite: false,
        };
        assert_eq!(
            duties(&host),
            vec![
                Duty::Mount(vec![tmpfs(STATE), tmpfs(STAGED)]),
                Duty::Dirs(&[CHANNEL, BOX_ROOT]),
                Duty::Deadline(Deadline::Told(Some(4_200_000_000))),
                Duty::Controllers,
                Duty::FindExe,
                Duty::Start(On::Unix(PathBuf::from(CHANNEL))),
                Duty::Notify(PathBuf::from("/run/shards/notify")),
                Duty::Serve(End::Exit),
            ]
        );
    }

    /// AC-T7.4.1 (docs/design/node-under-shards-init.md §2; sd_notify(3)): init tells its service manager it is ready
    /// with one datagram, READY=1, on the socket the manager named, within what is left of the boot's deadline. Do:
    /// bind a datagram socket as shards-init binds its own, tell it ready with no deadline and with the latest the
    /// guest's clock names, then with one passed. Expect: READY=1 read whole each time, and past the deadline a
    /// refusal as timed out, with nothing sent.
    #[test]
    fn init_tells_its_manager_it_is_ready_in_one_datagram_within_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("notify");
        let manager = std::os::unix::net::UnixDatagram::bind(&socket).unwrap();
        // A local datagram is on the manager's queue once its send returns (unix(7)), so nothing is waited for.
        manager.set_nonblocking(true).unwrap();
        let latest = deadline_at(u128::from(u64::MAX), Instant::now(), boottime_ns());
        assert!(latest.is_some());
        for by in [None, latest] {
            notify_ready(&socket, by).unwrap();
            let mut got = [0u8; 64];
            let n = manager.recv(&mut got).unwrap();
            assert_eq!(&got[..n], READY, "{by:?}");
        }
        let passed = notify_ready(&socket, Some(Instant::now()));
        assert!(
            matches!(&passed, Err(e) if e.kind() == io::ErrorKind::TimedOut),
            "{passed:?}"
        );
        let mut got = [0u8; 64];
        assert_eq!(
            manager.recv(&mut got).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    /// AC-T7.4.1, joint plan step 39 (docs/design/node-under-shards-init.md §8): Hex's PID-1 path is kept whole. Do:
    /// read init's host from a kernel's environment, which holds no `NOTIFY_SOCKET`, then its duties. Expect: every
    /// duty it has as the VM's init, in the order it has done them: leave the initramfs, mount §4's file systems, make
    /// the node's directories, mount the kernel's shares, take the kernel's deadline, enable the controllers, write
    /// the GPU catalog, give the node its descriptors, start the node and the relay on vsock, tell Firecracker where
    /// it asks, and power off at the end.
    #[test]
    fn as_the_vms_init_init_keeps_every_duty_it_had() {
        let host = host(|_| None).unwrap();
        assert_eq!(host, Host::Kernel);
        assert_eq!(
            duties(&host),
            vec![
                Duty::LeaveInitramfs,
                Duty::Mount(mounts().to_vec()),
                Duty::Dirs(&[RUN, STATE, BOX_ROOT]),
                Duty::KernelShares,
                Duty::Deadline(Deadline::Kernel),
                Duty::Controllers,
                Duty::FindExe,
                Duty::GpuAuthority,
                Duty::Descriptors,
                Duty::Start(On::Vsock),
                Duty::TellFirecracker,
                Duty::Serve(End::PowerOff),
            ]
        );
    }

    /// AC-T7.4.1, joint plan step 39 (original): the VM's own init found itself through `/proc/self/exe` before it had
    /// mounted `/proc`, and died: "hex guest: finding /init: no /proc/self/exe available. Is /proc mounted?", then
    /// "Kernel panic - not syncing: Attempted to kill init!" (guest_cost on KVM, adacachy2, main 7095f460; 219a8844
    /// resolved it ahead of every duty). Do: walk each host's duties in order. Expect: as the VM's init, no duty reads
    /// `/proc` before the duty that mounts it; under shards-init, whose `/proc` is its manager's, no duty mounts it.
    #[test]
    fn original_as_the_vms_init_nothing_reads_proc_before_it_is_mounted() {
        let mut mounted = false;
        for duty in duties(&Host::Kernel) {
            assert!(
                mounted || !duty.reads_proc(),
                "{duty:?} reads /proc before init mounts it"
            );
            mounted |= duty.mounts_proc();
        }
        assert!(mounted, "the VM's init mounts no /proc");
        let manager = Host::Manager {
            notify: PathBuf::from("/run/shards/notify"),
            ready_by: None,
        };
        let duties = duties(&manager);
        assert!(duties.iter().any(Duty::reads_proc));
        assert!(
            !duties.iter().any(Duty::mounts_proc),
            "init mounts a /proc over its manager's"
        );
    }

    /// AC-T7.4.1 (docs/design/node-under-shards-init.md §3): init's host comes from the words only that host sets. Do:
    /// read the host from shards-init's environment with no deadline and with one passed before its run arrived (0),
    /// then from environments whose socket isn't an absolute path, whose deadline isn't nanoseconds on the guest's
    /// clock, and with a deadline and no socket. Expect: a service manager with no deadline, one with the deadline 0,
    /// and a refusal naming the word for each of the rest.
    #[test]
    fn init_reads_its_host_from_the_words_only_its_host_sets() {
        let notify = PathBuf::from("/run/shards/notify");
        assert_eq!(
            host(shards_inits(None)),
            Ok(Host::Manager {
                notify: notify.clone(),
                ready_by: None,
            })
        );
        assert_eq!(
            host(shards_inits(Some("0"))),
            Ok(Host::Manager {
                notify,
                ready_by: Some(0),
            })
        );
        let past = (u128::MAX).to_string() + "0";
        for bad in ["", "x", "-1", "+1", "1.5", " 1", "1 ", past.as_str()] {
            let why = host(shards_inits(Some(bad))).unwrap_err();
            assert!(why.contains(SHARDS_READY_BY), "{bad:?}: {why}");
        }
        for socket in ["", "run/shards/notify", "@shards", "vsock:2:9"] {
            let why = host(|name| (name == NOTIFY_SOCKET).then(|| socket.into())).unwrap_err();
            assert!(why.contains(NOTIFY_SOCKET), "{socket:?}: {why}");
        }
        let why = host(|name| (name == SHARDS_READY_BY).then(|| "1".into())).unwrap_err();
        assert!(why.contains(NOTIFY_SOCKET), "{why}");
    }

    /// HEX-GUEST-37 (the boot split, bench/native/guest/boot-split): the relay's stamps line is init's three
    /// stamps as init handed them, then the relay's own, in `CLOCK_BOOTTIME` nanoseconds; a relay started
    /// without init's (outside a guest) says so in their place.
    #[test]
    fn the_stamps_line_carries_inits_stamps_then_the_relays() {
        assert_eq!(stamps_line(Some("10 20 30"), 40), "10 20 30 40\n");
        assert_eq!(stamps_line(None, 40), "- - - 40\n");
        assert_eq!(stamps_line(Some("10 x 30"), 40), "- - - 40\n");
    }
}
