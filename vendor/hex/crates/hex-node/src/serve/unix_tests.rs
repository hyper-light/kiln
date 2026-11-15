//! The socket's tests that hold on each OS it serves (Linux's sequenced packets, macOS's framed stream): a live
//! server in this process, reached through `hex-client`.

use super::*;

use std::io::{Read, Write as _};
use std::os::unix::net::UnixStream;

use hex_api::sim::SimRuntime;
use hex_api::{Ask, Bounds, BoxInfo, BoxSpec, ClassSpec, Signal as HexSignal, Usage};
use hex_client::Client;
use hex_testkit::waits::follow::{self, Party};

/// A server as the party of a wait on what it does: live while its I/O thread serves (the review's C-5).
fn serving<O: Os>(server: &Server<O>) -> Party {
    match &server.io {
        Some(io) if !io.is_finished() => Party::Live,
        _ => Party::Ended,
    }
}

/// The probe's count of sends (HEX-GUEST-41, at 31d5b387): one end of a Unix socket pair sent 20,000 times while
/// another thread made and closed Unix socket pairs (140,461 of them), on macOS 26.4 (XNU 25.4.0). With the sender
/// closing its copy at once, 719 arrived dead; with it kept until received, 0.
const PROBED: usize = 20_000;

/// A program that doesn't end while the test runs: a `sleep` the test started, a child of this process as the node's
/// programs are of the node's (the server watches no other process: GPU run r1119), which its kill leaves be.
struct Unending(u32);

impl Proc for Unending {
    fn pid(&self) -> u32 {
        self.0
    }
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        Ok(None)
    }
    fn wait_for(&mut self, _timeout: Duration) -> Result<Option<Exit>, Refusal> {
        Ok(None)
    }
    fn kill(&mut self) -> Result<(), Refusal> {
        Ok(())
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// Boxes in the simulator; each program [`Unending`], the one `sleep` whose pid is `program`, and the stdout an exec
/// asks for one end of a Unix socket pair whose other end has written one byte and closed: a guest program's streams
/// are connections like it.
struct SocketHost {
    sim: SimRuntime,
    program: u32,
}

impl Runtime for SocketHost {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(&self, id: &BoxId, _program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.inspect(id)?;
        let mut given = hex_api::Given::default();
        if stdio.ask.stdout {
            let (mut near, far) =
                UnixStream::pair().map_err(|e| Refusal::os("pairing a stream", e))?;
            near.write_all(b"x")
                .map_err(|e| Refusal::os("writing a stream", e))?;
            given.stdout = Some(far.into());
        }
        Ok(Box::new(hex_api::Asked::new(Unending(self.program), given)))
    }
    /// The run's box in the simulator, its program the exec's, as the other hosts' run is.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        let id = spec.id.clone();
        self.sim.create(spec)?;
        self.exec(&id, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: HexSignal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
}

impl Host for SocketHost {
    fn abandoned(&self, _id: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Ok(netns::Counts::default())
    }
}

fn limits() -> Limits {
    Limits {
        connections: 4,
        message_bytes: 4096,
        in_flight: 4,
        programs: 4,
    }
}

fn b() -> BoxId {
    BoxId::new("b").unwrap()
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): a given answer's sockets arrive alive while other Unix sockets
/// close. XNU's collector of descriptors in flight (`unp_gc`, run as a Unix socket closes while any are in flight)
/// flushes a socket whose only reference is a message in flight, which arrives reading nothing: the probe's 719 of
/// [`PROBED`]. The node keeps its copies of a given answer's ends until its client says it took them, so as many
/// sent under the same churn each arrive with their byte. Linux's collector takes only what no socket can reach, so
/// there this holds either way.
#[test]
fn a_given_answers_sockets_arrive_alive_while_other_unix_sockets_close() {
    let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/serve");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("sock-")
        .tempdir_in(&cache)
        .unwrap();
    let mut program = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .unwrap();
    let host = Arc::new(SocketHost {
        sim: SimRuntime::default(),
        program: program.id(),
    });
    host.sim
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: b(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        })
        .unwrap();
    let server = Server::bind(
        &dir.path().join("node.sock"),
        Arc::clone(&host) as Arc<dyn Host>,
        limits(),
        8,
    )
    .unwrap();
    let client = Client::connect(server.path()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut closed = 0usize;
            while !stop.load(Ordering::Relaxed) {
                drop(UnixStream::pair().unwrap());
                closed += 1;
            }
            closed
        })
    };
    let mut dead = 0usize;
    for _ in 0..PROBED {
        let asked = Stdio {
            ask: Ask {
                stdout: true,
                ..Ask::default()
            },
            ..Stdio::null()
        };
        let mut proc = client
            .exec(&b(), Program::of(&["unending"]), asked)
            .unwrap();
        let stdout = proc.streams().unwrap().stdout.unwrap();
        let mut got = Vec::new();
        let read = std::fs::File::from(stdout).read_to_end(&mut got);
        if !(matches!(read, Ok(1)) && got == b"x") {
            dead += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let closed = churn.join().unwrap();
    // The server first, so the program's end, which this test makes, reaches no watcher of it.
    drop((client, server));
    let _ = program.kill();
    let _ = program.wait();
    assert_eq!(
        dead, 0,
        "{dead} of {PROBED} given sockets arrived dead while {closed} other Unix socket pairs closed"
    );
}

/// Joint plan step 33 (docs/design/credential-grants.md C7): a Shards image class in the model, over the simulator. A
/// run of a box granting a credential leases as Shards' run host does: from its node's lessors as it starts, each
/// credential's value held until the run ends, when each lease is given back and the run's lessor let go.
struct LeasingRuntime {
    sim: SimRuntime,
    lessors: Arc<WireLessors>,
    runs: Mutex<BTreeMap<String, Leasing>>,
    /// Each box's sign-ins as its spec granted them, and what each run's capture of them gave (C8).
    sign_ins: Mutex<BTreeMap<String, Vec<hex_api::SignInGrant>>>,
    captured: Mutex<BTreeMap<String, Vec<hex_api::Captured>>>,
}

/// A run's lessor and the leases it holds.
type Leasing = (Arc<dyn hex_api::lease::Lessor>, Vec<hex_api::lease::Lease>);

impl LeasingRuntime {
    fn new(lessors: Arc<WireLessors>) -> Self {
        Self {
            sim: SimRuntime::default(),
            lessors,
            runs: Mutex::new(BTreeMap::new()),
            sign_ins: Mutex::new(BTreeMap::new()),
            captured: Mutex::new(BTreeMap::new()),
        }
    }

    /// The run of box `id`'s lessor, while it runs.
    fn lessor_of(&self, id: &BoxId) -> Option<Arc<dyn hex_api::lease::Lessor>> {
        let runs = self.runs.lock().unwrap();
        runs.get(id.as_str()).map(|(lessor, _)| Arc::clone(lessor))
    }

    /// The run of box `id` has ended: each of its leases given back, and its lessor let go.
    fn end_run(&self, id: &BoxId) {
        let run = self.runs.lock().unwrap().remove(id.as_str());
        if let Some((lessor, leases)) = run {
            for lease in &leases {
                lessor.release(&lease.lease);
            }
        }
    }

    /// The value the run of box `id` holds of its first credential.
    fn held(&self, id: &BoxId) -> Option<Vec<u8>> {
        let runs = self.runs.lock().unwrap();
        let (_, leases) = runs.get(id.as_str())?;
        leases.first().map(|lease| lease.value.expose().to_vec())
    }
}

impl Runtime for LeasingRuntime {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        let sign_ins = match &spec.class {
            ClassSpec::ShardsImage(image) => image.sign_ins.clone(),
            _ => Vec::new(),
        };
        let id = spec.id.to_string();
        let made = self.sim.create(spec)?;
        self.sign_ins.lock().unwrap().insert(id, sign_ins);
        Ok(made)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        let credentials = self.sim.inspect(id)?.credentials;
        let sign_ins = self
            .sign_ins
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .unwrap_or_default();
        self.sim.start(id)?;
        if credentials.is_empty() && sign_ins.is_empty() {
            return Ok(());
        }
        let lessor = hex_api::lease::Lessors::lessor(&*self.lessors, id)?;
        let mut leases = Vec::new();
        for grant in &credentials {
            let lease = lessor
                .lease(&grant.reference, None)
                .map_err(|why| Refusal::Broken(format!("its run's lease was refused: {why}")))?;
            leases.push(lease);
        }
        // Each sign-in captured as its CLI's first token response would be, its tokens the run's to hand on (C8).
        let mut captured = Vec::new();
        for grant in &sign_ins {
            let tokens = hex_api::lease::SignInTokens {
                access: hex_api::lease::Value::new(TOKEN.to_vec()),
                refresh: Some(hex_api::lease::Value::new(b"refresh-token".to_vec())),
                expires_in: Some(28_800),
                id_token: None,
                scope: Some("user:inference".to_owned()),
            };
            let got = lessor
                .capture(&grant.sign_in, tokens, None)
                .map_err(|why| Refusal::Broken(format!("its run's capture was refused: {why}")))?;
            captured.push(got);
        }
        self.captured
            .lock()
            .unwrap()
            .insert(id.to_string(), captured);
        self.runs
            .lock()
            .unwrap()
            .insert(id.to_string(), (lessor, leases));
        Ok(())
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: HexSignal) -> Result<(), Refusal> {
        self.end_run(id);
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.end_run(id);
        self.sim.delete(id)
    }
}

impl Host for LeasingRuntime {
    fn abandoned(&self, _id: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Ok(netns::Counts::default())
    }
    fn lessors(&self) -> Option<Arc<WireLessors>> {
        Some(Arc::clone(&self.lessors))
    }
}

/// The model's records are taken back as the simulator's are: it keeps no process and no native metadata.
impl crate::recorded::Recover for LeasingRuntime {
    fn project_routes(
        &self,
        pools: &[hex_core::record::PoolRecord],
        introduced: Option<(hex_api::Execution, &[hex_api::DeviceId])>,
        projection: &mut hex_runtime::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        crate::recorded::Recover::project_routes(&self.sim, pools, introduced, projection)
    }
    fn preflight_retained(
        &self,
        records: &hex_runtime::devices::Records,
        pools: &[hex_core::record::PoolRecord],
    ) -> Result<(), Refusal> {
        crate::recorded::Recover::preflight_retained(&self.sim, records, pools)
    }
    fn trace(&self, id: &BoxId) -> Result<Option<hex_core::record::ProcId>, Refusal> {
        crate::recorded::Recover::trace(&self.sim, id)
    }
    fn adopt(
        &self,
        spec: &BoxSpec,
        trace: Option<hex_core::record::ProcId>,
    ) -> Result<(), Refusal> {
        crate::recorded::Recover::adopt(&self.sim, spec, trace)
    }
    fn end(&self, trace: Option<hex_core::record::ProcId>) -> Result<(), Refusal> {
        crate::recorded::Recover::end(&self.sim, trace)
    }
}

/// What a waiting broker follows: the client's connection, its lessors' thread giving each lease back as it hears.
static GIVEN_BACK: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_node.serve.s33.given_back");

/// A sign-in's access token, as its CLI's token response gave it.
const TOKEN: &[u8] = b"hex-s33-token-5b0e8c2d71f4a693";

/// A broker that serves its runs' sign-ins (C8): each capture kept with its access token, its placeholders answered,
/// each refresh answered with a token response, each revocation kept.
#[derive(Default, Clone)]
struct SignInBroker {
    state: Arc<BrokerState>,
}

#[derive(Default)]
struct BrokerState {
    captures: Mutex<Vec<(String, Vec<u8>)>>,
    revoked: Mutex<Vec<String>>,
}

impl hex_api::lease::Lessors for SignInBroker {
    fn lessor(&self, _id: &BoxId) -> Result<Arc<dyn hex_api::lease::Lessor>, Refusal> {
        Ok(Arc::new(SignInRun(Arc::clone(&self.state))))
    }
}

/// The broker's lessor for one run.
struct SignInRun(Arc<BrokerState>);

impl hex_api::lease::Lessor for SignInRun {
    fn lease(
        &self,
        _: &str,
        _: Option<[u8; 32]>,
    ) -> Result<hex_api::lease::Lease, hex_api::lease::LeaseRefused> {
        Err(hex_api::lease::LeaseRefused::Denied(
            "this broker leases no credential".to_owned(),
        ))
    }
    fn release(&self, _: &str) {}
    fn capture(
        &self,
        sign_in: &str,
        tokens: hex_api::lease::SignInTokens,
        _key: Option<[u8; 32]>,
    ) -> Result<hex_api::Captured, hex_api::lease::SignInRefused> {
        self.0
            .captures
            .lock()
            .unwrap()
            .push((sign_in.to_owned(), tokens.access.expose().to_vec()));
        Ok(hex_api::Captured {
            access: format!("{sign_in}/access"),
            access_placeholder: "kiln-placeholder-6163".to_owned(),
            refresh_placeholder: Some("kiln-placeholder-7266".to_owned()),
        })
    }
    fn refresh(
        &self,
        _sign_in: &str,
        _key: Option<[u8; 32]>,
    ) -> Result<hex_api::lease::Value, hex_api::lease::SignInRefused> {
        Ok(hex_api::lease::Value::new(
            b"{\"access_token\":\"kiln-placeholder-6163\"}".to_vec(),
        ))
    }
    fn revoked(&self, sign_in: &str, _key: Option<[u8; 32]>) {
        self.0.revoked.lock().unwrap().push(sign_in.to_owned());
    }
}

/// A Shards image VM whose harness's CLI, Claude Code, signs in as `claude/ada` toward its provider.
fn signing_in(name: &str) -> BoxSpec {
    let mut spec = granting(name);
    if let ClassSpec::ShardsImage(image) = &mut spec.class {
        image.credentials.clear();
        image.sign_ins = vec![hex_api::SignInGrant {
            member: hex_api::Member {
                kind: hex_api::MemberKind::Agent,
                name: "claude".to_owned(),
            },
            sign_in: "claude/ada".to_owned(),
            cli: hex_api::SignInCli::ClaudeCode,
            toward: vec!["api.anthropic.com".to_owned()],
            captured: None,
        }];
    }
    spec
}

/// Joint plan step 33's sign-ins (docs/design/credential-grants.md C8): a harness VM created over the node's wire by a
/// client that leases has its CLI's sign-in captured through that wire, its tokens handed whole to the client's broker
/// and the broker's placeholders answered back; its refresh answered the same way; its revocation told to the broker.
#[test]
fn s33_a_remote_harnesss_sign_in_is_captured_refreshed_and_revoked_through_the_wire() {
    let dir = scratch("s33-sign-in-");
    let host = Arc::new(LeasingRuntime::new(WireLessors::new()));
    let server = Server::bind(
        &dir.path().join("node.sock"),
        Arc::clone(&host) as Arc<dyn Host>,
        limits(),
        8,
    )
    .unwrap();
    let broker = SignInBroker::default();
    let client = Client::connect(server.path())
        .unwrap()
        .with_lessors(Arc::new(broker.clone()))
        .unwrap();
    let spec = signing_in("harness-5");
    let id = spec.id.clone();
    client.create(spec).unwrap();
    client.start(&id).unwrap();
    let captured = host
        .captured
        .lock()
        .unwrap()
        .get(id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        captured,
        vec![hex_api::Captured {
            access: "claude/ada/access".to_owned(),
            access_placeholder: "kiln-placeholder-6163".to_owned(),
            refresh_placeholder: Some("kiln-placeholder-7266".to_owned()),
        }]
    );
    assert_eq!(
        broker.state.captures.lock().unwrap().clone(),
        vec![("claude/ada".to_owned(), TOKEN.to_vec())],
        "the broker holds the access token, whole"
    );
    let lessor = host.lessor_of(&id).unwrap();
    assert_eq!(
        lessor.refresh("claude/ada", None).unwrap().expose(),
        b"{\"access_token\":\"kiln-placeholder-6163\"}"
    );
    lessor.revoked("claude/ada", None);
    follow::until(
        &GIVEN_BACK,
        "the revocation told to the broker",
        || serving(&server),
        || broker.state.revoked.lock().unwrap().len() == 1,
    )
    .unwrap();
    drop(lessor);
    client.signal(&id, HexSignal::KILL).unwrap();
    client.delete(&id).unwrap();
    drop((client, server));
}

/// A credential's value no file or answer of the node's may hold.
const VALUE: &[u8] = b"hex-s33-value-1d9e4f7a2c58b6e0";

/// A folder of the test's own under the cache, never /tmp.
fn scratch(prefix: &str) -> tempfile::TempDir {
    let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/serve");
    std::fs::create_dir_all(&cache).unwrap();
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&cache)
        .unwrap()
}

/// A Shards image VM whose harness, `claude`, is granted its provider by name over TCP and its credential toward it.
fn granting(name: &str) -> BoxSpec {
    let claude = hex_api::Member {
        kind: hex_api::MemberKind::Agent,
        name: "claude".to_owned(),
    };
    let mut image = hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE);
    image.grants = vec![hex_api::Grant::Egress {
        member: claude.clone(),
        to: hex_api::Destination::Name("api.anthropic.com".to_owned()),
        ports: hex_api::Ports {
            proto: hex_api::Proto::Tcp,
            first: 443,
            last: 443,
        },
    }];
    image.credentials = vec![hex_api::CredentialGrant {
        member: claude,
        reference: "providers/anthropic".to_owned(),
        toward: vec!["api.anthropic.com".to_owned()],
        // As kiln's broker mints one: its prefix, then hex.
        placeholder: "kiln-placeholder-73333374657374".to_owned(),
    }];
    BoxSpec {
        route: None,
        allocation: hex_api::sim::sim_vm_allocation().ok(),
        devices: Vec::new(),
        id: BoxId::new(name).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(image)),
        owner: None,
        band: 0,
    }
}

/// A node serving the leasing model, and the broker its client leases from, holding [`VALUE`].
fn leasing_node(dir: &Path) -> (Arc<LeasingRuntime>, Server, hex_api::lease::sim::SimLessors) {
    let host = Arc::new(LeasingRuntime::new(WireLessors::new()));
    let server = Server::bind(
        &dir.join("node.sock"),
        Arc::clone(&host) as Arc<dyn Host>,
        limits(),
        8,
    )
    .unwrap();
    let broker = hex_api::lease::sim::SimLessors::new();
    broker.hold("providers/anthropic", VALUE).unwrap();
    (host, server, broker)
}

/// Joint plan step 33 (C7), red first: a harness VM created over the node's wire by a client that leases leases its
/// credential's value through that wire, from the client's broker, as its run starts; and gives it back as its run ends.
#[test]
fn s33_a_remote_harnesss_lease_is_answered_through_the_wire_and_given_back_at_its_end() {
    let dir = scratch("s33-lease-");
    let (host, server, broker) = leasing_node(dir.path());
    let client = Client::connect(server.path())
        .unwrap()
        .with_lessors(Arc::new(broker.clone()))
        .unwrap();
    let spec = granting("harness-1");
    let id = spec.id.clone();
    client.create(spec).unwrap();
    client.start(&id).unwrap();
    assert_eq!(
        host.held(&id).as_deref(),
        Some(VALUE),
        "the run holds its credential's value, leased through the wire"
    );
    assert_eq!(
        broker.held().unwrap(),
        1,
        "the broker holds the run's one lease"
    );

    client.signal(&id, HexSignal::KILL).unwrap();
    follow::until(
        &GIVEN_BACK,
        "the run's lease given back to its broker",
        || serving(&server),
        || broker.held().is_ok_and(|held| held == 0),
    )
    .unwrap();
    assert_eq!(broker.given_back().unwrap(), 1, "given back once");
    client.delete(&id).unwrap();
    drop((client, server));
}

/// Joint plan step 33 (C7), red first: a client's connection that drops while its box's run holds a lease gives that
/// lease back to its broker, and the run's next ask is refused, typed, naming the connection's end.
#[test]
fn s33_a_connection_that_drops_gives_back_its_leases() {
    let dir = scratch("s33-drop-");
    let (host, server, broker) = leasing_node(dir.path());
    let client = Client::connect(server.path())
        .unwrap()
        .with_lessors(Arc::new(broker.clone()))
        .unwrap();
    let spec = granting("harness-2");
    let id = spec.id.clone();
    client.create(spec).unwrap();
    client.start(&id).unwrap();
    assert_eq!(broker.held().unwrap(), 1);

    drop(client);
    follow::until(
        &GIVEN_BACK,
        "the dropped connection's lease given back to its broker",
        || serving(&server),
        || broker.held().is_ok_and(|held| held == 0),
    )
    .unwrap();
    let again = {
        let runs = host.runs.lock().unwrap();
        let (lessor, _) = runs.get(id.as_str()).unwrap();
        Arc::clone(lessor)
    };
    match again.lease("providers/anthropic", None) {
        Err(hex_api::lease::LeaseRefused::Unavailable(why)) => {
            assert!(why.contains("has ended"), "{why}");
        }
        other => panic!("a lease asked after its connection dropped: {other:?}"),
    }
    drop(again);
    host.end_run(&id);
    drop(server);
}

/// Joint plan step 33 (C7), default deny: a box granting a credential, created by a client that gave no lessors, is
/// refused `Unconfigured` before any record, and no one is asked for anything.
#[test]
fn s33_a_credential_granting_create_without_lessors_is_refused_before_any_record() {
    let dir = scratch("s33-deny-");
    let (host, server, broker) = leasing_node(dir.path());
    let client = Client::connect(server.path()).unwrap();
    match client.create(granting("harness-3")) {
        Err(Refusal::Unconfigured { what, why }) => {
            assert!(what.contains("lessors"), "{what}: {why}");
        }
        other => panic!("a credential granted by a client with no lessors: {other:?}"),
    }
    match client.create(signing_in("harness-6")) {
        Err(Refusal::Unconfigured { what, why }) => {
            assert!(what.contains("lessors"), "{what}: {why}");
        }
        other => panic!("a sign-in granted by a client with no lessors: {other:?}"),
    }
    assert!(host.sim.list().unwrap().is_empty(), "no record was made");
    assert_eq!(broker.held().unwrap(), 0);
    drop((client, server));
}

/// Joint plan step 33 (C7): a lease's value is a secret, so nothing of the node keeps it. A harness VM is created on a
/// recorded node (its records in its state folder, its socket in its run folder), leases a known value through the
/// wire, and runs to its stop and its delete; then every file under both folders, and every answer the node gave about
/// the box, is read, and the value's bytes are nowhere. The census holds wait names and times alone, by its type.
#[test]
fn s33_a_leased_value_is_kept_nowhere_on_the_node() {
    let state = scratch("s33-state-");
    let run = scratch("s33-run-");
    let wire = WireLessors::new();
    let node = Arc::new(
        crate::recorded::Recorded::open(LeasingRuntime::new(Arc::clone(&wire)), state.path())
            .unwrap(),
    );
    node.lease_over_wire(wire).unwrap();
    let server = Server::bind(
        &run.path().join("node.sock"),
        Arc::clone(&node) as Arc<dyn Host>,
        limits(),
        8,
    )
    .unwrap();
    let broker = hex_api::lease::sim::SimLessors::new();
    broker.hold("providers/anthropic", VALUE).unwrap();
    let client = Client::connect(server.path())
        .unwrap()
        .with_lessors(Arc::new(broker.clone()))
        .unwrap();
    let spec = granting("harness-4");
    let id = spec.id.clone();
    let mut said = format!("{:?}", client.create(spec).unwrap());
    client.start(&id).unwrap();
    said.push_str(&format!("{:?}", client.inspect(&id).unwrap()));
    client.signal(&id, HexSignal::KILL).unwrap();
    follow::until(
        &GIVEN_BACK,
        "the run's lease given back to its broker",
        || serving(&server),
        || broker.held().is_ok_and(|held| held == 0),
    )
    .unwrap();
    said.push_str(&format!("{:?}", client.inspect(&id).unwrap()));
    client.delete(&id).unwrap();
    drop((client, server));

    let mut read = 0usize;
    for root in [state.path(), run.path()] {
        let mut folders = vec![root.to_path_buf()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    folders.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    read += 1;
                    assert!(
                        !bytes.windows(VALUE.len()).any(|w| w == VALUE),
                        "{} holds the leased value",
                        path.display()
                    );
                }
            }
        }
    }
    assert!(read > 0, "the node's records were read");
    assert!(
        !said.as_bytes().windows(VALUE.len()).any(|w| w == VALUE),
        "an answer about the box holds the leased value: {said}"
    );
}
