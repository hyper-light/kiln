//! The HTTP/1.1 server connection (RFC 9112), one task per connection.
//!
//! Requests are taken from what was read, as many as arrived (pipelined ones too), and each is
//! answered as its handler finishes; answers gather in the write buffer and go out together when the
//! connection has nothing more to read, so a burst of requests costs one write. A handler is polled
//! in the connection's own task: one ready at once (most are) costs no task, no wake and no copy.
//!
//! While a handler works, or its response streams, the connection keeps reading: a request body
//! into its pipe as the reader wants it, and otherwise only to notice the client going away, which
//! ends the request (unless half-closed connections are allowed). A head must arrive whole within
//! the header timeout, counted from when the connection is ready for it, so an idle connection
//! closes too.

use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_io::{AsyncRead, AsyncWrite};
use http::{HeaderMap, Method, StatusCode};

use crate::body::{Feed, Incoming, Want};
use crate::buf::{SCRATCH, WriteBuf, poll_read_into};
use crate::error::{Kind, Parse};
use crate::h1::decode::{Decoded, Decoder};
use crate::h1::head::{self, Framing, RequestHead};
use crate::rt::Sleep;
use crate::service::{SendBody, Service};
use crate::upgrade::{self, Upgraded};
use crate::{Error, date};

/// A large response body goes out once this much of it gathered.
const FLUSH_AT: usize = 64 << 10;

/// How HTTP/1.1 connections are served.
#[derive(Clone, Copy, Debug)]
pub struct Builder {
    header_read_timeout: Option<Duration>,
    keep_alive: bool,
    max_head: usize,
    half_close: bool,
}

impl Default for Builder {
    fn default() -> Builder {
        Builder { header_read_timeout: Some(Duration::from_secs(30)), keep_alive: true, max_head: 64 << 10, half_close: false }
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    /// How long a request head may take to arrive, from when the connection is ready for it (`None`:
    /// forever). 30 seconds unless set.
    pub fn header_read_timeout(&mut self, timeout: Option<Duration>) -> &mut Builder {
        self.header_read_timeout = timeout;
        self
    }

    /// Whether a connection serves more than one request. On unless set.
    pub fn keep_alive(&mut self, on: bool) -> &mut Builder {
        self.keep_alive = on;
        self
    }

    /// The largest request head (64 KiB unless set): a larger one is answered `431` and the connection
    /// closed.
    pub fn max_head_size(&mut self, bytes: usize) -> &mut Builder {
        self.max_head = bytes.max(1024);
        self
    }

    /// Whether a client that closes its sending side keeps its request (off unless set: the request
    /// ends, as hyper's do).
    pub fn half_close(&mut self, on: bool) -> &mut Builder {
        self.half_close = on;
        self
    }

    /// Serves `io` until the client is done (or an error ends it).
    pub fn serve<I, S>(&self, io: I, service: S) -> impl Future<Output = Result<(), Error>> + use<I, S>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        S: Service,
    {
        serve(Conn::new(*self, io, Bytes::new()), service)
    }

    /// [`Builder::serve`], with what was read from `io` already (to tell what it speaks).
    pub fn serve_read<I, S>(&self, io: I, read: Bytes, service: S) -> impl Future<Output = Result<(), Error>> + use<I, S>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        S: Service,
    {
        serve(Conn::new(*self, io, read), service)
    }
}

/// A request body still arriving.
struct BodyIn {
    feed: Feed,
    decoder: Decoder,
    /// `100 Continue` goes out when the reader first asks for the body.
    send_continue: bool,
    /// The body was all read, or broke off.
    done: bool,
    /// The reader dropped it (the rest goes unread).
    gone: bool,
}

/// A response body's framing as it goes out.
struct Out {
    framing: Framing,
    left: u64,
    trailers: Option<Box<HeaderMap>>,
}

impl Out {
    fn new(framing: Framing) -> Out {
        Out { framing, left: if let Framing::Length(n) = framing { n } else { 0 }, trailers: None }
    }

    fn data(&mut self, w: &mut WriteBuf, data: Bytes) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        match self.framing {
            Framing::Empty => {}
            Framing::Length(_) => {
                let n = data.len() as u64;
                if n > self.left {
                    return Err(Error::body("the body is longer than its content-length"));
                }
                self.left -= n;
                w.push(data);
            }
            Framing::Chunked => {
                head::chunk_head(w.tail(), data.len() as u64);
                w.push(data);
                w.extend(b"\r\n");
            }
            Framing::Close => w.push(data),
        }
        Ok(())
    }

    fn finish(&mut self, w: &mut WriteBuf) -> Result<(), Error> {
        match self.framing {
            Framing::Chunked => head::last_chunk(w.tail(), self.trailers.as_deref()),
            Framing::Length(_) if self.left > 0 => return Err(Error::body("the body is shorter than its content-length")),
            _ => {}
        }
        Ok(())
    }
}

struct Conn<I> {
    io: I,
    rbuf: BytesMut,
    wbuf: WriteBuf,
    /// The client closed its sending side.
    eof: bool,
    cfg: Builder,
    /// The last response's header map, emptied, for the next request's headers to take.
    spare: Option<Box<HeaderMap>>,
}

impl<I> Conn<I> {
    fn new(cfg: Builder, io: I, read: Bytes) -> Conn<I> {
        Conn { io, rbuf: BytesMut::from(&read[..]), wbuf: WriteBuf::default(), eof: false, cfg, spare: None }
    }
}

impl<I: AsyncRead + AsyncWrite + Unpin> Conn<I> {
    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<Result<usize, Error>> {
        poll_read_into(&mut self.io, cx, &mut self.rbuf, SCRATCH).map_err(Error::io)
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.wbuf.poll_flush(&mut self.io, cx).map_err(Error::io)
    }

    /// The next request's head; `None` once the client is done. Answers gathered so far go out
    /// before the connection waits for more to read.
    fn poll_head(&mut self, cx: &mut Context<'_>, timer: &mut Option<Sleep>) -> Poll<Result<Option<RequestHead>, Error>> {
        loop {
            if let Some(h) = head::parse_request(&mut self.rbuf, self.cfg.max_head, &mut self.spare)? {
                return Poll::Ready(Ok(Some(h)));
            }
            if self.eof {
                return Poll::Ready(if self.rbuf.is_empty() { Ok(None) } else { Err(Error::closed()) });
            }
            if !self.wbuf.is_empty() {
                std::task::ready!(self.poll_flush(cx))?;
            }
            if self.rbuf.is_empty() {
                self.wbuf.release();
            }
            match self.poll_read(cx) {
                Poll::Ready(Ok(0)) => self.eof = true,
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    if let Some(t) = self.cfg.header_read_timeout {
                        let sleep = timer.get_or_insert_with(|| Sleep::after(t));
                        if sleep.poll_expired(cx).is_ready() {
                            return Poll::Ready(Err(Error::new(Kind::TimedOut)));
                        }
                    }
                    return Poll::Pending;
                }
            }
        }
    }

    /// Moves what the body's reader wants from the connection into its pipe: whether anything moved
    /// (the handler may go on). Otherwise it waits: for bytes to read, or for the reader to want more.
    fn pump(&mut self, cx: &mut Context<'_>, b: &mut BodyIn) -> Result<bool, Error> {
        let mut moved = false;
        while !b.done && !b.gone {
            match b.feed.want() {
                Want::Gone => {
                    b.gone = true;
                    break;
                }
                Want::Full => match b.feed.poll_want(cx) {
                    Poll::Pending => break,
                    Poll::Ready(_) => continue,
                },
                Want::More => {}
            }
            if b.send_continue {
                // The client waits for leave to send its body: given when the reader asks for it.
                if !b.feed.is_waiting() {
                    let _ = b.feed.poll_want(cx);
                    break;
                }
                b.send_continue = false;
                self.wbuf.extend(b"HTTP/1.1 100 Continue\r\n\r\n");
                if let Poll::Ready(Err(e)) = self.poll_flush(cx) {
                    b.feed.fail(e.share());
                    return Err(e);
                }
            }
            match b.decoder.decode(&mut self.rbuf) {
                Ok(Decoded::Data(chunk)) => {
                    b.feed.push(chunk);
                    moved = true;
                }
                Ok(Decoded::End(trailers)) => {
                    b.feed.end(trailers);
                    b.done = true;
                    moved = true;
                }
                Ok(Decoded::More) => {
                    if self.eof {
                        b.feed.fail(Error::closed());
                        b.done = true;
                        return Ok(true);
                    }
                    match self.poll_read(cx) {
                        Poll::Ready(Ok(0)) => self.eof = true,
                        Poll::Ready(Ok(_)) => {}
                        Poll::Ready(Err(e)) => {
                            b.feed.fail(e.share());
                            b.done = true;
                            return Err(e);
                        }
                        Poll::Pending => break,
                    }
                }
                Err(e) => {
                    // A body broken off: its reader learns why and answers; the connection ends after.
                    b.feed.fail(e);
                    b.done = true;
                    self.eof = true;
                    return Ok(true);
                }
            }
        }
        Ok(moved)
    }

    /// While a request is handled or answered: notices the client going away, which ends the request
    /// unless half-closed connections are allowed. What arrives meanwhile (the next requests) is kept.
    fn watch(&mut self, cx: &mut Context<'_>) -> Result<(), Error> {
        if self.eof || self.cfg.half_close {
            return Ok(());
        }
        while self.rbuf.len() < self.cfg.max_head {
            match self.poll_read(cx) {
                Poll::Ready(Ok(0)) => {
                    self.eof = true;
                    return Err(Error::closed());
                }
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(e)) => return Err(e),
                Poll::Pending => break,
            }
        }
        Ok(())
    }

    /// Polls the handler, feeding the request body meanwhile and sending what's answered already.
    fn poll_handle<F: Future>(&mut self, cx: &mut Context<'_>, mut fut: Pin<&mut F>, body: &mut Option<Box<BodyIn>>) -> Poll<Result<F::Output, Error>> {
        loop {
            if let Poll::Ready(resp) = fut.as_mut().poll(cx) {
                return Poll::Ready(Ok(resp));
            }
            let moved = match body {
                Some(b) if !b.done && !b.gone => self.pump(cx, b)?,
                _ => {
                    self.watch(cx)?;
                    false
                }
            };
            if moved {
                continue;
            }
            if !self.wbuf.is_empty()
                && let Poll::Ready(Err(e)) = self.poll_flush(cx)
            {
                return Poll::Ready(Err(e));
            }
            return Poll::Pending;
        }
    }

    /// Sends a response body, framed, as it comes; a body that pauses has what came so far sent.
    fn poll_send<B: SendBody>(&mut self, cx: &mut Context<'_>, mut body: Pin<&mut B>, out: &mut Out, body_in: &mut Option<Box<BodyIn>>) -> Poll<Result<(), Error>> {
        loop {
            if self.wbuf.len() >= FLUSH_AT {
                std::task::ready!(self.poll_flush(cx))?;
            }
            match body.as_mut().poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => out.data(&mut self.wbuf, data)?,
                    Err(frame) => {
                        if let Ok(t) = frame.into_trailers() {
                            out.trailers = Some(Box::new(t));
                        }
                    }
                },
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(Error::body(e))),
                Poll::Ready(None) => {
                    out.finish(&mut self.wbuf)?;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => {
                    if let Poll::Ready(Err(e)) = self.poll_flush(cx) {
                        return Poll::Ready(Err(e));
                    }
                    match body_in {
                        Some(b) if !b.done && !b.gone => {
                            if self.pump(cx, b)? {
                                continue;
                            }
                        }
                        _ => self.watch(cx)?,
                    }
                    return Poll::Pending;
                }
            }
        }
    }

    /// Answers a request that couldn't be read (the connection closes after).
    fn refuse(&mut self, e: &Error) {
        let status = match e.kind() {
            Kind::Parse(Parse::HeadTooLarge) => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Kind::Parse(Parse::Version) => StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            Kind::Parse(_) => StatusCode::BAD_REQUEST,
            _ => return,
        };
        let date = date::now();
        let _ = head::encode_response(self.wbuf.tail(), status, &HeaderMap::new(), Some(0), head::Answering { method: &Method::GET, version: http::Version::HTTP_11, keep_alive: false }, &date);
    }
}

/// What a request in hand still needs of the connection: kept small, because it lives in the
/// connection's future while the handler works (what a request is built of is taken apart in plain
/// functions, so none of it holds space in an idle connection).
struct Exchange {
    /// `HEAD` (no body goes out) or `CONNECT` (a tunnel may follow).
    head: bool,
    connect: bool,
    version: http::Version,
    keep_alive: bool,
    body_in: Option<Box<BodyIn>>,
    upgrade: Option<upgrade::Pending>,
}

/// Turns a head into the request the service is called with, and what the exchange needs.
fn begin<I: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<I>, h: RequestHead) -> (http::Request<Incoming>, Exchange) {
    let RequestHead { method, uri, version, headers, framing, keep_alive, expects_continue, upgrade: upgrading } = h;
    let (incoming, body_in) = match framing {
        Framing::Empty | Framing::Close => (Incoming::empty(), None),
        Framing::Length(n) if c.rbuf.len() as u64 >= n => (Incoming::whole(c.rbuf.split_to(n as usize).freeze()), None),
        Framing::Length(n) => {
            let (body, feed) = Incoming::piped(Some(n));
            (body, Some(Box::new(BodyIn { feed, decoder: Decoder::length(n), send_continue: expects_continue, done: false, gone: false })))
        }
        Framing::Chunked => {
            let (body, feed) = Incoming::piped(None);
            (body, Some(Box::new(BodyIn { feed, decoder: Decoder::chunked(), send_continue: expects_continue, done: false, gone: false })))
        }
    };
    let ex = Exchange { head: method == Method::HEAD, connect: method == Method::CONNECT, version, keep_alive: keep_alive && c.cfg.keep_alive, body_in, upgrade: None };
    let mut req = http::Request::new(incoming);
    *req.method_mut() = method;
    *req.uri_mut() = uri;
    *req.version_mut() = version;
    *req.headers_mut() = headers;
    let mut ex = ex;
    if upgrading {
        let (pending, on) = upgrade::pair();
        req.extensions_mut().insert(on);
        ex.upgrade = Some(pending);
    }
    (req, ex)
}

/// Encodes a response's head: its body, and how that goes out (`None`: it doesn't).
fn answer<I: AsyncRead + AsyncWrite + Unpin, B: SendBody>(c: &mut Conn<I>, resp: http::Response<B>, ex: &mut Exchange) -> (B, Option<Out>, StatusCode) {
    let (parts, body) = resp.into_parts();
    let date = date::now();
    let method = if ex.head {
        &Method::HEAD
    } else if ex.connect {
        &Method::CONNECT
    } else {
        &Method::GET
    };
    let framed = head::encode_response(c.wbuf.tail(), parts.status, &parts.headers, body.size_hint().exact(), head::Answering { method, version: ex.version, keep_alive: ex.keep_alive }, &date);
    ex.keep_alive &= !framed.close;
    if parts.headers.capacity() > 0 {
        c.spare = Some(Box::new(parts.headers));
    }
    let out = (framed.framing != Framing::Empty).then(|| Out::new(framed.framing));
    (body, out, parts.status)
}

async fn serve<I, S>(mut c: Conn<I>, service: S) -> Result<(), Error>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Service,
{
    // The handler's future, kept between requests of a burst and let go when the connection idles.
    let mut slot: Option<Pin<Box<Option<S::Future>>>> = None;
    let outcome = loop {
        let head = {
            let mut timer = None;
            let head = poll_fn(|cx| c.poll_head(cx, &mut timer)).await;
            match head {
                Ok(Some(h)) => h,
                Ok(None) => break Ok(()),
                Err(e) => {
                    c.refuse(&e);
                    break Err(e);
                }
            }
        };
        let (req, mut ex) = begin(&mut c, head);
        let (body, out, status) = {
            let fut = service.call(req);
            let handler = match &mut slot {
                Some(s) => {
                    s.set(Some(fut));
                    s
                }
                None => slot.insert(Box::pin(Some(fut))),
            };
            let resp = poll_fn(|cx| match handler.as_mut().as_pin_mut() {
                Some(fut) => c.poll_handle(cx, fut, &mut ex.body_in),
                None => Poll::Ready(Err(Error::canceled())),
            })
            .await;
            handler.set(None);
            match resp {
                Ok(r) => answer(&mut c, r, &mut ex),
                Err(e) => break Err(e),
            }
        };
        if let Some(mut out) = out {
            let mut body = pin!(body);
            if let Err(e) = poll_fn(|cx| c.poll_send(cx, body.as_mut(), &mut out, &mut ex.body_in)).await {
                break Err(e);
            }
        } else {
            drop(body);
        }
        let switching = status == StatusCode::SWITCHING_PROTOCOLS || (ex.connect && status.is_success());
        if switching && let Some(pending) = ex.upgrade.take() {
            if let Err(e) = poll_fn(|cx| c.poll_flush(cx)).await {
                break Err(e);
            }
            let Conn { io, rbuf, .. } = c;
            pending.fulfill(Upgraded::new(io, rbuf.freeze()));
            return Ok(());
        }
        // An unread rest of a body would read as the next request: the connection ends instead.
        if ex.body_in.as_ref().is_some_and(|b| !b.done) {
            ex.keep_alive = false;
        }
        if !ex.keep_alive {
            break Ok(());
        }
        if c.rbuf.is_empty() {
            // Going idle: the connection holds nothing it doesn't need, not even its read buffer.
            slot = None;
            c.spare = None;
            c.rbuf = BytesMut::new();
        }
    };
    let _ = poll_fn(|cx| c.poll_flush(cx)).await;
    let _ = poll_fn(|cx| Pin::new(&mut c.io).poll_close(cx)).await;
    outcome
}
