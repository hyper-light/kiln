//! The node agent's local socket over a live agent (decision 3 of docs/design/runtime-architecture.md;
//! AC-T7.1.1): `hex node` runs as a process of its own, and this test reaches it only through its
//! socket, with `hex-client`'s `Runtime`. "do X, expect Y" (§6):
//! - HEX-SOCK-9: connect, create a `Bundle` box, start it, exec `/usr/bin/true` with its stdout a pipe
//!   this test holds, read the output, and wait on the `Proc` for `Code(0)`, all over one socket;
//! - the hex-api conformance suite, every case, over the socket, on `oci` and on `process` boxes, and on
//!   `oci` boxes each with a network namespace of its own: as root they take the agent's pooled ones,
//!   rootless they make theirs on the create's path (decision 4; goal 4 plan step 4);
//! - HEX-GUEST-8 (native plan step 5, docs/design/shards-guest.md §3): the conformance suite, every
//!   case, on `oci` and `process` boxes, again through `hex guest relay` and `hex_client::guest`'s
//!   client, the path a macOS or Windows host takes to the node in its Shards guest, Unix sockets
//!   standing in for vsock;
//! - HEX-GUEST-18 (§4a): the conformance suite, every case, on `oci` boxes through
//!   `hex_runtime::guest::WithGuest`, the node's runtime on macOS and Windows, over `shards_api`'s
//!   simulator, whose guest bridges each port to `hex guest relay`: the real link, staging and spec
//!   rewrite, each box's root staged and its copy removed with it;
//! - HEX-GUEST-42 (native plan step 5's guest reached from the host's node through the seam, and step 8's
//!   guest stdio): the conformance suite, every case, on `oci` boxes through a host node's socket served
//!   over `Recorded<WithGuest<…>>`, as macOS's and Windows' nodes serve it, HEX-GUEST-18's simulated guest
//!   behind it: both stdio forms through the socket, the caller's own pipes copied by its client and the
//!   streams it asks for the guest's own connections;
//! - HEX-SOCK-10: a box whose bundle makes a network namespace of its own is counted, as root as one
//!   taken from the agent's pool, rootless as one made on its create's path, and the counts cross the
//!   socket (decision 4; goal 4 plan steps 2 and 4);
//! - HEX-SOCK-6, the disconnect rule: a caller that goes away while its exec'd programs run (its
//!   connection closes with no release, as when its process dies) has each program ended and reaped,
//!   and counted in its box, which inspect reports; nothing it started is left running;
//! - T-T7.1.2 (goal 5 extended step 6): kill the agent while a box of an owner holds a slot and another
//!   box's slot is idle, with a box of no owner beside them, and restart it: the idle slot is removed,
//!   said so on stderr, the live boxes are taken back, the owner's in its recorded slot, and once they're
//!   deleted nothing is left under the oci boxes' root;
//! - T-T7.1.2 (goal 5 plan step 2; goal 5 extended step 5): kill the agent at each point of a create
//!   (in a debug build, at its cgroup made and at its bundle rendered, where the agent armed by
//!   `hex node --crash-at` kills itself; its cgroup and its container's state directory made; its
//!   init cloned; inside its one record write; after it), its boxes' records durable and not, and
//!   restart it (a torn record's agent beside a configured Shards backend too): the box killed
//!   mid-create is cleaned up by what it left, the one made is taken back
//!   and deletes, and nothing of either is left; and a box whose own spec names the crash point, on an
//!   agent that armed none, is made whole, the agent living on. A case made to fail on purpose after the kill,
//!   its box's init alive at the exec FIFO: expect every process that was in its cgroups ended, the cgroups and
//!   its container's state removed, as the case unwinds (CLAUDE.md §2).
//!
//! Rootless where the caller isn't root, in a delegated cgroup v2 subtree, as oci_box.rs; where neither
//! is possible it records the skip and why (hex_testkit, which the gate prints) rather than pass
//! falsely.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as Std};
use std::sync::{Arc, Mutex};

use hex_api::conformance::{self, Act, Fixture};
use hex_api::{
    Bounds, BoxId, BoxOwner, BoxSpec, BoxState, Class, ClassSpec, Exit, Program, Refusal, Runtime,
    Stdio,
};
#[cfg(debug_assertions)]
use hex_api::{Network, OciSpec, Rootfs};
use hex_client::Client;
use hex_core::patience::Patience;
use hex_core::record::{BoxRecord, Phase, ProcId};
use hex_core::stream;
use hex_core::wire::{self, Answer, Call, Request, Streams};
use hex_node::OCI_BOXES;
use hex_testkit::waits::{CREATE, EMPTYING, EXEC, NETNS_MAKE, PROGRAM, START, until};

/// Each wait here takes its own fact's record (hex_testkit::waits; the lead's rule of 2026-11-08), or measures where no
/// recorded operation makes its fact. The pool's refill thread's make of a network namespace: `netns.make`.
static POOL_HOLDS: Patience = Patience::seeded("test.node_socket.pool_holds", NETNS_MAKE);
/// An exec's program in its box: made within the exec, before it answers, so the exec's record bounds it.
static RUNNING: Patience = Patience::seeded("test.node_socket.running", EXEC);
/// The agent's ending of a program whose caller went away, its kill, its reaping and its count in its box: a box's
/// emptying's work, whose record the server's own wait for the programs it ends takes (decision 9: `node.emptying`).
static ABANDONED: Patience = Patience::seeded("test.node_socket.abandoned", EMPTYING);
/// A create's answer, and what a create makes within itself: its hooks holding it, the agent waiting opening its FIFO.
static CREATING: Patience = Patience::seeded("test.node_socket.create", CREATE);
/// A start's startContainer hook holding it, made within the start.
static STARTING: Patience = Patience::seeded("test.node_socket.start", START);
/// The crash hook, a program this test runs itself: its run to its end, and its pipe opened on the way.
static HOOK: Patience = Patience::seeded("test.node_socket.hook", PROGRAM);
/// A line the agent wrote before it answered or ended, delivered by this test's own stderr reader: no recorded
/// operation makes the delivery, so measuring, for the fact alone, each answer recorded for the census to come.
static SAID: Patience = Patience::measuring("test.node_socket.said");
/// The agent's `READY=1` to its service manager once its socket listens: its start, its recovery and its bind, which
/// no recorded operation makes, so measuring, for the fact alone, as [`SAID`] is.
static READY: Patience = Patience::measuring("test.node_socket.ready");
/// Longest the test waits for the agent's answer to an exec over a raw connection: twice the exec answer's record.
const ANSWERED: std::time::Duration = EXEC.saturating_mul(2);

const TEST: &str = "the_seam_holds_its_contract_over_the_node_agents_socket";

#[test]
fn the_seam_holds_its_contract_over_the_node_agents_socket() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    match prepare_cgroup(&cache) {
        Ok(own) => exercise(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                hex_testkit::skip(
                    "the_seam_holds_its_contract_over_the_node_agents_socket",
                    "cgroup v2 delegation",
                    &format!(
                        "own cgroup isn't ours to subdivide ({direct}) and no systemd --user for a private scope"
                    ),
                )
                .expect("recording the skip");
                return;
            }
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(std::env::current_exe().expect("test binary path"))
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("run the test in a delegated scope");
            assert!(status.success(), "the delegated re-run failed");
        }
        Err(direct) => {
            hex_testkit::skip(
                "the_seam_holds_its_contract_over_the_node_agents_socket",
                "cgroup v2 delegation",
                &format!("none in the delegated scope either ({direct}); run on a cgroup-v2 host"),
            )
            .expect("recording the skip");
        }
    }
}

/// HEX-INST-3 (native plan step 6): on a machine where Hex's installer ran its one admin step, the
/// node it installed (the user's `hex-node@<user>` service) serves every Linux class with no further step: the
/// conformance suite passes over its socket, `HEX_INSTALLED_SOCKET`, on `oci` and `process` boxes.
/// Run by the installer's lab (bench/install/linux_lab.sh); elsewhere it prints SKIP.
#[test]
fn an_installed_node_serves_every_class() {
    let Some(socket) = std::env::var_os("HEX_INSTALLED_SOCKET") else {
        eprintln!("SKIP: HEX_INSTALLED_SOCKET names no installed node's socket");
        return;
    };
    let exe = env!("CARGO_BIN_EXE_hex");
    let client = Client::connect(Path::new(&socket)).expect("connect to the installed node");
    assert_eq!(client.classes(), [Class::Process, Class::Oci]);
    let lab = Lab::new(&cache_dir(), exe);
    example(&lab, &client).expect("an oci box on the installed node");
    let failures = conformance::run(&client, &OciBoxes(&lab, false));
    assert!(failures.is_empty(), "oci boxes: {failures:#?}");
    let failures = conformance::run(&client, &ProcessBoxes(exe));
    assert!(failures.is_empty(), "process boxes: {failures:#?}");
    // What a box whose spec names no RLIMIT_NOFILE gets from the installed node: recorded for the lab,
    // since the unit's LimitNOFILE= reaches it (docs/research/runtime/nofile-limit.md).
    for (class, spec) in [
        ("oci", ClassSpec::Bundle(lab.bundle(&Lab::act(&Act::Wait)))),
        ("process", ClassSpec::Process),
    ] {
        eprintln!(
            "{class} box, no RLIMIT_NOFILE in its spec: soft:hard {}",
            nofile_seen(&client, spec)
        );
    }
    assert!(client.list().unwrap().is_empty(), "a box is left");
}

/// HEX-INST-6 (native plan step 6, Ada 2026-11-06): a box whose spec names no RLIMIT_NOFILE, an oci
/// box from a bundle that names none and a process box, gets Docker 29's 1024:524288, capped at the
/// node's hard limit, not the node's own (which the installed node raises for the files it holds).
fn nofile_by_default(lab: &Lab, client: &Client) {
    let hard = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum;
    let want = hex_core::limits::default_nofile(hard);
    let want = format!("{}:{}", want.soft, want.hard);
    for (class, spec) in [
        ("oci", ClassSpec::Bundle(lab.bundle(&Lab::act(&Act::Wait)))),
        ("process", ClassSpec::Process),
    ] {
        assert_eq!(nofile_seen(client, spec), want, "{class} box");
    }
}

/// HEX-INST-6 (native plan step 6): an oci box whose spec names RLIMIT_NOFILE keeps the spec's, not the
/// default: here one less than the default's soft limit, soft, and the default's soft limit, hard
/// (1023:1024), below the default in both and a lowering a rootless init may make under any node.
fn nofile_named(lab: &Lab, client: &Client) {
    let hard = hex_core::limits::NOFILE_SOFT;
    let soft = hard - 1;
    let bundle = lab.bundle(&Lab::act(&Act::Wait));
    let config = bundle.join("config.json");
    let text = std::fs::read_to_string(&config).unwrap();
    let named = text.replacen(
        r#""noNewPrivileges": true"#,
        &format!(
            r#""noNewPrivileges": true, "rlimits": [{{"type": "RLIMIT_NOFILE", "soft": {soft}, "hard": {hard}}}]"#
        ),
        1,
    );
    assert_ne!(
        named, text,
        "the bundle's process has no noNewPrivileges to name the limit beside"
    );
    std::fs::write(&config, named).unwrap();
    assert_eq!(
        nofile_seen(client, ClassSpec::Bundle(bundle)),
        format!("{soft}:{hard}"),
        "oci box"
    );
}

/// The soft and hard descriptor limits a program sees in a box made from `class`, as `soft:hard`.
fn nofile_seen(client: &Client, class: ClassSpec) -> String {
    let id = BoxId::new("nofile").unwrap();
    client
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class,
            owner: None,
        })
        .unwrap();
    if client.inspect(&id).unwrap().state == BoxState::Created {
        client.start(&id).unwrap();
    }
    let (mut out, w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &id,
            Program::of(&["/bin/sh", "-c", "echo $(ulimit -Sn):$(ulimit -Hn)"]),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let mut said = String::new();
    out.read_to_string(&mut said).unwrap();
    assert_eq!(
        proc.wait_for(conformance::PATIENCE).unwrap(),
        Some(Exit::Code(0))
    );
    client.delete(&id).unwrap();
    said.trim().to_owned()
}

/// HEX-INST-8 (native plan step 6): `hex node` started with `$NOTIFY_SOCKET` (systemd's `Type=notify`) says
/// `READY=1` there once its socket listens, so the installed unit is up exactly when it serves, within the
/// start bound systemd gives it (sd_notify(3); systemd.service(5) `TimeoutStartSec=`), and the installer
/// needs no wait of its own. A client connects right after the datagram arrives.
fn ready_is_told_to_the_service_manager(own: &Path, cache: &Path, exe: &str) {
    let dir = tempfile::Builder::new()
        .prefix("notify-")
        .tempdir_in(cache)
        .unwrap();
    let notify = dir.path().join("notify.sock");
    let manager = std::os::unix::net::UnixDatagram::bind(&notify).unwrap();
    manager.set_nonblocking(true).unwrap();
    let socket = dir.path().join("node.sock");
    let mut node = Command::new(exe)
        .arg("node")
        .arg("--socket")
        .arg(&socket)
        .arg("--state")
        .arg(dir.path().join("state"))
        .arg("--box-root")
        .arg(own.join("hex-notify"))
        .env("NOTIFY_SOCKET", &notify)
        .stdout(Std::null())
        .spawn()
        .expect("start hex node");
    let mut said = [0u8; 64];
    let mut n = 0;
    wait_for(&READY, "READY=1 on $NOTIFY_SOCKET", || {
        match manager.recv(&mut said) {
            Ok(got) => {
                n = got;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("reading $NOTIFY_SOCKET: {e}"),
        }
    });
    let said = String::from_utf8_lossy(&said[..n]).into_owned();
    assert!(
        said.lines().any(|l| l == "READY=1"),
        "the node told {said:?}"
    );
    Client::connect(&socket).expect("connect once the node has said it's ready");
    node.kill().unwrap();
    node.wait().unwrap();
}

/// A `hex node` process serving its socket, killed when dropped.
struct Agent {
    child: Child,
    socket: PathBuf,
    box_root: PathBuf,
    _dir: Option<tempfile::TempDir>,
    /// The agent's stderr, drained by a thread as it is written so the pipe never fills: what it says
    /// about the boxes it recovered, which a crash case reads to see why one was removed.
    errors: Arc<Mutex<String>>,
}

impl Agent {
    fn start(exe: &str, cache: &Path, box_root: PathBuf) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("agent-")
            .tempdir_in(cache)
            .unwrap();
        let mut agent = Self::over(exe, dir.path(), box_root, None);
        agent._dir = Some(dir);
        agent
    }

    /// A `hex node` whose records are in `dir`'s `state` and whose socket is `dir`'s `node.sock`, its
    /// boxes under `box_root`, with `runtime` its `XDG_RUNTIME_DIR` when given; `dir` is the caller's.
    fn over(exe: &str, dir: &Path, box_root: PathBuf, runtime: Option<&Path>) -> Self {
        Self::armed(exe, dir, box_root, runtime, None, &[])
    }

    /// As [`Agent::over`], armed at a debug build's crash point (`hex node --crash-at`) when one's given, with
    /// `env` set for it too.
    fn armed(
        exe: &str,
        dir: &Path,
        box_root: PathBuf,
        runtime: Option<&Path>,
        crash_at: Option<&str>,
        env: &[(&str, std::ffi::OsString)],
    ) -> Self {
        let socket = dir.join("node.sock");
        let mut command = Command::new(exe);
        command
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(dir.join("state"))
            .arg("--box-root")
            .arg(&box_root)
            .stdout(Std::piped())
            .stderr(Std::piped());
        if let Some(point) = crash_at {
            command.arg("--crash-at").arg(point);
        }
        if let Some(runtime) = runtime {
            command.env("XDG_RUNTIME_DIR", runtime);
        }
        command.envs(env.iter().map(|(name, value)| (name, value)));
        let mut child = command.spawn().expect("start hex node");
        // Drain stderr as it comes so the pipe never fills and blocks the agent, and keep what it says
        // for a crash case to read (the boxes it recovered, and why each was removed).
        let errors = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&errors);
        let drain = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                sink.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
        // The fact a client needs: the agent says it's ready once its socket listens.
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        if line.trim() != format!("ready {}", socket.display()) {
            // An agent that won't start says why on stderr, read whole once it has ended.
            let ended = child.wait().unwrap();
            drain.join().unwrap();
            panic!(
                "hex node said {line:?} and ended {ended}: {}",
                errors.lock().unwrap()
            );
        }
        Self {
            child,
            socket,
            box_root,
            _dir: None,
            errors,
        }
    }

    /// What the agent has said on stderr so far: the boxes recovery removed, and why each
    /// ([`Agent::errors`]).
    fn errors(&self) -> String {
        self.errors.lock().unwrap().clone()
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket).expect("connect to hex node")
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).unwrap()
    }

    /// Waits for the agent to end, and how it ended.
    #[cfg(debug_assertions)]
    fn ended(&mut self) -> std::process::ExitStatus {
        self.child.wait().unwrap()
    }

    /// Whether the agent still runs.
    #[cfg(debug_assertions)]
    fn runs(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where one run keeps its bundles: a rootfs of the host's `/usr`, and the `hex` binary at `/hex`.
struct Lab {
    exe: String,
    rootfs: PathBuf,
    bundles: tempfile::TempDir,
    rootless: bool,
    uid: u32,
    gid: u32,
}

impl Lab {
    fn new(cache: &Path, exe: &str) -> Self {
        let (uid, gid) = (host_id("-u"), host_id("-g"));
        let bundles = tempfile::Builder::new()
            .prefix("socklab-")
            .tempdir_in(cache)
            .expect("bundles dir");
        let rootfs = bundles.path().join("rootfs");
        build_rootfs(&rootfs);
        Self {
            exe: exe.to_owned(),
            rootfs,
            bundles,
            rootless: uid != 0,
            uid,
            gid,
        }
    }

    fn bundle(&self, args: &[String]) -> PathBuf {
        self.bundle_with(args, false)
    }

    /// A bundle whose container runs `args`, with a network namespace of its own when `network`.
    fn bundle_with(&self, args: &[String], network: bool) -> PathBuf {
        self.bundle_hooked(args, network, "")
    }

    /// As [`Lab::bundle_with`], its config ending with `hooks`: a `"hooks"` member after a comma, or
    /// nothing.
    fn bundle_hooked(&self, args: &[String], network: bool, hooks: &str) -> PathBuf {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(self.bundles.path())
            .expect("bundle dir")
            .keep();
        let (mut namespaces, mappings) = if self.rootless {
            (
                r#"{"type":"user"},{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#
                    .to_owned(),
                format!(
                    r#","uidMappings":[{{"containerID":0,"hostID":{},"size":1}}],"gidMappings":[{{"containerID":0,"hostID":{},"size":1}}]"#,
                    self.uid, self.gid
                ),
            )
        } else {
            (
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#.to_owned(),
                String::new(),
            )
        };
        if network {
            namespaces.push_str(r#",{"type":"network"}"#);
        }
        assert!(args.iter().all(|a| a.is_ascii() && !a.contains('\\')));
        let config = format!(
            r#"{{
  "ociVersion": "1.3.0",
  "root": {{"path": "{rootfs}", "readonly": false}},
  "process": {{
    "cwd": "/",
    "args": {args:?},
    "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "user": {{"uid": 0, "gid": 0}},
    "capabilities": {capabilities},
    "noNewPrivileges": true
  }},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}},
    {{"destination": "/hex", "type": "bind", "source": "{exe}", "options": ["bind","ro"]}}
  ],
  "linux": {{"namespaces": [{namespaces}]{mappings}}}{hooks}
}}"#,
            rootfs = self.rootfs.display(),
            exe = self.exe,
            capabilities = capabilities(),
        );
        std::fs::write(dir.join("config.json"), config).expect("config.json");
        dir
    }

    /// An `OciSpec` the node renders a bundle for: the lab's rootfs, the host's network, an init that
    /// waits.
    #[cfg(debug_assertions)]
    fn spec(&self) -> OciSpec {
        OciSpec {
            rootfs: Rootfs::Dir(self.rootfs.clone()),
            init: program(&Self::act(&Act::Wait)),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Host,
            hosts: Vec::new(),
        }
    }

    /// The `hex selftest-box` program for `act`, as a container sees the binary (`/hex`).
    fn act(act: &Act) -> Vec<String> {
        let mut args = vec!["/hex".to_owned(), "selftest-box".to_owned()];
        args.extend(act_args(act));
        args
    }
}

fn act_args(act: &Act) -> Vec<String> {
    match act {
        Act::Say(words) => vec!["say".to_owned(), (*words).to_owned()],
        Act::Fail(code) => vec!["fail".to_owned(), code.to_string()],
        Act::Wait => vec!["wait".to_owned()],
        Act::Hog(bytes) => vec!["hog".to_owned(), bytes.to_string()],
        Act::Spawn(n) => vec!["spawn".to_owned(), n.to_string()],
        Act::Echo => vec!["echo".to_owned()],
    }
}

fn program(args: &[String]) -> Program {
    Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// `oci` boxes from bundles whose init and programs are `hex selftest-box` acts, each with a network
/// namespace of its own when the second field is set: one taken from the agent's pool as root, made on
/// the create's path rootless (decision 4).
struct OciBoxes<'a>(&'a Lab, bool);

/// The bound a program crosses the node's socket under: the tighter of the wire's (the client refuses past it)
/// and Linux's (the node refuses past it).
fn over_the_socket() -> hex_api::ExecLimit {
    let linux = hex_runtime::linux::process::exec_limit();
    if hex_client::WIRE_EXEC_LIMIT.block < linux.block {
        hex_client::WIRE_EXEC_LIMIT
    } else {
        linux
    }
}

impl Fixture for OciBoxes<'_> {
    fn exec_limit(&self) -> Option<hex_api::ExecLimit> {
        Some(over_the_socket())
    }
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds,
            class: ClassSpec::Bundle(self.0.bundle_with(&Lab::act(&Act::Wait), self.1)),
            owner: None,
        }
    }

    fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(self.0.bundle_with(&Lab::act(act), self.1)),
            owner: None,
        }
    }

    fn program(&self, act: &Act) -> Program {
        program(&Lab::act(act))
    }
}

/// `process` boxes, whose programs are the host's `hex selftest-box` acts.
struct ProcessBoxes<'a>(&'a str);

impl Fixture for ProcessBoxes<'_> {
    fn exec_limit(&self) -> Option<hex_api::ExecLimit> {
        Some(over_the_socket())
    }
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds,
            class: ClassSpec::Process,
            owner: None,
        }
    }

    fn init(&self, id: BoxId, _act: &Act) -> BoxSpec {
        self.spec(id, Bounds::default())
    }

    fn program(&self, act: &Act) -> Program {
        let mut args = vec![self.0.to_owned(), "selftest-box".to_owned()];
        args.extend(act_args(act));
        program(&args)
    }
}

fn exercise(own: &Path, cache: &Path, exe: &str) {
    ready_is_told_to_the_service_manager(own, cache, exe);
    let lab = Lab::new(cache, exe);
    let agent = Agent::start(exe, cache, own.join("hex-sock"));
    let client = agent.client();
    assert_eq!(client.classes(), [Class::Process, Class::Oci]);
    if let Err(e) = example(&lab, &client) {
        hex_testkit::skip(
            "the_seam_holds_its_contract_over_the_node_agents_socket",
            "unprivileged user namespaces",
            &format!("rootless OCI unavailable here ({e})"),
        )
        .expect("recording the skip");
        return;
    }
    namespaces_counted(&lab, &client);
    the_agent_parks_procs_as_root_and_none_rootless(&lab, own, cache, exe);
    let failures = hex_api::differential::run(&client, &OciBoxes(&lab, false));
    assert!(
        failures.is_empty(),
        "oci boxes over the socket: {failures:#?}"
    );
    conformance_with_namespaces(&lab, &client);
    let failures = hex_api::differential::run(&client, &ProcessBoxes(exe));
    assert!(
        failures.is_empty(),
        "process boxes over the socket: {failures:#?}"
    );
    nofile_by_default(&lab, &client);
    nofile_named(&lab, &client);
    through_the_guest_relay(&lab, &agent, exe);
    through_a_simulated_guest(&lab, &agent, exe);
    through_a_host_nodes_socket(&lab, &agent, exe);
    disconnect(&lab, &agent, &client);
    a_crash_with_idle_cgroups_and_live_boxes_leaves_nothing(&lab, own, exe);
    a_sigterm_says_the_parked_procs_and_leaves_the_boxes_as_a_sigkill_does(&lab, own, exe);
    crash_at_each_point_of_a_create(&lab, own, exe);
    assert!(client.list().unwrap().is_empty(), "a box is left");
}

/// Parked procs are the socket agent's, with no switch to turn them off (decision 8, parked procs; the lead,
/// 2026-11-06; Ada's 2026-11-06 rule removed `--parked-procs` once bench/competitive/probes/parked_procs_ab.md decided
/// them on; AC-T7.1.3). Do: start an agent. Expect: it says on stderr, from its node's own state, that it parks procs
/// (root) or that a rootless node parks none.
fn the_agent_parks_procs_as_root_and_none_rootless(lab: &Lab, own: &Path, cache: &Path, exe: &str) {
    let said = if lab.rootless {
        "hex node: parked procs: off: a rootless node parks none"
    } else {
        "hex node: parked procs: on"
    };
    let agent = Agent::start(exe, cache, own.join("hex-sock-pp"));
    wait_for(&SAID, &format!("hex node saying {said:?}"), || {
        agent.errors().lines().any(|l| l.starts_with(said))
    });
}

/// The guest's ports as Unix sockets in the relay's directory.
struct GuestPorts(PathBuf);

impl hex_client::guest::Link for GuestPorts {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        let name = match port {
            stream::CONTROL_PORT => "control.sock",
            stream::STDIO_PORT => "stdio.sock",
            stream::files::FILES_PORT => "files.sock",
            other => panic!("dialed port {other:#x}"),
        };
        hex_client::guest::own_duplex(
            std::os::unix::net::UnixStream::connect(self.0.join(name)).unwrap(),
        )
    }
}

/// HEX-GUEST-8: start `hex guest relay` to the agent's socket, connect through it as a host reaches
/// the node in its guest, and run the conformance suite on `oci` and `process` boxes: expect every case
/// to pass, as over the socket itself. The relay stages under the test's own folder: its default,
/// `/run/hex/staged`, is the guest's, which a rootless test can't make.
fn through_the_guest_relay(lab: &Lab, agent: &Agent, exe: &str) {
    let dir = tempfile::Builder::new()
        .prefix("relay-")
        .tempdir_in(agent.socket.parent().unwrap())
        .unwrap();
    let mut relay = Command::new(exe)
        .args(["guest", "relay", "--node"])
        .arg(&agent.socket)
        .arg("--unix")
        .arg(dir.path())
        .arg("--staged")
        .arg(dir.path().join("staged"))
        .stdout(Std::piped())
        .spawn()
        .map(Reaped)
        .expect("start hex guest relay");
    let mut line = String::new();
    BufReader::new(relay.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready", "hex guest relay said {line:?}");
    let guest = hex_client::guest::Client::connect(
        hex_client::guest::own_link(GuestPorts(dir.path().to_owned())).unwrap(),
    )
    .expect("connect through the guest relay");
    assert_eq!(guest.classes(), [Class::Process, Class::Oci]);
    let failures = conformance::run(&guest, &OciBoxes(lab, false));
    assert!(
        failures.is_empty(),
        "oci boxes through the guest relay: {failures:#?}"
    );
    let failures = conformance::run(&guest, &ProcessBoxes(exe));
    assert!(
        failures.is_empty(),
        "process boxes through the guest relay: {failures:#?}"
    );
    drop(guest);
    drop(relay);
}

/// A child ended and reaped when dropped, so a failing assertion leaves no relay running (one held a gate's
/// output pipe open for 80 minutes after its test failed, 2026-11-07).
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Copies `from` into `to` until `from` ends, then ends `to`'s sending half: one direction of a bridged
/// connection, the guest's vsock port to the relay's socket or back.
fn pump(mut from: Box<dyn shards_api::Stream>, to: std::os::unix::net::UnixStream) {
    let mut to_w = to.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut from, &mut to_w);
        let _ = to_w.shutdown(std::net::Shutdown::Write);
    });
}

fn pump_back(mut from: std::os::unix::net::UnixStream, mut to: Box<dyn shards_api::Stream>) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut from, &mut to);
        let _ = to.shutdown(std::net::Shutdown::Write);
    });
}

/// HEX-GUEST-18's simulated Shards guest: `hex guest relay` started to the agent's socket, staging under a folder
/// of the test's, and a guest whose PID 1 bridges each of the channel's ports to it, one share `/` seen at `/` (a
/// simulated guest is the host). The test's folder and the relay's staging folder, the relay (reaped when dropped),
/// the guest's runtime, and how many times it booted and how many stages it took.
struct SimulatedGuest {
    dir: tempfile::TempDir,
    staged: PathBuf,
    relay: Reaped,
    runtime: hex_runtime::guest::GuestRuntime,
    boots: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stages: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

fn simulated_guest(agent: &Agent, exe: &str) -> SimulatedGuest {
    use hex_runtime::guest::shares::Share;
    use hex_runtime::guest::{GuestConfig, GuestRuntime};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::Builder::new()
        .prefix("guest-")
        .tempdir_in(agent.socket.parent().unwrap())
        .unwrap();
    let staged = dir.path().join("staged");
    let mut relay = Command::new(exe)
        .args(["guest", "relay", "--node"])
        .arg(&agent.socket)
        .arg("--unix")
        .arg(dir.path())
        .arg("--staged")
        .arg(&staged)
        .stdout(Std::piped())
        .spawn()
        .map(Reaped)
        .expect("start hex guest relay");
    let mut line = String::new();
    BufReader::new(relay.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "ready", "hex guest relay said {line:?}");
    let ports = dir.path().to_owned();
    let boots = Arc::new(AtomicUsize::new(0));
    let booted = Arc::clone(&boots);
    // Connections to the staging port: a stage each.
    let stages = Arc::new(AtomicUsize::new(0));
    let staging = Arc::clone(&stages);
    let guests = shards_api::sim::SimGuests::new(move |host| {
        booted.fetch_add(1, Ordering::SeqCst);
        for port in [
            stream::CONTROL_PORT,
            stream::STDIO_PORT,
            stream::files::FILES_PORT,
        ] {
            host.listen(port);
        }
        while let Some((port, conn)) = host.accept() {
            let name = match port {
                stream::CONTROL_PORT => "control.sock",
                stream::STDIO_PORT => "stdio.sock",
                _ => {
                    staging.fetch_add(1, Ordering::SeqCst);
                    "files.sock"
                }
            };
            let relay = std::os::unix::net::UnixStream::connect(ports.join(name)).unwrap();
            pump(conn.try_clone().unwrap(), relay.try_clone().unwrap());
            pump_back(relay, conn);
        }
        shards_api::End::Stopped
    });
    let config = GuestConfig {
        // A simulated guest boots nothing: no binary, kernel or initramfs is read.
        binary: PathBuf::from("/nonexistent/shards"),
        kernel: PathBuf::from("/nonexistent/kernel"),
        initrd: PathBuf::from("/nonexistent/initrd"),
        // Shards' least guest: one vCPU and its architecture's least memory (`shards_api::check`).
        vcpus: 1,
        gpu: false,
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        shares: vec![Share {
            dir: PathBuf::from("/"),
            read_only: false,
            at: "/".to_owned(),
        }],
        staged: staged.to_str().unwrap().to_owned(),
    };
    SimulatedGuest {
        dir,
        staged,
        relay,
        runtime: GuestRuntime::new(Arc::new(guests), config),
        boots,
        stages,
    }
}

/// HEX-GUEST-18: start `hex guest relay` (staging under a folder of the test's) to the agent's socket,
/// boot a simulated Shards guest whose PID 1 bridges each of the channel's ports to it, and run the
/// conformance suite on `oci` boxes through `WithGuest` over that guest, one share `/` seen at `/` (a
/// simulated guest is the host): expect every case to pass, the guest booted once, and no staged
/// copy left once the suite's boxes are deleted.
fn through_a_simulated_guest(lab: &Lab, agent: &Agent, exe: &str) {
    use hex_runtime::guest::WithGuest;
    use std::sync::atomic::Ordering;
    let SimulatedGuest {
        dir: _dir,
        staged,
        relay,
        runtime,
        boots,
        stages,
    } = simulated_guest(agent, exe);
    // Routed by class as macOS's node is, over a host runtime of `process` boxes alone, as macOS's is: a
    // host that ran `oci` itself would leave an unrouted `oci` box ambiguous (`WithGuest::route_for`).
    let node = WithGuest::new(
        hex_api::sim::SimRuntime::for_backend(Class::Process),
        runtime,
    )
    .unwrap();
    assert_eq!(node.classes(), [Class::Process, Class::Oci]);
    let failures = conformance::run(&node, &OciBoxes(lab, false));
    assert!(
        failures.is_empty(),
        "oci boxes through a simulated Shards guest: {failures:#?}"
    );
    assert_eq!(
        boots.load(Ordering::SeqCst),
        1,
        "the guest booted more than once"
    );
    assert!(node.list().unwrap().is_empty(), "a box is left");
    // Every box's copy goes with its box; the layer store (§4a) stays, a cache for the next create.
    let left: Vec<_> = std::fs::read_dir(&staged)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != stream::files::STORE)
        .collect();
    assert!(left.is_empty(), "staged copies left: {left:?}");
    // HEX-GUEST-18 (CLAUDE.md §3; HEX-GUEST-19's HEX-RT-1 in a real guest): a second create of an id the
    // guest's node holds is refused as `Exists` before anything is staged: one stage for the two creates.
    let fixture = OciBoxes(lab, false);
    let id = BoxId::new("twice").unwrap();
    let before = stages.load(Ordering::SeqCst);
    node.create(fixture.spec(id.clone(), Bounds::default()))
        .unwrap();
    assert_eq!(
        node.create(fixture.spec(id.clone(), Bounds::default())),
        Err(Refusal::Exists(id.clone()))
    );
    assert_eq!(
        stages.load(Ordering::SeqCst) - before,
        1,
        "the refused create staged its root"
    );
    node.delete(&id).unwrap();
    drop(node);
    drop(relay);
}

/// The host's own boxes beside a node's guest: the simulator's, with no processes to take back after a crash,
/// which HEX-GUEST-42 doesn't ask of a node.
struct HostBoxes(hex_api::sim::SimRuntime);

impl Runtime for HostBoxes {
    fn classes(&self) -> Vec<Class> {
        self.0.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<hex_api::BoxInfo, Refusal> {
        self.0.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.start(id)
    }
    fn exec(
        &self,
        id: &BoxId,
        program: Program,
        stdio: Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.0.exec(id, program, stdio)
    }
    fn run(
        &self,
        spec: BoxSpec,
        program: Program,
        stdio: Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.0.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), Refusal> {
        self.0.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<hex_api::BoxInfo, Refusal> {
        self.0.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<hex_api::Usage, Refusal> {
        self.0.usage(id)
    }
    fn list(&self) -> Result<Vec<hex_api::BoxInfo>, Refusal> {
        self.0.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.delete(id)
    }
}

impl hex_node::recorded::Recover for HostBoxes {
    // A simulator keeps no native metadata: it has no names to join to the retained cohort.
    fn preflight_retained(
        &self,
        _records: &hex_runtime::devices::Records,
        _pools: &[hex_core::record::PoolRecord],
    ) -> Result<(), Refusal> {
        Ok(())
    }
    fn trace(&self, _id: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn adopt(&self, _spec: &BoxSpec, _trace: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn end(&self, _trace: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
}

/// HEX-GUEST-42 (native plan step 5: the guest reached from the host's node through the seam; step 8: guest
/// stdio through the node's socket): serve `Recorded<WithGuest<…>>` over HEX-GUEST-18's simulated guest on a
/// host node's socket, as macOS's and Windows' `hex node` serve their guests, and run the conformance suite on
/// `oci` boxes through `hex-client` over that socket: expect every case to pass, both stdio forms among them
/// (HEX-RT-2 and -9 the caller's own pipes, copied by its client; HEX-RT-17 the streams it asks for, the guest's
/// own connections), the guest booted once, and no box left.
fn through_a_host_nodes_socket(lab: &Lab, agent: &Agent, exe: &str) {
    use hex_runtime::guest::WithGuest;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    let SimulatedGuest {
        dir,
        staged: _staged,
        relay,
        runtime,
        boots,
        stages: _stages,
    } = simulated_guest(agent, exe);
    // The host's own `process` boxes alone, as `through_a_simulated_guest`'s.
    let node = WithGuest::new(
        HostBoxes(hex_api::sim::SimRuntime::for_backend(Class::Process)),
        runtime,
    )
    .unwrap();
    let node = hex_node::recorded::Recorded::open(node, &dir.path().join("state")).unwrap();
    let socket = dir.path().join("host.sock");
    // The host node's socket bounds, as a Linux node takes them from its host (goal 5's sizes).
    let platform = hex_node::sizes::Sizes::now().unwrap().platform();
    let server = hex_node::serve::Server::bind(
        &socket,
        Arc::new(node),
        hex_node::serve::limits(&platform),
        platform.threads,
    )
    .unwrap();
    let client = Client::connect(&socket).unwrap();
    assert_eq!(client.classes(), [Class::Process, Class::Oci]);
    let failures = conformance::run(&client, &OciBoxes(lab, false));
    assert!(
        failures.is_empty(),
        "oci boxes through a host node's socket over a simulated Shards guest: {failures:#?}"
    );
    assert_eq!(
        boots.load(Ordering::SeqCst),
        1,
        "the guest booted more than once"
    );
    assert!(client.list().unwrap().is_empty(), "a box is left");
    drop(client);
    drop(server);
    drop(relay);
}

/// HEX-SOCK-9, the plan's example: a `Bundle` box made, started, and `/usr/bin/true` exec'd in it
/// with its stdout a pipe this test holds, its output read and its `Code(0)` waited for, all over one
/// socket. `Err` when no container can be made here (rootless needs unprivileged user namespaces).
fn example(lab: &Lab, client: &Client) -> Result<(), String> {
    let id = BoxId::new("example").unwrap();
    let made = client.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(lab.bundle(&Lab::act(&Act::Wait))),
        owner: None,
    });
    let info = made.map_err(|e| e.to_string())?;
    assert_eq!((info.class, info.state), (Class::Oci, BoxState::Created));
    client.start(&id).unwrap();
    assert_eq!(client.inspect(&id).unwrap().state, BoxState::Running);
    let (mut out, w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &id,
            Program::of(&["/usr/bin/true"]),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let mut said = String::new();
    out.read_to_string(&mut said).unwrap();
    assert_eq!(said, "", "true says nothing");
    assert_eq!(
        proc.wait_for(conformance::PATIENCE).unwrap(),
        Some(Exit::Code(0))
    );
    let (mut out, w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &id,
            program(&Lab::act(&Act::Say("from the socket"))),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let mut said = String::new();
    out.read_to_string(&mut said).unwrap();
    assert_eq!(said, "from the socket\n");
    assert_eq!(
        proc.wait_for(conformance::PATIENCE).unwrap(),
        Some(Exit::Code(0))
    );
    client.delete(&id).unwrap();
    Ok(())
}

/// HEX-SOCK-10 over the live agent (decision 4; goal 4 plan steps 2 and 4): the agent counts each
/// box's network namespace, and the counts cross its socket. A box with a network namespace of its
/// own takes one from a root agent's pool, once the pool holds one, and a rootless agent's makes its
/// own on its create's path; boxes without one (the example's, and every other here) count none.
fn namespaces_counted(lab: &Lab, client: &Client) {
    let before = client.netns().unwrap();
    assert_eq!(
        (before.taken, before.made_on_path),
        (0, 0),
        "the example's box has no network namespace of its own"
    );
    if !lab.rootless {
        wait_for(&POOL_HOLDS, "the agent's pool to hold a namespace", || {
            let c = client.netns().unwrap();
            c.made_ahead > c.taken + c.discarded
        });
    }
    let id = BoxId::new("own-network").unwrap();
    client
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(lab.bundle_with(&Lab::act(&Act::Wait), true)),
            owner: None,
        })
        .unwrap();
    client.delete(&id).unwrap();
    let after = client.netns().unwrap();
    assert_eq!(
        (after.taken, after.made_on_path),
        if lab.rootless { (0, 1) } else { (1, 0) },
        "(taken, made on the path)"
    );
}

/// The conformance suite again over the socket, every `oci` box with a network namespace of its own
/// (decision 4): as root, once the agent's pool holds one, the boxes take theirs from it; rootless, each
/// is made on its create's path and none is taken.
fn conformance_with_namespaces(lab: &Lab, client: &Client) {
    if !lab.rootless {
        wait_for(&POOL_HOLDS, "the agent's pool to hold a namespace", || {
            let c = client.netns().unwrap();
            c.made_ahead > c.taken + c.discarded
        });
    }
    let before = client.netns().unwrap();
    let failures = hex_api::differential::run(client, &OciBoxes(lab, true));
    assert!(
        failures.is_empty(),
        "oci boxes with network namespaces of their own over the socket: {failures:#?}"
    );
    let after = client.netns().unwrap();
    if lab.rootless {
        assert_eq!(after.taken, before.taken, "a rootless agent takes none");
        assert!(after.made_on_path > before.made_on_path, "{after:?}");
    } else {
        assert!(
            after.taken > before.taken,
            "the pool served them: {after:?}"
        );
    }
}

/// HEX-SOCK-6 over the live agent: a caller execs a waiting program in an `oci` box and in a `process`
/// box over a connection of its own, then goes away without releasing them. Each program is ended and
/// reaped (no process of it in its box, no zombie of it under the agent), and its box counts it.
fn disconnect(lab: &Lab, agent: &Agent, client: &Client) {
    let oci = BoxId::new("left-oci").unwrap();
    let process = BoxId::new("left-process").unwrap();
    client
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: oci.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(lab.bundle(&Lab::act(&Act::Wait))),
            owner: None,
        })
        .unwrap();
    client.start(&oci).unwrap();
    client
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: process.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
        })
        .unwrap();
    let oci_cg = oci_cgroup(&agent.box_root.join(OCI_BOXES), &oci);
    let init = procs_in(&oci_cg);
    assert_eq!(init.len(), 1, "the oci box holds its init alone: {init:?}");
    let caller = raw(&agent.socket);
    let mut pids = Vec::new();
    for (n, (id, args)) in [
        (oci.clone(), Lab::act(&Act::Wait)),
        (process.clone(), {
            let mut a = vec![lab.exe.clone(), "selftest-box".to_owned()];
            a.extend(act_args(&Act::Wait));
            a
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let n = u64::try_from(n + 1).unwrap();
        let call = Call::Exec {
            id: id.clone(),
            program: program(&args),
            streams: Streams::default(),
        };
        send(
            &caller,
            &wire::request(n, &Request::Call(call), wire::MAX_MESSAGE).unwrap(),
        );
        match answer(&caller) {
            (h, Answer::Started { pid }) if h.id == n => pids.push(pid),
            other => panic!("exec in {id}: {other:?}"),
        }
    }
    wait_for(&RUNNING, "each program to run in its box", || {
        procs_in(&oci_cg).len() == 2 && procs(&agent.box_root, &process).len() == 1
    });
    drop(caller);
    for id in [&oci, &process] {
        wait_for(
            &ABANDONED,
            &format!("{id} to count its abandoned program"),
            || client.inspect(id).unwrap().abandoned == 1,
        );
    }
    // The count is the last of the agent's ending of a program, after its kill and reaping: once both are counted,
    // each fact of that ending holds at its first look, made within it.
    let ended = wait_for(&ABANDONED, "the programs to be ended", || {
        procs_in(&oci_cg) == init && procs(&agent.box_root, &process).is_empty()
    });
    assert_eq!(
        ended, 1,
        "a program counted abandoned was ended before its count"
    );
    for pid in &pids {
        let reaped = wait_for(&ABANDONED, &format!("program {pid} to be reaped"), || {
            !Path::new(&format!("/proc/{pid}")).exists()
        });
        assert_eq!(
            reaped, 1,
            "a program counted abandoned was reaped before its count"
        );
    }
    let reaped = wait_for(&ABANDONED, "no zombie under the agent", || {
        zombies_of(agent.pid()).is_empty()
    });
    assert_eq!(reaped, 1, "no zombie is left once the programs are counted");
    assert_eq!(
        client.inspect(&oci).unwrap().state,
        BoxState::Running,
        "the init runs on"
    );
    client.delete(&oci).unwrap();
    client.delete(&process).unwrap();
}

/// Where a crash case keeps the agent's containers' state, and so whether its boxes' records are
/// durable: on tmpfs, as the node keeps it in its runtime directory, the records beside it and not
/// fsynced; or on disk (an `XDG_RUNTIME_DIR` on disk, as a rootless node with none keeps it in its
/// state directory), every record in the state directory and fsynced.
#[derive(Clone, Copy, Debug)]
enum Kept {
    Tmpfs,
    Disk,
}

/// Where in a create a crash case kills the agent. The box's record is written once, last (goal 5
/// extended step 5), so before it the agent leaves only parts at the paths the box's id fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    /// Its cgroup made, nothing else of it: the agent armed to kill itself there (a debug build's crash
    /// point), the box rendered from an `OciSpec`.
    #[cfg(debug_assertions)]
    CgroupMade,
    /// Its bundle rendered after its cgroup, its container's state directory not made: likewise.
    #[cfg(debug_assertions)]
    BundleRendered,
    /// Its cgroup and its container's state directory made: the create held reading its bundle's
    /// config.json, a FIFO no one has opened.
    StateDir,
    /// Its init cloned and stopped before its root switch, its container's state not yet written: the
    /// create held in its createRuntime hook, which runs there (OCI config.md:607).
    Init,
    /// Past that stop: the init handed its state and running its createContainer hook, in its own
    /// namespaces before its root switch (config.md:618-622), the agent waiting for its ready signal; its
    /// container's state not yet written. The create held in that hook.
    CreateContainer,
    /// In the record write that marks it ready, a durable record's (so one written to a temp file and
    /// renamed): the box made, the write held opening its temp file, a FIFO no one has opened. A
    /// non-durable record's first write takes no temp ([`At::TornRecord`]).
    Ready,
    /// In the record write that marks it ready, a non-durable record's: the box made, the agent armed
    /// to kill itself between opening the record's file and writing it (a debug build's crash point),
    /// leaving a torn record. The first write to a non-durable record goes straight to its file, so no
    /// FIFO can hold it there; a release build can't arm the point, so it has no such case.
    #[cfg(debug_assertions)]
    TornRecord,
    /// As [`At::TornRecord`], the agent restarted beside a configured Shards backend (model cost inputs: no
    /// VM boots): a record its runtime directory holds is a native box's all the same.
    #[cfg(debug_assertions)]
    TornRecordBesideShards,
    /// After it: the create done.
    Made,
    /// As [`At::Made`], its record then marked as written in an earlier boot of the host (T-T7.1.2): the
    /// restarted agent removes it as that boot's, which ended everything of it, never taking it back.
    MadeInAnEarlierBoot,
    /// The box made, and its start under way in its startContainer hook, the start waiting on the exec
    /// FIFO the init hands its byte to only once its hooks have run. The agent's restart finds the start
    /// begun and never taken, and removes the box by that reason; its init, released, finds the start gone
    /// and ends with its own status (hex_runtime's `hooks::START_ABANDONED`).
    StartContainer,
}

impl At {
    /// The crash point the agent is armed at to die at this point, as `hex node --crash-at` names it.
    fn armed(self) -> Option<&'static str> {
        match self {
            #[cfg(debug_assertions)]
            Self::CgroupMade => Some("cgroup-made"),
            #[cfg(debug_assertions)]
            Self::BundleRendered => Some("bundle-rendered"),
            #[cfg(debug_assertions)]
            Self::TornRecord | Self::TornRecordBesideShards => Some("record-first-write"),
            Self::StateDir
            | Self::Init
            | Self::CreateContainer
            | Self::Ready
            | Self::Made
            | Self::MadeInAnEarlierBoot
            | Self::StartContainer => None,
        }
    }
}

/// T-T7.1.2 (goal 5 plan step 2; goal 5 extended step 5): kill the node agent at each point of a
/// create, with its boxes' records kept on tmpfs and on disk, then restart it. In a debug build the
/// agent is armed to kill itself once the box's cgroup is made, once its bundle is rendered, and —
/// where its record is non-durable, written once straight to its file — between that file's open and
/// the write, leaving a torn record: no file it opens can hold it at any of these. Elsewhere it is held
/// at a point by a FIFO no one has opened, or by the box's createRuntime hook, and killed once the
/// kernel shows it waiting there: with the cgroup and the container's state directory made; with the
/// init cloned and stopped for its create-time hooks; with the init past that stop, in its createContainer
/// hook; inside a durable record's write, opening the temp
/// file it renames; after the create; and, the box made, with its start in its startContainer hook.
/// Restarted, the agent removes the box killed mid-create by what it finds of it — a torn record it
/// reads as unreadable, and says so, a configured Shards backend beside it or not — and the one whose start
/// it cut, saying the start was interrupted in its startContainer hooks; takes back the one made, which then
/// deletes; and removes the one made whose record says an earlier boot of the host wrote it, saying so, so at
/// every point nothing of the box is left: no record in either place, no temp file, no cgroup, no container
/// state, no bundle.
fn crash_at_each_point_of_a_create(lab: &Lab, own: &Path, exe: &str) {
    for kept in [Kept::Tmpfs, Kept::Disk] {
        let mut points = Vec::new();
        #[cfg(debug_assertions)]
        points.extend([At::CgroupMade, At::BundleRendered]);
        points.extend([At::StateDir, At::Init, At::CreateContainer]);
        // The record write that marks a box ready is held by where its record is kept: a durable
        // record (on disk) is written to a temp file and renamed, so a FIFO holds the agent opening the
        // temp ([`At::Ready`]); a non-durable record (on tmpfs) is written once, straight to its file,
        // so only a debug build's crash point holds the agent there ([`At::TornRecord`]).
        match kept {
            Kept::Tmpfs => {
                #[cfg(debug_assertions)]
                points.extend([At::TornRecord, At::TornRecordBesideShards]);
            }
            Kept::Disk => points.push(At::Ready),
        }
        points.extend([At::Made, At::MadeInAnEarlierBoot, At::StartContainer]);
        for at in &points {
            crash_at(lab, own, exe, kept, *at);
        }
    }
    a_crash_case_that_fails_leaves_nothing_alive(lab, own, exe);
    #[cfg(debug_assertions)]
    a_box_cannot_arm_the_agents_crash_point(lab, own, exe);
}

fn crash_at(lab: &Lab, own: &Path, exe: &str, kept: Kept, at: At) {
    crash_case(lab, own, exe, kept, at, None);
}

/// The fifth crash case, the incident of 2026-11-05 on fedora (four inits left alive by rootless crash cases that
/// failed partway, their state directories gone; CLAUDE.md §2): run the box-made case set to fail right after the
/// kill, its box's init alive at the exec FIFO and the agent gone, holding a pidfd of each process in its cgroups.
/// Expect: the case unwinds, and every one of them has ended, its cgroups and its container's state removed.
fn a_crash_case_that_fails_leaves_nothing_alive(lab: &Lab, own: &Path, exe: &str) {
    let kept = if lab.rootless {
        Kept::Disk
    } else {
        Kept::Tmpfs
    };
    let mut held = Vec::new();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crash_case(lab, own, exe, kept, At::Made, Some(&mut held))
    }));
    assert!(failed.is_err(), "the case failed on purpose");
    assert!(
        !held.is_empty(),
        "non-vacuity: the failed case's cgroups held its init at the failure"
    );
    let alive = held.iter().filter(|fd| !has_ended(fd)).count();
    assert_eq!(alive, 0, "processes the failed case left alive");
    let name = format!("crash-{kept:?}-{:?}-failed", At::Made).to_lowercase();
    assert!(
        !own.join(&name).exists(),
        "the failed case's cgroups are left"
    );
    eprintln!(
        "{name}: failed on purpose with {} processes in its cgroups; each ended, nothing left",
        held.len()
    );
}

/// Whether the process `fd` is a pidfd of has ended: it reads ready once it has.
fn has_ended(fd: &OwnedFd) -> bool {
    let mut fds = [rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN)];
    let now = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&now)).is_ok_and(|n| n > 0)
}

/// A crash case, `fail` set to make it fail right after the kill (the box made, its init alive), with a pidfd of
/// each process then in its cgroups pushed to it.
fn crash_case(
    lab: &Lab,
    own: &Path,
    exe: &str,
    kept: Kept,
    at: At,
    fail: Option<&mut Vec<OwnedFd>>,
) {
    let name = match fail {
        None => format!("crash-{kept:?}-{at:?}").to_lowercase(),
        Some(_) => format!("crash-{kept:?}-{at:?}-failed").to_lowercase(),
    };
    let dir = tempfile::Builder::new()
        .prefix(&format!("{name}-"))
        .tempdir_in(lab.bundles.path())
        .unwrap();
    // The agent's oci root, where the node puts it: a root node's under /run, a rootless node's under
    // its runtime directory, which this case may set on disk.
    let (runtime, oci_root) = match (kept, lab.rootless) {
        (Kept::Tmpfs, false) => (None, PathBuf::from("/run/hex-node")),
        (Kept::Tmpfs, true) => match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(xdg) => (None, PathBuf::from(xdg).join("hex-node")),
            None => {
                eprintln!("SKIP {name}: no XDG_RUNTIME_DIR");
                return;
            }
        },
        (Kept::Disk, true) => {
            let run = dir.path().join("run");
            std::fs::create_dir(&run).unwrap();
            let oci_root = run.join("hex-node");
            (Some(run), oci_root)
        }
        (Kept::Disk, false) => {
            eprintln!("SKIP {name}: a root node keeps its containers' state under /run");
            return;
        }
    };
    // The box's id is this run's alone, the case's directory name: a rootless tmpfs case's oci root is the user's
    // runtime directory's, which every run of this test under that user shares, so a fixed id counts another run's
    // container state, left or under way, as this one's.
    let id = BoxId::new(dir.path().file_name().and_then(|n| n.to_str()).unwrap()).unwrap();
    let state = dir.path().join("state");
    let cgroup = own.join(&name).join(OCI_BOXES).join(id.as_str());
    let rendered = state.join("bundles").join(id.as_str());
    // Ends what the case made as it ends, passed or failed: declared before the agents, so it drops after them.
    let _made = CaseMade {
        boxes: own.join(&name),
        oci_root: oci_root.clone(),
        id: id.clone(),
    };
    let (hooked, go) = (dir.path().join("hooked"), dir.path().join("go"));
    // Keep a writer open until the case ends, including unwinding: an orphaned hook reads EOF
    // after an assertion failure. A polled release file can disappear with the TempDir before
    // the hook observes it, leaving the hook and this test's output pipe alive forever.
    let mut hook_release = None;
    let class = match at {
        #[cfg(debug_assertions)]
        At::CgroupMade | At::BundleRendered => ClassSpec::Oci(Box::new(lab.spec())),
        At::StateDir => {
            let bundle = dir.path().join("bundle");
            std::fs::create_dir(&bundle).unwrap();
            mkfifo(&bundle.join("config.json"));
            ClassSpec::Bundle(bundle)
        }
        At::Init | At::CreateContainer | At::Ready => {
            mkfifo(&go);
            hook_release = Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&go)
                    .unwrap(),
            );
            // The create waits in its createRuntime hook (or, past the init's stop for the hooks, in its
            // createContainer hook), its container made, until this test writes a line, or closes the
            // writer on an assertion failure.
            let script = crash_hook_script(&hooked, &go);
            let list = if at == At::CreateContainer {
                "createContainer"
            } else {
                "createRuntime"
            };
            let hooks = format!(
                r#",
  "hooks": {{"{list}": [{{"path": "/bin/sh", "args": ["sh", "-c", {script:?}], "env": ["PATH=/usr/bin:/bin"]}}]}}"#
            );
            ClassSpec::Bundle(lab.bundle_hooked(&Lab::act(&Act::Wait), false, &hooks))
        }
        // Armed to crash at the record write, so it creates fully, like a box made; a bundle box's
        // record names only its container's state (on tmpfs here), so it is non-durable.
        #[cfg(debug_assertions)]
        At::TornRecord | At::TornRecordBesideShards => {
            ClassSpec::Bundle(lab.bundle(&Lab::act(&Act::Wait)))
        }
        At::Made | At::MadeInAnEarlierBoot => ClassSpec::Bundle(lab.bundle(&Lab::act(&Act::Wait))),
        At::StartContainer => {
            mkfifo(&go);
            hook_release = Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&go)
                    .unwrap(),
            );
            // The hook runs inside the box's root, so this case's directory is bound in at /crash, where
            // it touches `hooked` and waits for a line on `go`.
            let script = crash_hook_script(Path::new("/crash/hooked"), Path::new("/crash/go"));
            let hooks = format!(
                r#",
  "hooks": {{"startContainer": [{{"path": "/bin/sh", "args": ["sh", "-c", {script:?}], "env": ["PATH=/usr/bin:/bin"]}}]}}"#
            );
            let bundle = lab.bundle_hooked(&Lab::act(&Act::Wait), false, &hooks);
            std::fs::create_dir_all(lab.rootfs.join("crash")).unwrap();
            let config = bundle.join("config.json");
            let mut spec: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
            spec["mounts"].as_array_mut().unwrap().push(serde_json::json!(
                {"destination": "/crash", "type": "bind", "source": dir.path(), "options": ["rbind", "rw"]}
            ));
            std::fs::write(&config, spec.to_string()).unwrap();
            ClassSpec::Bundle(bundle)
        }
    };
    let spec = BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class,
        owner: None,
    };
    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut agent = Agent::armed(
        exe,
        dir.path(),
        own.join(&name),
        runtime.as_deref(),
        at.armed(),
        &[],
    );
    if on_tmpfs(&oci_root) != matches!(kept, Kept::Tmpfs) {
        eprintln!("SKIP {name}: {} isn't where it's kept", oci_root.display());
        return;
    }
    // The record's place, now the agent has made its state directory: the state directory for a
    // durable one, else the node's own beside its containers' state.
    let records = match kept {
        Kept::Tmpfs => hex_node::records_dir(&oci_root, &state).unwrap(),
        Kept::Disk => state.clone(),
    };
    // A non-durable record is kept in its shard of the runtime record directory (goal 5 extended, step 6).
    let (record, temp) = match kept {
        Kept::Tmpfs => (
            hex_node::store::runtime_record(&records, &id),
            hex_node::store::runtime_temp(&records, &id),
        ),
        Kept::Disk => (
            records.join(format!("{id}.json")),
            records.join(format!(".{id}.tmp")),
        ),
    };
    let socket = agent.socket.clone();
    let create = std::thread::spawn(move || Client::connect(&socket).and_then(|c| c.create(spec)));
    let mut start = None;
    match at {
        #[cfg(debug_assertions)]
        At::CgroupMade | At::BundleRendered | At::TornRecord | At::TornRecordBesideShards => {
            use std::os::unix::process::ExitStatusExt;
            let ended = agent.ended();
            assert_eq!(
                ended.signal(),
                Some(9),
                "{name}: the agent killed itself at its crash point: {ended:?}"
            );
        }
        At::StateDir => {}
        At::Init | At::CreateContainer => {
            wait_for(&CREATING, "the create's hook to hold it", || {
                hooked.exists()
            });
            assert!(!create.is_finished(), "{name}: the hook holds the create");
        }
        At::Ready => {
            wait_for(&CREATING, "the create's hook to hold it", || {
                hooked.exists()
            });
            assert!(!create.is_finished(), "{name}: the hook holds the create");
            mkfifo(&temp);
            hook_release.as_mut().unwrap().write_all(b"\n").unwrap();
        }
        At::Made | At::MadeInAnEarlierBoot => {
            wait_for(&CREATING, "the create", || create.is_finished());
        }
        At::StartContainer => {
            wait_for(&CREATING, "the create", || create.is_finished());
            // Connected and greeted first: the wait below is for what the start makes within itself, its record's,
            // and a client's connect and hello are no part of a start (the waits census's run 5: this wait ran out at
            // 15.220 ms in 3 of 53 loops, the hello's answers under the suites' load reaching 17.232 ms).
            let client = Client::connect(&agent.socket).expect("connect for the start");
            let id = id.clone();
            start = Some(std::thread::spawn(move || client.start(&id)));
            wait_for(
                &STARTING,
                "the start's startContainer hook to hold it",
                || hooked.exists(),
            );
            assert!(
                !start.as_ref().unwrap().is_finished(),
                "{name}: the hook holds the start"
            );
        }
    }
    if matches!(at, At::StateDir | At::Ready) {
        wait_for(&CREATING, "the agent to wait opening a FIFO", || {
            waits_opening_a_fifo(agent.pid())
        });
        assert!(!create.is_finished(), "{name}: the FIFO holds the create");
    }
    // What the kill leaves, as the point says: a record only once the box is made, its container's
    // state directory from the runtime's create on, its state written once its init is past its hooks.
    let containers = || containers_of(&oci_root, &id);
    // A record counts as written only once it decodes whole: a crash between a non-durable record's
    // open and its write leaves a torn file, which reads as no record (At::TornRecord).
    let written = || {
        std::fs::read(&record)
            .ok()
            .and_then(|bytes| BoxRecord::decode(&bytes).ok())
            .map(|record| record.phase)
    };
    let made = if matches!(at, At::Made | At::MadeInAnEarlierBoot | At::StartContainer) {
        Some(Phase::Ready)
    } else {
        None
    };
    assert_eq!(written(), made, "{name}: the record at the kill");
    // A box of no owner has its cgroup chosen once the runtime has read its spec, as an owner's has, since the
    // node keeps cgroups made ahead for it (decision 8): the bundle is rendered, then the runtime makes the
    // container's state directory, then the cgroup is made. So every point but the bundle rendered has the
    // state directory, and every point past the state directory the cgroup; the record write that marks the
    // box ready comes after it is all made.
    let state_dir_made = match at {
        #[cfg(debug_assertions)]
        At::BundleRendered => false,
        _ => true,
    };
    let cgroup_made = state_dir_made && at != At::StateDir;
    assert_eq!(
        (containers().len(), cgroup.exists()),
        (usize::from(state_dir_made), cgroup_made),
        "{name}: (container states, the cgroup) at the kill"
    );
    #[cfg(debug_assertions)]
    assert_eq!(
        rendered.exists(),
        matches!(at, At::BundleRendered | At::CgroupMade),
        "{name}: the bundle at the kill"
    );
    let state_written = containers()
        .first()
        .is_some_and(|c| c.join("state.json").exists());
    assert_eq!(
        state_written,
        state_dir_made
            && !matches!(at, At::StateDir | At::Init | At::CreateContainer)
            && !at_cgroup_made(at),
        "{name}: the container's state at the kill"
    );
    if let Some(held) = fail {
        held.extend(
            processes_in(&own.join(&name))
                .into_iter()
                .filter_map(|pid| {
                    rustix::process::pidfd_open(
                        rustix::process::Pid::from_raw(pid)?,
                        rustix::process::PidfdFlags::empty(),
                    )
                    .ok()
                }),
        );
        panic!("{name}: failed on purpose after the kill");
    }
    drop(agent);
    if matches!(at, At::Init | At::CreateContainer | At::StartContainer) {
        // The hook may outlive the agent it ran under; let it end. Released, a startContainer hook's init
        // goes on to hand its byte to the start that is gone.
        hook_release.as_mut().unwrap().write_all(b"\n").unwrap();
    }
    let created = create.join().unwrap();
    assert_eq!(
        created.is_ok(),
        matches!(at, At::Made | At::MadeInAnEarlierBoot | At::StartContainer),
        "{name}: the create: {created:?}"
    );
    if at == At::MadeInAnEarlierBoot {
        // The record as an earlier boot of the host left it: its session another than this boot's.
        let mut written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        written["boot"] = serde_json::Value::from(EARLIER_BOOT);
        let mut bytes = serde_json::to_vec(&written).unwrap();
        bytes.push(b'\n');
        std::fs::write(&record, bytes).unwrap();
    }
    if let Some(start) = start {
        let started = start.join().unwrap();
        assert!(
            started.is_err(),
            "{name}: the start the kill cut: {started:?}"
        );
    }

    #[cfg(debug_assertions)]
    let env = if at == At::TornRecordBesideShards {
        shards_configured(dir.path())
    } else {
        Vec::new()
    };
    #[cfg(not(debug_assertions))]
    let env = Vec::new();
    let agent = Agent::armed(
        exe,
        dir.path(),
        own.join(&name),
        runtime.as_deref(),
        None,
        &env,
    );
    let client = agent.client();
    if at == At::Made {
        let info = client.inspect(&id).expect("the box made is taken back");
        assert_eq!(info.state, BoxState::Created, "{name}: as it was");
        client.delete(&id).unwrap();
    } else {
        assert_eq!(
            client.inspect(&id),
            Err(Refusal::NoSuchBox(id.clone())),
            "{name}: the box killed mid-create is cleaned up"
        );
    }
    // A record an earlier boot of the host wrote: recovery removed its box as that boot's, and said so
    // (hex_core::recovery's Why::EarlierBoot), never taking it back.
    if at == At::MadeInAnEarlierBoot {
        wait_for(
            &SAID,
            "recovery to report the box removed as an earlier boot's",
            || {
                agent
                    .errors()
                    .lines()
                    .any(|l| l.contains(id.as_str()) && l.contains("earlier boot"))
            },
        );
    }
    // A torn first write leaves a record that can't be read; recovery removed its box and said so
    // (hex_core::recovery's Why::Unreadable). The reason is the point of the case.
    // A start cut in its startContainer hooks: recovery removed the box by that reason, and said so
    // (hex_core::recovery's Why::StartInterrupted), never as a box merely stopped.
    if at == At::StartContainer {
        wait_for(
            &SAID,
            "recovery to report the box's start interrupted",
            || {
                agent
                    .errors()
                    .lines()
                    .any(|l| l.contains(id.as_str()) && l.contains("startContainer hooks"))
            },
        );
    }
    #[cfg(debug_assertions)]
    if matches!(at, At::TornRecord | At::TornRecordBesideShards) {
        wait_for(
            &SAID,
            "recovery to report the box removed as unreadable",
            || {
                agent
                    .errors()
                    .lines()
                    .any(|l| l.contains(id.as_str()) && l.contains("couldn't be read"))
            },
        );
    }
    assert!(client.list().unwrap().is_empty(), "{name}: a box is left");
    for place in [&state, &records] {
        assert!(
            !place.join(format!("{id}.json")).exists(),
            "{name}: its record is left in {}",
            place.display()
        );
    }
    assert!(!record.exists(), "{name}: its record is left");
    assert!(!temp.exists(), "{name}: its record's temp file is left");
    assert!(
        containers().is_empty(),
        "{name}: its container's state is left"
    );
    assert!(!cgroup.exists(), "{name}: its cgroup is left");
    assert!(!rendered.exists(), "{name}: its bundle is left");
    eprintln!(
        "{name}: killed with the record {made:?}, its cgroup made, its container's state directory {}made and its state {}written; after the restart {}, nothing left",
        if state_dir_made { "" } else { "not " },
        if state_written { "" } else { "not " },
        if at == At::Made {
            "taken back, then deleted"
        } else {
            "cleaned up"
        }
    );
}

/// A boot session no host is in, a UUID's form all zeros, as an earlier boot's: every boot's is random
/// (`/proc/sys/kernel/random/boot_id`).
const EARLIER_BOOT: &str = "00000000-0000-0000-0000-000000000000";

/// The environment that configures `hex node`'s Shards backend (AC-T7.4.1), from model inputs written in `dir`: its
/// guest named (nothing boots it here, so no file need exist) at Shards' least envelope, one vCPU and its least
/// memory, and its runtime cost, whose id is its evidence's SHA-256 and whose file its own digest pins.
#[cfg(debug_assertions)]
fn shards_configured(dir: &Path) -> Vec<(&'static str, std::ffi::OsString)> {
    use hex_runtime::guest::{
        ENV_COST_EVIDENCE, ENV_COST_FILE, ENV_COST_SHA256, ENV_CPUS, ENV_INITRD, ENV_KERNEL,
        ENV_MEMORY_MIB, ENV_SHARDS_BIN,
    };
    use sha2::{Digest, Sha256};
    let memory_mib = shards_api::min_memory_mib(std::env::consts::ARCH);
    let evidence = dir.join("cost-evidence");
    std::fs::write(&evidence, b"T-T7.1.2 model evidence: no native measurement").unwrap();
    let envelope = hex_api::CapacityResources {
        cpu_milli: 1000,
        mem_bytes: memory_mib << 20,
        disk_bytes: 0,
    };
    let least = hex_api::CapacityResources {
        cpu_milli: 1,
        mem_bytes: 1,
        disk_bytes: 0,
    };
    let cost = hex_api::RuntimeCost {
        id: Sha256::digest(std::fs::read(&evidence).unwrap()).into(),
        floor: envelope,
        ceiling: envelope,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: least,
        overhead: least,
    };
    let file = dir.join("cost.json");
    let bytes = serde_json::to_vec(&cost).unwrap();
    std::fs::write(&file, &bytes).unwrap();
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    vec![
        (ENV_SHARDS_BIN, "/nonexistent/shards".into()),
        (ENV_KERNEL, "/nonexistent/kernel".into()),
        (ENV_INITRD, "/nonexistent/initrd".into()),
        (ENV_CPUS, "1".into()),
        (ENV_MEMORY_MIB, memory_mib.to_string().into()),
        (ENV_COST_FILE, file.into_os_string()),
        (ENV_COST_SHA256, digest.into()),
        (ENV_COST_EVIDENCE, evidence.into_os_string()),
    ]
}

/// What a crash case makes outside its temporary directory: its boxes' cgroups under `boxes` and its container's
/// state under `oci_root`. Dropped, as the case ends or unwinds from a failed assertion, it ends every process in
/// those cgroups through `cgroup.kill`, waits for them to empty (bounded by the conformance suite's patience) and
/// removes them, deepest first, and the container's state: a case that fails leaves nothing alive (CLAUDE.md §2).
/// A case that passed has ended its boxes, so the drop finds their cgroups empty and removes them.
struct CaseMade {
    boxes: PathBuf,
    oci_root: PathBuf,
    id: BoxId,
}

impl Drop for CaseMade {
    fn drop(&mut self) {
        if self.boxes.exists() {
            // Waited on cgroup.events' `populated`, as a delete waits for a box to empty, not polled on a picked
            // period (Ada's 2026-11-06 rule).
            let cg = hex_runtime::linux::cgroup::BoxCgroup::at(self.boxes.clone());
            let _ = cg.kill();
            let _ = cg.wait_empty(EMPTYING.saturating_mul(2));
            let (mut walk, mut dirs) = (vec![self.boxes.clone()], Vec::new());
            while let Some(dir) = walk.pop() {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    walk.extend(
                        entries
                            .flatten()
                            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                            .map(|e| e.path()),
                    );
                }
                dirs.push(dir);
            }
            for dir in dirs.iter().rev() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        for state in containers_of(&self.oci_root, &self.id) {
            let _ = std::fs::remove_dir_all(state);
        }
    }
}

/// Every process in the cgroup `root` and those under it: a flat walk (CLAUDE.md §11).
fn processes_in(root: &Path) -> Vec<i32> {
    let (mut dirs, mut pids) = (vec![root.to_path_buf()], Vec::new());
    while let Some(dir) = dirs.pop() {
        if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
            pids.extend(procs.lines().filter_map(|l| l.trim().parse::<i32>().ok()));
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            dirs.extend(
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path()),
            );
        }
    }
    pids
}

fn crash_hook_script(hooked: &Path, go: &Path) -> String {
    format!(
        "touch {} && read -r release < {}",
        hooked.display(),
        go.display()
    )
}

/// T-T7.1.2's fixture: a killed node's hook still waits for release, and ends even when its test
/// unwinds without releasing it. No release filename can disappear before a polling hook sees it.
#[test]
fn a_crash_hook_waits_for_a_line_and_ends_when_its_writer_closes() {
    use std::os::unix::fs::OpenOptionsExt;

    struct Hook(Child);
    impl Drop for Hook {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for release in [true, false] {
        let dir = tempfile::tempdir_in(cache_dir()).unwrap();
        let go = dir.path().join("go");
        let ready = dir.path().join("ready");
        mkfifo(&go);
        let mut hook = Hook(
            Command::new("/bin/sh")
                .arg("-c")
                .arg(crash_hook_script(&ready, &go))
                .stdout(Std::null())
                .stderr(Std::null())
                .spawn()
                .unwrap(),
        );
        let mut writer = None;
        wait_for(&HOOK, "the hook to open its release pipe", || {
            assert!(
                hook.0.try_wait().unwrap().is_none(),
                "the crash hook ended before its pipe was released"
            );
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap())
                .open(&go)
            {
                Ok(f) => {
                    writer = Some(f);
                    true
                }
                // Opening a FIFO's writer without blocking succeeds only once a reader opened it.
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NXIO.raw_os_error()) => false,
                Err(e) => panic!("opening the release pipe: {e}"),
            }
        });
        if release {
            writer.as_mut().unwrap().write_all(b"\n").unwrap();
        }
        drop(writer);
        wait_for(
            &HOOK,
            "the hook to end after release or writer close",
            || hook.0.try_wait().unwrap().is_some(),
        );
    }
}

/// T-T7.1.2's crash point is the agent's alone to arm (`hex node --crash-at`, from its own arguments):
/// an agent started without it makes a box whose own spec names the point, in its init's environment as
/// a variable might carry it, whole, and lives on. Debug builds only, where the point exists to be proven
/// inert.
#[cfg(debug_assertions)]
fn a_box_cannot_arm_the_agents_crash_point(lab: &Lab, own: &Path, exe: &str) {
    let dir = tempfile::Builder::new()
        .prefix("crash-unarmed-")
        .tempdir_in(lab.bundles.path())
        .unwrap();
    let mut agent = Agent::over(exe, dir.path(), own.join("crash-unarmed"), None);
    let client = agent.client();
    let id = BoxId::new("names-the-crash-point").unwrap();
    let mut spec = lab.spec();
    for (name, point) in [
        ("HEX_CRASH_AT", "cgroup-made"),
        ("CRASH_AT", "bundle-rendered"),
    ] {
        spec.init.env.push((name.to_owned(), point.to_owned()));
    }
    let made = client
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Oci(Box::new(spec)),
            owner: None,
        })
        .expect("a box naming the crash point is made");
    assert_eq!(made.state, BoxState::Created);
    assert!(agent.runs(), "the agent lives on");
    client.delete(&id).unwrap();
    eprintln!(
        "a box naming the crash point in its own spec: made whole, the unarmed agent living on"
    );
}

/// The state directories under `oci_root` of box `id`'s container, whatever node's: each is named
/// `<node key>-<box id>`, the key 16 hex digits.
/// The cgroups under the oci boxes' root `oci` that are slots ([`hex_core::reuse::slot_of`]).
fn slots_under(oci: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(oci)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|name| hex_core::reuse::slot_of(name).is_some())
        .collect()
}

/// The slots under `oci` holding a process that isn't an init parked ahead (a fork of the create seed, `hex
/// __oci-seed`, until claimed: decision 8): the slots boxes hold.
fn box_slots(oci: &Path) -> std::collections::BTreeSet<String> {
    let parked = |pid: &String| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|b| b.split(|&c| c == 0).any(|a| a == b"__oci-seed"))
    };
    slots_under(oci)
        .into_iter()
        .filter(|name| {
            let p = procs_in(&oci.join(name));
            !p.is_empty() && !p.iter().all(parked)
        })
        .collect()
}

/// `hex node` ends on SIGTERM saying its parked procs (decision 8, parked procs; the lead, 2026-11-06, AC-T7.1.3,
/// T-T7.1.2): one line of counts on stderr, then it exits 143, and nothing else of its end differs from a
/// SIGKILL's. Do: over the socket, make and start A of owner O, delete A, make and start B of O; SIGTERM the
/// agent; start it again over the same state. Expect: it said `parked procs at exit:` with made, hits, misses,
/// dry, capped and alive, as root B's create a hit of the proc A's delete made (rootless none made); it exited 143;
/// its socket's file and B's processes are left as a SIGKILL leaves them; the restarted agent takes B back
/// running, and once B is deleted no box is listed.
fn a_sigterm_says_the_parked_procs_and_leaves_the_boxes_as_a_sigkill_does(
    lab: &Lab,
    own: &Path,
    exe: &str,
) {
    let dir = tempfile::Builder::new()
        .prefix("agent-term-")
        .tempdir_in(lab.bundles.path())
        .unwrap();
    let box_root = own.join("hex-sock-term");
    let mut agent = Agent::over(exe, dir.path(), box_root.clone(), None);
    let client = agent.client();
    let o = BoxOwner::named("owner-term").unwrap();
    let wait = Lab::act(&Act::Wait);
    let id = |name: &str| BoxId::new(name).unwrap();
    for name in ["term-a", "term-b"] {
        client
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id(name),
                bounds: Bounds::default(),
                class: ClassSpec::Bundle(lab.bundle(&wait)),
                owner: Some(o.clone()),
            })
            .unwrap();
        client.start(&id(name)).unwrap();
        if name == "term-a" {
            client.delete(&id(name)).unwrap();
        }
    }
    drop(client);
    let term = Command::new("kill")
        .args(["-TERM", &agent.pid().to_string()])
        .status()
        .unwrap();
    assert!(term.success(), "kill -TERM {}", agent.pid());
    let ended = agent.child.wait().unwrap();
    assert_eq!(
        ended.code(),
        Some(143),
        "hex node ended on SIGTERM: {ended:?}"
    );
    wait_for(&SAID, "the agent's parked procs line", || {
        agent.errors().contains("hex node: parked procs at exit:")
    });
    let said = agent.errors();
    let line = said
        .lines()
        .find_map(|l| l.strip_prefix("hex node: parked procs at exit: "))
        .unwrap()
        .to_owned();
    let counts: Vec<(String, u64)> = line
        .split(", ")
        .map(|kv| {
            let (k, v) = kv.split_once(' ').unwrap();
            (k.to_owned(), v.parse().unwrap())
        })
        .collect();
    let names: Vec<&str> = counts.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        ["made", "hits", "misses", "dry", "capped", "alive"],
        "{line}"
    );
    let count = |k: &str| counts.iter().find(|(n, _)| n == k).unwrap().1;
    if lab.rootless {
        assert_eq!(
            (count("made"), count("hits")),
            (0, 0),
            "a rootless node parks none: {line}"
        );
    } else {
        assert!(
            count("hits") >= 1 && count("made") >= count("hits"),
            "B took A's delete's proc: {line}"
        );
    }
    assert!(
        agent.socket.exists(),
        "the socket's file is left, as a SIGKILL leaves it"
    );
    assert!(
        !procs_in(&oci_cgroup(&box_root.join(OCI_BOXES), &id("term-b"))).is_empty(),
        "B runs on after its agent's SIGTERM"
    );
    drop(agent);
    let again = Agent::over(exe, dir.path(), box_root.clone(), None);
    let client = again.client();
    assert_eq!(
        client.inspect(&id("term-b")).unwrap().state,
        BoxState::Running,
        "B is taken back running"
    );
    client.delete(&id("term-b")).unwrap();
    assert!(client.list().unwrap().is_empty(), "no box is listed");
    eprintln!("T-T7.1.2, parked procs: SIGTERM said {line:?}, exited 143, B taken back");
}

/// Goal 5 extended, step 6 (T-T7.1.2). Do: over the socket, make and start boxes A and B of owner O and a
/// box U of none, delete A (its slot goes idle), SIGKILL the agent, and start it again over the same
/// state. Expect: the restarted agent removes A's idle slot and says so; takes B back in its recorded
/// slot and U by its id, both running, B's usage read through its slot; and once both are deleted
/// nothing is left under the oci boxes' root and no box is listed.
fn a_crash_with_idle_cgroups_and_live_boxes_leaves_nothing(lab: &Lab, own: &Path, exe: &str) {
    let dir = tempfile::Builder::new()
        .prefix("agent-reuse-")
        .tempdir_in(lab.bundles.path())
        .unwrap();
    let box_root = own.join("hex-sock-reuse");
    let mut agent = Agent::over(exe, dir.path(), box_root.clone(), None);
    let client = agent.client();
    let o = BoxOwner::named("owner-crash").unwrap();
    let wait = Lab::act(&Act::Wait);
    let id = |name: &str| BoxId::new(name).unwrap();
    for (name, owner) in [
        ("crash-a", Some(&o)),
        ("crash-b", Some(&o)),
        ("crash-u", None),
    ] {
        client
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id(name),
                bounds: Bounds::default(),
                class: ClassSpec::Bundle(lab.bundle(&wait)),
                owner: owner.cloned(),
            })
            .unwrap();
        client.start(&id(name)).unwrap();
    }
    let oci = box_root.join(OCI_BOXES);
    // The slots a box holds: those with a process in them that isn't an init parked ahead (decision 8), whose
    // slots made ahead are slots too.
    let slots = box_slots(&oci);
    assert_eq!(slots.len(), 2, "A and B each hold a slot: {slots:?}");
    assert!(
        oci.join("crash-u").is_dir(),
        "U's cgroup is its own, named by its id"
    );
    client.delete(&id("crash-a")).unwrap();
    let b_slot = box_slots(&oci);
    assert_eq!(b_slot.len(), 1, "B's slot alone holds a box: {b_slot:?}");
    let a_slot = slots.difference(&b_slot).next().unwrap().clone();
    assert!(
        oci.join(&a_slot).is_dir(),
        "A's slot is kept idle for its owner"
    );
    drop(client);
    agent.child.kill().unwrap();
    let ended = agent.child.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&ended),
        Some(9)
    );
    drop(agent);
    let again = Agent::over(exe, dir.path(), box_root.clone(), None);
    wait_for(
        &SAID,
        "the restarted agent to say it removed the idle slot",
        || again.errors().contains("removed idle cgroup"),
    );
    let left = slots_under(&oci);
    assert_eq!(
        left, b_slot,
        "A's idle slot is removed with every one made ahead, B's kept: {left:?}"
    );
    let client = again.client();
    for name in ["crash-b", "crash-u"] {
        let info = client.inspect(&id(name)).unwrap();
        assert_eq!(
            info.state,
            BoxState::Running,
            "{name} is taken back running"
        );
    }
    client.usage(&id("crash-b")).unwrap();
    for name in ["crash-b", "crash-u"] {
        client.delete(&id(name)).unwrap();
    }
    let names: Vec<String> = std::fs::read_dir(&oci)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    assert!(
        names.is_empty(),
        "nothing is left under the oci boxes' root: {names:?}"
    );
    assert!(client.list().unwrap().is_empty(), "no box is listed");
    eprintln!(
        "T-T7.1.2, step 6: idle slot removed on restart ({}), B taken back in {left:?}, nothing left",
        again
            .errors()
            .lines()
            .filter(|l| l.contains("idle cgroup"))
            .collect::<Vec<_>>()
            .join("; ")
    );
}

/// Whether `at` is the point the box's own cgroup is made at, before its state is written (a debug build's).
fn at_cgroup_made(at: At) -> bool {
    #[cfg(debug_assertions)]
    if at == At::CgroupMade {
        return true;
    }
    let _ = at;
    false
}

fn containers_of(oci_root: &Path, id: &BoxId) -> Vec<PathBuf> {
    // In the runtime directory's shards, or in the directory itself where a node before them kept them (goal 5
    // extended, step 6).
    (0..hex_runtime::linux::oci::state::SHARDS)
        .map(|n| oci_root.join(format!("{n:02x}")))
        .chain([oci_root.to_path_buf()])
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split_once('-'))
                .is_some_and(|(key, rest)| {
                    key.len() == 16
                        && key.bytes().all(|b| b.is_ascii_hexdigit())
                        && rest == id.as_str()
                })
        })
        .collect()
}

/// Whether a thread of process `pid` waits to open a FIFO no one has opened the other end of: the
/// kernel shows it in `wait_for_partner` (fs/pipe.c), or in `fifo_open` where that's inlined.
fn waits_opening_a_fifo(pid: i32) -> bool {
    std::fs::read_dir(format!("/proc/{pid}/task")).is_ok_and(|tasks| {
        tasks
            .filter_map(Result::ok)
            .filter_map(|task| std::fs::read_to_string(task.path().join("wchan")).ok())
            .any(|wchan| matches!(wchan.trim(), "wait_for_partner" | "fifo_open"))
    })
}

fn mkfifo(path: &Path) {
    assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
}

/// Whether `dir` lies on tmpfs (include/uapi/linux/magic.h, `TMPFS_MAGIC`).
fn on_tmpfs(dir: &Path) -> bool {
    rustix::fs::statfs(dir).is_ok_and(|fs| i128::from(fs.f_type) == 0x0102_1994)
}

/// A connection that sends exactly what it is given, and goes away as a dead process's does: no
/// release is sent for what it started.
fn raw(socket: &Path) -> std::os::fd::OwnedFd {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::connect(&fd, &SocketAddrUnix::new(socket).unwrap()).unwrap();
    fd
}

fn send(fd: &std::os::fd::OwnedFd, bytes: &[u8]) {
    rustix::net::send(fd, bytes, rustix::net::SendFlags::NOSIGNAL).unwrap();
}

fn answer(fd: &std::os::fd::OwnedFd) -> (wire::Header, Answer) {
    let mut ready = [rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN)];
    let ts = rustix::event::Timespec {
        tv_sec: i64::try_from(ANSWERED.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(ANSWERED.subsec_nanos()),
    };
    assert_eq!(
        rustix::event::poll(&mut ready, Some(&ts)).unwrap(),
        1,
        "no answer came"
    );
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let (n, _) = rustix::net::recv(fd, &mut buf, rustix::net::RecvFlags::empty()).unwrap();
    let msg = wire::Received {
        bytes: &buf[..n],
        size: n,
        descriptors: 0,
        descriptors_cut: false,
    };
    wire::read_answer(&msg, wire::MAX_MESSAGE).unwrap()
}

/// The processes in a box's cgroup (an ended one leaves it as it ends).
fn procs(box_root: &Path, id: &BoxId) -> Vec<String> {
    procs_in(&box_root.join(id.as_str()))
}

/// The processes in cgroup `dir`, by pid.
fn procs_in(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Started oci box `id`'s cgroup under the oci boxes' root `oci`: one named by its id, else the one slot holding a
/// process that isn't an init parked ahead (a fork of the create seed, `hex __oci-seed`, until claimed; decision 8),
/// the box's init having exec'd its program.
fn oci_cgroup(oci: &Path, id: &BoxId) -> PathBuf {
    let named = oci.join(id.as_str());
    if named.exists() {
        return named;
    }
    let parked = |pid: &String| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|b| b.split(|&c| c == 0).any(|a| a == b"__oci-seed"))
    };
    let found: Vec<PathBuf> = std::fs::read_dir(oci)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| hex_core::reuse::slot_of(n).is_some())
        })
        .map(|e| e.path())
        .filter(|dir| {
            let p = procs_in(dir);
            !p.is_empty() && !p.iter().all(parked)
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "box {id}'s cgroup, by its processes: {found:?}"
    );
    found.into_iter().next().unwrap()
}

/// The agent's children that have ended and aren't reaped.
fn zombies_of(agent: i32) -> Vec<String> {
    let mut out = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{agent}/task"))
        .unwrap()
        .flatten()
    {
        let children = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in children.split_whitespace() {
            let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
            if stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            {
                out.push(child.to_owned());
            }
        }
    }
    out
}

/// Waits for `cond` on `wait`, its own fact's record or measuring (hex_testkit::waits): says how many looks it took.
fn wait_for(wait: &Patience, what: &str, cond: impl FnMut() -> bool) -> u64 {
    until(wait, what, cond).unwrap()
}

/// The capability sets the node renders a bundle with (Docker's defaults, `hex_core::bundle`). A spec
/// naming none keeps its creator's, CAP_SYS_ADMIN among them, which a rootless box on the host's
/// network may not hold (spec.rs refuses it).
fn capabilities() -> String {
    let set = format!("{:?}", hex_core::bundle::CAPABILITIES);
    format!(r#"{{"bounding": {set}, "effective": {set}, "permitted": {set}}}"#)
}

/// A rootfs of the host's `/usr`, bound in by each bundle, with the host's top-level links into it.
fn build_rootfs(rootfs: &Path) {
    for d in ["proc", "dev", "tmp", "usr"] {
        std::fs::create_dir_all(rootfs.join(d)).unwrap();
    }
    std::fs::File::create(rootfs.join("hex")).unwrap();
    for link in ["bin", "sbin", "lib", "lib64"] {
        if let Ok(target) = std::fs::read_link(Path::new("/").join(link)) {
            let target = target.to_string_lossy().trim_start_matches('/').to_owned();
            std::os::unix::fs::symlink(target, rootfs.join(link)).unwrap();
        }
    }
}

fn host_id(flag: &str) -> u32 {
    let out = Command::new("id").arg(flag).output().expect("id");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("id is a number")
}

fn own_cgroup() -> PathBuf {
    let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
    let rel = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("a 0:: cgroup line")
        .trim_start_matches('/');
    PathBuf::from("/sys/fs/cgroup").join(rel)
}

/// Moves this process into a leaf of its own cgroup and confirms every controller is delegated, as
/// oci_box.rs does; the agent this test starts inherits the leaf, and makes its boxes beside it.
fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
    let own = own_cgroup();
    let leaf = own.join("hex-agent");
    std::fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf for the agent: {e}"))?;
    std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
        .map_err(|e| format!("moving self into a leaf: {e}"))?;
    // The probe root opening proves every controller a box uses is delegated here: `BoxRoot::open`
    // refuses by name otherwise (HEX-INST-1).
    hex_runtime::linux::cgroup::BoxRoot::open(
        &own.join("hex-probe"),
        hex_runtime::linux::disks::under(disk).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir(own.join("hex-probe"));
    Ok(own)
}

fn cache_dir() -> PathBuf {
    let cache =
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/socklab");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
