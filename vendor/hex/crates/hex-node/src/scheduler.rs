//! CP-T7.4/LB03: file-backed production shell around the adapted peer kernel and Runtime driver.
//! The ordinary node socket carries control. Declared peers use authenticated SSH stdio tunnels;
//! no cluster service or listening TCP socket is required, including on a single laptop.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, Weak};

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, Proc, Program, Refusal, Runtime, Signal, Stdio, Usage,
};
use hex_core::{schedule as dto, wire};
use hex_sim::index::Pick;
use hex_sim::model::{Classes, Cluster, NodeId, NodeSpec};
use hex_sim::peer::{self, Key};
use serde::{Deserialize, Serialize};

use crate::placement::{Driver, Envelope, Error, Notice, Payload, Runtimes, Step};
use crate::serve::Host;

mod charges;
mod offer;
mod order;
mod stopping;
use stopping::Stopping;
mod cache;
mod capacity;
pub mod tunnel;

/// CLI-owned reporting: library delivery never chooses an output stream.
pub type Report = Arc<dyn Fn(NodeId, &str) + Send + Sync>;

/// Explicitly initialized runtime endpoints; class names describe the configured backend,
/// while its hello independently establishes which BoxSpec forms it implements.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub class: String,
    pub socket: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub node: u32,
    /// A host in the user's SSH configuration, including its identity and pinned host key.
    pub host: String,
    /// One explicitly configured remote command, normally `hex schedule tunnel --socket …`.
    /// SSH's remote command syntax belongs to the administrator, never to a submitted box.
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Shared, explicit cluster identity; its SHA256 scopes physical boxes and peer messages.
    pub cluster: String,
    #[serde(default)]
    pub node: u32,
    pub state: PathBuf,
    pub socket: PathBuf,
    /// Previously exchanged peer facts. The receiver always checks its own live facts as well.
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
    #[serde(default)]
    pub models: Vec<String>,
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// The machine's capacity view on a QUIC lane of its own (joint plan step 13): without it no view is kept and
    /// nothing is bound for one.
    #[serde(default)]
    pub capacity: Option<CapacityLane>,
}

/// The machine's capacity view served on a QUIC lane of its own (joint plan step 13, `docs/design/capacity-view.md`
/// CV9): where it binds, the enrollment it presents, and the holders it serves.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityLane {
    pub bind: std::net::SocketAddr,
    /// This scheduler's enrollment with kiln's CA: the folder `hex node enroll` began and the CA's answer completed
    /// (`hex_quic::enroll::store`).
    pub credentials: PathBuf,
    /// The holders it serves: no other peer is heard.
    pub holders: Vec<CapacityHolder>,
}

/// A holder the person authorizes on this machine: the name kiln's CA enrolled it as.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityHolder {
    pub name: String,
}

/// The capacity view's lane, where the configuration asks for one: its server, the endpoint it is served on, and the
/// log its events go to.
struct Lane {
    server: Arc<hex_quic::capacity::Server<crate::placement::DriverView>>,
    endpoint: hex_quic::Endpoint,
    log: hex_quic::capacity::Log,
}

impl Lane {
    /// `config`'s enrollment and holders, read and checked whole before anything opens or binds
    /// (`docs/design/hex-quic.md` Q9): an enrollment it can't serve with, or a holder list it can't serve, is refused.
    fn credentials(config: &CapacityLane) -> Result<hex_quic::tls::Credentials, Refusal> {
        let lane = |why: String| Refusal::Invalid {
            what: "capacity lane",
            why,
        };
        if config.holders.is_empty() {
            return Err(lane("it names no holder to serve".into()));
        }
        let mut names = std::collections::BTreeSet::new();
        for holder in &config.holders {
            if holder.name.is_empty() || !names.insert(holder.name.as_str()) {
                return Err(lane(format!(
                    "holder {:?} is empty or named twice",
                    holder.name
                )));
            }
        }
        hex_quic::enroll::store::load(&config.credentials).map_err(|e| {
            lane(format!(
                "{e} (enroll with `hex node enroll`, and keep kiln's CA's answer there)"
            ))
        })
    }

    /// Opens the view in `state` for `config`'s holders, binds its endpoint with `credentials`, and serves it there.
    fn serve(
        driver: &Arc<Driver>,
        state: &Path,
        config: &CapacityLane,
        credentials: hex_quic::tls::Credentials,
        log: hex_quic::capacity::Log,
    ) -> Result<Self, Refusal> {
        driver
            .open_view(&state.join("capacity"), config.holders.len())
            .map_err(failed)?;
        // Each holder holds a connection and the one replacing it after its path changed, and its link one connection
        // on the port and the one replacing it as it says hello again (docs/design/hex-quic.md Q8).
        let connections = config
            .holders
            .len()
            .checked_mul(2)
            .ok_or(Refusal::Exhausted {
                what: "capacity lane connections",
                bound: usize::MAX,
            })?;
        let names: Vec<String> = config.holders.iter().map(|h| h.name.clone()).collect();
        // A peer the lane doesn't name is refused at its handshake, so it holds none of these connections
        // (docs/design/hex-quic.md Q10).
        let endpoint = hex_quic::Endpoint::start_admitting(
            hex_quic::Config {
                credentials,
                bind: config.bind,
                listen: true,
                serves: vec![hex_quic::capacity::CAPACITY_PORT],
                shape: hex_quic::Shape {
                    connections,
                    lanes: 2,
                },
            },
            hex_quic::Admits::Named(names.iter().cloned().collect()),
        )
        .map_err(|e| Refusal::Invalid {
            what: "capacity lane",
            why: e.to_string(),
        })?;
        let server = hex_quic::capacity::Server::new(
            crate::placement::DriverView(Arc::clone(driver)),
            &names,
            Arc::clone(&log),
        );
        let acceptor = endpoint.acceptor(hex_quic::capacity::CAPACITY_PORT);
        let serving = Arc::clone(&server);
        let ended = Arc::clone(&log);
        std::thread::Builder::new()
            .name("hex-capacity".into())
            .spawn(move || match serving.serve(acceptor) {
                hex_quic::Refused::Ended => {}
                why => ended(&format!("hex.capacity event=lane_ended why={why:?}")),
            })
            .map_err(|e| Refusal::os("starting the capacity lane", e))?;
        Ok(Self {
            server,
            endpoint,
            log,
        })
    }

    /// Says every hint owed to the holders; a failure is logged, the holders' next hello telling them the view.
    fn hint(&self) {
        if let Err(why) = self.server.hint() {
            (self.log)(&format!("hex.capacity event=hint_failed why={why:?}"));
        }
    }
}

fn failed(error: Error) -> Refusal {
    match error {
        Error::Runtime(why) => why,
        Error::Core(peer::Stop::Full { what, bound }) => Refusal::Exhausted { what, bound },
        Error::Core(why) => Refusal::Broken(format!("placement records: {why:?}")),
    }
}

struct BackingActions {
    prepared: Vec<(hex_sim::model::Class, hex_api::BackingKey)>,
    draining: Vec<(hex_sim::model::Class, hex_api::BackingOffer)>,
    released: Vec<(hex_sim::model::Class, hex_api::BackingOffer)>,
}

impl BackingActions {
    fn from_reports(
        reports: &[(hex_sim::model::Class, Option<hex_api::Capacity>)],
    ) -> Result<Self, Refusal> {
        Ok(Self {
            prepared: Runtimes::prepared_backings(reports).map_err(failed)?,
            draining: Runtimes::draining_backings(reports).map_err(failed)?,
            released: Runtimes::released_backings(reports).map_err(failed)?,
        })
    }

    fn only_available(
        &mut self,
        reports: &[(hex_sim::model::Class, Option<hex_api::Capacity>)],
        unavailable: &[hex_api::BackendUnavailable],
    ) -> Result<(), Refusal> {
        let blocked = |key| -> Result<bool, Refusal> {
            for (_, report) in reports {
                let Some(report) = report else { continue };
                for offer in report
                    .routes
                    .iter()
                    .filter(|offer| offer.backing == Some(key))
                {
                    let backing = report.backings.iter().find(|backing| backing.key == key);
                    for observation in unavailable {
                        if observation.blocks_route(offer, backing)? {
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(false)
        };
        let mut refusal = None;
        self.prepared.retain(|(_, key)| {
            if refusal.is_some() {
                return true;
            }
            match blocked(*key) {
                Ok(blocked) => !blocked,
                Err(why) => {
                    refusal = Some(why);
                    true
                }
            }
        });
        // Draining cleanup and validated Released refunds are native lifecycle authority, not
        // fresh observation grants. Unread renderer telemetry cannot suppress either action.
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        Ok(())
    }
}

fn key(k: dto::Key) -> Key {
    Key {
        owner: k.owner,
        epoch: k.epoch,
        seq: k.seq,
    }
}

/// A word per admitted/recovered record, plus each peer's one coalesced fact refresh.
fn control_bound(records: usize, peers: usize) -> Result<usize, Refusal> {
    records.checked_add(peers).ok_or_else(|| Refusal::Invalid {
        what: "scheduler control budget",
        why: "record and peer counts exceed usize".into(),
    })
}
fn external(k: Key) -> dto::Key {
    dto::Key {
        owner: k.owner,
        epoch: k.epoch,
        seq: k.seq,
    }
}

fn encoded_refusal(why: &Refusal) -> Result<Vec<u8>, Refusal> {
    wire::answer(0, &wire::Answer::Refused(why.clone()), wire::MAX_MESSAGE).map_err(Into::into)
}

fn drain_failure(key: hex_api::BackingKey, why: Refusal) -> Result<Refusal, Refusal> {
    match why {
        Refusal::Backing {
            key: actual,
            cause:
                cause @ (hex_api::LifecycleCause::Pending
                | hex_api::LifecycleCause::Busy
                | hex_api::LifecycleCause::Uncertain),
        } if actual == key => Ok(Refusal::Backing { key, cause }),
        other => Err(other),
    }
}

fn encoded_cleanup_failures(failures: Vec<Refusal>) -> Result<Vec<Vec<u8>>, Refusal> {
    if failures.len() > hex_api::MAX_RUNTIME_ROUTES {
        return Err(capacity::invalid(
            "cleanup diagnostics exceed the route catalog bound",
        ));
    }
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(failures.len())
        .map_err(|_| Refusal::Exhausted {
            what: "backing cleanup diagnostics",
            bound: hex_api::MAX_RUNTIME_ROUTES,
        })?;
    for failure in failures {
        encoded.push(encoded_refusal(&failure)?);
    }
    Ok(encoded)
}

/// Bootstrap has no control reply yet; the existing CLI reporter receives the same exact identity
/// and qualified cause. Fixed hexadecimal widths make its allocation bound independent of input.
fn cleanup_report(why: &Refusal) -> Result<String, Refusal> {
    let Refusal::Backing { key, cause } = why else {
        return Err(capacity::invalid(
            "cleanup diagnostic is not a backing refusal",
        ));
    };
    let cause = match cause {
        hex_api::LifecycleCause::Pending => "pending",
        hex_api::LifecycleCause::Busy => "busy",
        hex_api::LifecycleCause::Uncertain => "uncertain",
    };
    let prefix = "retained native backing cleanup pool=";
    let generation = " generation=";
    let reason = " cause=";
    // Two hexadecimal characters per byte of the existing fixed-width backing identity.
    let bound = key
        .pool
        .len()
        .checked_add(key.generation.len())
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add(prefix.len()))
        .and_then(|n| n.checked_add(generation.len()))
        .and_then(|n| n.checked_add(reason.len()))
        .and_then(|n| n.checked_add(cause.len()))
        .ok_or_else(|| capacity::invalid("cleanup diagnostic byte count overflows"))?;
    let mut text = String::new();
    text.try_reserve_exact(bound)
        .map_err(|_| Refusal::Exhausted {
            what: "backing cleanup diagnostic bytes",
            bound,
        })?;
    use std::fmt::Write as _;
    text.push_str(prefix);
    for byte in key.pool {
        write!(text, "{byte:02x}")
            .map_err(|_| capacity::invalid("cleanup identity rendering failed"))?;
    }
    text.push_str(generation);
    for byte in key.generation {
        write!(text, "{byte:02x}")
            .map_err(|_| capacity::invalid("cleanup identity rendering failed"))?;
    }
    text.push_str(reason);
    text.push_str(cause);
    Ok(text)
}

fn packet(
    envelope: &Envelope,
    models: &[String],
    epoch: u64,
    seq: u64,
    observations: Vec<dto::LaunchObservation>,
    realm: &str,
) -> Result<dto::Packet, Refusal> {
    Ok(dto::Packet {
        cluster: realm.into(),
        epoch,
        seq,
        observations,
        message: peer::encode_message(&envelope.msg, models, wire::MAX_MESSAGE)
            .map_err(|e| Refusal::Broken(format!("encoding placement message: {e:?}")))?,
        payload: envelope.payload.as_ref().map(|p| dto::Payload {
            spec: p.spec.clone(),
            start: p.start,
        }),
        refusal: envelope.refusal.as_ref().map(encoded_refusal).transpose()?,
    })
}

fn envelope(packet: dto::Packet, models: &[String]) -> Result<Envelope, Refusal> {
    let refusal = packet
        .refusal
        .map(|bytes| {
            let received = wire::Received {
                bytes: &bytes,
                size: bytes.len(),
                descriptors: 0,
                descriptors_cut: false,
            };
            match wire::read_answer(&received, wire::MAX_MESSAGE)
                .map_err(Refusal::from)?
                .1
            {
                wire::Answer::Refused(why) => Ok(why),
                _ => Err(Refusal::Invalid {
                    what: "peer refusal",
                    why: "expected a typed Refused answer".into(),
                }),
            }
        })
        .transpose()?;
    Ok(Envelope {
        msg: peer::decode_message(&packet.message, models, wire::MAX_MESSAGE).map_err(|e| {
            Refusal::Invalid {
                what: "placement message",
                why: format!("{e:?}"),
            }
        })?,
        payload: packet.payload.map(|p| Payload {
            spec: p.spec,
            start: p.start,
        }),
        refusal,
    })
}

struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct Queued {
    envelope: Option<Envelope>,
    observations: Vec<dto::LaunchObservation>,
    _permit: Permit,
    _refresh: Option<RefreshPermit>,
}

struct RefreshPermit(Arc<AtomicBool>);
impl Drop for RefreshPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct PeerLane {
    peer: Peer,
    id: NodeId,
    realm: String,
    models: Vec<String>,
    epoch: u64,
    stopping: Arc<Stopping>,
    report: Report,
    source: Arc<Mutex<Weak<Service>>>,
}

fn run_peer(lane: PeerLane, receive: Receiver<Queued>) {
    run_peer_with(
        lane,
        receive,
        tunnel::Connection::open,
        tunnel::Connection::call,
    )
}

// The factory/call seam keeps the production worker and its retry transitions under deterministic
// publication-gate tests without opening SSH or replacing transport policy.
fn run_peer_with<C>(
    lane: PeerLane,
    receive: Receiver<Queued>,
    open: impl FnMut(&Peer, &Arc<Stopping>) -> Result<C, Refusal>,
    call: impl FnMut(&mut C, dto::Command) -> Result<Result<dto::Reply, Refusal>, Refusal>,
) {
    run_peer_with_time(
        lane,
        receive,
        open,
        call,
        std::time::Instant::now,
        |stopping, pace| stopping.wait_retry(pace),
    )
}

// Tests advance only requested pauses; operation cost is fixed independently of those pauses.
fn run_peer_with_time<C>(
    lane: PeerLane,
    receive: Receiver<Queued>,
    mut open: impl FnMut(&Peer, &Arc<Stopping>) -> Result<C, Refusal>,
    mut call: impl FnMut(&mut C, dto::Command) -> Result<Result<dto::Reply, Refusal>, Refusal>,
    mut now: impl FnMut() -> std::time::Instant,
    mut wait: impl FnMut(&Stopping, std::time::Duration) -> std::io::Result<bool>,
) {
    let PeerLane {
        peer,
        id,
        realm,
        models,
        epoch,
        stopping,
        report,
        source,
    } = lane;
    let mut tunnel = None;
    let mut seq = 0u64;
    while let Ok(queued) = receive.recv() {
        let mut reconcile = false;
        let mut pace = std::time::Duration::ZERO;
        loop {
            if stopping.is_stopped() {
                return;
            }
            let mut began = now();
            let fresh = tunnel.is_none();
            if fresh {
                match open(&peer, &stopping) {
                    Ok(connection) => tunnel = Some(connection),
                    Err(why) => {
                        report(NodeId(peer.node), &why.to_string());
                        pace = pace.max(now().saturating_duration_since(began));
                        if wait(&stopping, pace).unwrap_or(true) {
                            return;
                        }
                        continue;
                    }
                }
            }
            let Some(connection) = tunnel.as_mut() else {
                return;
            };
            if fresh || queued.envelope.is_none() {
                match call(
                    connection,
                    dto::Command::Describe {
                        from: id.0,
                        cluster: realm.clone(),
                    },
                ) {
                    Ok(Ok(reply)) => {
                        if let Some(offer) = reply.offer {
                            loop {
                                let result = {
                                    let Some(service) =
                                        source.lock().ok().and_then(|s| s.upgrade())
                                    else {
                                        return;
                                    };
                                    service.command(dto::Command::Advertise {
                                        from: peer.node,
                                        cluster: realm.clone(),
                                        offer: offer.clone(),
                                        observations: reply.observations.clone(),
                                    })
                                };
                                match result {
                                    Ok(_) => break,
                                    Err(Refusal::Exhausted {
                                        what: "scheduler command publication",
                                        ..
                                    }) => {
                                        pace = pace.max(now().saturating_duration_since(began));
                                        if wait(&stopping, pace).unwrap_or(true) {
                                            return;
                                        }
                                        // AC-T7.4.1/r423: this inner retry stays in the same
                                        // outer attempt; its own pause is not operation cost.
                                        began = now();
                                    }
                                    Err(why) => {
                                        report(NodeId(peer.node), &why.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(why)) => report(NodeId(peer.node), &why.to_string()),
                    Err(why) => {
                        report(NodeId(peer.node), &why.to_string());
                        tunnel = None;
                        pace = pace.max(now().saturating_duration_since(began));
                        if wait(&stopping, pace).unwrap_or(true) {
                            return;
                        }
                        continue;
                    }
                }
            }
            let Some(envelope) = queued.envelope.as_ref() else {
                break;
            };
            if let peer::Msg::Start { key, .. } = envelope.msg {
                let status = {
                    let Some(service) = source.lock().ok().and_then(|s| s.upgrade()) else {
                        return;
                    };
                    service.driver.status(key)
                };
                match status {
                    Ok(Some(status))
                        if !status.deleting && status.node == Some(NodeId(peer.node)) => {}
                    Ok(_) => {
                        reconcile = true;
                        break;
                    }
                    Err(why) => {
                        report(NodeId(peer.node), &why.to_string());
                        break;
                    }
                }
            }
            let Some(next) = seq.checked_add(1) else {
                report(NodeId(peer.node), "peer sequence exhausted");
                return;
            };
            seq = next;
            let packet = match packet(
                envelope,
                &models,
                epoch,
                seq,
                queued.observations.clone(),
                &realm,
            ) {
                Ok(packet) => packet,
                Err(why) => {
                    report(NodeId(peer.node), &why.to_string());
                    break;
                }
            };
            let Some(connection) = tunnel.as_mut() else {
                return;
            };
            match call(connection, dto::Command::Hear { from: id.0, packet }) {
                Ok(Ok(_)) => break,
                Ok(Err(Refusal::Exhausted {
                    what: "scheduler command publication",
                    ..
                })) => {
                    pace = pace.max(now().saturating_duration_since(began));
                    if wait(&stopping, pace).unwrap_or(true) {
                        return;
                    }
                    continue;
                }
                Ok(Err(why)) => {
                    // A typed application refusal is not a transport retry. Keep ownership and let
                    // later Delete/Holds progress; it alone is not proof that the box is absent.
                    report(NodeId(peer.node), &why.to_string());
                    break;
                }
                Err(why) => {
                    report(NodeId(peer.node), &why.to_string());
                    tunnel = None;
                    pace = pace.max(now().saturating_duration_since(began));
                    if wait(&stopping, pace).unwrap_or(true) {
                        return;
                    }
                    // Check the durable owner before any retry; cancellation or reassignment
                    // suppresses an obsolete queued Start, while newer control remains queued.
                    continue;
                }
            }
        }
        drop(queued);
        if reconcile {
            loop {
                let began = now();
                let result = {
                    let Some(service) = source.lock().ok().and_then(|s| s.upgrade()) else {
                        return;
                    };
                    service.command(dto::Command::LinkUp { peer: peer.node })
                };
                match result {
                    Ok(_) => break,
                    Err(Refusal::Exhausted {
                        what: "scheduler command publication",
                        ..
                    }) => {
                        pace = pace.max(now().saturating_duration_since(began));
                        if wait(&stopping, pace).unwrap_or(true) {
                            return;
                        }
                    }
                    Err(why) => {
                        report(NodeId(peer.node), &why.to_string());
                        break;
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct Dispatch {
    refusals: Vec<Vec<u8>>,
    notices: Vec<Notice>,
}

/// A scheduler socket serves control only. Box execution continues through its configured Runtime
/// endpoints, so a caller cannot bypass placement admission by calling Create on this socket.
pub struct Service {
    id: NodeId,
    realm: String,
    command: Mutex<()>,
    fault: Mutex<Option<Refusal>>,
    cache: Mutex<cache::Cache>,
    _lease: std::fs::File,
    order: Mutex<order::Order>,
    driver: Arc<Driver>,
    /// The capacity view's lane, where the configuration asks for one (joint plan step 13).
    lane: Option<Lane>,
    runtimes: Runtimes,
    cluster: Mutex<Cluster>,
    facts_dir: std::path::PathBuf,
    initialized: Classes,
    physical_authority: Option<String>,
    stopping: Arc<Stopping>,
    queued: Arc<AtomicUsize>,
    control_limit: AtomicUsize,
    senders: BTreeMap<NodeId, Sender<Queued>>,
    refreshing: BTreeMap<NodeId, Arc<AtomicBool>>,
}

impl Service {
    pub fn open(config: &Config, report: Report) -> Result<Arc<Self>, Refusal> {
        // The capacity lane's enrollment and holders are read whole before anything opens or binds.
        let capacity = config
            .capacity
            .as_ref()
            .map(|lane| Lane::credentials(lane).map(|credentials| (lane, credentials)))
            .transpose()?;
        if config.cluster.trim().is_empty() || config.cluster.trim() != config.cluster {
            return Err(Refusal::Invalid {
                what: "scheduler cluster",
                why: "declare a nonempty shared cluster identity without surrounding whitespace"
                    .into(),
            });
        }
        use sha2::Digest;
        let namespace = format!("{:x}", sha2::Sha256::digest(config.cluster.as_bytes()));
        std::fs::create_dir_all(&config.state)
            .map_err(|e| Refusal::os("creating scheduler state", e))?;
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.state.join("scheduler.lock"))
            .map_err(|e| Refusal::os("opening scheduler state lease", e))?;
        lease.try_lock().map_err(|e| {
            Refusal::Broken(format!(
                "scheduler state is already held or cannot be locked: {e}"
            ))
        })?;
        let mut cluster = Cluster::single_owner(config.nodes.clone(), Vec::new());
        cluster.models = config.models.clone();
        let id = NodeId(config.node);
        if cluster.nodes.is_empty() {
            let ids: std::collections::BTreeSet<_> =
                config.peers.iter().map(|p| p.node).chain([id.0]).collect();
            let count = u32::try_from(ids.len()).map_err(|e| Refusal::Invalid {
                what: "scheduler nodes",
                why: e.to_string(),
            })?;
            if ids.len() != config.peers.len().saturating_add(1) || ids.iter().copied().ne(0..count)
            {
                return Err(Refusal::Invalid {
                    what: "scheduler nodes",
                    why: "node IDs must be dense from zero, with one route per remote node".into(),
                });
            }
            cluster.nodes = ids
                .into_iter()
                .map(|id| {
                    let mut spec = NodeSpec::new(NodeId(id), hex_sim::model::Resources::default());
                    spec.classes = Classes::NONE;
                    spec
                })
                .collect();
        }
        if !cluster.nodes.iter().any(|node| node.id == id) {
            return Err(Refusal::Invalid {
                what: "scheduler node",
                why: "own ID is absent from the configured peer facts".into(),
            });
        }
        let remote: std::collections::BTreeSet<_> = cluster
            .nodes
            .iter()
            .filter(|n| n.id != id)
            .map(|n| n.id.0)
            .collect();
        let routes: std::collections::BTreeSet<_> = config.peers.iter().map(|p| p.node).collect();
        if routes != remote || routes.len() != config.peers.len() {
            return Err(Refusal::Invalid {
                what: "scheduler peers",
                why: "configure exactly one authenticated route for every remote node".into(),
            });
        }
        let order = order::Order::open_with_peers(&config.state.join("transport"), &routes)?;
        let epoch = order.epoch;
        let cache = cache::Cache::open(
            &config.state.join("peers"),
            &routes,
            &mut cluster,
            |aside| {
                report(aside.peer().unwrap_or(id), &aside.to_string());
            },
        )?;
        let mut classes = Classes::NONE;
        let mut backends = Vec::new();
        for backend in &config.backends {
            let class = match backend.class.as_str() {
                "oci" => hex_sim::model::Class::Oci,
                "shards" => hex_sim::model::Class::Shards,
                "firecracker" => hex_sim::model::Class::Firecracker,
                other => {
                    return Err(Refusal::Invalid {
                        what: "runtime class",
                        why: other.into(),
                    });
                }
            };
            let runtime =
                Arc::new(hex_client::Client::connect(&backend.socket)?) as Arc<dyn Runtime>;
            classes = classes.with(class);
            backends.push((class, runtime));
        }
        let runtimes = Runtimes::new(backends).map_err(failed)?;
        let mut snapshot = runtimes.capacity_snapshot().map_err(failed)?;
        let backing_owned = runtimes
            .measured_available_backing_use(&mut snapshot)
            .map_err(failed)?;
        std::fs::create_dir_all(&config.state)
            .map_err(|e| Refusal::os("creating scheduler state", e))?;
        let (facts, physical_authority, unavailable) =
            runtime_observations(&config.state, classes, &runtimes)?;
        for observation in unavailable {
            snapshot.add_unavailable(observation)?;
        }
        let mut backing_actions = BackingActions::from_reports(&snapshot.reports)?;
        backing_actions.only_available(&snapshot.reports, &snapshot.unavailable)?;
        let mut facts_dir = std::path::PathBuf::new();
        facts_dir
            .try_reserve_exact(config.state.as_os_str().len())
            .map_err(|_| Refusal::Exhausted {
                what: "scheduler machine facts path bytes",
                bound: config.state.as_os_str().len(),
            })?;
        facts_dir.push(&config.state);
        let local = facts
            .node(id, &mut cluster)
            .map_err(|e| Refusal::Broken(e.to_string()))?;
        let mut local = local;
        // Joint plan step 14 (C2): what the node's learner carried, taken before the answers are applied.
        let learned = charges::learned(&snapshot.reports)?;
        capacity::apply(
            snapshot.reports,
            &facts,
            &mut cluster,
            &namespace,
            &mut local,
            &BTreeMap::new(),
            backing_owned,
        )?;
        capacity::retain_unavailable(&mut local, None, &snapshot.unavailable)?;
        charges::apply(&mut local, &learned)?;
        // With a capacity lane, the room measured here is the Driver's before its view opens, so a keeper started, or
        // started again after a kill (CV11), decides a holder's first reserve in the room there is, no command of Hex's
        // needed to measure it first (joint plan step 13).
        let measured = if capacity.is_some() {
            Some(local.try_clone().map_err(|_| Refusal::Exhausted {
                what: "local node snapshot",
                bound: 1,
            })?)
        } else {
            None
        };
        if let Some(slot) = cluster.nodes.iter_mut().find(|node| node.id == id) {
            *slot = local;
        }
        let driver = Arc::new(
            Driver::boot_in(
                crate::placement::Identity {
                    namespace: &namespace,
                    node: id,
                },
                &cluster.nodes,
                &cluster.models,
                crate::placement::budget::runtime_budget(id, &cluster.nodes)?,
                Pick::default(),
                &config.state.join("placements"),
                runtimes.clone(),
            )
            .map_err(failed)?,
        );
        if let Some(measured) = measured {
            driver.refresh_node(measured).map_err(failed)?;
        }
        // The view opens before any admission this process makes, so none is made without it (CV1).
        let lane = match capacity {
            Some((lane, credentials)) => {
                let report = Arc::clone(&report);
                let log: hex_quic::capacity::Log = Arc::new(move |line: &str| report(id, line));
                Some(Lane::serve(&driver, &config.state, lane, credentials, log)?)
            }
            None => None,
        };
        let control_limit =
            control_bound(driver.control_bound().map_err(failed)?, config.peers.len())?;
        let mut senders = BTreeMap::new();
        let mut refreshing = BTreeMap::new();
        let queued = Arc::new(AtomicUsize::new(0));
        let stopping = Arc::new(Stopping::new(config.peers.iter().map(|p| p.node)));
        let source = Arc::new(Mutex::new(Weak::<Service>::new()));
        for peer in &config.peers {
            let node = NodeId(peer.node);
            if node == id
                || !cluster.nodes.iter().any(|n| n.id == node)
                || senders.contains_key(&node)
            {
                return Err(Refusal::Invalid {
                    what: "scheduler peer",
                    why: format!("invalid or duplicate node {}", peer.node),
                });
            }
            // One global Permit bounds queued and in-flight words across every peer. Linked
            // storage avoids reserving that full population separately for each idle peer.
            let (send, receive) = channel::<Queued>();
            let peer = peer.clone();
            let realm = config.cluster.clone();
            let models = cluster.models.clone();
            let stopping = Arc::clone(&stopping);
            let report = Arc::clone(&report);
            let source = Arc::clone(&source);
            std::thread::Builder::new()
                .name(format!("hex-peer-{}", peer.node))
                .spawn(move || {
                    run_peer(
                        PeerLane {
                            peer,
                            id,
                            realm,
                            models,
                            epoch,
                            stopping,
                            report,
                            source,
                        },
                        receive,
                    )
                })
                .map_err(|e| Refusal::os("starting authenticated peer delivery", e))?;
            refreshing.insert(node, Arc::new(AtomicBool::new(false)));
            senders.insert(node, send);
        }
        let service = Arc::new(Self {
            id,
            realm: config.cluster.clone(),
            command: Mutex::new(()),
            fault: Mutex::new(None),
            cache: Mutex::new(cache),
            _lease: lease,
            order: Mutex::new(order),
            driver,
            lane,
            runtimes,
            cluster: Mutex::new(cluster),
            facts_dir,
            initialized: classes,
            physical_authority,
            stopping,
            queued,
            control_limit: AtomicUsize::new(control_limit),
            senders,
            refreshing,
        });
        *source
            .lock()
            .map_err(|_| Refusal::Broken("peer feedback lock poisoned".into()))? =
            Arc::downgrade(&service);
        {
            // Bootstrap follows the same transition-through-publication rule before peer feedback
            // or a bound listener can start another operation.
            let _initial = service
                .command
                .lock()
                .map_err(|_| Refusal::Broken("scheduler command lock poisoned".into()))?;
            for failure in service.initialize_backings(backing_actions)? {
                report(id, &cleanup_report(&failure)?);
            }
            service.refresh_peers()?;
            let recovered = service.driver.take_recovered().map_err(failed)?;
            service.dispatch(recovered)?;
        }
        Ok(service)
    }

    /// Where the capacity view's lane is bound, where the configuration asks for one.
    pub fn capacity_address(&self) -> Option<std::net::SocketAddr> {
        self.lane.as_ref().map(|lane| lane.endpoint.local_addr())
    }

    /// Cancels and reaps only this service's authenticated peer children.
    pub fn shutdown(&self) -> Result<(), Refusal> {
        self.stopping
            .stop()
            .map_err(|e| Refusal::os("stopping scheduler peer connections", e))
    }

    fn check_realm(&self, realm: &str) -> Result<(), Refusal> {
        if realm == self.realm {
            Ok(())
        } else {
            Err(Refusal::Invalid {
                what: "scheduler cluster",
                why: "peer belongs to a different configured cluster".into(),
            })
        }
    }

    fn observations(&self) -> Result<Vec<dto::LaunchObservation>, Refusal> {
        Ok(self
            .driver
            .start_observations()
            .map_err(failed)?
            .into_iter()
            .filter(|row| row.node == self.id)
            .map(|row| dto::LaunchObservation {
                class: match row.class {
                    hex_sim::model::Class::Oci => "oci",
                    hex_sim::model::Class::Firecracker => "firecracker",
                    hex_sim::model::Class::Shards => "shards",
                }
                .into(),
                sum_ns: row.sum_ns.to_string(),
                count: row.count,
            })
            .collect())
    }

    fn merge_observations(
        &self,
        from: u32,
        rows: &[dto::LaunchObservation],
    ) -> Result<(), Refusal> {
        let classes =
            usize::try_from(Classes::ALL.count()).map_err(|e| Refusal::Broken(e.to_string()))?;
        if rows.len() > classes {
            return Err(Refusal::Exhausted {
                what: "peer launch classes",
                bound: classes,
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        let observations = rows
            .iter()
            .map(|row| {
                let class = match row.class.as_str() {
                    "oci" => hex_sim::model::Class::Oci,
                    "firecracker" => hex_sim::model::Class::Firecracker,
                    "shards" => hex_sim::model::Class::Shards,
                    _ => {
                        return Err(Refusal::Invalid {
                            what: "peer launch class",
                            why: row.class.clone(),
                        });
                    }
                };
                let sum_ns = row.sum_ns.parse::<u128>().map_err(|e| Refusal::Invalid {
                    what: "peer launch sum",
                    why: e.to_string(),
                })?;
                if !seen.insert(class)
                    || row.count == 0
                    || sum_ns > u128::from(u64::MAX).saturating_mul(u128::from(row.count))
                {
                    return Err(Refusal::Invalid {
                        what: "peer launch aggregate",
                        why: "duplicate class or inconsistent aggregate".into(),
                    });
                }
                Ok(hex_sim::start_cost::StartObservation {
                    node: NodeId(from),
                    class,
                    sum_ns,
                    count: row.count,
                })
            })
            .collect::<Result<Vec<_>, Refusal>>()?;
        self.driver
            .merge_peer_start_observations(NodeId(from), &observations)
            .map_err(failed)
    }

    fn refresh_peers(&self) -> Result<(), Refusal> {
        let limit = self.control_limit.load(Ordering::Acquire);
        for (node, sender) in &self.senders {
            let refreshing = self
                .refreshing
                .get(node)
                .ok_or_else(|| Refusal::Broken("peer refresh gate absent".into()))?;
            if refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let refresh = RefreshPermit(Arc::clone(refreshing));
            self.queued
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    n.checked_add(1).filter(|n| *n <= limit)
                })
                .map_err(|_| Refusal::Exhausted {
                    what: "queued peer messages",
                    bound: limit,
                })?;
            let queued = Queued {
                envelope: None,
                observations: Vec::new(),
                _permit: Permit(Arc::clone(&self.queued)),
                _refresh: Some(refresh),
            };
            sender
                .send(queued)
                .map_err(|_| Refusal::Broken("scheduler peer worker disconnected".into()))?;
        }
        Ok(())
    }

    fn dispatch(&self, step: Step) -> Result<Dispatch, Refusal> {
        let limit = self.control_limit.load(Ordering::Acquire);
        let mut result = Dispatch::default();
        let mut work = VecDeque::from([step]);
        while let Some(mut step) = work.pop_front() {
            // Independent boxes' deletion controls take freed transport room before new work.
            step.send
                .sort_by_key(|(_, envelope)| !matches!(envelope.msg, peer::Msg::Delete { .. }));
            for notice in step.notices {
                match &notice {
                    Notice::Refused { refusal, .. } => {
                        result.refusals.push(encoded_refusal(refusal)?)
                    }
                    Notice::Unknown {
                        refusal, cleanup, ..
                    } => {
                        result.refusals.push(encoded_refusal(refusal)?);
                        if let Some(cleanup) = cleanup {
                            result.refusals.push(encoded_refusal(cleanup)?);
                        }
                    }
                    _ => {}
                }
                result.notices.push(notice);
            }
            for (node, envelope) in step.send {
                if node == self.id {
                    if work.len() >= limit {
                        return Err(Refusal::Exhausted {
                            what: "local placement transitions",
                            bound: limit,
                        });
                    }
                    work.push_back(self.driver.hear(self.id, envelope).map_err(failed)?);
                    continue;
                }
                let delivery = self
                    .senders
                    .get(&node)
                    .ok_or_else(|| Refusal::Invalid {
                        what: "scheduler peer",
                        why: format!("no authenticated route to node {}", node.0),
                    })
                    .and_then(|sender| {
                        self.queued
                            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                n.checked_add(1).filter(|n| *n <= limit)
                            })
                            .map_err(|_| Refusal::Exhausted {
                                what: "queued peer messages",
                                bound: limit,
                            })?;
                        let permit = Permit(Arc::clone(&self.queued));
                        let observations = self.observations()?;
                        let queued = Queued {
                            envelope: Some(envelope),
                            observations,
                            _permit: permit,
                            _refresh: None,
                        };
                        sender.send(queued).map_err(|_| {
                            Refusal::Broken("scheduler peer worker disconnected".into())
                        })?;
                        Ok(())
                    });
                if let Err(why) = delivery {
                    result.refusals.push(encoded_refusal(&why)?);
                }
            }
        }
        Ok(result)
    }

    fn refresh(&self) -> Result<Vec<Refusal>, Refusal> {
        let prepared = self.refresh_snapshot()?;
        self.initialize_backings(prepared)
    }

    /// One bounded pass over actual canonical lifecycle offers. Native cleanup finishes before
    /// Index refund and its separate ACK; new boot still consumes the same Index grant.
    fn initialize_backings(&self, actions: BackingActions) -> Result<Vec<Refusal>, Refusal> {
        if actions.draining.len() > hex_api::MAX_RUNTIME_ROUTES {
            return Err(capacity::invalid(
                "draining actions exceed the route catalog bound",
            ));
        }
        let mut failures = Vec::new();
        failures
            .try_reserve_exact(actions.draining.len())
            .map_err(|_| Refusal::Exhausted {
                what: "backing cleanup diagnostics",
                bound: hex_api::MAX_RUNTIME_ROUTES,
            })?;
        let mut initialized = false;
        for (endpoint, released) in actions.released {
            self.driver
                .release_backing(endpoint, &released)
                .map_err(failed)?;
            initialized = true;
        }
        for (endpoint, draining) in actions.draining {
            let released = match self.driver.drain_backing(endpoint, &draining) {
                Ok(released) => released,
                Err(why) => {
                    // A qualified failure leaves the SAME Index's full charge and claims intact.
                    // It is observable in this operation's reply; no Released ACK is attempted.
                    failures.push(drain_failure(draining.key, failed(why))?);
                    continue;
                }
            };
            self.driver
                .release_backing(endpoint, &released)
                .map_err(failed)?;
            initialized = true;
        }
        for (endpoint, key) in actions.prepared {
            // Ok(false) performs no native effect: the canonical typed Prepared receipt stays
            // observable in capacity. Leave it pending for an ordinary refresh without blocking
            // unrelated CPU work. Actual initialization errors still propagate to the caller.
            initialized |= self
                .driver
                .initialize_backing(endpoint, key)
                .map_err(failed)?;
        }
        if initialized {
            // Do not recursively initialize any new generation discovered in the result. Its
            // next ordinary refresh takes a new bounded pass after the current operation ends.
            self.refresh_snapshot()?;
        }
        Ok(failures)
    }

    fn refresh_snapshot(&self) -> Result<BackingActions, Refusal> {
        let owned_by_class = self.driver.measured_owned_use_by_class().map_err(failed)?;
        let mut snapshot = self.runtimes.capacity_snapshot().map_err(failed)?;
        let backing_owned = self
            .runtimes
            .measured_available_backing_use(&mut snapshot)
            .map_err(failed)?;
        let owned = owned_by_class
            .values()
            .try_fold(hex_sim::model::Resources::default(), |used, more| {
                used.checked_add(more)
                    .ok_or_else(|| Refusal::Broken("measured owned resource sum overflows".into()))
            })?
            .checked_add(&backing_owned)
            .ok_or_else(|| capacity::invalid("measured native resource sum overflows"))?;
        // All native RPCs finish before acquiring the cluster/model lock. The scheduling process
        // owns no GPU provider, and a changed Store/turn owner cannot inherit retained claims.
        let (facts, physical_authority, unavailable) =
            runtime_observations(&self.facts_dir, self.initialized, &self.runtimes)?;
        for observation in unavailable {
            snapshot.add_unavailable(observation)?;
        }
        let mut actions = BackingActions::from_reports(&snapshot.reports)?;
        actions.only_available(&snapshot.reports, &snapshot.unavailable)?;
        if physical_authority != self.physical_authority {
            return Err(Refusal::Invalid {
                what: "native physical report authority",
                why: "the initialized Store/turn owner changed during scheduler lifetime".into(),
            });
        }
        let mut cluster = self
            .cluster
            .lock()
            .map_err(|_| Refusal::Broken("scheduler model lock poisoned".into()))?;
        let previous = cluster
            .nodes
            .iter()
            .find(|node| node.id == self.id)
            .map(NodeSpec::try_clone)
            .transpose()
            .map_err(|_| Refusal::Exhausted {
                what: "retained local node view",
                bound: 1,
            })?;
        let mut local = facts
            .node_with_owned(self.id, &mut cluster, owned)
            .map_err(|e| Refusal::Broken(e.to_string()))?;
        // Joint plan step 14 (C2): what the node's learner carried, taken before the answers are applied.
        let learned = charges::learned(&snapshot.reports)?;
        capacity::apply(
            snapshot.reports,
            &facts,
            &mut cluster,
            self.driver.namespace(),
            &mut local,
            &owned_by_class,
            backing_owned,
        )?;
        capacity::retain_unavailable(&mut local, previous.as_ref(), &snapshot.unavailable)?;
        charges::apply(&mut local, &learned)?;
        self.driver
            .refresh_node(local.try_clone().map_err(|_| Refusal::Exhausted {
                what: "local node snapshot",
                bound: 1,
            })?)
            .map_err(failed)?;
        // Joint plan step 14 (C8): each VM at the holding its node's learner carried, booked against its key.
        let ids = self.driver.running_ids().map_err(failed)?;
        self.driver
            .rebook(&charges::holds(&learned, &ids)?)
            .map_err(failed)?;
        if let Some(node) = cluster.nodes.iter_mut().find(|n| n.id == self.id) {
            *node = local;
        }
        self.rebudget(&cluster)?;
        Ok(actions)
    }

    /// The person's release of `holder`'s reservations (joint plan step 13, CV6), logged as `hex.capacity
    /// event=forgotten`; refused, typed, while it is connected, for a name never seen, and without a lane.
    fn forget(&self, holder: &str) -> Result<dto::Reply, Refusal> {
        let Some(lane) = &self.lane else {
            return Err(Refusal::Invalid {
                what: "capacity view",
                why: "this scheduler's configuration declares no capacity lane".into(),
            });
        };
        let forget = match self.driver.view_forget(holder).map_err(failed)? {
            Ok(released) => {
                let sum = released.sum;
                (lane.log)(&format!(
                    "hex.capacity event=forgotten holder={holder:?} epoch={} count={} cpu_milli={} mem_bytes={} \
                     disk_bytes={} gpus={}",
                    released.epoch,
                    released.count,
                    sum.cpu_milli,
                    sum.mem_bytes,
                    sum.disk_bytes,
                    sum.gpus
                ));
                dto::Forget::Released {
                    holder: holder.to_owned(),
                    epoch: released.epoch,
                    count: released.count,
                    sum,
                }
            }
            Err(hex_api::capacity::NotForgotten::Connected) => dto::Forget::Connected,
            Err(hex_api::capacity::NotForgotten::Unknown) => dto::Forget::Unknown,
        };
        Ok(dto::Reply {
            forget: Some(forget),
            ..dto::Reply::default()
        })
    }

    fn rebudget(&self, cluster: &Cluster) -> Result<(), Refusal> {
        let budget = crate::placement::budget::runtime_budget(self.id, &cluster.nodes)?;
        let candidate = budget
            .owned
            .checked_add(budget.ran)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Refusal::Invalid {
                what: "scheduler control budget",
                why: "record counts exceed usize".into(),
            })?;
        // Refuse an unrepresentable control ceiling before changing admission.
        let previous = self.driver.control_bound().map_err(failed)?;
        control_bound(control_bound(candidate, previous)?, self.senders.len())?;
        let records = self.driver.set_budget(budget).map_err(failed)?;
        let limit = control_bound(records, self.senders.len())?;
        // Outstanding permits survive a reduction; producers resume below the new ceiling.
        self.control_limit.store(limit, Ordering::Release);
        Ok(())
    }

    /// One control command; then every hint the capacity view owes its holders, for a placement it made, a box that
    /// ended or a room measured anew (joint plan step 13, CV7).
    pub fn command(&self, command: dto::Command) -> Result<dto::Reply, Refusal> {
        let reply = self.commanded(command);
        if let Some(lane) = &self.lane {
            lane.hint();
        }
        reply
    }

    fn commanded(&self, command: dto::Command) -> Result<dto::Reply, Refusal> {
        // LB03: commit order must remain publication order through local effects and enqueue.
        // A delete cannot overtake a committed Start whose caller has not published it yet.
        let _command = self.command.try_lock().map_err(|why| match why {
            std::sync::TryLockError::WouldBlock => Refusal::Exhausted {
                what: "scheduler command publication",
                bound: 1,
            },
            std::sync::TryLockError::Poisoned(_) => {
                Refusal::Broken("scheduler command lock poisoned".into())
            }
        })?;
        if let Some(why) = self
            .fault
            .lock()
            .map_err(|_| Refusal::Broken("scheduler fault lock poisoned".into()))?
            .clone()
        {
            return Err(why);
        }

        // Current facts gate new admission. Refresh errors stop admission; an old, larger offer is
        // never used merely because the host can no longer establish its current capacity.
        let mut cleanup_failures = Vec::new();
        if matches!(&command, dto::Command::Submit { .. } | dto::Command::Retry) {
            match self.refresh() {
                Ok(failures) => cleanup_failures = encoded_cleanup_failures(failures)?,
                Err(why) => self.driver.block_admission(why).map_err(failed)?,
            }
        }

        let mut requested = None;
        let step = match command {
            dto::Command::Describe {
                from,
                cluster: realm,
            } => {
                self.check_realm(&realm)?;
                if !self.senders.contains_key(&NodeId(from)) {
                    return Err(Refusal::Invalid {
                        what: "peer description",
                        why: "sender is not configured".into(),
                    });
                }
                let failures = encoded_cleanup_failures(self.refresh()?)?;
                let cluster = self
                    .cluster
                    .lock()
                    .map_err(|_| Refusal::Broken("scheduler model lock poisoned".into()))?;
                let spec = cluster
                    .nodes
                    .iter()
                    .find(|n| n.id == self.id)
                    .ok_or_else(|| Refusal::Broken("own facts absent".into()))?;
                return Ok(dto::Reply {
                    offer: Some(offer::from_local(spec, &cluster.models)?),
                    observations: self.observations()?,
                    refusals: failures,
                    ..dto::Reply::default()
                });
            }
            dto::Command::Advertise {
                from,
                cluster: realm,
                offer: incoming,
                observations,
            } => {
                self.check_realm(&realm)?;
                if !self.senders.contains_key(&NodeId(from)) {
                    return Err(Refusal::Invalid {
                        what: "peer advertisement",
                        why: "sender is not configured".into(),
                    });
                }
                let mut cluster = self
                    .cluster
                    .lock()
                    .map_err(|_| Refusal::Broken("scheduler model lock poisoned".into()))?;
                let spec = offer::decode(&incoming, NodeId(from), &mut cluster)?;
                self.driver.refresh_node(spec.clone()).map_err(failed)?;
                if let Err(why) = self
                    .cache
                    .lock()
                    .map_err(|_| Refusal::Broken("peer cache lock poisoned".into()))?
                    .put(NodeId(from), &incoming)
                {
                    *self
                        .fault
                        .lock()
                        .map_err(|_| Refusal::Broken("scheduler fault lock poisoned".into()))? =
                        Some(why.clone());
                    return Err(why);
                }
                if let Some(slot) = cluster.nodes.iter_mut().find(|n| n.id == NodeId(from)) {
                    *slot = spec;
                }
                self.rebudget(&cluster)?;
                drop(cluster);
                self.merge_observations(from, &observations)?;
                let mut step = self.driver.link_up(NodeId(from)).map_err(failed)?;
                let retry = self.driver.retry().map_err(failed)?;
                step.send.extend(retry.send);
                step.notices.extend(retry.notices);
                step
            }
            dto::Command::Submit { demand, payload } => {
                let mut cluster = self
                    .cluster
                    .lock()
                    .map_err(|_| Refusal::Broken("scheduler model lock poisoned".into()))?;
                let needs = hex_sim::needs::parse(&demand).map_err(|why| Refusal::Invalid {
                    what: "placement demand",
                    why,
                })?;
                if needs
                    .gpu_model
                    .as_ref()
                    .is_some_and(|name| !cluster.models.contains(name))
                {
                    return Err(Refusal::Invalid {
                        what: "GPU model",
                        why: "no configured peer reported that model".into(),
                    });
                }
                let class = needs.class.ok_or_else(|| Refusal::Invalid {
                    what: "runtime class",
                    why: "name class:oci, class:shards or class:firecracker explicitly".into(),
                })?;
                let ask = needs.ask(&mut cluster).map_err(|e| Refusal::Invalid {
                    what: "placement demand",
                    why: e.to_string(),
                })?;
                let demand = hex_sim::model::Demand {
                    need: needs.need,
                    kind: hex_sim::model::Kind::Plain,
                    class,
                    ask,
                };
                drop(cluster);
                let (key, step) = self
                    .driver
                    .submit(
                        demand,
                        Payload {
                            spec: payload.spec,
                            start: payload.start,
                        },
                    )
                    .map_err(failed)?;
                requested = Some(key);
                step
            }
            dto::Command::Hear { from, packet } => {
                self.check_realm(&packet.cluster)?;
                if from != self.id.0 && !self.senders.contains_key(&NodeId(from)) {
                    return Err(Refusal::Invalid {
                        what: "scheduler sender",
                        why: format!("unconfigured peer {from}"),
                    });
                }
                let mut order = self
                    .order
                    .lock()
                    .map_err(|_| Refusal::Broken("peer order lock poisoned".into()))?;
                use sha2::Digest;
                let encoded = serde_json::to_vec(&packet).map_err(|e| Refusal::Invalid {
                    what: "peer packet",
                    why: e.to_string(),
                })?;
                let fingerprint: [u8; 32] = sha2::Sha256::digest(&encoded).into();
                if !order.accept(from, packet.epoch, packet.seq, fingerprint)? {
                    return Ok(dto::Reply {
                        key: None,
                        at: None,
                        physical: None,
                        created: false,
                        ended: false,
                        refusals: Vec::new(),
                        ..dto::Reply::default()
                    });
                }
                self.merge_observations(from, &packet.observations)?;
                let cluster = self
                    .cluster
                    .lock()
                    .map_err(|_| Refusal::Broken("scheduler model lock poisoned".into()))?;
                let message = envelope(packet, &cluster.models)?;
                drop(cluster);
                if matches!(message.msg, peer::Msg::Start { .. }) {
                    match self.refresh() {
                        Ok(failures) => cleanup_failures = encoded_cleanup_failures(failures)?,
                        Err(why) => self.driver.block_admission(why).map_err(failed)?,
                    }
                }
                let step = self.driver.hear(NodeId(from), message).map_err(failed)?;
                drop(order);
                step
            }
            dto::Command::LinkUp { peer } => self.driver.link_up(NodeId(peer)).map_err(failed)?,
            dto::Command::Retry => {
                self.refresh_peers()?;
                self.driver.retry().map_err(failed)?
            }
            dto::Command::Delete { key: k } => {
                requested = Some(key(k));
                let step = self.driver.delete(key(k)).map_err(failed)?;
                if let Some(status) = self.driver.status(key(k)).map_err(failed)?
                    && let Some(node) = status.node.filter(|node| *node != self.id)
                {
                    // Interrupt only an explicit cancellation after durable intent. Replayed
                    // Deletes must keep the fresh connection that will deliver them alive.
                    self.stopping
                        .interrupt_peer(node.0)
                        .map_err(|e| Refusal::os("interrupting a peer for deletion control", e))?;
                }
                step
            }
            dto::Command::Status { key: k } => {
                self.refresh_peers()?;
                requested = Some(key(k));
                self.driver.poll_status().map_err(failed)?
            }
            dto::Command::Forget { holder } => return self.forget(&holder),
        };
        let dispatch = self.dispatch(step)?;
        let refusals = if cleanup_failures.is_empty() {
            dispatch.refusals
        } else {
            let bound = cleanup_failures
                .len()
                .checked_add(dispatch.refusals.len())
                .ok_or_else(|| capacity::invalid("scheduler refusal count overflows"))?;
            cleanup_failures
                .try_reserve_exact(dispatch.refusals.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "scheduler control refusals",
                    bound,
                })?;
            cleanup_failures.extend(dispatch.refusals);
            cleanup_failures
        };
        let status = requested
            .map(|key| self.driver.status(key).map_err(failed))
            .transpose()?
            .flatten();
        Ok(dto::Reply {
            key: requested.map(external),
            tracked: status.is_some(),
            at: status.as_ref().and_then(|s| s.node).map(|n| n.0),
            physical: status.as_ref().map(|s| s.physical.clone()),
            created: status.as_ref().is_some_and(|s| s.created)
                || dispatch
                    .notices
                    .iter()
                    .any(|n| matches!(n, Notice::Started { key, .. } if Some(*key) == requested)),
            ended: status.as_ref().is_some_and(|s| s.ended)
                || dispatch
                    .notices
                    .iter()
                    .any(|n| matches!(n, Notice::Ended { key } if Some(*key) == requested)),
            refusals,
            ..dto::Reply::default()
        })
    }
}

/// Machine resources are local facts; GPU observations come only from the initialized native
/// Store/turn owner over Runtime. No Prober/Gpus is opened or cached in this process.
#[cfg(all(test, target_os = "linux"))]
fn runtime_facts(
    state: &Path,
    initialized: Classes,
    runtimes: &Runtimes,
) -> Result<(hex_sim::facts::Facts, Option<String>), Refusal> {
    let (facts, authority, _) = runtime_observations(state, initialized, runtimes)?;
    Ok((facts, authority))
}

fn runtime_observations(
    state: &Path,
    initialized: Classes,
    runtimes: &Runtimes,
) -> Result<
    (
        hex_sim::facts::Facts,
        Option<String>,
        Vec<hex_api::BackendUnavailable>,
    ),
    Refusal,
> {
    let report = runtimes.physical_report().map_err(failed)?;
    let mut facts = crate::facts::machine(state, &crate::facts::Declared::default(), initialized)?;
    let mut unavailable = Vec::new();
    let authority = match report {
        Some(report) => {
            facts.gpus = crate::facts::reported_gpus(&report)?;
            unavailable = report.unavailable;
            Some(report.authority)
        }
        None => None,
    };
    if facts.gpus.is_empty() || !unavailable.is_empty() {
        let message = if !unavailable.is_empty() {
            "initialized VM backend observation is unavailable; native facts remain authoritative"
        } else if authority.is_some() {
            "initialized native provider reports no verified GPUs"
        } else {
            "no configured native physical report owner"
        };
        let mut why = String::new();
        why.try_reserve_exact(message.len())
            .map_err(|_| Refusal::Exhausted {
                what: "native GPU report diagnostic bytes",
                bound: message.len(),
            })?;
        why.push_str(message);
        facts.no_gpus = Some(why);
    }
    Ok((facts, authority, unavailable))
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.stopping.stop();
    }
}

fn control_only() -> Refusal {
    Refusal::Invalid {
        what: "scheduler socket",
        why: "use scheduler control here and the configured runtime socket for box operations"
            .into(),
    }
}

impl Runtime for Service {
    fn classes(&self) -> Vec<Class> {
        Vec::new()
    }
    fn capacity(&self, _: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        Err(control_only())
    }
    fn create(&self, _: BoxSpec) -> Result<BoxInfo, Refusal> {
        Err(control_only())
    }
    fn start(&self, _: &BoxId) -> Result<(), Refusal> {
        Err(control_only())
    }
    fn exec(&self, _: &BoxId, _: Program, _: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        Err(control_only())
    }
    fn run(&self, _: BoxSpec, _: Program, _: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        Err(control_only())
    }
    fn signal(&self, _: &BoxId, _: Signal) -> Result<(), Refusal> {
        Err(control_only())
    }
    fn inspect(&self, _: &BoxId) -> Result<BoxInfo, Refusal> {
        Err(control_only())
    }
    fn usage(&self, _: &BoxId) -> Result<Usage, Refusal> {
        Err(control_only())
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        Err(control_only())
    }
    fn delete(&self, _: &BoxId) -> Result<(), Refusal> {
        Err(control_only())
    }
}

impl Host for Service {
    fn schedule(&self, command: dto::Command) -> Result<dto::Reply, Refusal> {
        self.command(command)
    }
    fn abandoned(&self, _: &BoxId) -> Result<(), Refusal> {
        Err(control_only())
    }
    fn netns(&self) -> Result<hex_core::netns::Counts, Refusal> {
        Err(control_only())
    }
}

/// A configuration is a wire-sized input, refused before allocation past that bound.
pub fn read_config(path: &Path) -> Result<Config, Refusal> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).map_err(|e| Refusal::os("opening scheduler configuration", e))?;
    let mut bytes = Vec::new();
    file.take(
        u64::try_from(wire::MAX_MESSAGE)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)
    .map_err(|e| Refusal::os("reading scheduler configuration", e))?;
    if bytes.len() > wire::MAX_MESSAGE {
        return Err(Refusal::Exhausted {
            what: "scheduler configuration bytes",
            bound: wire::MAX_MESSAGE,
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| Refusal::Invalid {
        what: "scheduler configuration",
        why: e.to_string(),
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod capacity_lane_tests;

#[cfg(all(test, target_os = "linux"))]
mod observer_cpu_tests;
