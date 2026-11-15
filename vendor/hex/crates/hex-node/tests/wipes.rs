//! The review's E-6 (docs/design/credential-grants.md C7, C8): every copy a node makes of a secret it carries on its
//! socket, a lease's value and a sign-in's tokens, is wiped before its memory is given back, on each OS it serves. A
//! live server in this process, over this OS's socket, reached through `hex-client`; every block this test binary
//! frees is read by the wipes watch, its global allocator (`hex_testkit::wipes`, CLAUDE.md §12).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]

use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hex_api::lease::{Lease, LeaseRefused, Lessor, Lessors, SignInRefused, SignInTokens, Value};
use hex_api::sim::SimRuntime;
use hex_api::{
    BoxId, BoxInfo, BoxSpec, ClassSpec, Proc, Program, Refusal, Runtime, Signal, Stdio, Usage,
};
use hex_client::Client;
use hex_node::serve::{Host, Limits, Native, Server, WireLessors};
use hex_quic::enroll::{self, sim::SimCa};
use hex_testkit::wipes::{self, SECRET};

/// Every block this test binary frees is read for the watched secret.
#[cfg(test)]
#[global_allocator]
static WIPES: wipes::Watch = wipes::Watch;

/// 32 random bytes: this process's random hash keys, one hash for each byte's place.
fn secret() -> [u8; SECRET] {
    let keys = std::collections::hash_map::RandomState::new();
    std::array::from_fn(|place| {
        let mut hash = keys.build_hasher();
        hash.write_usize(place);
        hash.finish().to_le_bytes()[0]
    })
}

/// A folder of the test's own under the user's cache, never the system's temporary folder.
fn scratch() -> tempfile::TempDir {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap();
    let cache = PathBuf::from(home)
        .join(".cache")
        .join("hex-t7")
        .join("wipes");
    std::fs::create_dir_all(&cache).unwrap();
    tempfile::Builder::new()
        .prefix("e6-")
        .tempdir_in(&cache)
        .unwrap()
}

/// A Shards image class over the simulator whose run, as it starts, leases its credential, has its sign-in captured
/// and then refreshed through the node's lessors, as Shards' run host does, each carrying the test's secret, and keeps
/// none of it past its start. Its own copy of the secret is a value, wiped as it drops.
struct Secrets {
    sim: SimRuntime,
    lessors: Arc<WireLessors>,
    secret: Arc<Value>,
}

impl Secrets {
    /// The run's lease, capture and refresh, each checked to carry the secret.
    fn carry(&self, id: &BoxId) -> Result<(), Refusal> {
        let broken =
            |what: &str, why: &dyn std::fmt::Display| Refusal::Broken(format!("{what}: {why}"));
        let lessor = Lessors::lessor(&*self.lessors, id)?;
        let lease = lessor
            .lease("providers/anthropic", None)
            .map_err(|why| broken("the run's lease", &why))?;
        if lease.value.expose() != self.secret.expose() {
            return Err(broken("the run's lease", &"its value isn't the secret"));
        }
        lessor.release(&lease.lease);
        drop(lease);
        let tokens = SignInTokens {
            access: Value::new(self.secret.expose().to_vec()),
            refresh: Some(Value::new(self.secret.expose().to_vec())),
            expires_in: Some(28_800),
            id_token: None,
            scope: Some("user:inference".to_owned()),
        };
        lessor
            .capture("claude/ada", tokens, None)
            .map_err(|why| broken("the run's capture", &why))?;
        let refreshed = lessor
            .refresh("claude/ada", None)
            .map_err(|why| broken("the run's refresh", &why))?;
        if refreshed.expose() != self.secret.expose() {
            return Err(broken("the run's refresh", &"its value isn't the secret"));
        }
        Ok(())
    }
}

impl Runtime for Secrets {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)?;
        self.carry(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
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

impl Host for Secrets {
    fn abandoned(&self, _id: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<hex_core::netns::Counts, Refusal> {
        Ok(hex_core::netns::Counts::default())
    }
    fn lessors(&self) -> Option<Arc<WireLessors>> {
        Some(Arc::clone(&self.lessors))
    }
}

/// The client's broker: it leases the secret, takes a capture's tokens (noting they were the secret) and refreshes
/// with the secret, keeping nothing else. Its copy of the secret is a value, wiped as it drops.
struct Broker {
    secret: Arc<Value>,
    captured: Arc<AtomicBool>,
}

impl Lessors for Broker {
    fn lessor(&self, _id: &BoxId) -> Result<Arc<dyn Lessor>, Refusal> {
        Ok(Arc::new(Broker {
            secret: Arc::clone(&self.secret),
            captured: Arc::clone(&self.captured),
        }))
    }
}

impl Lessor for Broker {
    fn lease(&self, _reference: &str, _key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused> {
        Ok(Lease {
            lease: "lease-e6".to_owned(),
            value: Value::new(self.secret.expose().to_vec()),
            life: None,
        })
    }
    fn release(&self, _lease: &str) {}
    fn capture(
        &self,
        _sign_in: &str,
        tokens: SignInTokens,
        _key: Option<[u8; 32]>,
    ) -> Result<hex_api::Captured, SignInRefused> {
        let whole = tokens.access.expose() == self.secret.expose()
            && tokens
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.expose() == self.secret.expose());
        self.captured.store(whole, Ordering::Release);
        Ok(hex_api::Captured {
            access: "claude/ada/access".to_owned(),
            access_placeholder: "kiln-placeholder-6163".to_owned(),
            refresh_placeholder: Some("kiln-placeholder-7266".to_owned()),
        })
    }
    fn refresh(&self, _sign_in: &str, _key: Option<[u8; 32]>) -> Result<Value, SignInRefused> {
        Ok(Value::new(self.secret.expose().to_vec()))
    }
}

/// A Shards image VM whose harness, `claude`, is granted its provider's credential by reference and signs in toward it.
fn harness(name: &str) -> BoxSpec {
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
        member: claude.clone(),
        reference: "providers/anthropic".to_owned(),
        toward: vec!["api.anthropic.com".to_owned()],
        placeholder: "kiln-placeholder-73333374657374".to_owned(),
    }];
    image.sign_ins = vec![hex_api::SignInGrant {
        member: claude,
        sign_in: "claude/ada".to_owned(),
        cli: hex_api::SignInCli::ClaudeCode,
        toward: vec!["api.anthropic.com".to_owned()],
        captured: None,
    }];
    BoxSpec {
        route: None,
        allocation: hex_api::sim::sim_vm_allocation().ok(),
        devices: Vec::new(),
        id: BoxId::new(name).unwrap(),
        bounds: hex_api::Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(image)),
        owner: None,
        band: 0,
    }
}

/// E-6, on this OS's socket: do run a harness VM whose run leases its credential (the client's `Leased`, into the
/// node), has its sign-in captured (the node's `CaptureAsk`, out to the client) and refreshed (the client's
/// `Refreshed`, in), each carrying the secret, then end it, its client and its node; expect every flow to have carried
/// the secret whole, and no block freed meanwhile, the node's, its socket's or its client's, to have held it.
#[test]
fn e6_no_block_freed_holds_a_secret_the_node_carried_on_its_socket() {
    let secret = secret();
    let dir = scratch();
    let watching = wipes::watch(&secret).unwrap();
    let captured = Arc::new(AtomicBool::new(false));
    {
        let held = Arc::new(Value::new(secret.to_vec()));
        let host = Arc::new(Secrets {
            sim: SimRuntime::default(),
            lessors: WireLessors::new(),
            secret: Arc::clone(&held),
        });
        let server = Server::bind(
            &dir.path().join("node.sock"),
            Arc::clone(&host) as Arc<dyn Host>,
            Limits {
                connections: 4,
                message_bytes: 4096,
                in_flight: 4,
                programs: 4,
            },
            8,
        )
        .unwrap();
        let client = Client::connect(server.path())
            .unwrap()
            .with_lessors(Arc::new(Broker {
                secret: held,
                captured: Arc::clone(&captured),
            }))
            .unwrap();
        let spec = harness("e6-harness");
        let id = spec.id.clone();
        client.create(spec).unwrap();
        client.start(&id).unwrap();
        client.signal(&id, Signal::KILL).unwrap();
        client.delete(&id).unwrap();
        drop((client, server, host));
    }
    assert!(
        captured.load(Ordering::Acquire),
        "the capture carried the secret whole to the client's broker"
    );
    assert_eq!(
        watching.found(),
        0,
        "freed blocks still holding the secret the node carried, (size, where) of each: {:?}",
        watching.seen()
    );
}

/// The test's broker, its run's flows' class and its harness VM, holding `held`, the secret's one value.
fn secrets(held: &Arc<Value>) -> Arc<Secrets> {
    Arc::new(Secrets {
        sim: SimRuntime::default(),
        lessors: WireLessors::new(),
        secret: Arc::clone(held),
    })
}

/// A peer's credentials, enrolled with the simulated CA under `name`.
fn enrolled(ca: &SimCa, name: &str) -> hex_quic::tls::Credentials {
    ca.admit(name, name.as_bytes());
    enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap()
}

/// E-6, over QUIC (docs/design/hex-quic.md Q13): as on the socket, a harness VM's lease, capture and refresh, each
/// carrying the secret, between the node and a client that reached it over QUIC, through hex-quic's control lane;
/// expect every flow to have carried the secret whole, and no block freed meanwhile, the node's, hex-quic's, hyper-quic's
/// and hyper-transport's beneath it, or its client's, to have held it: each message that carries one is sealed before
/// hex-quic's lanes, so what hyper-quic's send buffer and hyper-transport's receive pool hold of it is ciphertext.
#[test]
fn e6_no_block_freed_holds_a_secret_the_node_carried_over_quic() {
    let secret = secret();
    let dir = scratch();
    let watching = wipes::watch(&secret).unwrap();
    let captured = Arc::new(AtomicBool::new(false));
    {
        let held = Arc::new(Value::new(secret.to_vec()));
        let host = secrets(&held);
        let ca = SimCa::new().unwrap();
        let limits = Limits {
            connections: 4,
            message_bytes: hex_core::wire::MAX_MESSAGE,
            in_flight: 4,
            programs: 4,
        };
        let node = Arc::new(
            hex_quic::Endpoint::start(hex_quic::Config {
                credentials: enrolled(&ca, "node.wipes.test"),
                bind: std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
                listen: true,
                serves: hex_node::quic::ports(false),
                shape: hex_node::quic::shape(&limits, false),
            })
            .unwrap(),
        );
        let socket = dir.path().join("node.sock");
        let quic = hex_node::quic::Quic::new(
            Native::bind(&socket).unwrap(),
            Arc::clone(&node),
            hex_node::quic::Bounds::of(&limits),
            None,
        )
        .unwrap();
        let server = Server::with_os(
            quic,
            Arc::clone(&host) as Arc<dyn Host>,
            limits,
            limits.in_flight,
            &socket,
        )
        .unwrap();
        let client_end = Arc::new(
            hex_quic::Endpoint::start(hex_quic::Config {
                credentials: enrolled(&ca, "client.wipes.test"),
                bind: std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
                listen: false,
                serves: Vec::new(),
                shape: hex_quic::Shape {
                    connections: 1,
                    lanes: hex_quic::limits::client_lanes(limits.in_flight, false),
                },
            })
            .unwrap(),
        );
        let (client, _) = hex_quic::link::connect(
            client_end,
            "node.wipes.test",
            node.local_addr(),
            false,
            None,
        )
        .unwrap();
        let client = client
            .with_lessors(Arc::new(Broker {
                secret: held,
                captured: Arc::clone(&captured),
            }))
            .unwrap();
        let spec = harness("e6-quic-harness");
        let id = spec.id.clone();
        client.create(spec).unwrap();
        client.start(&id).unwrap();
        client.signal(&id, Signal::KILL).unwrap();
        client.delete(&id).unwrap();
        drop((client, server, host, node));
    }
    assert!(
        captured.load(Ordering::Acquire),
        "the capture carried the secret whole to the client's broker"
    );
    assert_eq!(
        watching.found(),
        0,
        "freed blocks still holding the secret the node carried over QUIC, (size, where) of each: {:?}",
        watching.seen()
    );
}
