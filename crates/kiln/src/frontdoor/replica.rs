//! A runner's front door while it holds a replica of kiln's records (docs/p2p.md, P2): requests
//! are served one by one. The owner's reads of what the replica holds ([`crate::reads`]) are
//! answered here, once the replica holds every write the server had made when asked; everything
//! else goes to the server as it came, a WebSocket piped through.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use http_body_util::BodyExt as _;

use super::proxy::Upstream;
use crate::http::io::{RtFuturesIo, RtIo};
use crate::http::server::{self as srv, Request, Response};
use crate::http::tls::{ClientTls, TlsStream};
use crate::store::learner::Learner;

/// How long a read waits for the replica to hold the server's writes; past it, the server answers.
const CURRENT_WITHIN: Duration = Duration::from_secs(2);
/// How long the server's word on whose credentials are the owner's is kept.
const OWNER_FOR: Duration = Duration::from_secs(30);
/// Credentials remembered at once, at most.
const OWNERS: usize = 1024;
/// After the replica didn't catch up in time, how long the server answers every read at once
/// (a replica that no longer follows costs a read nothing).
const STALE_FOR: Duration = Duration::from_secs(30);
/// Headers about one connection, never forwarded either way.
const HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];
/// What an answer from the replica says: the runner, and the last write its replica held.
pub const READ_HEADER: &str = "kiln-read";

/// The front door's handler with a replica here.
pub struct Door {
    upstream: Upstream,
    /// For a plain-HTTP upstream, which needs none.
    plain: Arc<ClientTls>,
    learner: Arc<Learner>,
    machine: String,
    owners: parking_lot::Mutex<HashMap<String, (bool, Instant)>>,
    /// Until when the server answers every read: the replica didn't catch up the last time.
    stale_until: parking_lot::Mutex<Option<Instant>>,
}

impl Door {
    pub fn new(upstream: Upstream, learner: Arc<Learner>) -> anyhow::Result<Self> {
        let plain = Arc::new(ClientTls::new(crate::http::client::Roots::WebPki, None)?);
        let machine = learner.machine().to_owned();
        Ok(Self { upstream, plain, learner, machine, owners: parking_lot::Mutex::new(HashMap::new()), stale_until: parking_lot::Mutex::new(None) })
    }

    /// Whether it's the handler for this replica and this upstream.
    pub fn serves(&self, learner: &Arc<Learner>, upstream: &Upstream) -> bool {
        Arc::ptr_eq(&self.learner, learner) && self.upstream.url == upstream.url
    }

    fn tls(&self) -> &ClientTls {
        self.upstream.tls.as_deref().unwrap_or(&self.plain)
    }

    /// The replica's answer to the owner's read of a route it answers, once it holds every write
    /// the server had made; `None`: the server answers.
    async fn read(&self, req: &Request) -> Option<Response> {
        if req.method() != Method::GET || crate::http::ws::is_upgrade(req) {
            return None;
        }
        let seg: Vec<String> = req.uri().path().trim_matches('/').split('/').filter(|s| !s.is_empty()).map(srv::decode).collect();
        let s: Vec<&str> = seg.iter().map(String::as_str).collect();
        if !crate::reads::answers(&s) || self.stale_until.lock().is_some_and(|t| Instant::now() < t) || !self.owner(req.headers()).await {
            return None;
        }
        if !self.learner.current(CURRENT_WITHIN).await {
            *self.stale_until.lock() = Some(Instant::now() + STALE_FOR);
            return None;
        }
        let v = crate::reads::answer(self.learner.store(), &s, &srv::query(req)).await?;
        let r = srv::json(StatusCode::OK, &v);
        Some(srv::with_header(r, HeaderName::from_static(READ_HEADER), &format!("{} {}", self.machine, self.learner.standing().0)))
    }

    /// Whether the request's credentials are the owner's, as the server says (kept a while).
    async fn owner(&self, headers: &HeaderMap) -> bool {
        let get = |h: header::HeaderName| headers.get(h).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        let (cookie, auth) = (get(header::COOKIE), get(header::AUTHORIZATION));
        if cookie.is_empty() && auth.is_empty() {
            return false;
        }
        let key = crate::util::sha256_hex(format!("{cookie}\n{auth}").as_bytes());
        if let Some((owner, at)) = self.owners.lock().get(&key).copied()
            && at.elapsed() < OWNER_FOR
        {
            return owner;
        }
        let asked = async {
            let mut req = http::Request::builder().method(Method::GET).uri("/api/cluster/owner").body(srv::empty())?;
            for (k, v) in [(header::HOST, self.authority()), (header::COOKIE, cookie), (header::AUTHORIZATION, auth)] {
                if !v.is_empty() {
                    req.headers_mut().insert(k, HeaderValue::from_str(&v)?);
                }
            }
            let resp = crate::http::io::send(self.tls(), &self.upstream.host, self.upstream.port, self.upstream.tls.is_some(), req).await?;
            anyhow::ensure!(resp.status().is_success(), "the server answered {}", resp.status());
            let body = resp.into_body().collect().await?.to_bytes();
            Ok::<_, anyhow::Error>(serde_json::from_slice::<serde_json::Value>(&body)?.get("owner").and_then(serde_json::Value::as_bool).unwrap_or(false))
        };
        // Not known (the server away): not the owner's, for now, and asked again next time.
        let Ok(owner) = asked.await else { return false };
        let mut owners = self.owners.lock();
        if owners.len() >= OWNERS {
            owners.retain(|_, (_, at)| at.elapsed() < OWNER_FOR);
            if owners.len() >= OWNERS {
                owners.clear();
            }
        }
        owners.insert(key, (owner, Instant::now()));
        owner
    }

    fn authority(&self) -> String {
        match (self.upstream.tls.is_some(), self.upstream.port) {
            (true, 443) | (false, 80) => self.upstream.host.clone(),
            (_, port) => format!("{}:{port}", self.upstream.host),
        }
    }

    /// `req` to the server as it came: a WebSocket piped, anything else streamed.
    async fn forward(&self, req: Request) -> Response {
        let r = if crate::http::ws::is_upgrade(&req) { self.forward_ws(req).await } else { self.forward_http(req).await };
        r.unwrap_or_else(|e| {
            tracing::debug!(error = %format!("{e:#}"), "front door: kiln's server didn't answer");
            srv::text(StatusCode::BAD_GATEWAY, &format!("kiln's server ({}) didn't answer: {e:#}", self.upstream.url))
        })
    }

    async fn forward_http(&self, req: Request) -> anyhow::Result<Response> {
        let (parts, body) = req.into_parts();
        let mut up = http::Request::builder().method(parts.method).uri(target(&parts.uri)).body(body.map_err(std::io::Error::other).boxed())?;
        *up.headers_mut() = without_hops(&parts.headers);
        let resp = crate::http::io::send(self.tls(), &self.upstream.host, self.upstream.port, self.upstream.tls.is_some(), up).await?;
        let (mut head, body) = resp.into_parts();
        for k in HOP {
            head.headers.remove(k);
        }
        Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()))
    }

    /// A WebSocket: the upgrade asked of the server first, its answer the browser's; then both
    /// connections piped until either ends.
    async fn forward_ws(&self, mut req: Request) -> anyhow::Result<Response> {
        let browser = kiln_http::upgrade::on(&mut req);
        let (parts, _) = req.into_parts();
        let mut up = http::Request::builder().method(Method::GET).uri(target(&parts.uri)).body(srv::empty())?;
        let mut headers = without_hops(&parts.headers);
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        *up.headers_mut() = headers;
        let (host, port) = (self.upstream.host.clone(), self.upstream.port);
        let tcp = crate::http::io::connect(&host, port).await?;
        let mut sender = match &self.upstream.tls {
            Some(ct) => {
                let stream = TlsStream::connect(RtFuturesIo::new(tcp), ct.config()?, crate::http::tls::server_name(&host)?).await.map_err(|e| anyhow::anyhow!("TLS to {host}: {e}"))?;
                let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(stream).await?;
                crate::rt::spawn_local(async move {
                    let _ = conn.with_upgrades().await;
                })
                .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
                sender
            }
            None => {
                let (sender, conn) = kiln_http::client::http1::handshake::<_, srv::Body>(RtIo::new(tcp)).await?;
                crate::rt::spawn_local(async move {
                    let _ = conn.with_upgrades().await;
                })
                .map_err(|e| anyhow::anyhow!("spawn connection task: {e}"))?;
                sender
            }
        };
        let mut resp = sender.send_request(up).await?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let (mut head, body) = resp.into_parts();
            for k in HOP {
                head.headers.remove(k);
            }
            return Ok(http::Response::from_parts(head, body.map_err(std::io::Error::other).boxed()));
        }
        let server = kiln_http::upgrade::on(&mut resp);
        let mut out = http::Response::new(srv::empty());
        *out.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        for k in [header::UPGRADE, header::CONNECTION, header::SEC_WEBSOCKET_ACCEPT, header::SEC_WEBSOCKET_PROTOCOL, header::SEC_WEBSOCKET_EXTENSIONS] {
            if let Some(v) = resp.headers().get(&k) {
                out.headers_mut().insert(k, v.clone());
            }
        }
        crate::rt::spawn_local(async move {
            if let (Ok(b), Ok(s)) = (browser.await, server.await)
                && let Err(e) = super::proxy::pipe(b, s).await
            {
                tracing::debug!(error = %e, "front door: a WebSocket to kiln's server ended");
            }
        })
        .map_err(|e| anyhow::anyhow!("spawn WebSocket pipe: {e}"))?;
        Ok(out)
    }
}

impl srv::Handler for Door {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        if let Some(r) = self.read(&req).await {
            return r;
        }
        self.forward(req).await
    }
}

fn target(uri: &http::Uri) -> String {
    uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str).to_owned()
}

/// A request's headers as they go on: all but those about its connection.
fn without_hops(incoming: &HeaderMap) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in incoming {
        if !HOP.contains(&k.as_str()) {
            h.append(k, v.clone());
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::Context as _;

    use super::*;
    use crate::pipeline;
    use crate::proto::AgentMsg;
    use crate::store::group::{self, Fetch, Group};
    use crate::store::learner::{self as wire, Learners, Msg};
    use crate::store::{Repo, Run, Status, Store};

    /// kiln's server, standing in: whoever brings the cookie `kiln_session=owner` is the owner, and
    /// anything else it answers says it did.
    fn upstream() -> anyhow::Result<String> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", l.local_addr()?);
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let owner = req.to_ascii_lowercase().contains("cookie: kiln_session=owner");
                let body = if path == "/api/cluster/owner" { format!(r#"{{"owner":{owner}}}"#) } else { format!("upstream {path}") };
                let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            }
        });
        Ok(url)
    }

    /// The door, serving plain HTTP on a port of its own (at the front door it serves HTTPS).
    fn serve(door: Arc<Door>) -> anyhow::Result<SocketAddr> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let at = l.local_addr()?;
        crate::rt::block_on(async move {
            crate::rt::spawn(async move {
                let Ok(listener) = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(l), 64) else { return };
                while let Ok(stream) = listener.accept().await {
                    let door = Arc::clone(&door);
                    crate::rt::spawn(async move {
                        let remote = stream.peer_addr().ok();
                        srv::serve_connection(crate::http::io::RtIo::new(stream), door, remote).await;
                    });
                }
            });
            Ok(())
        })?;
        Ok(at)
    }

    /// `GET path` of the door: the status, the head (lowercased) and the body.
    fn get(at: SocketAddr, path: &str, cookie: Option<&str>) -> anyhow::Result<(u16, String, String)> {
        let mut s = std::net::TcpStream::connect(at)?;
        s.set_read_timeout(Some(Duration::from_secs(20)))?;
        let cookie = cookie.map(|c| format!("cookie: {c}\r\n")).unwrap_or_default();
        write!(s, "GET {path} HTTP/1.1\r\nhost: kiln.test\r\n{cookie}connection: close\r\n\r\n")?;
        let mut out = String::new();
        s.read_to_string(&mut out)?;
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((out.as_str(), ""));
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok((status, head.to_ascii_lowercase(), body.to_owned()))
    }

    fn add_run(store: &Store, id: &str) -> anyhow::Result<()> {
        let p = pipeline::parse("name: ci\njobs:\n  a: {steps: [{run: x}]}\n")?;
        let run = Run {
            id: id.into(),
            repo_id: "r1".into(),
            repo_name: "hyper-light/x".into(),
            number: 0,
            pipeline: "ci".into(),
            event: "push".into(),
            branch: "main".into(),
            sha: "abc".into(),
            message: String::new(),
            author: String::new(),
            status: Status::Queued,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        };
        let s = store.clone();
        crate::rt::block_on(async move {
            s.create_run(run, pipeline::plan(&p, &pipeline::Context::default())?).await?;
            Ok(())
        })
    }

    #[test]
    fn the_owner_s_reads_are_answered_from_the_replica_and_everything_else_by_the_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = Store::open_grouped(&dir.path().join("kiln.db"), &dir.path().join("cluster"))?;
        let s = server.clone();
        crate::rt::block_on(async move {
            s.upsert_repo(Repo {
                id: "r1".into(),
                name: "hyper-light/x".into(),
                clone_url: "https://example/x".into(),
                branches: vec![],
                poll_seconds: 60,
                enabled: true,
                created_at: 0,
                last_polled_at: None,
                last_error: None,
                mode: "upstream".into(),
                default_branch: None,
                mirror: None,
            })
            .await?;
            Ok(())
        })?;
        add_run(&server, "first-run")?;
        // The runner's learner, from a copy of the server's kiln.db.
        let member = group::member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        let end = Instant::now() + Duration::from_secs(30);
        while !g.point().1.learners.contains(&member) {
            anyhow::ensure!(Instant::now() < end, "the learner never joined");
            std::thread::sleep(Duration::from_millis(10));
        }
        let fetch: Fetch = {
            let server = server.clone();
            Arc::new(move |dest: &std::path::Path| {
                let (s, d) = (server.clone(), dest.to_path_buf());
                crate::rt::block_on(async move { s.copy_for_learner(d).await })
            })
        };
        let db = dir.path().join("runner").join("kiln.db");
        let member_group = Arc::new(Group::learner(&db, &dir.path().join("runner").join("cluster"), member, fetch)?);
        let (learner, said) = Learner::around(member_group, db, "runner-1")?;
        let learner = Arc::new(learner);
        // The runner's link to the server: the group's messages both ways, and where kiln.db stands.
        let done = Arc::new(AtomicBool::new(false));
        let link = {
            let (server, learner, done, learners) = (server.clone(), Arc::clone(&learner), Arc::clone(&done), Learners::default());
            std::thread::spawn(move || {
                let Some(g) = server.group() else { return };
                let to_runner = g.outgoing();
                let mut id = 0;
                while !done.load(Ordering::SeqCst) {
                    let mut moved = false;
                    while let Ok((to, bytes)) = to_runner.try_recv() {
                        moved = true;
                        id += 1;
                        if to == member {
                            for (m, raft) in wire::frames(id, bytes) {
                                learner.heard(&m, raft);
                            }
                        }
                    }
                    while let Ok(AgentMsg::Cluster { json, raft }) = said.try_recv() {
                        moved = true;
                        match wire::decode(&json) {
                            Some(Msg::Ask { id }) => learner.heard(&Msg::At { id, seq: g.standing().0 }, Vec::new()),
                            Some(m) => learners.heard(g, "runner-1", &m, raft),
                            None => {}
                        }
                    }
                    if !moved {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            })
        };
        let door = Arc::new(Door::new(Upstream::parse(&upstream()?, &[])?, Arc::clone(&learner))?);
        let at = serve(door)?;
        let owner = Some("kiln_session=owner");

        let (status, head, body) = get(at, "/api/runs", owner)?;
        assert_eq!(status, 200, "{head}\n{body}");
        assert!(head.contains("kiln-read: runner-1"), "answered by the runner's replica: {head}");
        assert!(body.contains("first-run"), "{body}");
        // A write the server just made is in the next read: the replica holds what the server held.
        add_run(&server, "second-run")?;
        let (_, head, body) = get(at, "/api/runs", owner)?;
        assert!(head.contains("kiln-read: runner-1") && body.contains("second-run"), "{head}\n{body}");
        let (_, head, body) = get(at, "/api/runs/second-run", owner)?;
        assert!(head.contains("kiln-read") && body.contains("\"jobs\""), "{head}\n{body}");
        // The CI pages' own list too.
        let (_, head, body) = get(at, "/api/ci/runs", owner)?;
        assert!(head.contains("kiln-read") && body.contains("\"runs\""), "{head}\n{body}");

        // Everything else is the server's, as is anyone's but the owner's.
        let (_, head, body) = get(at, "/api/agents", owner)?;
        assert!(!head.contains("kiln-read") && body.contains("upstream /api/agents"), "{head}\n{body}");
        let (_, head, body) = get(at, "/api/runs", Some("kiln_session=someone"))?;
        assert!(!head.contains("kiln-read") && body.contains("upstream /api/runs"), "{head}\n{body}");
        let (_, _, body) = get(at, "/api/runs", None)?;
        assert!(body.contains("upstream /api/runs"), "{body}");
        let (_, _, body) = get(at, "/api/runs/no-such-run", owner)?;
        assert!(body.contains("upstream /api/runs/no-such-run"), "a run the replica hasn't is the server's to say: {body}");

        done.store(true, Ordering::SeqCst);
        link.join().ok();
        Ok(())
    }
}
