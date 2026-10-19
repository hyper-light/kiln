//! Peered clusters (docs/peers.md): other kiln clusters this one opens, or that open this one,
//! each peering authorized by hand with codes ([`codes`]) and kept by the server, so every runner
//! knows it and forwards to it ([`forward`]).
//!
//! * **Registering** (`POST /api/peers`): an admin names the cluster and its address as reached
//!   from here, and gets a challenge to carry over.
//! * **Authorizing** (`POST /api/peers/authorize`, on the other cluster): its admin pastes the
//!   challenge, picks what our people may do there (a role) and gets a response to carry back.
//! * **Peering** (`POST /api/peers/:id/complete`): the response, checked against our challenge,
//!   gives us the other cluster's key and CA. From then on our people open it from the cluster
//!   picker: the server signs each a grant (`POST /api/peers/:id/frame`) that every forwarder (the
//!   server, every runner's front door) passes on, and the other cluster checks by itself. The
//!   other cluster never reaches back: peering is one way; the other way is the same steps there.
//! * **Who arrives** from a cluster we authorized ([`caller`]): its grant's person, with the lower
//!   of the role we gave that cluster and the role their grant carries.

pub mod codes;
pub mod forward;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::FutureExt as _;
use http::StatusCode;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

pub type Answer = Result<Response, (StatusCode, String)>;

/// How long a challenge may be answered.
pub const CHALLENGE_TTL: i64 = 30 * 60;
/// How long a grant lasts: a frame opened from the picker gets a new one.
pub const GRANT_TTL: i64 = 8 * 3600;
const KV_PREFIX: &str = "peers:";
/// Peers' ports on the server (kiln reached by address): this far above its HTTP port, at most
/// [`PORTS`] of them.
const PORT_OFFSET: u16 = 100;
const PORTS: u16 = 100;
const BODY_LIMIT: usize = 64 << 10;

fn fail(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Which way a peering goes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Our people open it.
    Out,
    /// Its people open us.
    In,
}

/// A cluster this one is peered with, one way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub direction: Direction,
    /// Out: its address, as reached from here. In: the address its challenge said (shown only).
    #[serde(default)]
    pub url: String,
    /// Its public key (base64url), once known: an outgoing peering learns it from the response.
    #[serde(default)]
    pub key: String,
    /// Out, until peered: our challenge's nonce, and until when it may be answered.
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub expires_at: i64,
    /// In: the role its people get here. Out: the role it said it gave us.
    #[serde(default)]
    pub role: String,
    /// Out: its CA (DER, base64url), from its response.
    #[serde(default)]
    pub ca: String,
    /// Out: its name in addresses (`peer-<slug>.<domain>`) and its port on the server.
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub port: u16,
    pub created_at: i64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub peered_at: i64,
}

impl Peer {
    pub fn active(&self) -> bool {
        !self.key.is_empty()
    }

    fn public(&self) -> Option<Vec<u8>> {
        codes::decode_key(&self.key).ok()
    }

    fn kid(&self) -> String {
        self.public().map(|k| codes::key_id(&k)).unwrap_or_default()
    }

    fn view(&self, now: i64) -> serde_json::Value {
        let state = if self.active() {
            "peered"
        } else if self.expires_at <= now {
            "expired"
        } else {
            "waiting"
        };
        json!({
            "id": self.id, "name": self.name, "direction": self.direction, "url": self.url, "role": self.role,
            "slug": self.slug, "port": self.port, "state": state,
            "fingerprint": self.public().map(|k| codes::fingerprint(&k)),
            "created_at": self.created_at, "created_by": self.created_by,
            "peered_at": (self.peered_at > 0).then_some(self.peered_at),
            "expires_at": (state == "waiting").then_some(self.expires_at),
        })
    }
}

/// What the server keeps of its peerings: loaded at start, changed with the store.
#[derive(Default)]
pub struct Peers {
    list: RwLock<Vec<Peer>>,
    forwarder: RwLock<Option<Arc<forward::Forwarder>>>,
    /// Peers' listeners on the server, by port: the peer's slug, and what stops it.
    listeners: Mutex<HashMap<u16, (String, crate::rt::Shutdown)>>,
}

impl Peers {
    pub fn all(&self) -> Vec<Peer> {
        self.list.read().clone()
    }

    pub fn forwarder(&self) -> Option<Arc<forward::Forwarder>> {
        self.forwarder.read().clone()
    }
}

/// This cluster's peering key.
pub fn key(hub: &Hub) -> anyhow::Result<&'static codes::Key> {
    static K: OnceLock<codes::Key> = OnceLock::new();
    if let Some(k) = K.get() {
        return Ok(k);
    }
    let k = codes::Key::open(&hub.cfg.server.data_dir.join("keys"))?;
    Ok(K.get_or_init(|| k))
}

/// This cluster's name in the codes it makes.
fn our_name() -> String {
    crate::os::hostname()
}

/// Load what the store keeps, then make it so: forwarders, listeners, runners told.
pub async fn start(hub: &Arc<Hub>) {
    match hub.store.kv_prefix(KV_PREFIX.into()).await {
        Ok(rows) => *hub.peered.list.write() = rows.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect(),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "can't read the peered clusters"),
    }
    refresh(hub).await;
}

/// What forwarders need: this cluster's key and every peer our people open.
pub fn table(hub: &Hub) -> forward::Table {
    let Ok(k) = key(hub) else { return forward::Table::default() };
    let peers = hub
        .peered
        .all()
        .into_iter()
        .filter(|p| p.direction == Direction::Out && p.active())
        .map(|p| forward::Route { kid: p.kid(), slug: p.slug, name: p.name, url: p.url, ca: p.ca, port: p.port })
        .collect();
    forward::Table { key: codes::encode_key(k.public()), kid: k.id(), peers }
}

/// After a change: the server's forwarder and listeners, every runner's front door, the UI.
async fn refresh(hub: &Arc<Hub>) {
    *hub.peered.forwarder.write() = Some(Arc::new(forward::Forwarder::new(table(hub))));
    listen(hub);
    for r in hub.front_doors.runners() {
        crate::frontdoor::control::push(hub, &r, None, None).await;
    }
    hub.emit(&json!({ "type": "peers" }));
}

async fn save(hub: &Hub, p: &Peer) -> Result<(), (StatusCode, String)> {
    let json = serde_json::to_string(p).map_err(internal)?;
    hub.store.kv_set(format!("{KV_PREFIX}{}", p.id), json).await.map_err(|e| internal(format!("{e:#}")))?;
    let mut g = hub.peered.list.write();
    g.retain(|x| x.id != p.id);
    g.push(p.clone());
    Ok(())
}

fn find(hub: &Hub, id: &str, direction: Option<Direction>) -> Result<Peer, (StatusCode, String)> {
    hub.peered.all().into_iter().find(|p| p.id == id && direction.is_none_or(|d| p.direction == d)).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such peered cluster"))
}

/// A peer's name as people give it: printable, at most 60 characters.
fn checked_name(name: &str) -> Result<String, (StatusCode, String)> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 60 || n.chars().any(char::is_control) {
        return Err(fail(StatusCode::BAD_REQUEST, "Give it a name of up to 60 characters."));
    }
    Ok(n.to_owned())
}

/// An address as forwarders use it: `http(s)://host[:port]`, nothing after.
fn checked_url(url: &str) -> Result<String, (StatusCode, String)> {
    let bad = || fail(StatusCode::BAD_REQUEST, "Its address is http:// or https:// and a host (a port too, when not the usual one), as kiln is reached from here.");
    let u = url.trim().trim_end_matches('/');
    let uri: http::Uri = u.parse().map_err(|_| bad())?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none_or(str::is_empty) || uri.path_and_query().is_some_and(|p| !matches!(p.as_str(), "" | "/")) {
        return Err(bad());
    }
    Ok(u.to_owned())
}

/// A peer's name in addresses: its name's letters and digits, dashed, unique among them.
fn slug_for(name: &str, peers: &[Peer]) -> String {
    let mut base: String = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    base = base.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    base = base.chars().take(30).collect::<String>().trim_end_matches('-').to_owned();
    if base.is_empty() {
        base = "cluster".to_owned();
    }
    let taken = |s: &str| peers.iter().any(|p| p.direction == Direction::Out && p.slug == s);
    let mut slug = base.clone();
    let mut n = 2;
    while taken(&slug) {
        slug = format!("{base}-{n}");
        n += 1;
    }
    slug
}

/// The server's HTTP listener's address.
fn http_addr(hub: &Hub) -> SocketAddr {
    hub.cfg.server.http_addr.parse().unwrap_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 7070)))
}

/// The first port above the HTTP port's band no other peer has.
fn free_port(hub: &Hub, peers: &[Peer]) -> u16 {
    let base = http_addr(hub).port().saturating_add(PORT_OFFSET);
    (base..base.saturating_add(PORTS)).find(|p| !peers.iter().any(|x| x.port == *p)).unwrap_or(0)
}

#[derive(Deserialize)]
struct Register {
    name: String,
    url: String,
}

/// `POST /api/peers` (`{name, url}`): a cluster to open from here, and the challenge to take to it.
pub async fn register(hub: &Arc<Hub>, actor: &str, req: Request) -> Answer {
    let asked: Register = srv::body_json(req.into_body(), BODY_LIMIT).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let (name, url) = (checked_name(&asked.name)?, checked_url(&asked.url)?);
    let k = key(hub).map_err(internal)?;
    let now = crate::util::now_unix();
    let ch = codes::Challenge::new(&our_name(), k, &hub.cfg.server.public_url, now, CHALLENGE_TTL).map_err(internal)?;
    let peers = hub.peered.all();
    let p = Peer {
        id: crate::util::new_id(),
        slug: slug_for(&name, &peers),
        port: free_port(hub, &peers),
        name,
        direction: Direction::Out,
        url,
        key: String::new(),
        nonce: ch.c.clone(),
        expires_at: ch.x,
        role: String::new(),
        ca: String::new(),
        created_at: now,
        created_by: actor.to_owned(),
        peered_at: 0,
    };
    save(hub, &p).await?;
    hub.store.audit(actor.to_owned(), "peer.register".into(), format!("{} at {}", p.name, p.url)).await;
    hub.emit(&json!({ "type": "peers" }));
    Ok(srv::json(StatusCode::CREATED, &json!({ "peer": p.view(now), "challenge": ch.encode().map_err(internal)? })))
}

/// `POST /api/peers/:id/challenge`: a new challenge for a peering still waiting (the last one was
/// lost, or expired); the one before stops working.
pub async fn rechallenge(hub: &Arc<Hub>, actor: &str, id: &str) -> Answer {
    let mut p = find(hub, id, Some(Direction::Out))?;
    if p.active() {
        return Err(fail(StatusCode::CONFLICT, "It's peered already."));
    }
    let k = key(hub).map_err(internal)?;
    let now = crate::util::now_unix();
    let ch = codes::Challenge::new(&our_name(), k, &hub.cfg.server.public_url, now, CHALLENGE_TTL).map_err(internal)?;
    p.nonce.clone_from(&ch.c);
    p.expires_at = ch.x;
    save(hub, &p).await?;
    hub.store.audit(actor.to_owned(), "peer.challenge".into(), p.name.clone()).await;
    hub.emit(&json!({ "type": "peers" }));
    Ok(srv::json(StatusCode::OK, &json!({ "peer": p.view(now), "challenge": ch.encode().map_err(internal)? })))
}

#[derive(Deserialize)]
struct Complete {
    response: String,
}

/// `POST /api/peers/:id/complete` (`{response}`): the other cluster's answer to our challenge;
/// checked, it makes them peered.
pub async fn complete(hub: &Arc<Hub>, actor: &str, id: &str, req: Request) -> Answer {
    let asked: Complete = srv::body_json(req.into_body(), BODY_LIMIT).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let mut p = find(hub, id, Some(Direction::Out))?;
    if p.active() {
        return Err(fail(StatusCode::CONFLICT, "It's peered already."));
    }
    let now = crate::util::now_unix();
    if p.expires_at <= now {
        return Err(fail(StatusCode::GONE, "That challenge expired. Make a new one and take it over again."));
    }
    let r = codes::Response::decode(&asked.response).map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
    let k = key(hub).map_err(internal)?;
    let theirs = r.verify(k.public(), &p.nonce).map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
    if hub.peered.all().iter().any(|x| x.id != p.id && x.direction == Direction::Out && x.key == codes::encode_key(&theirs)) {
        return Err(fail(StatusCode::CONFLICT, format!("{} is peered already, under another name.", r.n)));
    }
    p.key = codes::encode_key(&theirs);
    p.role = r.r;
    p.ca = r.a;
    p.nonce.clear();
    p.peered_at = now;
    save(hub, &p).await?;
    hub.store.audit(actor.to_owned(), "peer.complete".into(), format!("{} ({}) as {}", p.name, codes::fingerprint(&theirs), p.role)).await;
    refresh(hub).await;
    Ok(srv::json(StatusCode::OK, &p.view(now)))
}

#[derive(Deserialize)]
struct Authorize {
    challenge: String,
    role: String,
}

/// `POST /api/peers/authorize` (`{challenge, role}`): let the cluster that made the challenge open
/// this one, its people with `role` at most; the response to take back to it.
pub async fn authorize(hub: &Arc<Hub>, actor: &str, req: Request) -> Answer {
    let asked: Authorize = srv::body_json(req.into_body(), BODY_LIMIT).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let ch = codes::Challenge::decode(&asked.challenge).map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
    let now = crate::util::now_unix();
    if ch.x <= now {
        return Err(fail(StatusCode::GONE, "That challenge expired. Ask for a new one."));
    }
    let role = codes::ROLES.iter().copied().find(|r| *r == asked.role.trim()).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "The role is viewer, developer, maintainer or admin."))?;
    let k = key(hub).map_err(internal)?;
    if ch.k == codes::encode_key(k.public()) {
        return Err(fail(StatusCode::BAD_REQUEST, "That's this cluster's own challenge: take it to the other cluster."));
    }
    // One authorization per cluster: a new one replaces the last.
    let id = hub.peered.all().into_iter().find(|p| p.direction == Direction::In && p.key == ch.k).map_or_else(crate::util::new_id, |p| p.id);
    let p = Peer {
        id,
        name: ch.n.clone(),
        direction: Direction::In,
        url: ch.u.clone(),
        key: ch.k.clone(),
        nonce: String::new(),
        expires_at: 0,
        role: role.to_owned(),
        ca: String::new(),
        slug: String::new(),
        port: 0,
        created_at: now,
        created_by: actor.to_owned(),
        peered_at: now,
    };
    let response = codes::Response::answer(&ch, &our_name(), k, role, &hub.pki.ca_der).and_then(|r| r.encode()).map_err(internal)?;
    save(hub, &p).await?;
    hub.store.audit(actor.to_owned(), "peer.authorize".into(), format!("{} ({}) as {role}", p.name, p.public().map(|k| codes::fingerprint(&k)).unwrap_or_default())).await;
    hub.emit(&json!({ "type": "peers" }));
    Ok(srv::json(StatusCode::OK, &json!({ "peer": p.view(now), "response": response })))
}

/// `DELETE /api/peers/:id`: a peering ends here (the other side keeps its half until it ends it
/// too, but nothing passes this way any more).
pub async fn remove(hub: &Arc<Hub>, actor: &str, id: &str) -> Answer {
    let p = find(hub, id, None)?;
    hub.store.kv_delete(format!("{KV_PREFIX}{id}")).await.map_err(|e| internal(format!("{e:#}")))?;
    hub.peered.list.write().retain(|x| x.id != id);
    hub.store.audit(actor.to_owned(), "peer.remove".into(), format!("{} ({:?})", p.name, p.direction)).await;
    refresh(hub).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `GET /api/peers`: this cluster's name and key, and every peering.
pub async fn list(hub: &Hub) -> Answer {
    let k = key(hub).map_err(internal)?;
    let now = crate::util::now_unix();
    let mut peers = hub.peered.all();
    peers.sort_by_key(|p| p.created_at);
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "name": our_name(), "fingerprint": codes::fingerprint(k.public()), "roles": codes::ROLES,
            "peers": peers.iter().map(|p| p.view(now)).collect::<Vec<_>>(),
        }),
    ))
}

/// `GET /api/peers/open`: the clusters someone signed in here may open (the picker's list).
pub fn openable(hub: &Hub) -> Answer {
    let peers: Vec<_> = hub.peered.all().into_iter().filter(|p| p.direction == Direction::Out && p.active()).map(|p| json!({ "id": p.id, "name": p.name, "slug": p.slug })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "name": our_name(), "peers": peers })))
}

/// `POST /api/peers/:id/frame`: where to open a peer, for the person asking, from the address
/// they reached this cluster at (`host`, as the browser named it; `secure`: over HTTPS), and the
/// grant to post there.
pub fn frame(hub: &Hub, caller: &crate::authz::Caller, id: &str, host: &str, secure: bool) -> Answer {
    let p = find(hub, id, Some(Direction::Out))?;
    if !p.active() {
        return Err(fail(StatusCode::CONFLICT, "It isn't peered yet."));
    }
    let role = caller.server_role().ok_or_else(|| fail(StatusCode::FORBIDDEN, "Opening a peered cluster takes a role on this whole cluster."))?;
    let origin = origin_for(hub, &p, host, secure).map_err(|e| fail(StatusCode::CONFLICT, e))?;
    let k = key(hub).map_err(internal)?;
    let g = codes::Grant { i: k.id(), a: p.kid(), s: caller.user.clone().unwrap_or_else(|| "owner".to_owned()), n: caller.name.clone(), r: role.to_owned(), e: crate::util::now_unix() + GRANT_TTL };
    let grant = g.sign(k).map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "origin": origin, "action": format!("{origin}{}", forward::ENTER), "grant": grant, "expires_at": g.e })))
}

/// `host` without its port, and the port (IPv6 literals keep their brackets).
fn split_host(host: &str) -> (&str, Option<&str>) {
    let h = host.trim();
    if let Some(rest) = h.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((addr, tail)) => (h.get(..addr.len() + 2).unwrap_or(h), tail.strip_prefix(':')),
            None => (h, None),
        };
    }
    match h.rsplit_once(':') {
        Some((n, p)) if !n.contains(':') && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (n, Some(p)),
        _ => (h, None),
    }
}

/// The front-door domain `host` is, or is under: where `peer-<slug>.<domain>` resolves and has a
/// certificate (every runner's front door answers its domain's wildcard).
fn front_door_domain(hub: &Hub, host: &str) -> Option<String> {
    let host = host.to_ascii_lowercase();
    hub.front_doors.snapshot().into_iter().map(|(_, s, _)| s.domain.trim().to_ascii_lowercase()).find(|d| !d.is_empty() && (host == *d || host.ends_with(&format!(".{d}"))))
}

/// Where the browser that reached this cluster at `host` opens peer `p`: `peer-<slug>.<domain>`
/// for a front door's domain (the same site as the page, so the frame keeps its cookie), else
/// its port on the server, on the same host (a site doesn't count ports). Not `peer-<slug>.localhost`
/// for `localhost`: browsers take each `*.localhost` name for a site of its own.
fn origin_for(hub: &Hub, p: &Peer, host: &str, secure: bool) -> Result<String, String> {
    let (name, port) = split_host(host);
    let port = port.map(|p| format!(":{p}")).unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    if let Some(domain) = front_door_domain(hub, &lower) {
        return Ok(format!("https://{}{}.{domain}{port}", forward::LABEL, p.slug));
    }
    if secure {
        return Err("This page is on HTTPS at an address without a front door's domain: open kiln at a runner's front-door domain to open peered clusters.".into());
    }
    if p.port == 0 {
        return Err("This peer has no port on the server: open kiln at a runner's front-door domain.".into());
    }
    Ok(format!("http://{name}:{}", p.port))
}

/// The peer a request to the server's own listener is for: `peer-<slug>.<a front door's domain>`
/// (from a runner's front door too old to forward it itself, which passes everything on) or
/// `peer-<slug>.localhost`.
pub fn route(hub: &Hub, req: &Request) -> Option<(Arc<forward::Forwarder>, String)> {
    let host = srv::header_str(req, &http::header::HOST)?;
    let (name, _) = split_host(host);
    if !name.starts_with(forward::LABEL) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if !(lower.ends_with(".localhost") || front_door_domain(hub, &lower).is_some()) {
        return None;
    }
    let f = hub.peered.forwarder()?;
    let slug = f.slug_of_host(&lower)?.to_owned();
    Some((f, slug))
}

/// Who a grant from a cluster we authorized is: its person, with the lower of the role we gave
/// that cluster and the role the grant carries. `None`: not one of theirs, changed or expired.
pub fn caller(hub: &Hub, token: &str) -> Option<crate::authz::Caller> {
    let claims = codes::Grant::peek(token)?;
    let peer = hub.peered.all().into_iter().find(|p| p.direction == Direction::In && p.active() && p.kid() == claims.i)?;
    let k = key(hub).ok()?;
    let g = codes::Grant::verify(token, &peer.public()?, &k.id(), crate::util::now_unix())?;
    let role = codes::lower(&g.r, &peer.role)?;
    Some(crate::authz::Caller::peer(&format!("peer:{}:{}", peer.name, g.n), role))
}

// ------------------------------------------------------------------ peers' ports on the server

struct PortHandler {
    hub: Arc<Hub>,
    slug: String,
}

impl srv::Handler for PortHandler {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        match self.hub.peered.forwarder() {
            Some(f) => f.serve(&self.slug, req, false).await,
            None => srv::text(StatusCode::NOT_FOUND, "no such peered cluster here"),
        }
    }
}

/// Each peer our people open gets a port on the server, for kiln reached by address (no
/// front-door domain to put `peer-<slug>.` under): started, and stopped when it ends.
fn listen(hub: &Arc<Hub>) {
    let want: HashMap<u16, String> = hub.peered.all().into_iter().filter(|p| p.direction == Direction::Out && p.active() && p.port != 0).map(|p| (p.port, p.slug)).collect();
    let ip: IpAddr = http_addr(hub).ip();
    let mut g = hub.peered.listeners.lock();
    g.retain(|port, (slug, stop)| {
        let keep = want.get(port) == Some(slug);
        if !keep {
            stop.cancel();
        }
        keep
    });
    for (port, slug) in want {
        if g.contains_key(&port) {
            continue;
        }
        let std_listener = match std::net::TcpListener::bind((ip, port)) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(port, peer = %slug, error = %e, "a peered cluster's port can't be opened; it opens at a front-door domain only");
                continue;
            }
        };
        let stop = crate::rt::Shutdown::new();
        let (sd, handler) = (stop.clone(), Arc::new(PortHandler { hub: Arc::clone(hub), slug: slug.clone() }));
        crate::rt::spawn(async move {
            let listener = match hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(std_listener), 128) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(port, error = %e, "can't hand a peered cluster's port to the runtime");
                    return;
                }
            };
            loop {
                let accepted = futures::select_biased! {
                    () = sd.cancelled().fuse() => return,
                    r = listener.accept().fuse() => r,
                };
                match accepted {
                    Ok(stream) => {
                        let (h, remote) = (Arc::clone(&handler), stream.peer_addr().ok());
                        crate::rt::spawn_spread(async move {
                            crate::http::server::serve_accepted(stream, h, remote, None).await;
                        });
                    }
                    Err(_) => crate::rt::sleep(Duration::from_millis(100)).await,
                }
            }
        });
        tracing::info!(port, peer = %slug, "peered cluster's port open");
        g.insert(port, (slug, stop));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(slug: &str, port: u16) -> Peer {
        Peer {
            id: slug.into(),
            name: slug.into(),
            direction: Direction::Out,
            url: String::new(),
            key: String::new(),
            nonce: String::new(),
            expires_at: 0,
            role: String::new(),
            ca: String::new(),
            slug: slug.into(),
            port,
            created_at: 0,
            created_by: String::new(),
            peered_at: 0,
        }
    }

    #[test]
    fn names_become_addresses_once_each() {
        let peers = vec![out("work", 7170)];
        assert_eq!(slug_for("Work", &peers), "work-2");
        assert_eq!(slug_for("  ACME  Corp / EU ", &peers), "acme-corp-eu");
        assert_eq!(slug_for("ü!!", &peers), "cluster");
    }

    #[test]
    fn addresses_are_a_scheme_and_a_host() {
        assert_eq!(checked_url(" https://kiln.work/ ").ok().as_deref(), Some("https://kiln.work"));
        assert_eq!(checked_url("http://10.0.0.5:7070").ok().as_deref(), Some("http://10.0.0.5:7070"));
        assert!(checked_url("kiln.work").is_err() && checked_url("ftp://kiln.work").is_err() && checked_url("https://kiln.work/ui").is_err());
    }

    #[test]
    fn hosts_split_from_their_ports() {
        assert_eq!(split_host("kiln.home:8443"), ("kiln.home", Some("8443")));
        assert_eq!(split_host("192.168.1.77"), ("192.168.1.77", None));
        assert_eq!(split_host("[fd00::1]:7070"), ("[fd00::1]", Some("7070")));
        assert_eq!(split_host("[fd00::1]"), ("[fd00::1]", None));
    }
}
