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
//!
//! As PID 1 it reaps what's orphaned and, once the runner (or the build) is done, reboots, which
//! ends the microVM. What goes wrong is said on its console first.

use std::collections::HashSet;
use std::io::{self, BufRead as _, Read, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

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
    let (Ok(mut a_in), Ok(mut b_out)) = (a.try_clone(), b.try_clone()) else { return };
    let there = std::thread::Builder::new().name("vsock-splice".into()).spawn(move || {
        let _ = io::copy(&mut a_in, &mut b_out);
        let _ = b_out.shutdown(std::net::Shutdown::Write);
    });
    let (mut a_out, mut b_in) = (a, b);
    let _ = io::copy(&mut b_in, &mut a_out);
    let _ = a_out.shutdown(std::net::Shutdown::Write);
    if let Ok(t) = there {
        let _ = t.join();
    }
}

/// `kiln guest init|build`: the microVM's PID 1. Anywhere else it refuses: it would reboot the
/// machine it ran on.
pub fn main(mode: &str) -> std::process::ExitCode {
    if std::process::id() != 1 {
        eprintln!("kiln guest is a microVM's init (docs/ci.md W13-R05): the firecracker executor boots it");
        return std::process::ExitCode::FAILURE;
    }
    let r = match mode {
        "init" => leg(),
        "build" => build(),
        other => Err(anyhow::anyhow!("`kiln guest {other}`: `init` or `build`")),
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

/// A leg: the image under an overlay on the scratch disk, moved into, and its runner forked.
fn leg() -> anyhow::Result<()> {
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
    #[allow(unsafe_code)]
    // SAFETY: PID 1 has one thread here (no runtime has started and nothing has spawned one), so
    // the child may go on as this program does.
    let forked = unsafe { nix::unistd::fork() }.context("starting the leg's runner")?;
    match forked {
        nix::unistd::ForkResult::Child => {
            let code = runner();
            #[allow(clippy::exit)]
            std::process::exit(code)
        }
        nix::unistd::ForkResult::Parent { child } => reap(child),
    }
    Ok(())
}

/// PID 1's work while the runner runs: reaping it and whatever is orphaned, until it's gone.
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
fn runner() -> i32 {
    match prepare() {
        Ok((start, conn)) => {
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

/// Before the runtime starts (with one thread, so the environment may be set): the host dialled,
/// the leg taken, its environment set, and the forwarders started.
fn prepare() -> anyhow::Result<(Box<LegStart>, UnixStream)> {
    let mut conn = dial(CONTROL_PORT)?;
    send(&mut conn, &FromGuest::Ready)?;
    let start = match recv::<ToGuest>(&mut conn)? {
        Some(ToGuest::Leg(start)) => start,
        Some(other) => anyhow::bail!("the host sent {other:?} where a leg goes"),
        None => anyhow::bail!("the host hung up before sending the leg"),
    };
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
    Ok((start, conn))
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
            Ev::Host(ToGuest::Leg(_) | ToGuest::Build(_)) => {}
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
