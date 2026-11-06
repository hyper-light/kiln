//! HTTP/1.1 and HTTP/2 server: kiln-http's connections on hyper-rt. Each accepted connection is one
//! hyper-rt task, its requests' handlers polled in it (an HTTP/2 connection's all side by side), or run
//! where the server's placement puts them (goal 7, `crate::placement`: on a less delayed shard, or as
//! movable tasks), their responses written by the connection's task; upgrades (WebSocket) are HTTP/1.1's.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::io::{AsyncRead, AsyncWrite};
use http::{HeaderValue, StatusCode, header};
use http_body::Frame;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
pub use kiln_http::Incoming;
use kiln_http::server::{http1, http2};
use kiln_http::service_fn;

/// Response bodies: whole buffers or streams of chunks, boxed to one type.
pub type Body = BoxBody<Bytes, std::io::Error>;
pub type Request = http::Request<Incoming>;
pub type Response = http::Response<Body>;

/// What serves requests. Implemented by the API router.
pub trait Handler: Send + Sync + 'static {
    fn handle(self: Arc<Self>, req: Request) -> impl Future<Output = Response> + Send;
}

/// Where a request came from and how its connection serves it, put on every request a connection reads (a request
/// extension): the client's address when the listener knew it, whether it came over TLS (the server's own HTTPS), and
/// its class (`crate::overload`). One extension, one allocation, where there were up to three and two allocations:
/// with kiln-http's arrival time before them and the router's caller after, they outgrew the request's table of
/// extensions, made again larger for each request the router authorized (goal 9, plan 01a1a87b2cec step 5).
#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub remote: Option<std::net::SocketAddr>,
    pub secure: bool,
    pub class: crate::overload::Class,
}

/// The address a request came from, when the listener knew it.
pub fn remote_addr(req: &Request) -> Option<std::net::SocketAddr> {
    req.extensions().get::<Arrival>().and_then(|a| a.remote)
}

/// The address a request came from, when the listener knew it.
pub fn remote_ip(req: &Request) -> Option<std::net::IpAddr> {
    remote_addr(req).map(|r| r.ip())
}

/// Whether a request came over TLS (the server's own HTTPS).
pub fn secure(req: &Request) -> bool {
    req.extensions().get::<Arrival>().is_some_and(|a| a.secure)
}

/// Serve one accepted connection until the client goes away. Errors are logged at debug level:
/// clients disappearing mid-request are normal.
pub async fn serve_connection<I, H>(io: I, handler: Arc<H>, remote: Option<std::net::SocketAddr>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    serve_connection_with(io, handler, remote, false).await;
}

/// How long a new connection may take to say whether it speaks TLS, and to finish the handshake.
const FIRST_BYTE: std::time::Duration = std::time::Duration::from_secs(30);
const HANDSHAKE: std::time::Duration = std::time::Duration::from_secs(15);
/// The most a connection's socket holds that it hasn't sent (`TCP_NOTSENT_LOWAT`): past that,
/// kiln's writer waits. Linux would let a slow reader's backlog grow to 4 MiB in the kernel.
const NOTSENT_LOWAT: u32 = 128 << 10;

/// What a client speaking HTTP/2 without TLS sends first (RFC 9113 §3.4): the front door, which
/// ends a browser's TLS and passes its bytes on, passes HTTP/2 on this way.
pub const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// What a new connection of kiln's listener speaks, from its first bytes.
#[derive(Debug, PartialEq, Eq)]
enum Opening {
    /// A TLS handshake record (0x16), which no HTTP request starts with.
    Tls,
    /// HTTP/2's preface.
    H2,
    Http1,
    /// Not decided yet: the bytes so far begin HTTP/2's preface.
    Unsure,
}

fn opening(head: &[u8]) -> Opening {
    match head {
        [0x16, ..] => Opening::Tls,
        _ if head.len() >= H2_PREFACE.len() && head.starts_with(H2_PREFACE) => Opening::H2,
        _ if H2_PREFACE.starts_with(head) => Opening::Unsure,
        _ => Opening::Http1,
    }
}

/// Serve a connection of kiln's listener in whatever it speaks: TLS when `tls` is set (with
/// `server.https`; plain HTTP stays for the runners and scripts that use it), and HTTP/1.1 or
/// HTTP/2 inside. Over TLS the client says which (ALPN); in the clear, HTTP/2 starts with its
/// preface. Nearly every connection tells by its first byte; only those starting with `P` take more.
pub async fn serve_accepted<H: Handler>(stream: hyper_rt::tcp::TcpStream, handler: Arc<H>, remote: Option<std::net::SocketAddr>, tls: Option<&super::tls::ServerTls>) {
    // Where the platform offers it: a slow reader's backlog waits here, where kiln can catch it
    // up (a session's viewer: docs/wire.md, "Revisions"), not megabytes deep in the kernel.
    let _ = stream.set_notsent_lowat(NOTSENT_LOWAT);
    // Where the browser protocol is served, what its socket needs of the connection should one of its
    // requests upgrade to it: a handle to the socket's options and its TLS buffer's bound
    // (`crate::live`; HTTP/1.1 only, as WebSockets are).
    let control = crate::live::control_for(&stream);
    let mut head = Vec::with_capacity(H2_PREFACE.len());
    let mut buf = [0u8; 32];
    let kind = loop {
        let want = if head.is_empty() { 1 } else { H2_PREFACE.len().saturating_sub(head.len()) };
        let n = match crate::rt::timeout(FIRST_BYTE, stream.read(buf.get_mut(..want).unwrap_or_default())).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        head.extend_from_slice(buf.get(..n).unwrap_or_default());
        match opening(&head) {
            Opening::Unsure => {}
            kind => break kind,
        }
    };
    let io = super::io::TcpIo::new(stream);
    // Each protocol's connection is boxed: a connection's task is the size of the one it speaks.
    match (kind, tls) {
        (Opening::Tls, Some(tls)) => {
            let Ok(config) = tls.config_alpn(&[super::tls::H2, super::tls::HTTP11]) else { return };
            let io = super::tls::Prefixed::new(head, io);
            match crate::rt::timeout(HANDSHAKE, super::tls::ServerTlsStream::accept(io, config)).await {
                Ok(Ok(mut s)) => {
                    // Full or resumed (goal 7 step 3): the listener's ticket keys are shared.
                    crate::metrics::tls().handshake("server", s.handshake());
                    if s.alpn_protocol() == Some(super::tls::H2) {
                        Box::pin(serve_h2(s, Bytes::new(), handler, remote, true)).await
                    } else {
                        if let Some(c) = &control {
                            s.set_send_limit(Arc::clone(&c.tls_limit));
                        }
                        Box::pin(serve_read(s, Bytes::new(), handler, remote, true, control)).await
                    }
                }
                Ok(Err(e)) => tracing::debug!(error = %e, ?remote, "TLS handshake failed"),
                Err(_) => tracing::debug!(?remote, "TLS handshake timed out"),
            }
        }
        (Opening::H2, _) => Box::pin(serve_h2(io, Bytes::from(head), handler, remote, false)).await,
        // HTTP/1.1, or TLS without a certificate here: HTTP/1.1 answers what it can.
        _ => Box::pin(serve_read(io, Bytes::from(head), handler, remote, false, control)).await,
    }
}

/// What every request of a connection carries: where it came from, whether over TLS, and its class ([`Arrival`]).
fn mark(req: &mut Request, remote: Option<std::net::SocketAddr>, secure: bool, class: crate::overload::Class) {
    req.extensions_mut().insert(Arrival { remote, secure, class });
}

/// [`serve_connection`], saying whether the connection is TLS ([`secure`] of its requests).
pub async fn serve_connection_with<I, H>(io: I, handler: Arc<H>, remote: Option<std::net::SocketAddr>, secure: bool)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    serve_read(io, Bytes::new(), handler, remote, secure, None).await;
}

/// HTTP/1.1 over `io`, whose first bytes `read` were taken off it already.
async fn serve_read<I, H>(io: I, read: Bytes, handler: Arc<H>, remote: Option<std::net::SocketAddr>, secure: bool, control: Option<Arc<crate::live::Control>>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    // What the connection serves now, by class: its task runs at the most urgent (goal 6, plan
    // 01a1a59c2abe step 3).
    let serving = crate::overload::Connection::new();
    let service = service_fn(move |mut req: Request| {
        let request = serving.begin(&req);
        // The connection's handle to its socket, where the browser protocol is served (`crate::live`).
        if let Some(c) = &control {
            req.extensions_mut().insert(Arc::clone(c));
        }
        mark(&mut req, remote, secure, request.class());
        answer(handler.clone(), req, request)
    });
    let conn = http1::Builder::new()
        // Slow or idle clients cannot pin a connection: headers must arrive within the timeout
        // (on the shard's timing wheel).
        .header_read_timeout(Some(std::time::Duration::from_secs(30)))
        .keep_alive(true)
        .max_head_size(1 << 20)
        .serve_read(io, read, service);
    if let Err(e) = conn.await {
        tracing::debug!(error = %e, "http connection ended with an error");
    }
}

/// Serve an HTTP/2 connection (whose first bytes `read` were taken off it already): every request
/// at once over the one connection, each handler polled in the connection's task. Requests reach
/// the handler shaped as HTTP/1.1's ([`as_http1`]). WebSockets stay HTTP/1.1's: no extended CONNECT
/// is offered, so a browser opens an HTTP/1.1 connection for them.
pub(crate) async fn serve_h2<I, H>(io: I, read: Bytes, handler: Arc<H>, remote: Option<std::net::SocketAddr>, secure: bool)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    let serving = crate::overload::Connection::new();
    let service = service_fn(move |mut req: Request| {
        as_http1(&mut req);
        let request = serving.begin(&req);
        mark(&mut req, remote, secure, request.class());
        answer(handler.clone(), req, request)
    });
    let conn = http2::Builder::new()
        // A connection whose client has gone quiet without closing is found and ended.
        .keep_alive_interval(Some(std::time::Duration::from_secs(30)))
        .keep_alive_timeout(std::time::Duration::from_secs(20))
        .max_header_list_size(64 << 10)
        // Its `Host` the authority's own bytes ([`as_http1`]'s, without a copy).
        .host_from_authority(true)
        .serve_read(io, read, service);
    if let Err(e) = conn.await {
        tracing::debug!(error = %e, "http/2 connection ended with an error");
    }
}

/// A request's handler run where the server's placement puts it (goal 7, `crate::placement`): in this task,
/// on a less delayed shard, or as a movable task; a 503 when the shard it was placed on stopped before it
/// answered. A `demoted` request (its head read in a more urgent class's turn) takes its own class's turn
/// first where it stays in this task (goal 6, plan 01a1a59c2abe step 3).
/// A request's future as its connection keeps and polls it (both HTTP/1.1's and HTTP/2's; goal 9, plan 01a1a87b2cec
/// step 5): its handler's future boxed where it's made, so each layer that carries it (this one, its placement,
/// the connection's slot for it) moves a pointer. Each moved the handler's whole future as it was first polled:
/// 36 KB a layer at rt6's tree, 6.6% of the shards' CPU in g9p6's calm profile copying them; 2 KB since goal 10's
/// step 5 (the router boxed apart). Boxed, it's copied once, into its box. Its answer's body goes to the connection
/// in its guard as it is, not boxed again (`crate::overload::Guarded`).
fn answer<H: Handler>(handler: Arc<H>, req: Request, serving: crate::overload::Serving) -> impl Future<Output = http::Response<crate::overload::Guarded>> + Send + 'static {
    let handling = Box::pin(handler.handle(req));
    async move { crate::overload::while_sent(placed(serving.class(), serving.demoted(), handling).await, serving) }
}

async fn placed(class: crate::overload::Class, demoted: bool, handling: impl Future<Output = Response> + Send + 'static) -> Response {
    match crate::placement::request(class, demoted, handling).await {
        Some(resp) => resp,
        None => text(StatusCode::SERVICE_UNAVAILABLE, "the server is stopping"),
    }
}

/// An HTTP/2 request shaped as the HTTP/1.1 request it stands for, so handlers see one shape:
/// its authority (`:authority`) as `Host` (kiln-http's connection gives it, as it was decoded:
/// [`serve_h2`]), its cookies in one `Cookie` (a browser sends them separately over HTTP/2, RFC 9113
/// §8.2.3), and its target as a path (origin form). The cookies are joined into one buffer made at
/// their length, the header's own (goal 9, plan 01a1a87b2cec step 5: a list of them, the joined copy
/// and the header's copy of that were three a request).
fn as_http1<B>(req: &mut http::Request<B>) {
    if !req.headers().contains_key(header::HOST)
        && let Some(v) = req.uri().authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        req.headers_mut().insert(header::HOST, v);
    }
    let cookies = req.headers().get_all(header::COOKIE);
    if cookies.iter().nth(1).is_some() {
        let mut joined = Vec::with_capacity(cookies.iter().map(|v| v.len() + 2).sum::<usize>().saturating_sub(2));
        for (i, v) in cookies.iter().enumerate() {
            if i > 0 {
                joined.extend_from_slice(b"; ");
            }
            joined.extend_from_slice(v.as_bytes());
        }
        if let Ok(v) = HeaderValue::from_maybe_shared(Bytes::from(joined)) {
            req.headers_mut().insert(header::COOKIE, v);
        }
    }
    if req.uri().scheme().is_some() || req.uri().authority().is_some() {
        let target = req.uri().path_and_query().cloned().unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
        *req.uri_mut() = http::Uri::from(target);
    }
}

// ---------------------------------------------------------------- bodies & responses

pub fn full(bytes: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

pub fn empty() -> Body {
    http_body_util::Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

/// A body fed by a channel of chunks (an async-channel's, or a lock-free `crate::memory::queue`'s: goal 4,
/// plan 01a1a59c2a9c step 5); the response ends when the sender is dropped.
pub fn channel_body(rx: impl futures::Stream<Item = Bytes> + Send + Sync + 'static) -> Body {
    use futures::StreamExt as _;
    let frames = rx.map(|chunk| Ok::<_, std::io::Error>(Frame::data(chunk)));
    BodyExt::boxed(http_body_util::StreamBody::new(frames))
}

/// A body of `parts` made already, sent in order as they are: none joined or copied here (kiln-http sends a
/// large one from its own buffer, a small one beside the head), its length known for the head (goal 10,
/// plan 01a1a87b2dae step 5: a session's events page, slices of what its copy keeps).
pub fn pieces(parts: Vec<Bytes>) -> Body {
    let left = parts.iter().map(|p| u64::try_from(p.len()).unwrap_or(u64::MAX)).fold(0u64, u64::saturating_add);
    BodyExt::boxed(Pieces { parts: parts.into(), left })
}

/// [`pieces`]' body: the parts still to go, and their bytes.
struct Pieces {
    parts: std::collections::VecDeque<Bytes>,
    left: u64,
}

impl http_body::Body for Pieces {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        while let Some(part) = this.parts.pop_front() {
            if !part.is_empty() {
                this.left = this.left.saturating_sub(u64::try_from(part.len()).unwrap_or(u64::MAX));
                return Poll::Ready(Some(Ok(Frame::data(part))));
            }
        }
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(self.left)
    }
}

/// The API's own content types, whose header each answer takes as the words themselves, not a copy made of
/// them for each answer (goal 10, plan 01a1a87b2dae step 5: every JSON answer copied its type).
static JSON: &str = "application/json";
static TEXT: &str = "text/plain; charset=utf-8";

pub fn response(status: StatusCode, content_type: &str, body: Body) -> Response {
    let mut r = http::Response::new(body);
    *r.status_mut() = status;
    let value = if content_type == JSON {
        Some(HeaderValue::from_static(JSON))
    } else if content_type == TEXT {
        Some(HeaderValue::from_static(TEXT))
    } else {
        HeaderValue::from_str(content_type).ok()
    };
    if let Some(v) = value {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r
}

pub fn json<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    // Encoded as it's answered (goal 10, plan 01a1a87b2dae step 2's trace: the lab's reads never are).
    crate::reads::trace::encoded();
    match encode(value) {
        Ok(b) => response(status, JSON, full(b)),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("serialize: {e}")),
    }
}

/// The most of [`encode`]'s buffer a thread keeps between answers: 64 KiB, what kiln-http keeps a thread for the
/// bytes it reads (its scratch buffer, docs/http.md). A larger answer's buffer is let go once it's copied out.
const ENCODE_KEPT: usize = 64 << 10;

thread_local! {
    /// Where [`encode`] writes, kept between answers on this thread.
    static ENCODING: std::cell::Cell<Vec<u8>> = const { std::cell::Cell::new(Vec::new()) };
}

/// `value` as JSON, written where this thread wrote its last answer and copied out once at its length (goal 9, plan
/// 01a1a87b2cec step 5). Each answer encoded into a vector of its own grew it from 128 bytes by doubling, each step
/// a reallocation and a copy of what it held (a 10 KB answer: 7 and about 16 KB moved), then a block more to share
/// it, its length short of its room.
fn encode<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Result<Bytes> {
    let mut buf = ENCODING.try_with(std::cell::Cell::take).unwrap_or_default();
    buf.clear();
    let out = serde_json::to_writer(&mut buf, value).map(|()| Bytes::copy_from_slice(&buf));
    if buf.capacity() <= ENCODE_KEPT {
        let _ = ENCODING.try_with(move |e| e.set(buf));
    }
    out
}

pub fn text(status: StatusCode, s: &str) -> Response {
    response(status, TEXT, full(s.to_owned()))
}

pub fn status(status: StatusCode) -> Response {
    let mut r = http::Response::new(empty());
    *r.status_mut() = status;
    r
}

pub fn with_header(mut r: Response, name: header::HeaderName, value: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(value) {
        r.headers_mut().append(name, v);
    }
    r
}

// ---------------------------------------------------------------- request helpers

/// Room on the stack for a request path's segments as kiln's routes match them: as many as the API's deepest route
/// names (8, CI's `/api/uploads/repos/{owner}/{repo}/releases/{id}/assets`). A path of more (under a route that takes
/// the rest: a file's, a blob's) has them in a vector of its own (goal 9, plan 01a1a87b2cec step 5).
pub const ROUTE_DEPTH: usize = 8;

/// `path`'s segments (empty ones skipped), in `room` when they fit, else in a vector of their own.
pub fn split_path<'r, 'p: 'r>(path: &'p str, room: &'r mut [&'p str; ROUTE_DEPTH]) -> std::borrow::Cow<'r, [&'p str]> {
    let mut n = 0;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        match room.get_mut(n) {
            Some(slot) => *slot = part,
            None => return std::borrow::Cow::Owned(path.split('/').filter(|p| !p.is_empty()).collect()),
        }
        n += 1;
    }
    let room: &'r [&'p str; ROUTE_DEPTH] = room;
    std::borrow::Cow::Borrowed(room.get(..n).unwrap_or_default())
}

/// Collect a request body, refusing more than `limit` bytes.
pub async fn body_bytes(body: Incoming, limit: usize) -> anyhow::Result<Bytes> {
    let collected = http_body_util::Limited::new(body, limit)
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("request body: {e}"))?;
    Ok(collected.to_bytes())
}

pub async fn body_json<T: serde::de::DeserializeOwned>(body: Incoming, limit: usize) -> anyhow::Result<T> {
    let b = body_bytes(body, limit).await?;
    if b.is_empty() {
        return serde_json::from_slice(b"{}").map_err(|e| anyhow::anyhow!("json: {e}"));
    }
    serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("json: {e}"))
}

/// Next data chunk of a streaming request body (`None` at the end).
pub async fn next_chunk(body: &mut Incoming) -> anyhow::Result<Option<Bytes>> {
    loop {
        match body.frame().await {
            None => return Ok(None),
            Some(Err(e)) => anyhow::bail!("request body: {e}"),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    return Ok(Some(data));
                }
            }
        }
    }
}

/// Query string → key/value pairs (percent-decoded).
pub fn query(req: &Request) -> std::collections::HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(k), decode(v))
        })
        .collect()
}

/// Percent-decode (and `+` → space, as browsers encode query strings).
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn header_str<'a>(req: &'a Request, name: &header::HeaderName) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        assert_eq!(decode("a%2Fb+c%zz"), "a/b c%zz");
        assert_eq!(decode("agent%3Abench-1"), "agent:bench-1");
    }

    #[test]
    fn a_connection_says_what_it_speaks_by_its_first_bytes() {
        assert_eq!(opening(&[0x16]), Opening::Tls);
        assert_eq!(opening(b"G"), Opening::Http1);
        // Only `P` needs another look: POST, PUT and PATCH part from the preface at once.
        assert_eq!(opening(b"P"), Opening::Unsure);
        assert_eq!(opening(b"POST /api/runs HTTP/1.1\r\n"), Opening::Http1);
        assert_eq!(opening(b"PUT "), Opening::Http1);
        assert_eq!(opening(b"PRI * HTTP/2.0\r\n"), Opening::Unsure);
        assert_eq!(opening(H2_PREFACE), Opening::H2);
        assert_eq!(opening(b"PRI * HTTP/2.0\r\n\r\nXX\r\n\r\n"), Opening::Http1);
    }

    #[test]
    fn an_http2_request_reaches_handlers_shaped_as_http1() -> anyhow::Result<()> {
        let mut req = http::Request::builder().uri("https://kiln.dev:8443/api/runs?limit=5").header(header::COOKIE, "kiln_session=abc").header(header::COOKIE, "theme=dark").body(())?;
        as_http1(&mut req);
        assert_eq!(req.headers().get(header::HOST).and_then(|v| v.to_str().ok()), Some("kiln.dev:8443"));
        assert_eq!(req.headers().get_all(header::COOKIE).iter().count(), 1);
        assert_eq!(req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()), Some("kiln_session=abc; theme=dark"));
        assert_eq!(req.uri().to_string(), "/api/runs?limit=5");
        // A Host the client sent stands, and one cookie stays as it is.
        let mut req = http::Request::builder().uri("http://a.test/").header(header::HOST, "b.test").header(header::COOKIE, "x=1").body(())?;
        as_http1(&mut req);
        assert_eq!(req.headers().get(header::HOST).and_then(|v| v.to_str().ok()), Some("b.test"));
        assert_eq!(req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()), Some("x=1"));
        assert_eq!(req.uri().to_string(), "/");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: an answer's own content type is the words themselves, not a copy
    /// made of them for it; another type is said as it's given.
    #[test]
    fn g10_s5_an_answers_own_content_type_is_not_copied() {
        let said = |r: &Response| r.headers().get(header::CONTENT_TYPE).map(|v| (v.as_bytes().as_ptr(), v.as_bytes().to_vec()));
        assert_eq!(said(&response(StatusCode::OK, "application/json", empty())), Some((JSON.as_ptr(), JSON.as_bytes().to_vec())), "the JSON type's own words");
        assert_eq!(said(&json(StatusCode::OK, &1)).map(|s| s.0), Some(JSON.as_ptr()));
        assert_eq!(said(&text(StatusCode::OK, "ok")), Some((TEXT.as_ptr(), TEXT.as_bytes().to_vec())));
        assert_eq!(said(&response(StatusCode::OK, "image/png", empty())).map(|s| s.1), Some(b"image/png".to_vec()), "another as given");
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a body of pieces sends each as it is, in order (a large one from its
    /// own buffer, not a copy), its length known for the head; an empty one is passed over.
    #[test]
    fn g10_s5_a_body_of_pieces_sends_each_as_it_is() -> anyhow::Result<()> {
        use http_body::Body as _;
        let big = Bytes::from(vec![b'x'; 8 << 10]);
        let mut body = pieces(vec![Bytes::from_static(b"["), Bytes::new(), big.clone(), Bytes::from_static(b"]")]);
        assert_eq!(body.size_hint().exact(), Some(2 + (8 << 10)));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut frames = Vec::new();
        while let Poll::Ready(Some(frame)) = Pin::new(&mut body).poll_frame(&mut cx) {
            frames.push(frame?.into_data().map_err(|_| anyhow::anyhow!("a frame that isn't data"))?);
        }
        assert_eq!(frames.len(), 3, "the empty one passed over");
        assert_eq!(frames.get(1).map(|f| f.as_ptr()), Some(big.as_ptr()), "the large one as it is");
        assert_eq!(frames.concat(), [b"[".as_slice(), big.as_ref(), b"]"].concat());
        assert!(body.is_end_stream());
        Ok(())
    }

    /// Says what it was asked, as the handler saw it.
    struct Echo;

    impl Handler for Echo {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let host = header_str(&req, &header::HOST).unwrap_or("-").to_owned();
            let cookie = header_str(&req, &header::COOKIE).unwrap_or("-").to_owned();
            let secure = secure(&req);
            text(StatusCode::OK, &format!("{:?} {host} {} [{cookie}] secure={secure}", req.version(), req.uri()))
        }
    }

    /// kiln's listener, serving [`Echo`] on a free port: with `tls`, TLS or plain.
    fn listener(tls: Option<super::super::tls::ServerTls>) -> anyhow::Result<std::net::SocketAddr> {
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let (tls, remote) = (tls.clone(), stream.peer_addr().ok());
                crate::rt::spawn(async move { serve_accepted(stream, Arc::new(Echo), remote, tls.as_ref()).await });
            }
        });
        Ok(addr)
    }

    /// One HTTP/2 request over `io`, its body as text.
    async fn h2_get<I>(io: I, uri: &str) -> anyhow::Result<String>
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut send, conn) = kiln_http::client::http2::handshake::<_, http_body_util::Empty<Bytes>>(io).await?;
        crate::rt::spawn(conn);
        let req = http::Request::builder().uri(uri).header(header::COOKIE, "kiln_session=abc").header(header::COOKIE, "theme=dark").body(http_body_util::Empty::<Bytes>::new())?;
        let resp = send.send_request(req).await?;
        anyhow::ensure!(resp.version() == http::Version::HTTP_2, "answered over {:?}", resp.version());
        Ok(String::from_utf8_lossy(&resp.into_body().collect().await?.to_bytes()).into_owned())
    }

    /// HTTP/2 in the clear (as the front door passes it on) and over TLS (ALPN), and HTTP/1.1 still,
    /// on the one listener; handlers see the same request whichever.
    #[test]
    fn the_listener_speaks_http2_and_http1() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::front_door_csr(&["kiln.test".to_owned()], &key)?;
            let cert = pki.sign_front_door(&csr, &["kiln.test".to_owned()], 30)?;
            let addr = listener(Some(super::super::tls::ServerTls::from_pem(&cert, &key)?))?;

            // In the clear, with prior knowledge.
            let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let said = h2_get(super::super::io::TcpIo::new(tcp), "http://kiln.test/api/runs?limit=5").await?;
            assert_eq!(said, "HTTP/2.0 kiln.test /api/runs?limit=5 [kiln_session=abc; theme=dark] secure=false");

            // Over TLS, agreed by ALPN.
            let roots = super::super::tls::ClientTls::new(super::super::client::Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?;
            let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let config = roots.config_alpn(&[super::super::tls::H2, super::super::tls::HTTP11])?;
            let tls = super::super::tls::TlsStream::connect(super::super::io::RtFuturesIo::new(tcp), config, super::super::tls::server_name("kiln.test")?).await?;
            assert_eq!(tls.alpn_protocol(), Some(super::super::tls::H2));
            let said = h2_get(tls, "https://kiln.test/").await?;
            assert_eq!(said, "HTTP/2.0 kiln.test / [kiln_session=abc; theme=dark] secure=true");

            // HTTP/1.1, in the clear and over TLS.
            let plain = crate::http::client::Client::public()?;
            let resp = plain.get(&format!("http://{addr}/x")).send().await?;
            assert_eq!(String::from_utf8_lossy(&resp.bytes(1 << 16).await?), format!("HTTP/1.1 {addr} /x [-] secure=false"));
            let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let tls = super::super::tls::TlsStream::connect(super::super::io::RtFuturesIo::new(tcp), roots.config()?, super::super::tls::server_name("kiln.test")?).await?;
            assert_eq!(tls.alpn_protocol(), Some(super::super::tls::HTTP11));
            Ok(())
        })
    }

    /// Goal 7 step 3 (plan 01a1a832ebbf7327a203df446e16ecac): `server.https` resumes TLS
    /// sessions across connections (its ticket keys are the listener's), so the second connection from one client configuration
    /// resumes, and `kiln_tls_handshakes_total` counts both kinds.
    #[test]
    fn g7_s3_server_https_resumes_across_connections() -> anyhow::Result<()> {
        use futures::AsyncWriteExt as _;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(dir.path())?;
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::front_door_csr(&["kiln.test".to_owned()], &key)?;
            let cert = pki.sign_front_door(&csr, &["kiln.test".to_owned()], 30)?;
            let addr = listener(Some(super::super::tls::ServerTls::from_pem(&cert, &key)?))?;
            let roots = super::super::tls::ClientTls::new(super::super::client::Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?;
            let counted = |kind: &str| crate::metrics::tls().handshakes.get(&["server", kind]);
            let resumed_before = counted("resumed");
            let mut config = roots.config()?;
            let mut kinds = Vec::new();
            for _ in 0..2 {
                let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                let mut tls = super::super::tls::TlsStream::connect(super::super::io::RtFuturesIo::new(tcp), config, super::super::tls::server_name("kiln.test")?).await?;
                kinds.push(tls.handshake());
                // An answer read: the tickets, sent before it, are filed in the client's configuration.
                tls.write_all(b"GET /x HTTP/1.1\r\nHost: kiln.test\r\nConnection: close\r\n\r\n").await?;
                tls.flush().await?;
                let mut said = Vec::new();
                let mut buf = [0u8; 4096];
                while !String::from_utf8_lossy(&said).contains("secure=true") {
                    let n = tls.read_some(&mut buf).await?;
                    anyhow::ensure!(n > 0, "no answer: {}", String::from_utf8_lossy(&said));
                    said.extend_from_slice(buf.get(..n).unwrap_or_default());
                }
                config = tls.into_config();
            }
            assert_eq!(kinds, ["full", "resumed"]);
            assert!(counted("resumed") > resumed_before, "counted on /metrics");
            assert!(crate::metrics::render().contains("kiln_tls_handshakes_total{door=\"server\",kind=\"resumed\"}"));
            Ok(())
        })
    }

    /// A handler whose future is large: it holds a buffer across an await, as a router with every route inline did
    /// (goal 9, plan 01a1a87b2cec step 5).
    struct Big;

    impl Handler for Big {
        async fn handle(self: Arc<Self>, _req: Request) -> Response {
            let held = [7u8; 64 << 10];
            hyper_rt::futures::yield_now().await;
            text(StatusCode::OK, if held.iter().all(|b| *b == 7) { "held" } else { "lost" })
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a request's future, as its connection keeps it, is under a kilobyte however
    /// big its handler's future is (64 KiB here; kiln's API's 2 KB since goal 10's step 5): the handler's future is
    /// boxed where it's made, so no layer that carries it copies it whole; and it still answers.
    #[test]
    fn g9_s5_a_requests_future_is_under_a_kilobyte_whatever_its_handlers() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let request = || http::Request::builder().uri("/api/me").header(header::AUTHORIZATION, "Bearer test-admin-token").body(Incoming::empty());
            let conn = crate::overload::Connection::new();
            let handling = Arc::new(Big).handle(request()?);
            let whole = size_of_val(&handling);
            drop(handling);
            let req = request()?;
            let serving = conn.begin(&req);
            let fut = answer(Arc::new(Big), req, serving);
            let kept = size_of_val(&fut);
            anyhow::ensure!(whole >= 64 << 10 && kept < 1024, "the handler's future {whole} bytes, the request's as kept {kept}");
            let resp = fut.await;
            anyhow::ensure!(resp.status() == StatusCode::OK, "it answers: {}", resp.status());
            // kiln's API's own, on a hub of its own.
            let hub = crate::hub::tests::scratch_hub().await?;
            let api = Arc::new(crate::api::Api { hub });
            let handled = size_of_val(&Handler::handle(Arc::clone(&api), request()?));
            let req = request()?;
            let serving = conn.begin(&req);
            let kept = size_of_val(&answer(api, req, serving));
            anyhow::ensure!(kept < 1024, "the API's handler future {handled} bytes, the request's as kept {kept}");
            Ok(())
        })
    }

    /// What `f` allocated on this thread, and its output.
    fn allocating<T>(f: impl FnOnce() -> T) -> (T, u64) {
        let before = crate::os::counted::made().blocks;
        let out = f();
        (out, crate::os::counted::made().blocks.saturating_sub(before))
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what a request's connection puts on it is one extension, where it came from,
    /// whether over TLS and its class, and one allocation; the router's caller after it is one more, its table room
    /// enough for kiln-http's arrival time, the connection's and the caller's.
    #[test]
    fn g9_s5_a_requests_arrival_is_one_extension() -> anyhow::Result<()> {
        let addr = std::net::SocketAddr::from(([10, 0, 0, 7], 4242));
        let mut req = http::Request::builder().uri("/api/me").body(Incoming::empty())?;
        req.extensions_mut().insert(kiln_http::Received(std::time::Instant::now()));
        let ((), marking) = allocating(|| mark(&mut req, Some(addr), true, crate::overload::Class::Normal));
        assert_eq!(marking, 1, "one extension, one allocation");
        assert_eq!((remote_addr(&req), remote_ip(&req), secure(&req)), (Some(addr), Some(addr.ip()), true));
        assert_eq!(req.extensions().get::<Arrival>().map(|a| a.class), Some(crate::overload::Class::Normal));
        let ((), calling) = allocating(|| {
            req.extensions_mut().insert(crate::authz::Caller::owner());
        });
        assert_eq!(calling, 2, "the caller's name and its box: the table had room");
        assert_eq!(req.extensions().len(), 3);
        let plain = http::Request::builder().uri("/").body(Incoming::empty())?;
        assert_eq!((remote_ip(&plain), secure(&plain)), (None, false), "a request no connection marked");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an answer's body goes to its connection in its guard as it is (no box made
    /// for it), and sends what it did.
    #[test]
    fn g9_s5_an_answers_body_goes_in_its_guard_unboxed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let conn = crate::overload::Connection::new();
            let req = http::Request::builder().uri("/api/me").body(Incoming::empty())?;
            let serving = conn.begin(&req);
            let resp = text(StatusCode::OK, "said");
            let (guarded, made) = allocating(|| crate::overload::while_sent(resp, serving));
            anyhow::ensure!(made == 0, "{made} allocations to guard it");
            let body = guarded.into_body().collect().await?.to_bytes();
            anyhow::ensure!(body.as_ref() == b"said", "{body:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an answer's JSON is written where the thread wrote its last and copied out
    /// once at its length: the same bytes, in four allocations however long (the copy, the header map's two, the
    /// body's box), where its own vector grew by doubling and was shared in a block more.
    #[test]
    fn g9_s5_json_is_written_where_the_last_was_and_copied_once() -> anyhow::Result<()> {
        let value: Vec<serde_json::Value> = (0..200).map(|i| serde_json::json!({ "id": format!("session-{i}"), "seq": i, "text": "the retry loop reads its deadline" })).collect();
        let whole = serde_json::to_vec(&value)?;
        anyhow::ensure!(whole.len() > 10 << 10, "{} bytes", whole.len());
        let (_, own) = allocating(|| Bytes::from(serde_json::to_vec(&value).unwrap_or_default()));
        let _ = json(StatusCode::OK, &value);
        let (resp, made) = allocating(|| json(StatusCode::OK, &value));
        anyhow::ensure!(made <= 4 && made < own + 3, "{made} allocations to answer; its own vector alone took {own}");
        let body = crate::rt::block_on(async move { Ok(resp.into_body().collect().await?.to_bytes()) })?;
        anyhow::ensure!(body.as_ref() == whole.as_slice(), "the same bytes");
        let ok = serde_json::json!({ "ok": true });
        let (small, made) = allocating(|| json(StatusCode::OK, &ok));
        anyhow::ensure!(made <= 4 && small.status() == StatusCode::OK, "{made} allocations for a small one");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an HTTP/2 request's cookies (a browser sends each apart) are joined into one
    /// buffer made at their length, the header's own: one allocation, where there were three; its `Host`, given by
    /// its connection, stands.
    #[test]
    fn g9_s5_an_http2_requests_cookies_are_joined_in_one_allocation() -> anyhow::Result<()> {
        // Its target shared, as kiln-http decodes it (a clone of it counts, copies nothing).
        let mut req = http::Request::builder()
            .uri(http::Uri::from_maybe_shared(Bytes::from_static(b"https://kiln.dev/api/runs"))?)
            .header(header::HOST, "kiln.dev")
            .header(header::COOKIE, "kiln_session=abc")
            .header(header::COOKIE, "theme=dark")
            .header(header::COOKIE, "tz=utc")
            .body(())?;
        let ((), made) = allocating(|| as_http1(&mut req));
        anyhow::ensure!(made == 1, "{made} allocations");
        assert_eq!(req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()), Some("kiln_session=abc; theme=dark; tz=utc"));
        assert_eq!(req.headers().get_all(header::COOKIE).iter().count(), 1);
        assert_eq!((req.headers().get(header::HOST).and_then(|v| v.to_str().ok()), req.uri().to_string()), (Some("kiln.dev"), "/api/runs".to_owned()));
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a path's segments are split on the stack, as many as a route names, with no
    /// allocation; a path of more has them all in a vector.
    #[test]
    fn g9_s5_a_paths_segments_are_split_on_the_stack() {
        use std::borrow::Cow;
        let on_stack = |path: &'static str| {
            let mut room = [""; ROUTE_DEPTH];
            allocating(|| match split_path(path, &mut room) {
                Cow::Borrowed(segs) => Some(segs.join("|")),
                Cow::Owned(_) => None,
            })
        };
        assert_eq!(on_stack("/api/harness/sessions/a//events/").0.as_deref(), Some("api|harness|sessions|a|events"));
        let (deepest, _) = on_stack("/api/uploads/repos/o/r/releases/1/assets");
        assert_eq!(deepest.as_deref(), Some("api|uploads|repos|o|r|releases|1|assets"), "the deepest route's on the stack");
        let mut room = [""; ROUTE_DEPTH];
        let (borrowed, made) = allocating(|| matches!(split_path("/api/me", &mut room), Cow::Borrowed(_)));
        assert!(borrowed && made == 0, "{made} allocations to split it");
        let mut room = [""; ROUTE_DEPTH];
        let deep = split_path("/v/x/a/b/c/d/e/f/g", &mut room);
        assert!(matches!(deep, std::borrow::Cow::Owned(_)));
        assert_eq!(&*deep, ["v", "x", "a", "b", "c", "d", "e", "f", "g"]);
        let mut room = [""; ROUTE_DEPTH];
        assert!(split_path("/", &mut room).is_empty());
    }
}
