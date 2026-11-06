//! One HTTP/3 connection that admits one WebTransport CONNECT and nothing else (goal 4 steps 3
//! and 4, plan 01a1a832eb597110a6dcfda227981604), sans-IO.
//!
//! The caller owns the QUIC connection and hands this its streams through [`Transport`]: it says
//! which peer streams can be read ([`Connection::readable`]), which were reset or stopped, and this
//! reads them, writes, resets and stops as HTTP/3 says. It holds no clock (the reset rate takes
//! `now` from the caller) and no buffer a peer sizes: frames it doesn't keep are skipped by their
//! length, the one field section it decodes is bounded by the budget before it's read, streams
//! that wait for their session are held unread, and a datagram is read only when the session asks.
//!
//! What it admits (RFC 9114, RFC 9220, draft-ietf-webtrans-http3-02 and -14):
//!
//! * It sends one control stream whose SETTINGS carry every WebTransport dialect ([`Settings::door`]).
//! * It reads no request until the client's SETTINGS have arrived (draft-14 §3.1), and takes the
//!   dialect from them ([`Draft`]).
//! * One request is decoded at a time; it must be CONNECT with `:protocol webtransport`, `:scheme
//!   https`, an `:authority` and a `:path`. Only such a request reaches the caller's [`Admit`],
//!   which answers 200, 403 or 404. A request with another method is reset with
//!   H3_REQUEST_REJECTED, another `:protocol` gets 501, a malformed one H3_MESSAGE_ERROR, and
//!   every request once a session is admitted is reset unread (one session a connection, no
//!   WebTransport flow control: draft-14 §5.1).
//! * A push stream from the client, a second control, encoder or decoder stream, a critical
//!   stream closed, a frame where it can't be, a reserved HTTP/2 frame or setting: connection
//!   errors. Unknown stream types are stopped and forgotten; unknown frames and settings skipped.
//! * Peer resets past twice the stream limit in a second end the connection with
//!   H3_EXCESSIVE_LOAD, as kiln-http's HTTP/2 server meets Rapid Reset (CVE-2023-44487).
//!
//! The admitted session ([`Event::Accepted`]) is the API goal 5 builds on: its streams, opened by
//! either side, read and written through [`Connection::wt_read`] and [`Connection::wt_write`],
//! datagrams through [`Connection::send_datagram`] and [`Connection::recv_datagram`], and
//! [`Connection::close`] and [`Connection::drain`]. A close never waits on the peer's credit
//! (webtransport-go waited, CVE-2026-21435): the WT_CLOSE_SESSION capsule goes out in the same call
//! if the credit already granted covers it, and otherwise the CONNECT stream is reset at once.

use std::collections::VecDeque;

use bytes::{BufMut as _, Bytes, BytesMut};

use super::capsule::{self, Capsule, Capsules};
use super::qpack::{self, Budget, Refused};
use super::varint::{self, Partial};
use super::{Draft, Settings, code, frame, put_frame, setting, settings_payload, stream_type};

// ------------------------------------------------------------------------------- the seams

/// What a read of a stream found.
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
    /// Bytes, at most as many as asked for.
    Data(Bytes),
    /// The stream ended (its FIN).
    Fin,
    /// Nothing more now.
    Blocked,
    /// The peer reset it, with this code.
    Reset(u64),
    /// There is no such stream to read (it was never opened, or is done).
    Gone,
}

/// The QUIC connection's streams and datagrams, as the HTTP/3 core reads and writes them. Stream
/// ids are QUIC's (RFC 9000 §2.1). Every call returns at once.
pub trait Transport {
    /// Reads at most `max` bytes of stream `id`, in order.
    fn read(&mut self, id: u64, max: usize) -> Read;
    /// Writes what the stream's credit and buffer allow of `data`: the bytes taken (0 when
    /// blocked); `None` when the stream can't be written (stopped, reset, unknown).
    fn write(&mut self, id: u64, data: &[u8]) -> Option<usize>;
    /// Ends our side of `id` after what was written.
    fn finish(&mut self, id: u64);
    /// Resets our side of `id` (RESET_STREAM).
    fn reset(&mut self, id: u64, code: u64);
    /// Asks the peer to stop sending on `id` (STOP_SENDING).
    fn stop(&mut self, id: u64, code: u64);
    /// Opens a stream of ours: unidirectional, or bidirectional. `None` when the peer's limit
    /// allows no more now.
    fn open(&mut self, bidi: bool) -> Option<u64>;
    /// Sends a QUIC datagram; false when it can't (too large, unsupported, no room).
    fn send_datagram(&mut self, data: Bytes) -> bool;
    /// The next QUIC datagram received, if any.
    fn recv_datagram(&mut self) -> Option<Bytes>;
    /// The largest datagram the peer takes, if it takes datagrams.
    fn max_datagram(&self) -> Option<usize>;
    /// Closes the connection with an HTTP/3 error code.
    fn close(&mut self, code: u64, reason: &[u8]);
}

/// A WebTransport CONNECT, as the caller decides on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    /// The CONNECT stream's id: the session's id.
    pub stream: u64,
    pub authority: Vec<u8>,
    pub path: Vec<u8>,
    /// Its `origin` header (a browser always sends one: draft-14 §3.2).
    pub origin: Option<Vec<u8>>,
    /// The dialect the connection speaks.
    pub draft: Draft,
}

/// The answer to a CONNECT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// 200: the session is up.
    Accept,
    /// 403: the origin, or the ticket, isn't allowed.
    Forbidden,
    /// 404: nothing is served at this path.
    NotFound,
}

/// Decides on CONNECTs: the only requests that reach the caller.
pub trait Admit {
    fn admit(&mut self, connect: &Connect) -> Admission;
}

impl<F: FnMut(&Connect) -> Admission> Admit for F {
    fn admit(&mut self, connect: &Connect) -> Admission {
        self(connect)
    }
}

/// What the connection tells its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A session was admitted (200 sent).
    Accepted(Connect),
    /// A CONNECT was refused with this status.
    Refused { stream: u64, status: u16 },
    /// The peer opened a WebTransport stream of the session; its header is read.
    Stream { id: u64, bidi: bool },
    /// A session stream can be read.
    Readable(u64),
    /// A session stream of ours can be written again.
    Writable(u64),
    /// A session stream was reset by the peer, with its WebTransport code if it carries one.
    StreamReset { id: u64, code: Option<u32> },
    /// The peer asked us to stop sending on a session stream.
    StreamStopped { id: u64, code: Option<u32> },
    /// The peer asked for the session to end (WT_DRAIN_SESSION).
    Draining,
    /// Datagrams arrived for the session: read them with `recv_datagram`.
    Datagram,
    /// The session ended: by the peer's WT_CLOSE_SESSION (its code and message), its CONNECT
    /// stream ending or reset (code 0), or our close.
    Closed { code: u32, message: Vec<u8> },
    /// The connection ended with this HTTP/3 error code.
    ConnectionClosed { code: u64 },
}

/// A connection's limits.
#[derive(Clone, Debug)]
pub struct Config {
    /// The field-section budget, advertised and enforced.
    pub budget: Budget,
    /// WebTransport streams held unread before their session is admitted; past it they're
    /// refused with WT_BUFFERED_STREAM_REJECTED (draft-14 §4.6).
    pub pending_streams: usize,
    /// Peer streams whose first bytes (their type, or their first frame) are being read at once.
    pub classifying: usize,
    /// Session streams the peer may have open at once beyond what QUIC limits (a second bound).
    pub session_streams: usize,
    /// The largest DATAGRAM capsule kept.
    pub datagram_max: usize,
    /// Resets a second past which the connection ends (twice the stream limit).
    pub resets_per_second: u32,
}

impl Config {
    /// The door's.
    pub fn door() -> Config {
        Config { budget: super::DOOR_BUDGET, pending_streams: 4, classifying: 16, session_streams: 256, datagram_max: 1200, resets_per_second: 2 * 64 }
    }
}

// ------------------------------------------------------------------------------- state

/// A frame header read in pieces.
#[derive(Clone, Copy, Debug, Default)]
struct FrameHead {
    kind: Option<u64>,
    p: Partial,
}

impl FrameHead {
    /// Takes from `data` until a frame's type and length are whole: them and the bytes taken.
    fn take(&mut self, data: &[u8]) -> (Option<(u64, u64)>, usize) {
        let mut used = 0;
        loop {
            let (v, n) = self.p.take(data.get(used..).unwrap_or_default());
            used += n;
            match (v, self.kind) {
                (None, _) => return (None, used),
                (Some(k), None) => self.kind = Some(k),
                (Some(len), Some(k)) => {
                    *self = FrameHead::default();
                    return (Some((k, len)), used);
                }
            }
        }
    }
}

/// Where the peer's control stream is.
#[derive(Debug)]
enum Ctl {
    /// Before its first frame (which must be SETTINGS).
    First(FrameHead),
    /// In its SETTINGS, this many bytes left; `id` is a setting's id read, its value to come.
    Settings { left: u64, p: Partial, id: Option<u64> },
    /// Between frames.
    Head(FrameHead),
    /// Skipping this many bytes of a frame.
    Skip(u64),
    /// Reading a frame whose payload is one integer (GOAWAY, MAX_PUSH_ID, CANCEL_PUSH).
    One { left: u64, p: Partial },
}

/// The client's settings, as far as the door reads them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerSettings {
    pub enable_webtransport: Option<u64>,
    pub wt_max_sessions_draft07: Option<u64>,
    pub wt_max_sessions: Option<u64>,
    pub wt_enabled_draft15: Option<u64>,
    pub h3_datagram: Option<u64>,
    pub h3_datagram_draft04: Option<u64>,
    pub max_field_section_size: Option<u64>,
    pub qpack_max_table_capacity: Option<u64>,
    pub qpack_blocked_streams: Option<u64>,
    pub enable_connect_protocol: Option<u64>,
}

impl PeerSettings {
    /// Records one setting; `Err` for a duplicate of one it reads, or a reserved HTTP/2 id.
    fn set(&mut self, id: u64, v: u64) -> Result<(), u64> {
        if setting::reserved_h2(id) {
            return Err(code::H3_SETTINGS_ERROR);
        }
        let slot = match id {
            setting::ENABLE_WEBTRANSPORT => &mut self.enable_webtransport,
            setting::WEBTRANSPORT_MAX_SESSIONS_DRAFT07 => &mut self.wt_max_sessions_draft07,
            setting::WT_MAX_SESSIONS => &mut self.wt_max_sessions,
            setting::WT_ENABLED_DRAFT15 => &mut self.wt_enabled_draft15,
            setting::H3_DATAGRAM => &mut self.h3_datagram,
            setting::H3_DATAGRAM_DRAFT04 => &mut self.h3_datagram_draft04,
            setting::MAX_FIELD_SECTION_SIZE => &mut self.max_field_section_size,
            setting::QPACK_MAX_TABLE_CAPACITY => &mut self.qpack_max_table_capacity,
            setting::QPACK_BLOCKED_STREAMS => &mut self.qpack_blocked_streams,
            setting::ENABLE_CONNECT_PROTOCOL => &mut self.enable_connect_protocol,
            // Unknown settings are ignored (RFC 9114 §7.2.4), GREASE included.
            _ => return Ok(()),
        };
        if slot.is_some() {
            return Err(code::H3_SETTINGS_ERROR);
        }
        *slot = Some(v);
        Ok(())
    }

    /// The WebTransport dialect these settings speak: the latest both sides do.
    pub fn draft(&self) -> Option<Draft> {
        let datagrams = self.h3_datagram == Some(1) || self.h3_datagram_draft04 == Some(1);
        if !datagrams {
            return None;
        }
        if self.wt_max_sessions.is_some_and(|v| v > 0) {
            Some(Draft::Draft14)
        } else if self.wt_max_sessions_draft07.is_some_and(|v| v > 0) {
            Some(Draft::Draft07)
        } else if self.enable_webtransport == Some(1) {
            Some(Draft::Draft02)
        } else {
            None
        }
    }
}

/// A peer stream whose first bytes are being read.
#[derive(Clone, Copy, Debug)]
struct Classify {
    id: u64,
    /// Bidirectional (a request, or a WebTransport stream).
    bidi: bool,
    /// Its type (uni) or first frame type / signal (bidi), as it arrives.
    head: Partial,
    /// For a WebTransport stream, its session id is being read.
    wt: Option<Partial>,
}

/// The request being decoded.
#[derive(Debug)]
struct Request {
    id: u64,
    phase: Phase,
}

/// Where a request stream is, before its HEADERS are whole.
#[derive(Debug)]
enum Phase {
    /// Reading a frame's type and length.
    Head(FrameHead),
    /// Skipping this many bytes of a frame that isn't kept (an unknown one, GREASE).
    Skip(u64),
    /// Reading the HEADERS payload: `need` bytes in all, at most the budget.
    Headers { need: usize, bytes: Vec<u8> },
}

/// A WebTransport stream waiting, unread, for its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    id: u64,
    bidi: bool,
    session: u64,
}

/// A stream of the session.
#[derive(Clone, Copy, Debug)]
struct WtStream {
    id: u64,
    /// Our header (signal or type, and session id) still to write, at `header[at..len]`.
    header: [u8; 16],
    at: u8,
    len: u8,
}

/// The admitted session.
#[derive(Debug)]
struct Session {
    connect: Connect,
    /// The CONNECT stream's frames after its HEADERS.
    head: FrameHead,
    /// Bytes of the DATA frame (capsules) or of a skipped frame still to come.
    data_left: u64,
    skipping: bool,
    capsules: Capsules,
    /// Session streams, peer's and ours.
    streams: Vec<WtStream>,
    /// DATAGRAM capsules waiting for the application (a few, each within the bound).
    capsule_datagrams: VecDeque<Bytes>,
    /// Our side of the CONNECT stream is done (closed or reset).
    ended: bool,
    /// The peer's side is done.
    peer_ended: bool,
}

/// Bytes still to go out on one of our streams before anything else on it (the control stream's
/// SETTINGS, a response, a capsule): a few, written as credit comes.
#[derive(Debug)]
struct Owed {
    id: u64,
    bytes: Vec<u8>,
    fin: bool,
}

/// One HTTP/3 connection's state.
#[derive(Debug)]
pub struct Connection {
    cfg: Config,
    settings: Settings,
    closed: Option<u64>,
    control: Option<u64>,
    peer_control: Option<(u64, Ctl)>,
    peer_encoder: Option<u64>,
    peer_decoder: Option<(u64, qpack::DecoderStream)>,
    peer: Option<PeerSettings>,
    classifying: Vec<Classify>,
    /// Client bidi streams readable before the client's SETTINGS arrived: read once they have.
    waiting: Vec<u64>,
    request: Option<Request>,
    session: Option<Session>,
    /// The latest request stream seen: a stream naming a session at or before it that isn't the
    /// live one (or the one being decided) names a session that's gone.
    highest_request: Option<u64>,
    pending: Vec<Pending>,
    owed: Vec<Owed>,
    events: VecDeque<Event>,
    /// The reset rate's window: when it began (ns) and the resets in it.
    resets: (u64, u32),
    goaway: bool,
    /// Counted for tests and reports.
    pub stats: Stats,
}

/// What a connection did, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// CONNECTs handed to the caller's [`Admit`].
    pub admitted_asked: u64,
    /// Requests refused before they reached [`Admit`].
    pub refused: u64,
    /// WebTransport streams refused for want of a session.
    pub buffered_rejected: u64,
    /// Datagrams dropped (no session, another session, malformed).
    pub datagrams_dropped: u64,
}

/// The largest read of a stream's first bytes: one variable-length integer.
const HEAD_READ: usize = 8;


impl Connection {
    /// A connection with `cfg`'s limits, advertising the door's settings for its budget.
    pub fn new(cfg: Config) -> Connection {
        let settings = Settings::with_budget(cfg.budget);
        Connection {
            cfg,
            settings,
            closed: None,
            control: None,
            peer_control: None,
            peer_encoder: None,
            peer_decoder: None,
            peer: None,
            classifying: Vec::new(),
            waiting: Vec::new(),
            request: None,
            session: None,
            highest_request: None,
            pending: Vec::new(),
            owed: Vec::new(),
            events: VecDeque::new(),
            resets: (0, 0),
            goaway: false,
            stats: Stats::default(),
        }
    }

    /// The settings this connection sends.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The client's settings, once they've arrived.
    pub fn peer_settings(&self) -> Option<&PeerSettings> {
        self.peer.as_ref()
    }

    /// The admitted session's CONNECT, if there is one.
    pub fn session(&self) -> Option<&Connect> {
        self.session.as_ref().map(|s| &s.connect)
    }

    /// Whether the connection has ended, and with what code.
    pub fn closed(&self) -> Option<u64> {
        self.closed
    }

    /// The next event for the caller.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// A rough count of the heap bytes this connection holds (for its memory bound's checks).
    pub fn held(&self) -> usize {
        let request = self.request.as_ref().map_or(0, |r| size_of::<Request>() + if let Phase::Headers { bytes, .. } = &r.phase { bytes.capacity() } else { 0 });
        let session = self.session.as_ref().map_or(0, |s| {
            size_of::<Session>()
                + s.connect.path.capacity()
                + s.connect.authority.capacity()
                + s.connect.origin.as_ref().map_or(0, Vec::capacity)
                + s.streams.capacity() * size_of::<WtStream>()
                + s.capsules.held()
                + s.capsule_datagrams.iter().map(Bytes::len).sum::<usize>()
        });
        request
            + session
            + self.classifying.capacity() * size_of::<Classify>()
            + self.waiting.capacity() * 8
            + self.pending.capacity() * size_of::<Pending>()
            + self.owed.iter().map(|o| o.bytes.capacity() + size_of::<Owed>()).sum::<usize>()
            + self.events.capacity() * size_of::<Event>()
    }

    /// Opens the control stream and sends SETTINGS (RFC 9114 §6.2.1): the first thing the server
    /// does, needing nothing from the peer.
    pub fn start(&mut self, t: &mut impl Transport) {
        if self.control.is_some() || self.closed.is_some() {
            return;
        }
        let Some(id) = t.open(false) else {
            return self.fail(t, code::H3_STREAM_CREATION_ERROR, b"no room for the control stream");
        };
        self.control = Some(id);
        let mut out = Vec::with_capacity(64);
        varint::put(&mut out, stream_type::CONTROL);
        put_frame(&mut out, frame::SETTINGS, &settings_payload(&self.settings));
        self.send(t, id, out, false);
    }

    /// Ends the connection with `code`.
    fn fail(&mut self, t: &mut impl Transport, code: u64, reason: &[u8]) {
        if self.closed.is_some() {
            return;
        }
        self.closed = Some(code);
        t.close(code, reason);
        self.request = None;
        self.pending.clear();
        self.classifying.clear();
        self.waiting.clear();
        self.owed.clear();
        if let Some(s) = self.session.take()
            && !s.ended
        {
            self.events.push_back(Event::Closed { code: 0, message: Vec::new() });
        }
        self.events.push_back(Event::ConnectionClosed { code });
    }

    /// Writes `bytes` on `id`, owing what the credit doesn't cover yet.
    fn send(&mut self, t: &mut impl Transport, id: u64, bytes: Vec<u8>, fin: bool) {
        if let Some(o) = self.owed.iter_mut().find(|o| o.id == id) {
            o.bytes.extend_from_slice(&bytes);
            o.fin |= fin;
            return;
        }
        match t.write(id, &bytes) {
            Some(n) if n == bytes.len() => {
                if fin {
                    t.finish(id);
                }
            }
            Some(n) => self.owed.push(Owed { id, bytes: bytes.get(n..).unwrap_or_default().to_vec(), fin }),
            None => {}
        }
    }

    /// A stream of ours can take more: what's owed on it goes first, then the session's writer
    /// hears of it.
    pub fn writable(&mut self, t: &mut impl Transport, id: u64) {
        if let Some(i) = self.owed.iter().position(|o| o.id == id) {
            let Some(o) = self.owed.get_mut(i) else { return };
            match t.write(id, &o.bytes) {
                Some(n) if n == o.bytes.len() => {
                    let fin = o.fin;
                    self.owed.swap_remove(i);
                    if fin {
                        t.finish(id);
                    }
                }
                Some(n) => {
                    o.bytes.drain(..n);
                    return;
                }
                None => {
                    self.owed.swap_remove(i);
                    return;
                }
            }
        }
        if let Some(s) = &mut self.session
            && let Some(w) = s.streams.iter_mut().find(|w| w.id == id)
        {
            if !flush_header(t, w) {
                return;
            }
            self.events.push_back(Event::Writable(id));
        }
    }

    // ------------------------------------------------------------------- peer streams

    /// A peer stream can be read (new or not): `now` is the caller's clock, for the reset rate.
    pub fn readable(&mut self, t: &mut impl Transport, admit: &mut dyn Admit, id: u64) {
        if self.closed.is_some() {
            return;
        }
        // A session stream: the application reads it.
        if let Some(s) = &self.session
            && s.streams.iter().any(|w| w.id == id)
        {
            self.events.push_back(Event::Readable(id));
            return;
        }
        if self.session.as_ref().is_some_and(|s| s.connect.stream == id) {
            return self.read_connect(t);
        }
        if self.request.as_ref().is_some_and(|r| r.id == id) {
            return self.read_request(t, admit);
        }
        if self.peer_control.as_ref().is_some_and(|(c, _)| *c == id) {
            return self.read_control(t, admit);
        }
        if self.peer_encoder == Some(id) {
            return self.read_encoder(t);
        }
        if self.peer_decoder.as_ref().is_some_and(|(d, _)| *d == id) {
            return self.read_decoder(t);
        }
        if self.pending.iter().any(|p| p.id == id) || self.waiting.contains(&id) {
            // Held unread until its session (or the client's SETTINGS) comes.
            return;
        }
        // Server-initiated streams are ours: a readable one is a session stream we no longer track.
        if id & 1 == 1 {
            return;
        }
        let bidi = id & 2 == 0;
        if bidi && self.peer.is_none() {
            // No request is read before the client's SETTINGS (draft-14 §3.1).
            if self.waiting.len() >= self.cfg.classifying {
                return self.refuse_stream(t, id, true, code::H3_REQUEST_REJECTED);
            }
            self.waiting.push(id);
            return;
        }
        let i = match self.classifying.iter().position(|c| c.id == id) {
            Some(i) => i,
            None => {
                if self.classifying.len() >= self.cfg.classifying {
                    return self.refuse_stream(t, id, bidi, code::H3_EXCESSIVE_LOAD);
                }
                self.classifying.push(Classify { id, bidi, head: Partial::default(), wt: None });
                self.classifying.len() - 1
            }
        };
        self.classify(t, admit, i);
    }

    /// Reads a peer stream's first bytes, a byte or an integer at a time, until it knows what the
    /// stream is.
    fn classify(&mut self, t: &mut impl Transport, admit: &mut dyn Admit, i: usize) {
        loop {
            let Some(c) = self.classifying.get(i).copied() else { return };
            let p = c.wt.unwrap_or(c.head);
            let data = match t.read(c.id, p.left()) {
                Read::Data(b) => b,
                Read::Blocked => return,
                Read::Fin | Read::Reset(_) | Read::Gone => {
                    self.classifying.swap_remove(i);
                    return;
                }
            };
            let mut p = p;
            let (v, _) = p.take(&data);
            let Some(c) = self.classifying.get_mut(i) else { return };
            if c.wt.is_some() {
                c.wt = Some(p);
            } else {
                c.head = p;
            }
            let Some(v) = v else { continue };
            let c = *c;
            if c.wt.is_some() {
                // A WebTransport stream's session id.
                self.classifying.swap_remove(i);
                return self.wt_stream(t, c.id, c.bidi, v);
            }
            if c.bidi {
                if v == frame::WT_STREAM {
                    if let Some(c) = self.classifying.get_mut(i) {
                        c.wt = Some(Partial::default());
                    }
                    continue;
                }
                self.classifying.swap_remove(i);
                return self.new_request(t, admit, c.id, v);
            }
            self.classifying.swap_remove(i);
            return self.new_uni(t, admit, c.id, v);
        }
    }

    /// A peer uni stream of type `kind` (RFC 9114 §6.2).
    fn new_uni(&mut self, t: &mut impl Transport, admit: &mut dyn Admit, id: u64, kind: u64) {
        match kind {
            stream_type::CONTROL => {
                if self.peer_control.is_some() {
                    return self.fail(t, code::H3_STREAM_CREATION_ERROR, b"a second control stream");
                }
                self.peer_control = Some((id, Ctl::First(FrameHead::default())));
                self.read_control(t, admit);
            }
            stream_type::ENCODER => {
                // A second one (nginx CVE-2026-42530 reopened it) is an error before any of its
                // state is kept; so is one after the first ended (that's H3_CLOSED_CRITICAL_STREAM,
                // already raised).
                if self.peer_encoder.is_some() {
                    return self.fail(t, code::H3_STREAM_CREATION_ERROR, b"a second QPACK encoder stream");
                }
                self.peer_encoder = Some(id);
                self.read_encoder(t);
            }
            stream_type::DECODER => {
                if self.peer_decoder.is_some() {
                    return self.fail(t, code::H3_STREAM_CREATION_ERROR, b"a second QPACK decoder stream");
                }
                self.peer_decoder = Some((id, qpack::DecoderStream::default()));
                self.read_decoder(t);
            }
            // A client may not push (RFC 9114 §6.2.2).
            stream_type::PUSH => self.fail(t, code::H3_STREAM_CREATION_ERROR, b"a push stream from the client"),
            stream_type::WEBTRANSPORT => {
                self.classifying.push(Classify { id, bidi: false, head: Partial::default(), wt: Some(Partial::default()) });
                let i = self.classifying.len() - 1;
                self.classify(t, admit, i);
            }
            // Unknown types (GREASE included) are stopped and forgotten (RFC 9114 §6.2).
            _ => t.stop(id, code::H3_STREAM_CREATION_ERROR),
        }
    }

    /// Refuses a peer stream: STOP_SENDING, and RESET_STREAM for a bidi one.
    fn refuse_stream(&mut self, t: &mut impl Transport, id: u64, bidi: bool, code: u64) {
        t.stop(id, code);
        if bidi {
            t.reset(id, code);
        }
    }

    /// A critical stream ended (RFC 9114 §6.2.1, RFC 9204 §4.2).
    fn critical(&self, id: u64) -> bool {
        self.peer_control.as_ref().is_some_and(|(c, _)| *c == id) || self.peer_encoder == Some(id) || self.peer_decoder.as_ref().is_some_and(|(d, _)| *d == id)
    }

    fn read_encoder(&mut self, t: &mut impl Transport) {
        let Some(id) = self.peer_encoder else { return };
        loop {
            match t.read(id, 256) {
                Read::Data(b) => {
                    if let Err(c) = qpack::encoder_stream(&b) {
                        return self.fail(t, c, b"QPACK encoder stream");
                    }
                }
                Read::Blocked => return,
                _ => return self.fail(t, code::H3_CLOSED_CRITICAL_STREAM, b"the QPACK encoder stream closed"),
            }
        }
    }

    fn read_decoder(&mut self, t: &mut impl Transport) {
        let Some((id, mut d)) = self.peer_decoder else { return };
        loop {
            match t.read(id, 256) {
                Read::Data(b) => {
                    if let Err(c) = d.read(&b) {
                        return self.fail(t, c, b"QPACK decoder stream");
                    }
                    self.peer_decoder = Some((id, d));
                }
                Read::Blocked => return,
                _ => return self.fail(t, code::H3_CLOSED_CRITICAL_STREAM, b"the QPACK decoder stream closed"),
            }
        }
    }

    fn read_control(&mut self, t: &mut impl Transport, admit: &mut dyn Admit) {
        let Some((id, _)) = &self.peer_control else { return };
        let id = *id;
        loop {
            let data = match t.read(id, 1024) {
                Read::Data(b) => b,
                Read::Blocked => return,
                _ => return self.fail(t, code::H3_CLOSED_CRITICAL_STREAM, b"the control stream closed"),
            };
            if let Err(c) = self.control_bytes(&data) {
                return self.fail(t, c, b"control stream");
            }
            if self.peer.is_some() && !self.waiting.is_empty() {
                // The client's SETTINGS are here: the requests that waited for them go on.
                for w in std::mem::take(&mut self.waiting) {
                    self.readable(t, admit, w);
                    if self.closed.is_some() {
                        return;
                    }
                }
            }
        }
    }

    /// The control stream's bytes (RFC 9114 §6.2.1, §7.2).
    fn control_bytes(&mut self, mut data: &[u8]) -> Result<(), u64> {
        while !data.is_empty() {
            let Some((_, ctl)) = &mut self.peer_control else { return Ok(()) };
            match ctl {
                Ctl::First(_) | Ctl::Head(_) => {
                    let first = matches!(ctl, Ctl::First(_));
                    let (Ctl::First(h) | Ctl::Head(h)) = ctl else { return Ok(()) };
                    let (v, n) = h.take(data);
                    data = data.get(n..).unwrap_or_default();
                    let Some((kind, len)) = v else { continue };
                    *ctl = match kind {
                        frame::SETTINGS if first => {
                            // A SETTINGS frame is a few dozen bytes; one far longer is load.
                            if len > 4096 {
                                return Err(code::H3_EXCESSIVE_LOAD);
                            }
                            Ctl::Settings { left: len, p: Partial::default(), id: None }
                        }
                        _ if first => return Err(code::H3_MISSING_SETTINGS),
                        frame::SETTINGS | frame::DATA | frame::HEADERS | frame::PUSH_PROMISE | frame::WT_STREAM => return Err(code::H3_FRAME_UNEXPECTED),
                        k if frame::reserved_h2(k) => return Err(code::H3_FRAME_UNEXPECTED),
                        frame::GOAWAY | frame::MAX_PUSH_ID | frame::CANCEL_PUSH => {
                            if len == 0 || len > 8 {
                                return Err(code::H3_FRAME_ERROR);
                            }
                            Ctl::One { left: len, p: Partial::default() }
                        }
                        _ if len == 0 => Ctl::Head(FrameHead::default()),
                        _ => Ctl::Skip(len),
                    };
                }
                Ctl::Settings { left, p, id } => {
                    let room = usize::try_from(*left).unwrap_or(usize::MAX).min(data.len());
                    let (chunk, rest) = data.split_at_checked(room).unwrap_or((data, &[]));
                    let mut used = 0;
                    while used < chunk.len() {
                        let (v, n) = p.take(chunk.get(used..).unwrap_or_default());
                        used += n;
                        let Some(v) = v else { break };
                        match id.take() {
                            None => *id = Some(v),
                            Some(k) => self.peer.get_or_insert_default().set(k, v)?,
                        }
                    }
                    *left -= chunk.len() as u64;
                    data = rest;
                    if *left == 0 {
                        if p.started() || id.is_some() {
                            return Err(code::H3_FRAME_ERROR);
                        }
                        self.peer.get_or_insert_default();
                        *ctl = Ctl::Head(FrameHead::default());
                    }
                }
                Ctl::Skip(left) => {
                    let n = usize::try_from(*left).unwrap_or(usize::MAX).min(data.len());
                    *left -= n as u64;
                    data = data.get(n..).unwrap_or_default();
                    if *left == 0 {
                        *ctl = Ctl::Head(FrameHead::default());
                    }
                }
                Ctl::One { left, p } => {
                    let n = usize::try_from(*left).unwrap_or(usize::MAX).min(data.len());
                    let (chunk, rest) = data.split_at_checked(n).unwrap_or((data, &[]));
                    let (v, used) = p.take(chunk);
                    *left -= used as u64;
                    data = rest.get(..).unwrap_or_default();
                    if used < chunk.len() || (*left == 0) != v.is_some() {
                        return Err(code::H3_FRAME_ERROR);
                    }
                    if *left == 0 {
                        *ctl = Ctl::Head(FrameHead::default());
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------- the request

    /// A client bidi stream whose first frame is of type `kind`: a request.
    fn new_request(&mut self, t: &mut impl Transport, admit: &mut dyn Admit, id: u64, kind: u64) {
        self.highest_request = Some(self.highest_request.map_or(id, |h| h.max(id)));
        // One request at a time, and none once a session is up (draft-14 §5.1: one session a
        // connection without WebTransport flow control); it's reset unread.
        if self.request.is_some() || self.session.is_some() || self.goaway {
            self.stats.refused += 1;
            return self.refuse_stream(t, id, true, code::H3_REQUEST_REJECTED);
        }
        if let Err(c) = check_request_frame(kind) {
            return self.fail(t, c, b"request stream frame");
        }
        // The first integer read was the frame's type; its length follows.
        let head = FrameHead { kind: Some(kind), p: Partial::default() };
        self.request = Some(Request { id, phase: Phase::Head(head) });
        self.read_request(t, admit);
    }

    fn read_request(&mut self, t: &mut impl Transport, admit: &mut dyn Admit) {
        loop {
            let Some(r) = &mut self.request else { return };
            let id = r.id;
            // Read exactly what the phase needs: never a byte of what follows.
            let want = match &r.phase {
                Phase::Head(h) => h.p.left(),
                Phase::Skip(n) => usize::try_from(*n).unwrap_or(usize::MAX).min(16 << 10),
                Phase::Headers { need, bytes } => need - bytes.len(),
            };
            let data = match t.read(id, want) {
                Read::Data(b) => b,
                Read::Blocked => return,
                _ => return self.request_gone(t, id),
            };
            match &mut r.phase {
                Phase::Skip(n) => {
                    *n -= (data.len() as u64).min(*n);
                    if *n == 0 {
                        r.phase = Phase::Head(FrameHead::default());
                    }
                }
                Phase::Headers { need, bytes } => {
                    bytes.extend_from_slice(&data);
                    if bytes.len() >= *need {
                        let section = std::mem::take(bytes);
                        self.request = None;
                        return self.headers(t, admit, id, &section);
                    }
                }
                Phase::Head(h) => {
                    let (v, _) = h.take(&data);
                    let Some((kind, len)) = v else { continue };
                    if let Err(c) = check_request_frame(kind) {
                        return self.fail(t, c, b"request stream frame");
                    }
                    r.phase = match kind {
                        frame::HEADERS => {
                            // Refused before its payload is read when it's longer than the
                            // budget: a field section never encodes larger than it decodes (each
                            // field counts 32 bytes it doesn't send).
                            if len == 0 || len > self.cfg.budget.bytes as u64 {
                                self.request = None;
                                self.stats.refused += 1;
                                return self.respond_refusal(t, id, 431);
                            }
                            Phase::Headers { need: len as usize, bytes: Vec::with_capacity(len as usize) }
                        }
                        // DATA before HEADERS (RFC 9114 §4.1).
                        frame::DATA => return self.fail(t, code::H3_FRAME_UNEXPECTED, b"DATA before HEADERS"),
                        _ if len == 0 => Phase::Head(FrameHead::default()),
                        _ => Phase::Skip(len),
                    };
                }
            }
        }
    }

    /// The request stream ended or was reset before its HEADERS.
    fn request_gone(&mut self, t: &mut impl Transport, id: u64) {
        self.request = None;
        self.stats.refused += 1;
        t.stop(id, code::H3_REQUEST_INCOMPLETE);
    }

    /// A request's field section, whole.
    fn headers(&mut self, t: &mut impl Transport, admit: &mut dyn Admit, id: u64, section: &[u8]) {
        let mut h = Head::default();
        let mut scratch = Vec::new();
        let decoded = qpack::decode(section, self.cfg.budget, &mut scratch, |n, v| h.field(n, v));
        drop(scratch);
        match decoded {
            Ok(_) => {}
            Err(Refused::Failed) => return self.fail(t, qpack::code::DECOMPRESSION_FAILED, b"QPACK"),
            Err(Refused::TooLarge) => {
                self.stats.refused += 1;
                return self.respond_refusal(t, id, 431);
            }
            Err(Refused::Caller(())) => {
                self.stats.refused += 1;
                return self.refuse_stream(t, id, true, code::H3_MESSAGE_ERROR);
            }
        }
        // RFC 9114 §4.3.1, §4.4, RFC 9220 §3, draft-14 §3.2.
        let Some(method) = h.method.as_deref() else {
            self.stats.refused += 1;
            return self.refuse_stream(t, id, true, code::H3_MESSAGE_ERROR);
        };
        if method != b"CONNECT" {
            self.stats.refused += 1;
            return self.refuse_stream(t, id, true, code::H3_REQUEST_REJECTED);
        }
        let Some(protocol) = &h.protocol else {
            // A classic CONNECT (a tunnel): not served here.
            self.stats.refused += 1;
            return self.refuse_stream(t, id, true, code::H3_REQUEST_REJECTED);
        };
        if protocol.as_slice() != b"webtransport" {
            self.stats.refused += 1;
            return self.respond_refusal(t, id, 501);
        }
        let draft = self.peer.and_then(|p| p.draft());
        let (Some(authority), Some(path), Some(b"https"), Some(draft)) = (h.authority.take(), h.path.take(), h.scheme.as_deref(), draft) else {
            // Malformed, or from a client whose SETTINGS don't speak WebTransport.
            self.stats.refused += 1;
            return self.refuse_stream(t, id, true, code::H3_MESSAGE_ERROR);
        };
        let connect = Connect { stream: id, authority, path, origin: h.origin.take(), draft };
        self.stats.admitted_asked += 1;
        match admit.admit(&connect) {
            Admission::Accept => self.accept(t, connect, h.draft02),
            Admission::Forbidden => {
                self.drop_pending(t, id);
                self.respond_refusal(t, id, 403);
            }
            Admission::NotFound => {
                self.drop_pending(t, id);
                self.respond_refusal(t, id, 404);
            }
        }
    }

    /// Answers a request with `status` and ends it, reading no more of it.
    fn respond_refusal(&mut self, t: &mut impl Transport, id: u64, status: u16) {
        let mut block = Vec::with_capacity(8);
        qpack::Encoder::begin(&mut block);
        qpack::Encoder::field(&mut block, b":status", status.to_string().as_bytes());
        let mut out = Vec::with_capacity(16);
        put_frame(&mut out, frame::HEADERS, &block);
        t.stop(id, code::H3_NO_ERROR);
        self.send(t, id, out, true);
        self.events.push_back(Event::Refused { stream: id, status });
    }

    /// Admits the session: 200, and the streams that waited for it.
    fn accept(&mut self, t: &mut impl Transport, connect: Connect, draft02_header: bool) {
        let id = connect.stream;
        let mut block = Vec::with_capacity(48);
        qpack::Encoder::begin(&mut block);
        qpack::Encoder::field(&mut block, b":status", b"200");
        if draft02_header || connect.draft == Draft::Draft02 {
            qpack::Encoder::field(&mut block, b"sec-webtransport-http3-draft", b"draft02");
        }
        let mut out = Vec::with_capacity(64);
        put_frame(&mut out, frame::HEADERS, &block);
        self.send(t, id, out, false);
        self.session = Some(Session {
            connect: connect.clone(),
            head: FrameHead::default(),
            data_left: 0,
            skipping: false,
            capsules: Capsules::new(self.cfg.datagram_max),
            streams: Vec::new(),
            capsule_datagrams: VecDeque::new(),
            ended: false,
            peer_ended: false,
        });
        self.events.push_back(Event::Accepted(connect));
        // Streams that came before the session are its now; any other id can't be served.
        for p in std::mem::take(&mut self.pending) {
            if p.session == id {
                self.adopt(p.id, p.bidi);
            } else {
                self.refuse_stream(t, p.id, p.bidi, code::WT_SESSION_GONE);
            }
        }
        self.read_connect(t);
    }

    /// Refuses the streams that waited for session `id`.
    fn drop_pending(&mut self, t: &mut impl Transport, id: u64) {
        let (gone, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending).into_iter().partition(|p| p.session == id);
        self.pending = keep;
        for p in gone {
            self.refuse_stream(t, p.id, p.bidi, code::WT_SESSION_GONE);
        }
    }

    // ------------------------------------------------------------------- WebTransport streams

    /// A peer WebTransport stream naming session `session` (draft-14 §4.2, §4.3, §4.6).
    fn wt_stream(&mut self, t: &mut impl Transport, id: u64, bidi: bool, session: u64) {
        // A session id must be a client-initiated bidi stream's (draft-14 §4).
        if session & 3 != 0 {
            return self.fail(t, code::H3_ID_ERROR, b"a session id that can't be one");
        }
        if let Some(s) = &self.session {
            if s.connect.stream == session && !s.ended {
                if s.streams.len() >= self.cfg.session_streams {
                    return self.refuse_stream(t, id, bidi, code::H3_EXCESSIVE_LOAD);
                }
                self.adopt(id, bidi);
                return;
            }
            return self.refuse_stream(t, id, bidi, code::WT_SESSION_GONE);
        }
        let deciding = self.request.as_ref().is_some_and(|r| r.id == session);
        if !deciding && self.highest_request.is_some_and(|h| session <= h) {
            return self.refuse_stream(t, id, bidi, code::WT_SESSION_GONE);
        }
        // Its session hasn't been admitted (yet): held unread, a few at most.
        if self.pending.len() >= self.cfg.pending_streams {
            self.stats.buffered_rejected += 1;
            return self.refuse_stream(t, id, bidi, code::WT_BUFFERED_STREAM_REJECTED);
        }
        self.pending.push(Pending { id, bidi, session });
    }

    /// A peer stream joins the session: the application hears of it.
    fn adopt(&mut self, id: u64, bidi: bool) {
        if let Some(s) = &mut self.session {
            s.streams.push(WtStream { id, header: [0; 16], at: 0, len: 0 });
            self.events.push_back(Event::Stream { id, bidi });
            self.events.push_back(Event::Readable(id));
        }
    }

    /// Opens a session stream of ours, its header written (or owed, written as credit comes).
    pub fn wt_open(&mut self, t: &mut impl Transport, bidi: bool) -> Option<u64> {
        let s = self.session.as_mut().filter(|s| !s.ended)?;
        if s.streams.len() >= self.cfg.session_streams {
            return None;
        }
        let id = t.open(bidi)?;
        let mut h = Vec::with_capacity(16);
        varint::put(&mut h, if bidi { frame::WT_STREAM } else { stream_type::WEBTRANSPORT });
        varint::put(&mut h, s.connect.stream);
        let mut w = WtStream { id, header: [0; 16], at: 0, len: h.len() as u8 };
        if let Some(dst) = w.header.get_mut(..h.len()) {
            dst.copy_from_slice(&h);
        }
        flush_header(t, &mut w);
        s.streams.push(w);
        Some(id)
    }

    /// Reads a session stream's next bytes (at most `max`).
    pub fn wt_read(&mut self, t: &mut impl Transport, id: u64, max: usize) -> Read {
        if !self.session.as_ref().is_some_and(|s| s.streams.iter().any(|w| w.id == id)) {
            return Read::Gone;
        }
        let r = t.read(id, max);
        if let Read::Reset(_) | Read::Fin = r {
            self.forget_if_done(id, true);
        }
        r
    }

    /// Writes on a session stream: the bytes taken (0 while its header waits for credit).
    pub fn wt_write(&mut self, t: &mut impl Transport, id: u64, data: &[u8]) -> Option<usize> {
        let s = self.session.as_mut()?;
        let w = s.streams.iter_mut().find(|w| w.id == id)?;
        if !flush_header(t, w) {
            return Some(0);
        }
        t.write(id, data)
    }

    /// Ends our side of a session stream.
    pub fn wt_finish(&mut self, t: &mut impl Transport, id: u64) {
        if self.session.as_ref().is_some_and(|s| s.streams.iter().any(|w| w.id == id && w.at == w.len)) {
            t.finish(id);
        }
    }

    /// Resets our side of a session stream with a WebTransport application code.
    pub fn wt_reset(&mut self, t: &mut impl Transport, id: u64, app: u32) {
        if self.session.as_ref().is_some_and(|s| s.streams.iter().any(|w| w.id == id)) {
            t.reset(id, super::wt_error_to_h3(app));
        }
    }

    /// Asks the peer to stop sending on a session stream, with a WebTransport application code.
    pub fn wt_stop(&mut self, t: &mut impl Transport, id: u64, app: u32) {
        if self.session.as_ref().is_some_and(|s| s.streams.iter().any(|w| w.id == id)) {
            t.stop(id, super::wt_error_to_h3(app));
        }
    }

    /// The application is done with a session stream (both sides ended): it's forgotten.
    pub fn wt_forget(&mut self, id: u64) {
        self.forget_if_done(id, false);
    }

    fn forget_if_done(&mut self, id: u64, peer_only: bool) {
        if let Some(s) = &mut self.session {
            // A peer's uni stream is done when read to its end; any other waits for the app.
            let uni_peer = id & 2 == 2 && id & 1 == 0;
            if !peer_only || uni_peer {
                s.streams.retain(|w| w.id != id);
            }
        }
    }

    /// Sends a datagram on the session (its quarter stream id first, RFC 9297 §2.1).
    pub fn send_datagram(&mut self, t: &mut impl Transport, payload: &[u8]) -> bool {
        let Some(s) = self.session.as_ref().filter(|s| !s.ended) else { return false };
        let q = s.connect.stream / 4;
        let mut b = BytesMut::with_capacity(varint::len(q) + payload.len());
        let mut head = Vec::with_capacity(8);
        varint::put(&mut head, q);
        b.put_slice(&head);
        b.put_slice(payload);
        t.send_datagram(b.freeze())
    }

    /// The largest datagram payload the session can send.
    pub fn max_datagram(&self, t: &impl Transport) -> Option<usize> {
        let s = self.session.as_ref()?;
        t.max_datagram().and_then(|m| m.checked_sub(varint::len(s.connect.stream / 4)))
    }

    /// The session's next datagram: from QUIC (those for no session, or another, are dropped) or
    /// from a DATAGRAM capsule.
    pub fn recv_datagram(&mut self, t: &mut impl Transport) -> Option<Bytes> {
        if let Some(d) = self.session.as_mut().and_then(|s| s.capsule_datagrams.pop_front()) {
            return Some(d);
        }
        while let Some(d) = t.recv_datagram() {
            let want = self.session.as_ref().filter(|s| !s.ended).map(|s| s.connect.stream / 4);
            match (varint::decode(&d), want) {
                (Some((q, n)), Some(w)) if q == w => return Some(d.slice(n..)),
                _ => self.stats.datagrams_dropped += 1,
            }
        }
        None
    }

    // ------------------------------------------------------------------- the CONNECT stream

    fn read_connect(&mut self, t: &mut impl Transport) {
        loop {
            let Some(s) = &mut self.session else { return };
            if s.peer_ended {
                return;
            }
            let id = s.connect.stream;
            let max = if s.data_left > 0 { usize::try_from(s.data_left).unwrap_or(usize::MAX).min(16 << 10) } else { HEAD_READ };
            let data = match t.read(id, max) {
                Read::Data(b) => b,
                Read::Blocked => return,
                Read::Fin | Read::Gone => return self.peer_closed(t, 0, Vec::new()),
                Read::Reset(_) => return self.peer_closed(t, 0, Vec::new()),
            };
            let mut rest: &[u8] = &data;
            while !rest.is_empty() {
                let Some(s) = &mut self.session else { return };
                if s.data_left > 0 {
                    let n = usize::try_from(s.data_left).unwrap_or(usize::MAX).min(rest.len());
                    let (chunk, after) = rest.split_at_checked(n).unwrap_or((rest, &[]));
                    s.data_left -= n as u64;
                    rest = after;
                    if s.skipping {
                        continue;
                    }
                    let mut got: Vec<(u8, u32, Vec<u8>)> = Vec::new();
                    let datagram_room = 4usize.saturating_sub(s.capsule_datagrams.len());
                    let mut datagrams = Vec::new();
                    let r = s.capsules.read(chunk, &mut |c| match c {
                        Capsule::Close { code, message } => got.push((0, code, message.to_vec())),
                        Capsule::Drain => got.push((1, 0, Vec::new())),
                        Capsule::Datagram(d) => {
                            if datagrams.len() < datagram_room {
                                datagrams.push(Bytes::copy_from_slice(d));
                            }
                        }
                    });
                    s.capsule_datagrams.extend(datagrams);
                    if r.is_err() {
                        return self.end_session(t, code::H3_MESSAGE_ERROR);
                    }
                    for (k, code, message) in got {
                        if k == 0 {
                            return self.peer_closed(t, code, message);
                        }
                        self.events.push_back(Event::Draining);
                    }
                    continue;
                }
                let (v, n) = s.head.take(rest);
                rest = rest.get(n..).unwrap_or_default();
                let Some((kind, len)) = v else { continue };
                match kind {
                    frame::DATA => {
                        s.data_left = len;
                        s.skipping = false;
                    }
                    // Trailers, or anything else a request stream may not carry here.
                    frame::HEADERS => return self.end_session(t, code::H3_MESSAGE_ERROR),
                    k => {
                        if let Err(c) = check_request_frame(k) {
                            return self.fail(t, c, b"CONNECT stream frame");
                        }
                        s.data_left = len;
                        s.skipping = true;
                    }
                }
            }
        }
    }

    /// The peer ended the session.
    fn peer_closed(&mut self, t: &mut impl Transport, code: u32, message: Vec<u8>) {
        let Some(s) = &mut self.session else { return };
        // Told once: a STOP_SENDING or the FIN after a WT_CLOSE_SESSION ends nothing more.
        if s.peer_ended && s.ended {
            return;
        }
        s.peer_ended = true;
        let id = s.connect.stream;
        let ended = s.ended;
        let streams: Vec<u64> = s.streams.drain(..).map(|w| w.id).collect();
        for w in streams {
            self.refuse_stream(t, w, w & 2 == 0, code::WT_SESSION_GONE);
        }
        self.drop_pending(t, id);
        if !ended {
            if let Some(s) = &mut self.session {
                s.ended = true;
            }
            // Our side ends too: with what's owed, then FIN.
            if self.owed.iter().any(|o| o.id == id) {
                if let Some(o) = self.owed.iter_mut().find(|o| o.id == id) {
                    o.fin = true;
                }
            } else {
                t.finish(id);
            }
        }
        self.events.push_back(Event::Closed { code, message });
    }

    /// Ends the session at once (a malformed CONNECT stream): reset both ways.
    fn end_session(&mut self, t: &mut impl Transport, code: u64) {
        let Some(s) = &mut self.session else { return };
        let id = s.connect.stream;
        s.ended = true;
        s.peer_ended = true;
        let streams: Vec<u64> = s.streams.drain(..).map(|w| w.id).collect();
        self.owed.retain(|o| o.id != id);
        t.reset(id, code);
        t.stop(id, code);
        for w in streams {
            self.refuse_stream(t, w, w & 2 == 0, code::WT_SESSION_GONE);
        }
        self.events.push_back(Event::Closed { code: 0, message: Vec::new() });
    }

    /// Closes the session with an application code and message (at most 1024 bytes): the
    /// WT_CLOSE_SESSION capsule goes out now if the credit the peer already granted covers it,
    /// else the CONNECT stream is reset now. Nothing waits on the peer (CVE-2026-21435), and no
    /// timer is armed: when this returns, the session's streams are reset and it holds nothing.
    pub fn close(&mut self, t: &mut impl Transport, app: u32, message: &[u8]) {
        let Some(s) = &mut self.session else { return };
        if s.ended {
            return;
        }
        s.ended = true;
        let id = s.connect.stream;
        let streams: Vec<u64> = s.streams.drain(..).map(|w| w.id).collect();
        s.capsule_datagrams.clear();
        for w in streams {
            self.refuse_stream(t, w, w & 2 == 0, code::WT_SESSION_GONE);
        }
        self.drop_pending(t, id);
        let mut out = Vec::with_capacity(16 + message.len());
        put_frame(&mut out, frame::DATA, &capsule::close(app, message));
        let owed = self.owed.iter().any(|o| o.id == id);
        match (owed, t.write(id, &out)) {
            (false, Some(n)) if n == out.len() => t.finish(id),
            _ => {
                self.owed.retain(|o| o.id != id);
                t.reset(id, code::WT_SESSION_GONE);
            }
        }
        t.stop(id, code::WT_SESSION_GONE);
        if let Some(s) = &mut self.session {
            s.peer_ended = true;
        }
        self.events.push_back(Event::Closed { code: app, message: message.get(..capsule::CLOSE_MESSAGE_MAX).unwrap_or(message).to_vec() });
    }

    /// Asks the peer to end the session soon (WT_DRAIN_SESSION) and, with `goaway`, tells it no
    /// further request will be served (GOAWAY on the control stream, RFC 9114 §5.2).
    pub fn drain(&mut self, t: &mut impl Transport, goaway: bool) {
        if goaway {
            self.goaway(t);
        }
        let Some(s) = self.session.as_ref().filter(|s| !s.ended) else { return };
        let id = s.connect.stream;
        let mut out = Vec::with_capacity(8);
        put_frame(&mut out, frame::DATA, &capsule::drain());
        self.send(t, id, out, false);
    }

    /// Sends GOAWAY once: requests from the next client bidi stream on are refused.
    pub fn goaway(&mut self, t: &mut impl Transport) {
        if self.goaway {
            return;
        }
        let Some(control) = self.control else { return };
        self.goaway = true;
        let last = self.session.as_ref().map_or(0, |s| s.connect.stream + 4);
        let mut payload = Vec::with_capacity(8);
        varint::put(&mut payload, last);
        let mut out = Vec::with_capacity(12);
        put_frame(&mut out, frame::GOAWAY, &payload);
        self.send(t, control, out, false);
    }

    // ------------------------------------------------------------------- resets and stops

    /// The peer reset stream `id` at `now_ns` (the caller's clock).
    pub fn reset_by_peer(&mut self, t: &mut impl Transport, admit: &mut dyn Admit, id: u64, error: u64, now_ns: u64) {
        if self.closed.is_some() {
            return;
        }
        if self.critical(id) {
            return self.fail(t, code::H3_CLOSED_CRITICAL_STREAM, b"a critical stream was reset");
        }
        // Resets past twice the stream limit in a second end the connection (CVE-2023-44487).
        let (start, n) = self.resets;
        self.resets = if now_ns.saturating_sub(start) >= 1_000_000_000 { (now_ns, 1) } else { (start, n.saturating_add(1)) };
        if self.resets.1 > self.cfg.resets_per_second {
            return self.fail(t, code::H3_EXCESSIVE_LOAD, b"too many stream resets");
        }
        if self.request.as_ref().is_some_and(|r| r.id == id) {
            return self.request_gone(t, id);
        }
        if self.session.as_ref().is_some_and(|s| s.connect.stream == id) {
            return self.read_connect(t);
        }
        self.classifying.retain(|c| c.id != id);
        self.waiting.retain(|w| *w != id);
        self.pending.retain(|p| p.id != id);
        if let Some(s) = &mut self.session
            && s.streams.iter().any(|w| w.id == id)
        {
            self.events.push_back(Event::StreamReset { id, code: super::h3_error_to_wt(error) });
        }
        let _ = admit;
    }

    /// The peer asked us to stop sending on `id`.
    pub fn stopped_by_peer(&mut self, t: &mut impl Transport, id: u64, error: u64) {
        if self.closed.is_some() {
            return;
        }
        if self.control == Some(id) {
            return self.fail(t, code::H3_CLOSED_CRITICAL_STREAM, b"the control stream was stopped");
        }
        self.owed.retain(|o| o.id != id);
        if let Some(s) = &self.session {
            if s.connect.stream == id {
                // The peer won't read the CONNECT stream: the session is over (draft-14 §6).
                t.reset(id, code::WT_SESSION_GONE);
                return self.peer_closed(t, 0, Vec::new());
            }
            if s.streams.iter().any(|w| w.id == id) {
                t.reset(id, error);
                self.events.push_back(Event::StreamStopped { id, code: super::h3_error_to_wt(error) });
                return;
            }
        }
        t.reset(id, code::H3_REQUEST_CANCELLED);
    }
}

/// Writes what's left of a stream's header: true once it's all out.
fn flush_header(t: &mut impl Transport, w: &mut WtStream) -> bool {
    if w.at >= w.len {
        return true;
    }
    let Some(rest) = w.header.get(usize::from(w.at)..usize::from(w.len)) else { return true };
    if let Some(n) = t.write(w.id, rest) {
        w.at = w.at.saturating_add(n as u8);
    }
    w.at >= w.len
}

/// Whether a frame of type `kind` may appear on a request stream (RFC 9114 §7.2).
fn check_request_frame(kind: u64) -> Result<(), u64> {
    match kind {
        frame::SETTINGS | frame::GOAWAY | frame::MAX_PUSH_ID | frame::CANCEL_PUSH | frame::PUSH_PROMISE => Err(code::H3_FRAME_UNEXPECTED),
        frame::WT_STREAM => Err(code::H3_FRAME_ERROR),
        k if frame::reserved_h2(k) => Err(code::H3_FRAME_UNEXPECTED),
        _ => Ok(()),
    }
}

/// A request's fields as the door reads them: the pseudo-headers and the few headers it uses;
/// every other field is checked and dropped as it decodes.
#[derive(Default)]
struct Head {
    method: Option<Vec<u8>>,
    protocol: Option<Vec<u8>>,
    scheme: Option<Vec<u8>>,
    authority: Option<Vec<u8>>,
    path: Option<Vec<u8>>,
    origin: Option<Vec<u8>>,
    draft02: bool,
    regular: bool,
}

impl Head {
    /// One field: `Err` makes the request malformed (RFC 9114 §4.1.2, §4.2, §4.3).
    fn field(&mut self, name: &[u8], value: &[u8]) -> Result<(), ()> {
        if name.is_empty() || name.iter().any(|b| b.is_ascii_uppercase()) || value.iter().any(|&b| b == 0 || b == b'\r' || b == b'\n') {
            return Err(());
        }
        if let Some(pseudo) = name.strip_prefix(b":") {
            // Pseudo-headers come first, once each.
            if self.regular {
                return Err(());
            }
            let slot = match pseudo {
                b"method" => &mut self.method,
                b"protocol" => &mut self.protocol,
                b"scheme" => &mut self.scheme,
                b"authority" => &mut self.authority,
                b"path" => &mut self.path,
                _ => return Err(()),
            };
            if slot.is_some() {
                return Err(());
            }
            *slot = Some(value.to_vec());
            return Ok(());
        }
        self.regular = true;
        match name {
            b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding" | b"upgrade" => return Err(()),
            b"te" if value != b"trailers" => return Err(()),
            b"origin" => {
                if self.origin.is_some() {
                    return Err(());
                }
                self.origin = Some(value.to_vec());
            }
            b"sec-webtransport-http3-draft02" => self.draft02 = value == b"1",
            _ => {}
        }
        Ok(())
    }
}
