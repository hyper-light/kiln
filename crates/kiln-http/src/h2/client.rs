//! The HTTP/2 client connection: [`handshake`] gives a [`SendRequest`] (clone it to send from
//! many places; requests go out at once, each its own stream) and the [`Connection`] that does the
//! reading and writing (run it as a task). Responses' bodies stream through their [`Incoming`],
//! flow-controlled by how fast each is read.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use bytes::{Buf as _, Bytes, BytesMut};
use futures_channel::{mpsc, oneshot};
use futures_core::Stream as _;
use futures_io::{AsyncRead, AsyncWrite};
use http::header::{self, HeaderMap, HeaderValue};
use http::{StatusCode, Version};

use super::frame::{self, Head};
use super::hpack::{self, Field, Name};
use crate::Error;
use crate::body::{Feed, Incoming, Want};
use crate::buf::{SCRATCH, WriteBuf, poll_read_into};
use crate::service::SendBody;

type Answer = oneshot::Sender<Result<http::Response<Incoming>, Error>>;

struct Job<B> {
    req: http::Request<B>,
    answer: Answer,
}

#[derive(Default)]
struct Shared {
    closed: Mutex<bool>,
}

/// Sends requests over an HTTP/2 connection; clones share it.
pub struct SendRequest<B> {
    jobs: mpsc::UnboundedSender<Job<B>>,
    shared: Arc<Shared>,
}

impl<B> Clone for SendRequest<B> {
    fn clone(&self) -> Self {
        SendRequest { jobs: self.jobs.clone(), shared: Arc::clone(&self.shared) }
    }
}

impl<B> std::fmt::Debug for SendRequest<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendRequest").field("closed", &self.is_closed()).finish()
    }
}

impl<B> SendRequest<B> {
    pub fn is_closed(&self) -> bool {
        *self.shared.closed.lock().unwrap_or_else(PoisonError::into_inner) || self.jobs.is_closed()
    }

    /// Always ready while open: HTTP/2 sends requests side by side.
    pub fn is_ready(&self) -> bool {
        !self.is_closed()
    }

    pub async fn ready(&mut self) -> Result<(), Error> {
        if self.is_closed() { Err(Error::closed()) } else { Ok(()) }
    }

    /// Sends `req` on a stream of its own: its response once its head arrived.
    pub fn send_request(&mut self, req: http::Request<B>) -> impl Future<Output = Result<http::Response<Incoming>, Error>> + use<B> {
        let (answer, rx) = oneshot::channel();
        let sent = self.jobs.unbounded_send(Job { req, answer }).is_ok();
        async move {
            if !sent {
                return Err(Error::canceled());
            }
            match rx.await {
                Ok(outcome) => outcome,
                Err(oneshot::Canceled) => Err(Error::closed()),
            }
        }
    }
}

/// A client connection over `io`, which speaks HTTP/2 (agreed by ALPN, or known beforehand).
pub async fn handshake<I, B>(io: I) -> Result<(SendRequest<B>, Connection<I, B>), Error>
where
    I: AsyncRead + AsyncWrite + Unpin,
    B: SendBody + Unpin,
{
    let (tx, rx) = mpsc::unbounded();
    let shared = Arc::new(Shared::default());
    let mut wbuf = WriteBuf::default();
    wbuf.extend(frame::PREFACE);
    frame::put_settings(wbuf.tail(), &[(frame::ENABLE_PUSH, 0), (frame::INITIAL_WINDOW_SIZE, STREAM_WINDOW as u32)]);
    frame::put_window_update(wbuf.tail(), 0, (CONN_WINDOW - frame::DEFAULT_WINDOW) as u32);
    let conn = Connection {
        io,
        rbuf: BytesMut::new(),
        wbuf,
        eof: false,
        decoder: hpack::Decoder::new(4096),
        encoder: hpack::Encoder::new(),
        jobs: rx,
        shared: Arc::clone(&shared),
        streams: HashMap::new(),
        queued: std::collections::VecDeque::new(),
        next_id: 1,
        peer_max_streams: 100,
        peer_max_frame: frame::DEFAULT_MAX_FRAME as usize,
        peer_window: frame::DEFAULT_WINDOW,
        send_window: frame::DEFAULT_WINDOW,
        recv_window: CONN_WINDOW,
        owed: 0,
        block: None,
        going_away: None,
        jobs_done: false,
        fields: Vec::new(),
    };
    Ok((SendRequest { jobs: tx, shared }, conn))
}

const STREAM_WINDOW: i64 = 1 << 20;
const CONN_WINDOW: i64 = 16 << 20;

/// A request body going out.
struct Out<B> {
    body: Option<B>,
    held: Bytes,
    trailers: Option<HeaderMap>,
    ended: bool,
}

struct Stream<B> {
    out: Option<Out<B>>,
    send_window: i64,
    /// Who waits for the response head.
    answer: Option<Answer>,
    /// The response body arriving.
    feed: Option<Feed>,
    recv_window: i64,
    mark: usize,
    owed: usize,
    remote_done: bool,
    local_done: bool,
}

struct Block {
    stream: u32,
    end_stream: bool,
    buf: BytesMut,
}

/// The reading and writing of an HTTP/2 client connection: run it as a task.
pub struct Connection<I, B> {
    io: I,
    rbuf: BytesMut,
    wbuf: WriteBuf,
    eof: bool,
    decoder: hpack::Decoder,
    encoder: hpack::Encoder,
    jobs: mpsc::UnboundedReceiver<Job<B>>,
    shared: Arc<Shared>,
    streams: HashMap<u32, Stream<B>>,
    /// Requests waiting for the server to allow another stream.
    queued: std::collections::VecDeque<Job<B>>,
    next_id: u32,
    peer_max_streams: usize,
    peer_max_frame: usize,
    peer_window: i64,
    send_window: i64,
    recv_window: i64,
    owed: usize,
    block: Option<Block>,
    /// The server sent GOAWAY with this last stream.
    going_away: Option<u32>,
    jobs_done: bool,
    fields: Vec<Field>,
}

/// A connection error.
struct ConnError(u32);

impl From<hpack::DecodeError> for ConnError {
    fn from(_: hpack::DecodeError) -> ConnError {
        ConnError(frame::COMPRESSION_ERROR)
    }
}

impl<I, B> Connection<I, B>
where
    I: AsyncRead + AsyncWrite + Unpin,
    B: SendBody + Unpin,
{
    /// Opens a stream for a request: its head written, its body to follow.
    fn open(&mut self, job: Job<B>) {
        let Job { req, answer } = job;
        let id = self.next_id;
        self.next_id += 2;
        let (parts, body) = req.into_parts();
        let empty = http_body::Body::is_end_stream(&body) || http_body::Body::size_hint(&body).exact() == Some(0);
        let tail = self.wbuf.tail();
        let start = tail.len();
        tail.extend_from_slice(&[0; 9]);
        self.encoder.begin(tail);
        self.encoder.pseudo(2, parts.method.as_str().as_bytes(), tail);
        if parts.method != http::Method::CONNECT {
            self.encoder.pseudo(6, parts.uri.scheme_str().unwrap_or("http").as_bytes(), tail);
            self.encoder.pseudo(4, parts.uri.path_and_query().map_or("/", |p| p.as_str()).as_bytes(), tail);
        }
        let authority = parts.uri.authority().map(|a| a.as_str().as_bytes().to_vec()).or_else(|| parts.headers.get(header::HOST).map(|h| h.as_bytes().to_vec()));
        if let Some(a) = authority {
            self.encoder.pseudo(1, &a, tail);
        }
        for (name, value) in &parts.headers {
            if *name == header::HOST
                || *name == header::CONNECTION
                || *name == header::TRANSFER_ENCODING
                || *name == header::UPGRADE
                || name.as_str() == "keep-alive"
                || name.as_str() == "proxy-connection"
            {
                continue;
            }
            self.encoder.header(name, value, tail);
        }
        let len = tail.len() - start - 9;
        let flags = frame::END_HEADERS | if empty { frame::END_STREAM } else { 0 };
        let mut h = Vec::with_capacity(9);
        frame::put_head(&mut h, len, frame::HEADERS, flags, id);
        if let Some(slot) = tail.get_mut(start..start + 9) {
            slot.copy_from_slice(&h);
        }
        let out = (!empty).then_some(Out { body: Some(body), held: Bytes::new(), trailers: None, ended: false });
        self.streams.insert(id, Stream { out, send_window: self.peer_window, answer: Some(answer), feed: None, recv_window: STREAM_WINDOW, mark: 0, owed: 0, remote_done: false, local_done: empty });
    }

    /// A request's trailers: a header block that ends the stream.
    fn trailers(&mut self, id: u32, t: &HeaderMap) {
        let tail = self.wbuf.tail();
        let start = tail.len();
        tail.extend_from_slice(&[0; 9]);
        self.encoder.begin(tail);
        for (name, value) in t {
            self.encoder.header(name, value, tail);
        }
        let len = tail.len() - start - 9;
        let mut h = Vec::with_capacity(9);
        frame::put_head(&mut h, len, frame::HEADERS, frame::END_STREAM | frame::END_HEADERS, id);
        if let Some(slot) = tail.get_mut(start..start + 9) {
            slot.copy_from_slice(&h);
        }
    }

    fn data(&mut self, id: u32, mut data: Bytes, end: bool) -> Bytes {
        let Some(s) = self.streams.get_mut(&id) else { return Bytes::new() };
        loop {
            let room = s.send_window.min(self.send_window).max(0) as usize;
            let n = data.len().min(room).min(self.peer_max_frame);
            if n == 0 && !data.is_empty() {
                return data;
            }
            let last = n == data.len();
            let piece = data.split_to(n);
            frame::put_head(self.wbuf.tail(), n, frame::DATA, if last && end { frame::END_STREAM } else { 0 }, id);
            self.wbuf.push(piece);
            s.send_window -= n as i64;
            self.send_window -= n as i64;
            if last {
                return data;
            }
        }
    }

    /// Moves request bodies out as flow control allows; gives response bodies' windows back as
    /// they're read.
    fn pump(&mut self, cx: &mut Context<'_>) {
        let ids: Vec<u32> = self.streams.keys().copied().collect();
        for id in ids {
            // Out.
            let mut out = self.streams.get_mut(&id).and_then(|s| s.out.take());
            if let Some(o) = &mut out {
                loop {
                    if !o.held.is_empty() {
                        let held = std::mem::take(&mut o.held);
                        let end = o.ended && o.trailers.is_none();
                        o.held = self.data(id, held, end);
                        if !o.held.is_empty() {
                            break;
                        }
                        if end {
                            o.body = None;
                            break;
                        }
                    }
                    if o.ended {
                        match o.trailers.take() {
                            Some(t) => self.trailers(id, &t),
                            None => frame::put_head(self.wbuf.tail(), 0, frame::DATA, frame::END_STREAM, id),
                        }
                        o.body = None;
                        break;
                    }
                    let Some(body) = &mut o.body else { break };
                    match http_body::Body::poll_frame(Pin::new(body), cx) {
                        Poll::Ready(Some(Ok(f))) => match f.into_data() {
                            Ok(d) => o.held = d,
                            Err(f) => {
                                if let Ok(t) = f.into_trailers() {
                                    o.trailers = Some(t);
                                }
                            }
                        },
                        Poll::Ready(Some(Err(_))) => {
                            frame::put_rst(self.wbuf.tail(), id, frame::CANCEL);
                            o.body = None;
                            o.ended = true;
                            o.held = Bytes::new();
                            break;
                        }
                        Poll::Ready(None) => o.ended = true,
                        Poll::Pending => break,
                    }
                    if o.held.is_empty() && !o.ended {
                        continue;
                    }
                    if self.wbuf.len() >= 256 << 10 {
                        break;
                    }
                }
            }
            let Some(s) = self.streams.get_mut(&id) else { continue };
            match out {
                Some(o) if o.body.is_some() || !o.held.is_empty() => s.out = Some(o),
                Some(_) => s.local_done = true,
                None => {}
            }
            // In.
            if let Some(feed) = &s.feed {
                let taken = feed.taken_since(cx, &mut s.mark);
                s.owed += taken;
                self.owed += taken;
                if s.owed as i64 >= STREAM_WINDOW / 2 && !s.remote_done {
                    frame::put_window_update(self.wbuf.tail(), id, s.owed as u32);
                    s.recv_window += s.owed as i64;
                    s.owed = 0;
                }
                if feed.want() == Want::Gone && !s.remote_done {
                    // Nobody reads the rest: the stream is refused.
                    frame::put_rst(self.wbuf.tail(), id, frame::CANCEL);
                    self.owed += feed.buffered();
                    s.remote_done = true;
                    s.local_done = true;
                }
            }
            if s.local_done && s.remote_done && s.answer.is_none() {
                self.streams.remove(&id);
            }
        }
        if self.owed as i64 >= CONN_WINDOW / 2 {
            frame::put_window_update(self.wbuf.tail(), 0, self.owed as u32);
            self.recv_window += self.owed as i64;
            self.owed = 0;
        }
    }

    fn on_frame(&mut self, h: Head, payload: Bytes) -> Result<(), ConnError> {
        if let Some(block) = &mut self.block {
            if h.kind != frame::CONTINUATION || h.stream != block.stream {
                return Err(ConnError(frame::PROTOCOL_ERROR));
            }
            block.buf.extend_from_slice(&payload);
            if block.buf.len() > 1 << 20 {
                return Err(ConnError(frame::ENHANCE_YOUR_CALM));
            }
            if h.flags & frame::END_HEADERS != 0
                && let Some(b) = self.block.take()
            {
                return self.on_block(b.stream, b.end_stream, b.buf.freeze());
            }
            return Ok(());
        }
        match h.kind {
            frame::DATA => {
                let len = payload.len();
                if len as i64 > self.recv_window {
                    return Err(ConnError(frame::FLOW_CONTROL_ERROR));
                }
                self.recv_window -= len as i64;
                let mut data = payload;
                if h.flags & frame::PADDED != 0 {
                    let pad = usize::from(*data.first().ok_or(ConnError(frame::PROTOCOL_ERROR))?);
                    if pad >= data.len() {
                        return Err(ConnError(frame::PROTOCOL_ERROR));
                    }
                    data.advance(1);
                    data.truncate(data.len() - pad);
                }
                self.owed += len - data.len();
                let end = h.flags & frame::END_STREAM != 0;
                let Some(s) = self.streams.get_mut(&h.stream) else {
                    self.owed += data.len();
                    return Ok(());
                };
                s.recv_window -= len as i64;
                match &s.feed {
                    Some(feed) if feed.want() != Want::Gone => feed.push(data),
                    _ => self.owed += data.len(),
                }
                if end {
                    if let Some(feed) = &s.feed {
                        feed.end(None);
                    }
                    s.remote_done = true;
                }
                Ok(())
            }
            frame::HEADERS => {
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
                self.on_block(h.stream, end_stream, block)
            }
            frame::RST_STREAM => {
                let code = frame::u32_at(&payload, 0).ok_or(ConnError(frame::FRAME_SIZE_ERROR))?;
                if let Some(mut s) = self.streams.remove(&h.stream) {
                    if let Some(a) = s.answer.take() {
                        let _ = a.send(Err(if code == frame::REFUSED_STREAM { Error::canceled() } else { Error::h2(code) }));
                    }
                    if let Some(feed) = s.feed.take() {
                        feed.fail(Error::h2(code));
                    }
                }
                Ok(())
            }
            frame::SETTINGS => {
                if h.flags & frame::ACK != 0 {
                    return Ok(());
                }
                let (settings, rest) = payload.as_chunks::<6>();
                if !rest.is_empty() {
                    return Err(ConnError(frame::FRAME_SIZE_ERROR));
                }
                for &[a, b, c, d, e, f] in settings {
                    let v = u32::from_be_bytes([c, d, e, f]);
                    match u16::from_be_bytes([a, b]) {
                        frame::HEADER_TABLE_SIZE => self.encoder.set_max(v as usize),
                        frame::MAX_CONCURRENT_STREAMS => self.peer_max_streams = v as usize,
                        frame::INITIAL_WINDOW_SIZE => {
                            let v = i64::from(v);
                            if v > frame::MAX_WINDOW {
                                return Err(ConnError(frame::FLOW_CONTROL_ERROR));
                            }
                            let delta = v - self.peer_window;
                            self.peer_window = v;
                            for s in self.streams.values_mut() {
                                s.send_window += delta;
                            }
                        }
                        frame::MAX_FRAME_SIZE => {
                            if !(frame::DEFAULT_MAX_FRAME..=16_777_215).contains(&v) {
                                return Err(ConnError(frame::PROTOCOL_ERROR));
                            }
                            self.peer_max_frame = v as usize;
                        }
                        _ => {}
                    }
                }
                frame::put_head(self.wbuf.tail(), 0, frame::SETTINGS, frame::ACK, 0);
                Ok(())
            }
            frame::PING => {
                let data: [u8; 8] = payload.as_ref().try_into().map_err(|_| ConnError(frame::FRAME_SIZE_ERROR))?;
                if h.flags & frame::ACK == 0 {
                    frame::put_ping(self.wbuf.tail(), true, data);
                }
                Ok(())
            }
            frame::GOAWAY => {
                let last = frame::u32_at(&payload, 0).ok_or(ConnError(frame::FRAME_SIZE_ERROR))? & 0x7fff_ffff;
                self.going_away = Some(last);
                // Streams past `last` were never taken: their requests may go elsewhere.
                let refused: Vec<u32> = self.streams.keys().copied().filter(|id| *id > last).collect();
                for id in refused {
                    if let Some(mut s) = self.streams.remove(&id)
                        && let Some(a) = s.answer.take()
                    {
                        let _ = a.send(Err(Error::canceled()));
                    }
                }
                Ok(())
            }
            frame::WINDOW_UPDATE => {
                let inc = i64::from(frame::u32_at(&payload, 0).ok_or(ConnError(frame::FRAME_SIZE_ERROR))? & 0x7fff_ffff);
                if h.stream == 0 {
                    self.send_window += inc;
                } else if let Some(s) = self.streams.get_mut(&h.stream) {
                    s.send_window += inc;
                }
                Ok(())
            }
            frame::PUSH_PROMISE | frame::CONTINUATION => Err(ConnError(frame::PROTOCOL_ERROR)),
            _ => Ok(()),
        }
    }

    fn on_block(&mut self, id: u32, end_stream: bool, block: Bytes) -> Result<(), ConnError> {
        let mut fields = std::mem::take(&mut self.fields);
        fields.clear();
        self.decoder.decode(&block, 1 << 20, &mut fields)?;
        let Some(s) = self.streams.get_mut(&id) else {
            self.fields = fields;
            return Ok(());
        };
        let mut status = None;
        let mut headers = HeaderMap::with_capacity(fields.len());
        for f in fields.drain(..) {
            match f.name {
                Name::Status => status = std::str::from_utf8(&f.value).ok().and_then(|v| v.parse::<u16>().ok()).and_then(|v| StatusCode::from_u16(v).ok()),
                Name::Header(n) => {
                    if let Ok(v) = HeaderValue::from_maybe_shared(f.value) {
                        headers.append(n, v);
                    }
                }
                _ => {}
            }
        }
        self.fields = fields;
        let Some(answer) = s.answer.take() else {
            // Trailers.
            if let Some(feed) = &s.feed {
                feed.end(Some(headers));
            }
            s.remote_done = true;
            return Ok(());
        };
        let Some(status) = status else {
            let _ = answer.send(Err(Error::parse(crate::Parse::Frame)));
            frame::put_rst(self.wbuf.tail(), id, frame::PROTOCOL_ERROR);
            s.remote_done = true;
            s.local_done = true;
            return Ok(());
        };
        if status.is_informational() {
            s.answer = Some(answer);
            return Ok(());
        }
        let length = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        let body = if end_stream {
            s.remote_done = true;
            Incoming::empty()
        } else {
            let (body, feed) = Incoming::piped(length);
            s.feed = Some(feed);
            body
        };
        let mut resp = http::Response::new(body);
        *resp.status_mut() = status;
        *resp.version_mut() = Version::HTTP_2;
        *resp.headers_mut() = headers;
        let _ = answer.send(Ok(resp));
        Ok(())
    }

    fn fail_all(&mut self, e: &Error) {
        for (_, mut s) in self.streams.drain() {
            if let Some(a) = s.answer.take() {
                let _ = a.send(Err(e.share()));
            }
            if let Some(feed) = s.feed.take() {
                feed.fail(e.share());
            }
        }
        for job in self.queued.drain(..) {
            let _ = job.answer.send(Err(Error::canceled()));
        }
        *self.shared.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            let mut busy = false;
            // New requests, as many as the server allows open.
            while !self.jobs_done {
                match Pin::new(&mut self.jobs).poll_next(cx) {
                    Poll::Ready(Some(job)) => self.queued.push_back(job),
                    Poll::Ready(None) => self.jobs_done = true,
                    Poll::Pending => break,
                }
            }
            while self.going_away.is_none() && self.streams.len() < self.peer_max_streams.max(1) && self.next_id < (1 << 31) {
                let Some(job) = self.queued.pop_front() else { break };
                self.open(job);
                busy = true;
            }
            while let Some(h) = frame::head(&self.rbuf) {
                if h.len > 1 << 24 || self.rbuf.len() < 9 + h.len {
                    break;
                }
                self.rbuf.advance(9);
                let payload = self.rbuf.split_to(h.len).freeze();
                busy = true;
                if let Err(ConnError(code)) = self.on_frame(h, payload) {
                    frame::put_goaway(self.wbuf.tail(), 0, code);
                    let _ = self.wbuf.poll_flush(&mut self.io, cx);
                    let e = Error::h2(code);
                    self.fail_all(&e);
                    return Poll::Ready(Err(e));
                }
            }
            self.pump(cx);
            if !self.wbuf.is_empty() {
                match self.wbuf.poll_flush(&mut self.io, cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => {
                        let e = Error::io(e);
                        self.fail_all(&e);
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {}
                }
            }
            let finished = self.streams.is_empty() && self.queued.is_empty();
            if (self.jobs_done || self.going_away.is_some()) && finished && self.wbuf.is_empty() {
                self.fail_all(&Error::closed());
                let _ = Pin::new(&mut self.io).poll_close(cx);
                return Poll::Ready(Ok(()));
            }
            if self.eof {
                let e = Error::closed();
                self.fail_all(&e);
                return Poll::Ready(if finished { Ok(()) } else { Err(e) });
            }
            match poll_read_into(&mut self.io, cx, &mut self.rbuf, SCRATCH) {
                Poll::Ready(Ok(0)) => self.eof = true,
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(e)) => {
                    let e = Error::io(e);
                    self.fail_all(&e);
                    return Poll::Ready(Err(e));
                }
                Poll::Pending if busy => {}
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<I, B> Future for Connection<I, B>
where
    I: AsyncRead + AsyncWrite + Unpin,
    B: SendBody + Unpin,
{
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_drive(cx)
    }
}
