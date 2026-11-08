//! HTTP/1.1 client (kiln-http's) over hyper-rt TCP, with HTTPS from hyper-tls (hyper-raft's rustls fork):
//! GitHub's API, the Kubernetes API, and agent⇄server HTTP (enrollment, source archives,
//! artifacts). One connection per request: the volumes are small and it keeps failure handling
//! trivial (no pool to poison).

use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderName, HeaderValue, header};
use http_body_util::BodyExt as _;
use kiln_http::Incoming;

use super::server::{Body, channel_body, empty, full};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    /// Whether something is there, without its body.
    Head,
}

impl Method {
    fn as_http(self) -> http::Method {
        match self {
            Self::Get => http::Method::GET,
            Self::Post => http::Method::POST,
            Self::Put => http::Method::PUT,
            Self::Patch => http::Method::PATCH,
            Self::Delete => http::Method::DELETE,
            Self::Head => http::Method::HEAD,
        }
    }
}

/// Which certificate authorities an HTTPS client trusts.
pub enum Roots {
    /// The Mozilla root set (webpki-roots), for public services such as GitHub.
    WebPki,
    /// A PEM bundle (e.g. a Kubernetes cluster CA or kiln's own CA).
    Pem(Vec<u8>),
    /// The public roots plus one extra CA certificate in DER (agents: kiln's CA).
    WebPkiPlusDer(Vec<u8>),
}

/// A client certificate (PEM chain + PEM private key).
pub struct Identity {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// An HTTP(S) client. Cheap to share; holds the TLS configuration built once.
/// The process-wide client for GitHub's API (public roots), built once.
pub fn github() -> anyhow::Result<&'static Client> {
    static C: std::sync::OnceLock<Option<Client>> = std::sync::OnceLock::new();
    C.get_or_init(|| match Client::public() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "cannot build the HTTPS client");
            None
        }
    })
    .as_ref()
    .ok_or_else(|| anyhow::anyhow!("HTTPS client unavailable"))
}

pub struct Client {
    tls: super::tls::ClientTls,
    user_agent: &'static str,
}

impl Client {
    pub fn new(roots: Roots, identity: Option<Identity>) -> anyhow::Result<Self> {
        Ok(Self { tls: super::tls::ClientTls::new(roots, identity)?, user_agent: "kiln" })
    }

    /// Trusts the public web roots; the right choice for GitHub and plain `http://` targets.
    pub fn public() -> anyhow::Result<Self> {
        Self::new(Roots::WebPki, None)
    }

    pub fn request(&self, method: Method, url: &str) -> RequestBuilder<'_> {
        RequestBuilder {
            client: self,
            method,
            url: url.to_owned(),
            headers: Vec::new(),
            body: Payload::None,
            timeout: DEFAULT_TIMEOUT,
            reach: None,
        }
    }

    pub fn get(&self, url: &str) -> RequestBuilder<'_> {
        self.request(Method::Get, url)
    }

    pub fn post(&self, url: &str) -> RequestBuilder<'_> {
        self.request(Method::Post, url)
    }

    pub fn put(&self, url: &str) -> RequestBuilder<'_> {
        self.request(Method::Put, url)
    }

    /// Open a WebSocket (`ws(s)://`, or `http(s)://` for the same) trusting this client's roots;
    /// see [`super::ws::connect`].
    pub async fn websocket(&self, url: &str, max_message: usize) -> anyhow::Result<super::ws::ClientSocket> {
        self.websocket_with(url, &[], max_message).await
    }

    /// [`Self::websocket`], with `headers` on the handshake. An opening a kiln server shed did nothing (goal 6, plan
    /// 01a1a59c2abe step 4): it is opened again after the server's `Retry-After`, while that leaves time before the
    /// deadline (a runner's tunnel then opens once the server takes it, not on a path of its own choosing).
    #[allow(clippy::disallowed_methods)] // (b) retry backoff, doubling from a picked start
    pub async fn websocket_with(&self, url: &str, headers: &[(&str, &str)], max_message: usize) -> anyhow::Result<super::ws::ClientSocket> {
        let deadline = Instant::now().checked_add(DEFAULT_TIMEOUT);
        loop {
            let left = deadline.map_or(DEFAULT_TIMEOUT, |d| d.saturating_duration_since(Instant::now()));
            let opened = match crate::rt::timeout(left, super::ws::connect_with(&self.tls, url, headers, max_message)).await {
                Ok(r) => r,
                Err(_) => anyhow::bail!("WebSocket to {url} timed out after {DEFAULT_TIMEOUT:?}"),
            };
            let shed = opened.as_ref().err().and_then(|e| e.downcast_ref::<super::ws::Shed>()).map(|s| s.wait);
            match shed {
                Some(wait) if Instant::now().checked_add(wait).zip(deadline).is_some_and(|(next, end)| next < end) => {
                    tracing::debug!(url, wait_s = wait.as_secs(), "the server shed the WebSocket's opening; opening it again after its Retry-After");
                    crate::rt::sleep(wait).await;
                }
                _ => return opened,
            }
        }
    }
}

enum Payload {
    None,
    Bytes(Bytes),
    Stream(Chunks),
    /// A stream made anew for each send ([`RequestBuilder::body_reopen`]).
    Reopen(Reopen),
}

/// A request's body as it streams: its chunks as they come, from whatever makes them (a lock-free queue of kiln's,
/// `crate::memory::queue`, a file read ahead, `crate::rt::file_source`; or a channel).
pub type Chunks = std::pin::Pin<Box<dyn futures::Stream<Item = Bytes> + Send + Sync>>;

/// What makes a request's streamed body anew for each send (a file read again from its start): a request a kiln
/// server shed is sent again whole (goal 6, plan 01a1a59c2abe step 4).
pub type Reopen = std::sync::Arc<dyn Fn() -> anyhow::Result<Chunks> + Send + Sync>;

pub struct RequestBuilder<'c> {
    client: &'c Client,
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Payload,
    timeout: Duration,
    reach: Option<super::io::Reach>,
}

impl RequestBuilder<'_> {
    /// Only to where `reach` lets a request go: for a URL someone supplied (docs/ci.md W22-AC07).
    pub fn reaching(mut self, reach: &super::io::Reach) -> Self {
        self.reach = Some(reach.clone());
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        let v = format!("Bearer {token}");
        self.header("Authorization", &v)
    }

    pub fn json<T: serde::Serialize>(self, value: &T) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_default();
        self.header("Content-Type", "application/json").body(body)
    }

    pub fn body(mut self, bytes: impl Into<Bytes>) -> Self {
        self.body = Payload::Bytes(bytes.into());
        self
    }

    /// Stream the request body from a stream of chunks (sent chunked): a queue's or a channel's, ending when its
    /// senders drop. A request a kiln server sheds can't be sent again with it ([`Self::body_reopen`] can).
    pub fn body_stream(mut self, rx: impl futures::Stream<Item = Bytes> + Send + Sync + 'static) -> Self {
        self.body = Payload::Stream(Box::pin(rx));
        self
    }

    /// Stream the request body (sent chunked) from what `open` makes, made anew for each send: a request a kiln
    /// server shed is sent again after its `Retry-After` with its body whole (goal 6, plan 01a1a59c2abe step 4).
    pub fn body_reopen<S: futures::Stream<Item = Bytes> + Send + Sync + 'static>(mut self, open: impl Fn() -> anyhow::Result<S> + Send + Sync + 'static) -> Self {
        self.body = Payload::Reopen(std::sync::Arc::new(move || open().map(|s| -> Chunks { Box::pin(s) })));
        self
    }

    /// Stream the request body from the file at `path` (sent chunked), read from its start for each send
    /// ([`Self::body_reopen`]).
    pub fn body_file(self, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        self.body_reopen(move || crate::rt::file_source(path.clone()))
    }

    /// Time allowed to connect and receive the response head (bodies are read separately).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    #[allow(clippy::disallowed_methods)] // (b) retry backoff, doubling from a picked start
    pub async fn send(self) -> anyhow::Result<Response> {
        let timeout = self.timeout;
        let deadline = Instant::now().checked_add(timeout);
        let mut request = self;
        loop {
            let again = request.again();
            let left = deadline.map_or(timeout, |d| d.saturating_duration_since(Instant::now()));
            let resp = match crate::rt::timeout(left, request.send_inner()).await {
                Ok(r) => r?,
                Err(_) => anyhow::bail!("request timed out after {timeout:?}"),
            };
            // A kiln server that shed the request did nothing with it (goal 6, plan 01a1a59c2abe step 4):
            // it is sent again after the server's `Retry-After`, while that leaves time before the deadline.
            match (again, shed_wait(&resp)) {
                (Some(again), Some(wait)) if Instant::now().checked_add(wait).zip(deadline).is_some_and(|(next, end)| next < end) => {
                    tracing::debug!(url = %again.url, wait_s = wait.as_secs(), "the server shed the request; sending it again after its Retry-After");
                    crate::rt::sleep(wait).await;
                    request = again;
                }
                _ => return Ok(resp),
            }
        }
    }

    /// The same request again, when its body can be sent again (none, bytes held, or a stream made anew; a
    /// channel's stream can't).
    fn again(&self) -> Option<Self> {
        let body = match &self.body {
            Payload::None => Payload::None,
            Payload::Bytes(b) => Payload::Bytes(b.clone()),
            Payload::Reopen(open) => Payload::Reopen(std::sync::Arc::clone(open)),
            Payload::Stream(_) => return None,
        };
        Some(Self {
            client: self.client,
            method: self.method,
            url: self.url.clone(),
            headers: self.headers.clone(),
            body,
            timeout: self.timeout,
            reach: self.reach.clone(),
        })
    }

    async fn send_inner(self) -> anyhow::Result<Response> {
        let uri: http::Uri = self.url.parse().map_err(|e| anyhow::anyhow!("bad url {}: {e}", self.url))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            other => anyhow::bail!("unsupported scheme {other:?} in {}", self.url),
        };
        let host = uri.host().ok_or_else(|| anyhow::anyhow!("no host in {}", self.url))?.to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let authority = uri.authority().map(|a| a.as_str().to_owned()).unwrap_or_else(|| host.clone());
        let path = uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned();

        let body: Body = match self.body {
            Payload::None => empty(),
            Payload::Bytes(b) => full(b),
            Payload::Stream(rx) => channel_body(rx),
            Payload::Reopen(open) => channel_body(open()?),
        };
        let mut req = http::Request::builder()
            .method(self.method.as_http())
            .uri(path)
            .header(header::HOST, authority)
            .header(header::USER_AGENT, self.client.user_agent)
            .body(body)?;
        for (k, v) in &self.headers {
            let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) else {
                anyhow::bail!("invalid header {k}");
            };
            req.headers_mut().append(name, value);
        }

        let resp = super::io::send_reaching(&self.client.tls, &host, port, https, req, self.reach.as_ref()).await?;
        Ok(Response { inner: resp })
    }
}

pub struct Response {
    inner: http::Response<Incoming>,
}

/// How long a kiln server asked a request it shed to wait (`x-kiln-shed`, `Retry-After` in seconds:
/// `crate::overload`); `None` for any other answer.
fn shed_wait(resp: &Response) -> Option<Duration> {
    if resp.status() != 503 || resp.header("x-kiln-shed").is_none() {
        return None;
    }
    resp.header("retry-after")?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

impl Response {
    pub fn status(&self) -> u16 {
        self.inner.status().as_u16()
    }

    pub fn is_success(&self) -> bool {
        self.inner.status().is_success()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// Every header, names lowercase, values that aren't text left out.
    pub fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect()
    }

    /// The whole body, refusing more than `limit` bytes.
    pub async fn bytes(self, limit: usize) -> anyhow::Result<Bytes> {
        let collected = http_body_util::Limited::new(self.inner.into_body(), limit)
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("response body: {e}"))?;
        Ok(collected.to_bytes())
    }

    pub async fn json<T: serde::de::DeserializeOwned>(self, limit: usize) -> anyhow::Result<T> {
        let b = self.bytes(limit).await?;
        serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("decode response: {e}"))
    }

    /// Stream the body chunk by chunk (`None` at the end), e.g. for large downloads.
    pub async fn chunk(&mut self) -> anyhow::Result<Option<Bytes>> {
        super::server::next_chunk(self.inner.body_mut()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 6, plan 01a1a59c2abe step 4: a request a kiln server shed (503, `x-kiln-shed`, `Retry-After`) is sent
    /// again after the wait it was given, its body with it, and the answer that follows is the one returned; a 503
    /// without the mark is returned as it is, and a shed answer whose wait would pass the deadline is returned too.
    #[test]
    fn g6_s4_a_shed_request_is_sent_again_after_its_retry_after() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        // Answers, one connection each: shed, then served (echoing the body it read), then a plain 503.
        let server = std::thread::spawn(move || -> anyhow::Result<Vec<String>> {
            let mut bodies = Vec::new();
            for answer in [
                "HTTP/1.1 503 Service Unavailable\r\nretry-after: 1\r\nx-kiln-shed: background\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                "HTTP/1.1 503 Service Unavailable\r\nretry-after: 1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            ] {
                let (mut s, _) = listener.accept()?;
                s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                while !got.windows(4).any(|w| w == b"\r\n\r\n") || !got.ends_with(b"hello") {
                    let n = s.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(buf.get(..n).unwrap_or_default());
                }
                bodies.push(String::from_utf8_lossy(&got).into_owned());
                s.write_all(answer.as_bytes())?;
            }
            Ok(bodies)
        });
        crate::rt::block_on(async move {
            let c = Client::public()?;
            let started = Instant::now();
            let r = c.post(&format!("http://{addr}/api/sync/blobs/x")).body("hello").send().await?;
            anyhow::ensure!(r.status() == 200, "served on the second send: {}", r.status());
            anyhow::ensure!(started.elapsed() >= Duration::from_secs(1), "after the Retry-After: {:?}", started.elapsed());
            let r = c.post(&format!("http://{addr}/api/sync/blobs/x")).body("hello").send().await?;
            anyhow::ensure!(r.status() == 503, "a 503 without kiln's mark is returned: {}", r.status());
            Ok(())
        })?;
        let bodies = server.join().map_err(|_| anyhow::anyhow!("the server thread panicked"))??;
        anyhow::ensure!(bodies.iter().take(2).all(|b| b.ends_with("hello")), "the body went again: {bodies:?}");
        Ok(())
    }

    /// The body of HTTP/1.1 request `raw` (its head, then its body chunked), unchunked.
    fn dechunk(raw: &[u8]) -> Vec<u8> {
        let Some(head) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { return Vec::new() };
        let mut rest = raw.get(head + 4..).unwrap_or_default();
        let mut out = Vec::new();
        while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
            let size = std::str::from_utf8(rest.get(..eol).unwrap_or_default()).ok().and_then(|h| usize::from_str_radix(h.split(';').next().unwrap_or("").trim(), 16).ok()).unwrap_or(0);
            if size == 0 {
                break;
            }
            out.extend_from_slice(rest.get(eol + 2..eol + 2 + size).unwrap_or_default());
            rest = rest.get(eol + 4 + size..).unwrap_or_default();
        }
        out
    }

    /// Goal 6, plan 01a1a59c2abe step 4: a file's upload a kiln server shed (a runner's blob or bundle, a memo's file:
    /// `body_file`, `body_reopen`) is sent again after the wait it was given, the file read again from its start, so
    /// whole each time.
    #[test]
    fn g6_s4_a_shed_upload_of_a_file_is_sent_again_whole() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("blob");
        let content: Vec<u8> = (0..70_000u32).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
        std::fs::write(&path, &content)?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let server = std::thread::spawn(move || -> anyhow::Result<Vec<Vec<u8>>> {
            let mut bodies = Vec::new();
            for answer in [
                "HTTP/1.1 503 Service Unavailable\r\nretry-after: 1\r\nx-kiln-shed: background\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            ] {
                let (mut s, _) = listener.accept()?;
                s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
                let (mut got, mut buf) = (Vec::new(), [0u8; 8192]);
                // A chunked body ends with its last chunk, of no bytes.
                while !got.ends_with(b"\r\n0\r\n\r\n") {
                    let n = s.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(buf.get(..n).unwrap_or_default());
                }
                bodies.push(dechunk(&got));
                s.write_all(answer.as_bytes())?;
            }
            Ok(bodies)
        });
        let upload = path.clone();
        crate::rt::block_on(async move {
            let started = Instant::now();
            let r = Client::public()?.put(&format!("http://{addr}/api/sync/blobs/x")).body_file(upload).send().await?;
            anyhow::ensure!(r.status() == 200, "served on the second send: {}", r.status());
            anyhow::ensure!(started.elapsed() >= Duration::from_secs(1), "after the Retry-After: {:?}", started.elapsed());
            Ok(())
        })?;
        let bodies = server.join().map_err(|_| anyhow::anyhow!("the server thread panicked"))??;
        anyhow::ensure!(bodies.len() == 2 && bodies.iter().all(|b| *b == content), "the file went whole each time: {:?} of {}", bodies.iter().map(Vec::len).collect::<Vec<_>>(), content.len());
        Ok(())
    }

    /// HTTPS to real public endpoints through hyper-tls on hyper-rt (needs network; run with
    /// `cargo test -- --ignored https_public`).
    #[test]
    #[ignore]
    fn https_public_endpoints() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let c = github()?;
            let zen = c.get("https://api.github.com/zen").header("Accept", "application/vnd.github+json").send().await?;
            assert!(zen.is_success(), "github zen: HTTP {}", zen.status());
            let text = zen.bytes(4096).await?;
            assert!(!text.is_empty());
            let org: serde_json::Value = c.get("https://api.github.com/orgs/hyper-light").send().await?.json(1 << 20).await?;
            assert_eq!(org.get("login").and_then(|v| v.as_str()), Some("hyper-light"));
            // A large chunked download, streamed.
            let mut resp = c.get("https://codeload.github.com/hyper-light/cocoa/tar.gz/refs/heads/main").send().await?;
            assert!(resp.is_success(), "codeload: HTTP {}", resp.status());
            let mut n = 0usize;
            while let Some(chunk) = resp.chunk().await? {
                n += chunk.len();
            }
            assert!(n > 1000, "downloaded {n} bytes");
            Ok(())
        })
    }
}
