//! `hex node`: the node agent as a process of its own, serving the `Runtime` seam on its local socket
//! to callers that don't link Hex (decision 3 of docs/design/runtime-architecture.md):
//!
//!   hex node --socket <path> --state <dir> [--box-root <cgroup dir>]
//!            [--quic <address> --credentials <dir> [--staged <dir>]]
//!   hex node enroll --name <name> --credentials <dir>
//!   hex node links --socket <path>
//!
//! With `--quic` it serves the same seam over QUIC too (docs/design/hex-quic.md Q9): its listener binds `<address>`
//! and presents the enrollment kept in `--credentials` (its key, made by `hex node enroll`, and kiln's CA's answer to
//! the request that made: `chain.pem` and `roots.pem`, [`hex_quic::enroll::store`]), read whole before anything opens
//! or binds, so one it can't serve with is refused, typed, with nothing bound; it says `quic <address bound>` on
//! stdout before its `ready`. On Linux `--staged` names where it stages file trees over QUIC; a macOS or Windows node
//! stages none, on any transport. Without `--quic` it binds no QUIC listener. `hex node enroll` begins an enrollment:
//! a key made here, kept in `<dir>` for this user alone, and its request for kiln's CA, whose path it says as
//! `request <path>`. `hex node links` asks the node serving `<path>` for its links over QUIC (docs/design/hex-quic.md
//! Q12) and says each as `link <peer> suite <suite> group <group>`.
//!
//! On macOS (native plan step 8b) the node is the macOS runtime's `process` class with its boxes'
//! records kept in `--state` ([`hex_node::recorded::Recorded`]), and, where a Shards guest is configured
//! (`SHARDS_BIN`, `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD`, ...,
//! [`hex_runtime::guest::GuestConfig::from_env`]), Linux images through that guest, initialized before the node
//! accepts a box (HEX-GUEST-39); `--box-root` (cgroups) is Linux's. On Windows (native plan step 8) the
//! node is the Windows runtime's `process` class, its boxes' records kept in `--state` as macOS's are,
//! its socket an AF_UNIX stream on one completion port (`hex_node::serve::windows`). On Linux:
//!
//! It opens the node, its boxes in cgroups under `--box-root` (else in a subtree beside its own
//! cgroup), their records under `--state` and their containers' state on tmpfs under the runtime
//! directory (`/run/hex-node` as root, `$XDG_RUNTIME_DIR/hex-node` rootless; decision 1), takes back
//! or cleans up what an agent before it left, saying on stderr each box it removed and why
//! (T-T7.1.2), raises its soft descriptor limit to the hard one, says on stderr whether it parks procs with
//! its inits (decision 8, parked procs: a root node's, never a rootless one's), binds the
//! socket, says `ready <path>` on stdout (and `READY=1` to systemd, where `$NOTIFY_SOCKET` names it), and serves
//! until it is signalled; if serving fails it says why on stderr and exits 1. On SIGTERM it writes one line of its
//! parked procs' counts on stderr (made, hits, misses, dry, capped, alive) and exits 143 with nothing else done: its
//! boxes, connections and socket's file are left as a SIGKILL leaves them, for the next agent's recovery. A socket an
//! ended agent left at the path is replaced; one a live agent serves is refused. A root node whose network-namespace
//! pool was refused for a capability it lacks says so on stderr as it opens (decision 4). A debug build also takes
//! `--crash-at <cgroup-made|bundle-rendered|record-first-write>`, which arms the node's crash point there
//! (`hex_node::CrashPoint`) for T-T7.1.2's crash test: from the agent's own arguments alone, so no container can arm
//! it. The macOS node, too, says on stderr each box its recovery removed and why.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hex_api::Refusal;
#[cfg(target_os = "linux")]
use hex_core::netns::Counts;
#[cfg(all(target_os = "linux", debug_assertions))]
use hex_node::CrashPoint;
#[cfg(target_os = "linux")]
use hex_node::Node;
use hex_node::RecoverySummary;
use hex_node::serve::{Host, Limits, Server};
#[cfg(target_os = "macos")]
use rustix::process::Resource;

/// Runs `hex node`'s arguments (everything after `node`); returns the exit status.
pub fn main(args: &[String]) -> u8 {
    #[cfg(target_os = "linux")]
    match args {
        [descriptors_flag] if descriptors_flag == "--descriptors" => {
            return descriptors(Path::new("/"));
        }
        [descriptors_flag, state_flag, state]
            if descriptors_flag == "--descriptors" && state_flag == "--state" =>
        {
            return descriptors(Path::new(state));
        }
        _ => {}
    }
    let ran = match args {
        [verb, rest @ ..] if verb == "enroll" => enroll(rest),
        [verb, rest @ ..] if verb == "links" => links(rest),
        _ => serve(args),
    };
    match ran {
        Ok(()) => 0,
        Err((status, why)) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(format!("hex node: {why}\n").as_bytes());
            status
        }
    }
}

const ENROLL_USAGE: &str = "usage: hex node enroll --name <name> --credentials <dir>";

/// `hex node enroll --name <name> --credentials <dir>`: begins `<name>`'s enrollment in `<dir>`
/// ([`hex_quic::enroll::store::begin`]) and says `request <path>` on stdout, the request kiln's CA answers.
fn enroll(args: &[String]) -> Result<(), (u8, String)> {
    let (mut name, mut dir) = (None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.as_str() {
            "--name" => &mut name,
            "--credentials" => &mut dir,
            other => return Err((2, format!("unknown argument {other:?}\n{ENROLL_USAGE}"))),
        };
        let value = rest
            .next()
            .ok_or_else(|| (2, format!("{flag} needs a value\n{ENROLL_USAGE}")))?;
        if slot.replace(value.clone()).is_some() {
            return Err((2, format!("{flag} is given twice\n{ENROLL_USAGE}")));
        }
    }
    let name = name.ok_or_else(|| (2, format!("--name is needed\n{ENROLL_USAGE}")))?;
    let dir = dir.ok_or_else(|| (2, format!("--credentials is needed\n{ENROLL_USAGE}")))?;
    let request =
        hex_quic::enroll::store::begin(Path::new(&dir), &name).map_err(|e| (1, e.to_string()))?;
    let mut out = std::io::stdout().lock();
    out.write_all(format!("request {}\n", request.display()).as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| (1, format!("saying the request's path: {e}")))
}

const LINKS_USAGE: &str = "usage: hex node links --socket <path>";

/// `hex node links --socket <path>`: asks the node serving `<path>` for its links over QUIC
/// ([`hex_client::Client::links`], docs/design/hex-quic.md Q12) and says each on a line of stdout ([`link_line`]). A
/// node serving no QUIC listener refuses, typed, and that is said on stderr (exit 1).
fn links(args: &[String]) -> Result<(), (u8, String)> {
    let [flag, socket] = args else {
        return Err((2, LINKS_USAGE.to_owned()));
    };
    if flag != "--socket" {
        return Err((2, format!("unknown argument {flag:?}\n{LINKS_USAGE}")));
    }
    let client = hex_client::Client::connect(Path::new(socket)).map_err(|e| (1, e.to_string()))?;
    let links = client.links().map_err(|e| (1, e.to_string()))?;
    let mut out = std::io::stdout().lock();
    links
        .iter()
        .try_for_each(|link| out.write_all(link_line(link).as_bytes()))
        .and_then(|()| out.flush())
        .map_err(|e| (1, format!("saying the node's links: {e}")))
}

/// A link as `hex node links` says it: `link <peer> suite <suite> group <group>`, the suite and group as rustls's Debug
/// names their registries' values, as kiln's reach links say theirs (`TLS13_AES_256_GCM_SHA384`,
/// `secp384r1MLKEM1024`; one it gives no name as `CipherSuite(0x…)`), and `group none` where the handshake named none.
/// A peer is its certificate's DNS name, which holds no space, so the line's words are its fields.
fn link_line(link: &hex_core::wire::Link) -> String {
    let suite = hex_quic::CipherSuite::from(link.suite);
    let group = link.group.map_or_else(
        || "none".to_owned(),
        |group| format!("{:?}", hex_quic::NamedGroup::from(group)),
    );
    format!("link {} suite {suite:?} group {group}\n", link.peer)
}

#[cfg(target_os = "linux")]
const USAGE: &str = "usage: hex node --socket <path> --state <dir> [--box-root <cgroup dir>] [--quic <address> --credentials <dir> [--staged <dir>]] | hex node --descriptors [--state <dir>] | hex node enroll --name <name> --credentials <dir> | hex node links --socket <path>";
#[cfg(not(target_os = "linux"))]
const USAGE: &str = "usage: hex node --socket <path> --state <dir> [--quic <address> --credentials <dir>] | hex node enroll --name <name> --credentials <dir> | hex node links --socket <path>";

/// `hex node --descriptors [--state <dir>]`: what a node of this process's owner, its state under `state` (`/` where
/// none is named), running VM boxes where a backend is configured as `hex node` reads it, needs of its soft
/// descriptor limit on this host, at a first start's bounds, its own and every kept namespace's files
/// (`hex_node::netns::needed_on_this_host`, the one formula), said as `own N files M need T`: what an installer gives
/// the node's service and the install lab checks, never a figure copied from one host's (goal 6 step 6's HEX-INST-5).
#[cfg(target_os = "linux")]
fn descriptors(state: &Path) -> u8 {
    let runs = hex_node::Runs {
        vms: guest_requested() || std::env::var_os("HEX_FIRECRACKER_CONFIG").is_some(),
    };
    let needed = hex_node::netns::needed_on_this_host(state, runs);
    match needed {
        Ok(n) => {
            let line = format!("own {} files {} need {}\n", n.own, n.files, n.total());
            match std::io::stdout().lock().write_all(line.as_bytes()) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        }
        Err(e) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(format!("hex node: {e}\n").as_bytes());
            1
        }
    }
}

struct Args {
    socket: PathBuf,
    state: PathBuf,
    box_root: Option<PathBuf>,
    quic: Option<QuicArgs>,
    #[cfg(all(target_os = "linux", debug_assertions))]
    crash_at: Option<CrashPoint>,
}

/// The node's listener over QUIC, where `--quic` asks for one (docs/design/hex-quic.md Q9).
struct QuicArgs {
    /// The address it binds.
    bind: std::net::SocketAddr,
    /// The folder its enrollment is kept in ([`hex_quic::enroll::store`]).
    credentials: PathBuf,
    /// Where a Linux node stages file trees over QUIC.
    staged: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let (mut socket, mut state, mut box_root) = (None, None, None);
    let (mut quic, mut credentials, mut staged) = (None, None, None);
    #[cfg(all(target_os = "linux", debug_assertions))]
    let mut crash_at = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        if flag == "--quic" {
            let value = rest
                .next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
            let bind = value.parse::<std::net::SocketAddr>().map_err(|e| {
                format!("{flag} needs an address and port, not {value:?}: {e}\n{USAGE}")
            })?;
            if quic.replace(bind).is_some() {
                return Err(format!("{flag} is given twice\n{USAGE}"));
            }
            continue;
        }
        #[cfg(all(target_os = "linux", debug_assertions))]
        if flag == "--crash-at" {
            let name = rest
                .next()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
            let point = CrashPoint::named(name)
                .ok_or_else(|| format!("{flag} names no crash point: {name:?}"))?;
            if crash_at.replace(point).is_some() {
                return Err(format!("{flag} is given twice\n{USAGE}"));
            }
            continue;
        }
        let slot = match flag.as_str() {
            "--socket" => &mut socket,
            "--state" => &mut state,
            "--box-root" => &mut box_root,
            "--credentials" => &mut credentials,
            "--staged" => &mut staged,
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        };
        let value = rest
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        if slot.replace(PathBuf::from(value)).is_some() {
            return Err(format!("{flag} is given twice\n{USAGE}"));
        }
    }
    let quic = match (quic, credentials, staged) {
        (None, None, None) => None,
        (Some(_), None, _) => {
            return Err(format!(
                "--quic needs --credentials <dir>, the node's enrollment (`hex node enroll`)\n{USAGE}"
            ));
        }
        (None, _, _) => return Err(format!("--credentials and --staged are --quic's\n{USAGE}")),
        (Some(bind), Some(credentials), staged) => {
            if staged.is_some() && !cfg!(target_os = "linux") {
                return Err(format!(
                    "--staged names where a Linux node stages file trees over QUIC; this node stages none, \
                     on any transport\n{USAGE}"
                ));
            }
            Some(QuicArgs {
                bind,
                credentials,
                staged,
            })
        }
    };
    Ok(Args {
        socket: socket.ok_or_else(|| format!("--socket is needed\n{USAGE}"))?,
        state: state.ok_or_else(|| format!("--state is needed\n{USAGE}"))?,
        box_root,
        quic,
        #[cfg(all(target_os = "linux", debug_assertions))]
        crash_at,
    })
}

fn serve(args: &[String]) -> Result<(), (u8, String)> {
    let mut args = parse(args).map_err(|why| (2, why))?;
    let failed = |e: Refusal| (1, e.to_string());
    // The enrollment the QUIC listener presents is read whole before anything opens or binds: one it can't serve with
    // is refused, typed, with nothing bound (docs/design/hex-quic.md Q9).
    let credentials = args
        .quic
        .as_ref()
        .map(|quic| {
            hex_quic::enroll::store::load(&quic.credentials).map_err(|e| {
                (
                    1,
                    format!(
                        "{e} (enroll with `hex node enroll`, and keep kiln's CA's answer there)"
                    ),
                )
            })
        })
        .transpose()?;
    // The waits census, where the probe asks for it (bench/competitive/probes/waits_census.md); nothing otherwise.
    hex_node::census::install_from_env().map_err(failed)?;
    // SIGTERM is read from a signalfd, blocked before the node starts a thread so every thread inherits the block;
    // each process the node starts has its mask emptied before it execs (the seeds' and execs' `default_signals`,
    // std's Command), so none inherits it.
    #[cfg(target_os = "linux")]
    let term =
        hex_runtime::linux::oci::sys::forward_signals(&[rustix::process::Signal::TERM.as_raw()])
            .map_err(|e| (1, format!("reading SIGTERM: {e}")))?;
    // A Linux node raises its own at its start (hex_node::netns::hold_descriptors), as every embedding does.
    #[cfg(target_os = "macos")]
    raise_descriptor_limit().map_err(failed)?;
    // The state's disk is what the boxes' io bounds apply to, so it exists before the node looks.
    std::fs::create_dir_all(&args.state)
        .map_err(|e| (1, format!("making {}: {e}", args.state.display())))?;
    args.state = std::fs::canonicalize(&args.state)
        .map_err(|e| (1, format!("resolving node state: {e}")))?;
    let hosted = host(&args)?;
    let (ended, end) = std::sync::mpsc::channel();
    #[cfg(target_os = "linux")]
    end_on_sigterm(term, Arc::clone(&hosted.node), ended.clone()).map_err(failed)?;
    match (args.quic.take(), credentials) {
        (Some(quic), Some(credentials)) => {
            let staging = quic.staged.is_some();
            let endpoint = hex_quic::Endpoint::start(hex_quic::Config {
                credentials,
                bind: quic.bind,
                listen: true,
                serves: hex_node::quic::ports(staging),
                shape: hex_node::quic::shape(&hosted.limits, staging),
            })
            .map_err(|e| {
                (
                    1,
                    format!("starting the QUIC listener at {}: {e}", quic.bind),
                )
            })?;
            let endpoint = Arc::new(endpoint);
            let bound = endpoint.local_addr();
            let os = hex_node::quic::Quic::new(
                hex_node::serve::Native::bind(&args.socket).map_err(failed)?,
                endpoint,
                hex_node::quic::Bounds::of(&hosted.limits),
                quic.staged,
            )
            .map_err(failed)?;
            let server =
                Server::with_os(os, hosted.host, hosted.limits, hosted.workers, &args.socket)
                    .map_err(failed)?;
            served(server, &args.socket, Some(bound), ended, &end)
        }
        _ => {
            let server = Server::bind(&args.socket, hosted.host, hosted.limits, hosted.workers)
                .map_err(failed)?;
            served(server, &args.socket, None, ended, &end)
        }
    }
}

/// Says the node is ready, its QUIC listener's address first where it has one (`quic <address>`), then `ready <path>`,
/// and serves until [`serve`]'s end comes: the server's own, or on Linux a SIGTERM's.
fn served<O: hex_node::serve::Os + 'static>(
    mut server: Server<O>,
    socket: &Path,
    quic: Option<std::net::SocketAddr>,
    ended: std::sync::mpsc::Sender<(u8, String)>,
    end: &std::sync::mpsc::Receiver<(u8, String)>,
) -> Result<(), (u8, String)> {
    let mut out = std::io::stdout().lock();
    let said = match quic {
        Some(at) => format!("quic {at}\nready {}\n", socket.display()),
        None => format!("ready {}\n", socket.display()),
    };
    out.write_all(said.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| (1, format!("saying it is ready: {e}")))?;
    drop(out);
    // systemd's readiness protocol is Linux's; launchd starts a job without one.
    #[cfg(target_os = "linux")]
    tell_the_service_manager().map_err(|e| (1, format!("telling systemd it is ready: {e}")))?;
    std::thread::Builder::new()
        .name("hex-serve".to_owned())
        .spawn(move || {
            let why = server
                .wait()
                .unwrap_or_else(|| "its socket stopped".to_owned());
            let _ = ended.send((1, why));
        })
        .map_err(|e| (1, format!("starting the server's thread: {e}")))?;
    Err(end
        .recv()
        .unwrap_or_else(|_| (1, "its ending threads stopped unsaid".to_owned())))
}

/// What ends the node: on Linux a thread reading SIGTERM sends its exit status and last words here, and the thread
/// that owns the server sends why it stopped. [`serve`] returns the first that comes, so `main` exits with the server
/// still owned by a thread blocked in its wait: no destructor runs, and the server, its connections and its boxes are
/// left as a SIGKILL leaves them, for the next agent's recovery (T-T7.1.2).
#[cfg(target_os = "linux")]
type Ended = std::sync::mpsc::Sender<(u8, String)>;

/// A node to serve: its host, its socket's bounds and workers, and on Linux the node itself, whose parked procs a
/// SIGTERM's line reports.
struct Hosted {
    host: Arc<dyn Host>,
    limits: Limits,
    workers: usize,
    #[cfg(target_os = "linux")]
    node: Arc<Node>,
}

/// Under systemd's `Type=notify` (`$NOTIFY_SOCKET` set), says `READY=1` there, so the unit is up exactly
/// when the node serves (sd_notify(3): one datagram to the socket the variable names, a path or, after
/// `@`, a Linux abstract name). Non-blocking: a manager not reading is an error, never a wait.
#[cfg(target_os = "linux")]
fn tell_the_service_manager() -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let Some(at) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(at.as_os_str());
    let addr = match bytes.strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(std::path::Path::new(&at))?,
    };
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    socket.send_to_addr(b"READY=1", &addr)?;
    Ok(())
}

/// Says on stderr each box a recovery removed and why, each idle cgroup it removed that no box's record named, each
/// torn record it held, why and what to do, why its guest couldn't say which boxes it holds, and why the host's
/// boot session couldn't be read, so nothing is dropped unseen (CLAUDE.md §3), before the socket opens.
/// Starts the node's charge learner (joint plan step 14) and says what of the rule its platform can't hold, or says
/// why it doesn't run: a platform whose window nothing derives yet refuses it, typed, and the node runs on with its
/// VMs booked whole.
#[cfg(any(target_os = "macos", windows))]
fn charges<R: hex_node::recorded::Recover + Send + Sync + 'static>(
    node: &Arc<hex_node::recorded::Recorded<R>>,
) {
    let mut err = std::io::stderr().lock();
    match node.start_charges() {
        Ok((window, unheld)) => {
            let _ = writeln!(
                err,
                "hex node: the charge learner's window is {} ns",
                window.as_nanos()
            );
            for why in unheld {
                let _ = writeln!(err, "hex node: the charge learner can't hold this: {why}");
            }
        }
        Err(why) => {
            let _ = writeln!(err, "hex node: the charge learner doesn't run: {why}");
        }
    }
}

fn say_recovered(recovered: &RecoverySummary) {
    let mut err = std::io::stderr().lock();
    // Slots are Linux's node's alone: no other OS's hands a box's cgroup on.
    #[cfg(target_os = "linux")]
    for n in &recovered.slots_removed {
        let _ = writeln!(
            err,
            "hex node: recovered: removed idle cgroup {}: no box's record named it",
            hex_core::reuse::name(*n)
        );
    }
    for (id, why) in &recovered.removed {
        let _ = writeln!(err, "hex node: recovered: removed {id}: {why}");
    }
    for (id, why) in &recovered.held {
        let _ = writeln!(err, "hex node: recovered: held {id}: {why}");
    }
    if let Some(why) = &recovered.orphans_unknown {
        let _ = writeln!(
            err,
            "hex node: recovered: the guest couldn't say which boxes it holds ({why}): none was deleted, \
             and a box no record names waits for the next start"
        );
    }
    if let Some(why) = &recovered.boot_unknown {
        let _ = writeln!(
            err,
            "hex node: recovered: the host's boot session couldn't be read ({why}): no record was known for \
             an earlier boot's, and the records written now carry none"
        );
    }
}

/// The Linux node: its boxes in cgroups under `--box-root` (else beside its own cgroup), recovered, its socket's
/// bounds from its host ([`hex_node::serve::limits`]).
#[cfg(target_os = "linux")]
fn host(args: &Args) -> Result<Hosted, (u8, String)> {
    let failed = |e: Refusal| (1, e.to_string());
    let guest_config = guest_configuration().map_err(failed)?;
    let firecracker_config = match std::env::var_os("HEX_FIRECRACKER_CONFIG") {
        None => None,
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(failed(startup_invalid(
                    "HEX_FIRECRACKER_CONFIG",
                    "expected an absolute configured backend file",
                )));
            }
            Some(hex_runtime::firecracker::Config::from_file(&path).map_err(failed)?)
        }
    };
    // Its start counts what it runs past its own boxes among its own descriptors: VM boxes where a backend is configured.
    let runs = hex_node::Runs {
        vms: guest_config.is_some() || firecracker_config.is_some(),
    };
    // Made before the node's start, so the GPU handles it holds are among what the start counts as held.
    let prober = hex_node::facts::shared_prober(&args.state, hex_node::facts::Declared::default())
        .map_err(failed)?;
    let node = match &args.box_root {
        Some(root) => Node::open_running(root, &args.state, &args.state, runs),
        None => Node::beside_self_running(&args.state, &args.state, runs),
    }
    .map_err(failed)?;
    // What a label whose module the host's kernel doesn't run does: its owner's word, else skipped with a
    // warning.
    let node = node
        .with_lsm(hex_runtime::linux::oci::lsm::Policy::from_env().map_err(|e| failed(e.into()))?);
    #[cfg(debug_assertions)]
    let node = match args.crash_at {
        Some(point) => node.with_crash_point(point),
        None => node,
    };
    if let Some(line) = refused_pool(&node.netns().map_err(failed)?) {
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }
    let node = Arc::new(node);
    node.bind_prober(prober).map_err(failed)?;
    let shards = configured_guest(
        guest_config,
        &args.state,
        node.device_catalog(),
        node.shards_native_paths().map_err(failed)?,
    )?
    .map(Arc::new);
    let firecracker = firecracker_config
        .map(|config| {
            hex_runtime::firecracker::FirecrackerRuntime::open(
                config,
                node.device_catalog(),
                node.backend_books(),
                node.single_use_marks(),
                node.boot_session(),
            )
            .map(Arc::new)
            .map_err(failed)
        })
        .transpose()?;
    node.install_backends(firecracker, shards).map_err(failed)?;
    // What a fleet's driver reads the learner's windows by (joint plan step 14): none where no VM backend runs it.
    if let Some(window) = node.charge_window() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "hex node: the charge learner's window is {} ns",
            window.as_nanos()
        );
    }
    // Recovery removes a box it can't take back; say which and why, so nothing is dropped unseen
    // (CLAUDE.md §3), before the socket opens.
    say_recovered(&node.recover().map_err(failed)?);
    {
        let mut err = std::io::stderr().lock();
        // The kept namespaces' held files fit in the descriptor limit the node was given, never raised:
        // say how many may hold them, so a node at a default limit isn't read as holding any.
        if let Ok(Some(kept)) = node.netns_reuse()
            && let Some(files) = kept.files_bound
        {
            let soft = files
                .soft
                .map_or_else(|| "unlimited".to_owned(), |n| n.to_string());
            let _ = writeln!(
                err,
                "hex node: kept network namespaces hold their /proc/net files for at most {} of them: \
                 the soft descriptor limit is {soft}, the node's own at its bounds {}{}",
                files.namespaces,
                files.own,
                if files.namespaces == 0 {
                    "; each check enters its namespace instead, counted (a soft limit past the node's own \
                     lets them hold files)"
                } else {
                    ""
                }
            );
        }
    }
    {
        let said = if node.parked_procs() {
            format!("on, at most {} alive", node.sizes().procs_alive())
        } else {
            "off: a rootless node parks none".to_owned()
        };
        let _ = writeln!(std::io::stderr().lock(), "hex node: parked procs: {said}");
    }
    let platform = node.sizes().platform();
    Ok(Hosted {
        host: Arc::clone(&node) as Arc<dyn Host>,
        limits: hex_node::serve::limits(&platform),
        workers: platform.threads,
        node,
    })
}

/// The macOS node: the `process` class, and Linux images in a Shards guest where one is configured, its socket's
/// bounds from its kernel ([`hex_node::serve::macos::platform`]).
#[cfg(target_os = "macos")]
fn host(args: &Args) -> Result<Hosted, (u8, String)> {
    use hex_node::recorded::Recorded;
    use hex_runtime::guest::WithGuest;
    let failed = |e: Refusal| (1, e.to_string());
    let guest_config = guest_configuration().map_err(failed)?;
    if args.box_root.is_some() {
        return Err((
            2,
            format!("--box-root names a cgroup directory, which is Linux's\n{USAGE}"),
        ));
    }
    // The node's machine class's records seed its boxes' pacing and their hold, which its watchers then
    // learn up from (hex_core::watch::seed and lateness_seed of its hw.model; a model with no record takes
    // twice the highest on record).
    let model = hex_runtime::macos::machine_model().map_err(failed)?;
    let rates = hex_core::watch::seed(&model);
    let process = hex_runtime::macos::MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .map_err(failed)?;
    let managed = guest_config
        .as_ref()
        .map(|inputs| managed_control(inputs, &model))
        .transpose()
        .map_err(failed)?;
    let prober = hex_node::facts::shared_prober(&args.state, hex_node::facts::Declared::default())
        .map_err(failed)?;
    let catalog = hex_runtime::devices::shared_catalog(
        &backend_path(&args.state, "devices").map_err(failed)?,
    )
    .map_err(failed)?;
    let host: Arc<dyn Host> = match configured_guest(
        guest_config,
        &args.state,
        catalog,
        runtime_paths(&args.state).map_err(failed)?,
    )? {
        Some(guest) => {
            let (control, cost) = managed.ok_or_else(|| {
                failed(Refusal::Broken(
                    "a configured guest lost its managed CPU control".to_owned(),
                ))
            })?;
            let process = managed_host(
                process,
                &args.state,
                control,
                &cost,
                guest.first_dial_bound().map_err(failed)?,
            )
            .map_err(failed)?;
            let routed = WithGuest::configured(process, guest, hex_runtime::guest::by_class);
            let node =
                Arc::new(Recorded::open_with_prober(routed, &args.state, prober).map_err(failed)?);
            node.bind_guest_books().map_err(failed)?;
            // Joint plan step 14: what this node recorded under a census cost is retired first.
            node.retire_census().map_err(failed)?;
            // AC-T7.4.1: the wrapper validates ALL retained owners before publishing
            // Prepared. Fresh GPU observation waits for its real Index/native grant.
            node.prepare_guest_backing()
                .map_err(failed)?
                .allocation()
                .map_err(failed)?;
            node.recover_guest_backing().map_err(failed)?;
            say_recovered(&node.recover().map_err(failed)?);
            charges(&node);
            node
        }
        None => {
            let node = Recorded::open_with_prober(process, &args.state, prober).map_err(failed)?;
            say_recovered(&node.recover().map_err(failed)?);
            Arc::new(node)
        }
    };
    let platform = hex_node::serve::macos::platform().map_err(failed)?;
    Ok(Hosted {
        host,
        limits: hex_node::serve::limits(&platform),
        workers: platform.threads,
    })
}

/// On SIGTERM, from a thread of its own: one line of the node's parked procs' counts, status 143 (128 + SIGTERM, as a
/// shell reports a SIGTERM's end). The signalfd reads only SIGTERM; a read that fails ends the node saying why. A count
/// the node can't read is said as unread, never left out.
#[cfg(target_os = "linux")]
fn end_on_sigterm(
    term: hex_runtime::linux::oci::sys::ForwardedSignals,
    node: Arc<Node>,
    ended: Ended,
) -> Result<(), Refusal> {
    std::thread::Builder::new()
        .name("hex-sigterm".to_owned())
        .spawn(move || {
            let end = match term.next() {
                Ok(_) => (
                    143,
                    format!("parked procs at exit: {}", parked_counts(&node)),
                ),
                Err(e) => (1, format!("reading SIGTERM: {e}")),
            };
            let _ = ended.send(end);
        })
        .map(drop)
        .map_err(|e| Refusal::os("starting the SIGTERM thread", e))
}

#[cfg(target_os = "linux")]
fn parked_counts(node: &Node) -> String {
    match node.cgroup_reuse() {
        Ok(c) => {
            let p = c.parked;
            format!(
                "made {}, hits {}, misses {}, dry {}, capped {}, alive {}",
                p.procs_made, p.proc_hits, p.proc_misses, p.proc_dry, p.procs_capped, p.procs_alive
            )
        }
        Err(e) => format!("unread: {e}"),
    }
}

/// What `hex node` says on stderr when its network-namespace pool was refused: what the node lacks,
/// and that its creates make their namespaces on the path, which its counts say as `refused_on_path`.
#[cfg(target_os = "linux")]
fn refused_pool(counts: &Counts) -> Option<String> {
    counts.refused.map(|missing| {
        format!(
            "hex node: network-namespace pool refused: this node lacks {missing}, which making one \
             ahead takes; each create makes its container's on its path (counted as refused_on_path)\n"
        )
    })
}

/// The Windows node: the `process` and `oci` classes, its boxes outliving it through a keeper (`hex keep`,
/// started apart from the node and holding a second handle to each box's job), their records in `--state`,
/// taken back or cleaned up after a crash (T-T7.1.2, native plan step 8). The node and the box jobs are
/// named for the state folder, so the next node on the same `--state` finds and takes back the keeper and
/// the boxes. `--box-root` is Linux's.
#[cfg(windows)]
fn host(args: &Args) -> Result<Hosted, (u8, String)> {
    use hex_node::recorded::Recorded;
    use hex_runtime::windows::{WinRuntime, keeper};
    let failed = |e: Refusal| (1, e.to_string());
    let guest_config = guest_configuration().map_err(failed)?;
    if args.box_root.is_some() {
        return Err((
            2,
            format!("--box-root names a cgroup directory, which is Linux's\n{USAGE}"),
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|e| (1, format!("the node can't name its own binary: {e}")))?;
    // `hex keep`, started by the runtime with its state folder and ready path in the environment.
    let keep = vec![exe.to_string_lossy().into_owned(), "keep".to_owned()];
    let keeper_dir = args.state.join("keeper");
    let name = keeper::node_name(&keeper_dir);
    let catalog = hex_runtime::devices::shared_catalog(
        &backend_path(&args.state, "devices").map_err(failed)?,
    )
    .map_err(failed)?;
    let runtime = WinRuntime::outliving_with_catalog(&name, &keeper_dir, &keep, catalog.clone())
        .map_err(failed)?;
    let prober = hex_node::facts::shared_prober(&args.state, hex_node::facts::Declared::default())
        .map_err(failed)?;
    let host: Arc<dyn Host> = match configured_guest(
        guest_config,
        &args.state,
        catalog,
        runtime_paths(&args.state).map_err(failed)?,
    )? {
        Some(guest) => {
            let routed =
                hex_runtime::guest::WithGuest::configured(runtime, guest, WinRuntime::oci_side);
            let node =
                Arc::new(Recorded::open_with_prober(routed, &args.state, prober).map_err(failed)?);
            node.bind_guest_books().map_err(failed)?;
            // Joint plan step 14: what this node recorded under a census cost is retired first.
            node.retire_census().map_err(failed)?;
            // AC-T7.4.1: the wrapper validates ALL retained owners before publishing
            // Prepared. Fresh GPU observation waits for its real Index/native grant.
            node.prepare_guest_backing()
                .map_err(failed)?
                .allocation()
                .map_err(failed)?;
            node.recover_guest_backing().map_err(failed)?;
            say_recovered(&node.recover().map_err(failed)?);
            charges(&node);
            node
        }
        None => {
            let node = Recorded::open_with_prober(runtime, &args.state, prober).map_err(failed)?;
            say_recovered(&node.recover().map_err(failed)?);
            Arc::new(node)
        }
    };
    let platform = hex_node::serve::windows::platform().map_err(failed)?;
    Ok(Hosted {
        host,
        limits: hex_node::serve::limits(&platform),
        workers: platform.threads,
    })
}

/// AC-T7.4.1: configure the actual renderer and its verified cost before recovery.
/// The existing native owner supplies both catalog and Books; this constructor boots nothing.
fn guest_configuration() -> Result<Option<hex_runtime::guest::GuestInputs>, Refusal> {
    if !guest_requested() {
        return Ok(None);
    }
    hex_runtime::guest::GuestConfig::from_env_with_native_inputs().map(Some)
}

/// Whether any of a Shards guest's configuration is given, which `hex node` then reads whole or refuses.
fn guest_requested() -> bool {
    [
        hex_runtime::guest::ENV_SHARDS_BIN,
        hex_runtime::guest::ENV_KERNEL,
        hex_runtime::guest::ENV_INITRD,
        hex_runtime::guest::ENV_IMAGE,
        hex_runtime::guest::ENV_CPUS,
        hex_runtime::guest::ENV_MEMORY_MIB,
        hex_runtime::guest::ENV_GPU,
        hex_runtime::guest::ENV_COST_FILE,
        hex_runtime::guest::ENV_COST_SHA256,
        hex_runtime::guest::ENV_COST_EVIDENCE,
        hex_runtime::guest::ENV_GPU_OBSERVER_FILE,
        hex_runtime::guest::ENV_GPU_OBSERVER_SHA256,
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some())
}

fn configured_guest(
    configuration: Option<hex_runtime::guest::GuestInputs>,
    state: &Path,
    catalog: hex_runtime::devices::SharedCatalog,
    paths: (PathBuf, PathBuf),
) -> Result<Option<hex_runtime::guest::GuestRuntime>, (u8, String)> {
    use hex_runtime::guest::{GuestInputs, GuestRuntime};
    let Some(GuestInputs {
        config,
        cost,
        native_images,
        cpu_responses: _,
    }) = configuration
    else {
        return Ok(None);
    };
    let failed = |e: Refusal| (1, e.to_string());
    let provider = shards_api::Shards::configured(paths.0, paths.1).map_err(|why| {
        failed(hex_client::guest::shards::refusal(
            "configuring node runtime paths",
            &why,
        ))
    })?;
    let paths = provider.runtime_paths().try_clone().map_err(|why| {
        failed(hex_client::guest::shards::refusal(
            "retaining node runtime paths",
            &why,
        ))
    })?;
    // The Shards class on Agentfile images (joint plan step 12): Shards' image boot, which every host runs, or
    // the refusal naming this node's SHARDS_KERNEL and SHARDS_INIT where its run hosts couldn't take them as a
    // pair, read here once, which a create of the class then says; its waits learned from their own answers
    // (decision 9: no record is read).
    let mut guest = GuestRuntime::new(Arc::new(provider), config)
        .recorded_at(backend_path(state, "@guest").map_err(failed)?)
        .with_runtime_paths(paths)
        .with_gpu_catalog(catalog)
        .with_images(
            hex_runtime::guest::host_images(),
            Ok(hex_runtime::guest::ImageWaits::new()),
            backend_path(state, "image-diagnostics").map_err(failed)?,
        )
        .with_runtime_cost(cost)
        .map_err(failed)?;
    if let Some(images) = native_images {
        guest = guest.with_native_images(images).map_err(failed)?;
    }
    if let Some(input) = hex_runtime::guest::GpuObserverInput::from_env().map_err(failed)? {
        guest = guest.with_gpu_observer(input).map_err(failed)?;
    }
    Ok(Some(guest))
}

/// AC-T7.4.1: a managed VM on macOS runs only under its measured CPU control. The response is the cost
/// evidence's row for this host class (the same evidence the cost id hashes); a host with none is
/// refused, typed, naming the evidence it lacks. Nothing is defaulted.
#[cfg(target_os = "macos")]
fn managed_control(
    inputs: &hex_runtime::guest::GuestInputs,
    model: &str,
) -> Result<(hex_runtime::macos::CpuControl, hex_api::RuntimeCost), Refusal> {
    let row = inputs
        .cpu_responses
        .iter()
        .find(|row| row.host == model)
        .ok_or_else(|| {
            let mut id = String::new();
            for byte in inputs.cost.id {
                use std::fmt::Write as _;
                let _ = write!(id, "{byte:02x}");
            }
            Refusal::Invalid {
                what: "managed VM CPU response evidence",
                why: format!(
                    "cost evidence {id} has no measured CPU-response row for host class {model}"
                ),
            }
        })?;
    Ok((
        hex_runtime::macos::CpuControl {
            cost_id: inputs.cost.id,
            response: row.response(),
        },
        inputs.cost,
    ))
}

/// The macOS runtime with its measured CPU control and its persistent managed-VM owner: this same
/// binary, its endpoint and receipt in the node's private `managed-helper` folder (0700), its control
/// patience the guest's own measured boot bound. Nothing starts here; the helper starts with the first
/// admitted scope (AC-T7.4.1).
#[cfg(target_os = "macos")]
fn managed_host(
    process: hex_runtime::macos::MacRuntime,
    state: &Path,
    control: hex_runtime::macos::CpuControl,
    cost: &hex_api::RuntimeCost,
    patience: std::time::Duration,
) -> Result<hex_runtime::macos::MacRuntime, Refusal> {
    use std::os::unix::fs::DirBuilderExt;
    let directory = backend_path(state, "managed-helper")?;
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(Refusal::os("making the managed helper's folder", error)),
    }
    let binary = std::env::current_exe()
        .map_err(|error| Refusal::os("naming this binary for its managed helper", error))?;
    let helper = hex_runtime::macos::ManagedHelper::new(
        binary,
        directory.join("helper.sock"),
        directory.join("helper.receipt"),
        patience,
    )?;
    let runtime = process
        .with_cpu_control(control)?
        .with_managed_helper(helper)?;
    runtime.check_vm_cost(cost)?;
    Ok(runtime)
}

#[cfg(any(target_os = "macos", windows))]
fn runtime_paths(state: &Path) -> Result<(PathBuf, PathBuf), Refusal> {
    Ok((
        backend_path(state, "shards-native-cache")?,
        backend_path(state, "shards-native-home")?,
    ))
}

fn backend_path(state: &Path, name: &str) -> Result<PathBuf, Refusal> {
    let bound = state
        .as_os_str()
        .len()
        .checked_add(name.len())
        .and_then(|size| size.checked_add(1))
        .ok_or(Refusal::Exhausted {
            what: "node backend path bytes",
            bound: usize::MAX,
        })?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(bound)
        .map_err(|_| Refusal::Exhausted {
            what: "node backend path bytes",
            bound,
        })?;
    path.push(state);
    path.push(name);
    Ok(path)
}

#[cfg(target_os = "linux")]
fn startup_invalid(what: &'static str, message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return Refusal::Exhausted {
            what: "backend configuration refusal bytes",
            bound: message.len(),
        };
    }
    why.push_str(message);
    Refusal::Invalid { what, why }
}

/// Raises the soft limit on open descriptors to the hard one: each connection and watched program
/// holds descriptors, and a service's soft limit stays 1,024 for `select(2)`'s sake while its hard
/// limit is 524,288 (systemd-system.conf(5), `DefaultLimitNOFILE=`), which a program that needs
/// more raises its soft limit to. The macOS node's; a Linux node raises its own at its start.
#[cfg(target_os = "macos")]
fn raise_descriptor_limit() -> Result<(), Refusal> {
    let limit = rustix::process::getrlimit(Resource::Nofile);
    if limit.current < limit.maximum {
        rustix::process::setrlimit(
            Resource::Nofile,
            rustix::process::Rlimit {
                current: limit.maximum,
                maximum: limit.maximum,
            },
        )
        .map_err(|e| Refusal::os("raising the descriptor limit", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(more: &[&str]) -> Vec<String> {
        ["--socket", "/s", "--state", "/d"]
            .iter()
            .chain(more)
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// Parked procs have no off switch (decision 8, parked procs; Ada's 2026-11-06 rule, once
    /// bench/competitive/probes/parked_procs_ab.md decided them on; AC-T7.1.3). Do: parse `--parked-procs` with `on`
    /// and with `off`. Expect: each refused as an unknown argument, naming it.
    #[test]
    fn parked_procs_have_no_flag() {
        for value in ["on", "off"] {
            let why = parse(&args(&["--parked-procs", value])).err().unwrap();
            assert!(
                why.starts_with("unknown argument \"--parked-procs\""),
                "{value}: {why}"
            );
        }
    }

    /// HEX-QUIC-24 (docs/design/hex-quic.md Q9), as parsed: `--quic` asks for a listener only with the enrollment it
    /// presents, and the enrollment's flags only with `--quic`; `--staged` is a Linux node's. Do: parse each half of the
    /// pair, a bad address, both, and `--staged`. Expect: each half refused naming what it lacks, the address refused,
    /// both taken, and `--staged` taken on Linux and refused elsewhere.
    #[test]
    fn hex_quic_24_quic_is_asked_with_its_enrollment_or_not_at_all() {
        let why = parse(&args(&["--quic", "127.0.0.1:0"])).err().unwrap();
        assert!(why.starts_with("--quic needs --credentials"), "{why}");
        let why = parse(&args(&["--credentials", "/c"])).err().unwrap();
        assert!(
            why.starts_with("--credentials and --staged are --quic's"),
            "{why}"
        );
        let why = parse(&args(&["--quic", "node:1", "--credentials", "/c"]))
            .err()
            .unwrap();
        assert!(why.starts_with("--quic needs an address and port"), "{why}");
        let both = parse(&args(&["--quic", "127.0.0.1:0", "--credentials", "/c"])).unwrap();
        let quic = both.quic.unwrap();
        assert_eq!(quic.bind, "127.0.0.1:0".parse().unwrap());
        assert_eq!(quic.credentials, PathBuf::from("/c"));
        assert!(parse(&args(&[])).unwrap().quic.is_none());
        let staged = parse(&args(&[
            "--quic",
            "127.0.0.1:0",
            "--credentials",
            "/c",
            "--staged",
            "/s",
        ]));
        if cfg!(target_os = "linux") {
            assert_eq!(
                staged.unwrap().quic.unwrap().staged,
                Some(PathBuf::from("/s"))
            );
        } else {
            assert!(
                staged
                    .err()
                    .unwrap()
                    .starts_with("--staged names where a Linux node stages")
            );
        }
    }

    /// HEX-QUIC-31 (docs/design/hex-quic.md Q12): `hex node links` says each link on a line of its own, the suite and
    /// group by rustls's names, a value rustls gives no name by its number, and `none` for a handshake that named no
    /// group; it takes `--socket <path>` alone. Do: say a link under AES-256-GCM and SecP384r1MLKEM1024, one under a
    /// suite and a group rustls doesn't name, and one with no group; give the verb no socket and another flag.
    /// Expect: the three lines; each argument list refused as a usage (2) before anything is reached.
    #[test]
    fn hex_quic_31_hex_node_links_says_each_link_as_a_line() {
        let link = |suite, group| hex_core::wire::Link {
            peer: "kiln.links.test".to_owned(),
            suite,
            group,
        };
        assert_eq!(
            link_line(&link(0x1302, Some(0x11ed))),
            "link kiln.links.test suite TLS13_AES_256_GCM_SHA384 group secp384r1MLKEM1024\n"
        );
        assert_eq!(
            link_line(&link(0x1306, Some(0x0a0a))),
            "link kiln.links.test suite CipherSuite(0x1306) group NamedGroup(0xa0a)\n"
        );
        assert_eq!(
            link_line(&link(0x1302, None)),
            "link kiln.links.test suite TLS13_AES_256_GCM_SHA384 group none\n"
        );
        let given = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(links(&given(&[])).err().unwrap().0, 2);
        assert_eq!(links(&given(&["--socket"])).err().unwrap().0, 2);
        let why = links(&given(&["--state", "/s"])).err().unwrap();
        assert_eq!(why.0, 2);
        assert!(
            why.1.starts_with("unknown argument \"--state\""),
            "{}",
            why.1
        );
    }

    #[cfg(target_os = "linux")]
    use hex_core::netns::Missing;

    /// HEX-NETNS-13: a node whose pool was refused says so on stderr, naming what it lacks; one whose
    /// pool wasn't says nothing. Do: render both. Expect: the line naming CAP_SYS_ADMIN, and none.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_refused_pool_is_said_by_name() {
        let refused = Counts {
            refused: Missing::of(false, true),
            ..Counts::default()
        };
        assert_eq!(
            refused_pool(&refused).unwrap(),
            "hex node: network-namespace pool refused: this node lacks CAP_SYS_ADMIN, which making \
             one ahead takes; each create makes its container's on its path (counted as \
             refused_on_path)\n"
        );
        assert_eq!(refused_pool(&Counts::default()), None);
    }
}
