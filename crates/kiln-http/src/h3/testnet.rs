//! A door and hyper-quic clients playing browsers, joined by an in-memory network with virtual
//! time (goal 4 steps 5, 8 and 9): no socket, no wall clock past one anchor read, and every
//! choice the network makes (loss, reordering, delay) from one seed.

use std::collections::BinaryHeap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use hyper_quic::rustls;
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use hyper_quic::{ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig, StreamId, TransportConfig, VarInt};

use super::conn::{Admission, Connect, Event, Read};
use super::door::{ConnId, Door, DoorConfig};

/// A self-signed ECDSA P-256 certificate for `localhost` and its key (DER).
pub(crate) fn cert() -> Option<(Vec<u8>, Vec<u8>)> {
    let key = rcgen::KeyPair::generate().ok()?;
    let params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).ok()?;
    let cert = params.self_signed(&key).ok()?;
    Some((cert.der().to_vec(), key.serialize_der()))
}

/// The xorshift every choice comes from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rng(pub u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// A datagram on the wire.
#[derive(PartialEq, Eq)]
struct Flight {
    at: Instant,
    seq: u64,
    from: SocketAddr,
    to: SocketAddr,
    bytes: Vec<u8>,
}

impl Ord for Flight {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.at, other.seq).cmp(&(self.at, self.seq))
    }
}

impl PartialOrd for Flight {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A hyper-quic client playing a browser: its endpoint, its one connection.
pub(crate) struct Browser {
    pub endpoint: Endpoint,
    pub conn: Option<(ConnectionHandle, Connection)>,
    pub addr: SocketAddr,
    /// Bytes it received per stream, in order.
    pub got: std::collections::BTreeMap<u64, Vec<u8>>,
    pub finished: Vec<u64>,
    pub resets: Vec<(u64, u64)>,
    pub datagrams: Vec<Bytes>,
    /// Its connection's soonest timer, as last asked.
    deadline: Option<Instant>,
}

impl Browser {
    pub(crate) fn c(&mut self) -> Option<&mut Connection> {
        self.conn.as_mut().map(|(_, c)| c)
    }

    pub(crate) fn connected(&self) -> bool {
        self.conn.as_ref().is_some_and(|(_, c)| !c.is_handshaking() && !c.is_closed())
    }

    pub(crate) fn closed(&self) -> bool {
        self.conn.as_ref().is_none_or(|(_, c)| c.is_closed())
    }

    pub(crate) fn open(&mut self, bidi: bool) -> Option<u64> {
        self.c()?.streams().open(if bidi { Dir::Bi } else { Dir::Uni }).map(u64::from)
    }

    /// Writes all of `bytes` it can on `id`: the bytes taken.
    pub(crate) fn write(&mut self, id: u64, bytes: &[u8]) -> usize {
        let Some(c) = self.c() else { return 0 };
        let Some(s) = VarInt::from_u64(id).ok().map(StreamId::from) else { return 0 };
        c.send_stream(s).write(bytes).unwrap_or(0)
    }

    pub(crate) fn finish(&mut self, id: u64) {
        if let (Some(c), Some(s)) = (self.conn.as_mut().map(|(_, c)| c), VarInt::from_u64(id).ok().map(StreamId::from)) {
            let _ = c.send_stream(s).finish();
        }
    }

    pub(crate) fn reset(&mut self, id: u64, code: u64) {
        if let (Some(c), Some(s), Ok(code)) = (self.conn.as_mut().map(|(_, c)| c), VarInt::from_u64(id).ok().map(StreamId::from), VarInt::from_u64(code)) {
            let _ = c.send_stream(s).reset(code);
        }
    }

    /// STOP_SENDING on a stream the peer sends on.
    pub(crate) fn stop(&mut self, id: u64, code: u64) {
        if let (Some(c), Some(s), Ok(code)) = (self.conn.as_mut().map(|(_, c)| c), VarInt::from_u64(id).ok().map(StreamId::from), VarInt::from_u64(code)) {
            let _ = c.recv_stream(s).stop(code);
        }
    }

    pub(crate) fn datagram(&mut self, bytes: &[u8]) -> bool {
        self.c().is_some_and(|c| c.datagrams().send(Bytes::copy_from_slice(bytes), true).is_ok())
    }

    /// Reads what arrived on every stream.
    fn pump(&mut self) {
        let Some((_, c)) = self.conn.as_mut() else { return };
        while let Some(ev) = c.poll() {
            match ev {
                hyper_quic::Event::Stream(hyper_quic::StreamEvent::Opened { dir }) => {
                    while let Some(s) = c.streams().accept(dir) {
                        self.got.entry(u64::from(s)).or_default();
                    }
                }
                hyper_quic::Event::DatagramReceived => {
                    while let Some(d) = c.datagrams().recv() {
                        self.datagrams.push(d);
                    }
                }
                _ => {}
            }
        }
        let ids: Vec<u64> = self.got.keys().copied().collect();
        for id in ids {
            let Some(s) = VarInt::from_u64(id).ok().map(StreamId::from) else { continue };
            let mut recv = c.recv_stream(s);
            let Ok(mut chunks) = recv.read(true) else { continue };
            loop {
                match chunks.next(usize::MAX) {
                    Ok(Some(ch)) => self.got.entry(id).or_default().extend_from_slice(&ch.bytes),
                    Ok(None) => {
                        if !self.finished.contains(&id) {
                            self.finished.push(id);
                        }
                        break;
                    }
                    Err(hyper_quic::ReadError::Reset(code)) => {
                        self.resets.push((id, code.into_inner()));
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = chunks.finalize();
        }
    }

    /// Watches a stream of ours (a bidi stream's replies).
    pub(crate) fn watch(&mut self, id: u64) {
        self.got.entry(id).or_default();
    }
}

/// What a test's door does with what it's told: echoes, like the lab's.
pub(crate) trait App {
    fn admit(&mut self, c: &Connect) -> Admission {
        let _ = c;
        Admission::Accept
    }
    fn event(&mut self, door: &mut Door, id: ConnId, e: Event, now: Instant) {
        echo(door, id, e, now);
    }
}

/// Echoes a session's streams and datagrams back.
pub(crate) fn echo(door: &mut Door, id: ConnId, e: Event, now: Instant) {
    let Some(mut s) = door.session(id, now) else { return };
    match e {
        Event::Readable(sid) => loop {
            match s.read(sid, 4096) {
                Read::Data(b) => {
                    if sid & 2 == 0 {
                        let _ = s.write(sid, &b);
                    } else if let Some(out) = s.open(false) {
                        let _ = s.write(out, &b);
                        s.finish(out);
                    }
                }
                Read::Fin => {
                    if sid & 2 == 0 {
                        s.finish(sid);
                    }
                    s.forget(sid);
                    break;
                }
                _ => break,
            }
        },
        Event::Datagram => {
            while let Some(d) = s.recv_datagram() {
                let _ = s.send_datagram(&d);
            }
        }
        _ => {}
    }
}

pub(crate) struct Echo;
impl App for Echo {}

/// The door, browsers, the wire and virtual time.
pub(crate) struct Net<A: App> {
    pub now: Instant,
    pub door: Door,
    pub door_addr: SocketAddr,
    pub app: A,
    pub browsers: Vec<Browser>,
    wire: BinaryHeap<Flight>,
    seq: u64,
    pub rng: Rng,
    /// Loss and reordering, parts per million of datagrams.
    pub loss_ppm: u64,
    pub reorder_ppm: u64,
    pub delay: Duration,
    root: Vec<u8>,
    buf: Vec<u8>,
    /// Bytes the door sent to, and received from, each address.
    pub door_sent: std::collections::BTreeMap<SocketAddr, usize>,
    pub door_received: std::collections::BTreeMap<SocketAddr, usize>,
    pub events: Vec<(ConnId, Event)>,
    /// The door's replies are written here (made once, before any counting).
    reply: Vec<u8>,
    /// Counting the door's allocations alone: on only while the door's code runs.
    measuring: bool,
    /// Addresses whose datagrams from the door are lost (spoofed sources, RFC 9000 §8).
    pub black_holes: Vec<SocketAddr>,
    /// Browsers that may have something to do (sent to, written on, a timer fired): only these
    /// are asked, so a step costs what moved, not every browser.
    active: std::collections::BTreeSet<usize>,
    by_addr: std::collections::BTreeMap<SocketAddr, usize>,
}

/// Turns the door's counting on for `f`.
macro_rules! door_side {
    ($net:expr, $body:expr) => {{
        if $net.measuring {
            hyper_measure::alloc::resume();
        }
        let r = $body;
        if $net.measuring {
            hyper_measure::alloc::pause();
        }
        r
    }};
}

fn addr(n: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, (n >> 8) as u8, n as u8), 40_000))
}

/// The client's TLS: what a browser's is, enough for the door (TLS 1.3, ALPN h3, the root).
fn client_config(root: &[u8]) -> Option<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(root.to_vec())).ok()?;
    let mut c = rustls::ClientConfig::builder_with_provider(&rustls::crypto::aws_lc_rs::DEFAULT_PROVIDER).with_protocol_versions(&[&rustls::version::TLS13]).ok()?.with_root_certificates(roots).with_no_client_auth();
    c.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = hyper_quic::crypto::rustls::QuicClientConfig::try_from(c).ok()?;
    let mut cc = ClientConfig::new(Box::new(crypto));
    let mut t = TransportConfig::default();
    t.datagram_receive_buffer_size(Some(1 << 20));
    cc.transport_config(t);
    Some(cc)
}

impl<A: App> Net<A> {
    pub(crate) fn new(cfg: DoorConfig, app: A, seed: u64) -> Option<Net<A>> {
        let (cert, key) = cert()?;
        let tls = super::door::tls(vec![CertificateDer::from(cert.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))).ok()?;
        let mut s = [0u8; 32];
        s.get_mut(..8)?.copy_from_slice(&seed.to_le_bytes());
        let door = Door::new(cfg, tls, Some(s)).ok()?;
        #[allow(clippy::disallowed_methods, reason = "the one host clock read: virtual time's anchor")]
        let now = Instant::now();
        Some(Net {
            now,
            door,
            door_addr: addr(1),
            app,
            browsers: Vec::new(),
            wire: BinaryHeap::new(),
            seq: 0,
            rng: Rng(seed ^ 0x9e37_79b9_7f4a_7c15),
            loss_ppm: 0,
            reorder_ppm: 0,
            delay: Duration::from_millis(5),
            root: cert,
            buf: Vec::with_capacity(1 << 16),
            door_sent: Default::default(),
            door_received: Default::default(),
            events: Vec::new(),
            measuring: false,
            reply: Vec::with_capacity(1 << 16),
            black_holes: Vec::new(),
            active: Default::default(),
            by_addr: Default::default(),
        })
    }

    /// A browser that dials the door; its index.
    pub(crate) fn dial(&mut self) -> Option<usize> {
        let n = self.browsers.len();
        let b = self.browser(addr(10 + n as u16))?;
        self.by_addr.insert(b.addr, n);
        self.browsers.push(b);
        self.active.insert(n);
        Some(n)
    }

    /// Browser `i` dials again from its address, a new connection in place of its last.
    pub(crate) fn redial(&mut self, i: usize) -> Option<()> {
        let at = self.browsers.get(i)?.addr;
        let b = self.browser(at)?;
        *self.browsers.get_mut(i)? = b;
        self.active.insert(i);
        Some(())
    }

    fn browser(&mut self, at: SocketAddr) -> Option<Browser> {
        let mut seed = [0u8; 32];
        seed.get_mut(..8)?.copy_from_slice(&self.rng.next().to_le_bytes());
        let mut endpoint = Endpoint::new(EndpointConfig::default(), None, true, Some(seed)).ok()?;
        let cfg = endpoint.insert_client_config(client_config(&self.root)?).ok()?;
        let conn = endpoint.connect(self.now, cfg, self.door_addr, "localhost", None).ok()?;
        Some(Browser { endpoint, conn: Some(conn), addr: at, got: Default::default(), finished: Vec::new(), resets: Vec::new(), datagrams: Vec::new(), deadline: None })
    }

    fn put(&mut self, from: SocketAddr, to: SocketAddr, bytes: Vec<u8>) {
        if self.black_holes.contains(&to) {
            return;
        }
        if self.loss_ppm > 0 && self.rng.below(1_000_000) < self.loss_ppm {
            return;
        }
        let mut at = self.now + self.delay;
        if self.reorder_ppm > 0 && self.rng.below(1_000_000) < self.reorder_ppm {
            at += Duration::from_micros(self.rng.below(20_000));
        }
        self.seq += 1;
        self.wire.push(Flight { at, seq: self.seq, from, to, bytes });
    }

    /// Moves everything that can move now; whether anything did.
    pub(crate) fn step(&mut self) -> bool {
        let mut moved = false;
        let now = self.now;
        // Browsers send: those that may have something to.
        let act: Vec<usize> = std::mem::take(&mut self.active).into_iter().collect();
        let mut again = std::collections::BTreeSet::new();
        for &i in &act {
            while let Some(b) = self.browsers.get_mut(i) {
                let from = b.addr;
                let Some((h, c)) = b.conn.as_mut() else { break };
                self.buf.clear();
                let Some(t) = c.poll_transmit(now, 1, &mut self.buf, b.endpoint.configs()) else { break };
                while let Some(ev) = c.poll_endpoint_events() {
                    if let Some(back) = b.endpoint.handle_event(*h, ev) {
                        c.handle_event(back, b.endpoint.configs_mut());
                    }
                }
                let bytes = self.buf.get(..t.size).unwrap_or_default().to_vec();
                self.put(from, t.destination, bytes);
                again.insert(i);
                moved = true;
            }
        }
        // The door sends.
        while let Some(t) = door_side!(self, self.door.poll_transmit(now, 1, &mut self.buf)) {
            let bytes = self.buf.get(..t.size).unwrap_or_default().to_vec();
            *self.door_sent.entry(t.destination).or_default() += bytes.len();
            let from = self.door_addr;
            self.put(from, t.destination, bytes);
            moved = true;
        }
        // Delivery of what's due.
        while self.wire.peek().is_some_and(|f| f.at <= now) {
            let Some(f) = self.wire.pop() else { break };
            moved = true;
            if f.to == self.door_addr {
                *self.door_received.entry(f.from).or_default() += f.bytes.len();
                let app = &mut self.app;
                let mut admit = |c: &Connect| app.admit(c);
                let out = &mut self.reply;
                let door = &mut self.door;
                // The datagram is the door's from here (its stream data may keep it).
                let reply = door_side!(self, door.handle(now, f.from, BytesMut::from(f.bytes.as_slice()), out, &mut admit));
                if let Some(t) = reply {
                    let bytes = self.reply.get(..t.size).unwrap_or_default().to_vec();
                    *self.door_sent.entry(t.destination).or_default() += bytes.len();
                    let from = self.door_addr;
                    self.put(from, t.destination, bytes);
                }
            } else if let Some(&i) = self.by_addr.get(&f.to)
                && let Some(b) = self.browsers.get_mut(i)
            {
                again.insert(i);
                let mut out = Vec::new();
                match b.endpoint.handle(now, f.from, None, None, BytesMut::from(f.bytes.as_slice()), &mut out) {
                    Some(DatagramEvent::ConnectionEvent(_, ev)) => {
                        if let Some((_, c)) = b.conn.as_mut() {
                            c.handle_event(ev, b.endpoint.configs_mut());
                        }
                    }
                    Some(DatagramEvent::Response(t)) => {
                        let bytes = out.get(..t.size).unwrap_or_default().to_vec();
                        let from = b.addr;
                        self.put(from, t.destination, bytes);
                    }
                    _ => {}
                }
            }
        }
        for &i in act.iter().chain(again.iter()) {
            if let Some(b) = self.browsers.get_mut(i) {
                b.pump();
                b.deadline = b.conn.as_mut().and_then(|(_, c)| c.poll_timeout());
            }
        }
        self.active.extend(again);
        while let Some((id, e)) = door_side!(self, self.door.poll_event()) {
            self.events.push((id, e.clone()));
            let (door, app) = (&mut self.door, &mut self.app);
            door_side!(self, {
                app.event(door, id, e, now);
                let mut admit = |c: &Connect| app.admit(c);
                door.flush(id, now, &mut admit);
            });
        }
        moved
    }

    /// Advances virtual time to the next delivery or timer; false when there's neither.
    pub(crate) fn advance(&mut self) -> bool {
        let Some(next) = self.next_due() else { return false };
        self.now = self.now.max(next);
        let now = self.now;
        for (i, b) in self.browsers.iter_mut().enumerate() {
            if b.deadline.is_some_and(|t| t <= now)
                && let Some((_, c)) = b.conn.as_mut()
            {
                c.handle_timeout(now);
                b.deadline = c.poll_timeout();
                self.active.insert(i);
            }
        }
        let (door, app) = (&mut self.door, &mut self.app);
        door_side!(self, {
            let mut admit = |c: &Connect| app.admit(c);
            door.handle_timeout(now, &mut admit);
        });
        true
    }

    /// Runs until `done` or `limit` steps pass; whether `done` held.
    pub(crate) fn until(&mut self, limit: usize, mut done: impl FnMut(&mut Net<A>) -> bool) -> bool {
        for _ in 0..limit {
            if done(self) {
                return true;
            }
            if !self.step() && !self.advance() {
                return done(self);
            }
        }
        done(self)
    }

    /// Runs for `d` of virtual time, firing no timer due after it.
    pub(crate) fn run_for(&mut self, d: Duration) {
        let end = self.now + d;
        for _ in 0..1_000_000 {
            if self.now >= end {
                return;
            }
            if self.step() {
                continue;
            }
            if self.next_due().is_none_or(|t| t > end) {
                self.now = end;
                return;
            }
            self.advance();
        }
    }

    /// The next delivery or timer.
    fn next_due(&mut self) -> Option<Instant> {
        let mut next = self.wire.peek().map(|f| f.at);
        for t in self.browsers.iter().filter_map(|b| b.deadline) {
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        if let Some(t) = door_side!(self, self.door.poll_timeout()) {
            next = Some(next.map_or(t, |n| n.min(t)));
        }
        next
    }

    /// From here, the counting allocator counts the door's allocations alone (the door's code
    /// runs with counting on, everything else with it off).
    pub(crate) fn measure(&mut self) {
        hyper_measure::alloc::begin();
        hyper_measure::alloc::pause();
        self.measuring = true;
    }

    /// The door's live bytes since [`Net::measure`], and its peak.
    pub(crate) fn door_live(&self) -> (i64, i64) {
        let c = hyper_measure::alloc::read();
        (c.live, c.peak)
    }

    /// Browser `i` closes its connection.
    pub(crate) fn hang_up(&mut self, i: usize) {
        let now = self.now;
        if let Some((_, c)) = self.browsers.get_mut(i).and_then(|b| b.conn.as_mut()) {
            c.close(now, VarInt::from_u32(0), Bytes::new());
        }
        self.active.insert(i);
    }

    pub(crate) fn b(&mut self, i: usize) -> Option<&mut Browser> {
        // What the test does on it may give it something to send.
        self.active.insert(i);
        self.browsers.get_mut(i)
    }
}

/// What a browser sends to open a session: its control stream with `settings`, a CONNECT to
/// `path`; the CONNECT's stream id.
pub(crate) fn open_session(b: &mut Browser, settings: &[(u64, u64)], path: &[u8]) -> Option<u64> {
    let ctl = b.open(false)?;
    b.write(ctl, &super::tests::control(settings));
    let id = b.open(true)?;
    b.write(id, &super::tests::wt_connect(path));
    b.watch(id);
    Some(id)
}
