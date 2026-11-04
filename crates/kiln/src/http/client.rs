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

    /// [`Self::websocket`], with `headers` on the handshake.
    pub async fn websocket_with(&self, url: &str, headers: &[(&str, &str)], max_message: usize) -> anyhow::Result<super::ws::ClientSocket> {
        match crate::rt::timeout(DEFAULT_TIMEOUT, super::ws::connect_with(&self.tls, url, headers, max_message)).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!("WebSocket to {url} timed out after {DEFAULT_TIMEOUT:?}"),
        }
    }
}

enum Payload {
    None,
    Bytes(Bytes),
    Stream(async_channel::Receiver<Bytes>),
}

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

    /// Stream the request body from a channel (sent chunked); the body ends when the sender drops.
    pub fn body_stream(mut self, rx: async_channel::Receiver<Bytes>) -> Self {
        self.body = Payload::Stream(rx);
        self
    }

    /// Time allowed to connect and receive the response head (bodies are read separately).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

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

    /// The same request again, when its body can be sent again (none, or bytes held; a stream can't).
    fn again(&self) -> Option<Self> {
        let body = match &self.body {
            Payload::None => Payload::None,
            Payload::Bytes(b) => Payload::Bytes(b.clone()),
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
