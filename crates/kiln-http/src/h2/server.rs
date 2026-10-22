//! The HTTP/2 server connection (RFC 9113): one task for the connection and every stream on it.
//!
//! What a read brings is taken frame by frame. A request's handler is called and polled at once in
//! the connection's task; one ready then (most are) is answered there and then, its HEADERS and DATA
//! appended to the write buffer, and the stream never needs a place of its own. A handler that
//! waits, a body that streams, or a response held back by flow control keeps its stream in the
//! table, and is polled again whenever the connection is woken. Everything gathered leaves in one
//! write once the read is taken.
//!
//! Limits: a header block (and its CONTINUATION frames) no larger than four times the header list
//! limit, the list itself within the limit (else `431`), streams within the concurrency limit (else
//! `REFUSED_STREAM`), and a client that resets streams faster than it could use them is told
//! `ENHANCE_YOUR_CALM` (CVE-2023-44487).

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::hash::{BuildHasherDefault, Hasher};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::{Buf as _, Bytes, BytesMut};
use futures_io::{AsyncRead, AsyncWrite};
use http::header::{self, HeaderMap, HeaderValue};
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{Method, StatusCode, Uri, Version};
use http_body::Body as _;

use super::frame::{self, Head};
use super::hpack::{self, Field, Name};
use crate::body::{Feed, Incoming, Want};
use crate::buf::{SCRATCH, WriteBuf, poll_read_into};
use crate::rt::Sleep;
use crate::service::{SendBody, Service};
use crate::{Error, date};

/// The write buffer this full stops the connection taking more frames until it drains.
const HIGH_WATER: usize = 256 << 10;

/// How HTTP/2 connections are served.
#[derive(Clone, Copy, Debug)]
pub struct Builder {
    max_concurrent_streams: u32,
    stream_window: u32,
    connection_window: u32,
    max_header_list_size: u32,
    keep_alive_interval: Option<Duration>,
    keep_alive_timeout: Duration,
}

impl Default for Builder {
    fn default() -> Builder {
        Builder {
            max_concurrent_streams: 256,
            stream_window: 1 << 20,
            connection_window: 16 << 20,
            max_header_list_size: 64 << 10,
            keep_alive_interval: None,
            keep_alive_timeout: Duration::from_secs(20),
        }
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    /// The most streams a client may have open at once (256 unless set).
    pub fn max_concurrent_streams(&mut self, n: u32) -> &mut Builder {
        self.max_concurrent_streams = n.max(1);
        self
    }

    /// How much of a request body may arrive ahead of its reader (1 MiB unless set).
    pub fn initial_stream_window_size(&mut self, bytes: u32) -> &mut Builder {
        self.stream_window = bytes.clamp(1024, frame::MAX_WINDOW as u32);
        self
    }

    /// How much of all request bodies together may arrive ahead of their readers (16 MiB unless set).
    pub fn initial_connection_window_size(&mut self, bytes: u32) -> &mut Builder {
        self.connection_window = bytes.clamp(frame::DEFAULT_WINDOW as u32, frame::MAX_WINDOW as u32);
        self
    }

    /// The largest header list a request may have (64 KiB unless set): a larger one is answered `431`.
    pub fn max_header_list_size(&mut self, bytes: u32) -> &mut Builder {
        self.max_header_list_size = bytes.max(1024);
        self
    }

    /// Pings a connection quiet this long, and closes it when no answer comes within the timeout.
    pub fn keep_alive_interval(&mut self, every: Option<Duration>) -> &mut Builder {
        self.keep_alive_interval = every;
        self
    }

    pub fn keep_alive_timeout(&mut self, timeout: Duration) -> &mut Builder {
        self.keep_alive_timeout = timeout;
        self
    }

    /// Serves `io` (a connection that starts with the client's preface) until it ends.
    pub fn serve<I, S>(&self, io: I, service: S) -> impl Future<Output = Result<(), Error>> + use<I, S>
    where
        I: AsyncRead + AsyncWrite + Unpin,
        S: Service<Body: Unpin>,
    {
        serve(*self, io, Bytes::new(), service)
    }

    /// [`Builder::serve`], with what was read from `io` already (the preface, or some of it).
    pub fn serve_read<I, S>(&self, io: I, read: Bytes, service: S) -> impl Future<Output = Result<(), Error>> + use<I, S>
    where
        I: AsyncRead + AsyncWrite + Unpin,
        S: Service<Body: Unpin>,
    {
        serve(*self, io, read, service)
    }
}

/// A connection error: the code GOAWAY says.
struct ConnError(u32);

impl From<hpack::DecodeError> for ConnError {
    fn from(_: hpack::DecodeError) -> ConnError {
        ConnError(frame::COMPRESSION_ERROR)
    }
}

/// Stream ids are small and dense: a multiply spreads them well enough.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 << 8) | u64::from(*b);
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type Ids = BuildHasherDefault<IdHasher>;

/// A request body still arriving.
struct BodyIn {
    feed: Feed,
    /// What the content-length still owes, when it gave one.
    left: Option<u64>,
    /// What the stream may still receive.
    window: i64,
    /// The reader's count at the last look.
    mark: usize,
    /// Taken by the reader and not yet given back to the stream's window.
    owed: usize,
}

/// A response going out.
struct Out<B> {
    /// The body, while it has more to give.
    body: Option<B>,
    /// Data taken from the body and not yet sent (flow control held it back).
    held: Bytes,
    trailers: Option<HeaderMap>,
    /// The body is over: END_STREAM once what's held is sent.
    ended: bool,
}

struct Stream<F, B> {
    handler: Option<Pin<Box<Option<F>>>>,
    body_in: Option<BodyIn>,
    out: Option<Out<B>>,
    send_window: i64,
    /// The client sent END_STREAM.
    remote_done: bool,
    /// We sent END_STREAM, or reset the stream.
    local_done: bool,
    /// The request was `HEAD`: no body goes out.
    head: bool,
}

/// A header block arriving in pieces.
struct Block {
    stream: u32,
    end_stream: bool,
    buf: BytesMut,
}

/// The connection's transport, buffers, compression and windows: everything but the streams.
struct Wire<I> {
    io: I,
    rbuf: BytesMut,
    wbuf: WriteBuf,
    eof: bool,
    decoder: hpack::Decoder,
    encoder: hpack::Encoder,
    /// The largest frame the client takes.
    peer_max_frame: usize,
    /// The client's window for each new stream.
    peer_window: i64,
    /// What the connection may send.
    send_window: i64,
    /// What the connection may still receive.
    recv_window: i64,
    /// Received bytes taken (or thrown away) and not yet given back to the connection's window.
    owed: usize,
    cfg: Builder,
    /// A response's header map, emptied, for the next request's headers to take.
    spare: Option<HeaderMap>,
    /// The client acknowledged our settings: until it has, it may hold to the defaults.
    acked: bool,
    /// The date responses carry, read once a pass over what was read.
    date: [u8; date::LEN],
}

impl<I: AsyncRead + AsyncWrite + Unpin> Wire<I> {
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.wbuf.poll_flush(&mut self.io, cx).map_err(Error::io)
    }
}

impl<I> Wire<I> {
    /// Keeps a header map's allocation for the next request (emptied).
    fn keep_spare(&mut self, mut map: HeaderMap) {
        if map.capacity() > 0 && self.spare.is_none() {
            map.clear();
            self.spare = Some(map);
        }
    }

    /// The next whole frame read, its payload taken from the buffer.
    fn next_frame(&mut self) -> Result<Option<(Head, Bytes)>, ConnError> {
        let Some(h) = frame::head(&self.rbuf) else { return Ok(None) };
        if h.len > frame::DEFAULT_MAX_FRAME as usize {
            return Err(ConnError(frame::FRAME_SIZE_ERROR));
        }
        if self.rbuf.len() < 9 + h.len {
            return Ok(None);
        }
        self.rbuf.advance(9);
        Ok(Some((h, self.rbuf.split_to(h.len).freeze())))
    }

    /// A frame of `len` bytes' head, appended (the payload follows).
    fn head(&mut self, len: usize, kind: u8, flags: u8, stream: u32) {
        frame::put_head(self.wbuf.tail(), len, kind, flags, stream);
    }

    /// Encodes a response head as HEADERS (and CONTINUATION, when it's larger than a frame).
    fn headers(&mut self, stream: u32, status: StatusCode, headers: &HeaderMap, length: Option<u64>, end_stream: bool) {
        let tail = self.wbuf.tail();
        let start = tail.len();
        tail.extend_from_slice(&[0; 9]);
        self.encoder.begin(tail);
        self.encoder.status(status, tail);
        let mut has_length = false;
        let mut has_date = false;
        for (name, value) in headers {
            // Connection-specific headers have no meaning in HTTP/2 and make a response malformed.
            if *name == header::CONNECTION || *name == header::TRANSFER_ENCODING || *name == header::UPGRADE || name.as_str() == "keep-alive" || name.as_str() == "proxy-connection" {
                continue;
            }
            has_length |= *name == header::CONTENT_LENGTH;
            has_date |= *name == header::DATE;
            self.encoder.header(name, value, tail);
        }
        if !has_length && let Some(n) = length {
            // content-length (static name 28), never indexed: it differs from response to response.
            hpack::put_int(tail, 0x00, 4, 28);
            let at = tail.len();
            tail.push(0);
            crate::buf::put_dec(tail, n);
            let digits = tail.len() - at - 1;
            if let Some(len) = tail.get_mut(at) {
                *len = digits as u8;
            }
        }
        if !has_date {
            self.encoder.header_bytes(&header::DATE, &self.date, tail);
        }
        let len = tail.len() - start - 9;
        let flags = if end_stream { frame::END_STREAM } else { 0 };
        if len <= self.peer_max_frame {
            let mut h = Vec::with_capacity(9);
            frame::put_head(&mut h, len, frame::HEADERS, flags | frame::END_HEADERS, stream);
            if let Some(slot) = tail.get_mut(start..start + 9) {
                slot.copy_from_slice(&h);
            }
            return;
        }
        // Too large for one frame: the block goes out in pieces.
        let block = tail.split_off(start + 9);
        tail.truncate(start);
        let mut pieces = block.chunks(self.peer_max_frame).peekable();
        let mut kind = frame::HEADERS;
        let mut first_flags = flags;
        while let Some(piece) = pieces.next() {
            let last = pieces.peek().is_none();
            frame::put_head(tail, piece.len(), kind, first_flags | if last { frame::END_HEADERS } else { 0 }, stream);
            tail.extend_from_slice(piece);
            kind = frame::CONTINUATION;
            first_flags = 0;
        }
    }

    /// Sends what flow control lets go of `data`: what's left over.
    fn data(&mut self, stream: u32, send_window: &mut i64, mut data: Bytes, end_stream: bool) -> Bytes {
        loop {
            let room = (*send_window).min(self.send_window).max(0) as usize;
            let n = data.len().min(room).min(self.peer_max_frame);
            if n == 0 && !data.is_empty() {
                return data;
            }
            let last = n == data.len();
            let piece = data.split_to(n);
            self.head(n, frame::DATA, if last && end_stream { frame::END_STREAM } else { 0 }, stream);
            self.wbuf.push(piece);
            *send_window -= n as i64;
            self.send_window -= n as i64;
            if last {
                return data;
            }
        }
    }

    fn rst(&mut self, stream: u32, code: u32) {
        frame::put_rst(self.wbuf.tail(), stream, code);
    }
}

/// Puts a response on its stream: the head, and as much of the body as goes at once. Whether the
/// response is all sent (else what remains is in `s.out`).
fn respond<I, F, B: SendBody + Unpin>(wire: &mut Wire<I>, id: u32, s: &mut Stream<F, B>, resp: http::Response<B>, cx: &mut Context<'_>) -> bool {
    let (parts, mut body) = resp.into_parts();
    let length = body.size_hint().exact();
    let empty = s.head || body.is_end_stream() || length == Some(0);
    wire.headers(id, parts.status, &parts.headers, length.filter(|_| !s.head), empty);
    wire.keep_spare(parts.headers);
    if empty {
        s.local_done = true;
        return true;
    }
    let mut out = Out { body: None, held: Bytes::new(), trailers: None, ended: false };
    match pump(wire, id, s, &mut body, &mut out, cx) {
        true => {
            s.local_done = true;
            true
        }
        false => {
            if !out.ended {
                out.body = Some(body);
            }
            s.out = Some(out);
            false
        }
    }
}

/// Moves a response body to the wire as flow control allows: whether it's all sent.
fn pump<I, F, B: SendBody + Unpin>(wire: &mut Wire<I>, id: u32, s: &mut Stream<F, B>, body: &mut B, out: &mut Out<B>, cx: &mut Context<'_>) -> bool {
    loop {
        if !out.held.is_empty() {
            let held = std::mem::take(&mut out.held);
            let end = out.ended && out.trailers.is_none();
            out.held = wire.data(id, &mut s.send_window, held, end);
            if !out.held.is_empty() {
                return false;
            }
            if end {
                return true;
            }
        }
        if out.ended {
            match out.trailers.take() {
                Some(t) => {
                    trailers(wire, id, &t);
                }
                None => {
                    // The body ended without its last data carrying END_STREAM.
                    wire.head(0, frame::DATA, frame::END_STREAM, id);
                }
            }
            return true;
        }
        if wire.wbuf.len() >= HIGH_WATER {
            // Enough waits to be written: the connection writes it, then comes back.
            return false;
        }
        match Pin::new(&mut *body).poll_frame(cx) {
            Poll::Ready(Some(Ok(f))) => match f.into_data() {
                Ok(data) => {
                    let last = body.is_end_stream();
                    if last {
                        out.ended = true;
                    }
                    if data.is_empty() {
                        continue;
                    }
                    out.held = wire.data(id, &mut s.send_window, data, last);
                    if out.held.is_empty() && last {
                        return true;
                    }
                }
                Err(f) => {
                    if let Ok(t) = f.into_trailers() {
                        out.trailers = Some(t);
                    }
                }
            },
            Poll::Ready(Some(Err(_))) => {
                wire.rst(id, frame::INTERNAL_ERROR);
                return true;
            }
            Poll::Ready(None) => out.ended = true,
            Poll::Pending => return false,
        }
    }
}

fn trailers<I>(wire: &mut Wire<I>, id: u32, t: &HeaderMap) {
    let tail = wire.wbuf.tail();
    let start = tail.len();
    tail.extend_from_slice(&[0; 9]);
    wire.encoder.begin(tail);
    for (name, value) in t {
        wire.encoder.header(name, value, tail);
    }
    let len = tail.len() - start - 9;
    let mut h = Vec::with_capacity(9);
    frame::put_head(&mut h, len, frame::HEADERS, frame::END_STREAM | frame::END_HEADERS, id);
    if let Some(slot) = tail.get_mut(start..start + 9) {
        slot.copy_from_slice(&h);
    }
}

/// The request a header block describes (RFC 9113 §8.3.1), built field by field as the block is
/// decoded, each value moved where it goes (a clone of a shared value costs an atomic count).
#[derive(Default)]
struct Building {
    method: Option<Bytes>,
    scheme: Option<Bytes>,
    authority: Option<Bytes>,
    path: Option<Bytes>,
    headers: HeaderMap,
    length: Option<u64>,
    seen_regular: bool,
    /// Malformed: the stream is reset.
    bad: bool,
}

impl Building {
    fn new(headers: HeaderMap) -> Building {
        Building { headers, ..Building::default() }
    }

    fn add(&mut self, f: Field) {
        if self.bad {
            return;
        }
        match f.name {
            Name::Header(name) => {
                self.seen_regular = true;
                if name == header::CONNECTION || name == header::TRANSFER_ENCODING || name == header::UPGRADE || name.as_str() == "keep-alive" || name.as_str() == "proxy-connection" {
                    self.bad = true;
                    return;
                }
                if name == header::TE && &f.value[..] != b"trailers" {
                    self.bad = true;
                    return;
                }
                if name == header::CONTENT_LENGTH {
                    let n = std::str::from_utf8(&f.value).ok().filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())).and_then(|v| v.parse::<u64>().ok());
                    match n {
                        Some(n) if self.length.is_none_or(|m| m == n) => self.length = Some(n),
                        _ => {
                            self.bad = true;
                            return;
                        }
                    }
                }
                match HeaderValue::from_maybe_shared(f.value) {
                    Ok(v) => {
                        self.headers.append(name, v);
                    }
                    Err(_) => self.bad = true,
                }
            }
            Name::Invalid(_) | Name::Status | Name::Protocol => self.bad = true,
            pseudo => {
                // Pseudo-headers come first, each once.
                let slot = match pseudo {
                    Name::Method => &mut self.method,
                    Name::Scheme => &mut self.scheme,
                    Name::Authority => &mut self.authority,
                    _ => &mut self.path,
                };
                if self.seen_regular || slot.is_some() {
                    self.bad = true;
                    return;
                }
                *slot = Some(f.value);
            }
        }
    }

    /// The request, its body from `body` (given the content-length): `Err` with the header map to
    /// reuse when it's malformed.
    fn finish(self, body: impl FnOnce(Option<u64>) -> Option<Incoming>) -> Result<http::Request<Incoming>, HeaderMap> {
        let Building { method, scheme, authority, path, headers, length, bad, .. } = self;
        if bad {
            return Err(headers);
        }
        let uri = || -> Result<(Method, Uri), ()> {
            let method = match method.as_deref() {
                Some(b"GET") => Method::GET,
                Some(b"POST") => Method::POST,
                Some(m) => Method::from_bytes(m).map_err(|_| ())?,
                None => return Err(()),
            };
            let mut parts = http::uri::Parts::default();
            if method == Method::CONNECT {
                if scheme.is_some() || path.is_some() {
                    return Err(());
                }
                parts.authority = Some(Authority::from_maybe_shared(authority.ok_or(())?).map_err(|_| ())?);
            } else {
                let scheme = scheme.ok_or(())?;
                parts.scheme = Some(match &scheme[..] {
                    b"https" => Scheme::HTTPS,
                    b"http" => Scheme::HTTP,
                    other => Scheme::try_from(other).map_err(|_| ())?,
                });
                if let Some(a) = authority.filter(|a| !a.is_empty()) {
                    parts.authority = Some(Authority::from_maybe_shared(a).map_err(|_| ())?);
                }
                let path = path.filter(|p| !p.is_empty()).ok_or(())?;
                parts.path_and_query = Some(if &path[..] == b"/" { slash() } else { PathAndQuery::from_maybe_shared(path).map_err(|_| ())? });
            }
            Ok((method, Uri::from_parts(parts).map_err(|_| ())?))
        };
        let Ok((method, uri)) = uri() else { return Err(headers) };
        let Some(body) = body(length) else { return Err(headers) };
        let mut req = http::Request::new(body);
        *req.method_mut() = method;
        *req.uri_mut() = uri;
        *req.version_mut() = Version::HTTP_2;
        *req.headers_mut() = headers;
        Ok(req)
    }
}

/// `/`, parsed once (a clone of it shares static bytes: no count).
fn slash() -> PathAndQuery {
    static SLASH: std::sync::LazyLock<PathAndQuery> = std::sync::LazyLock::new(|| PathAndQuery::from_static("/"));
    SLASH.clone()
}

struct Conn<I, S: Service> {
    wire: Wire<I>,
    service: S,
    streams: HashMap<u32, Stream<S::Future, S::Body>, Ids>,
    /// The highest stream the client opened.
    last_stream: u32,
    block: Option<Block>,
    /// The preface and the client's first SETTINGS arrived.
    started: bool,
    /// We sent GOAWAY (closing), or the client did.
    going_away: bool,
    /// Spare handler slots.
    slots: Vec<Pin<Box<Option<S::Future>>>>,
    /// Streams the client reset before their answers, in the current second.
    resets: (Instant, u32),
    /// Keep-alive: when the connection last heard from the client, the timer, and a ping out.
    heard: Instant,
    keep_alive: Option<Sleep>,
    ping_sent: Option<Instant>,
}

impl<I: AsyncRead + AsyncWrite + Unpin, S: Service<Body: Unpin>> Conn<I, S> {
    fn slot(&mut self, fut: S::Future) -> Pin<Box<Option<S::Future>>> {
        match self.slots.pop() {
            Some(mut s) => {
                s.set(Some(fut));
                s
            }
            None => Box::pin(Some(fut)),
        }
    }

    fn give_slot(&mut self, mut slot: Pin<Box<Option<S::Future>>>) {
        slot.set(None);
        if self.slots.len() < 8 {
            self.slots.push(slot);
        }
    }

    fn on_frame(&mut self, h: Head, payload: Bytes, cx: &mut Context<'_>) -> Result<(), ConnError> {
        if let Some(block) = &mut self.block {
            // Nothing may come between a header block's pieces (RFC 9113 §6.10).
            if h.kind != frame::CONTINUATION || h.stream != block.stream {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            block.buf.extend_from_slice(&payload);
            if block.buf.len() > self.wire.cfg.max_header_list_size as usize * 4 {
                return Err(ConnError(frame::ENHANCE_YOUR_CALM));
            }
            if h.flags & frame::END_HEADERS != 0
                && let Some(b) = self.block.take()
            {
                return self.on_block(b.stream, b.end_stream, b.buf.freeze(), cx);
            }
            return Ok(());
        }
        if !self.started {
            if h.kind != frame::SETTINGS || h.flags & frame::ACK != 0 {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            self.started = true;
        }
        match h.kind {
            frame::DATA => self.on_data(h, payload),
            frame::HEADERS => self.on_headers(h, payload, cx),
            frame::PRIORITY => {
                if h.stream == 0 {
                    return Err(ConnError(frame::PROTOCOL_ERROR));
                }
                if payload.len() != 5 {
                    self.wire.rst(h.stream, frame::FRAME_SIZE_ERROR);
                }
                Ok(())
            }
            frame::RST_STREAM => self.on_rst(h, &payload),
            frame::SETTINGS => self.on_settings(h, &payload),
            frame::PUSH_PROMISE => Err(ConnError(frame::PROTOCOL_ERROR)),
            frame::PING => {
                if h.stream != 0 {
                    return Err(ConnError(frame::PROTOCOL_ERROR));
                }
                let data: [u8; 8] = payload.as_ref().try_into().map_err(|_| ConnError(frame::FRAME_SIZE_ERROR))?;
                if h.flags & frame::ACK == 0 {
                    frame::put_ping(self.wire.wbuf.tail(), true, data);
                } else {
                    self.ping_sent = None;
                }
                Ok(())
            }
            frame::GOAWAY => {
                if h.stream != 0 {
                    return Err(ConnError(frame::PROTOCOL_ERROR));
                }
                self.going_away = true;
                Ok(())
            }
            frame::WINDOW_UPDATE => self.on_window_update(h, &payload),
            frame::CONTINUATION => Err(ConnError(frame::PROTOCOL_ERROR)),
            _ => Ok(()),
        }
    }

    fn on_settings(&mut self, h: Head, payload: &[u8]) -> Result<(), ConnError> {
        if h.stream != 0 {
            return Err(ConnError(frame::PROTOCOL_ERROR));
        }
        if h.flags & frame::ACK != 0 {
            if !payload.is_empty() {
                return Err(ConnError(frame::FRAME_SIZE_ERROR));
            }
            self.wire.acked = true;
            return Ok(());
        }
        let (settings, rest) = payload.as_chunks::<6>();
        if !rest.is_empty() {
            return Err(ConnError(frame::FRAME_SIZE_ERROR));
        }
        for &[a, b, c, d, e, f] in settings {
            let id = u16::from_be_bytes([a, b]);
            let v = u32::from_be_bytes([c, d, e, f]);
            match id {
                frame::HEADER_TABLE_SIZE => self.wire.encoder.set_max(v as usize),
                frame::ENABLE_PUSH if v > 1 => return Err(ConnError(frame::PROTOCOL_ERROR)),
                frame::INITIAL_WINDOW_SIZE => {
                    let v = i64::from(v);
                    if v > frame::MAX_WINDOW {
                        return Err(ConnError(frame::FLOW_CONTROL_ERROR));
                    }
                    let delta = v - self.wire.peer_window;
                    self.wire.peer_window = v;
                    for s in self.streams.values_mut() {
                        s.send_window += delta;
                        if s.send_window > frame::MAX_WINDOW {
                            return Err(ConnError(frame::FLOW_CONTROL_ERROR));
                        }
                    }
                }
                frame::MAX_FRAME_SIZE => {
                    if !(frame::DEFAULT_MAX_FRAME..=16_777_215).contains(&v) {
                        return Err(ConnError(frame::PROTOCOL_ERROR));
                    }
                    self.wire.peer_max_frame = v as usize;
                }
                _ => {}
            }
        }
        frame::put_head(self.wire.wbuf.tail(), 0, frame::SETTINGS, frame::ACK, 0);
        Ok(())
    }

    fn on_window_update(&mut self, h: Head, payload: &[u8]) -> Result<(), ConnError> {
        if payload.len() != 4 {
            return Err(ConnError(frame::FRAME_SIZE_ERROR));
        }
        let inc = i64::from(frame::u32_at(payload, 0).unwrap_or(0) & 0x7fff_ffff);
        if h.stream == 0 {
            if inc == 0 {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            self.wire.send_window += inc;
            if self.wire.send_window > frame::MAX_WINDOW {
                return Err(ConnError(frame::FLOW_CONTROL_ERROR));
            }
            return Ok(());
        }
        match self.streams.get_mut(&h.stream) {
            Some(s) => {
                if inc == 0 {
                    s.local_done = true;
                    self.wire.rst(h.stream, frame::PROTOCOL_ERROR);
                    return Ok(());
                }
                s.send_window += inc;
                if s.send_window > frame::MAX_WINDOW {
                    s.local_done = true;
                    s.out = None;
                    self.wire.rst(h.stream, frame::FLOW_CONTROL_ERROR);
                }
                Ok(())
            }
            None if h.stream > self.last_stream => Err(ConnError(frame::PROTOCOL_ERROR)),
            None => Ok(()),
        }
    }

    fn on_rst(&mut self, h: Head, payload: &[u8]) -> Result<(), ConnError> {
        if h.stream == 0 {
            return Err(ConnError(frame::PROTOCOL_ERROR));
        }
        if payload.len() != 4 {
            return Err(ConnError(frame::FRAME_SIZE_ERROR));
        }
        if h.stream > self.last_stream {
            return Err(ConnError(frame::PROTOCOL_ERROR));
        }
        if let Some(s) = self.streams.remove(&h.stream) {
            if s.handler.is_some() {
                // A request ended before its answer: a client doing that fast is attacking.
                let now = Instant::now();
                if now.duration_since(self.resets.0) > Duration::from_secs(1) {
                    self.resets = (now, 0);
                }
                self.resets.1 += 1;
                if self.resets.1 > self.wire.cfg.max_concurrent_streams.saturating_mul(2).max(100) {
                    return Err(ConnError(frame::ENHANCE_YOUR_CALM));
                }
            }
            self.forget(s);
        }
        Ok(())
    }

    /// Lets go of a stream: its handler and bodies dropped, what its body received given back.
    fn forget(&mut self, mut s: Stream<S::Future, S::Body>) {
        if let Some(slot) = s.handler.take() {
            self.give_slot(slot);
        }
        if let Some(b) = s.body_in.take() {
            self.wire.owed += b.feed.buffered();
        }
    }

    fn on_data(&mut self, h: Head, payload: Bytes) -> Result<(), ConnError> {
        if h.stream == 0 {
            return Err(ConnError(frame::PROTOCOL_ERROR));
        }
        let len = payload.len();
        if len as i64 > self.wire.recv_window {
            return Err(ConnError(frame::FLOW_CONTROL_ERROR));
        }
        self.wire.recv_window -= len as i64;
        let mut data = payload;
        if h.flags & frame::PADDED != 0 {
            let pad = usize::from(*data.first().ok_or(ConnError(frame::PROTOCOL_ERROR))?);
            if pad >= data.len() {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            data.advance(1);
            data.truncate(data.len() - pad);
        }
        // Padding is given back at once.
        self.wire.owed += len - data.len();
        let end = h.flags & frame::END_STREAM != 0;
        let Some(s) = self.streams.get_mut(&h.stream) else {
            if h.stream > self.last_stream {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            self.wire.owed += data.len();
            self.wire.rst(h.stream, frame::STREAM_CLOSED);
            return Ok(());
        };
        if s.remote_done {
            self.wire.owed += data.len();
            s.local_done = true;
            s.out = None;
            self.wire.rst(h.stream, frame::STREAM_CLOSED);
            return Ok(());
        }
        let Some(b) = &mut s.body_in else {
            self.wire.owed += data.len();
            return Ok(());
        };
        if len as i64 > b.window {
            s.local_done = true;
            s.out = None;
            self.wire.rst(h.stream, frame::FLOW_CONTROL_ERROR);
            return Ok(());
        }
        b.window -= len as i64;
        b.owed += len - data.len();
        if let Some(left) = &mut b.left {
            match left.checked_sub(data.len() as u64) {
                Some(l) if !end || l == 0 => *left = l,
                _ => {
                    b.feed.fail(Error::h2(frame::PROTOCOL_ERROR));
                    s.local_done = true;
                    s.out = None;
                    self.wire.rst(h.stream, frame::PROTOCOL_ERROR);
                    return Ok(());
                }
            }
        }
        if b.feed.want() == Want::Gone {
            self.wire.owed += data.len();
        } else {
            b.feed.push(data);
        }
        if end {
            b.feed.end(None);
            s.remote_done = true;
        }
        Ok(())
    }

    fn on_headers(&mut self, h: Head, payload: Bytes, cx: &mut Context<'_>) -> Result<(), ConnError> {
        if h.stream == 0 || h.stream.is_multiple_of(2) {
            return Err(ConnError(frame::PROTOCOL_ERROR));
        }
        let mut block = payload;
        if h.flags & frame::PADDED != 0 {
            let pad = usize::from(*block.first().ok_or(ConnError(frame::PROTOCOL_ERROR))?);
            if pad >= block.len() {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            block.advance(1);
            block.truncate(block.len() - pad);
        }
        if h.flags & frame::PRIORITY_FLAG != 0 {
            if block.len() < 5 {
                return Err(ConnError(frame::FRAME_SIZE_ERROR));
            }
            block.advance(5);
        }
        let end_stream = h.flags & frame::END_STREAM != 0;
        if h.flags & frame::END_HEADERS == 0 {
            self.block = Some(Block { stream: h.stream, end_stream, buf: BytesMut::from(&block[..]) });
            return Ok(());
        }
        self.on_block(h.stream, end_stream, block, cx)
    }

    /// A whole header block: a new request, or a request's trailers.
    fn on_block(&mut self, id: u32, end_stream: bool, block: Bytes, cx: &mut Context<'_>) -> Result<(), ConnError> {
        let max_list = self.wire.cfg.max_header_list_size as usize;
        if let Some(s) = self.streams.get_mut(&id) {
            // Trailers: they end the request body.
            let mut t = HeaderMap::new();
            self.wire.decoder.decode_each(&block, max_list, |f| {
                if let (Name::Header(n), Ok(v)) = (f.name, HeaderValue::from_maybe_shared(f.value)) {
                    t.append(n, v);
                }
            })?;
            if s.remote_done || !end_stream {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            s.remote_done = true;
            if let Some(b) = &s.body_in {
                b.feed.end(Some(t));
            }
            return Ok(());
        }
        let mut building = Building::new(self.wire.spare.take().unwrap_or_default());
        let whole = self.wire.decoder.decode_each(&block, max_list, |f| building.add(f))?;
        self.on_request(id, end_stream, whole, building, cx)
    }

    fn on_request(&mut self, id: u32, end_stream: bool, whole: bool, building: Building, cx: &mut Context<'_>) -> Result<(), ConnError> {
        if id <= self.last_stream {
            return Err(ConnError(frame::PROTOCOL_ERROR));
        }
        self.last_stream = id;
        if self.going_away {
            return Ok(());
        }
        if self.streams.len() >= self.wire.cfg.max_concurrent_streams as usize {
            self.wire.rst(id, frame::REFUSED_STREAM);
            return Ok(());
        }
        let mut s = Stream { handler: None, body_in: None, out: None, send_window: self.wire.peer_window, remote_done: end_stream, local_done: false, head: false };
        if !whole {
            let resp = http::Response::builder().status(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE).body(()).map(|r| r.into_parts().0).ok();
            if let Some(parts) = resp {
                self.wire.headers(id, parts.status, &parts.headers, None, true);
            }
            if !end_stream {
                self.wire.rst(id, frame::NO_ERROR);
            }
            return Ok(());
        }
        // A client that hasn't taken our settings yet may send what the default window allows.
        let window = if self.wire.acked { i64::from(self.wire.cfg.stream_window) } else { i64::from(self.wire.cfg.stream_window).max(frame::DEFAULT_WINDOW) };
        let body_in = &mut s.body_in;
        let built = building.finish(|length| {
            if end_stream {
                // A length that says there's a body, with none: malformed.
                return length.is_none_or(|n| n == 0).then(Incoming::empty);
            }
            let (body, feed) = Incoming::piped(length);
            *body_in = Some(BodyIn { feed, left: length, window, mark: 0, owed: 0 });
            Some(body)
        });
        let req = match built {
            Ok(r) => r,
            Err(spare) => {
                self.wire.keep_spare(spare);
                self.wire.rst(id, frame::PROTOCOL_ERROR);
                return Ok(());
            }
        };
        s.head = req.method() == Method::HEAD;
        let fut = self.service.call(req);
        let mut slot = self.slot(fut);
        let polled = slot.as_mut().as_pin_mut().map(|f| f.poll(cx));
        match polled {
            Some(Poll::Ready(resp)) => {
                self.give_slot(slot);
                let done = respond(&mut self.wire, id, &mut s, resp, cx);
                if !(done && s.remote_done) {
                    self.finish_or_keep(id, s);
                }
            }
            _ => {
                s.handler = Some(slot);
                self.streams.insert(id, s);
            }
        }
        Ok(())
    }

    /// A stream whose answer went out: kept while its request body still arrives, else let go (the
    /// rest of a body nobody will read is refused).
    fn finish_or_keep(&mut self, id: u32, mut s: Stream<S::Future, S::Body>) {
        if s.local_done && !s.remote_done {
            self.wire.rst(id, frame::NO_ERROR);
            s.remote_done = true;
        }
        if s.local_done && s.remote_done {
            self.forget(s);
        } else {
            self.streams.insert(id, s);
        }
    }

    /// Polls streams whose handlers wait or whose responses stream: whether any moved.
    fn poll_streams(&mut self, cx: &mut Context<'_>) -> bool {
        let mut moved = false;
        let Conn { wire, streams, slots, .. } = self;
        let mut freed = Vec::new();
        let before = wire.wbuf.len();
        streams.retain(|&id, s| {
            if let Some(slot) = &mut s.handler {
                let polled = slot.as_mut().as_pin_mut().map(|f| f.poll(cx));
                match polled {
                    Some(Poll::Ready(resp)) => {
                        if let Some(slot) = s.handler.take() {
                            freed.push(slot);
                        }
                        moved = true;
                        respond(wire, id, s, resp, cx);
                    }
                    Some(Poll::Pending) => {}
                    None => s.handler = None,
                }
            } else if let Some(mut out) = s.out.take() {
                let mut body = out.body.take();
                let done = match &mut body {
                    Some(b) => pump(wire, id, s, b, &mut out, cx),
                    None => pump_held(wire, id, s, &mut out),
                };
                if done {
                    s.local_done = true;
                    moved = true;
                } else {
                    out.body = body;
                    s.out = Some(out);
                }
            }
            // A request body's reader took some: its window opens again.
            if let Some(b) = &mut s.body_in {
                let taken = b.feed.taken_since(cx, &mut b.mark);
                b.owed += taken;
                wire.owed += taken;
                let half = (wire.cfg.stream_window / 2) as usize;
                if b.owed >= half && !s.remote_done {
                    frame::put_window_update(wire.wbuf.tail(), id, b.owed as u32);
                    b.window += b.owed as i64;
                    b.owed = 0;
                }
            }
            if s.local_done && !s.remote_done && s.handler.is_none() {
                frame::put_rst(wire.wbuf.tail(), id, frame::NO_ERROR);
                s.remote_done = true;
            }
            let keep = !(s.local_done && s.remote_done);
            if !keep && let Some(b) = s.body_in.take() {
                // What arrived and nobody will read goes back to the connection's window.
                wire.owed += b.feed.buffered();
            }
            keep
        });
        for mut slot in freed {
            slot.set(None);
            if slots.len() < 8 {
                slots.push(slot);
            }
        }
        moved || wire.wbuf.len() != before
    }

    /// The connection's window opens as bodies are taken.
    fn give_back(&mut self) {
        let w = &mut self.wire;
        if w.owed >= (w.cfg.connection_window / 2) as usize || (w.owed > 0 && w.recv_window < i64::from(w.cfg.connection_window) / 2) {
            frame::put_window_update(w.wbuf.tail(), 0, w.owed as u32);
            w.recv_window += w.owed as i64;
            w.owed = 0;
        }
    }

    /// Ends the connection with GOAWAY.
    fn go_away(&mut self, code: u32) {
        frame::put_goaway(self.wire.wbuf.tail(), self.last_stream, code);
        self.going_away = true;
        let streams = std::mem::take(&mut self.streams);
        for (_, s) in streams {
            self.forget(s);
        }
    }

    /// Keep-alive: a ping for a quiet connection, and the end of one whose ping went unanswered.
    fn poll_keep_alive(&mut self, cx: &mut Context<'_>) -> Result<(), Error> {
        let Some(every) = self.wire.cfg.keep_alive_interval else { return Ok(()) };
        if let Some(sent) = self.ping_sent
            && sent.elapsed() > self.wire.cfg.keep_alive_timeout
        {
            return Err(Error::new(crate::Kind::TimedOut));
        }
        loop {
            let timer = self.keep_alive.get_or_insert_with(|| Sleep::after(every));
            if timer.poll_expired(cx).is_pending() {
                return Ok(());
            }
            self.keep_alive = None;
            if self.heard.elapsed() >= every && self.ping_sent.is_none() {
                frame::put_ping(self.wire.wbuf.tail(), false, *b"kilnping");
                self.ping_sent = Some(Instant::now());
            }
        }
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            let mut busy = false;
            self.wire.date = date::now();
            while self.wire.wbuf.len() < HIGH_WATER {
                match self.wire.next_frame() {
                    Ok(Some((h, payload))) => {
                        busy = true;
                        if let Err(ConnError(code)) = self.on_frame(h, payload, cx) {
                            self.go_away(code);
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(ConnError(code)) => {
                        self.go_away(code);
                        break;
                    }
                }
            }
            if busy {
                self.heard = Instant::now();
            }
            if !self.streams.is_empty() {
                busy |= self.poll_streams(cx);
            }
            self.give_back();
            if let Err(e) = self.poll_keep_alive(cx) {
                return Poll::Ready(Err(e));
            }
            if !self.wire.wbuf.is_empty() {
                match self.wire.poll_flush(cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending if self.wire.wbuf.len() >= HIGH_WATER => return Poll::Pending,
                    Poll::Pending => {}
                }
            }
            if self.going_away && self.streams.is_empty() && self.wire.wbuf.is_empty() {
                return Poll::Ready(Ok(()));
            }
            if self.wire.eof {
                // The client is gone: so is every request it made.
                let streams = std::mem::take(&mut self.streams);
                for (_, s) in streams {
                    self.forget(s);
                }
                return Poll::Ready(Ok(()));
            }
            if self.wire.rbuf.is_empty() && self.wire.wbuf.is_empty() {
                // Idle: the connection holds no buffer.
                self.wire.wbuf.release();
                if self.wire.rbuf.capacity() > 0 && self.streams.is_empty() {
                    self.wire.rbuf = BytesMut::new();
                }
            }
            match poll_read_into(&mut self.wire.io, cx, &mut self.wire.rbuf, SCRATCH) {
                Poll::Ready(Ok(0)) => self.wire.eof = true,
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(Error::io(e))),
                Poll::Pending if busy => {}
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Sends what a finished body left held back: whether it's all sent.
fn pump_held<I, F, B>(wire: &mut Wire<I>, id: u32, s: &mut Stream<F, B>, out: &mut Out<B>) -> bool {
    if !out.held.is_empty() {
        let held = std::mem::take(&mut out.held);
        let end = out.trailers.is_none();
        out.held = wire.data(id, &mut s.send_window, held, end);
        if !out.held.is_empty() {
            return false;
        }
        if end {
            return true;
        }
    }
    match out.trailers.take() {
        Some(t) => trailers(wire, id, &t),
        None => wire.head(0, frame::DATA, frame::END_STREAM, id),
    }
    true
}

async fn serve<I, S>(cfg: Builder, io: I, read: Bytes, service: S) -> Result<(), Error>
where
    I: AsyncRead + AsyncWrite + Unpin,
    S: Service<Body: Unpin>,
{
    let mut wire = Wire {
        io,
        rbuf: BytesMut::from(&read[..]),
        wbuf: WriteBuf::default(),
        eof: false,
        decoder: hpack::Decoder::new(4096),
        encoder: hpack::Encoder::new(),
        peer_max_frame: frame::DEFAULT_MAX_FRAME as usize,
        peer_window: frame::DEFAULT_WINDOW,
        send_window: frame::DEFAULT_WINDOW,
        recv_window: i64::from(cfg.connection_window),
        owed: 0,
        cfg,
        spare: None,
        acked: false,
        date: date::now(),
    };
    // Our settings go first; the client's preface is checked as it arrives.
    frame::put_settings(
        wire.wbuf.tail(),
        &[(frame::MAX_CONCURRENT_STREAMS, cfg.max_concurrent_streams), (frame::INITIAL_WINDOW_SIZE, cfg.stream_window), (frame::MAX_HEADER_LIST_SIZE, cfg.max_header_list_size)],
    );
    if cfg.connection_window as i64 > frame::DEFAULT_WINDOW {
        frame::put_window_update(wire.wbuf.tail(), 0, cfg.connection_window - frame::DEFAULT_WINDOW as u32);
    }
    let preface = poll_fn(|cx| {
        loop {
            if wire.rbuf.len() >= frame::PREFACE.len() {
                if wire.rbuf.get(..frame::PREFACE.len()) != Some(&frame::PREFACE[..]) {
                    return Poll::Ready(Err(Error::parse(crate::Parse::Frame)));
                }
                wire.rbuf.advance(frame::PREFACE.len());
                return Poll::Ready(Ok(()));
            }
            if !frame::PREFACE.starts_with(&wire.rbuf) {
                return Poll::Ready(Err(Error::parse(crate::Parse::Frame)));
            }
            match poll_read_into(&mut wire.io, cx, &mut wire.rbuf, SCRATCH) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(Error::closed())),
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(Error::io(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await;
    preface?;
    let mut conn = Conn {
        wire,
        service,
        streams: HashMap::default(),
        last_stream: 0,
        block: None,
        started: false,
        going_away: false,
        slots: Vec::new(),
        resets: (Instant::now(), 0),
        heard: Instant::now(),
        keep_alive: None,
        ping_sent: None,
    };
    let out = poll_fn(|cx| conn.poll_drive(cx)).await;
    let _ = poll_fn(|cx| conn.wire.poll_flush(cx)).await;
    let _ = poll_fn(|cx| Pin::new(&mut conn.wire.io).poll_close(cx)).await;
    out
}
