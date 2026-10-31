//! U15: a transport shell that runs before SQL bootstrap and remains independent of application
//! handlers waiting for quorum. It never drives Raft or acknowledges an application write.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

use amphora::database::Database;
use amphora::replication::{ReceiveReceipt, ReplicaHandle};
use amphora_api::Refusal;
use futures::FutureExt as _;
use hyper_transport::PeerId;

use super::{Config, Incoming};
use crate::net::{BoundSend, Connection, Kind, NetEvent, NetHandle, Out, SERVER_PEER};
use crate::proto::{self, AgentMsg, ServerMsg};
use crate::rt::Shutdown;

const DRAIN: usize = 16;
const POLL: Duration = Duration::from_millis(10);
const RECEIVE: Duration = Duration::from_secs(5);
const RECEIPTS: usize = 64;
const CAPABILITY_WAIT: Duration = Duration::from_secs(60);
const OWNER_POLL: Duration = Duration::from_millis(500);
const OWNER_DEADLINE: Duration = Duration::from_secs(5);
const CONNECTIONS: usize = 4096;
const REFUSAL_PREFIX: &str = "Amphora transport: ";
/// The pre-SQL carrier declares only its own transport. It is consumed here; the later
/// application Caps still follow Hello and carry the rest of that process's features.
const CARRIER_CAPS: u64 = proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT;
#[derive(Debug, thiserror::Error)]
#[error("configured Amphora peer does not support the required product transport capability")]
struct UnsupportedPeer;
#[derive(Debug, thiserror::Error)]
#[error("Amphora upstream refused replication: {0}")]
struct Refused(String);

struct Activity(Arc<AtomicBool>);
impl Activity {
    fn activate(&self) { self.0.store(true, Ordering::Release); }
}
impl Drop for Activity {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

async fn wait_for_owner(database: Database) -> anyhow::Result<ReplicaHandle> {
    loop {
        let checking = database.clone();
        match crate::rt::blocking_in(crate::rt::SHARE_STORE, move || checking.ensure_owner(Instant::now() + OWNER_DEADLINE)).await? {
            Ok(Some(replica)) => return Ok(replica),
            Ok(None) => {},
            // This probe only selects/authenticates the product owner. Retrying it cannot
            // replay an application command or an SQL closure after an unknown outcome.
            Err(error) if matches!(&error, Refusal::Unavailable(_) | Refusal::Timeout) => {
                tracing::warn!(target: "kiln.replication", event = "owner_wait", error = %error, "Amphora ownership probe will retry; application writes are not retried");
            }
            Err(error) => return Err(error.into()),
        }
        crate::rt::sleep(OWNER_POLL).await;
    }
}
// At most 64 MiB/1024 events wait for application initialization, plus one output frame
// (bounded by net to 4 MiB). Filling either budget stops the host without acknowledging work.
const APPLICATION_EVENTS: usize = 1024;
const APPLICATION_BYTES: usize = 64 << 20;
fn event_bytes(event: &NetEvent) -> usize {
    let allocated = match event {
        NetEvent::Frame { data, .. } => data.capacity(),
        NetEvent::Connected { name, .. } => name.as_ref().map_or(0, String::capacity),
        NetEvent::Closed { reason, .. } | NetEvent::Unreachable { reason, .. } => reason.capacity(),
    };
    allocated.saturating_add(128)
}

struct Application {
    output: async_channel::Sender<NetEvent>,
    pending: VecDeque<NetEvent>,
    bytes: usize,
}
impl Application {
    fn new(output: async_channel::Sender<NetEvent>) -> Self { Self { output, pending: VecDeque::new(), bytes: 0 } }
    fn flush(&mut self) -> bool {
        if self.output.is_closed() { return false; }
        while let Some(event) = self.pending.pop_front() {
            let bytes = event_bytes(&event);
            match self.output.try_send(event) {
                Ok(()) => self.bytes = self.bytes.saturating_sub(bytes),
                Err(async_channel::TrySendError::Full(event)) => { self.pending.push_front(event); break; }
                Err(async_channel::TrySendError::Closed(_)) => return false,
            }
        }
        true
    }
    fn push(&mut self, event: NetEvent) -> anyhow::Result<()> {
        // The carrier may consume declared replication before SQL bootstrap; these immutable
        // application-fenced events are never buffered for a later serving Hub.
        if !event.application_ready() { return Ok(()); }
        self.bytes = self.bytes.checked_add(event_bytes(&event)).ok_or_else(|| anyhow::anyhow!("Amphora application event size overflow"))?;
        anyhow::ensure!(self.pending.len() < APPLICATION_EVENTS && self.bytes <= APPLICATION_BYTES, "Amphora carrier application event budget exhausted");
        self.pending.push_back(event);
        Ok(())
    }
}

async fn standby<T>(events: &async_channel::Receiver<NetEvent>, application: &mut Application, shutdown: &Shutdown, ownership: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<Option<T>> {
    // Retain one selection future: dropping a blocking-pool future does not stop its closure.
    // Shutdown may leave that single selection finishing, but cannot activate its result.
    let ownership = ownership.fuse();
    futures::pin_mut!(ownership);
    let output = application.output.clone();
    loop {
        if shutdown.is_cancelled() || !application.flush() { return Ok(None); }
        futures::select_biased! {
            () = shutdown.cancelled().fuse() => return Ok(None),
            () = output.closed().fuse() => return Ok(None),
            owner = ownership => {
                if shutdown.is_cancelled() || application.output.is_closed() { return Ok(None); }
                return owner.map(Some);
            },
            event = events.recv().fuse() => application.push(event?)?,
            () = crate::rt::sleep(POLL).fuse() => {},
        }
    }
}

struct Peer { machine: String, declared: bool, capable: bool, connected: Instant, waiting: Option<Instant>, connection: Option<Connection> }
impl Peer {
    fn new(machine: String, declared: bool, connection: Option<Connection>) -> Self {
        Self { machine, declared, capable: false, connected: Instant::now(), waiting: None, connection }
    }
    fn on_connection(&self, origin: Option<Connection>) -> bool {
        origin.is_some() && self.connection == origin
    }
}

fn admit_waiting(peers: &mut BTreeMap<PeerId, Peer>, machine: &str) {
    if peers.values().any(|peer| peer.machine == machine && peer.capable) { return; }
    let mut waiting = peers.iter().filter_map(|(id, peer)| (peer.machine == machine && peer.waiting.is_some()).then_some(*id));
    let candidate = waiting.next();
    if waiting.next().is_some() { return; }
    if let Some(peer) = candidate.and_then(|id| peers.get_mut(&id)) { peer.capable = true; peer.waiting = None; }
}

fn disconnected(peers: &mut BTreeMap<PeerId, Peer>, peer: PeerId) {
    if let Some(source) = peers.remove(&peer) { admit_waiting(peers, &source.machine); }
}

fn announce(peers: &mut BTreeMap<PeerId, Peer>, peer: PeerId, caps: u64, origin: Option<Connection>) -> Result<bool, &'static str> {
    let capable = caps & (proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT) == (proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT);
    // A takeover rebuild does not purge events already queued from the standby connection.
    // Wait for a fresh authenticated Connected before accepting or refusing its route claim.
    let Some(source) = peers.get(&peer) else { return Ok(false); };
    if !source.on_connection(origin) { return Ok(false); }
    proto::check_unified_layout(caps)?;
    if capable && !source.declared { return Err("authenticated machine has no declared product member route"); }
    let machine = source.machine.clone();
    let occupied = capable && peers.iter().any(|(id, other)| *id != peer && other.machine == machine && other.capable);
    if occupied && peers.iter().any(|(id, other)| *id != peer && other.machine == machine && other.waiting.is_some()) {
        return Err("more than one successor claims this occupied product member route");
    }
    if let Some(source) = peers.get_mut(&peer) {
        source.capable = capable && !occupied;
        if occupied { source.waiting.get_or_insert_with(Instant::now); } else { source.waiting = None; }
    }
    if !capable { admit_waiting(peers, &machine); }
    Ok(true)
}

struct Carrier {
    config: Config,
    replica: ReplicaHandle,
    net: NetHandle,
    incoming: Incoming,
    receipts: VecDeque<PendingReceive>,
    peers: BTreeMap<PeerId, Peer>,
    next: u64,
}

struct PendingReceive {
    peer: PeerId,
    from: u64,
    message: u64,
    machine: String,
    bytes: usize,
    admitted: Instant,
    until: Instant,
    receipt: ReceiveReceipt,
}

impl Drop for Carrier {
    fn drop(&mut self) {
        // U15: observe every result already ready on exit, without waiting on processing.
        // A pending receipt only relinquishes observation; the product still owns its packet.
        if let Err(error) = self.reap_receives() {
            tracing::warn!(target: "kiln.replication", event = "receive_final_refused", error = %format!("{error:#}"), "Amphora carrier observed a processing refusal while stopping");
        }
        if !self.receipts.is_empty() {
            tracing::info!(target: "kiln.replication", event = "receive_observation_stopped", pending = self.receipts.len(), "Amphora carrier stopped observing pending transport packets; no processing or application acknowledgement is implied");
            for pending in &self.receipts {
                tracing::debug!(target: "kiln.replication", event = "receive_observation_pending", peer = pending.peer, from = pending.from, to = self.config.id, message = pending.message, machine = %pending.machine, deadline = ?pending.until, "Amphora product retains ownership of this packet");
            }
        }
    }
}

/// Start before any schema/import/session call waits for quorum. On a runner, dial the upstream
/// before awaiting database bootstrap. SDK clients pass application traffic until the product
/// grants local ownership; the same stream then carries application events alongside replication.
/// The flag is true only while this coordinator carries a local product replica. Read it with
/// Acquire for capability announcements. Transport and SQL initialization remain independent.
pub fn start(database: Database, config: Config, net: NetHandle, events: async_channel::Receiver<NetEvent>, shutdown: Shutdown) -> anyhow::Result<(async_channel::Receiver<NetEvent>, Arc<AtomicBool>)> {
    config.validate()?;
    let replica = database.replica();
    let (tx, rx) = async_channel::bounded(1);
    let active = Arc::new(AtomicBool::new(false));
    let activity = Activity(Arc::clone(&active));
    crate::rt::spawn(async move {
        let active_owner = activity;
        let result = async {
            let mut application = Application::new(tx);
            if shutdown.is_cancelled() || application.output.is_closed() { return Ok(()); }
            let (replica, takeover) = match replica {
                Some(replica) => (replica, false),
                None => {
                    let Some(replica) = standby(&events, &mut application, &shutdown, wait_for_owner(database)).await? else { return Ok(()); };
                    (replica, true)
                }
            };
            if shutdown.is_cancelled() || application.output.is_closed() { return Ok(()); }
            anyhow::ensure!(replica.status()?.node_id == config.id, "Amphora carrier and local product member ids differ");
            let carrier = Carrier { config, replica, net, incoming: Incoming::default(), receipts: VecDeque::new(), peers: BTreeMap::new(), next: 0 };
            active_owner.activate();
            if takeover {
                // Standby events went to the application. Rebuilding yields fresh authenticated
                // Connected/Caps events for the carrier while preserving the same application RX.
                let rebuilt = futures::select_biased! {
                    () = shutdown.cancelled().fuse() => return Ok(()),
                    () = application.output.closed().fuse() => return Ok(()),
                    rebuilt = carrier.net.command(Out::Rebuild).fuse() => rebuilt,
                };
                anyhow::ensure!(rebuilt, "Amphora network command queue is closed during ownership takeover");
                tracing::info!(target: "kiln.replication", event = "owner_active", node = carrier.config.id, "Amphora product granted local ownership; carrier is active");
            }
            carrier.run(events, application, shutdown.clone()).await
        }.await;
        if let Err(error) = result {
            tracing::error!(target: "kiln.replication", event = "carrier_failed", error = %format!("{error:#}"), "Amphora carrier stopped; host is shutting down without acknowledging more work");
            shutdown.cancel();
        }
    });
    Ok((rx, active))
}

impl Carrier {
    fn hub(&self) -> bool { self.config.id == self.config.hub }

    fn reap_receives(&mut self) -> anyhow::Result<()> {
        let mut fenced = None;
        for _ in 0..self.receipts.len() {
            let Some(mut pending) = self.receipts.pop_front() else { break; };
            match pending.receipt.try_result() {
                Ok(None) => self.receipts.push_back(pending),
                // U15: a processed wire record is not an application durability ACK.
                Ok(Some(())) => {},
                Err(error) => {
                    tracing::warn!(target: "kiln.replication", event = "receive_processing_refused", peer = pending.peer, from = pending.from, to = self.config.id, message = pending.message, machine = %pending.machine, bytes = pending.bytes, elapsed = ?pending.admitted.elapsed(), deadline = ?pending.until, error = %error, "Amphora rejected an admitted transport message without an application acknowledgement");
                    if matches!(&error, Refusal::Fenced(_)) && fenced.is_none() {
                        fenced = Some(anyhow::Error::new(error).context(format!("Amphora receive processing fenced for peer {} member {} message {}", pending.peer, pending.from, pending.message)));
                    }
                }
            }
        }
        match fenced { Some(error) => Err(error), None => Ok(()) }
    }

    async fn send(&self, frame: super::Frame) -> anyhow::Result<()> {
        frame.validate()?;
        if self.hub() {
            let machine = self.config.machine(frame.to).ok_or_else(|| anyhow::anyhow!("Amphora member has no declared authenticated route"))?;
            let connection = self.peers.values().find_map(|p| (p.machine == machine && p.capable).then_some(p.connection).flatten()).ok_or_else(|| anyhow::anyhow!("Amphora member {machine} is disconnected or unsupported"))?;
            anyhow::ensure!(self.net.send_bound(connection, proto::LANE_SYNC, Kind::Control, proto::encode(&ServerMsg::Amphora { frame })?).await == BoundSend::QueuedUnknown, "Amphora network queue is closed");
        } else {
            let connection = self.peers.get(&SERVER_PEER).filter(|p| p.capable).and_then(|p| p.connection).ok_or_else(|| anyhow::anyhow!("Amphora hub is disconnected or unsupported"))?;
            anyhow::ensure!(self.net.send_bound(connection, proto::LANE_SYNC, Kind::Control, proto::encode(&AgentMsg::Amphora { frame })?).await == BoundSend::QueuedUnknown, "Amphora network queue is closed");
        }
        Ok(())
    }

    async fn frame(&mut self, peer: PeerId, frame: super::Frame) -> anyhow::Result<()> {
        frame.validate()?;
        let source = self.peers.get(&peer).ok_or_else(|| anyhow::anyhow!("Amphora frame arrived without an authenticated connection"))?;
        anyhow::ensure!(source.capable, "Amphora transport capability was not announced");
        if self.hub() {
            anyhow::ensure!(self.config.source(&source.machine, frame.from), "Amphora sender does not match the authenticated runner");
        } else {
            anyhow::ensure!(peer == SERVER_PEER && (frame.from == self.config.hub || self.config.machine(frame.from).is_some()), "Amphora frame has no declared member route");
        }
        if frame.to != self.config.id {
            anyhow::ensure!(self.hub(), "Amphora frame is addressed to another member");
            return self.send(frame).await;
        }
        let from = frame.from;
        let message = frame.id;
        let machine = source.machine.clone();
        if let Some(bytes) = self.incoming.receive(frame, Instant::now())? {
            self.reap_receives()?;
            if self.receipts.len() >= RECEIPTS {
                return Err(anyhow::Error::new(Refusal::Full { limit: RECEIPTS }).context(format!("Amphora outstanding receive budget exhausted for peer {peer} member {from} message {message}")));
            }
            let size = bytes.len();
            let admitted = Instant::now();
            let until = admitted + RECEIVE;
            // The product owns the queued payload and its existing 512 MiB byte budget.
            // Admission never waits for processing; this sole loop must keep egress/relay alive.
            let receipt = self.replica.admit_receive(from, bytes, until)
                .map_err(|error| anyhow::Error::new(error).context(format!("Amphora receive admission refused for peer {peer} member {from} message {message}")))?;
            self.receipts.push_back(PendingReceive { peer, from, message, machine, bytes: size, admitted, until, receipt });
        }
        Ok(())
    }

    async fn event(&mut self, event: NetEvent) -> anyhow::Result<Option<NetEvent>> {
        match &event {
            NetEvent::Connected { peer, epoch, name, binding, .. } => {
                let connection = binding.map(|binding| Connection::new(*peer, *epoch, binding));
                if self.hub() {
                    // U15: owner-SDK application clients share their owner's machine name.
                    // Bind an exclusive route only after CAP_AMPHORA. Keep all authenticated
                    // connections so undeclared claims differ from stale pre-rebuild events.
                    if self.peers.len() >= CONNECTIONS && !self.peers.contains_key(peer) {
                        self.net.command(Out::Disconnect { peer: *peer }).await;
                        anyhow::bail!("Amphora transport connection budget exhausted");
                    }
                    let machine = name.clone().unwrap_or_default();
                    let declared = self.config.peers.contains_key(&machine);
                    self.peers.insert(*peer, Peer::new(machine, declared, connection));
                    if declared && let Some(connection) = connection {
                        self.net.send_bound(connection, proto::LANE_CONTROL, Kind::Control, proto::encode(&ServerMsg::Caps { caps: proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT })?).await;
                    }
                } else {
                    anyhow::ensure!(*peer == SERVER_PEER, "unexpected Amphora upstream peer");
                    self.peers.insert(*peer, Peer::new(String::new(), true, connection));
                    if let Some(connection) = connection {
                        self.net.send_bound(connection, proto::LANE_CONTROL, Kind::Control, proto::encode(&AgentMsg::Caps { caps: proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT })?).await;
                    }
                }
            }
            NetEvent::Closed { peer, epoch, .. } => {
                if self.peers.get(peer).is_some_and(|p| p.connection.is_none_or(|connection| connection.epoch() == *epoch)) {
                    disconnected(&mut self.peers, *peer);
                }
            }
            // Dial failure has no connection provenance. It cannot retire a later bound route;
            // that route's exact Closed event will do so.
            NetEvent::Unreachable { peer, .. } => {
                if self.peers.get(peer).is_some_and(|p| p.connection.is_none()) {
                    disconnected(&mut self.peers, *peer);
                }
            }
            NetEvent::Frame { peer, lane, data, connection, .. } => {
                if self.hub() {
                    match proto::read_agent(data) {
                        // Ordinary application-only Caps are not the carrier's to consume.
                        Ok(AgentMsg::Caps { caps }) if caps & (proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT) == 0
                            && proto::check_unified_layout(caps).is_ok()
                            && !self.peers.get(peer).is_some_and(|p| p.on_connection(*connection)) => {},
                        Ok(AgentMsg::Caps { caps }) => {
                            match announce(&mut self.peers, *peer, caps, *connection) {
                                Ok(false) => return Ok(None),
                                Ok(true) if caps == CARRIER_CAPS => return Ok(None),
                                Ok(true) => {},
                                Err(reason) => {
                                    let reason = format!("{REFUSAL_PREFIX}{reason}");
                                    tracing::warn!(target: "kiln.replication", event = "owner_refused", peer, %reason, "Amphora member route claim refused");
                                    // The runner carrier consumes this before SQL startup and
                                    // terminates instead of retrying a rejected owner.
                                    if let Some(connection) = connection {
                                        self.net.send_bound(*connection, proto::LANE_CONTROL, Kind::Control, proto::encode(&ServerMsg::Refused { reason })?).await;
                                    }
                                    return Ok(None);
                                }
                            }
                        }
                        Ok(AgentMsg::Amphora { frame }) => {
                            if !self.peers.get(peer).is_some_and(|p| p.on_connection(*connection)) { return Ok(None); }
                            anyhow::ensure!(*lane == proto::LANE_SYNC, "Amphora message arrived on the wrong lane");
                            self.frame(*peer, frame).await?;
                            return Ok(None);
                        }
                        _ => {}
                    }
                } else {
                    match proto::read_server(data) {
                        Ok(ServerMsg::Caps { caps }) if caps & (proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT) == 0
                            && proto::check_unified_layout(caps).is_ok()
                            && !self.peers.get(peer).is_some_and(|p| p.on_connection(*connection)) => {},
                        Ok(ServerMsg::Caps { caps }) => {
                            let Some(p) = self.peers.get_mut(peer) else { return Ok(None); };
                            if !p.on_connection(*connection) { return Ok(None); }
                            if proto::check_unified_layout(caps).is_err() || caps & (proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT) != (proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT) { return Err(UnsupportedPeer.into()); }
                            p.capable = true;
                            if caps == CARRIER_CAPS { return Ok(None); }
                        }
                        Ok(ServerMsg::Refused { reason }) if *peer == SERVER_PEER && reason.starts_with(REFUSAL_PREFIX) => {
                            if !self.peers.get(peer).is_some_and(|p| p.on_connection(*connection)) { return Ok(None); }
                            return Err(Refused(reason).into());
                        },
                        Ok(ServerMsg::Amphora { frame }) => {
                            if !self.peers.get(peer).is_some_and(|p| p.on_connection(*connection)) { return Ok(None); }
                            anyhow::ensure!(*lane == proto::LANE_SYNC, "Amphora message arrived on the wrong lane");
                            self.frame(*peer, frame).await?;
                            return Ok(None);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(Some(event))
    }

    async fn run(mut self, events: async_channel::Receiver<NetEvent>, mut application: Application, shutdown: Shutdown) -> anyhow::Result<()> {
        loop {
            if shutdown.is_cancelled() || !application.flush() { return Ok(()); }
            self.reap_receives()?;
            // Hub connections may be application-only owner-SDK clients. A runner carrier,
            // however, explicitly requires product transport from its authenticated upstream.
            anyhow::ensure!(self.hub() || !self.peers.values().any(|p| !p.capable && p.connected.elapsed() >= CAPABILITY_WAIT), UnsupportedPeer);
            if self.hub() {
                let expired: Vec<_> = self.peers.iter().filter_map(|(id, peer)| peer.waiting.filter(|at| at.elapsed() >= CAPABILITY_WAIT).map(|_| *id)).collect();
                for peer in expired {
                    let connection = self.peers.get_mut(&peer).and_then(|source| { source.waiting = None; source.connection });
                    let reason = format!("{REFUSAL_PREFIX}previous product member connection did not release its route before the successor deadline");
                    if let Some(connection) = connection {
                        self.net.send_bound(connection, proto::LANE_CONTROL, Kind::Control, proto::encode(&ServerMsg::Refused { reason })?).await;
                    }
                }
            }
            let replica = self.replica.clone();
            let outgoing = crate::rt::blocking(move || replica.drain(DRAIN)).await??;
            for message in outgoing {
                self.next = self.next.checked_add(1).ok_or_else(|| anyhow::anyhow!("Amphora transport message id exhausted"))?;
                for frame in super::frames(self.config.id, message.to, self.next, message.bytes)? {
                    if let Err(error) = self.send(frame).await {
                        // A lost transport message is never a committed write. Only the product
                        // protocol decides whether to send it again; report the loss explicitly.
                        tracing::warn!(target: "kiln.replication", event = "route_unavailable", to = message.to, error = %format!("{error:#}"), "Amphora transport did not deliver this frame");
                        break;
                    }
                }
            }
            let event = futures::select_biased! {
                () = shutdown.cancelled().fuse() => return Ok(()),
                () = application.output.closed().fuse() => return Ok(()),
                event = events.recv().fuse() => Some(event?),
                () = crate::rt::sleep(POLL).fuse() => None,
            };
            if let Some(event) = event {
                match self.event(event).await {
                    Ok(Some(event)) => application.push(event)?,
                    Ok(None) => {}
                    Err(error) if error.is::<UnsupportedPeer>() || error.is::<Refused>() || error.downcast_ref::<Refusal>().is_some_and(|error| matches!(error, Refusal::Fenced(_))) => return Err(error),
                    Err(error) => tracing::warn!(target: "kiln.replication", event = "transport_refused", error = %format!("{error:#}"), "Amphora transport message refused without acknowledgement"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u15_sdk_standby_passes_application_events_until_one_owner_is_selected() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (input, events) = async_channel::bounded(4);
            let (output, application) = async_channel::bounded(1);
            let (selected, ownership) = futures::channel::oneshot::channel::<u64>();
            let shutdown = Shutdown::new();
            let pending = crate::rt::spawn(async move {
                let mut application = Application::new(output);
                let selected = standby(&events, &mut application, &shutdown, async move { Ok(ownership.await?) }).await?;
                Ok::<_, anyhow::Error>((selected, events))
            });
            input.send(NetEvent::Connected { admission:crate::net::ApplicationPermit::fixture(), peer: 7, epoch: 1, name: Some("runner-a".into()), binding: None }).await?;
            let connected = crate::rt::timeout(Duration::from_secs(5), application.recv()).await??;
            anyhow::ensure!(matches!(connected, NetEvent::Connected { peer: 7, .. }), "standby lost authenticated application connection");
            input.send(NetEvent::Frame { admission:crate::net::ApplicationPermit::fixture(), connection: None, peer: 7, lane: proto::LANE_CONTROL, kind: Kind::Control, data: vec![1, 2].into() }).await?;
            let frame = crate::rt::timeout(Duration::from_secs(5), application.recv()).await??;
            anyhow::ensure!(matches!(frame, NetEvent::Frame { peer: 7, data, .. } if data == vec![1, 2]), "standby did not pass application traffic while its owner probe waited");
            selected.send(9).map_err(|_| anyhow::anyhow!("owner selection ended before application passthrough"))?;
            // Owner selection is ready before Caps enters the input. Connected was already
            // consumed during standby, so this frame must await a fresh connection after reset.
            input.send(NetEvent::Frame { admission:crate::net::ApplicationPermit::fixture(), connection: None, peer: 7, lane: proto::LANE_CONTROL, kind: Kind::Control, data: proto::encode(&AgentMsg::Caps { caps: proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT })?.into() }).await?;
            let (selected, remaining) = crate::rt::timeout(Duration::from_secs(5), pending).await???;
            assert_eq!(selected, Some(9));
            let event = crate::rt::timeout(Duration::from_secs(5), remaining.recv()).await??;
            let (caps, stale_origin) = match event {
                NetEvent::Frame { data, connection, .. } => match proto::read_agent(&data).map_err(|e| anyhow::anyhow!("{e:?}"))? {
                    AgentMsg::Caps { caps } => (caps, connection),
                    _ => anyhow::bail!("expected queued standby Caps"),
                },
                _ => anyhow::bail!("expected queued standby frame"),
            };
            let mut peers = BTreeMap::new();
            let connection = Some(Connection::new(7, 2, [7; 32]));
            assert!(!announce(&mut peers, 7, caps, stale_origin).map_err(anyhow::Error::msg)?, "stale Caps must not refuse an owner before its fresh Connected event");
            peers.insert(7, Peer::new("runner-a".into(), true, connection));
            assert!(!announce(&mut peers, 7, caps, stale_origin).map_err(anyhow::Error::msg)?, "the queued unbound Caps cannot borrow a fresh connection");
            assert!(announce(&mut peers, 7, caps, connection).map_err(anyhow::Error::msg)?);
            assert!(peers.get(&7).is_some_and(|peer| peer.capable));
            Ok(())
        })
    }

    #[test]
    fn u15_sdk_standby_shutdown_or_application_close_prevents_late_activation() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for cancel in [true, false] {
                let (_input, events) = async_channel::bounded(1);
                let (output, application) = async_channel::bounded(1);
                let (selected, ownership) = futures::channel::oneshot::channel::<u64>();
                let shutdown = Shutdown::new();
                let stopping = shutdown.clone();
                let pending = crate::rt::spawn(async move {
                    let mut application = Application::new(output);
                    standby(&events, &mut application, &shutdown, async move { Ok(ownership.await?) }).await
                });
                if cancel { stopping.cancel(); } else { drop(application); }
                let _ = selected.send(9);
                assert_eq!(crate::rt::timeout(Duration::from_secs(5), pending).await???, None);
            }
            let active = Arc::new(AtomicBool::new(false));
            let activity = Activity(Arc::clone(&active));
            activity.activate();
            assert!(active.load(Ordering::Acquire));
            drop(activity);
            assert!(!active.load(Ordering::Acquire), "ending the coordinator must clear its advertised transport ownership");
            Ok(())
        })
    }

    #[test]
    fn u15_application_overlap_preserves_the_single_product_owner_route() -> anyhow::Result<()> {
        let connection = |id| Some(Connection::new(id, 1, [1; 32]));
        let peer = |id| Peer::new("runner-a".into(), true, connection(id));
        let mut peers = BTreeMap::from([(1, peer(1)), (2, peer(2)), (3, peer(3)), (4, peer(4))]);
        announce(&mut peers, 1, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(1)).map_err(anyhow::Error::msg)?;
        announce(&mut peers, 2, proto::CAP_HANDOVER, connection(2)).map_err(anyhow::Error::msg)?;
        assert!(peers.get(&1).is_some_and(|p| p.capable));
        assert!(peers.get(&2).is_some_and(|p| !p.capable));
        announce(&mut peers, 3, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(3)).map_err(anyhow::Error::msg)?;
        assert!(announce(&mut peers, 4, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(4)).is_err());
        assert!(peers.get(&1).is_some_and(|p| p.capable));
        let waiting = peers.get(&3).and_then(|p| p.waiting);
        assert!(waiting.is_some());
        assert!(peers.get(&3).is_some_and(|p| !p.capable));
        announce(&mut peers, 3, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(3)).map_err(anyhow::Error::msg)?;
        assert_eq!(peers.get(&3).and_then(|p| p.waiting), waiting, "repeated Caps must not renew a pending takeover deadline");
        disconnected(&mut peers, 1);
        assert!(peers.get(&3).is_some_and(|p| p.capable && p.waiting.is_none()));
        announce(&mut peers, 2, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(2)).map_err(anyhow::Error::msg)?;
        assert!(peers.get(&2).is_some_and(|p| !p.capable && p.waiting.is_some()));
        disconnected(&mut peers, 3);
        assert!(peers.get(&2).is_some_and(|p| p.capable));
        assert!(!announce(&mut peers, 5, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(5)).map_err(anyhow::Error::msg)?);
        peers.insert(6, Peer { declared: false, ..peer(6) });
        assert!(announce(&mut peers, 6, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, connection(6)).is_err());
        // A replacement Connected resets proof. Buffered old Caps cannot authorize it or
        // refuse it, and unbound/private frames cannot borrow the new declaration.
        let previous = connection(1);
        let replacement = Some(Connection::new(1, 2, [2; 32]));
        peers.insert(1, Peer::new("runner-a".into(), true, replacement));
        assert!(!announce(&mut peers, 1, proto::CAP_JOB_ATTEMPT, previous).map_err(anyhow::Error::msg)?);
        assert!(peers.get(&1).is_some_and(|p| !p.capable && !p.on_connection(previous) && !p.on_connection(None)));
        assert!(!announce(&mut peers, 1, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, previous).map_err(anyhow::Error::msg)?);
        // Retire only the old test routes; no process/death proof is involved.
        disconnected(&mut peers, 2);
        disconnected(&mut peers, 3);
        assert!(announce(&mut peers, 1, proto::CAP_AMPHORA | proto::CAP_UNIFIED_LAYOUT, replacement).map_err(anyhow::Error::msg)?);
        assert!(peers.get(&1).is_some_and(|p| p.capable && p.on_connection(replacement) && !p.on_connection(None)));
        Ok(())
    }
}
