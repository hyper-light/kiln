//! Inside a Firecracker microVM (docs/ci.md W13-R05). The host's runner boots it with kiln's own
//! disk as its root file system and `kiln guest` as its init, which:
//!
//! - **runs a leg** (`kiln guest init`): mounts the leg's image (an ext4 made from its OCI image,
//!   `/dev/vdb`) read-only under an overlay whose changes go to the leg's scratch disk
//!   (`/dev/vdc`), moves into it with kiln's disk at `/.kiln`, and forks the leg's runner. That
//!   runner dials the host over vsock, takes the leg and runs it as the `host` executor runs one,
//!   its events and logs going back the same way. Its one way out is the host's egress proxy,
//!   through a forwarder on its loopback (and, when the server is on the host's loopback, the
//!   server's own port there).
//! - **builds an image** (`kiln guest build`): extracts an OCI image's layers (one file each on
//!   `/dev/vdb`) onto an empty ext4 (`/dev/vdc`) as root, so owners, modes, device files and
//!   whiteouts come out as the image has them, and says how it went.
//! - **runs a deployment** (`kiln guest serve`, W15-R02): enters its image as a leg's, then a
//!   manager runs the image's service and keeps it running, checks its health, and takes the
//!   host's connections in to its ports (its way out is the host's egress proxy, as a leg's), until
//!   the host says stop or hangs up.
//!
//! As PID 1 it reaps what's orphaned and, once the runner (or the build, or the deployment) is
//! done, reboots, which ends the microVM. What goes wrong is said on its console first.

use std::collections::HashSet;
use std::io::{self, BufRead as _, Read, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use nix::mount::{MntFlags, MsFlags};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::proto::{AgentMsg, JobResult, LogChunk};

/// The vsock port the guest dials the host on: the leg (or the build), its events and its result.
pub const CONTROL_PORT: u32 = 10_790;
/// The port each of the guest's connections out dials: the host's egress proxy.
pub const PROXY_PORT: u32 = 10_791;
/// The port the server's own, on the host's loopback, is forwarded through.
pub const SERVER_PORT: u32 = 10_792;
/// Where the forwarder to the egress proxy listens in the guest.
pub const LOCAL_PROXY: u16 = 3128;
/// Where the leg's runner works: on the overlay, so on the leg's scratch disk.
pub const WORK: &str = "/work";
/// A deployment's guest takes the host's connections to its service's port `p` on vsock port
/// `INBOUND + p`.
pub const INBOUND: u32 = 0x1_0000;
/// The host, as vsock addresses it.
const HOST_CID: u32 = 2;
/// The largest frame either side sends (an assignment with its actions, a log chunk).
const MAX_FRAME: usize = 64 << 20;

/// What the host sends.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToGuest {
    Leg(Box<LegStart>),
    /// The layers on `/dev/vdb`, bottom first.
    Build(Vec<Layer>),
    /// The leg is cancelled: its steps stop, and what runs after a cancel runs.
    Cancel,
    /// What the host's egress proxy refused (W18-R08).
    Blocked(String),
    /// A terminal into the leg (W17-R01), or one command run there (W17-R05).
    TermOpen {
        session: String,
        cols: u16,
        rows: u16,
        command: Option<String>,
    },
    TermInput {
        session: String,
        data: Vec<u8>,
    },
    TermResize {
        session: String,
        cols: u16,
        rows: u16,
    },
    /// What brings a fresh terminal to the session's state.
    TermAttach {
        session: String,
    },
    TermClose {
        session: String,
    },
    /// The kept leg let go (W17-R02).
    Release,
    /// The kept leg kept this many minutes from now.
    Extend(u32),
    /// A step of the kept leg run again in place, as step `idx` of its log (W17-R04).
    RerunStep {
        step: u32,
        idx: u32,
    },
    /// A deployment's service (W15-R02).
    Serve(Box<Service>),
    /// The deployment is over: its service stopped, then the microVM.
    Stop,
}

/// A leg for the guest's runner.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LegStart {
    /// The `AssignmentV2`, as JSON.
    pub assignment: String,
    pub runner_name: String,
    pub cache_max: Option<u64>,
    /// The image's environment (its config's `Env`: `PATH` and the rest).
    pub env: Vec<(String, String)>,
    /// kiln's certificate authority (DER), which the server's certificate may come from.
    pub ca: Option<Vec<u8>>,
    /// The server's port on the host's loopback, forwarded to the guest's.
    pub server_port: Option<u16>,
    /// The host's time as it sent the leg (Unix nanoseconds): a microVM restored from a snapshot
    /// (W13-R09) takes it, its own clock being the snapshot's.
    pub now_ns: u128,
    /// Randomness of the host's, mixed into the guest kernel's and reseeding it: microVMs restored
    /// from one snapshot would otherwise share its state.
    pub entropy: Vec<u8>,
}

/// A deployment's service (W15-R02): what a `microvm` environment's microVM runs and keeps
/// running, and how it's known to be healthy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Service {
    /// The image's entrypoint and command (the step's `command` in place of the image's).
    pub argv: Vec<String>,
    /// The image's environment, then the step's.
    pub env: Vec<(String, String)>,
    /// Where it runs (the image's `WorkingDir`); `/` when that's empty.
    pub cwd: String,
    /// Whom it runs as (the image's `User`: `name`, `uid`, `uid:gid`, `name:group`); root when
    /// that's empty.
    pub user: String,
    pub health: Option<Health>,
    /// Its ports the host forwards connections to.
    pub ports: Vec<u16>,
}

/// A health check, as Docker's `HEALTHCHECK` has it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Health {
    /// `["CMD", program, args…]`, `["CMD-SHELL", script]`, or `["HTTP", url]`: a `GET` answered
    /// with a 2xx or 3xx.
    pub test: Vec<String>,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    /// Failures in a row before it's unhealthy.
    pub retries: u32,
    /// How long after it starts its failures don't count.
    pub start_secs: u64,
}

/// One of an image's layers, as a file on the layers' disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    pub file: String,
    pub media_type: String,
}

/// What the guest sends.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromGuest {
    /// It's up and dialled in.
    Ready,
    Msg(AgentMsg),
    Log(LogChunk),
    Term(AgentMsg),
    /// The leg's result: the last frame.
    Done(JobResult),
    /// The build: `None` when it went well, else why not.
    Built(Option<String>),
    /// A deployment's health as it changes (W15-R02): healthy or not, and why.
    Health {
        healthy: bool,
        said: String,
    },
    /// What a deployment's service wrote.
    Output(Vec<u8>),
    /// A deployment's service ended; it starts again `again_secs` on.
    Exited {
        code: Option<i32>,
        again_secs: u64,
    },
    /// The guest is done: nothing follows.
    Bye,
}

/// Sends one frame: its length (big-endian `u32`), then the value in postcard.
pub fn send(w: &mut impl Write, v: &impl Serialize) -> anyhow::Result<()> {
    let bytes = postcard::to_allocvec(v)?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "a frame of {} bytes", bytes.len());
    w.write_all(&u32::try_from(bytes.len())?.to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame; `None` when the other side has closed.
pub fn recv<T: DeserializeOwned>(r: &mut impl Read) -> anyhow::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = usize::try_from(u32::from_be_bytes(len))?;
    anyhow::ensure!(n <= MAX_FRAME, "a frame of {n} bytes");
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(postcard::from_bytes(&buf)?))
}

/// Bytes both ways between `a` and `b` until each side is done.
pub fn splice(a: std::net::TcpStream, b: UnixStream) {
    splice_with(a, b, std::net::Shutdown::Write);
}

/// As [`splice`], `b` shut down `how` once `a` is done sending.
fn splice_with(a: std::net::TcpStream, b: UnixStream, how: std::net::Shutdown) {
    let (Ok(mut a_in), Ok(mut b_out)) = (a.try_clone(), b.try_clone()) else { return };
    let there = std::thread::Builder::new().name("vsock-splice".into()).spawn(move || {
        let _ = io::copy(&mut a_in, &mut b_out);
        let _ = b_out.shutdown(how);
    });
    let (mut a_out, mut b_in) = (a, b);
    let _ = io::copy(&mut b_in, &mut a_out);
    let _ = a_out.shutdown(std::net::Shutdown::Write);
    if let Ok(t) = there {
        let _ = t.join();
    }
}

/// `kiln guest init|build|serve`: the microVM's PID 1. Anywhere else it refuses: it would reboot the
/// machine it ran on.
pub fn main(mode: &str) -> std::process::ExitCode {
    if std::process::id() != 1 {
        eprintln!("kiln guest is a microVM's init (docs/ci.md W13-R05): the firecracker executor boots it");
        return std::process::ExitCode::FAILURE;
    }
    let r = match mode {
        "init" => leg(),
        "build" => build(),
        "serve" => deployment(),
        other => Err(anyhow::anyhow!("`kiln guest {other}`: `init`, `build` or `serve`")),
    };
    if let Err(e) = r {
        eprintln!("kiln guest: {e:#}");
    }
    end()
}

/// Ends the microVM: what's written is written out, then a reboot, which Firecracker takes as the
/// microVM's end.
fn end() -> ! {
    nix::unistd::sync();
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn mount(source: &str, target: &str, fstype: &str, flags: MsFlags, data: Option<&str>) -> anyhow::Result<()> {
    nix::mount::mount(Some(source), target, Some(fstype), flags, data).with_context(|| format!("mounting {source} ({fstype}) on {target}"))
}

/// `/proc`, `/sys` and `/dev` under `root`, and `/dev`'s own: what a system expects to find.
fn system(root: &str) -> anyhow::Result<()> {
    for (source, dir, fstype) in [("proc", "proc", "proc"), ("sysfs", "sys", "sysfs"), ("devtmpfs", "dev", "devtmpfs")] {
        let target = format!("{root}/{dir}");
        let _ = std::fs::create_dir_all(&target);
        match nix::mount::mount(Some(source), target.as_str(), Some(fstype), MsFlags::empty(), None::<&str>) {
            // The kernel mounts devtmpfs itself when it's built to.
            Ok(()) | Err(nix::errno::Errno::EBUSY) => {}
            Err(e) => return Err(anyhow::anyhow!("mounting {fstype} on {target}: {e}")),
        }
    }
    Ok(())
}

/// A leg: its image entered, the host dialled and the leg taken, then its runner forked. A pool's
/// snapshot is taken while it waits for the host (W13-R09), so a microVM restored from it sets its
/// clock and reseeds its randomness from what the host sends before the runner starts, which seeds
/// its own after.
fn leg() -> anyhow::Result<()> {
    enter()?;
    let (start, conn) = take_leg()?;
    fresh(&start);
    #[allow(unsafe_code)]
    // SAFETY: PID 1 has one thread here (no runtime has started and nothing has spawned one), so
    // the child may go on as this program does.
    let forked = unsafe { nix::unistd::fork() }.context("starting the leg's runner")?;
    match forked {
        nix::unistd::ForkResult::Child => {
            let code = runner(start, conn);
            #[allow(clippy::exit)]
            std::process::exit(code)
        }
        nix::unistd::ForkResult::Parent { child } => reap(child),
    }
    Ok(())
}

/// The host dialled (again and again until it listens: a snapshot's template waits here) and the
/// leg taken.
fn take_leg() -> anyhow::Result<(Box<LegStart>, UnixStream)> {
    // What's written so far is on its disk: a snapshot taken from here goes with that disk.
    nix::unistd::sync();
    eprintln!("{WAITING}");
    let mut conn = loop {
        match dial(CONTROL_PORT) {
            Ok(c) => break c,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    send(&mut conn, &FromGuest::Ready)?;
    match recv::<ToGuest>(&mut conn)? {
        Some(ToGuest::Leg(start)) => Ok((start, conn)),
        Some(other) => anyhow::bail!("the host sent {other:?} where a leg goes"),
        None => anyhow::bail!("the host hung up before sending the leg"),
    }
}

/// What a guest says on its console as it waits for the host: up, a template's snapshot to take.
pub const WAITING: &str = "kiln guest: waiting for the host";

/// The host's time, and its randomness mixed into the kernel's, reseeding it.
fn fresh(start: &LegStart) {
    if let Ok(ns) = i64::try_from(start.now_ns) {
        let t = nix::sys::time::TimeSpec::new(ns / 1_000_000_000, ns % 1_000_000_000);
        if let Err(e) = nix::time::clock_settime(nix::time::ClockId::CLOCK_REALTIME, t) {
            eprintln!("kiln guest: setting the clock: {e}");
        }
    }
    if start.entropy.is_empty() {
        return;
    }
    let mixed = std::fs::OpenOptions::new().write(true).open("/dev/urandom").and_then(|mut f| {
        f.write_all(&start.entropy)?;
        // RNDRESEEDCRNG: the kernel's generator reseeds now from what it has, this included.
        #[allow(unsafe_code)]
        // SAFETY: `f` is open, and RNDRESEEDCRNG takes no argument.
        let r = unsafe { nix::libc::ioctl(f.as_raw_fd(), 0x5207 as _) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    });
    if let Err(e) = mixed {
        eprintln!("kiln guest: reseeding its randomness: {e}");
    }
}

/// The image under an overlay on the scratch disk, moved into with kiln's disk at `/.kiln`.
fn enter() -> anyhow::Result<()> {
    system("")?;
    mount("/dev/vdb", "/mnt/image", "ext4", MsFlags::MS_RDONLY, None)?;
    mount("/dev/vdc", "/mnt/scratch", "ext4", MsFlags::empty(), None)?;
    for d in ["/mnt/scratch/upper", "/mnt/scratch/work"] {
        std::fs::create_dir_all(d)?;
    }
    mount("overlay", "/mnt/root", "overlay", MsFlags::empty(), Some("lowerdir=/mnt/image,upperdir=/mnt/scratch/upper,workdir=/mnt/scratch/work"))?;
    for d in [".kiln", "proc", "sys", "dev", "run", "tmp", "root", WORK.trim_start_matches('/')] {
        std::fs::create_dir_all(Path::new("/mnt/root").join(d))?;
    }
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions("/mnt/root/tmp", std::fs::Permissions::from_mode(0o1777))?;
    system("/mnt/root")?;
    for d in ["/mnt/root/dev/pts", "/mnt/root/dev/shm"] {
        std::fs::create_dir_all(d)?;
    }
    mount("devpts", "/mnt/root/dev/pts", "devpts", MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC, Some("gid=5,mode=620,ptmxmode=666"))?;
    mount("tmpfs", "/mnt/root/dev/shm", "tmpfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=1777"))?;
    mount("tmpfs", "/mnt/root/run", "tmpfs", MsFlags::MS_NOSUID | MsFlags::MS_NODEV, Some("mode=755"))?;
    // cgroups, for a container runtime the image has; binfmt_misc, for emulators a leg registers
    // (`setup-qemu-action`): a microVM's kernel is its own to change.
    let _ = nix::mount::mount(Some("cgroup2"), "/mnt/root/sys/fs/cgroup", Some("cgroup2"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC, None::<&str>);
    let _ = nix::mount::mount(Some("binfmt_misc"), "/mnt/root/proc/sys/fs/binfmt_misc", Some("binfmt_misc"), MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC, None::<&str>);
    // Into the image, kiln's disk kept at `/.kiln` (this binary runs from it).
    std::env::set_current_dir("/mnt/root")?;
    nix::unistd::pivot_root(".", ".kiln").context("moving into the image")?;
    nix::unistd::chroot(".")?;
    std::env::set_current_dir("/")?;
    for m in ["/.kiln/dev", "/.kiln/proc", "/.kiln/sys"] {
        let _ = nix::mount::umount2(m, MntFlags::MNT_DETACH);
    }
    let _ = nix::unistd::sethostname("kiln-microvm");
    if let Err(e) = loopback_up() {
        eprintln!("kiln guest: the loopback interface: {e:#}");
    }
    Ok(())
}

/// PID 1's work while the runner (or a deployment's manager) runs: reaping it and whatever is
/// orphaned, until it's gone.
fn reap(runner: nix::unistd::Pid) {
    loop {
        match nix::sys::wait::waitpid(None::<nix::unistd::Pid>, None) {
            Ok(s) if s.pid() == Some(runner) => return,
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(_) => return,
        }
    }
}

/// The loopback interface, up: the forwarders listen on it, and a leg's own services too.
#[allow(unsafe_code)]
fn loopback_up() -> anyhow::Result<()> {
    use nix::libc;
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    // SAFETY: an all-zero `ifreq` is a valid one (a name of NULs, no flags).
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, s) in ifr.ifr_name.iter_mut().zip(b"lo") {
        *d = *s as libc::c_char;
    }
    // SAFETY: `ifr` names `lo` and outlives the call, which writes its flags into it.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS, &mut ifr) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: SIOCGIFFLAGS filled the union's flags, which are read and written as flags here.
    unsafe {
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
    }
    // SAFETY: as above; the call reads `ifr`.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS, &ifr) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// A connection to the host over vsock, to `port`.
fn dial(port: u32) -> anyhow::Result<UnixStream> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, VsockAddr, connect, socket};
    let fd = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::SOCK_CLOEXEC, None).context("a vsock socket")?;
    connect(fd.as_raw_fd(), &VsockAddr::new(HOST_CID, port)).with_context(|| format!("dialling the host's vsock port {port}"))?;
    Ok(UnixStream::from(fd))
}

/// Connections to `127.0.0.1:listen` in the guest, each to the host's vsock `port`.
fn forward(listen: u16, port: u32) -> anyhow::Result<()> {
    let l = std::net::TcpListener::bind(("127.0.0.1", listen)).with_context(|| format!("listening on 127.0.0.1:{listen}"))?;
    std::thread::Builder::new().name(format!("forward-{listen}")).spawn(move || {
        for c in l.incoming().flatten() {
            let _ = std::thread::Builder::new().name(format!("forward-{listen}-conn")).spawn(move || match dial(port) {
                Ok(v) => splice(c, v),
                Err(e) => eprintln!("kiln guest: {e:#}"),
            });
        }
    })?;
    Ok(())
}

static BLOCKED: OnceLock<async_channel::Receiver<String>> = OnceLock::new();

/// In a microVM's leg: what the host's egress proxy refused, as the host says it. `None` anywhere
/// else, where the runner's own proxy says it (W18-R08).
pub fn blocked() -> Option<async_channel::Receiver<String>> {
    BLOCKED.get().cloned()
}

/// The forked runner: dialled in, given its leg, then the leg run on a runtime of its own. Its
/// exit code is 0 when it ran the leg to its result, whatever that was.
fn runner(start: Box<LegStart>, conn: UnixStream) -> i32 {
    match prepare(&start) {
        Ok(()) => {
            crate::rt::configure(crate::rt::Profile::agent());
            match crate::rt::block_on(run(*start, conn)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("kiln guest: {e:#}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("kiln guest: {e:#}");
            1
        }
    }
}

/// Before the runtime starts (with one thread, so the environment may be set): the leg's
/// environment set, and the forwarders started.
fn prepare(start: &LegStart) -> anyhow::Result<()> {
    let proxy = format!("http://127.0.0.1:{LOCAL_PROXY}");
    let mut env: Vec<(String, String)> = vec![("HOME".into(), "/root".into()), ("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into())];
    env.extend(start.env.iter().cloned());
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        env.push((k.into(), proxy.clone()));
    }
    for k in ["NO_PROXY", "no_proxy"] {
        env.push((k.into(), "localhost,127.0.0.1,::1".into()));
    }
    // kiln's own requests go through the proxy to private addresses too: the guest has no other way.
    env.push(("KILN_PROXY_PRIVATE".into(), "1".into()));
    // Docker actions share the guest's network, so the proxy on its loopback is theirs too.
    env.push(("KILN_CONTAINER_NETWORK".into(), "host".into()));
    for (k, v) in &env {
        if !k.is_empty() && !k.contains(['=', '\0']) && !v.contains('\0') {
            #[allow(unsafe_code)]
            // SAFETY: the forked runner has one thread until the runtime starts, after this.
            unsafe {
                std::env::set_var(k, v);
            }
        }
    }
    forward(LOCAL_PROXY, PROXY_PORT)?;
    if let Some(port) = start.server_port {
        forward(port, SERVER_PORT)?;
    }
    if let Err(e) = docker_daemon() {
        eprintln!("kiln guest: the image's Docker: {e:#}");
    }
    Ok(())
}

/// When the image has Docker (`dockerd`), its daemon, for the leg's Docker actions and services
/// (W13-R05): its data on the scratch disk (an overlay can't hold its layers), its pulls through
/// the egress proxy like the rest, and no networking of its own beyond its containers' bridges.
fn docker_daemon() -> anyhow::Result<()> {
    let Some(dockerd) = crate::os::which("dockerd") else { return Ok(()) };
    let data = Path::new("/.kiln/mnt/scratch/docker");
    std::fs::create_dir_all(data)?;
    std::fs::create_dir_all("/var/lib/docker")?;
    std::fs::create_dir_all("/var/log")?;
    nix::mount::mount(Some(data), "/var/lib/docker", None::<&str>, MsFlags::MS_BIND, None::<&str>).context("its data on the scratch disk")?;
    let log = std::fs::File::create("/var/log/kiln-dockerd.log")?;
    std::process::Command::new(dockerd)
        // `host.docker.internal` is the guest itself: its containers share its network.
        .args(["--iptables=false", "--ip6tables=false", "--bridge=none", "--host-gateway-ip=127.0.0.1", "--host=unix:///var/run/docker.sock", "--data-root=/var/lib/docker"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("starting dockerd")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while UnixStream::connect("/var/run/docker.sock").is_err() {
        if std::time::Instant::now() > deadline {
            let said = std::fs::read_to_string("/var/log/kiln-dockerd.log").unwrap_or_default();
            let from = said.len().saturating_sub(2000);
            anyhow::bail!("dockerd didn't answer within a minute:\n{}", said.get(from..).unwrap_or_default());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// What the guest's runner hears: the host, or one of its terminals.
enum Ev {
    Host(ToGuest),
    Pty(String, crate::terminal::PtyOut),
}

/// The leg, run: frames to the host on a thread of their own, the host's on another. Its result
/// goes once every event before it has; a kept leg keeps the microVM, its terminals and its reruns,
/// until it's let go.
async fn run(start: LegStart, conn: UnixStream) -> anyhow::Result<()> {
    let a: kiln_ci::wire::AssignmentV2 = serde_json::from_str(&start.assignment).context("the leg's assignment")?;
    let job = a.job_id.clone();
    let (out_tx, out_rx) = async_channel::unbounded::<FromGuest>();
    let (mut reader, mut writer) = (conn.try_clone()?, conn);
    let wrote = std::thread::Builder::new().name("guest-out".into()).spawn(move || {
        while let Ok(f) = out_rx.recv_blocking() {
            let last = matches!(f, FromGuest::Bye);
            if send(&mut writer, &f).is_err() || last {
                break;
            }
        }
    })?;
    let (ev_tx, ev_rx) = async_channel::unbounded::<Ev>();
    let from_host = ev_tx.clone();
    std::thread::Builder::new().name("guest-in".into()).spawn(move || {
        while let Ok(Some(m)) = recv::<ToGuest>(&mut reader) {
            if from_host.send_blocking(Ev::Host(m)).is_err() {
                break;
            }
        }
    })?;
    let (blocked_tx, blocked_rx) = async_channel::unbounded();
    let _ = BLOCKED.set(blocked_rx);
    let cancel = crate::rt::Shutdown::new();
    crate::rt::spawn(serve(ev_rx, ev_tx, out_tx.clone(), cancel.clone(), blocked_tx, job.clone()));
    let http = std::sync::Arc::new(match start.ca {
        Some(ca) => crate::http::client::Client::new(crate::http::client::Roots::WebPkiPlusDer(ca), None)?,
        None => crate::http::client::Client::public()?,
    });
    let (up_tx, up_rx) = async_channel::unbounded::<crate::exec::Up>();
    let (result_tx, result_rx) = async_channel::bounded::<JobResult>(1);
    let relay = crate::rt::spawn(relay_up(up_rx, result_rx, out_tx.clone()));
    let leg = super::runtime::Leg {
        a,
        work_dir: PathBuf::from(WORK),
        runner_name: start.runner_name,
        up: crate::exec::Uplink(up_tx),
        cancel,
        http,
        warm: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        cache_max: start.cache_max,
    };
    let result = leg.run().await;
    let _ = result_tx.send(result).await;
    // Kept (W17-R02): here until its keep runs out or it's let go.
    while super::kept::kept_until(&job).is_some() {
        crate::rt::sleep(Duration::from_millis(500)).await;
    }
    let _ = crate::rt::timeout(Duration::from_secs(5), relay).await;
    let _ = out_tx.send(FromGuest::Bye).await;
    let _ = crate::rt::blocking(move || wrote.join()).await;
    Ok(())
}

/// The leg's events as they come, its result once every event before it has gone, then what its
/// kept leg says (its reruns) until the leg lets go of it.
async fn relay_up(up_rx: async_channel::Receiver<crate::exec::Up>, result_rx: async_channel::Receiver<JobResult>, out: async_channel::Sender<FromGuest>) {
    use futures::future::Either;
    loop {
        let next = {
            let up = std::pin::pin!(up_rx.recv());
            let result = std::pin::pin!(result_rx.recv());
            match futures::future::select(up, result).await {
                Either::Left((up, _)) => Either::Left(up),
                Either::Right((result, _)) => Either::Right(result),
            }
        };
        match next {
            Either::Left(Ok(up)) => to_host(&out, up).await,
            Either::Left(Err(_)) => {
                // Nothing more from the leg: its result, when it comes, is the last.
                if let Ok(result) = result_rx.recv().await {
                    let _ = out.send(FromGuest::Done(result)).await;
                }
                return;
            }
            Either::Right(Ok(result)) => {
                while let Ok(up) = up_rx.try_recv() {
                    to_host(&out, up).await;
                }
                let _ = out.send(FromGuest::Done(result)).await;
                break;
            }
            Either::Right(Err(_)) => break,
        }
    }
    while let Ok(up) = up_rx.recv().await {
        to_host(&out, up).await;
    }
}

async fn to_host(out: &async_channel::Sender<FromGuest>, up: crate::exec::Up) {
    use crate::exec::Up;
    let f = match up {
        Up::Msg(m) => FromGuest::Msg(m),
        Up::Log(c) => FromGuest::Log(c),
        Up::Term(m) => FromGuest::Term(m),
        _ => return,
    };
    let _ = out.send(f).await;
}

/// The host's requests for the leg (its cancel, its kept leg's release, extension and reruns,
/// terminals into it) and its terminals' output.
async fn serve(
    ev_rx: async_channel::Receiver<Ev>,
    ev_tx: async_channel::Sender<Ev>,
    out: async_channel::Sender<FromGuest>,
    cancel: crate::rt::Shutdown,
    blocked: async_channel::Sender<String>,
    job: String,
) {
    use crate::terminal::PtyOut;
    let ended = |session: String, why: String| FromGuest::Term(AgentMsg::TermExit { session, code: None, error: Some(why) });
    let mut terms: std::collections::HashMap<String, (crate::terminal::PtySession, std::sync::Arc<crate::termsess::Session>)> = std::collections::HashMap::new();
    while let Ok(ev) = ev_rx.recv().await {
        match ev {
            Ev::Host(ToGuest::Cancel) => cancel.cancel(),
            Ev::Host(ToGuest::Blocked(b)) => {
                let _ = blocked.send(b).await;
            }
            Ev::Host(ToGuest::TermOpen { session, cols, rows, command }) => {
                if terms.contains_key(&session) {
                    continue;
                }
                let Some(shell) = super::kept::shell(&job) else {
                    let _ = out.send(ended(session, "the leg isn't running or kept in its microVM".into())).await;
                    continue;
                };
                let mut l = shell.launch(command.as_deref(), None);
                // A command runs in bash, as a step would, where the image has it.
                if l.shell.as_deref() == Some("bash") && crate::os::which("bash").is_none() {
                    l.shell = Some("sh".into());
                }
                let env: Vec<(&str, String)> = l.env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                match crate::terminal::spawn_at(l.shell.as_deref(), l.command.as_deref(), l.program.as_deref(), l.cwd.as_deref(), cols, rows, &env) {
                    Ok((pty, rx)) => {
                        let (tx, s, masked) = (ev_tx.clone(), session.clone(), super::kept::masked(rx, &shell.masks));
                        crate::rt::spawn(async move {
                            while let Ok(o) = masked.recv().await {
                                if tx.send(Ev::Pty(s.clone(), o)).await.is_err() {
                                    break;
                                }
                            }
                        });
                        let state = crate::termsess::Session::detached(session.clone(), format!("leg:{job}"), None, cols, rows);
                        terms.insert(session, (pty, state));
                    }
                    Err(e) => {
                        let _ = out.send(ended(session, format!("{e:#}"))).await;
                    }
                }
            }
            Ev::Host(ToGuest::TermInput { session, data }) => {
                if let Some((pty, _)) = terms.get(&session) {
                    pty.write(data);
                }
            }
            Ev::Host(ToGuest::TermResize { session, cols, rows }) => {
                if let Some((pty, state)) = terms.get(&session) {
                    pty.resize(cols, rows);
                    state.send(crate::termsess::Input::Resize { cols, rows });
                }
            }
            Ev::Host(ToGuest::TermAttach { session }) => {
                let msg = match terms.get(&session) {
                    Some((_, state)) => {
                        let a = state.attach();
                        state.detach();
                        FromGuest::Term(AgentMsg::TermReplay { session, data: a.replay })
                    }
                    None => ended(session, "session no longer exists".into()),
                };
                let _ = out.send(msg).await;
            }
            Ev::Host(ToGuest::TermClose { session }) => {
                terms.remove(&session);
            }
            Ev::Host(ToGuest::Release) => {
                let j = job.clone();
                crate::rt::spawn(async move {
                    super::kept::release(&j).await;
                });
            }
            Ev::Host(ToGuest::Extend(minutes)) => {
                let _ = super::kept::extend(&job, minutes);
            }
            Ev::Host(ToGuest::RerunStep { step, idx }) => {
                let _ = super::runtime::rerun_step(&job, step, idx);
            }
            Ev::Host(ToGuest::Leg(_) | ToGuest::Build(_) | ToGuest::Serve(_) | ToGuest::Stop) => {}
            Ev::Pty(session, PtyOut::Data(data)) => {
                if let Some((_, state)) = terms.get(&session) {
                    state.output(&data);
                }
                let _ = out.send(FromGuest::Term(AgentMsg::TermOutput { session, data })).await;
            }
            Ev::Pty(session, PtyOut::Exit(code)) => {
                terms.remove(&session);
                let _ = out.send(FromGuest::Term(AgentMsg::TermExit { session, code, error: None })).await;
            }
        }
    }
}

/// A deployment (W15-R02): its image entered, and its service's manager forked.
fn deployment() -> anyhow::Result<()> {
    enter()?;
    #[allow(unsafe_code)]
    // SAFETY: as for a leg: PID 1 has one thread here, so the child may go on as this program does.
    let forked = unsafe { nix::unistd::fork() }.context("starting the deployment's manager")?;
    match forked {
        nix::unistd::ForkResult::Child => {
            let code = match manage() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("kiln guest: {e:#}");
                    1
                }
            };
            #[allow(clippy::exit)]
            std::process::exit(code)
        }
        nix::unistd::ForkResult::Parent { child } => reap(child),
    }
    Ok(())
}

/// What's known of a deployment's health, for saying when it changes.
const UNKNOWN: u8 = 0;
const HEALTHY: u8 = 1;
const UNHEALTHY: u8 = 2;

/// The deployment's manager: dialled in and given its service, which it runs, and runs again when
/// it ends (waiting longer each time it ends soon after starting, up to half a minute), checks, and
/// takes the host's connections in to; until the host says stop or hangs up, when the service is
/// stopped (`SIGTERM` to its process group, `SIGKILL` ten seconds on) and the microVM ends.
fn manage() -> anyhow::Result<()> {
    let mut conn = dial(CONTROL_PORT)?;
    send(&mut conn, &FromGuest::Ready)?;
    let svc = match recv::<ToGuest>(&mut conn)? {
        Some(ToGuest::Serve(s)) => *s,
        Some(other) => anyhow::bail!("the host sent {other:?} where a service goes"),
        None => anyhow::bail!("the host hung up before sending the service"),
    };
    anyhow::ensure!(!svc.argv.is_empty(), "the service has no command: its image has no entrypoint or command, and the step gave none");
    forward(LOCAL_PROXY, PROXY_PORT)?;
    for &port in &svc.ports {
        inbound(port)?;
    }
    let (out, out_rx) = std::sync::mpsc::channel::<FromGuest>();
    let mut writer = conn.try_clone()?;
    let wrote = std::thread::Builder::new().name("guest-out".into()).spawn(move || {
        while let Ok(f) = out_rx.recv() {
            let last = matches!(f, FromGuest::Bye);
            if send(&mut writer, &f).is_err() || last {
                break;
            }
        }
    })?;
    // The service's process while it runs.
    let running: Arc<parking_lot::Mutex<Option<u32>>> = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    // The host's word: stop, or its hanging up, which is the same.
    {
        let (stop, running, mut reader) = (Arc::clone(&stop), Arc::clone(&running), conn);
        std::thread::Builder::new().name("guest-in".into()).spawn(move || {
            while let Ok(Some(m)) = recv::<ToGuest>(&mut reader) {
                if matches!(m, ToGuest::Stop) {
                    break;
                }
            }
            stop.store(true, Ordering::SeqCst);
            terminate(&running);
        })?;
    }
    let user = user_of(&svc.user)?;
    let env = service_env(&svc, user.as_ref());
    let health = Arc::new(AtomicU8::new(UNKNOWN));
    if let Some(h) = svc.health.clone() {
        let (stop, out, env, health) = (Arc::clone(&stop), out.clone(), env.clone(), Arc::clone(&health));
        let user = user.as_ref().map(|u| (u.uid, u.gid));
        std::thread::Builder::new().name("health".into()).spawn(move || watch(&h, &env, user, &stop, &health, &out))?;
    }
    let mut wait = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        let code = match start(&svc, &env, user.as_ref(), &out) {
            Ok(mut child) => {
                let pid = child.id();
                *running.lock() = Some(pid);
                // A stop that came as it started.
                if stop.load(Ordering::SeqCst) {
                    terminate(&running);
                }
                // With no check of its own, it's healthy once it has run two seconds.
                if svc.health.is_none() {
                    let (running, out, health) = (Arc::clone(&running), out.clone(), Arc::clone(&health));
                    std::thread::Builder::new().name("running".into()).spawn(move || {
                        std::thread::sleep(Duration::from_secs(2));
                        if *running.lock() == Some(pid) && health.swap(HEALTHY, Ordering::SeqCst) != HEALTHY {
                            let _ = out.send(FromGuest::Health { healthy: true, said: "running".into() });
                        }
                    })?;
                }
                let status = child.wait();
                *running.lock() = None;
                status.ok().and_then(|s| s.code())
            }
            Err(e) => {
                let _ = out.send(FromGuest::Output(format!("kiln: {e:#}\n").into_bytes()));
                None
            }
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Said again once it's back, checked or not.
        health.store(UNKNOWN, Ordering::SeqCst);
        if svc.health.is_none() {
            let _ = out.send(FromGuest::Health { healthy: false, said: "it ended".into() });
        }
        if started.elapsed() > Duration::from_secs(60) {
            wait = Duration::from_secs(1);
        }
        let _ = out.send(FromGuest::Exited { code, again_secs: wait.as_secs() });
        sleep_unless(&stop, wait);
        wait = wait.saturating_mul(2).min(Duration::from_secs(30));
    }
    let _ = out.send(FromGuest::Bye);
    let _ = wrote.join();
    Ok(())
}

/// Sleeps `d`, or less once `stop` is set.
fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let until = Instant::now() + d;
    while !stop.load(Ordering::SeqCst) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The service told to stop (`SIGTERM` to its process group), and killed ten seconds on if it
/// hasn't.
fn terminate(running: &Arc<parking_lot::Mutex<Option<u32>>>) {
    use nix::sys::signal::{Signal, killpg};
    let Some(pid) = *running.lock() else { return };
    let Ok(group) = i32::try_from(pid).map(nix::unistd::Pid::from_raw) else { return };
    let _ = killpg(group, Signal::SIGTERM);
    let running = Arc::clone(running);
    let _ = std::thread::Builder::new().name("terminate".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        if *running.lock() == Some(pid) {
            let _ = killpg(group, Signal::SIGKILL);
        }
    });
}

/// Whom a service runs as, from the image's `/etc/passwd` and `/etc/group`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct User {
    uid: u32,
    gid: u32,
    home: Option<String>,
}

/// `user` (the image's `User`: `name`, `uid`, `uid:gid`, `name:group`), looked up in the image;
/// `None` for root's.
fn user_of(user: &str) -> anyhow::Result<Option<User>> {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    resolve_user(user, &passwd, &group)
}

fn resolve_user(user: &str, passwd: &str, group: &str) -> anyhow::Result<Option<User>> {
    let user = user.trim();
    if user.is_empty() || user == "root" || user == "0" {
        return Ok(None);
    }
    let (u, g) = user.split_once(':').map_or((user, None), |(u, g)| (u, Some(g)));
    let rows = |text: &str| text.lines().map(|l| l.split(':').map(str::to_owned).collect::<Vec<String>>()).collect::<Vec<_>>();
    let (users, groups) = (rows(passwd), rows(group));
    let number = |row: &Vec<String>, i: usize| row.get(i).and_then(|v| v.parse::<u32>().ok());
    let found = match u.parse::<u32>() {
        Ok(n) => users.iter().find(|r| number(r, 2) == Some(n)),
        Err(_) => Some(users.iter().find(|r| r.first().map(String::as_str) == Some(u)).with_context(|| format!("the image has no user `{u}`"))?),
    };
    let uid = match u.parse::<u32>() {
        Ok(n) => n,
        Err(_) => found.and_then(|r| number(r, 2)).with_context(|| format!("the image's user `{u}` has no uid"))?,
    };
    let gid = match g {
        None => found.and_then(|r| number(r, 3)).unwrap_or(0),
        Some(g) => match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => groups.iter().find(|r| r.first().map(String::as_str) == Some(g)).and_then(|r| number(r, 2)).with_context(|| format!("the image has no group `{g}`"))?,
        },
    };
    Ok(Some(User { uid, gid, home: found.and_then(|r| r.get(5).cloned()).filter(|h| !h.is_empty()) }))
}

/// The service's environment: a default `PATH` and `HOME`, the image's and the step's, and the way
/// out through the host's egress proxy unless they say otherwise.
fn service_env(svc: &Service, user: Option<&User>) -> Vec<(String, String)> {
    let home = user.map_or_else(|| Some("/root".to_owned()), |u| u.home.clone()).unwrap_or_else(|| "/".to_owned());
    let mut env: Vec<(String, String)> = vec![("PATH".into(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()), ("HOME".into(), home)];
    env.extend(svc.env.iter().cloned());
    let proxy = format!("http://127.0.0.1:{LOCAL_PROXY}");
    let set = |env: &mut Vec<(String, String)>, k: &str, v: &str| {
        if !env.iter().any(|(e, _)| e == k) {
            env.push((k.to_owned(), v.to_owned()));
        }
    };
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        set(&mut env, k, &proxy);
    }
    for k in ["NO_PROXY", "no_proxy"] {
        set(&mut env, k, "localhost,127.0.0.1,::1");
    }
    env
}

/// The service started, in its own process group: what it writes goes to the host as it comes.
fn start(svc: &Service, env: &[(String, String)], user: Option<&User>, out: &std::sync::mpsc::Sender<FromGuest>) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;
    let (program, args) = svc.argv.split_first().context("the service has no command")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(if svc.cwd.is_empty() { "/" } else { svc.cwd.as_str() })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
    if let Some(u) = user {
        cmd.uid(u.uid).gid(u.gid);
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {program}"))?;
    let outs: [Option<Box<dyn Read + Send>>; 2] = [child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>), child.stderr.take().map(|e| Box::new(e) as Box<dyn Read + Send>)];
    for mut r in outs.into_iter().flatten() {
        let out = out.clone();
        std::thread::Builder::new().name("service-output".into()).spawn(move || {
            let mut buf = vec![0u8; 16 << 10];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 || out.send(FromGuest::Output(buf.get(..n).unwrap_or_default().to_vec())).is_err() {
                    break;
                }
            }
        })?;
    }
    Ok(child)
}

/// The service's health check, every interval: healthy once it passes, unhealthy once it has
/// failed `retries` times in a row (failures in its start period don't count), said each time that
/// changes (and again after the service starts again).
fn watch(h: &Health, env: &[(String, String)], user: Option<(u32, u32)>, stop: &AtomicBool, health: &AtomicU8, out: &std::sync::mpsc::Sender<FromGuest>) {
    let started = Instant::now();
    let mut fails = 0u32;
    while !stop.load(Ordering::SeqCst) {
        match check(h, env, user) {
            Ok(()) => {
                fails = 0;
                if health.swap(HEALTHY, Ordering::SeqCst) != HEALTHY {
                    let _ = out.send(FromGuest::Health { healthy: true, said: "healthy".into() });
                }
            }
            Err(why) => {
                if started.elapsed() >= Duration::from_secs(h.start_secs) {
                    fails = fails.saturating_add(1);
                }
                if fails >= h.retries.max(1) && health.swap(UNHEALTHY, Ordering::SeqCst) != UNHEALTHY {
                    let _ = out.send(FromGuest::Health { healthy: false, said: why });
                }
            }
        }
        sleep_unless(stop, Duration::from_secs(h.interval_secs.max(1)));
    }
}

/// One run of a health check: why it failed, when it did.
fn check(h: &Health, env: &[(String, String)], user: Option<(u32, u32)>) -> Result<(), String> {
    let timeout = Duration::from_secs(h.timeout_secs.max(1));
    match h.test.split_first() {
        Some((kind, rest)) if kind == "HTTP" => get(rest.first().map_or("", String::as_str), timeout),
        Some((kind, rest)) if kind == "CMD" => run_check(rest, env, user, timeout),
        Some((kind, rest)) if kind == "CMD-SHELL" => run_check(&["/bin/sh".to_owned(), "-c".to_owned(), rest.join(" ")], env, user, timeout),
        _ => Ok(()),
    }
}

/// A check's command, run within `timeout`: it passes when it exits 0.
fn run_check(argv: &[String], env: &[(String, String)], user: Option<(u32, u32)>, timeout: Duration) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;
    let (program, args) = argv.split_first().ok_or("the check has no command")?;
    let said = Path::new("/run/kiln-health.out");
    let file = std::fs::File::create(said).map_err(|e| format!("{}: {e}", said.display()))?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str()))).stdin(std::process::Stdio::null());
    if let Ok(copy) = file.try_clone() {
        cmd.stdout(copy);
    }
    cmd.stderr(file);
    if let Some((uid, gid)) = user {
        cmd.uid(uid).gid(gid);
    }
    let mut child = cmd.spawn().map_err(|e| format!("{program}: {e}"))?;
    let deadline = Instant::now() + timeout;
    let output = || {
        let text = std::fs::read(said).unwrap_or_default();
        let from = text.len().saturating_sub(300);
        String::from_utf8_lossy(text.get(from..).unwrap_or_default()).trim().to_owned()
    };
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => {
                let code = s.code().map_or_else(|| "on a signal".to_owned(), |c| c.to_string());
                let said = output();
                return Err(if said.is_empty() { format!("the check exited {code}") } else { format!("the check exited {code}: {said}") });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the check took longer than {}s", timeout.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// A `GET` of `url` (`http://host[:port]/path`) within `timeout`: it passes on a 2xx or 3xx.
fn get(url: &str, timeout: Duration) -> Result<(), String> {
    use std::net::ToSocketAddrs as _;
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("{url}: a health check's URL is http://"))?;
    let (authority, path) = rest.split_once('/').map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    let addr = if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) { authority.to_owned() } else { format!("{authority}:80") };
    let to = addr.to_socket_addrs().map_err(|e| format!("{addr}: {e}"))?.next().ok_or_else(|| format!("{addr}: no address"))?;
    let mut s = std::net::TcpStream::connect_timeout(&to, timeout).map_err(|e| format!("{url}: {e}"))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: {authority}\r\nUser-Agent: kiln-health\r\nConnection: close\r\n\r\n").map_err(|e| format!("{url}: {e}"))?;
    let mut line = String::new();
    io::BufReader::new(s).read_line(&mut line).map_err(|e| format!("{url}: {e}"))?;
    let code = line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).ok_or_else(|| format!("{url}: not an HTTP answer"))?;
    if (200..400).contains(&code) { Ok(()) } else { Err(format!("{url} answered {code}")) }
}

/// The host's connections in to vsock port `INBOUND + port`, each to the service's `port` on the
/// guest's loopback.
fn inbound(port: u16) -> anyhow::Result<()> {
    use nix::sys::socket::{AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept, bind, listen, socket};
    let vport = INBOUND + u32::from(port);
    let fd = socket(AddressFamily::Vsock, SockType::Stream, SockFlag::SOCK_CLOEXEC, None).context("a vsock socket")?;
    bind(fd.as_raw_fd(), &VsockAddr::new(nix::libc::VMADDR_CID_ANY, vport)).with_context(|| format!("listening on vsock port {vport}"))?;
    listen(&fd, Backlog::new(64)?)?;
    std::thread::Builder::new().name(format!("inbound-{port}")).spawn(move || {
        loop {
            match accept(fd.as_raw_fd()) {
                Ok(c) => {
                    #[allow(unsafe_code)]
                    // SAFETY: `accept` made `c`, a descriptor nothing else owns.
                    let s = UnixStream::from(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(c) });
                    // Its end once the service is done sending: Firecracker doesn't pass a guest's
                    // half-close on to the host, which would wait for more.
                    let _ = std::thread::Builder::new().name(format!("inbound-{port}-conn")).spawn(move || {
                        if let Ok(t) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                            splice_with(t, s, std::net::Shutdown::Both);
                        }
                    });
                }
                Err(nix::errno::Errno::EINTR) => {}
                Err(_) => return,
            }
        }
    })?;
    Ok(())
}

/// An image built: its layers (`/dev/vdb`) extracted onto its new root file system (`/dev/vdc`).
fn build() -> anyhow::Result<()> {
    system("")?;
    mount("/dev/vdb", "/mnt/image", "ext4", MsFlags::MS_RDONLY, None)?;
    mount("/dev/vdc", "/mnt/scratch", "ext4", MsFlags::empty(), None)?;
    let mut conn = dial(CONTROL_PORT)?;
    send(&mut conn, &FromGuest::Ready)?;
    let layers = match recv::<ToGuest>(&mut conn)? {
        Some(ToGuest::Build(layers)) => layers,
        Some(other) => anyhow::bail!("the host sent {other:?} where an image's layers go"),
        None => anyhow::bail!("the host hung up before sending the layers"),
    };
    let root = Path::new("/mnt/scratch");
    let mut outcome = None;
    for l in &layers {
        anyhow::ensure!(!l.file.is_empty() && !l.file.contains('/'), "`{}` isn't a layer's file", l.file);
        if let Err(e) = apply(&Path::new("/mnt/image").join(&l.file), root) {
            outcome = Some(format!("layer {} ({}): {e:#}", l.file, l.media_type));
            break;
        }
    }
    nix::unistd::sync();
    if let Err(e) = nix::mount::umount2("/mnt/scratch", MntFlags::empty())
        && outcome.is_none()
    {
        outcome = Some(format!("unmounting the image: {e}"));
    }
    send(&mut conn, &FromGuest::Built(outcome))?;
    Ok(())
}

/// One layer applied onto `root`, as OCI layers are: its whiteouts hide what's below (`.wh.<name>`
/// a path, `.wh..wh..opq` a directory's earlier contents), and the rest is unpacked with its owners,
/// modes, times and extended attributes, device files and FIFOs made as they are.
pub fn apply(file: &Path, root: &Path) -> anyhow::Result<()> {
    let mut f = io::BufReader::with_capacity(1 << 20, std::fs::File::open(file)?);
    let head = f.fill_buf()?;
    let reader: Box<dyn Read> = match head {
        [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(f)),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => anyhow::bail!("zstd layers aren't read here yet: push the image with gzip layers"),
        _ => Box::new(f),
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    ar.set_preserve_ownerships(true);
    ar.set_preserve_mtime(true);
    ar.set_unpack_xattrs(true);
    ar.set_overwrite(true);
    // This layer's own paths, which a directory's opaque whiteout doesn't hide.
    let mut made: HashSet<PathBuf> = HashSet::new();
    for entry in ar.entries()? {
        let mut e = entry?;
        let Some(rel) = relative(&e.path()?) else { continue };
        let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
        if name == ".wh..wh..opq" {
            hide_below(&root.join(&parent), &parent, &made)?;
            continue;
        }
        if let Some(hidden) = name.strip_prefix(".wh.") {
            remove(&root.join(&parent).join(hidden))?;
            continue;
        }
        let target = root.join(&rel);
        let kind = e.header().entry_type();
        // What's there in the way: a directory where something else goes, or the reverse.
        if let Ok(m) = std::fs::symlink_metadata(&target) {
            if m.is_dir() && !kind.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else if !m.is_dir() && kind.is_dir() {
                std::fs::remove_file(&target)?;
            }
        }
        if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
            special(&e, &target)?;
        } else {
            e.unpack_in(root).with_context(|| format!("unpacking {}", rel.display()))?;
        }
        made.insert(rel);
    }
    Ok(())
}

/// `p` inside the image: `.`, `/` and `..` taken out (an entry that climbs out is dropped).
fn relative(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// A whiteout: `p` and everything under it gone.
fn remove(p: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p)?,
        Ok(_) => std::fs::remove_file(p)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// An opaque whiteout: what earlier layers put in `dir` (`rel` in the image) gone, what this
/// layer put there kept.
fn hide_below(dir: &Path, rel: &Path, made: &HashSet<PathBuf>) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !made.contains(&rel.join(entry.file_name())) {
            remove(&entry.path())?;
        }
    }
    Ok(())
}

/// A device file or a FIFO, made as the entry says, with its owner.
fn special<R: Read>(e: &tar::Entry<'_, R>, target: &Path) -> anyhow::Result<()> {
    use nix::sys::stat::{Mode, SFlag, makedev, mknod};
    let h = e.header();
    let kind = h.entry_type();
    let flag = if kind.is_character_special() {
        SFlag::S_IFCHR
    } else if kind.is_block_special() {
        SFlag::S_IFBLK
    } else {
        SFlag::S_IFIFO
    };
    let dev = makedev(u64::from(h.device_major()?.unwrap_or(0)), u64::from(h.device_minor()?.unwrap_or(0)));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(target);
    let mode = Mode::from_bits_truncate(nix::libc::mode_t::try_from(h.mode()? & 0o7777).unwrap_or(0o644));
    mknod(target, flag, mode, dev).with_context(|| format!("making {}", target.display()))?;
    let (uid, gid) = (u32::try_from(h.uid()?).unwrap_or(0), u32::try_from(h.gid()?).unwrap_or(0));
    nix::unistd::chown(target, Some(nix::unistd::Uid::from_raw(uid)), Some(nix::unistd::Gid::from_raw(gid)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(dir: &Path, name: &str, entries: &[(&str, Option<&str>)]) -> anyhow::Result<PathBuf> {
        let path = dir.join(name);
        let gz = flate2::write::GzEncoder::new(std::fs::File::create(&path)?, flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        // Owned by whoever runs the test: it may chown only to itself.
        let (uid, gid) = (u64::from(nix::unistd::getuid().as_raw()), u64::from(nix::unistd::getgid().as_raw()));
        for (p, body) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_mtime(0);
            match body {
                Some(text) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(text.len() as u64);
                    h.set_mode(0o644);
                    h.set_cksum();
                    b.append_data(&mut h, p, text.as_bytes())?;
                }
                None => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    h.set_mode(0o755);
                    h.set_cksum();
                    b.append_data(&mut h, p, io::empty())?;
                }
            }
        }
        b.into_inner()?.finish()?;
        Ok(path)
    }

    /// W13-R05: layers applied as OCI applies them: a file's whiteout removes it, a directory's
    /// opaque whiteout hides what earlier layers put there but not what its own layer does, and a
    /// path that climbs out of the image is dropped.
    #[test]
    fn w13_r05_layers_apply_with_their_whiteouts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root)?;
        let base = layer(dir.path(), "0", &[("etc", None), ("etc/keep", Some("kept")), ("etc/gone", Some("gone")), ("opt", None), ("opt/old", Some("old"))])?;
        let top = layer(dir.path(), "1", &[("etc/.wh.gone", Some("")), ("opt/new", Some("new")), ("opt/.wh..wh..opq", Some(""))])?;
        apply(&base, &root)?;
        apply(&top, &root)?;
        assert_eq!(std::fs::read_to_string(root.join("etc/keep"))?, "kept");
        assert!(!root.join("etc/gone").exists(), "a whiteout removes its file");
        assert!(!root.join("opt/old").exists(), "an opaque directory hides what's below");
        assert_eq!(std::fs::read_to_string(root.join("opt/new"))?, "new", "and keeps its own layer's");
        assert_eq!(relative(Path::new("./etc/./keep")), Some(PathBuf::from("etc/keep")));
        assert_eq!(relative(Path::new("/etc/keep")), Some(PathBuf::from("etc/keep")));
        assert_eq!(relative(Path::new("etc/../../escape")), None);
        Ok(())
    }

    /// Frames go across as they were sent, and a closed connection reads as the end.
    #[test]
    fn frames_round_trip() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        send(&mut buf, &ToGuest::Blocked("example.com:443".into()))?;
        send(&mut buf, &FromGuest::Built(Some("no space".into())))?;
        let mut r = io::Cursor::new(buf);
        assert!(matches!(recv::<ToGuest>(&mut r)?, Some(ToGuest::Blocked(b)) if b == "example.com:443"));
        assert!(matches!(recv::<FromGuest>(&mut r)?, Some(FromGuest::Built(Some(e))) if e == "no space"));
        assert!(recv::<ToGuest>(&mut r)?.is_none());
        Ok(())
    }
}
