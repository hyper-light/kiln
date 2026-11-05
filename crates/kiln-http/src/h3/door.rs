//! The door's QUIC endpoint (goal 4 step 5, plan 01a1a832eb597110a6dcfda227981604): a raw
//! hyper-quic [`Endpoint`] on its own UDP address, the HTTP/3 core ([`Connection`]) on each of its
//! connections, and every transport limit set from one memory bound a connection.
//!
//! Sans-IO, as hyper-quic is: [`Door::handle`] takes a datagram, [`Door::poll_transmit`] gives
//! the next one to send, [`Door::poll_timeout`] and [`Door::handle_timeout`] carry hyper-quic's
//! timers, and `now` is always passed in. The driver runs it on hyper-rt's UDP socket; the tests
//! run it against a hyper-quic client over an in-memory network.
//!
//! **Windows sized to memory** (RFC 9000 §21.7: the reassembly attack is mitigated only "if flow
//! control windows correspond to available memory"). hyper-quic's defaults hold far more than a
//! fixed bound (a connection window of 2^62, 100 + 100 streams of 1.25 MB each), so none is
//! left to it: [`DoorConfig::transport`] sets each, and a connection with no session yet gets the
//! small [`Limits`], raised (receive window, stream counts, send window) only once its CONNECT is
//! admitted. [`DoorConfig::bound`] is what a connection may hold beyond its idle state.
//!
//! **Connections with no session are the cheap kind** (Slowloris, RFC 9000 §21.6): they are
//! capped endpoint-wide, and when the cap is full the oldest such connection is closed for the
//! new one, never an admitted session; there's no new timer. Past a number of handshakes at once,
//! an unvalidated address is sent a Retry (RFC 9000 §8.1) before the endpoint spends more on it.
//!
//! Congestion control and the post-session windows are knobs goal 5 sets within the bound.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use hyper_quic::{ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig, Event as QuicEvent, ReadError, ServerConfig, StreamEvent, StreamId, TransportConfig, VarInt};

use super::conn::{self, Admit, Connection, Event, Read, Transport};

/// What a connection may use, as transport parameters and buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The connection's receive window (MAX_DATA): stream bytes buffered at most, all streams.
    pub receive_window: u64,
    /// Peer bidirectional streams open at once: the CONNECT and its WebTransport streams.
    pub bidi_streams: u64,
    /// Peer unidirectional streams open at once: control, QPACK encoder and decoder (RFC 9114 §6.2
    /// asks at least these three), then WebTransport's.
    pub uni_streams: u64,
    /// Bytes of ours buffered to send, all streams.
    pub send_window: u64,
}

/// The door's configuration: per connection, before and after its session, and endpoint-wide.
#[derive(Clone, Debug)]
pub struct DoorConfig {
    /// A connection with no session yet.
    pub pre: Limits,
    /// Once its CONNECT is admitted.
    pub post: Limits,
    /// Each stream's receive window (fixed for a connection's life: it's the initial
    /// MAX_STREAM_DATA); the connection window bounds all streams together.
    pub stream_receive_window: u64,
    /// Received QUIC datagrams buffered at most (bytes).
    pub datagram_receive_buffer: usize,
    /// Datagrams to send buffered at most (bytes).
    pub datagram_send_buffer: usize,
    /// The TLS handshake's bytes buffered at most (CRYPTO frames).
    pub crypto_buffer: usize,
    /// QUIC's idle timeout (its own timer, RFC 9000 §10.1).
    pub idle_timeout: Duration,
    /// Connections at most.
    pub max_connections: usize,
    /// Connections with no session at most; past it the oldest is closed for the new one.
    pub max_pre_session: usize,
    /// Handshakes under way past which an unvalidated address is sent a Retry.
    pub retry_after: usize,
    /// The HTTP/3 core's limits.
    pub h3: conn::Config,
}

/// The fixed HTTP/3 state of a connection, at most (its tables of streams being classified and
/// held, its events, what it owes a stream): what [`Connection::held`] is checked against.
pub const H3_FIXED: usize = 4096;

impl DoorConfig {
    /// The door's defaults: B under 64 KiB before a session.
    pub fn door() -> DoorConfig {
        let mut h3 = conn::Config::door();
        let pre = Limits { receive_window: 16 << 10, bidi_streams: 1 + h3.pending_streams as u64, uni_streams: 3 + h3.pending_streams as u64, send_window: 4 << 10 };
        let post = Limits { receive_window: 1 << 20, bidi_streams: 1 + 64, uni_streams: 3 + 64, send_window: 1 << 20 };
        // Resets past twice the stream limit in a second end the connection (CVE-2023-44487).
        h3.resets_per_second = u32::try_from(2 * (post.bidi_streams + post.uni_streams)).unwrap_or(u32::MAX);
        DoorConfig {
            pre,
            post,
            stream_receive_window: 64 << 10,
            datagram_receive_buffer: 8 << 10,
            datagram_send_buffer: 64 << 10,
            crypto_buffer: 8 << 10,
            idle_timeout: Duration::from_secs(30),
            max_connections: 10_000,
            max_pre_session: 1_000,
            retry_after: 64,
            h3,
        }
    }

    /// B: the bytes a connection may hold beyond its idle state, before a session (`post` false)
    /// or after: its receive window, its send window, its datagram buffers (the send buffer only
    /// once it may send), its handshake buffer, the field section and its decode (twice the
    /// budget: the frame's payload and the decoder's scratch), and the HTTP/3 core's fixed state.
    pub fn bound(&self, post: bool) -> usize {
        let l = if post { self.post } else { self.pre };
        let datagrams = self.datagram_receive_buffer + if post { self.datagram_send_buffer } else { 0 };
        l.receive_window as usize + l.send_window as usize + datagrams + self.crypto_buffer + 2 * self.h3.budget.bytes + H3_FIXED
    }

    /// The transport configuration a connection starts with: every parameter that sizes memory
    /// set here, none left at hyper-quic's default.
    pub fn transport(&self) -> TransportConfig {
        let mut t = TransportConfig::default();
        t.max_concurrent_bidi_streams(varint(self.pre.bidi_streams))
            .max_concurrent_uni_streams(varint(self.pre.uni_streams))
            .receive_window(varint(self.pre.receive_window))
            .stream_receive_window(varint(self.stream_receive_window))
            .send_window(self.pre.send_window)
            .datagram_receive_buffer_size(Some(self.datagram_receive_buffer))
            .datagram_send_buffer_size(self.datagram_send_buffer)
            .crypto_buffer_size(self.crypto_buffer)
            .max_idle_timeout(hyper_quic::IdleTimeout::try_from(self.idle_timeout).ok())
            .keep_alive_interval(None);
        t
    }
}

fn varint(v: u64) -> VarInt {
    VarInt::from_u64(v).unwrap_or(VarInt::MAX)
}

/// The door's TLS for browsers: TLS 1.3 alone, ALPN `h3`, no client certificate, early data off
/// (goal 7 decides 0-RTT), session tickets on (one a handshake), on aws-lc-rs's default groups
/// (browsers offer X25519MLKEM768, X25519 and P-256; hyper-quic's node provider is post-quantum
/// only, and goal 7 owns the post-quantum setting).
pub fn tls(chain: Vec<hyper_quic::rustls::pki_types::CertificateDer<'static>>, key: hyper_quic::rustls::pki_types::PrivateKeyDer<'static>) -> Result<hyper_quic::rustls::ServerConfig, hyper_quic::rustls::Error> {
    use hyper_quic::rustls;
    let mut c = rustls::ServerConfig::builder_with_provider(&rustls::crypto::aws_lc_rs::DEFAULT_PROVIDER)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    c.alpn_protocols = vec![b"h3".to_vec()];
    c.max_early_data_size = 0;
    c.send_tls13_tickets = 1;
    c.ticketer = rustls::crypto::aws_lc_rs::Ticketer::new()?;
    Ok(c)
}

/// One connection of the door.
struct Conn {
    handle: ConnectionHandle,
    quic: hyper_quic::Connection,
    h3: Connection,
    /// When it arrived, by the door's count: the oldest pre-session connection goes first.
    born: u64,
    admitted: bool,
    /// Its soonest timer, as hyper-quic last said.
    deadline: Option<Instant>,
    /// Whether it's in the door's list of connections that may have something to send.
    dirty: bool,
}

/// Remote addresses the endpoint remembers (each of its two per-address memories), endpoint-wide.
pub const REMEMBERED_ADDRESSES: usize = 256;

/// A connection's id at the door.
pub type ConnId = usize;

/// The door: its endpoint and connections.
pub struct Door {
    cfg: DoorConfig,
    endpoint: Endpoint,
    conns: Vec<Option<Conn>>,
    born: u64,
    events: VecDeque<(ConnId, Event)>,
    /// Connections that may have something to send, in turn: only these are asked, so a send
    /// costs what moved, not every connection held.
    dirty: VecDeque<ConnId>,
    /// The first `now` the door saw: the reset rate's clock counts from it.
    epoch: Option<Instant>,
    /// Counted for tests and reports.
    pub stats: DoorStats,
}

/// What the door did, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DoorStats {
    pub accepted: u64,
    pub retried: u64,
    pub refused: u64,
    /// Pre-session connections closed for a newer one.
    pub evicted: u64,
    /// Datagrams dropped for want of a session.
    pub datagrams_dropped: u64,
}

/// Why a door couldn't be made.
#[derive(Debug)]
pub enum DoorError {
    Tls(hyper_quic::rustls::Error),
    Quic(String),
}

impl std::fmt::Display for DoorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DoorError::Tls(e) => write!(f, "the door's TLS: {e}"),
            DoorError::Quic(e) => write!(f, "the door's QUIC endpoint: {e}"),
        }
    }
}

impl std::error::Error for DoorError {}

impl From<hyper_quic::rustls::Error> for DoorError {
    fn from(e: hyper_quic::rustls::Error) -> Self {
        DoorError::Tls(e)
    }
}

/// A QUIC connection's streams and datagrams as the HTTP/3 core uses them.
struct Quic<'a> {
    c: &'a mut hyper_quic::Connection,
    now: Instant,
}

fn sid(id: u64) -> Option<StreamId> {
    VarInt::from_u64(id).ok().map(StreamId::from)
}

impl Transport for Quic<'_> {
    fn read(&mut self, id: u64, max: usize) -> Read {
        let Some(s) = sid(id) else { return Read::Gone };
        let mut recv = self.c.recv_stream(s);
        let Ok(mut chunks) = recv.read(true) else { return Read::Gone };
        let r = match chunks.next(max) {
            Ok(Some(c)) => Read::Data(c.bytes),
            Ok(None) => Read::Fin,
            Err(ReadError::Blocked) => Read::Blocked,
            Err(ReadError::Reset(code)) => Read::Reset(code.into_inner()),
        };
        let _ = chunks.finalize();
        r
    }
    fn write(&mut self, id: u64, data: &[u8]) -> Option<usize> {
        let s = sid(id)?;
        match self.c.send_stream(s).write(data) {
            Ok(n) => Some(n),
            Err(hyper_quic::WriteError::Blocked) => Some(0),
            Err(_) => None,
        }
    }
    fn finish(&mut self, id: u64) {
        if let Some(s) = sid(id) {
            let _ = self.c.send_stream(s).finish();
        }
    }
    fn reset(&mut self, id: u64, code: u64) {
        if let Some(s) = sid(id) {
            let _ = self.c.send_stream(s).reset(varint(code));
        }
    }
    fn stop(&mut self, id: u64, code: u64) {
        if let Some(s) = sid(id) {
            let _ = self.c.recv_stream(s).stop(varint(code));
        }
    }
    fn open(&mut self, bidi: bool) -> Option<u64> {
        self.c.streams().open(if bidi { Dir::Bi } else { Dir::Uni }).map(u64::from)
    }
    fn send_datagram(&mut self, data: Bytes) -> bool {
        self.c.datagrams().send(data, true).is_ok()
    }
    fn recv_datagram(&mut self) -> Option<Bytes> {
        self.c.datagrams().recv()
    }
    fn max_datagram(&self) -> Option<usize> {
        // hyper-quic reports it through a mutable borrow: [`Session::max_datagram`] asks it.
        None
    }
    fn close(&mut self, code: u64, reason: &[u8]) {
        self.c.close(self.now, varint(code), Bytes::copy_from_slice(reason));
    }
}

/// The session API goal 5 uses, on one admitted connection: its streams and datagrams, its close
/// and drain, without HTTP/3's frames.
pub struct Session<'a> {
    conn: &'a mut Conn,
    now: Instant,
}

impl Session<'_> {
    fn parts(&mut self) -> (Quic<'_>, &mut Connection) {
        (Quic { c: &mut self.conn.quic, now: self.now }, &mut self.conn.h3)
    }
    /// The session's CONNECT (path, origin, dialect).
    pub fn connect(&self) -> Option<&conn::Connect> {
        self.conn.h3.session()
    }
    /// Opens a stream of ours (its header goes first).
    pub fn open(&mut self, bidi: bool) -> Option<u64> {
        let (mut t, h) = self.parts();
        h.wt_open(&mut t, bidi)
    }
    pub fn read(&mut self, id: u64, max: usize) -> Read {
        let (mut t, h) = self.parts();
        h.wt_read(&mut t, id, max)
    }
    pub fn write(&mut self, id: u64, data: &[u8]) -> Option<usize> {
        let (mut t, h) = self.parts();
        h.wt_write(&mut t, id, data)
    }
    pub fn finish(&mut self, id: u64) {
        let (mut t, h) = self.parts();
        h.wt_finish(&mut t, id);
    }
    pub fn reset(&mut self, id: u64, app: u32) {
        let (mut t, h) = self.parts();
        h.wt_reset(&mut t, id, app);
    }
    pub fn stop(&mut self, id: u64, app: u32) {
        let (mut t, h) = self.parts();
        h.wt_stop(&mut t, id, app);
    }
    /// The application is done with a stream.
    pub fn forget(&mut self, id: u64) {
        self.conn.h3.wt_forget(id);
    }
    pub fn send_datagram(&mut self, payload: &[u8]) -> bool {
        let (mut t, h) = self.parts();
        h.send_datagram(&mut t, payload)
    }
    pub fn recv_datagram(&mut self) -> Option<Bytes> {
        let (mut t, h) = self.parts();
        h.recv_datagram(&mut t)
    }
    /// The largest datagram payload the session can send now.
    pub fn max_datagram(&mut self) -> Option<usize> {
        let q = self.conn.h3.session()?.stream / 4;
        self.conn.quic.datagrams().max_size().and_then(|m| m.checked_sub(super::varint::len(q)))
    }
    pub fn close(&mut self, app: u32, message: &[u8]) {
        let (mut t, h) = self.parts();
        h.close(&mut t, app, message);
    }
    pub fn drain(&mut self, goaway: bool) {
        let (mut t, h) = self.parts();
        h.drain(&mut t, goaway);
    }
    /// The QUIC connection, for goal 5's knobs (congestion control, windows within the bound).
    pub fn quic(&mut self) -> &mut hyper_quic::Connection {
        &mut self.conn.quic
    }
}

impl Door {
    /// A door serving `tls` under `cfg`; `seed` seeds the endpoint's randomness (connection ids,
    /// packet-number skips) where a test wants it fixed.
    pub fn new(cfg: DoorConfig, tls: hyper_quic::rustls::ServerConfig, seed: Option<[u8; 32]>) -> Result<Door, DoorError> {
        let server = Self::server_config(&cfg, tls)?;
        // The endpoint's memories of remote addresses (loss bursts, Careful Resume's path
        // measurements) are the endpoint's to bound, not a connection's: set here, not left.
        let mut ec = EndpointConfig::default();
        let mut resume = hyper_quic::CarefulResumeConfig::default();
        resume.remembered(REMEMBERED_ADDRESSES);
        ec.careful_resume(Some(resume)).loss_memory(REMEMBERED_ADDRESSES, Duration::from_secs(3600));
        let endpoint = Endpoint::new(ec, Some(server), true, seed).map_err(|e| DoorError::Quic(format!("{e:?}")))?;
        Ok(Door { cfg, endpoint, conns: Vec::new(), born: 0, events: VecDeque::new(), dirty: VecDeque::new(), epoch: None, stats: DoorStats::default() })
    }

    fn server_config(cfg: &DoorConfig, tls: hyper_quic::rustls::ServerConfig) -> Result<ServerConfig, DoorError> {
        let crypto = hyper_quic::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|e| DoorError::Quic(format!("{e:?}")))?;
        let mut s = ServerConfig::with_crypto(Box::new(crypto));
        s.transport_config(cfg.transport());
        s.migration(false);
        s.max_incoming(cfg.max_pre_session.max(1));
        // Early data is off: an attempt buffers nothing before it's accepted, which is at once.
        s.incoming_buffer_size(16 << 10);
        s.incoming_buffer_size_total((cfg.max_pre_session as u64).saturating_mul(16 << 10).min(64 << 20));
        Ok(s)
    }

    /// Serves `tls` to connections from now on (a rotated certificate, step 6); connections up
    /// keep theirs.
    pub fn set_tls(&mut self, tls: hyper_quic::rustls::ServerConfig) -> Result<(), DoorError> {
        let server = Self::server_config(&self.cfg, tls)?;
        self.endpoint.set_server_config(Some(server)).map_err(|e| DoorError::Quic(format!("{e:?}")))
    }

    pub fn config(&self) -> &DoorConfig {
        &self.cfg
    }

    /// Connections open, and how many of them have no session.
    pub fn counts(&self) -> (usize, usize) {
        let open = self.conns.iter().flatten().count();
        let pre = self.conns.iter().flatten().filter(|c| !c.admitted).count();
        (open, pre)
    }

    /// The ids of the connections open.
    pub fn ids(&self) -> Vec<ConnId> {
        self.conns.iter().enumerate().filter_map(|(i, c)| c.as_ref().map(|_| i)).collect()
    }

    /// The HTTP/3 core of a connection (what it holds, its stats).
    pub fn h3(&self, id: ConnId) -> Option<&Connection> {
        self.conns.get(id)?.as_ref().map(|c| &c.h3)
    }

    /// Whether connection `id` has its session.
    pub fn admitted(&self, id: ConnId) -> bool {
        self.conns.get(id).and_then(|c| c.as_ref()).is_some_and(|c| c.admitted)
    }

    /// The QUIC connection of `id` (its stats).
    pub fn quic(&self, id: ConnId) -> Option<&hyper_quic::Connection> {
        self.conns.get(id)?.as_ref().map(|c| &c.quic)
    }

    /// The admitted session of `id`.
    pub fn session(&mut self, id: ConnId, now: Instant) -> Option<Session<'_>> {
        // What the application does on it may give it something to send.
        self.mark(id);
        let conn = self.conns.get_mut(id)?.as_mut().filter(|c| c.admitted)?;
        Some(Session { conn, now })
    }

    /// The next event, and its connection.
    pub fn poll_event(&mut self) -> Option<(ConnId, Event)> {
        self.events.pop_front()
    }

    /// A datagram from `from`. A reply the endpoint makes itself (a Retry, a version
    /// negotiation, a refusal) is written to `buf` and its transmit returned.
    pub fn handle(&mut self, now: Instant, from: SocketAddr, data: BytesMut, buf: &mut Vec<u8>, admit: &mut dyn Admit) -> Option<hyper_quic::Transmit> {
        self.epoch.get_or_insert(now);
        buf.clear();
        match self.endpoint.handle(now, from, None, None, data, buf)? {
            DatagramEvent::ConnectionEvent(h, ev) => {
                let i = h.0;
                if let Some(Some(c)) = self.conns.get_mut(i) {
                    c.quic.handle_event(ev, self.endpoint.configs_mut());
                }
                self.after(i, now, admit);
                None
            }
            DatagramEvent::Response(t) => Some(t),
            DatagramEvent::NewConnection(incoming) => self.incoming(incoming, now, buf, admit),
        }
    }

    fn incoming(&mut self, incoming: hyper_quic::Incoming, now: Instant, buf: &mut Vec<u8>, admit: &mut dyn Admit) -> Option<hyper_quic::Transmit> {
        let (open, pre) = self.counts();
        let handshaking = self.conns.iter().flatten().filter(|c| c.quic.is_handshaking()).count();
        if !incoming.remote_address_validated() && handshaking >= self.cfg.retry_after && incoming.may_retry() {
            self.stats.retried += 1;
            return self.endpoint.retry(incoming, buf).ok();
        }
        if open >= self.cfg.max_connections && pre == 0 {
            self.stats.refused += 1;
            return self.endpoint.refuse(incoming, buf);
        }
        if pre >= self.cfg.max_pre_session || open >= self.cfg.max_connections {
            // The oldest connection with no session makes room (never an admitted one).
            self.evict_oldest_pre(now);
        }
        match self.endpoint.accept(incoming, now, buf, None, None) {
            Ok((handle, quic)) => {
                let i = handle.0;
                if self.conns.len() <= i {
                    self.conns.resize_with(i + 1, || None);
                }
                self.born += 1;
                let h3 = Connection::new(self.cfg.h3.clone());
                if let Some(slot) = self.conns.get_mut(i) {
                    *slot = Some(Conn { handle, quic, h3, born: self.born, admitted: false, deadline: None, dirty: false });
                }
                self.stats.accepted += 1;
                self.after(i, now, admit);
                None
            }
            Err(e) => e.response,
        }
    }

    fn evict_oldest_pre(&mut self, now: Instant) {
        let oldest = self.conns.iter().enumerate().filter_map(|(i, c)| c.as_ref().filter(|c| !c.admitted && !c.quic.is_closed()).map(|c| (c.born, i))).min();
        if let Some((_, i)) = oldest
            && let Some(Some(c)) = self.conns.get_mut(i)
        {
            c.quic.close(now, varint(super::code::H3_EXCESSIVE_LOAD), Bytes::from_static(b"too many connections without a session"));
            self.stats.evicted += 1;
            let mut refuse = |_: &conn::Connect| conn::Admission::Forbidden;
            self.after(i, now, &mut refuse);
        }
    }

    /// The reset rate's clock: nanoseconds since the first `now` the door saw.
    fn ns(&self, now: Instant) -> u64 {
        self.epoch.map_or(0, |e| u64::try_from(now.saturating_duration_since(e).as_nanos()).unwrap_or(u64::MAX))
    }

    /// Carries a connection's events through its HTTP/3 core and the endpoint.
    fn after(&mut self, i: ConnId, now: Instant, admit: &mut dyn Admit) {
        let ns = self.ns(now);
        let post = self.cfg.post;
        let Some(Some(c)) = self.conns.get_mut(i) else { return };
        while let Some(ev) = c.quic.poll() {
            let mut t = Quic { c: &mut c.quic, now };
            match ev {
                QuicEvent::Connected => c.h3.start(&mut t),
                QuicEvent::Stream(StreamEvent::Opened { dir }) => {
                    while let Some(s) = t.c.streams().accept(dir) {
                        readable(&mut t, &mut c.h3, admit, u64::from(s), ns);
                    }
                }
                QuicEvent::Stream(StreamEvent::Readable { id }) => readable(&mut t, &mut c.h3, admit, u64::from(id), ns),
                QuicEvent::Stream(StreamEvent::Writable { id }) => c.h3.writable(&mut t, u64::from(id)),
                QuicEvent::Stream(StreamEvent::Stopped { id, error_code }) => c.h3.stopped_by_peer(&mut t, u64::from(id), error_code.into_inner()),
                QuicEvent::DatagramReceived => {
                    if c.h3.session().is_none() {
                        // No session: nothing waits for one.
                        while t.c.datagrams().recv().is_some() {
                            self.stats.datagrams_dropped += 1;
                        }
                    } else {
                        self.events.push_back((i, Event::Datagram));
                    }
                }
                _ => {}
            }
            while let Some(e) = c.h3.poll_event() {
                if let Event::Accepted(_) = &e
                    && !c.admitted
                {
                    // Admitted: the windows and stream counts a session gets.
                    c.admitted = true;
                    c.quic.set_receive_window(varint(post.receive_window));
                    c.quic.set_max_concurrent_streams(Dir::Bi, varint(post.bidi_streams));
                    c.quic.set_max_concurrent_streams(Dir::Uni, varint(post.uni_streams));
                    c.quic.set_send_window(post.send_window);
                }
                self.events.push_back((i, e));
            }
        }
        while let Some(e) = c.h3.poll_event() {
            self.events.push_back((i, e));
        }
        while let Some(ev) = c.quic.poll_endpoint_events() {
            if let Some(back) = self.endpoint.handle_event(c.handle, ev) {
                c.quic.handle_event(back, self.endpoint.configs_mut());
            }
        }
        if c.quic.is_drained() {
            if let Some(slot) = self.conns.get_mut(i) {
                *slot = None;
            }
            self.events.push_back((i, Event::ConnectionClosed { code: 0 }));
            return;
        }
        c.deadline = c.quic.poll_timeout();
        if !c.dirty {
            c.dirty = true;
            self.dirty.push_back(i);
        }
    }

    /// Connection `i` may have something to send.
    fn mark(&mut self, i: ConnId) {
        if let Some(Some(c)) = self.conns.get_mut(i)
            && !c.dirty
        {
            c.dirty = true;
            self.dirty.push_back(i);
        }
    }

    /// The next datagrams to send (up to `max_datagrams` of one connection's, segmented as
    /// hyper-quic makes them): the transmit, its bytes in `buf`. Connections take turns.
    pub fn poll_transmit(&mut self, now: Instant, max_datagrams: usize, buf: &mut Vec<u8>) -> Option<hyper_quic::Transmit> {
        while let Some(i) = self.dirty.pop_front() {
            let Some(Some(c)) = self.conns.get_mut(i) else { continue };
            buf.clear();
            let t = c.quic.poll_transmit(now, max_datagrams, buf, self.endpoint.configs());
            c.deadline = c.quic.poll_timeout();
            match t {
                Some(t) => {
                    self.dirty.push_back(i);
                    return Some(t);
                }
                None => c.dirty = false,
            }
        }
        None
    }

    /// The soonest timer of any connection.
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        self.conns.iter().flatten().filter_map(|c| c.deadline).min()
    }

    /// Fires the timers due at `now`.
    pub fn handle_timeout(&mut self, now: Instant, admit: &mut dyn Admit) {
        for i in 0..self.conns.len() {
            let due = self.conns.get(i).and_then(|c| c.as_ref()).and_then(|c| c.deadline).is_some_and(|t| t <= now);
            if due {
                if let Some(Some(c)) = self.conns.get_mut(i) {
                    c.quic.handle_timeout(now);
                }
                self.after(i, now, admit);
            }
        }
    }

    /// Carries what the application did on connection `id` (its writes, its close) on.
    pub fn flush(&mut self, id: ConnId, now: Instant, admit: &mut dyn Admit) {
        self.after(id, now, admit);
    }
}

/// A peer stream is readable: a reset is told as one (the reset rate counts it), else the core
/// reads it.
fn readable(t: &mut Quic<'_>, h3: &mut Connection, admit: &mut dyn Admit, id: u64, ns: u64) {
    let reset = sid(id).and_then(|s| t.c.recv_stream(s).received_reset().ok().flatten());
    match reset {
        Some(code) => h3.reset_by_peer(t, admit, id, code.into_inner(), ns),
        None => h3.readable(t, admit, id),
    }
}
