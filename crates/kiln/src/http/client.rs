//! HTTP/1.1 client over hyper-rt TCP, with HTTPS from hyper-tls (hyper-raft's rustls fork):
//! GitHub's API, the Kubernetes API, and agent⇄server HTTP (enrollment, source archives,
//! artifacts). One connection per request: the volumes are small and it keeps failure handling
//! trivial (no pool to poison).

use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, header};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;

use super::server::{Body, channel_body, empty, full};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    fn as_http(self) -> http::Method {
        match self {
            Self::Get => http::Method::GET,
            Self::Post => http::Method::POST,
            Self::Put => http::Method::PUT,
            Self::Delete => http::Method::DELETE,
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
}

impl RequestBuilder<'_> {
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
        match crate::rt::timeout(timeout, self.send_inner()).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!("request timed out after {timeout:?}"),
        }
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

        let resp = super::io::send(&self.client.tls, &host, port, https, req).await?;
        Ok(Response { inner: resp })
    }
}

pub struct Response {
    inner: http::Response<Incoming>,
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
