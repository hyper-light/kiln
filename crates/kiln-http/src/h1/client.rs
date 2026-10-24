//! The HTTP/1.1 client connection: [`handshake`] gives a [`SendRequest`] to send requests with and the
//! [`Connection`] that does the reading and writing (run it as a task). One request at a time, kept
//! alive between them; a response's body streams through its [`Incoming`] while the connection reads
//! it, and the connection takes the next request once that body is done.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use bytes::BytesMut;
use futures_channel::{mpsc, oneshot};
use futures_core::Stream as _;
use futures_io::{AsyncRead, AsyncWrite};
use http::{Method, StatusCode};

use crate::Error;
use crate::body::{Feed, Incoming, Want};
use crate::buf::{SCRATCH, WriteBuf, poll_read_into};
use crate::h1::decode::{Decoded, Decoder};
use crate::h1::head::{self, Framing, ResponseHead};
use crate::service::SendBody;
use crate::upgrade::{self, Upgraded};

type Answer = oneshot::Sender<Result<http::Response<Incoming>, Error>>;

struct Job<B> {
    req: http::Request<B>,
    answer: Answer,
}

#[derive(Default)]
struct State {
    /// A request is queued or in flight.
    busy: bool,
    closed: bool,
    waiters: Vec<Waker>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        let waiters = {
            let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut s);
            if s.busy && !s.closed { Vec::new() } else { std::mem::take(&mut s.waiters) }
        };
        for w in waiters {
            w.wake();
        }
    }
}

/// Sends requests over a client connection.
pub struct SendRequest<B> {
    jobs: mpsc::UnboundedSender<Job<B>>,
    shared: Arc<Shared>,
}

impl<B> std::fmt::Debug for SendRequest<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendRequest").field("ready", &self.is_ready()).field("closed", &self.is_closed()).finish()
    }
}

impl<B> SendRequest<B> {
    /// Whether the connection takes a request now (open, and not busy with one).
    pub fn is_ready(&self) -> bool {
        let s = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        !s.busy && !s.closed
    }

    /// Whether the connection is gone.
    pub fn is_closed(&self) -> bool {
        self.shared.state.lock().unwrap_or_else(PoisonError::into_inner).closed || self.jobs.is_closed()
    }

    /// Waits until the connection takes a request; an error once it's gone.
    pub fn ready(&mut self) -> impl Future<Output = Result<(), Error>> + '_ {
        std::future::poll_fn(|cx| {
            let mut s = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            if s.closed {
                return Poll::Ready(Err(Error::closed()));
            }
            if !s.busy {
                return Poll::Ready(Ok(()));
            }
            if !s.waiters.iter().any(|w| w.will_wake(cx.waker())) {
                s.waiters.push(cx.waker().clone());
            }
            Poll::Pending
        })
    }

    /// Sends `req`: its response once its head arrived (the body streams after). Refused (canceled)
    /// when the connection is gone or busy with another request.
    pub fn send_request(&mut self, req: http::Request<B>) -> impl Future<Output = Result<http::Response<Incoming>, Error>> + use<B> {
        let (answer, rx) = oneshot::channel();
        let accepted = {
            let mut s = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            if s.closed || s.busy {
                false
            } else {
                s.busy = true;
                true
            }
        };
        let sent = accepted && self.jobs.unbounded_send(Job { req, answer }).is_ok();
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

/// How client connections are made.
#[derive(Clone, Copy, Debug)]
pub struct Builder {
    max_head: usize,
}

impl Default for Builder {
    fn default() -> Builder {
        Builder { max_head: 1 << 20 }
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    /// The largest response head accepted (1 MiB unless set).
    pub fn max_head_size(&mut self, bytes: usize) -> &mut Builder {
        self.max_head = bytes.max(1024);
        self
    }

    /// A client connection over `io`.
    pub fn handshake<I, B>(&self, io: I) -> (SendRequest<B>, Connection<I, B>)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        B: SendBody + Unpin,
    {
        let (tx, rx) = mpsc::unbounded();
        let shared = Arc::new(Shared::default());
        let wire = Wire { io: Some(io), rbuf: BytesMut::new(), wbuf: WriteBuf::default(), eof: false, max_head: self.max_head };
        (SendRequest { jobs: tx, shared: Arc::clone(&shared) }, Connection { wire, jobs: rx, shared, step: Step::Idle })
    }
}

/// A client connection over `io`, with its defaults.
pub async fn handshake<I, B>(io: I) -> Result<(SendRequest<B>, Connection<I, B>), Error>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: SendBody + Unpin,
{
    Ok(Builder::default().handshake(io))
}

/// Where the connection is.
enum Step<B> {
    /// Waiting for a request.
    Idle,
    /// Sending a request's body, and watching for an early response.
    Sending {
        body: B,
        out: Out,
        ask: Ask,
    },
    /// Waiting for the response's head.
    Awaiting {
        ask: Ask,
        sent_all: bool,
    },
    /// Reading the response's body into its pipe.
    Receiving {
        decoder: Decoder,
        feed: Feed,
        reuse: bool,
    },
    Done,
}

/// What a request in flight asked, and who waits for its answer.
struct Ask {
    method: Method,
    upgrading: bool,
    answer: Option<Answer>,
}

/// A request body's framing as it goes out.
struct Out {
    framing: Framing,
    left: u64,
    trailers: Option<Box<http::HeaderMap>>,
    /// The body ended: all of it, and its framing's end, is in the write buffer (it isn't asked
    /// for more).
    done: bool,
}

/// How far gathering a request body got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gathered {
    All,
    /// The body has nothing more now.
    Paused,
    /// Enough is gathered to write first.
    Full,
}

/// The connection's transport and buffers.
struct Wire<I> {
    io: Option<I>,
    rbuf: BytesMut,
    wbuf: WriteBuf,
    eof: bool,
    max_head: usize,
}

impl<I: AsyncRead + AsyncWrite + Unpin + Send + 'static> Wire<I> {
    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<Result<usize, Error>> {
        let Some(io) = self.io.as_mut() else { return Poll::Ready(Ok(0)) };
        match poll_read_into(io, cx, &mut self.rbuf, SCRATCH) {
            Poll::Ready(Ok(0)) => {
                self.eof = true;
                Poll::Ready(Ok(0))
            }
            other => other.map_err(Error::io),
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let Some(io) = self.io.as_mut() else { return Poll::Ready(Err(Error::closed())) };
        self.wbuf.poll_flush(io, cx).map_err(Error::io)
    }

    /// The response head, parsed from what was read; informational answers (but `101`) skipped.
    fn take_head(&mut self, method: &Method) -> Result<Option<ResponseHead>, Error> {
        loop {
            let Some(h) = head::parse_response(&mut self.rbuf, self.max_head, method)? else { return Ok(None) };
            if h.status.is_informational() && h.status != StatusCode::SWITCHING_PROTOCOLS {
                continue;
            }
            return Ok(Some(h));
        }
    }

    /// Gathers a request body, framed, as it comes.
    fn gather<B: SendBody + Unpin>(&mut self, cx: &mut Context<'_>, body: &mut B, out: &mut Out) -> Result<Gathered, Error> {
        if out.done {
            return Ok(Gathered::All);
        }
        while self.wbuf.len() < 64 << 10 {
            match Pin::new(&mut *body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    let data = match frame.into_data() {
                        Ok(data) => data,
                        Err(frame) => {
                            if let Ok(t) = frame.into_trailers() {
                                out.trailers = Some(Box::new(t));
                            }
                            continue;
                        }
                    };
                    if data.is_empty() {
                        continue;
                    }
                    match out.framing {
                        Framing::Length(_) => {
                            let n = (data.len() as u64).min(out.left);
                            out.left -= n;
                            self.wbuf.push(data.slice(..n as usize));
                        }
                        Framing::Chunked => {
                            head::chunk_head(self.wbuf.tail(), data.len() as u64);
                            self.wbuf.push(data);
                            self.wbuf.extend(b"\r\n");
                        }
                        Framing::Empty | Framing::Close => {}
                    }
                }
                Poll::Ready(Some(Err(e))) => return Err(Error::body(e)),
                Poll::Ready(None) => {
                    if out.framing == Framing::Chunked {
                        head::last_chunk(self.wbuf.tail(), out.trailers.as_deref());
                    }
                    out.done = true;
                    return Ok(Gathered::All);
                }
                Poll::Pending => return Ok(Gathered::Paused),
            }
        }
        Ok(Gathered::Full)
    }

    /// Moves a response body from the connection into its pipe while the reader wants it: `Ready(true)`
    /// once the body is done (or its reader gone).
    fn poll_receive(&mut self, cx: &mut Context<'_>, decoder: &mut Decoder, feed: &Feed) -> Poll<Result<(), Error>> {
        loop {
            match feed.want() {
                Want::Gone => return Poll::Ready(Ok(())),
                Want::Full => {
                    if feed.poll_want(cx).is_pending() {
                        return Poll::Pending;
                    }
                    continue;
                }
                Want::More => {}
            }
            match decoder.decode(&mut self.rbuf) {
                Ok(Decoded::Data(chunk)) => feed.push(chunk),
                Ok(Decoded::End(trailers)) => {
                    feed.end(trailers);
                    return Poll::Ready(Ok(()));
                }
                Ok(Decoded::More) => {
                    if self.eof {
                        match decoder.at_eof() {
                            Ok(Decoded::End(t)) => feed.end(t),
                            _ => feed.fail(Error::closed()),
                        }
                        return Poll::Ready(Ok(()));
                    }
                    match self.poll_read(cx) {
                        Poll::Ready(Ok(_)) => {}
                        Poll::Ready(Err(e)) => {
                            feed.fail(e.share());
                            return Poll::Ready(Err(e));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                Err(e) => {
                    feed.fail(e.share());
                    return Poll::Ready(Err(e));
                }
            }
        }
    }
}

/// The reading and writing of a client connection: run it as a task. It ends when the connection
/// closes, or once every [`SendRequest`] dropped and the last exchange is done.
pub struct Connection<I, B> {
    wire: Wire<I>,
    jobs: mpsc::UnboundedReceiver<Job<B>>,
    shared: Arc<Shared>,
    step: Step<B>,
}

impl<I, B> Connection<I, B> {
    /// Upgrades are always handed over; kept for the shape of hyper's API.
    pub fn with_upgrades(self) -> Self {
        self
    }
}

/// Begins an exchange: the head gathered, and the body's framing.
fn start<I, B: SendBody>(wire: &mut Wire<I>, job: Job<B>) -> Step<B> {
    let Job { req, answer } = job;
    let (parts, body) = req.into_parts();
    let upgrading = parts.method == Method::CONNECT
        || (parts.headers.contains_key(http::header::UPGRADE)
            && parts.headers.get_all(http::header::CONNECTION).iter().any(|v| v.as_bytes().split(|b| *b == b',').any(|t| t.trim_ascii().eq_ignore_ascii_case(b"upgrade"))));
    let framing = head::encode_request(wire.wbuf.tail(), &parts.method, &parts.uri, &parts.headers, body.size_hint().exact(), false);
    let out = Out { framing, left: if let Framing::Length(n) = framing { n } else { 0 }, trailers: None, done: false };
    Step::Sending { body, out, ask: Ask { method: parts.method, upgrading, answer: Some(answer) } }
}

/// Delivers a response head: the response to its asker, and what reading its body takes.
fn deliver<I: AsyncRead + AsyncWrite + Unpin + Send + 'static, B>(wire: &mut Wire<I>, shared: &Shared, h: ResponseHead, ask: Ask, sent_all: bool) -> Step<B> {
    let ResponseHead { status, version, headers, framing, keep_alive } = h;
    let Ask { method, upgrading, answer } = ask;
    let switching = upgrading && (status == StatusCode::SWITCHING_PROTOCOLS || (method == Method::CONNECT && status.is_success()));
    let (body, receive) = if switching {
        (Incoming::empty(), None)
    } else {
        match framing {
            Framing::Empty => (Incoming::empty(), None),
            Framing::Length(n) if wire.rbuf.len() as u64 >= n => (Incoming::whole(wire.rbuf.split_to(n as usize).freeze()), None),
            Framing::Length(n) => {
                let (body, feed) = Incoming::piped(Some(n));
                (body, Some((Decoder::length(n), feed)))
            }
            Framing::Chunked => {
                let (body, feed) = Incoming::piped(None);
                (body, Some((Decoder::chunked(), feed)))
            }
            Framing::Close => {
                let (body, feed) = Incoming::piped(None);
                (body, Some((Decoder::Close, feed)))
            }
        }
    };
    let mut resp = http::Response::new(body);
    *resp.status_mut() = status;
    *resp.version_mut() = version;
    *resp.headers_mut() = headers;
    if switching {
        let (pending, on) = upgrade::pair();
        resp.extensions_mut().insert(on);
        if let Some(a) = answer {
            let _ = a.send(Ok(resp));
        }
        if let Some(io) = wire.io.take() {
            pending.fulfill(Upgraded::new(io, std::mem::take(&mut wire.rbuf).freeze()));
        }
        return Step::Done;
    }
    if let Some(a) = answer {
        let _ = a.send(Ok(resp));
    }
    let reuse = keep_alive && sent_all;
    match receive {
        Some((decoder, feed)) => Step::Receiving { decoder, feed, reuse },
        None if reuse => {
            shared.update(|s| s.busy = false);
            Step::Idle
        }
        None => Step::Done,
    }
}

impl<I, B> Connection<I, B>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: SendBody + Unpin,
{
    /// Ends the connection with `e`, telling a request waiting for its answer.
    fn fail(&mut self, e: Error) -> Poll<Result<(), Error>> {
        if let Step::Sending { ask: Ask { answer: Some(a), .. }, .. } | Step::Awaiting { ask: Ask { answer: Some(a), .. }, .. } = std::mem::replace(&mut self.step, Step::Done) {
            let _ = a.send(Err(e.share()));
        }
        self.shared.update(|s| s.closed = true);
        Poll::Ready(Err(e))
    }

    fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        loop {
            let next = match &mut self.step {
                Step::Idle => {
                    if !self.wire.rbuf.is_empty() {
                        return self.fail(Error::parse(crate::Parse::Unexpected));
                    }
                    match Pin::new(&mut self.jobs).poll_next(cx) {
                        Poll::Ready(Some(job)) => start(&mut self.wire, job),
                        Poll::Ready(None) => Step::Done,
                        Poll::Pending => {
                            self.wire.wbuf.release();
                            // An idle connection the server closed is gone.
                            match self.wire.poll_read(cx) {
                                Poll::Ready(Ok(0)) => Step::Done,
                                Poll::Ready(Ok(_)) => continue,
                                Poll::Ready(Err(e)) => return self.fail(e),
                                Poll::Pending => return Poll::Pending,
                            }
                        }
                    }
                }
                Step::Sending { body, out, ask } => {
                    let gathered = match self.wire.gather(cx, body, out) {
                        Ok(g) => g,
                        Err(e) => return self.fail(e),
                    };
                    let flushed = match self.wire.poll_flush(cx) {
                        Poll::Ready(Ok(())) => true,
                        Poll::Ready(Err(e)) => return self.fail(e),
                        Poll::Pending => false,
                    };
                    // Sent only once the body ended and all of it went: a last write the socket
                    // couldn't take yet still has to go, or the server waits for it and never
                    // answers (over a link slower than loopback, the end of a body often waits so).
                    if gathered == Gathered::All && flushed {
                        let ask = Ask { method: ask.method.clone(), upgrading: ask.upgrading, answer: ask.answer.take() };
                        Step::Awaiting { ask, sent_all: true }
                    } else {
                        // An early answer (refusing the body, say) is taken now; the rest goes unsent.
                        if let Poll::Ready(Err(e)) = self.wire.poll_read(cx) {
                            return self.fail(e);
                        }
                        match self.wire.take_head(&ask.method) {
                            Ok(Some(h)) => {
                                let ask = Ask { method: ask.method.clone(), upgrading: ask.upgrading, answer: ask.answer.take() };
                                deliver(&mut self.wire, &self.shared, h, ask, false)
                            }
                            Ok(None) if self.wire.eof => return self.fail(Error::closed()),
                            Ok(None) if flushed && gathered == Gathered::Full => continue,
                            Ok(None) => return Poll::Pending,
                            Err(e) => return self.fail(e),
                        }
                    }
                }
                Step::Awaiting { ask, sent_all } => match self.wire.take_head(&ask.method) {
                    Ok(Some(h)) => {
                        let ask = Ask { method: ask.method.clone(), upgrading: ask.upgrading, answer: ask.answer.take() };
                        deliver(&mut self.wire, &self.shared, h, ask, *sent_all)
                    }
                    Ok(None) if self.wire.eof => return self.fail(Error::closed()),
                    Ok(None) => match self.wire.poll_read(cx) {
                        Poll::Ready(Ok(_)) => continue,
                        Poll::Ready(Err(e)) => return self.fail(e),
                        Poll::Pending => return Poll::Pending,
                    },
                    Err(e) => return self.fail(e),
                },
                Step::Receiving { decoder, feed, reuse } => match self.wire.poll_receive(cx, decoder, feed) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return self.fail(e),
                    Poll::Ready(Ok(())) => {
                        // The body is done, or its reader gone with the rest unread.
                        if *reuse && decoder.is_done() && !self.wire.eof {
                            self.shared.update(|s| s.busy = false);
                            Step::Idle
                        } else {
                            Step::Done
                        }
                    }
                },
                Step::Done => {
                    self.shared.update(|s| s.closed = true);
                    let _ = self.wire.poll_flush(cx);
                    if let Some(mut io) = self.wire.io.take() {
                        let _ = Pin::new(&mut io).poll_close(cx);
                    }
                    return Poll::Ready(Ok(()));
                }
            };
            self.step = next;
        }
    }
}

impl<I, B> Future for Connection<I, B>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: SendBody + Unpin,
{
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.get_mut().poll_drive(cx)
    }
}

impl<I, B> Drop for Connection<I, B> {
    fn drop(&mut self) {
        let waiters = {
            let mut s = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
            s.closed = true;
            std::mem::take(&mut s.waiters)
        };
        for w in waiters {
            w.wake();
        }
    }
}
