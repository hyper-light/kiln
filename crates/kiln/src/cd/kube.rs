//! A cluster's API server, from the kiln server: kiln-cd's request paths over kiln's own HTTP
//! stack (hyper on hyper-rt, TLS by hyper-tls).
//!
//! Requests reuse kept-alive connections from a small pool per cluster; watches hold
//! connections of their own. Credentials are refreshed as they expire: token files are read
//! again on every request (service-account tokens rotate), credential plugins are run again a
//! minute before their tokens expire.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderValue, Method, header};
use http_body_util::BodyExt as _;
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use kiln_cd::kube::{self as k, Auth, Discovery, Endpoint, Resource};
use kiln_cd::obj::{self, GroupKind, ResKey};
use parking_lot::Mutex;
use serde_json::Value;

use crate::http::client::{Identity, Roots};
use crate::http::io::{RtFuturesIo, RtIo};
use crate::http::server::{Body, empty, full};
use crate::http::tls::{ClientTls, TlsStream};

/// Longest a request waits for its answer.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes a (non-watch) response may hold.
const BODY_LIMIT: usize = 256 << 20;
/// Kept-alive connections a cluster's pool holds.
const POOL: usize = 32;
/// What most requests ask for.
const JSON: &str = "application/json";
/// Lists of objects' metadata alone.
const METADATA: &str = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1,application/json";
/// Logs are text.
const ANY: &str = "*/*";

/// One cluster's API server.
pub struct Cluster {
    /// kiln's name for it.
    pub name: String,
    endpoint: Endpoint,
    tls: ClientTls,
    host: String,
    port: u16,
    https: bool,
    /// A path the server's URL carries (Rancher and other proxies), before `/api`.
    prefix: String,
    sni: String,
    pool: Mutex<Vec<SendRequest<Body>>>,
    exec: Mutex<Option<(String, Instant)>>,
    /// What discovery found.
    pub discovery: parking_lot::RwLock<Arc<Discovery>>,
    /// `gitVersion` without the `v`, for Helm's `--kube-version`.
    pub version: parking_lot::RwLock<Option<String>>,
    /// When discovery last ran, so kinds it doesn't know send it again only now and then.
    discovered_at: Mutex<Option<Instant>>,
}

impl Cluster {
    /// A client for `endpoint`.
    pub fn new(name: &str, endpoint: Endpoint) -> Result<Cluster> {
        if endpoint.insecure {
            bail!("cluster {name}: skipping TLS verification is not supported; give its CA instead");
        }
        let uri: http::Uri = endpoint.server.parse().with_context(|| format!("cluster {name}: bad server URL {}", endpoint.server))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            other => bail!("cluster {name}: unsupported scheme {other:?}"),
        };
        let host = uri.host().context("the server URL has no host")?.trim_matches(['[', ']']).to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let roots = match &endpoint.ca {
            Some(ca) => Roots::Pem(ca.clone()),
            None => Roots::WebPki,
        };
        let identity = match &endpoint.auth {
            Auth::Certificate { cert, key } => Some(Identity { cert_pem: cert.clone(), key_pem: key.clone() }),
            _ => None,
        };
        let sni = endpoint.tls_server_name.clone().unwrap_or_else(|| host.clone());
        Ok(Cluster {
            name: name.to_owned(),
            tls: ClientTls::new(roots, identity)?,
            prefix: uri.path().trim_end_matches('/').to_owned(),
            host,
            port,
            https,
            sni,
            endpoint,
            pool: Mutex::new(Vec::new()),
            exec: Mutex::new(None),
            discovery: parking_lot::RwLock::new(Arc::new(Discovery::default())),
            version: parking_lot::RwLock::new(None),
            discovered_at: Mutex::new(None),
        })
    }

    /// The `Authorization` header, if any.
    async fn authorization(&self) -> Result<Option<String>> {
        Ok(match &self.endpoint.auth {
            Auth::None | Auth::Certificate { .. } => None,
            Auth::Token(t) => Some(format!("Bearer {t}")),
            Auth::TokenFile(p) => {
                let p = p.clone();
                let t = crate::rt::blocking(move || std::fs::read_to_string(p)).await??;
                Some(format!("Bearer {}", t.trim()))
            }
            Auth::Basic { username, password } => {
                Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))))
            }
            Auth::Exec(exec) => {
                if let Some((t, until)) = self.exec.lock().clone()
                    && Instant::now() < until
                {
                    return Ok(Some(format!("Bearer {t}")));
                }
                let env = k::exec_env(exec, &self.endpoint);
                let (cmd, args) = (exec.command.clone(), exec.args.clone());
                let out = crate::rt::blocking(move || std::process::Command::new(cmd).args(args).envs(env).output()).await??;
                if !out.status.success() {
                    bail!("credential plugin {} failed: {}", exec.command, String::from_utf8_lossy(&out.stderr).trim());
                }
                let cred = k::exec_credential(&String::from_utf8_lossy(&out.stdout))?;
                let token = cred.token.context("the credential plugin gave a client certificate; only tokens are supported")?;
                // Until a minute before it expires (fifteen minutes when it doesn't say).
                let life = cred
                    .expires
                    .and_then(|e| time::OffsetDateTime::parse(&e, &time::format_description::well_known::Rfc3339).ok())
                    .map(|t| (t - time::OffsetDateTime::now_utc()).whole_seconds().max(0) as u64)
                    .unwrap_or(900);
                *self.exec.lock() = Some((token.clone(), Instant::now() + Duration::from_secs(life.saturating_sub(60))));
                Some(format!("Bearer {token}"))
            }
        })
    }

    async fn connect(&self) -> Result<SendRequest<Body>> {
        let tcp = crate::http::io::connect(&self.host, self.port).await?;
        if self.https {
            let name = crate::http::tls::server_name(&self.sni)?;
            let stream = TlsStream::connect(RtFuturesIo::new(tcp), self.tls.config()?, name)
                .await
                .map_err(|e| anyhow::anyhow!("TLS to cluster {}: {e}", self.name))?;
            let (sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(stream).await?;
            crate::rt::spawn_local(async move {
                let _ = conn.await;
            })
            .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
            Ok(sender)
        } else {
            let (sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(RtIo::new(tcp)).await?;
            crate::rt::spawn_local(async move {
                let _ = conn.await;
            })
            .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
            Ok(sender)
        }
    }

    fn build(&self, method: &Method, path: &str, ctype: Option<&str>, body: Option<&Bytes>, auth: Option<&str>, accept: &str) -> Result<http::Request<Body>> {
        let authority = if (self.https && self.port == 443) || (!self.https && self.port == 80) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let mut b = http::Request::builder()
            .method(method.clone())
            .uri(format!("{}{path}", self.prefix))
            .header(header::HOST, authority)
            .header(header::USER_AGENT, "kiln")
            .header(header::ACCEPT, accept);
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, HeaderValue::from_str(a)?);
        }
        if let Some(ct) = ctype {
            b = b.header(header::CONTENT_TYPE, ct);
        }
        Ok(b.body(match body {
            Some(bytes) => full(bytes.clone()),
            None => empty(),
        })?)
    }

    /// One request; the whole answer.
    pub async fn request(&self, method: Method, path: &str, ctype: Option<&str>, body: Option<Bytes>) -> Result<(u16, Bytes)> {
        self.request_as(method, path, ctype, body, JSON).await
    }

    /// One request asking for `accept`; the whole answer.
    pub async fn request_as(&self, method: Method, path: &str, ctype: Option<&str>, body: Option<Bytes>, accept: &str) -> Result<(u16, Bytes)> {
        let auth = self.authorization().await?;
        let fut = async {
            // A kept-alive connection may have closed since: try the next, then a new one.
            for _ in 0..3 {
                let pooled = self.pool.lock().pop();
                let mut sender = match pooled {
                    Some(s) if !s.is_closed() => s,
                    Some(_) => continue,
                    None => self.connect().await?,
                };
                if sender.ready().await.is_err() {
                    continue;
                }
                let req = self.build(&method, path, ctype, body.as_ref(), auth.as_deref(), accept)?;
                let resp = match sender.send_request(req).await {
                    Ok(r) => r,
                    Err(e) if e.is_canceled() || e.is_closed() => continue,
                    Err(e) => return Err(anyhow::anyhow!("cluster {}: {e}", self.name)),
                };
                let status = resp.status().as_u16();
                let bytes = http_body_util::Limited::new(resp.into_body(), BODY_LIMIT)
                    .collect()
                    .await
                    .map_err(|e| anyhow::anyhow!("cluster {}: reading the answer: {e}", self.name))?
                    .to_bytes();
                let mut pool = self.pool.lock();
                if pool.len() < POOL {
                    pool.push(sender);
                }
                return Ok((status, bytes));
            }
            bail!("cluster {}: no connection would take the request", self.name)
        };
        match crate::rt::timeout(TIMEOUT, fut).await {
            Ok(r) => r,
            Err(_) => bail!("cluster {}: no answer within {} s", self.name, TIMEOUT.as_secs()),
        }
    }

    /// A long-lived request (a watch) on a connection of its own.
    pub async fn stream(&self, path: &str) -> Result<http::Response<Incoming>> {
        self.stream_as(path, JSON).await
    }

    /// A long-lived request asking for `accept` (a log that follows) on a connection of its own.
    pub async fn stream_as(&self, path: &str, accept: &str) -> Result<http::Response<Incoming>> {
        let auth = self.authorization().await?;
        let mut sender = crate::rt::timeout(TIMEOUT, self.connect()).await.map_err(|_| anyhow::anyhow!("connecting timed out"))??;
        let req = self.build(&Method::GET, path, None, None, auth.as_deref(), accept)?;
        let resp = crate::rt::timeout(TIMEOUT, sender.send_request(req)).await.map_err(|_| anyhow::anyhow!("no answer"))??;
        Ok(resp)
    }

    async fn get_json(&self, path: &str, accept_aggregated: bool) -> Result<Value> {
        let auth = self.authorization().await?;
        let mut sender = self.connect().await?;
        let mut req = self.build(&Method::GET, path, None, None, auth.as_deref(), JSON)?;
        if accept_aggregated {
            req.headers_mut().insert(
                header::ACCEPT,
                HeaderValue::from_static("application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,application/json;g=apidiscovery.k8s.io;v=v2beta1;as=APIGroupDiscoveryList,application/json"),
            );
        }
        let resp = crate::rt::timeout(TIMEOUT, sender.send_request(req)).await.map_err(|_| anyhow::anyhow!("no answer"))??;
        let status = resp.status().as_u16();
        let body = http_body_util::Limited::new(resp.into_body(), BODY_LIMIT).collect().await.map_err(|e| anyhow::anyhow!("{e}"))?.to_bytes();
        if !(200..300).contains(&status) {
            bail!("{path}: {}", k::status_message(status, &body));
        }
        Ok(serde_json::from_slice(&body)?)
    }

    /// Discovers every kind the cluster serves, and its version.
    pub async fn discover(&self) -> Result<Arc<Discovery>> {
        *self.discovered_at.lock() = Some(Instant::now());
        let mut d = Discovery::default();
        for path in ["/api", "/apis"] {
            let doc = self.get_json(path, true).await?;
            if doc.get("kind").and_then(Value::as_str) != Some("APIGroupDiscoveryList") {
                bail!("cluster {}: its API server is older than Kubernetes 1.30 (no aggregated discovery)", self.name);
            }
            d.add_aggregated(&doc);
        }
        let v = self.get_json("/version", false).await?;
        let version = obj::str_at(&v, &["gitVersion"]).map(|g| g.trim_start_matches('v').split(['-', '+']).next().unwrap_or("").to_owned());
        *self.version.write() = version;
        let d = Arc::new(d);
        *self.discovery.write() = d.clone();
        Ok(d)
    }

    fn resource(&self, group: &str, kind: &str) -> Result<Resource> {
        // Worded as the API server words it, so a dry run of a kind whose CRD the same sync
        // applies reads as the expected miss it is.
        self.discovery
            .read()
            .get(group, kind)
            .cloned()
            .with_context(|| format!("{group}/{kind} not found: the server could not find the requested resource (cluster {} doesn't serve it)", self.name))
    }

    /// Whether discovery found `gk`.
    pub fn knows(&self, gk: &GroupKind) -> bool {
        self.discovery.read().get(&gk.group, &gk.kind).is_some()
    }

    /// Claims a discovery if the last ran at least `every` ago, so callers don't all run one.
    pub fn claim_discovery(&self, every: Duration) -> bool {
        let mut at = self.discovered_at.lock();
        if at.is_some_and(|t| t.elapsed() < every) {
            return false;
        }
        *at = Some(Instant::now());
        true
    }

    /// Server-side apply of `object` as `manager`: the object as stored.
    pub async fn apply(&self, object: &Value, manager: &str, dry_run: bool) -> Result<Value> {
        let key = ResKey::new(obj::group(object), obj::kind(object), obj::namespace(object), obj::name(object));
        let r = self.resource(&key.group, &key.kind)?;
        let path = format!("{}?{}", k::object_path(&r, &key), k::apply_query(manager, true, dry_run));
        let body = Bytes::from(serde_json::to_vec(object)?);
        let (status, bytes) = self.request(Method::PATCH, &path, Some("application/apply-patch+yaml"), Some(body)).await?;
        if !(200..300).contains(&status) {
            bail!("{}", k::status_message(status, &bytes));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Deletes an object; `Ok` when it is gone or going.
    pub async fn delete(&self, key: &ResKey, propagation: &str) -> Result<()> {
        let r = self.resource(&key.group, &key.kind)?;
        let body = serde_json::json!({"kind": "DeleteOptions", "apiVersion": "v1", "propagationPolicy": propagation});
        let (status, bytes) = self.request(Method::DELETE, &k::object_path(&r, key), Some("application/json"), Some(Bytes::from(body.to_string()))).await?;
        match status {
            200..=299 => Ok(()),
            404 => bail!("{key} not found"),
            _ => bail!("{}", k::status_message(status, &bytes)),
        }
    }

    /// One object, if it exists.
    pub async fn get(&self, key: &ResKey) -> Result<Option<Value>> {
        let r = self.resource(&key.group, &key.kind)?;
        let (status, bytes) = self.request(Method::GET, &k::object_path(&r, key), None, None).await?;
        match status {
            200..=299 => Ok(Some(serde_json::from_slice(&bytes)?)),
            404 => Ok(None),
            _ => bail!("{}", k::status_message(status, &bytes)),
        }
    }

    /// Every object of a kind (matching `selector`), and the list's resource version. List items
    /// carry no `kind` or `apiVersion`; they get them back here.
    pub async fn list(&self, gk: &GroupKind, selector: Option<&str>) -> Result<(Vec<Value>, String)> {
        let r = self.resource(&gk.group, &gk.kind)?;
        let api_version = if r.group.is_empty() { r.version.clone() } else { format!("{}/{}", r.group, r.version) };
        let mut out = Vec::new();
        let mut cont = String::new();
        loop {
            let mut q = String::from("limit=500");
            if let Some(s) = selector {
                q.push_str(&format!("&labelSelector={}", k::enc(s)));
            }
            if !cont.is_empty() {
                q.push_str(&format!("&continue={}", k::enc(&cont)));
            }
            let (status, bytes) = self.request(Method::GET, &format!("{}?{q}", k::collection_path(&r, None)), None, None).await?;
            if !(200..300).contains(&status) {
                bail!("listing {gk}: {}", k::status_message(status, &bytes));
            }
            let mut doc: Value = serde_json::from_slice(&bytes)?;
            let rv = obj::str_at(&doc, &["metadata", "resourceVersion"]).unwrap_or("").to_owned();
            cont = obj::str_at(&doc, &["metadata", "continue"]).unwrap_or("").to_owned();
            if let Some(Value::Array(items)) = doc.get_mut("items").map(Value::take) {
                for mut item in items {
                    if let Some(m) = item.as_object_mut() {
                        m.insert("apiVersion".into(), Value::String(api_version.clone()));
                        m.insert("kind".into(), Value::String(gk.kind.to_string()));
                    }
                    out.push(item);
                }
            }
            if cont.is_empty() {
                return Ok((out, rv));
            }
        }
    }

    /// The objects of a kind in one namespace (every namespace when `None`, or when the kind is
    /// cluster-scoped) matching `selector`, as [`Cluster::list`] gives them. With `metadata_only`,
    /// the API server sends each object's metadata alone (a `PartialObjectMetadataList`): what a
    /// tree needs of kinds whose bodies are large or secret.
    pub async fn list_in(&self, gk: &GroupKind, namespace: Option<&str>, selector: Option<&str>, metadata_only: bool) -> Result<Vec<Value>> {
        let r = self.resource(&gk.group, &gk.kind)?;
        let api_version = if r.group.is_empty() { r.version.clone() } else { format!("{}/{}", r.group, r.version) };
        let accept = if metadata_only { METADATA } else { JSON };
        let mut out = Vec::new();
        let mut cont = String::new();
        loop {
            let mut q = String::from("limit=500");
            if let Some(s) = selector {
                q.push_str(&format!("&labelSelector={}", k::enc(s)));
            }
            if !cont.is_empty() {
                q.push_str(&format!("&continue={}", k::enc(&cont)));
            }
            let path = format!("{}?{q}", k::collection_path(&r, namespace));
            let (status, bytes) = self.request_as(Method::GET, &path, None, None, accept).await?;
            if !(200..300).contains(&status) {
                bail!("listing {gk}{}: {}", namespace.map(|n| format!(" in {n}")).unwrap_or_default(), k::status_message(status, &bytes));
            }
            let mut doc: Value = serde_json::from_slice(&bytes)?;
            cont = obj::str_at(&doc, &["metadata", "continue"]).unwrap_or("").to_owned();
            if let Some(Value::Array(items)) = doc.get_mut("items").map(Value::take) {
                for mut item in items {
                    if let Some(m) = item.as_object_mut() {
                        m.insert("apiVersion".into(), Value::String(api_version.clone()));
                        m.insert("kind".into(), Value::String(gk.kind.to_string()));
                    }
                    out.push(item);
                }
            }
            if cont.is_empty() {
                return Ok(out);
            }
        }
    }

    /// Whether the cluster serves a kind, by discovery.
    pub fn serves(&self, group: &str, kind: &str) -> bool {
        self.discovery.read().get(group, kind).is_some()
    }

    /// A GET whose answer is JSON, or why not.
    pub async fn get_path(&self, path: &str) -> Result<Value> {
        let (status, bytes) = self.request(Method::GET, path, None, None).await?;
        if !(200..300).contains(&status) {
            bail!("{}", k::status_message(status, &bytes));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// A pod's log (`query` as the log endpoint takes it: `container`, `tailLines`, `sinceSeconds`,
    /// `timestamps`, `previous`, `limitBytes`).
    pub async fn pod_log(&self, namespace: &str, pod: &str, query: &str) -> Result<Bytes> {
        let path = format!("/api/v1/namespaces/{}/pods/{}/log?{query}", k::enc(namespace), k::enc(pod));
        let (status, bytes) = self.request_as(Method::GET, &path, None, None, ANY).await?;
        if !(200..300).contains(&status) {
            bail!("{}", k::status_message(status, &bytes));
        }
        Ok(bytes)
    }

    /// A pod's log as it is written (`follow=true` is added to `query`), on a connection of its own.
    pub async fn follow_log(&self, namespace: &str, pod: &str, query: &str) -> Result<http::Response<Incoming>> {
        let path = format!("/api/v1/namespaces/{}/pods/{}/log?follow=true&{query}", k::enc(namespace), k::enc(pod));
        self.stream_as(&path, ANY).await
    }

    /// The watch request's path for a kind from `rv`.
    pub fn watch_path(&self, gk: &GroupKind, rv: &str, selector: Option<&str>) -> Result<String> {
        let r = self.resource(&gk.group, &gk.kind)?;
        if !r.watch {
            bail!("{gk} can't be watched");
        }
        let mut q = format!("watch=1&allowWatchBookmarks=true&timeoutSeconds=300&resourceVersion={}", k::enc(rv));
        if let Some(s) = selector {
            q.push_str(&format!("&labelSelector={}", k::enc(s)));
        }
        Ok(format!("{}?{q}", k::collection_path(&r, None)))
    }
}

/// Reads the next chunk of a streamed body.
pub async fn next_chunk(body: &mut Incoming) -> Result<Option<Bytes>> {
    crate::http::server::next_chunk(body).await
}
