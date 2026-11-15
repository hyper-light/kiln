//! HEX-QUIC-20 (CP-T7.3; the joint plan's step 4; docs/design/hex-quic.md): the hex-api conformance suite, every
//! case, through the node's QUIC listener. A node in this process serves its `process` boxes over its local socket
//! and over hyper-transport on loopback, through one server ([`hex_node::quic::Quic`]); a client enrolled with the
//! simulator CA reaches it through [`hex_quic::link`] alone, the node agent's guest client over QUIC connections
//! (control, a program's stdio streams); and both ends of their connection report `TLS13_AES_256_GCM_SHA384`. The
//! programs are `hex selftest-box` acts, as the class's own conformance tests run them.
//!
//! HEX-QUIC-23 and -24 (Q9): the same through `hex node` itself, a process of its own: configured with `--quic` and
//! the enrollment `hex node enroll` began and the simulator CA answered, it serves the suite over QUIC; without
//! `--quic` it binds no UDP socket, and asked for QUIC without an enrollment it can serve with, it refuses, typed,
//! with nothing bound.
//!
//! HEX-QUIC-25 ([`two_hosts`]): the same across two hosts, in phases the lead runs (every other run ignores them). The
//! node's host and the client's host share one simulator CA by request and answer alone. The node is killed mid-suite
//! and restarted, and its UDP port is dropped and let through again.
//!
//! Linux needs a delegated cgroup v2 subtree for the class, as `process_box.rs` does, re-running a test once in a
//! `systemd-run --user --scope -p Delegate=yes` where its own isn't one, and recording the skip where neither is.
//! macOS and Windows run their `process` class rootless.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio as Std};
use std::sync::{Arc, Mutex};

use hex_api::conformance::{self, Act, Fixture};
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Program};
use hex_node::quic::{Bounds as QuicBounds, Quic};
use hex_node::serve::{Host, Limits, Native, Server};
use hex_quic::enroll::sim::SimCa;
use hex_quic::enroll::{Ca, store};
use hex_quic::{Config, Endpoint, Shape, enroll, peer_id};

struct BinFixture {
    exe: String,
    /// The node's host's bound on a program's arguments and environment: this host's for a node here, the node's host's
    /// in the two-host run ([`two_hosts`]).
    limit: hex_api::ExecLimit,
    /// Whether the node's host counts a box's refused forks.
    forks_counted: bool,
}

impl Fixture for BinFixture {
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds,
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        }
    }

    fn init(&self, id: BoxId, _act: &Act) -> BoxSpec {
        self.spec(id, Bounds::default())
    }

    fn program(&self, act: &Act) -> Program {
        let mut args = vec![self.exe.clone(), "selftest-box".to_owned()];
        match act {
            Act::Say(words) => args.extend(["say".to_owned(), (*words).to_owned()]),
            Act::Fail(code) => args.extend(["fail".to_owned(), code.to_string()]),
            Act::Wait => args.push("wait".to_owned()),
            Act::Hog(bytes) => args.extend(["hog".to_owned(), bytes.to_string()]),
            Act::Spawn(n) => args.extend(["spawn".to_owned(), n.to_string()]),
            Act::Echo => args.push("echo".to_owned()),
        }
        Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn refused_forks_counted(&self) -> bool {
        self.forks_counted
    }

    fn exec_limit(&self) -> Option<hex_api::ExecLimit> {
        Some(self.limit)
    }
}

/// The suite's programs as this host runs them.
fn fixture() -> BinFixture {
    BinFixture {
        exe: env!("CARGO_BIN_EXE_hex").to_owned(),
        limit: this_hosts_limit(),
        // Windows' job object counts no member's own refused fork (windows_process_box.rs, Ada's call 2026-11-03).
        forks_counted: !cfg!(windows),
    }
}

/// This host's bound on a program's arguments and environment, as its `process` class reads it.
fn this_hosts_limit() -> hex_api::ExecLimit {
    #[cfg(target_os = "linux")]
    return hex_runtime::linux::process::exec_limit();
    #[cfg(target_os = "macos")]
    return hex_runtime::macos::exec_limit().expect("kern.argmax");
    #[cfg(windows)]
    return hex_core::hcs::exec_limit();
}

/// A runtime served as a node's host, in this process: macOS's and Windows' `process` class, and on every OS the
/// simulator's (HEX-QUIC-27).
mod hosted {
    use hex_api::{
        BackingGrant, BackingKey, BackingOffer, BoxId, BoxInfo, BoxSpec, Capacity, Class, Proc,
        Program, Refusal, Runtime, Signal, Stdio, Usage,
    };
    use hex_core::netns;
    use hex_node::serve::Host;

    /// A runtime served as a node's host, its abandoned programs counted by the runtime's own inspect.
    pub struct Hosted<R>(pub R);

    impl<R: Runtime> Runtime for Hosted<R> {
        fn backing_usage(&self, key: &BackingKey) -> Result<Usage, Refusal> {
            self.0.backing_usage(key)
        }
        fn initialize_backing(&self, grant: BackingGrant) -> Result<BackingOffer, Refusal> {
            self.0.initialize_backing(grant)
        }
        fn release_backing(&self, key: BackingKey) -> Result<BackingOffer, Refusal> {
            self.0.release_backing(key)
        }
        fn classes(&self) -> Vec<Class> {
            self.0.classes()
        }
        fn physical_report(&self) -> Result<hex_api::PhysicalReport, Refusal> {
            self.0.physical_report()
        }
        fn capacity(&self, class: Class) -> Result<Option<Capacity>, Refusal> {
            self.0.capacity(class)
        }
        fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
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
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.0.exec(id, program, stdio)
        }
        fn run(
            &self,
            spec: BoxSpec,
            program: Program,
            stdio: Stdio,
        ) -> Result<Box<dyn Proc>, Refusal> {
            self.0.run(spec, program, stdio)
        }
        fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
            self.0.signal(id, signal)
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
            self.0.inspect(id)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
            self.0.usage(id)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
            self.0.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
            self.0.delete(id)
        }
        fn member(&self, id: &BoxId, member: &hex_api::Member) -> Result<Box<dyn Proc>, Refusal> {
            self.0.member(id, member)
        }
        fn signal_member(
            &self,
            id: &BoxId,
            member: &hex_api::Member,
            signal: Signal,
        ) -> Result<(), Refusal> {
            self.0.signal_member(id, member, signal)
        }
    }

    impl<R: Runtime> Host for Hosted<R> {
        fn abandoned(&self, _id: &BoxId) -> Result<(), Refusal> {
            Ok(())
        }
        fn netns(&self) -> Result<netns::Counts, Refusal> {
            Ok(netns::Counts::default())
        }
    }
}

/// The server's bounds for this test: a node's few connections, and as many calls and programs as the suite runs at
/// once.
fn limits() -> Limits {
    Limits {
        connections: 8,
        message_bytes: hex_core::wire::MAX_MESSAGE,
        in_flight: 16,
        programs: 64,
    }
}

/// A folder of the test's own: under ~/.cache on Unix (the hazards' rule: never /tmp), the system's temp folder on
/// Windows, as Hex's Windows tests take.
fn scratch(prefix: &str) -> tempfile::TempDir {
    #[cfg(unix)]
    {
        let cache =
            PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/quic");
        std::fs::create_dir_all(&cache).expect("cache dir");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&cache)
            .unwrap()
    }
    #[cfg(windows)]
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

/// HEX-QUIC-20's body over `host`: the node served over QUIC, the suite through it, and the suite on both ends.
fn exercise(host: Arc<dyn Host>) {
    let ca = SimCa::new().unwrap();
    let enrolled = |name: &str| {
        ca.admit(name, name.as_bytes());
        enroll::enroll(&ca, name, name.as_bytes().to_vec()).unwrap()
    };
    let limits = limits();
    // A Linux node stages file trees over QUIC; a macOS or Windows one stages none, on any transport.
    let staging = cfg!(target_os = "linux");
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled("node.quic.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: hex_node::quic::ports(staging),
            shape: hex_node::quic::shape(&limits, staging),
        })
        .unwrap(),
    );
    let address = node.local_addr();
    let dir = scratch("q-");
    let socket = dir.path().join("node.sock");
    let staged = if staging {
        Some(dir.path().join("staged"))
    } else {
        None
    };
    let quic = Quic::new(
        Native::bind(&socket).unwrap(),
        Arc::clone(&node),
        QuicBounds::of(&limits),
        staged,
    )
    .unwrap();
    let server = Server::with_os(quic, host, limits, limits.in_flight, &socket).unwrap();
    let client_end = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled("client.quic.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: hex_quic::limits::client_lanes(limits.in_flight, staging),
            },
        })
        .unwrap(),
    );
    // No deadline of the test's own: the dial until the driver tells it, the hello on its kind's learned wait
    // (`client.quic.answer.hello`, following the node until its first answer: decision 9).
    let (client, peer) = hex_quic::link::connect(
        Arc::clone(&client_end),
        "node.quic.test",
        address,
        staging,
        None,
    )
    .unwrap();
    let aes = hex_quic::CipherSuite::TLS13_AES_256_GCM_SHA384;
    assert_eq!(client_end.negotiated(peer).unwrap().unwrap().0, aes);
    assert_eq!(
        node.negotiated(peer_id("client.quic.test"))
            .unwrap()
            .unwrap()
            .0,
        aes
    );
    let failures = conformance::run(&client, &fixture());
    assert!(failures.is_empty(), "through QUIC: {failures:#?}");
    drop(client);
    drop(server);
}

#[cfg(target_os = "macos")]
#[test]
fn hex_quic_20_the_conformance_suite_passes_over_quic() {
    let model = hex_runtime::macos::machine_model().expect("this Mac's model");
    let rates = hex_core::watch::seed(&model);
    let runtime = hex_runtime::macos::MacRuntime::new(
        rates.bytes,
        rates.processes,
        hex_core::watch::lateness_seed(&model),
    )
    .expect("a macOS runtime");
    exercise(Arc::new(hosted::Hosted(runtime)));
}

#[cfg(windows)]
#[test]
fn hex_quic_20_the_conformance_suite_passes_over_quic() {
    exercise(Arc::new(hosted::Hosted(
        hex_runtime::windows::WinRuntime::new(),
    )));
}

/// HEX-QUIC-27 (Q8; joint plan step 4): a client holds itself to the lanes its node grants a peer, whatever the node's
/// connections bound. Do: serve a simulator node over QUIC whose connections bound (64) is past what it grants one peer
/// (its calls in flight, 2: a control and a stdio connection each, and its log's: 5 lanes, `hex_node::quic::shape`);
/// connect a client (`link::connect`), then beside its control connection open stdio connections up to the grant, and
/// one more. Expect the one past the grant refused here, typed (`Why::Lanes`), and the client's calls still answered:
/// a connection the node's stream limit can't take is never opened, so none waits for a lane that never comes.
#[test]
fn hex_quic_27_a_client_holds_itself_to_the_lanes_its_node_grants() {
    use hex_api::Runtime as _;
    let ca = SimCa::new().unwrap();
    let enrolled = |name: &str| {
        ca.admit(name, name.as_bytes());
        enroll::enroll(&ca, name, name.as_bytes().to_vec()).unwrap()
    };
    let limits = Limits {
        connections: 64,
        message_bytes: hex_core::wire::MAX_MESSAGE,
        in_flight: 2,
        programs: 4,
    };
    let grant = hex_node::quic::shape(&limits, false).lanes;
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled("node.lanes.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: hex_node::quic::ports(false),
            shape: hex_node::quic::shape(&limits, false),
        })
        .unwrap(),
    );
    let dir = scratch("ln-");
    let socket = dir.path().join("node.sock");
    let quic = Quic::new(
        Native::bind(&socket).unwrap(),
        Arc::clone(&node),
        QuicBounds::of(&limits),
        None,
    )
    .unwrap();
    let host: Arc<dyn Host> = Arc::new(hosted::Hosted(hex_api::sim::SimRuntime::new()));
    let server = Server::with_os(quic, host, limits, limits.in_flight, &socket).unwrap();
    let client_end = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled("client.lanes.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: hex_quic::limits::client_lanes(limits.in_flight, false),
            },
        })
        .unwrap(),
    );
    assert_eq!(
        hex_quic::limits::client_lanes(limits.in_flight, false),
        grant,
        "the lanes the client asks are its node's grant: one derivation"
    );
    let (client, peer) = hex_quic::link::connect(
        Arc::clone(&client_end),
        "node.lanes.test",
        node.local_addr(),
        false,
        None,
    )
    .unwrap();
    // Its control connection holds one of the grant's lanes.
    let held: Vec<_> = (1..grant)
        .map(|_| client_end.open(peer, hex_core::stream::STDIO_PORT).unwrap())
        .collect();
    let past = client_end.open(peer, hex_core::stream::STDIO_PORT);
    assert!(
        matches!(
            past,
            Err(hex_quic::Refused::Conn {
                why: hex_quic::Why::Lanes
            })
        ),
        "an open past the node's grant of {grant}: {past:?}"
    );
    drop(held);
    let listed = client.list();
    assert!(listed.is_ok(), "a call after the refused open: {listed:?}");
    drop(client);
    drop(server);
}

/// A simulator node served over QUIC in this process, a client of its own reaching it, and, made and started through
/// that client, a Shards image VM whose spec names `claude`, a harness its caller drives (joint plan steps 12 and 17):
/// what the member tests over QUIC share. Its fields drop in order: the client before the node it reaches.
struct MemberVm {
    client: hex_quic::link::Client,
    id: BoxId,
    claude: hex_api::Member,
    _server: Server<Quic<Native>>,
    _client_end: Arc<Endpoint>,
    _node: Arc<Endpoint>,
    _dir: tempfile::TempDir,
}

/// [`MemberVm`]: its node named `name`, its box `id`.
fn member_vm(name: &str, id: &str) -> MemberVm {
    use hex_api::Runtime as _;
    let ca = SimCa::new().unwrap();
    let enrolled = |name: &str| {
        ca.admit(name, name.as_bytes());
        enroll::enroll(&ca, name, name.as_bytes().to_vec()).unwrap()
    };
    let limits = limits();
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(name),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: hex_node::quic::ports(false),
            shape: hex_node::quic::shape(&limits, false),
        })
        .unwrap(),
    );
    let dir = scratch("mb-");
    let socket = dir.path().join("node.sock");
    let quic = Quic::new(
        Native::bind(&socket).unwrap(),
        Arc::clone(&node),
        QuicBounds::of(&limits),
        None,
    )
    .unwrap();
    let host: Arc<dyn Host> = Arc::new(hosted::Hosted(hex_api::sim::SimRuntime::new()));
    let server = Server::with_os(quic, host, limits, limits.in_flight, &socket).unwrap();
    let client_end = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&format!("client.{name}")),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: hex_quic::limits::client_lanes(limits.in_flight, false),
            },
        })
        .unwrap(),
    );
    let (client, _peer) = hex_quic::link::connect(
        Arc::clone(&client_end),
        name,
        node.local_addr(),
        false,
        None,
    )
    .unwrap();
    let claude = hex_api::Member {
        kind: hex_api::MemberKind::Harness,
        name: "claude".to_owned(),
    };
    let id = client
        .create(BoxSpec {
            id: BoxId::new(id).unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage {
                members: vec![hex_api::ImageMember::new(claude.clone())],
                ..hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE)
            })),
            owner: None,
            allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
            route: None,
            devices: Vec::new(),
            band: 0,
        })
        .unwrap()
        .id;
    client.start(&id).unwrap();
    MemberVm {
        client,
        id,
        claude,
        _server: server,
        _client_end: client_end,
        _node: node,
        _dir: dir,
    }
}

/// Joint plan steps 12 and 17's remote callers, AC-T7.4.1 (docs/design/shards-image-class.md §2): a member of a Shards
/// image VM that its caller drives is handed over through QUIC as through the node's socket. A QUIC client makes the
/// streams it asks for itself (lanes), so it carries the member's stdin, stdout and stderr, and the node joins each to
/// the member's own. Do: through a simulator node served over QUIC in this process, create and start a VM whose spec
/// names a member its caller drives, take that member, write its stdin and close it, read its stdout to its end, and
/// wait for its end as it comes. Expect: its stdout the bytes written, and its end Code(0).
#[test]
fn a_member_is_handed_over_through_quic() {
    use hex_api::Runtime as _;
    use std::io::{Read as _, Write as _};
    let vm = member_vm("node.member.test", "member-over-quic");
    let mut member = vm.client.member(&vm.id, &vm.claude).unwrap();
    let given = member.streams().unwrap();
    let mut stdin = hex_api::OwnedStream::from(given.stdin.unwrap());
    let mut stdout = hex_api::OwnedStream::from(given.stdout.unwrap());
    stdin.write_all(b"a session's line over QUIC\n").unwrap();
    drop(stdin);
    let mut echoed = String::new();
    stdout.read_to_string(&mut echoed).unwrap();
    assert_eq!(echoed, "a session's line over QUIC\n");
    assert_eq!(
        member.wait_for(std::time::Duration::MAX).unwrap(),
        Some(hex_api::Exit::Code(0))
    );
    drop(member);
    vm.client.delete(&vm.id).unwrap();
}

/// Goal 2's item 2 (ROOT, 2026-11-14: the node's wire carries a member's signal): a member handed over through QUIC is
/// killed as in process, KILL on its own lane, the member's alone: the node's host signals it, no longer refused
/// `Awaits(MemberSignal)`. Do: take a member of a running image VM through a simulator node served over QUIC, its
/// streams held open, and kill it. Expect: the kill done; its end Signal(9); its VM running on.
#[test]
fn w2_a_member_handed_over_through_quic_is_killed_alone() {
    use hex_api::Runtime as _;
    let vm = member_vm("node.member-kill.test", "member-killed-over-quic");
    let mut member = vm.client.member(&vm.id, &vm.claude).unwrap();
    let given = member.streams().unwrap();
    assert_eq!(member.kill(), Ok(()));
    assert_eq!(
        member.wait_for(std::time::Duration::MAX).unwrap(),
        Some(hex_api::Exit::Signal(9))
    );
    assert_eq!(
        vm.client.inspect(&vm.id).unwrap().state,
        hex_api::BoxState::Running
    );
    drop((member, given));
    vm.client.delete(&vm.id).unwrap();
}

/// Goal 2's item 2 (ROOT, 2026-11-14): the node's wire carries a member's own signal (`Call::MemberSignal`, appended),
/// so a caller through QUIC signals a member as one in process does, and its refusals come typed as they are made. Do:
/// take a member of a running image VM through a simulator node served over QUIC, its streams held open; send it TERM;
/// send it TERM again once it has ended; send TERM to a member its run declares not. Expect: the first done, the
/// member's end Signal(15), as its echo keeps no handler, and its VM running on; the second refused NotRunning, "signal
/// a member: it has ended"; the third refused Invalid, of the member.
#[test]
fn w2_a_members_signal_goes_through_quic() {
    use hex_api::Runtime as _;
    let vm = member_vm("node.member-signal.test", "member-signalled-over-quic");
    let mut member = vm.client.member(&vm.id, &vm.claude).unwrap();
    let given = member.streams().unwrap();
    assert_eq!(
        vm.client
            .signal_member(&vm.id, &vm.claude, hex_api::Signal::TERM),
        Ok(())
    );
    assert_eq!(
        member.wait_for(std::time::Duration::MAX).unwrap(),
        Some(hex_api::Exit::Signal(15))
    );
    assert_eq!(
        vm.client.inspect(&vm.id).unwrap().state,
        hex_api::BoxState::Running
    );
    assert_eq!(
        vm.client
            .signal_member(&vm.id, &vm.claude, hex_api::Signal::TERM),
        Err(hex_api::Refusal::NotRunning(
            vm.id.clone(),
            "signal a member: it has ended"
        ))
    );
    let helper = hex_api::Member {
        kind: hex_api::MemberKind::Agent,
        name: "helper".to_owned(),
    };
    let undeclared = vm
        .client
        .signal_member(&vm.id, &helper, hex_api::Signal::TERM);
    assert!(
        matches!(
            &undeclared,
            Err(hex_api::Refusal::Invalid { what: "member", .. })
        ),
        "{undeclared:?}"
    );
    drop((member, given));
    vm.client.delete(&vm.id).unwrap();
}

/// Goal 2 (ROOT, 2026-11-14: the node's wire carries `started`): a remote caller follows a VM's start through `started`
/// as a caller in the node's own process does, waiting on the fact, never looking. Do: through a simulator node served
/// over QUIC in this process, ask `started` of a VM it started, and of an id it holds no box of. Expect: the VM's info,
/// Running; and NoSuchBox for the other.
#[test]
fn w2_a_vms_start_is_followed_through_started_over_quic() {
    use hex_api::Runtime as _;
    let vm = member_vm("node.started.test", "started-over-quic");
    let started = vm.client.started(&vm.id).unwrap();
    assert_eq!(
        (started.id, started.state),
        (vm.id.clone(), hex_api::BoxState::Running)
    );
    let none = BoxId::new("started-over-quic-none").unwrap();
    assert_eq!(
        vm.client.started(&none),
        Err(hex_api::Refusal::NoSuchBox(none.clone()))
    );
    vm.client.delete(&vm.id).unwrap();
}

/// The calls in flight the node named `name` at `address` runs at once, as its hello says (its budget): how a client that
/// doesn't know its node learns it before it shapes its own endpoint (`hex_quic::limits::client_lanes`), over an
/// endpoint of `credentials` bound at `bind`, shaped for that hello alone (its control connection's one lane).
fn node_in_flight(
    credentials: hex_quic::tls::Credentials,
    bind: SocketAddr,
    name: &str,
    address: SocketAddr,
) -> usize {
    let probe = Arc::new(
        Endpoint::start(Config {
            credentials,
            bind,
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: hex_quic::limits::client_lanes(0, false),
            },
        })
        .unwrap(),
    );
    let (client, _) = hex_quic::link::connect(probe, name, address, false, None).unwrap();
    client.budgets().in_flight
}

/// The `hex` binary of this build.
fn hex() -> &'static str {
    env!("CARGO_BIN_EXE_hex")
}

/// A `hex node` process over a folder of the test's, killed when dropped (on Windows with its keeper, which outlives
/// a killed node by design), its stderr kept as it comes.
struct NodeProcess {
    child: Child,
    said: Arc<Mutex<String>>,
    #[cfg(windows)]
    keeping: PathBuf,
}

impl NodeProcess {
    /// `hex node` over `dir`'s socket and state with `more` arguments, its boxes under `box_root` where given (Linux);
    /// and the lines it says on stdout until its `ready`.
    fn start(
        dir: &Path,
        box_root: Option<&Path>,
        more: &[&std::ffi::OsStr],
    ) -> (Self, Vec<String>) {
        let mut command = Command::new(hex());
        command
            .arg("node")
            .arg("--socket")
            .arg(dir.join("node.sock"))
            .arg("--state")
            .arg(dir.join("state"));
        if let Some(root) = box_root {
            command.arg("--box-root").arg(root);
        }
        let mut child = command
            .args(more)
            .stdout(Std::piped())
            .stderr(Std::piped())
            .spawn()
            .expect("start hex node");
        let said = Arc::new(Mutex::new(String::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&said);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                sink.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if out.read_line(&mut line).unwrap() == 0 {
                let ended = child.wait().unwrap();
                panic!(
                    "hex node ended {ended} having said {lines:?}: {}",
                    said.lock().unwrap()
                );
            }
            let line = line.trim_end().to_owned();
            let ready = line.starts_with("ready ");
            lines.push(line);
            if ready {
                break;
            }
        }
        // Nothing more comes on stdout; what it says on stderr is kept.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut out, &mut std::io::sink());
        });
        let node = Self {
            child,
            said,
            #[cfg(windows)]
            keeping: dir.join("state").join("keeper"),
        };
        (node, lines)
    }

    fn said(&self) -> String {
        self.said.lock().unwrap().clone()
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The keeper its `keeper.owner` names is asked to stop, as a node going down cleanly does (windows_node.rs).
        #[cfg(windows)]
        if let Ok(text) = std::fs::read_to_string(self.keeping.join("keeper.owner")) {
            let mut words = text.split_whitespace();
            if let (Some(Ok(pid)), Some(Ok(created))) = (
                words.next().map(str::parse::<u32>),
                words.next().map(str::parse::<u64>),
            ) && let Ok(keeper) =
                hex_runtime::windows::keeper::NodeKeeper::open(pid, created, &self.keeping)
            {
                keeper.stop();
            }
        }
    }
}

/// The UDP sockets process `pid` holds: on Linux its descriptors' sockets that its namespace's `/proc/<pid>/net/udp`
/// and `udp6` list (proc(5)); on macOS what lsof(8) lists; on Windows what `netstat -ano` lists for it.
fn udp_sockets(pid: u32) -> usize {
    #[cfg(target_os = "linux")]
    {
        let inodes: std::collections::BTreeSet<String> =
            std::fs::read_dir(format!("/proc/{pid}/fd"))
                .unwrap()
                .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
                .filter_map(|link| {
                    link.to_str()?
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')
                        .map(str::to_owned)
                })
                .collect();
        ["udp", "udp6"]
            .iter()
            .map(|table| {
                std::fs::read_to_string(format!("/proc/{pid}/net/{table}"))
                    .unwrap_or_default()
                    .lines()
                    .skip(1)
                    .filter(|row| {
                        row.split_whitespace()
                            .nth(9)
                            .is_some_and(|inode| inodes.contains(inode))
                    })
                    .count()
            })
            .sum()
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("lsof")
            .args(["-nP", "-a", "-p", &pid.to_string(), "-i", "UDP", "-F", "n"])
            .output()
            .expect("lsof");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.starts_with('n'))
            .count()
    }
    #[cfg(windows)]
    {
        let pid = pid.to_string();
        ["UDP", "UDPv6"]
            .iter()
            .map(|protocol| {
                let out = Command::new("netstat")
                    .args(["-ano", "-p", protocol])
                    .output()
                    .expect("netstat");
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|row| {
                        row.trim_start().starts_with("UDP")
                            && row.split_whitespace().last() == Some(pid.as_str())
                    })
                    .count()
            })
            .sum()
    }
}

/// HEX-QUIC-23's body: `hex node enroll` begins the node's enrollment, the simulator CA answers its request as kiln's
/// CA would, and `hex node --quic` serves the suite over QUIC, at the address it says before its `ready`, holding the
/// UDP socket it binds, both ends of a client's connection at TLS13_AES_256_GCM_SHA384: the client's from its endpoint,
/// the node's as `hex node links` says it (Q12, HEX-QUIC-31).
fn configured(box_root: Option<&Path>) {
    let dir = scratch("cli-");
    let credentials = dir.path().join("credentials");
    let begun = Command::new(hex())
        .args(["node", "enroll", "--name", "node.cli.test", "--credentials"])
        .arg(&credentials)
        .output()
        .expect("hex node enroll");
    assert!(
        begun.status.success(),
        "{}",
        String::from_utf8_lossy(&begun.stderr)
    );
    assert_eq!(
        String::from_utf8(begun.stdout).unwrap(),
        format!("request {}\n", credentials.join(store::REQUEST).display())
    );
    let ca = SimCa::new().unwrap();
    ca.admit("node.cli.test", b"grant");
    let issued = ca
        .enroll(&store::request(&credentials, b"grant".to_vec()).unwrap())
        .unwrap();
    store::finish(&credentials, &issued).unwrap();
    let (node, said) = NodeProcess::start(
        dir.path(),
        box_root,
        &[
            "--quic".as_ref(),
            "127.0.0.1:0".as_ref(),
            "--credentials".as_ref(),
            credentials.as_os_str(),
        ],
    );
    let [quic, ready] = said.as_slice() else {
        panic!("hex node said {said:?}");
    };
    let address: SocketAddr = quic
        .strip_prefix("quic ")
        .unwrap_or_else(|| panic!("{quic:?}"))
        .parse()
        .unwrap();
    assert_eq!(
        ready,
        &format!("ready {}", dir.path().join("node.sock").display())
    );
    assert!(
        udp_sockets(node.child.id()) >= 1,
        "a configured hex node holds no UDP socket"
    );
    // This node stages nothing (no --staged), and says its calls in flight to a client's hello.
    ca.admit("probe.cli.test", b"probe");
    let in_flight = node_in_flight(
        enroll::enroll(&ca, "probe.cli.test", b"probe".to_vec()).unwrap(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        "node.cli.test",
        address,
    );
    ca.admit("client.cli.test", b"client");
    let client_end = Arc::new(
        Endpoint::start(Config {
            credentials: enroll::enroll(&ca, "client.cli.test", b"client".to_vec()).unwrap(),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: hex_quic::limits::client_lanes(in_flight, false),
            },
        })
        .unwrap(),
    );
    // As HEX-QUIC-20's: the dial and the hello each on their own wait.
    let (client, peer) = hex_quic::link::connect(
        Arc::clone(&client_end),
        "node.cli.test",
        address,
        false,
        None,
    )
    .unwrap();
    assert_eq!(
        client_end.negotiated(peer).unwrap().unwrap().0,
        hex_quic::CipherSuite::TLS13_AES_256_GCM_SHA384
    );
    let asked = Command::new(hex())
        .args(["node", "links", "--socket"])
        .arg(dir.path().join("node.sock"))
        .output()
        .expect("hex node links");
    assert!(
        asked.status.success(),
        "{}",
        String::from_utf8_lossy(&asked.stderr)
    );
    // Every link the node reports is at AES-256-GCM under a hybrid group, its client's among them: the probe's may
    // be reported still, its close not yet come.
    let said = String::from_utf8(asked.stdout).unwrap();
    let peer = |line: &str| {
        ["secp384r1MLKEM1024", "X25519MLKEM768", "secp256r1MLKEM768"]
            .iter()
            .find_map(|group| {
                line.strip_suffix(&format!(" suite TLS13_AES_256_GCM_SHA384 group {group}"))?
                    .strip_prefix("link ")
            })
            .map(str::to_owned)
    };
    let peers: Vec<Option<String>> = said.lines().map(peer).collect();
    assert!(
        peers.iter().all(Option::is_some) && peers.contains(&Some("client.cli.test".to_owned())),
        "hex node links said {said:?}"
    );
    let failures = conformance::run(&client, &fixture());
    assert!(
        failures.is_empty(),
        "through hex node's QUIC listener: {failures:#?}\n{}",
        node.said()
    );
}

/// HEX-QUIC-24's body: without `--quic`, `hex node` says only its `ready` and holds no UDP socket; asked for QUIC
/// without its enrollment, or with one kiln's CA hasn't answered, it refuses, typed, naming what is missing, before it
/// makes its state folder or binds its socket.
fn unconfigured(box_root: Option<&Path>) {
    let dir = scratch("cli-");
    let (node, said) = NodeProcess::start(dir.path(), box_root, &[]);
    assert_eq!(
        said,
        vec![format!("ready {}", dir.path().join("node.sock").display())]
    );
    assert_eq!(
        udp_sockets(node.child.id()),
        0,
        "an unconfigured hex node holds a UDP socket"
    );
    drop(node);
    let half = scratch("cli-");
    let node = |more: &[&std::ffi::OsStr]| {
        Command::new(hex())
            .arg("node")
            .arg("--socket")
            .arg(half.path().join("node.sock"))
            .arg("--state")
            .arg(half.path().join("state"))
            .args(more)
            .output()
            .expect("hex node")
    };
    let refused = node(&["--quic".as_ref(), "127.0.0.1:0".as_ref()]);
    assert_eq!(refused.status.code(), Some(2));
    let why = String::from_utf8_lossy(&refused.stderr);
    assert!(why.contains("--quic needs --credentials"), "{why}");
    let credentials = half.path().join("credentials");
    let begun = Command::new(hex())
        .args([
            "node",
            "enroll",
            "--name",
            "node.unanswered.test",
            "--credentials",
        ])
        .arg(&credentials)
        .output()
        .expect("hex node enroll");
    assert!(begun.status.success());
    let unanswered = node(&[
        "--quic".as_ref(),
        "127.0.0.1:0".as_ref(),
        "--credentials".as_ref(),
        credentials.as_os_str(),
    ]);
    assert_eq!(unanswered.status.code(), Some(1));
    let why = String::from_utf8_lossy(&unanswered.stderr);
    assert!(
        why.contains(store::CHAIN) && why.contains("hex node enroll"),
        "{why}"
    );
    assert!(
        !half.path().join("node.sock").exists(),
        "a socket was bound"
    );
    assert!(!half.path().join("state").exists(), "the node opened");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn hex_quic_23_a_configured_hex_node_serves_the_suite_over_quic() {
    configured(None);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn hex_quic_24_without_its_enrollment_hex_node_binds_no_quic_and_refuses_typed() {
    unconfigured(None);
}

/// HEX-QUIC-25 (the joint plan's step 4; docs/design/hex-quic.md Q6, Q7, Q9): the two-host run, which is the lead's
/// to run. A `hex node` on host A serves the suite over QUIC to a client on host B. Both are enrolled under one
/// simulator CA, which lives only in B's phase B1: A's request goes to B, and the chain and roots come back. Every
/// private key stays on the host that made it: the node's in A's `hex node enroll`, the client's in B's
/// `store::begin`, and the CA's in B1's process, which ends. Then A's node is killed mid-suite and restarted, and its
/// UDP port is dropped and let through again. Each loss is refused by type, and nothing is left behind.
///
/// Each phase is a test every other run ignores. It runs by name on its host:
/// `cargo test --locked -p hex-cli --test quic_conformance -- --ignored --nocapture --test-threads=1 hex_quic_25_<phase>`.
/// Its inputs come from the environment. Its outputs go to stdout as `HEX_2H_<NAME>=<value>` lines for the next
/// phase. What travels between the hosts is public and small (a request and certificates, hex DER, under 2 KiB
/// each), so it goes in the environment. In order:
/// 1. `a1`, on A: takes `HEX_2H_DIR` (a fresh folder per run) and `HEX_2H_NODE` (the node's name). Says
///    `HEX_2H_REQUEST`, `HEX_2H_NODE_HEX`, `HEX_2H_NODE_LIMIT` and `HEX_2H_NODE_FORKS`.
/// 2. `b1`, on B: takes `HEX_2H_DIR`, `HEX_2H_NODE` and `HEX_2H_REQUEST`. Says `HEX_2H_CHAIN` and `HEX_2H_ROOTS`.
/// 3. `a2`, on A: takes `HEX_2H_DIR`, `HEX_2H_CHAIN` and `HEX_2H_ROOTS`.
/// 4. `a3`, on A (Linux), in the background: takes `HEX_2H_DIR` and `HEX_2H_BIND` (`<A's address>:<port>`). Says
///    `HEX_2H_NODE_PID` and `HEX_2H_NODE_QUIC` once a start is ready, and again after a KILL ends it and it starts
///    over.
/// 5. `b2`, on B: takes `HEX_2H_DIR`, `HEX_2H_NODE`, `HEX_2H_ADDR` (A's `HEX_2H_BIND`) and A1's three
///    `HEX_2H_NODE_*` lines.
/// 6. `b3` with `HEX_2H_LOSS=kill`, on B, in the background: once it says `HEX_2H_READY`, `kill -9` the node's
///    pid on A. It says `HEX_2H_LOST`, and A3 starts the node again.
/// 7. `b4` with `HEX_2H_LOSS=kill`, on B.
/// 8. Phases 6 and 7 with `HEX_2H_LOSS=partition`: drop the node's UDP port on A once B3 says `HEX_2H_READY`,
///    and let it through again once B3 says `HEX_2H_LOST`, before B4.
/// 9. `kill -TERM` the node's pid on A: A3 says `HEX_2H_LEFT=none` and passes.
mod two_hosts {
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use hex_api::conformance::{self, Act, Fixture};
    use hex_api::{BlockCount, Bounds, BoxId, ExecLimit, Refusal, Runtime, Stdio, StringUnit};
    use hex_quic::enroll::sim::SimCa;
    use hex_quic::enroll::{Ca, Issued, Request, store};
    use hex_quic::{CipherSuite, Config, Endpoint, Refused, Shape};

    use super::{BinFixture, hex};

    /// The grants the simulator CA admits the node and the client with, each taken once (HEX-ENROLL-5).
    const NODE_GRANT: &[u8] = b"hex-quic-25 node";
    const CLIENT_GRANT: &[u8] = b"hex-quic-25 client";
    /// The client's name: B's peer.
    const CLIENT: &str = "client.two-hosts.test";
    /// The box whose waiting program sees a loss (B3), and which B4 finds afterwards.
    const LOSS_BOX: &str = "two-hosts-loss";

    /// The run's variable `name`, which the phase needs.
    fn var(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("HEX-QUIC-25's phase needs {name}"))
    }

    /// This host's folder for the run (`HEX_2H_DIR`, a fresh one each run: an enrollment begins once).
    fn dir() -> PathBuf {
        let dir = PathBuf::from(var("HEX_2H_DIR"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Says `name=value` for the next phase, at once, on a line of its own: libtest's `test <name> ... ` comes first
    /// on the line it begins.
    fn say(name: &str, value: impl std::fmt::Display) {
        let mut out = std::io::stdout().lock();
        writeln!(out, "\n{name}={value}").unwrap();
        out.flush().unwrap();
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn from_hex(text: &str) -> Vec<u8> {
        let text = text.trim().as_bytes();
        assert!(text.len().is_multiple_of(2), "hex of an odd length");
        text.chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Certificates as the run carries them: each one's DER in hex, joined by commas.
    fn certs_hex<C: AsRef<[u8]>>(certs: &[C]) -> String {
        certs
            .iter()
            .map(|c| to_hex(c.as_ref()))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn certs_der(text: &str) -> Vec<Vec<u8>> {
        text.split(',')
            .filter(|c| !c.is_empty())
            .map(from_hex)
            .collect()
    }

    /// The node's exec bound as the run carries it: `block,counts,string,unit`, `string` `none` where it has none.
    fn limit_line(limit: &ExecLimit) -> String {
        let string = limit
            .string
            .map_or_else(|| "none".to_owned(), |s| s.to_string());
        format!(
            "{},{:?},{string},{:?}",
            limit.block, limit.counts, limit.unit
        )
    }

    fn limit_of(line: &str) -> ExecLimit {
        let parts: Vec<&str> = line.split(',').collect();
        let [block, counts, string, unit] = parts.as_slice() else {
            panic!("HEX_2H_NODE_LIMIT {line:?}");
        };
        ExecLimit {
            block: block.parse().unwrap(),
            counts: match *counts {
                "Execve" => BlockCount::Execve,
                "Environment" => BlockCount::Environment,
                other => panic!("HEX_2H_NODE_LIMIT's counts {other:?}"),
            },
            string: match *string {
                "none" => None,
                n => Some(n.parse().unwrap()),
            },
            unit: match *unit {
                "Bytes" => StringUnit::Bytes,
                "Utf16" => StringUnit::Utf16,
                other => panic!("HEX_2H_NODE_LIMIT's unit {other:?}"),
            },
            source: "the node's host's bound, from phase A1 (HEX_2H_NODE_LIMIT)",
        }
    }

    /// The suite's programs as the node's host runs them: its `hex`, its exec bound, and whether it counts refused
    /// forks (A1's lines).
    fn node_fixture() -> BinFixture {
        BinFixture {
            exe: var("HEX_2H_NODE_HEX"),
            limit: limit_of(&var("HEX_2H_NODE_LIMIT")),
            forks_counted: var("HEX_2H_NODE_FORKS") == "counted",
        }
    }

    fn node_addr() -> SocketAddr {
        var("HEX_2H_ADDR").parse().unwrap()
    }

    /// Any address of `to`'s family, for the client's endpoint.
    fn any_of(to: SocketAddr) -> SocketAddr {
        let any: IpAddr = if to.is_ipv4() {
            Ipv4Addr::UNSPECIFIED.into()
        } else {
            Ipv6Addr::UNSPECIFIED.into()
        };
        SocketAddr::new(any, 0)
    }

    /// An endpoint of the client B1 enrolled, bound on any address of `to`'s family, shaped for a node of `in_flight`
    /// calls at once that stages nothing (`hex node` without --staged).
    fn client_end(to: SocketAddr, in_flight: usize) -> Arc<Endpoint> {
        Arc::new(
            Endpoint::start(Config {
                credentials: store::load(&dir().join("client")).unwrap(),
                bind: any_of(to),
                listen: false,
                serves: Vec::new(),
                shape: Shape {
                    connections: 1,
                    lanes: hex_quic::limits::client_lanes(in_flight, false),
                },
            })
            .unwrap(),
        )
    }

    /// The client connected to the node across the hosts, its end of the link at TLS13_AES_256_GCM_SHA384, shaped for
    /// the calls in flight the node's hello says, the dial and the hello on their own waits (no deadline of the test's).
    fn connected() -> hex_quic::link::Client {
        let to = node_addr();
        let in_flight = super::node_in_flight(
            store::load(&dir().join("client")).unwrap(),
            any_of(to),
            &var("HEX_2H_NODE"),
            to,
        );
        let end = client_end(to, in_flight);
        let (client, peer) =
            hex_quic::link::connect(Arc::clone(&end), &var("HEX_2H_NODE"), to, false, None)
                .unwrap();
        assert_eq!(
            end.negotiated(peer).unwrap().unwrap().0,
            CipherSuite::TLS13_AES_256_GCM_SHA384
        );
        client
    }

    /// HEX-QUIC-25 A1, on the node's host. Do: `hex node enroll` the node `HEX_2H_NODE` in `$HEX_2H_DIR/credentials`.
    /// Expect: its request for the CA (`HEX_2H_REQUEST`), the `hex` its programs run as (`HEX_2H_NODE_HEX`), and its
    /// host's exec bound and fork count (`HEX_2H_NODE_LIMIT`, `HEX_2H_NODE_FORKS`) said, and its key kept here alone.
    #[test]
    #[ignore = "HEX-QUIC-25's phase A1, on the node's host: the lead's two-host run"]
    fn hex_quic_25_a1_the_node_enrolls_here() {
        let credentials = dir().join("credentials");
        let name = var("HEX_2H_NODE");
        let begun = Command::new(hex())
            .args(["node", "enroll", "--name", &name, "--credentials"])
            .arg(&credentials)
            .output()
            .expect("hex node enroll");
        assert!(
            begun.status.success(),
            "{}",
            String::from_utf8_lossy(&begun.stderr)
        );
        let request = store::request(&credentials, Vec::new()).unwrap();
        assert_eq!(request.name, name);
        let here = super::fixture();
        say("HEX_2H_REQUEST", to_hex(&request.csr));
        say("HEX_2H_NODE_HEX", hex());
        say("HEX_2H_NODE_LIMIT", limit_line(&here.limit));
        say(
            "HEX_2H_NODE_FORKS",
            if here.forks_counted {
                "counted"
            } else {
                "uncounted"
            },
        );
    }

    /// HEX-QUIC-25 B1, on the client's host. Do: make the simulator CA here, admit `HEX_2H_NODE` and answer its request
    /// (`HEX_2H_REQUEST`), then enroll this host's client in `$HEX_2H_DIR/client`, its key made here
    /// (`store::begin`). Expect: the node's chain and roots said (`HEX_2H_CHAIN`, `HEX_2H_ROOTS`) and the client's
    /// kept. The CA's key ends with this process.
    #[test]
    #[ignore = "HEX-QUIC-25's phase B1, on the client's host: the lead's two-host run"]
    fn hex_quic_25_b1_the_ca_answers_here_and_the_client_enrolls() {
        let ca = SimCa::new().unwrap();
        let node = var("HEX_2H_NODE");
        ca.admit(&node, NODE_GRANT);
        let issued = ca
            .enroll(&Request {
                name: node,
                csr: from_hex(&var("HEX_2H_REQUEST")),
                grant: NODE_GRANT.to_vec(),
            })
            .unwrap();
        let client = dir().join("client");
        store::begin(&client, CLIENT).unwrap();
        ca.admit(CLIENT, CLIENT_GRANT);
        let answer = ca
            .enroll(&store::request(&client, CLIENT_GRANT.to_vec()).unwrap())
            .unwrap();
        store::finish(&client, &answer).unwrap();
        say("HEX_2H_CHAIN", certs_hex(&issued.chain));
        say("HEX_2H_ROOTS", certs_hex(&issued.roots));
    }

    /// HEX-QUIC-25 A2, on the node's host. Do: keep the CA's answer (`HEX_2H_CHAIN`, `HEX_2H_ROOTS`) for the node's
    /// enrollment. Expect: taken, being for the key made here and the name asked, and the credentials load.
    #[test]
    #[ignore = "HEX-QUIC-25's phase A2, on the node's host: the lead's two-host run"]
    fn hex_quic_25_a2_the_node_keeps_the_cas_answer() {
        let credentials = dir().join("credentials");
        let issued = Issued {
            chain: certs_der(&var("HEX_2H_CHAIN"))
                .into_iter()
                .map(Into::into)
                .collect(),
            roots: certs_der(&var("HEX_2H_ROOTS"))
                .into_iter()
                .map(Into::into)
                .collect(),
        };
        store::finish(&credentials, &issued).unwrap();
        store::load(&credentials).unwrap();
        say("HEX_2H_KEPT", credentials.display());
    }

    /// HEX-QUIC-25 B2, on the client's host. Do: connect to the node at `HEX_2H_ADDR` across the hosts and run the
    /// hex-api conformance suite, its programs the node's host's `hex selftest-box`. Expect: every case passes, the
    /// link at TLS13_AES_256_GCM_SHA384, as HEX-QUIC-23 on one host.
    #[test]
    #[ignore = "HEX-QUIC-25's phase B2, on the client's host: the lead's two-host run"]
    fn hex_quic_25_b2_the_suite_passes_across_hosts() {
        let client = connected();
        let failures = conformance::run(&client, &node_fixture());
        assert!(failures.is_empty(), "across the hosts: {failures:#?}");
        say("HEX_2H_SUITE", "passed");
    }

    /// The loss B3 and B4 take (`HEX_2H_LOSS`): the node killed on its host and started again, or its UDP port
    /// dropped and let through again.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Loss {
        Kill,
        Partition,
    }

    fn loss() -> Loss {
        match var("HEX_2H_LOSS").as_str() {
            "kill" => Loss::Kill,
            "partition" => Loss::Partition,
            other => panic!("HEX_2H_LOSS is kill or partition, not {other:?}"),
        }
    }

    /// HEX-QUIC-25 B3, on the client's host, in the background. Do: connect, start a program that waits in a box of
    /// its own with its stdout a pipe here, run the suite over and over, and say `HEX_2H_READY`; then the node is lost
    /// (`HEX_2H_LOSS`: killed on its host, or its UDP port dropped). Expect: the waiting program's stdout ends with the
    /// link, and the suite's run under way returns. The program's end and the next call are refused by type
    /// (`Refusal::Broken`). With the port dropped, a new dial is refused by type too, once QUIC's idle timeout passes
    /// with nothing from the node. It waits for the loss as long as the operator takes; the run's own time limit bounds
    /// it.
    #[test]
    #[ignore = "HEX-QUIC-25's phase B3, on the client's host: the lead's two-host run"]
    fn hex_quic_25_b3_a_lost_node_is_refused_by_type() {
        let loss = loss();
        let fixture = node_fixture();
        let client = connected();
        let id = BoxId::new(LOSS_BOX).unwrap();
        let _ = client.delete(&id);
        client
            .create(fixture.spec(id.clone(), Bounds::default()))
            .unwrap();
        let (mut out, w) = std::io::pipe().unwrap();
        let mut watched = client
            .exec(
                &id,
                fixture.program(&Act::Wait),
                Stdio {
                    stdout: Some(w),
                    ..Stdio::null()
                },
            )
            .unwrap();
        let lost = AtomicBool::new(false);
        let (read, runs) = std::thread::scope(|s| {
            let suite = s.spawn(|| {
                let mut failures = Vec::new();
                while !lost.load(Ordering::Acquire) {
                    failures.push(conformance::run(&client, &fixture).len());
                }
                failures
            });
            say(
                "HEX_2H_READY",
                match loss {
                    Loss::Kill => "kill -9 the node on its host now",
                    Loss::Partition => "drop the node's UDP port on its host now",
                },
            );
            // The program writes nothing: its stdout ends with the link, never before.
            let read = out.read(&mut [0u8; 64]);
            lost.store(true, Ordering::Release);
            (read, suite.join().unwrap())
        });
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the waiting program said something: {read:?}"
        );
        // A call first: the suite may have been between runs as the link went, so this may be the client's first word
        // of its end, which it then tells every program it watches.
        let listed = client.list();
        assert!(
            matches!(listed, Err(Refusal::Broken(_))),
            "a call after the loss: {listed:?}"
        );
        let ended = watched.try_wait();
        assert!(
            matches!(ended, Err(Refusal::Broken(_))),
            "the watched program's end after the loss: {ended:?}"
        );
        say(
            "HEX_2H_SUITE_RUNS",
            format!("{} (each one's failures: {runs:?})", runs.len()),
        );
        if loss == Loss::Partition {
            let to = node_addr();
            // Any shape: no hello comes while the node's port is dropped.
            match hex_quic::link::connect(client_end(to, 0), &var("HEX_2H_NODE"), to, false, None) {
                Ok(_) => panic!("a dial while the node's port is dropped connected"),
                Err(refused) => assert!(
                    matches!(
                        refused,
                        Refused::Unreachable | Refused::Transport { .. } | Refused::Closed { .. }
                    ),
                    "a dial while the node's port is dropped: {refused:?}"
                ),
            }
        }
        say("HEX_2H_LOST", format!("{loss:?}, refused by type"));
    }

    /// HEX-QUIC-25 B4, on the client's host, once B3's loss is over (the node started again, or its port let through
    /// again). Do: connect, inspect the loss's box, delete every box listed, list again, and run the suite. Expect:
    /// after a kill, the box still the restarted node's, its record having outlived the KILL and recovery having kept
    /// it (`adopted` said as found: a Linux node's inspect never carries it, `Node::recover` reports it). After a
    /// partition, the waiting program ended by the node as abandoned when the link went. None listed once each is
    /// deleted, and every case passes.
    #[test]
    #[ignore = "HEX-QUIC-25's phase B4, on the client's host: the lead's two-host run"]
    fn hex_quic_25_b4_nothing_is_left_after_the_loss() {
        let loss = loss();
        let client = connected();
        let watched = client.inspect(&BoxId::new(LOSS_BOX).unwrap()).unwrap();
        match loss {
            Loss::Kill => say(
                "HEX_2H_TAKEN_BACK",
                format!("{} adopted={}", watched.id, watched.adopted),
            ),
            Loss::Partition => assert!(
                watched.abandoned >= 1,
                "the node didn't end the program the lost link watched: {watched:?}"
            ),
        }
        for listed in client.list().unwrap() {
            client.delete(&listed.id).unwrap();
        }
        let left = client.list().unwrap();
        assert!(left.is_empty(), "left behind: {left:?}");
        let failures = conformance::run(&client, &node_fixture());
        assert!(failures.is_empty(), "after the loss: {failures:#?}");
        say("HEX_2H_LEFT", "none");
    }

    /// Starts `hex node` over `dir` with `more`, its boxes under `box_root`, and says its pid and the address its QUIC
    /// listener bound (`HEX_2H_NODE_PID`, `HEX_2H_NODE_QUIC`).
    #[cfg(target_os = "linux")]
    fn started(
        dir: &std::path::Path,
        box_root: &std::path::Path,
        more: &[&std::ffi::OsStr],
    ) -> super::NodeProcess {
        let (node, said) = super::NodeProcess::start(dir, Some(box_root), more);
        say("HEX_2H_NODE_PID", node.child.id());
        let quic = said
            .iter()
            .find_map(|line| line.strip_prefix("quic "))
            .unwrap_or_else(|| panic!("hex node said {said:?}"));
        say("HEX_2H_NODE_QUIC", quic);
        node
    }

    /// Every cgroup under `root` that holds a process, with its `cgroup.procs`: the tree walked with a stack of its
    /// own (CLAUDE.md §11).
    #[cfg(target_os = "linux")]
    fn processes_under(root: &std::path::Path) -> Vec<(PathBuf, String)> {
        let mut held = Vec::new();
        let mut walk = vec![root.to_owned()];
        while let Some(cgroup) = walk.pop() {
            if let Ok(procs) = std::fs::read_to_string(cgroup.join("cgroup.procs"))
                && !procs.trim().is_empty()
            {
                held.push((cgroup.clone(), procs));
            }
            for entry in std::fs::read_dir(&cgroup).into_iter().flatten().flatten() {
                if entry.path().is_dir() {
                    walk.push(entry.path());
                }
            }
        }
        held
    }

    /// HEX-QUIC-25 A3, on the node's host (Linux), in the background. Do: run `hex node --quic $HEX_2H_BIND` with the
    /// kept enrollment, its boxes in a delegated cgroup subtree. Once a KILL ends it, start it again over the same
    /// state, box root and address, then wait for its last end (`kill -TERM`). Expect: each start said
    /// (`HEX_2H_NODE_PID`, `HEX_2H_NODE_QUIC`), and no process left in its box root once it has ended
    /// (`HEX_2H_LEFT=none`). A host that can't delegate a cgroup records the skip and serves nothing.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "HEX-QUIC-25's phase A3, on the node's host: the lead's two-host run"]
    fn hex_quic_25_a3_hex_node_serves_through_a_kill_and_leaves_nothing() {
        use std::os::unix::process::ExitStatusExt;
        super::linux::delegated(
            "two_hosts::hex_quic_25_a3_hex_node_serves_through_a_kill_and_leaves_nothing",
            |own| {
                let dir = dir();
                let box_root = own.join("hex-boxes-two-hosts");
                let credentials = dir.join("credentials");
                let bind = var("HEX_2H_BIND");
                let more: [&std::ffi::OsStr; 4] = [
                    "--quic".as_ref(),
                    bind.as_ref(),
                    "--credentials".as_ref(),
                    credentials.as_os_str(),
                ];
                let mut node = started(&dir, &box_root, &more);
                let first = node.child.wait().unwrap();
                assert_eq!(
                    first.signal(),
                    Some(9),
                    "the node ended {first}, not by KILL: {}",
                    node.said()
                );
                say("HEX_2H_KILLED", first);
                drop(node);
                let mut node = started(&dir, &box_root, &more);
                let last = node.child.wait().unwrap();
                say("HEX_2H_ENDED", last);
                let held = processes_under(&box_root);
                assert!(
                    held.is_empty(),
                    "left behind under {}: {held:?}",
                    box_root.display()
                );
                say("HEX_2H_LEFT", "none");
            },
        );
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    /// This process's own cgroup, delegated to it and prepared once for every test in this binary
    /// (`hex_testkit::cgroup::delegated`: the process and every process of its own moved into a leaf), or why it
    /// isn't ours to subdivide. The probe root's opening proves every controller a box uses is delegated here:
    /// `BoxRoot::open` refuses by name otherwise (HEX-INST-1).
    fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
        hex_testkit::cgroup::delegated(|own| {
            let probe = hex_runtime::linux::cgroup::BoxRoot::open(
                &own.join("hex-probe"),
                hex_runtime::linux::disks::under(disk).unwrap_or_default(),
            )
            .map(|_| ())
            .map_err(|e| e.to_string());
            let _ = std::fs::remove_dir(own.join("hex-probe"));
            probe
        })
    }

    /// Runs `body` with this process's cgroup, delegated: here where it is, else once more as `test` alone in a
    /// `systemd-run --user --scope -p Delegate=yes` (an ignored test too, as the two-host run's are), else the skip
    /// recorded.
    pub(super) fn delegated(test: &str, body: impl FnOnce(&Path)) {
        let disk = super::scratch("disk-");
        match prepare_cgroup(disk.path()) {
            Ok(own) => body(&own),
            Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
                let has_systemd = Command::new("systemd-run")
                    .arg("--version")
                    .output()
                    .is_ok_and(|o| o.status.success());
                if !has_systemd {
                    hex_testkit::skip(
                        test,
                        "cgroup v2 delegation",
                        &format!(
                            "own cgroup isn't ours to subdivide ({direct}) and no systemd --user"
                        ),
                    )
                    .expect("recording the skip");
                    return;
                }
                let status = Command::new("systemd-run")
                    .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                    .arg(std::env::current_exe().expect("test binary path"))
                    .args([
                        test,
                        "--exact",
                        "--include-ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("HEX_DELEGATED", "1")
                    .status()
                    .expect("run the test in a delegated scope");
                assert!(status.success(), "the delegated re-run of {test} failed");
            }
            // HEX_DELEGATED promised this binary a cgroup delegated to it: a preparation that fails there fails the
            // test, never a skip that would hide the breakage.
            Err(direct) => panic!("{test}: its cgroup isn't delegated to it ({direct})"),
        }
    }

    #[test]
    fn hex_quic_20_the_conformance_suite_passes_over_quic() {
        delegated(
            "linux::hex_quic_20_the_conformance_suite_passes_over_quic",
            |own| {
                let state = super::scratch("node-");
                let node =
                    hex_node::Node::open(&own.join("hex-boxes-quic"), state.path(), state.path())
                        .expect("open a node");
                super::exercise(Arc::new(node));
            },
        );
    }

    #[test]
    fn hex_quic_23_a_configured_hex_node_serves_the_suite_over_quic() {
        delegated(
            "linux::hex_quic_23_a_configured_hex_node_serves_the_suite_over_quic",
            |own| super::configured(Some(&own.join("hex-boxes-cli"))),
        );
    }

    #[test]
    fn hex_quic_24_without_its_enrollment_hex_node_binds_no_quic_and_refuses_typed() {
        delegated(
            "linux::hex_quic_24_without_its_enrollment_hex_node_binds_no_quic_and_refuses_typed",
            |own| super::unconfigured(Some(&own.join("hex-boxes-cli-none"))),
        );
    }
}
