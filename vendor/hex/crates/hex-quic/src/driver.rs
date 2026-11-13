//! The driver (docs/design/hex-quic.md Q2): one thread owns hyper-transport's endpoint and its UDP socket, and waits
//! on the socket and a wake of its own until the endpoint's next timeout. Other threads reach it through a bounded
//! queue of [`Command`]s and the wake; each connection's bytes cross through its own seats ([`crate::conn`]).
//! Shards' driver for hyper-quic has this shape (Shards `crates/remote/src/net.rs`, D180), as the node agent's I/O
//! thread does (`hex-node` `serve.rs`).

use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hex_core::lanes::{self, Kind, LaneId, Mux, Out, Why};
use hyper_quic::rustls::{CipherSuite, NamedGroup};
use hyper_transport::tls::CertificateDer;
use hyper_transport::{Authenticated, Directory, Event, PeerId};

use crate::classes::{Hex, Role};
use crate::conn::{Conn, Shared};
use crate::limits::Derived;
use crate::{Admits, Negotiated, Refused};

/// The endpoint the driver owns.
type Endpoint = hyper_transport::Endpoint<Hex, hyper_transport::Fixed, Names>;

/// What another thread asks of the driver.
pub(crate) enum Command {
    Dial {
        name: String,
        address: SocketAddr,
        reply: hex_wait::mpsc::SyncSender<Result<PeerId, Refused>>,
    },
    Open {
        peer: PeerId,
        port: u32,
        reply: SyncSender<Result<Arc<Shared>, Refused>>,
    },
    /// A connection's outbox holds a frame.
    Send {
        peer: PeerId,
        conn: lanes::Conn,
    },
    End {
        peer: PeerId,
        conn: lanes::Conn,
    },
    Reset {
        peer: PeerId,
        conn: lanes::Conn,
        why: Why,
    },
    /// A connection's owner read `bytes` whole: the peer may send that many more.
    Consumed {
        peer: PeerId,
        conn: lanes::Conn,
        bytes: u64,
    },
    Negotiated {
        peer: PeerId,
        reply: SyncSender<Option<(CipherSuite, Option<NamedGroup>)>>,
    },
    Links {
        reply: SyncSender<Vec<Negotiated>>,
    },
    /// The peer holds at most `lanes` connections at once: this side opens no more.
    Limit {
        peer: PeerId,
        lanes: u32,
    },
    Stop,
}

/// What wakes the driver from its wait: a loopback datagram socket connected to itself, so no other socket can
/// write it, on every OS (Winsock's `WSAPoll` waits on sockets alone; `hex-client` `calls.rs`' Windows wake).
struct Wake {
    sock: UdpSocket,
}

impl Wake {
    fn new() -> std::io::Result<Self> {
        let sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        sock.connect(sock.local_addr()?)?;
        sock.set_nonblocking(true)?;
        Ok(Self { sock })
    }

    fn wake(&self) {
        let _ = self.sock.send(&[1]);
    }

    fn drain(&self) {
        let mut buf = [0u8; 1];
        while self.sock.recv(&mut buf).is_ok() {}
    }
}

/// The commands waiting for the driver, at most its bound ([`Derived::commands`]); grown as they come, never laid
/// out at the bound.
struct Queue {
    state: Mutex<QueueState>,
    room: Condvar,
    bound: usize,
}

struct QueueState {
    commands: VecDeque<Command>,
    /// The driver has ended: nothing more is taken.
    closed: bool,
}

/// Where commands go: the bounded queue and the driver's wake.
#[derive(Clone)]
pub(crate) struct Commands {
    queue: Arc<Queue>,
    wake: Arc<Wake>,
}

impl Commands {
    /// Queues `command` and wakes the driver; waits for room while the queue holds its bound, which the driver
    /// empties every turn. Refused once the driver has ended.
    pub(crate) fn send(&self, command: Command) -> Result<(), Refused> {
        let mut state = self.queue.state.lock().map_err(|_| Refused::Ended)?;
        while state.commands.len() >= self.queue.bound && !state.closed {
            state = self.queue.room.wait(state).map_err(|_| Refused::Ended)?;
        }
        if state.closed {
            return Err(Refused::Ended);
        }
        state.commands.push_back(command);
        drop(state);
        self.wake.wake();
        Ok(())
    }
}

impl Queue {
    /// The commands waiting, at most `most`.
    fn take(&self, most: usize) -> Vec<Command> {
        let Ok(mut state) = self.state.lock() else {
            return vec![Command::Stop];
        };
        let n = most.min(state.commands.len());
        let taken: Vec<Command> = state.commands.drain(..n).collect();
        drop(state);
        self.room.notify_all();
        taken
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.commands.clear();
        }
        self.room.notify_all();
    }
}

/// Which peer a connection's handshake names: the DNS name its certificate carries, which kiln's CA binds at
/// enrollment (docs/design/hex-quic.md Q5), as its id ([`peer_id`]); a connection the peer opened, only where the
/// endpoint admits that name (Q10).
pub(crate) struct Names {
    /// The names this side dials, by id: the name a server's certificate is checked against.
    dialing: BTreeMap<PeerId, String>,
    admits: Admits,
    /// The name each handshake identified since the driver last polled the endpoint's events. hyper-transport queues
    /// an admitted connection's `Event::Connected` within the call that identifies it, so the poll after takes each
    /// admitted one's name; what is left was refused, and goes then.
    identified: BTreeMap<PeerId, String>,
}

/// A peer's id: the first eight bytes of the SHA-256 of its name, so a dialer knows the id of the peer it dials
/// before it hears from it, and a peer that authenticates as another name is another id, refused for a dial.
pub fn peer_id(name: &str) -> PeerId {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(name.as_bytes());
    let mut id = [0u8; 8];
    for (to, from) in id.iter_mut().zip(digest.iter()) {
        *to = *from;
    }
    PeerId::from_be_bytes(id)
}

/// The first DNS name a certificate carries.
pub(crate) fn certificate_name(der: &[u8]) -> Option<String> {
    let der = CertificateDer::from(der);
    let cert = webpki::EndEntityCert::try_from(&der).ok()?;
    cert.valid_dns_names().next().map(str::to_owned)
}

impl Directory for Names {
    type Role = Role;

    fn identify(&mut self, authenticated: &Authenticated<'_>) -> Option<(PeerId, Role)> {
        if authenticated.protocol != crate::tls::ALPN {
            return None;
        }
        let name = certificate_name(authenticated.certificate?)?;
        let peer = peer_id(&name);
        let admitted = match authenticated.dialed {
            Some(dialed) => dialed == peer,
            None => self.admits.admits(&name),
        };
        if admitted {
            self.identified.insert(peer, name);
            Some((peer, Role::Member))
        } else {
            None
        }
    }

    fn server_name(&self, peer: PeerId) -> Option<&str> {
        self.dialing.get(&peer).map(String::as_str)
    }
}

/// What the driver keeps for one peer it is connected to.
struct PeerState {
    epoch: hyper_transport::Epoch,
    /// The name its certificate carries.
    name: String,
    /// The peer admitted this side (its Welcome came), or this side accepted it.
    welcomed: bool,
    mux: Mux,
    conns: BTreeMap<lanes::Conn, Arc<Shared>>,
    /// Frames each of this side's lanes holds back until the lane has room, in order.
    waiting: BTreeMap<LaneId, VecDeque<(Kind, Vec<u8>)>>,
    /// Connections whose End waits for the frame in their outbox to go first.
    end_after: std::collections::BTreeSet<lanes::Conn>,
}

/// The frames one of this side's lanes may hold back: two connections' (the live one's opening, its one data frame,
/// its End and its Reset; its predecessor's End and Reset), since a lane carries a new connection only once the
/// last is gone. Past it the peer reads no lane while ending connections, and its connection is closed.
const WAITING: usize = 6;

/// Connections accepted for a port and not yet taken by its owner.
#[derive(Default)]
pub(crate) struct Accepts {
    queues: hex_wait::Mutex<AcceptQueues>,
    came: hex_wait::Condvar,
}

#[derive(Default)]
struct AcceptQueues {
    by_port: BTreeMap<u32, VecDeque<(PeerId, Arc<Shared>)>>,
    /// The driver has ended: no connection comes.
    closed: bool,
}

/// An accept's wait that couldn't wait: the queues' lock poisoned, the driver gone with it, or the OS refused it
/// (macOS's kqueue).
fn wait_refused(why: hex_wait::Failed) -> Refused {
    match why {
        hex_wait::Failed::Poisoned => Refused::Ended,
        hex_wait::Failed::Os(why) => Refused::Os {
            op: "waiting for an accepted connection",
            why: why.to_string(),
        },
    }
}

impl Accepts {
    pub(crate) fn take(
        &self,
        port: u32,
        deadline: Option<Instant>,
    ) -> Result<(PeerId, Arc<Shared>), Refused> {
        let mut queues = self.queues.lock().map_err(|_| Refused::Ended)?;
        loop {
            if let Some(next) = queues.by_port.get_mut(&port).and_then(VecDeque::pop_front) {
                return Ok(next);
            }
            if queues.closed {
                return Err(Refused::Ended);
            }
            queues = match deadline {
                None => self.came.wait(queues).map_err(wait_refused)?,
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Refused::Deadline {
                            what: "an accepted connection",
                        });
                    }
                    self.came
                        .wait_timeout(queues, left)
                        .map_err(wait_refused)?
                        .0
                }
            };
        }
    }

    fn put(&self, port: u32, peer: PeerId, shared: Arc<Shared>) {
        if let Ok(mut queues) = self.queues.lock() {
            queues
                .by_port
                .entry(port)
                .or_default()
                .push_back((peer, shared));
        }
        self.came.notify_all();
    }

    fn close(&self) {
        if let Ok(mut queues) = self.queues.lock() {
            queues.closed = true;
            for (_, queue) in std::mem::take(&mut queues.by_port) {
                for (_, shared) in queue {
                    shared.gone(Why::Closed);
                }
            }
        }
        self.came.notify_all();
    }
}

pub(crate) struct Driver {
    ep: Endpoint,
    sock: UdpSocket,
    wake: Arc<Wake>,
    commands: Arc<Queue>,
    /// A clone of the senders' end, for the connections the driver makes.
    sender: Commands,
    accepts: Arc<Accepts>,
    derived: Derived,
    serves: Vec<u32>,
    peers: BTreeMap<PeerId, PeerState>,
    dials: BTreeMap<PeerId, Vec<hex_wait::mpsc::SyncSender<Result<PeerId, Refused>>>>,
    out: Vec<u8>,
    buf: Vec<u8>,
}

/// The driver's thread and what its owner reaches it by.
pub(crate) struct Started {
    pub(crate) commands: Commands,
    pub(crate) accepts: Arc<Accepts>,
    pub(crate) local: SocketAddr,
    pub(crate) thread: std::thread::JoinHandle<()>,
}

impl Driver {
    /// A driver admitting every peer kiln's CA enrolled: the tests' endpoints of another TLS (`tests.rs`, `under`).
    #[cfg(test)]
    pub(crate) fn start(
        ep_config: hyper_transport::Config<Role>,
        budget: hyper_transport::Fixed,
        bind: SocketAddr,
        derived: Derived,
        serves: Vec<u32>,
    ) -> Result<Started, Refused> {
        Self::start_admitting(ep_config, budget, bind, derived, serves, Admits::Enrolled)
    }

    pub(crate) fn start_admitting(
        ep_config: hyper_transport::Config<Role>,
        budget: hyper_transport::Fixed,
        bind: SocketAddr,
        derived: Derived,
        serves: Vec<u32>,
        admits: Admits,
    ) -> Result<Started, Refused> {
        let os = |op: &'static str| {
            move |e: std::io::Error| Refused::Os {
                op,
                why: e.to_string(),
            }
        };
        let sock = UdpSocket::bind(bind).map_err(os("binding the endpoint's socket"))?;
        sock.set_nonblocking(true)
            .map_err(os("making the endpoint's socket non-blocking"))?;
        let local = sock
            .local_addr()
            .map_err(os("reading the endpoint's address"))?;
        let names = Names {
            dialing: BTreeMap::new(),
            admits,
            identified: BTreeMap::new(),
        };
        let ep = Endpoint::new(ep_config, budget, names, Instant::now())
            .map_err(|refusal| Refused::Transport { refusal })?;
        let wake = Arc::new(Wake::new().map_err(os("making the driver's wake"))?);
        let commands = Arc::new(Queue {
            state: Mutex::new(QueueState {
                commands: VecDeque::new(),
                closed: false,
            }),
            room: Condvar::new(),
            bound: derived.commands,
        });
        let sender = Commands {
            queue: Arc::clone(&commands),
            wake: Arc::clone(&wake),
        };
        let accepts = Arc::new(Accepts::default());
        let driver = Driver {
            ep,
            sock,
            wake,
            commands,
            sender: sender.clone(),
            accepts: Arc::clone(&accepts),
            derived,
            serves,
            peers: BTreeMap::new(),
            dials: BTreeMap::new(),
            out: Vec::with_capacity(hyper_transport::RECEIVE_CHUNK),
            buf: vec![0u8; hyper_transport::RECEIVE_CHUNK],
        };
        let thread = std::thread::Builder::new()
            .name("hex-quic-driver".to_owned())
            .spawn(move || driver.run())
            .map_err(os("starting the driver's thread"))?;
        Ok(Started {
            commands: sender,
            accepts,
            local,
            thread,
        })
    }

    fn run(mut self) {
        loop {
            self.receive();
            if self.commands() {
                break;
            }
            let now = Instant::now();
            if self.ep.poll_timeout().is_some_and(|due| due <= now) {
                self.ep.handle_timeout(now);
            }
            self.events();
            self.transmit();
            let wait = self
                .ep
                .poll_timeout()
                .map(|due| due.saturating_duration_since(Instant::now()));
            self.wait(wait);
        }
        self.commands.close();
        self.stop();
        self.accepts.close();
    }

    /// Ends every connection, sends what that says, and tells every connection and dial why.
    fn stop(&mut self) {
        let now = Instant::now();
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            self.ep.disconnect(now, peer);
            self.peer_gone(peer, Refused::Ended);
        }
        self.transmit();
        for (_, waiters) in std::mem::take(&mut self.dials) {
            for waiter in waiters {
                let _ = waiter.try_send(Err(Refused::Ended));
            }
        }
    }

    /// Waits on the socket and the wake, at most `wait` (none: until either is ready).
    fn wait(&self, wait: Option<Duration>) {
        #[cfg(unix)]
        let (sock, wake) = (
            std::os::fd::AsFd::as_fd(&self.sock),
            std::os::fd::AsFd::as_fd(&self.wake.sock),
        );
        #[cfg(windows)]
        let (sock, wake) = (
            std::os::windows::io::AsSocket::as_socket(&self.sock),
            std::os::windows::io::AsSocket::as_socket(&self.wake.sock),
        );
        let mut fds = [
            rustix::event::PollFd::from_borrowed_fd(sock, rustix::event::PollFlags::IN),
            rustix::event::PollFd::from_borrowed_fd(wake, rustix::event::PollFlags::IN),
        ];
        let timeout = wait.map(crate::limits::timespec);
        let _ = rustix::event::poll(&mut fds, timeout.as_ref());
        self.wake.drain();
    }

    /// Every datagram the socket holds, at most the endpoint's receive chunks a turn, each handed to the endpoint
    /// and what it answers sent before the next (hyper-transport `Limits::max_responses`).
    fn receive(&mut self) {
        for _ in 0..self.derived.receive_turn {
            match self.sock.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    let bytes = self.buf.get(..n).unwrap_or_default();
                    self.ep.handle_datagram(Instant::now(), from, None, bytes);
                    self.transmit();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // An ICMP unreachable reported on a UDP socket (Winsock's WSAECONNRESET): the path's, whose
                // connection QUIC times out; the socket reads on.
                Err(_) => {}
            }
        }
    }

    fn transmit(&mut self) {
        while let Some(transmit) = self.ep.poll_transmit(Instant::now(), &mut self.out) {
            let bytes = self.out.get(..transmit.size).unwrap_or_default();
            // A datagram the socket has no room for is lost as a path loses one: QUIC's loss detection counts it
            // (the connection's stats) and sends its frames again (RFC 9002 §6).
            let _ = self.sock.send_to(bytes, transmit.destination);
        }
    }

    /// Runs what was asked, at most the queue's bound a turn; whether the driver is to stop.
    fn commands(&mut self) -> bool {
        for command in self.commands.take(self.derived.commands) {
            if matches!(command, Command::Stop) {
                return true;
            }
            self.command(command);
        }
        false
    }

    fn command(&mut self, command: Command) {
        let now = Instant::now();
        match command {
            Command::Dial {
                name,
                address,
                reply,
            } => {
                let peer = peer_id(&name);
                if self.peers.get(&peer).is_some_and(|state| state.welcomed) {
                    let _ = reply.try_send(Ok(peer));
                    return;
                }
                if self.peers.contains_key(&peer) {
                    self.dials.entry(peer).or_default().push(reply);
                    return;
                }
                self.ep.directory_mut().dialing.insert(peer, name);
                match self.ep.connect(now, peer, address) {
                    Ok(()) => self.dials.entry(peer).or_default().push(reply),
                    Err(refusal) => {
                        let _ = reply.try_send(Err(Refused::Transport { refusal }));
                    }
                }
            }
            Command::Open { peer, port, reply } => {
                let _ = reply.try_send(self.open(peer, port));
            }
            Command::Send { peer, conn } => self.send(peer, conn),
            Command::End { peer, conn } => {
                if let Some(state) = self.peers.get_mut(&peer) {
                    // Its End follows the frame its outbox still holds.
                    if state
                        .conns
                        .get(&conn)
                        .is_some_and(|shared| shared.outbox_waits())
                    {
                        state.end_after.insert(conn);
                        return;
                    }
                    let outs = state.mux.end(conn);
                    self.apply(peer, outs);
                }
            }
            Command::Reset { peer, conn, why } => {
                if let Some(state) = self.peers.get_mut(&peer) {
                    state.conns.remove(&conn);
                    let outs = state.mux.reset(conn, why);
                    self.apply(peer, outs);
                }
            }
            Command::Consumed { peer, conn, bytes } => {
                if let Some(out) = self
                    .peers
                    .get_mut(&peer)
                    .and_then(|state| state.mux.grant(conn, bytes))
                {
                    self.apply(peer, vec![out]);
                }
            }
            Command::Negotiated { peer, reply } => {
                let _ = reply.try_send(self.ep.negotiated(peer));
            }
            Command::Links { reply } => {
                let links = self
                    .peers
                    .iter()
                    .filter_map(|(peer, state)| {
                        let (suite, group) = self.ep.negotiated(*peer)?;
                        Some(Negotiated {
                            peer: state.name.clone(),
                            suite,
                            group,
                        })
                    })
                    .collect();
                let _ = reply.try_send(links);
            }
            Command::Limit { peer, lanes } => {
                if let Some(state) = self.peers.get_mut(&peer) {
                    state.mux.limit(lanes);
                }
            }
            Command::Stop => {}
        }
    }

    fn open(&mut self, peer: PeerId, port: u32) -> Result<Arc<Shared>, Refused> {
        let state = self.peers.get_mut(&peer).ok_or(Refused::NotConnected)?;
        let (conn, out) = state
            .mux
            .connect(port)
            .map_err(|why| Refused::Conn { why })?;
        let shared = Arc::new(Shared::new(peer, conn, port, self.sender.clone()));
        state.conns.insert(conn, Arc::clone(&shared));
        self.apply(peer, vec![out]);
        Ok(shared)
    }

    /// A connection's outbox holds a frame: it goes, unless its lane holds frames back or the peer's credit has no
    /// room for it, when it waits in the outbox, and its writer with it, for the lane's room or the peer's Credit.
    fn send(&mut self, peer: PeerId, conn: lanes::Conn) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let Some(shared) = state.conns.get(&conn) else {
            return;
        };
        let lane_waits = state
            .mux
            .our_lane(conn)
            .is_some_and(|lane| state.waiting.get(&lane).is_some_and(|w| !w.is_empty()));
        let room = usize::try_from(state.mux.room(conn)).unwrap_or(usize::MAX);
        if lane_waits || shared.outbox_len().is_none_or(|len| len > room) {
            return;
        }
        let Some(bytes) = shared.take_outbox() else {
            return;
        };
        let shared = Arc::clone(shared);
        let outs = state.mux.send(conn, &bytes);
        match outs {
            Ok(outs) => self.apply(peer, outs),
            Err(why) => shared.gone(why),
        }
        shared.sent();
        if let Some(state) = self.peers.get_mut(&peer)
            && state.end_after.remove(&conn)
        {
            let outs = state.mux.end(conn);
            self.apply(peer, outs);
        }
    }

    fn events(&mut self) {
        while let Some(event) = self.ep.poll_event() {
            self.event(event);
        }
        self.ep.directory_mut().identified.clear();
    }

    fn event(&mut self, event: Event<Hex>) {
        match event {
            Event::Connected { peer, epoch, .. } => {
                let Some(name) = self.ep.directory_mut().identified.remove(&peer) else {
                    // A connection whose handshake's name the driver didn't see can't be named: refused, as an
                    // identity it can't take is.
                    self.peer_gone(peer, Refused::Identity);
                    self.ep
                        .refuse(Instant::now(), peer, hyper_transport::Refusal::Identity);
                    return;
                };
                let mux = Mux::new(
                    lanes::Limits {
                        lanes: self.derived.lanes,
                    },
                    &self.serves,
                );
                // A connection this side dialed waits for the peer's Welcome; one the peer opened is admitted here,
                // its handshake verified, and is told so.
                let dialed = self.ep.directory().dialing.contains_key(&peer);
                let welcome = (!dialed).then(|| mux.welcome());
                self.peers.insert(
                    peer,
                    PeerState {
                        epoch,
                        name,
                        welcomed: !dialed,
                        mux,
                        conns: BTreeMap::new(),
                        waiting: BTreeMap::new(),
                        end_after: std::collections::BTreeSet::new(),
                    },
                );
                if let Some(welcome) = welcome {
                    self.apply(peer, vec![welcome]);
                }
            }
            Event::Closed {
                peer,
                epoch,
                refusal,
            } => {
                if self
                    .peers
                    .get(&peer)
                    .is_some_and(|state| state.epoch == epoch)
                {
                    self.peer_gone(peer, Refused::closed(refusal));
                }
                self.dial_failed(peer, Refused::closed(refusal));
            }
            Event::Unreachable { peer, refusal } => {
                self.dial_failed(peer, Refused::unreachable(refusal));
            }
            Event::Frame {
                peer,
                lane,
                kind,
                frame,
            } => {
                let bytes = frame.bytes().to_vec();
                self.ep.release(frame);
                let Some(state) = self.peers.get_mut(&peer) else {
                    return;
                };
                let made = state.mux.frame(lane, kind, bytes);
                match made {
                    Ok(outs) => self.apply(peer, outs),
                    Err(_broken) => {
                        self.ep
                            .refuse(Instant::now(), peer, hyper_transport::Refusal::Corrupt);
                        self.peer_gone(peer, Refused::Broken);
                    }
                }
            }
            Event::LaneRoom { peer, lane, .. } => self.flush(peer, lane),
            Event::Request { exchange, .. } => {
                // Hex opens no exchange; a peer's is refused by type.
                self.ep
                    .refuse_exchange(exchange, hyper_transport::Refusal::Kind);
            }
            Event::Reply { .. }
            | Event::BodyReady { .. }
            | Event::Writable { .. }
            | Event::Refused { .. }
            | Event::Degrading { .. }
            | Event::Progressing { .. } => {}
        }
    }

    fn dial_failed(&mut self, peer: PeerId, why: Refused) {
        for waiter in self.dials.remove(&peer).unwrap_or_default() {
            let _ = waiter.try_send(Err(why.clone()));
        }
        if !self.peers.contains_key(&peer) {
            self.ep.directory_mut().dialing.remove(&peer);
        }
    }

    /// The connection to `peer` is gone: every connection on it hears why.
    fn peer_gone(&mut self, peer: PeerId, _why: Refused) {
        let Some(mut state) = self.peers.remove(&peer) else {
            return;
        };
        for out in state.mux.closed() {
            if let Out::Reset { conn, why } = out
                && let Some(shared) = state.conns.remove(&conn)
            {
                shared.gone(why);
            }
        }
        for (_, shared) in std::mem::take(&mut state.conns) {
            shared.gone(Why::Closed);
        }
        self.ep.directory_mut().dialing.remove(&peer);
    }

    /// Lane `lane` has room: what it held back goes, then the outbox of the connection on it.
    fn flush(&mut self, peer: PeerId, lane: LaneId) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let Some(waiting) = state.waiting.get_mut(&lane) else {
            return;
        };
        while let Some((kind, payload)) = waiting.front() {
            match self.ep.send_frame(peer, lane, *kind, payload) {
                Ok(()) => {
                    waiting.pop_front();
                }
                Err(hyper_transport::Refusal::LaneFull) => return,
                Err(_) => {
                    waiting.clear();
                    break;
                }
            }
        }
        state.waiting.remove(&lane);
        // The connection on the lane, if its outbox waited for it.
        let conn = state
            .conns
            .keys()
            .copied()
            .find(|conn| state.mux.our_lane(*conn) == Some(lane));
        if let Some(conn) = conn {
            self.send(peer, conn);
        }
    }

    /// Does what the mux said.
    fn apply(&mut self, peer: PeerId, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Send {
                    lane,
                    kind,
                    payload,
                } => self.send_frame(peer, lane, kind, payload),
                Out::Accepted { conn, port } => {
                    let Some(state) = self.peers.get_mut(&peer) else {
                        continue;
                    };
                    let shared = Arc::new(Shared::new(peer, conn, port, self.sender.clone()));
                    state.conns.insert(conn, Arc::clone(&shared));
                    self.accepts.put(port, peer, shared);
                }
                Out::Opened { .. } => {}
                // The peer's room for it grew: its outbox, if it waited for it, goes.
                Out::Credit { conn } => self.send(peer, conn),
                Out::Welcomed => {
                    if let Some(state) = self.peers.get_mut(&peer) {
                        state.welcomed = true;
                    }
                    for waiter in self.dials.remove(&peer).unwrap_or_default() {
                        let _ = waiter.try_send(Ok(peer));
                    }
                }
                Out::Refused { conn, why } | Out::Reset { conn, why } => {
                    if let Some(shared) = self
                        .peers
                        .get_mut(&peer)
                        .and_then(|state| state.conns.remove(&conn))
                    {
                        shared.gone(why);
                    }
                }
                Out::Data { conn, bytes } => {
                    if let Some(shared) = self
                        .peers
                        .get(&peer)
                        .and_then(|state| state.conns.get(&conn))
                    {
                        shared.arrived(bytes);
                    }
                }
                Out::Ended { conn } => {
                    if let Some(shared) = self
                        .peers
                        .get(&peer)
                        .and_then(|state| state.conns.get(&conn))
                    {
                        shared.ended();
                    }
                }
                Out::Closed { conn } => {
                    if let Some(state) = self.peers.get_mut(&peer) {
                        state.conns.remove(&conn);
                    }
                }
            }
        }
    }

    /// Queues a frame on this side's lane, after what the lane holds back; a lane with no room holds it back.
    fn send_frame(&mut self, peer: PeerId, lane: LaneId, kind: Kind, payload: Vec<u8>) {
        let Some(state) = self.peers.get_mut(&peer) else {
            return;
        };
        let waiting = state.waiting.entry(lane).or_default();
        if waiting.is_empty() {
            match self.ep.send_frame(peer, lane, kind, &payload) {
                Ok(()) => {
                    state.waiting.remove(&lane);
                    return;
                }
                Err(hyper_transport::Refusal::LaneFull) => {}
                Err(_) => {
                    state.waiting.remove(&lane);
                    return;
                }
            }
        }
        if waiting.len() >= WAITING {
            self.ep
                .refuse(Instant::now(), peer, hyper_transport::Refusal::Busy);
            self.peer_gone(peer, Refused::Broken);
            return;
        }
        waiting.push_back((kind, payload));
    }
}

/// Waits for a dial's answer until `deadline` (none: until the driver tells it, or ends).
pub(crate) fn wait_dial(
    answer: &hex_wait::mpsc::Receiver<Result<PeerId, Refused>>,
    deadline: Option<Instant>,
) -> Result<PeerId, Refused> {
    use hex_wait::mpsc::RecvTimeoutError;
    let answered = match deadline {
        Some(deadline) => answer.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        None => answer.recv().map_err(RecvTimeoutError::from),
    };
    match answered {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(Refused::Deadline { what: "a dial" }),
        Err(RecvTimeoutError::Disconnected) => Err(Refused::Ended),
        Err(RecvTimeoutError::Failed(why)) => Err(Refused::Os {
            op: "waiting for a dial's answer",
            why: why.to_string(),
        }),
    }
}

/// Wraps a connection the driver accepted for its owner.
pub(crate) fn conn(shared: Arc<Shared>) -> Conn {
    Conn { shared }
}

#[cfg(test)]
mod tests;
