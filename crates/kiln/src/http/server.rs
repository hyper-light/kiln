//! HTTP/1.1 and HTTP/2 server: kiln-http's connections on hyper-rt. Each accepted connection is one
//! hyper-rt task, its requests' handlers polled in it (an HTTP/2 connection's all side by side);
//! upgrades (WebSocket) are HTTP/1.1's.

use std::future::Future;
use std::sync::Arc;

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

/// The client's address, attached to every request of a connection (request extension).
#[derive(Clone, Copy, Debug)]
pub struct RemoteAddr(pub std::net::SocketAddr);

/// The address a request came from, when the listener knew it.
pub fn remote_ip(req: &Request) -> Option<std::net::IpAddr> {
    req.extensions().get::<RemoteAddr>().map(|r| r.0.ip())
}

/// Attached to every request of a connection that came over TLS (the server's own HTTPS).
#[derive(Clone, Copy, Debug)]
pub struct Secure;

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
                Ok(Ok(s)) if s.alpn_protocol() == Some(super::tls::H2) => Box::pin(serve_h2(s, Bytes::new(), handler, remote, true)).await,
                Ok(Ok(s)) => Box::pin(serve_read(s, Bytes::new(), handler, remote, true)).await,
                Ok(Err(e)) => tracing::debug!(error = %e, ?remote, "TLS handshake failed"),
                Err(_) => tracing::debug!(?remote, "TLS handshake timed out"),
            }
        }
        (Opening::H2, _) => Box::pin(serve_h2(io, Bytes::from(head), handler, remote, false)).await,
        // HTTP/1.1, or TLS without a certificate here: HTTP/1.1 answers what it can.
        _ => Box::pin(serve_read(io, Bytes::from(head), handler, remote, false)).await,
    }
}

/// What every request of a connection carries: where it came from, and whether over TLS.
fn mark(req: &mut Request, remote: Option<std::net::SocketAddr>, secure: bool) {
    if let Some(r) = remote {
        req.extensions_mut().insert(RemoteAddr(r));
    }
    if secure {
        req.extensions_mut().insert(Secure);
    }
}

/// [`serve_connection`], saying whether the connection is TLS ([`Secure`] on its requests).
pub async fn serve_connection_with<I, H>(io: I, handler: Arc<H>, remote: Option<std::net::SocketAddr>, secure: bool)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    serve_read(io, Bytes::new(), handler, remote, secure).await;
}

/// HTTP/1.1 over `io`, whose first bytes `read` were taken off it already.
async fn serve_read<I, H>(io: I, read: Bytes, handler: Arc<H>, remote: Option<std::net::SocketAddr>, secure: bool)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    let service = service_fn(move |mut req: Request| {
        mark(&mut req, remote, secure);
        handler.clone().handle(req)
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
    let service = service_fn(move |mut req: Request| {
        as_http1(&mut req);
        mark(&mut req, remote, secure);
        handler.clone().handle(req)
    });
    let conn = http2::Builder::new()
        // A connection whose client has gone quiet without closing is found and ended.
        .keep_alive_interval(Some(std::time::Duration::from_secs(30)))
        .keep_alive_timeout(std::time::Duration::from_secs(20))
        .max_header_list_size(64 << 10)
        .serve_read(io, read, service);
    if let Err(e) = conn.await {
        tracing::debug!(error = %e, "http/2 connection ended with an error");
    }
}

/// An HTTP/2 request shaped as the HTTP/1.1 request it stands for, so handlers see one shape:
/// its authority (`:authority`) as `Host`, its cookies in one `Cookie` (a browser sends them
/// separately over HTTP/2, RFC 9113 §8.2.3), and its target as a path (origin form).
fn as_http1<B>(req: &mut http::Request<B>) {
    if !req.headers().contains_key(header::HOST)
        && let Some(v) = req.uri().authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        req.headers_mut().insert(header::HOST, v);
    }
    if req.headers().get_all(header::COOKIE).iter().nth(1).is_some() {
        let joined = req.headers().get_all(header::COOKIE).iter().map(HeaderValue::as_bytes).collect::<Vec<_>>().join(&b"; "[..]);
        if let Ok(v) = HeaderValue::from_bytes(&joined) {
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

/// A body fed by a channel of chunks; the response ends when the sender is dropped.
pub fn channel_body(rx: async_channel::Receiver<Bytes>) -> Body {
    use futures::StreamExt as _;
    let frames = rx.map(|chunk| Ok::<_, std::io::Error>(Frame::data(chunk)));
    BodyExt::boxed(http_body_util::StreamBody::new(frames))
}

pub fn response(status: StatusCode, content_type: &str, body: Body) -> Response {
    let mut r = http::Response::new(body);
    *r.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(content_type) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r
}

pub fn json<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(b) => response(status, "application/json", full(b)),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, &format!("serialize: {e}")),
    }
}

pub fn text(status: StatusCode, s: &str) -> Response {
    response(status, "text/plain; charset=utf-8", full(s.to_owned()))
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

    /// Says what it was asked, as the handler saw it.
    struct Echo;

    impl Handler for Echo {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let host = header_str(&req, &header::HOST).unwrap_or("-").to_owned();
            let cookie = header_str(&req, &header::COOKIE).unwrap_or("-").to_owned();
            let secure = req.extensions().get::<Secure>().is_some();
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
}
