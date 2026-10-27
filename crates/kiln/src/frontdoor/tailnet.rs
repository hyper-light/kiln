//! The front door on the tailnet (Tailscale): every device on the tailnet finds kiln by the
//! front door's names, wherever it is.
//!
//! * **Runner.** With `tailscale.enabled`, the front door also listens on this machine's tailnet
//!   addresses: DNS binds them (HTTPS listens everywhere already), a query that arrives there is
//!   answered with them, and the certificate carries them and the MagicDNS name. A [`Watcher`]
//!   asks Tailscale about this machine every [`LOOK_EVERY`] and asks Tailscale's own resolver
//!   (`100.100.100.100`, the one every tailnet device asks) for the domain, so the status says
//!   whether the tailnet really finds it.
//! * **Server.** A runner whose front door answers DNS with the tailnet on is the tailnet's DNS
//!   for its names: the server keeps the tailnet's split DNS saying so ([`TailnetDns`]), through
//!   the Tailscale API with an OAuth client (or API access token) given in the UI, marking the
//!   routes "use with exit node" where the API allows it. Without one, the UI shows the steps for
//!   the admin console, and the runner's status shows when they took.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use http::StatusCode;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::discovery::Iface;
use crate::dns::{self, Message};
use crate::hub::Hub;
use crate::rt::{Notify, Shutdown};
use crate::tailscale::api::{Api, Credential, Error as ApiError};
use crate::tailscale::split::{self, Routes};
use crate::tailscale::{Local, Resolver};

use super::{DNS_PORT, Fix, Settings, State, Status};

/// How often a runner asks Tailscale about its machine.
pub const LOOK_EVERY: Duration = Duration::from_secs(30);
/// Tailscale's resolver on every device.
pub const QUAD100: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(100, 100, 100, 100)), 53);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the server checks the tailnet's split DNS when nothing tells it to.
pub const SYNC_EVERY: Duration = Duration::from_secs(300);
/// Changes arriving together are synced once.
const SETTLE: Duration = Duration::from_secs(2);
const CREDENTIAL_FILE: &str = "tailscale-api.json";
const OWNED_KEY: &str = "tailnet/owned";

// ------------------------------------------------------------------------------ settings, status

/// `Settings.tailscale`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tailnet {
    /// Serve the front door on this machine's tailnet addresses too, and make it the tailnet's
    /// DNS for the front door's names.
    pub enabled: bool,
}

impl Tailnet {
    pub fn is_off(&self) -> bool {
        !self.enabled
    }
}

/// What the tailnet's split DNS says for one of the front door's names, as this machine got it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteSeen {
    pub name: String,
    /// The route that decides it (for the name or a domain above it); `None`: no route.
    pub domain: Option<String>,
    pub to: Vec<String>,
    /// Every resolver of it is used with exit nodes too.
    pub exit_node: bool,
    /// It goes to this machine.
    pub here: bool,
}

/// The domain, asked of Tailscale's resolver from this machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Resolved {
    pub name: String,
    pub answers: Vec<String>,
    pub error: Option<String>,
}

/// `Status.tailscale`: the front door on the tailnet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TailnetStatus {
    pub state: State,
    pub error: Option<String>,
    pub fix: Option<Fix>,
    /// This machine's MagicDNS name, its tailnet and its tailnet addresses.
    pub name: String,
    pub tailnet: String,
    pub addresses: Vec<String>,
    /// Where the tailnet's DNS for the front door's names should go: the tailnet address its DNS
    /// answers on (port 53). `None` while it does not.
    pub nameserver: Option<String>,
    pub magic_dns: bool,
    /// This machine itself takes the tailnet's DNS settings (other devices decide for
    /// themselves).
    pub accept_dns: Option<bool>,
    pub routes: Vec<RouteSeen>,
    pub resolved: Option<Resolved>,
    pub health: Vec<String>,
    pub version: String,
}

// ------------------------------------------------------------------------------ runner: watching

/// What the watcher looks at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    pub enabled: bool,
    /// The name to ask Tailscale's resolver for (the domain, while DNS answers it).
    pub resolve: Option<String>,
    /// Whether DNS listens on the tailnet address (a change asks again at once).
    pub listening: bool,
}

/// What the watcher saw last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub local: Result<Local, String>,
    pub resolved: Option<Resolved>,
}

#[derive(Default)]
struct Watched {
    probe: Mutex<Probe>,
    seen: Mutex<Option<Seen>>,
    changed: Notify,
    poke: Notify,
}

/// Asks Tailscale about this machine while the front door wants the tailnet.
pub struct Watcher {
    w: Arc<Watched>,
    stop: Shutdown,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// A handle the front door's loop waits on.
#[derive(Clone)]
pub struct Changes(Arc<Watched>);

impl Changes {
    pub async fn changed(&self) {
        self.0.changed.notified().await;
    }
}

impl Watcher {
    pub fn start() -> Self {
        let w = Arc::new(Watched::default());
        let stop = Shutdown::new();
        let (ww, sd) = (Arc::clone(&w), stop.clone());
        crate::rt::spawn(async move { watch(ww, sd).await });
        Self { w, stop }
    }

    /// Look at this from now on; a change looks at once.
    pub fn want(&self, probe: Probe) {
        let mut p = self.w.probe.lock();
        if *p != probe {
            *p = probe;
            self.w.poke.notify();
        }
    }

    pub fn seen(&self) -> Option<Seen> {
        self.w.seen.lock().clone()
    }

    pub fn changes(&self) -> Changes {
        Changes(Arc::clone(&self.w))
    }
}

async fn watch(w: Arc<Watched>, stop: Shutdown) {
    loop {
        let probe = w.probe.lock().clone();
        let seen = if probe.enabled {
            let local = crate::tailscale::local().await;
            let up = local.as_ref().is_ok_and(|l| l.running());
            let resolved = match (&probe.resolve, up) {
                (Some(name), true) => Some(resolve(name).await),
                _ => None,
            };
            Some(Seen { local, resolved })
        } else {
            None
        };
        if *w.seen.lock() != seen {
            *w.seen.lock() = seen;
            w.changed.notify();
        }
        futures::select_biased! {
            () = stop.cancelled().fuse() => return,
            () = w.poke.notified().fuse() => {}
            () = crate::rt::sleep(LOOK_EVERY).fuse() => {}
        }
    }
}

/// `name`'s A records, from Tailscale's resolver.
pub async fn resolve(name: &str) -> Resolved {
    resolve_at(QUAD100, name).await
}

async fn resolve_at(server: SocketAddr, name: &str) -> Resolved {
    let mut out = Resolved { name: name.to_owned(), ..Resolved::default() };
    let id: u16 = rand::random();
    let mut q = super::zone::query(name, dns::TYPE_A, true);
    q.id = id;
    let bytes = match q.encode() {
        Ok(b) => b,
        Err(e) => {
            out.error = Some(format!("{name}: {e}"));
            return out;
        }
    };
    let answer = match crate::discovery::unicast::exchange_udp(server, &bytes, id, RESOLVE_TIMEOUT).await {
        Ok(a) => a,
        Err(_) => {
            out.error = Some("Tailscale's resolver (100.100.100.100) did not answer here".to_owned());
            return out;
        }
    };
    match Message::decode(&answer) {
        Ok(m) if m.rcode() == dns::RCODE_NOERROR => {
            out.answers = m
                .answers
                .iter()
                .filter_map(|r| match r.data {
                    dns::RData::A(ip) => Some(ip.to_string()),
                    _ => None,
                })
                .collect();
            if out.answers.is_empty() {
                out.error = Some("answered without an address".to_owned());
            }
        }
        Ok(m) if m.rcode() == 2 => out.error = Some("no route for it (SERVFAIL): the tailnet's DNS does not send it to a nameserver".to_owned()),
        Ok(m) => out.error = Some(dns::rcode_name(m.rcode()).to_owned()),
        Err(e) => out.error = Some(format!("an unreadable answer: {e}")),
    }
    out
}

// ------------------------------------------------------------------------------ runner: using it

/// This machine on the tailnet as an interface of the front door's: its tailnet addresses (each
/// on its own), when Tailscale runs on a kernel interface. A query that arrives on one of them
/// is answered with them.
pub fn iface(l: &Local) -> Option<Iface> {
    if !(l.running() && l.tun) {
        return None;
    }
    let v4: Vec<(Ipv4Addr, Ipv4Addr)> = l
        .ips
        .iter()
        .filter_map(|a| match a {
            IpAddr::V4(v) => Some((*v, Ipv4Addr::BROADCAST)),
            IpAddr::V6(_) => None,
        })
        .collect();
    let v6 = l
        .ips
        .iter()
        .filter_map(|a| match a {
            IpAddr::V6(v) => Some(*v),
            IpAddr::V4(_) => None,
        })
        .collect::<Vec<_>>();
    (!v4.is_empty() || !v6.is_empty()).then(|| Iface { name: "tailscale".to_owned(), index: 0, v4, v6 })
}

/// The one-time fix for a machine whose Tailscale is not signed in.
fn sign_in_fix() -> Fix {
    let command = if cfg!(target_os = "linux") { "sudo tailscale up" } else { "tailscale up" };
    Fix { why: "Sign this machine in to the tailnet (or open the Tailscale app and connect):".to_owned(), commands: vec![command.to_owned()] }
}

/// The front door on the tailnet, for its status: what Tailscale says (`seen`), and where DNS
/// listens now (`dns_bound`).
pub fn status(s: &Settings, seen: Option<&Seen>, dns_bound: &[SocketAddr]) -> Option<TailnetStatus> {
    if !s.tailscale.enabled {
        return None;
    }
    let mut st = TailnetStatus::default();
    let Some(seen) = seen else {
        st.state = State::Waiting;
        st.error = Some("Asking Tailscale about this machine.".to_owned());
        return Some(st);
    };
    let l = match &seen.local {
        Ok(l) => l,
        Err(e) => {
            st.state = State::Error;
            st.error = Some(e.clone());
            return Some(st);
        }
    };
    st.name = l.name.clone();
    st.tailnet = l.tailnet.clone();
    st.addresses = l.ips.iter().map(IpAddr::to_string).collect();
    st.magic_dns = l.magic_dns;
    st.accept_dns = l.accept_dns;
    st.health = l.health.clone();
    st.version = l.version.clone();
    st.resolved = seen.resolved.clone();
    let primary = l.primary();
    for name in s.dns_names() {
        let route = l.route_for(&name);
        let to: Vec<String> = route.map(|(_, r)| r.iter().map(|x| x.addr.clone()).collect()).unwrap_or_default();
        let here = route.is_some_and(|(_, r)| r.iter().any(|x| x.ip().is_some_and(|ip| l.ips.contains(&ip))));
        let exit_node = route.is_some_and(|(_, r)| !r.is_empty() && r.iter().all(|x| x.exit_node));
        st.routes.push(RouteSeen { name, domain: route.map(|(d, _)| d.to_owned()), to, exit_node, here });
    }
    if !l.running() {
        st.state = State::Error;
        let what = if l.backend.is_empty() { "not connected".to_owned() } else { l.backend.clone() };
        st.error = Some(format!("Tailscale is {what} on this machine."));
        st.fix = Some(sign_in_fix());
        return Some(st);
    }
    if !l.tun {
        st.state = State::Error;
        st.error = Some("Tailscale runs with userspace networking here, so nothing on this machine can listen on its tailnet address. Run it with a TUN device (the default).".to_owned());
        return Some(st);
    }
    let Some(primary) = primary else {
        st.state = State::Error;
        st.error = Some("Tailscale has not given this machine an address yet.".to_owned());
        return Some(st);
    };
    st.state = State::On;
    if s.dns.enabled && !s.domain.is_empty() {
        if s.dns.port != DNS_PORT {
            st.state = State::Error;
            st.error = Some(format!("Tailscale asks a nameserver on port {DNS_PORT}, and this DNS listens on {}.", s.dns.port));
        } else if dns_bound.contains(&SocketAddr::new(primary, DNS_PORT)) {
            st.nameserver = Some(primary.to_string());
        } else {
            st.state = State::Waiting;
            st.error = Some(format!("DNS is not listening on {primary}:{DNS_PORT} yet (see DNS)."));
        }
    }
    Some(st)
}

/// What a certificate for this front door carries for the tailnet: its tailnet IPv4 addresses
/// and the MagicDNS name.
pub fn cert_names(seen: Option<&Seen>) -> Vec<String> {
    let Some(Ok(l)) = seen.map(|s| &s.local) else { return Vec::new() };
    if !l.running() {
        return Vec::new();
    }
    let mut v: Vec<String> = l.ips.iter().filter(|a| a.is_ipv4()).map(IpAddr::to_string).collect();
    if !l.name.is_empty() {
        v.push(l.name.clone());
    }
    v
}

// ------------------------------------------------------------------------------ server

/// What the tailnet's split DNS should say: each name a front door with DNS and the tailnet on
/// answers → the tailnet addresses where its DNS listens; and the names still settling (a runner
/// serving them has not reported where, or reported for older settings), left as they are.
pub fn desired(doors: &[(String, Settings, Option<Status>)]) -> (BTreeMap<String, Vec<String>>, BTreeSet<String>) {
    let mut want: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pending = BTreeSet::new();
    for (_, s, st) in doors {
        if !(s.dns.enabled && s.tailscale.enabled && !s.domain.is_empty()) {
            continue;
        }
        let ns = st.as_ref().filter(|st| st.settings == s.fingerprint() && !st.standby).and_then(|st| st.tailscale.as_ref()).and_then(|t| t.nameserver.clone());
        for name in s.dns_names().iter().map(|n| crate::tailscale::normalize(n)) {
            match &ns {
                Some(a) => {
                    let v = want.entry(name).or_default();
                    if !v.contains(a) {
                        v.push(a.clone());
                    }
                }
                None => {
                    pending.insert(name);
                }
            }
        }
    }
    want.retain(|n, _| !pending.contains(n));
    for v in want.values_mut() {
        v.sort();
    }
    (want, pending)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Mode {
    /// `/dns/configuration`: routes can be marked "use with exit node".
    Configuration,
    /// `/dns/split-dns` only.
    SplitDns,
}

/// How the last sync went.
#[derive(Clone, Debug, Default, Serialize)]
struct Sync {
    at: i64,
    ok: bool,
    error: Option<String>,
    /// The tailnet's routes for kiln's names, as of `at`.
    routes: BTreeMap<String, Vec<Resolver>>,
    /// Routes kiln replaced last time it changed something (someone else's nameserver).
    replaced: BTreeMap<String, Vec<String>>,
}

#[derive(Default)]
struct Inner {
    api: Option<Arc<Api>>,
    mode: Option<Mode>,
    /// The routes kiln set (and keeps), by domain.
    owned: BTreeMap<String, Vec<String>>,
    last: Sync,
}

/// The server's side: the Tailscale credential, the routes it keeps, how the last sync went.
#[derive(Default)]
pub struct TailnetDns {
    inner: Mutex<Inner>,
    pub wake: Notify,
}

impl TailnetDns {
    /// Read the stored credential and the routes kiln owns (at server start).
    pub async fn load(&self, cfg: &crate::config::Config, store: &crate::store::Store) {
        let path = cfg.data(CREDENTIAL_FILE);
        let api = match std::fs::read(&path) {
            Ok(b) => match serde_json::from_slice::<Credential>(&b).map_err(anyhow::Error::from).and_then(Api::new) {
                Ok(a) => Some(Arc::new(a)),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "tailnet DNS: the stored Tailscale credential is unreadable");
                    None
                }
            },
            Err(_) => None,
        };
        let owned = match store.kv_get(OWNED_KEY.to_owned()).await {
            Ok(Some(v)) => serde_json::from_str(&v).unwrap_or_default(),
            _ => BTreeMap::new(),
        };
        let mut g = self.inner.lock();
        g.api = api;
        g.owned = owned;
    }
}

/// Keep the tailnet's split DNS on the front doors: now and then, and whenever woken.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    // Runners report within moments of the server starting: give them that before the first look.
    if !hub.shutdown.sleep(Duration::from_secs(20)).await {
        return Ok(());
    }
    loop {
        sync_once(&hub).await;
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = hub.tailnet.wake.notified().fuse() => {}
            () = crate::rt::sleep(SYNC_EVERY).fuse() => {}
        }
        crate::rt::sleep(SETTLE).await;
    }
}

/// The tailnet's routes now, and its whole DNS configuration when the API gives it.
async fn read(api: &Api, mode: Option<Mode>) -> Result<(Routes, Option<serde_json::Value>, Mode), ApiError> {
    if mode != Some(Mode::SplitDns) {
        match api.configuration().await {
            Ok(c) => return Ok((split::routes_of_configuration(&c), Some(c), Mode::Configuration)),
            // An older API, or a key that may use only the split-DNS endpoints.
            Err(ApiError::Forbidden(_) | ApiError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let m = api.split_dns().await?;
    Ok((split::routes_of_split_dns(&m), None, Mode::SplitDns))
}

/// Read the tailnet's split DNS, change what the plan says, and return the plan, the routes
/// after it and the mode the API allowed.
async fn reconcile(
    api: &Api,
    mode: Option<Mode>,
    want: &BTreeMap<String, Vec<String>>,
    pending: &BTreeSet<String>,
    owned: &BTreeMap<String, Vec<String>>,
) -> Result<(split::Plan, Routes, Mode), ApiError> {
    let (current, config, mode) = read(api, mode).await?;
    let plan = split::plan(want, pending, owned, &current, mode == Mode::Configuration);
    if plan.is_empty() {
        return Ok((plan, current, mode));
    }
    let after = match &config {
        Some(c) => {
            let back = api.set_configuration(&split::apply_to_configuration(c, &plan)).await?;
            if back.is_object() { split::routes_of_configuration(&back) } else { read(api, Some(mode)).await?.0 }
        }
        None => split::routes_of_split_dns(&api.update_split_dns(&split::patch_of(&plan)).await?),
    };
    Ok((plan, after, mode))
}

/// Look once, change what the plan says, and remember how it went.
pub async fn sync_once(hub: &Hub) {
    let (want, pending) = desired(&hub.front_doors.snapshot());
    let (api, mode, owned) = {
        let g = hub.tailnet.inner.lock();
        (g.api.clone(), g.mode, g.owned.clone())
    };
    let Some(api) = api else { return };
    let result = reconcile(&api, mode, &want, &pending, &owned).await;
    let now = crate::util::now_unix();
    match result {
        Ok((plan, after, mode)) => {
            let owned_now = split::owned_after(&want, &pending, &owned);
            if owned_now != owned
                && let Ok(json) = serde_json::to_string(&owned_now)
                && let Err(e) = hub.store.kv_set(OWNED_KEY.to_owned(), json).await
            {
                tracing::warn!(error = %format!("{e:#}"), "tailnet DNS: the routes kiln set were not stored");
            }
            for (d, a) in &plan.set {
                let was = plan.replaced.get(d).map(|w| format!(" (it went to {})", w.join(", "))).unwrap_or_default();
                tracing::info!(domain = %d, to = ?a, "tailnet DNS: route set");
                hub.store.audit("kiln".into(), "tailnet.dns".into(), format!("{d} → {}{was}", a.join(", "))).await;
            }
            for d in &plan.clear {
                hub.store.audit("kiln".into(), "tailnet.dns".into(), format!("{d}: route removed (no front door serves it now)")).await;
            }
            for d in &plan.disown {
                hub.store.audit("kiln".into(), "tailnet.dns".into(), format!("{d}: changed in Tailscale since kiln set it; left as it is")).await;
            }
            let names: BTreeSet<&String> = want.keys().chain(pending.iter()).chain(owned.keys()).chain(owned_now.keys()).collect();
            let routes = after.into_iter().filter(|(d, _)| names.contains(d)).collect();
            let changed = {
                let mut g = hub.tailnet.inner.lock();
                let before = (g.last.ok, g.last.routes.clone(), g.last.error.clone());
                g.mode = Some(mode);
                g.owned = owned_now;
                let replaced = if plan.is_empty() { g.last.replaced.clone() } else { plan.replaced.clone() };
                g.last = Sync { at: now, ok: true, error: None, routes, replaced };
                before != (g.last.ok, g.last.routes.clone(), g.last.error.clone()) || !plan.is_empty()
            };
            if changed {
                hub.emit(&crate::wire::events::ServerEvent::Tailscale);
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let changed = {
                let mut g = hub.tailnet.inner.lock();
                let changed = g.last.error.as_deref() != Some(msg.as_str());
                if changed {
                    tracing::warn!(error = %msg, "tailnet DNS: Tailscale's API");
                }
                g.last.at = now;
                g.last.ok = false;
                g.last.error = Some(msg);
                changed
            };
            if changed {
                hub.emit(&crate::wire::events::ServerEvent::Tailscale);
            }
        }
    }
}

/// `GET /api/tailscale` (and what the others answer with).
pub fn view(hub: &Hub) -> serde_json::Value {
    let (want, pending) = desired(&hub.front_doors.snapshot());
    let g = hub.tailnet.inner.lock();
    json!({
        "configured": g.api.is_some(),
        "kind": g.api.as_ref().map(|a| a.credential().kind()),
        "id": g.api.as_ref().map(|a| a.credential().public()),
        "mode": g.mode,
        "exit_nodes": g.mode.map(|m| m == Mode::Configuration),
        "want": want,
        "pending": pending,
        "owned": g.owned,
        "synced_at": (g.last.at > 0).then_some(g.last.at),
        "ok": g.last.ok,
        "error": g.last.error,
        "routes": g.last.routes,
        "replaced": g.last.replaced,
    })
}

pub async fn get(hub: &Hub) -> super::control::Answer {
    Ok(crate::http::server::json(StatusCode::OK, &view(hub)))
}

#[derive(Deserialize)]
struct Given {
    secret: String,
    #[serde(default)]
    client_id: String,
}

/// `PUT /api/tailscale`: a credential for the Tailscale API, tried before it is kept (in the
/// server's data directory, mode 0600; never sent back, never logged).
pub async fn put(hub: &Hub, req: crate::http::server::Request) -> super::control::Answer {
    let given: Given = crate::http::server::body_json(req.into_body(), 8 * 1024)
        .await
        .map_err(|_| (StatusCode::BAD_REQUEST, "expected {\"secret\": \"tskey-…\", \"client_id\": \"…\"}".to_owned()))?;
    let cred = Credential::parse(&given.secret, &given.client_id).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let api = Api::new(cred.clone()).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let mode = match read(&api, None).await {
        Ok((_, _, mode)) => mode,
        Err(e @ (ApiError::Unauthorized(_) | ApiError::Forbidden(_) | ApiError::NotFound(_))) => return Err((StatusCode::BAD_REQUEST, e.to_string())),
        Err(e) => return Err((StatusCode::BAD_GATEWAY, e.to_string())),
    };
    let json = serde_json::to_vec(&cred).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let path = hub.cfg.data(CREDENTIAL_FILE);
    crate::util::write_secret(&path, &json, 0o600).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("store the credential: {e}")))?;
    {
        let mut g = hub.tailnet.inner.lock();
        g.api = Some(Arc::new(api));
        g.mode = Some(mode);
        g.last = Sync::default();
    }
    hub.store.audit("admin".into(), "tailnet.credential".into(), format!("Tailscale API: {} {}", cred.kind(), cred.public())).await;
    sync_once(hub).await;
    hub.emit(&crate::wire::events::ServerEvent::Tailscale);
    Ok(crate::http::server::json(StatusCode::OK, &view(hub)))
}

/// `DELETE /api/tailscale`: forget the credential. The routes kiln set stay as they are.
pub async fn delete(hub: &Hub) -> super::control::Answer {
    let path = hub.cfg.data(CREDENTIAL_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("remove the credential: {e}"))),
    }
    {
        let mut g = hub.tailnet.inner.lock();
        g.api = None;
        g.mode = None;
        g.last = Sync::default();
    }
    hub.store.audit("admin".into(), "tailnet.credential".into(), "Tailscale API: forgotten".into()).await;
    hub.emit(&crate::wire::events::ServerEvent::Tailscale);
    Ok(crate::http::server::json(StatusCode::OK, &view(hub)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontdoor::{Dns, Listener};

    fn local() -> Local {
        Local {
            backend: "Running".into(),
            ips: vec!["100.64.0.5".parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), "fd7a:115c:a1e0::5".parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))],
            name: "box.tail1.ts.net".into(),
            tailnet: "someone@example.com".into(),
            magic_dns: true,
            tun: true,
            accept_dns: Some(true),
            routes: Some([("kiln.dev".to_owned(), vec![Resolver { addr: "100.64.0.5".into(), exit_node: true }])].into()),
            ..Local::default()
        }
    }

    fn settings(tailnet: bool) -> Settings {
        Settings { domain: "kiln.dev".into(), aliases: vec!["ci.home".into()], dns: Dns { enabled: true, ..Dns::default() }, tailscale: Tailnet { enabled: tailnet }, ..Settings::default() }
    }

    fn at(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), port)
    }

    #[test]
    fn the_tailnet_is_an_interface_of_the_front_door() {
        let i = iface(&local());
        assert_eq!(i.as_ref().map(|i| i.v4.clone()), Some(vec![(Ipv4Addr::new(100, 64, 0, 5), Ipv4Addr::BROADCAST)]));
        assert_eq!(i.as_ref().map(|i| i.v6.len()), Some(1));
        // A query arriving at the tailnet address is answered with both tailnet addresses.
        let f = super::super::netinfo::facing(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 5)), &i.into_iter().collect::<Vec<_>>());
        assert_eq!((f.v4.len(), f.v6.len()), (1, 1));
        // Not when it is down, or in userspace mode.
        assert!(iface(&Local { backend: "Stopped".into(), ..local() }).is_none());
        assert!(iface(&Local { tun: false, ..local() }).is_none());
        assert_eq!(cert_names(Some(&Seen { local: Ok(local()), resolved: None })), ["100.64.0.5", "box.tail1.ts.net"]);
        assert!(cert_names(Some(&Seen { local: Err("no".into()), resolved: None })).is_empty());
    }

    #[test]
    fn the_status_says_where_the_tailnet_should_send_dns() {
        let seen = Seen { local: Ok(local()), resolved: Some(Resolved { name: "kiln.dev".into(), answers: vec!["100.64.0.5".into()], error: None }) };
        assert!(status(&settings(false), Some(&seen), &[]).is_none(), "off: no tailnet part");
        let waiting = status(&settings(true), None, &[]).unwrap_or_default();
        assert_eq!(waiting.state, State::Waiting);
        // DNS not on the tailnet address yet: no nameserver to offer.
        let st = status(&settings(true), Some(&seen), &[at("192.168.1.77", 53)]).unwrap_or_default();
        assert_eq!((st.state, st.nameserver.as_deref()), (State::Waiting, None));
        // Listening there: that is the nameserver; the routes it sees are reported per name.
        let st = status(&settings(true), Some(&seen), &[at("100.64.0.5", 53)]).unwrap_or_default();
        assert_eq!((st.state, st.nameserver.as_deref()), (State::On, Some("100.64.0.5")));
        assert_eq!(st.routes.len(), 2);
        let first = st.routes.first().cloned().unwrap_or_default();
        assert!(first.here && first.exit_node && first.domain.as_deref() == Some("kiln.dev"));
        let alias = st.routes.get(1).cloned().unwrap_or_default();
        assert!(!alias.here && alias.domain.is_none(), "{alias:?}");
        assert_eq!(st.resolved.map(|r| r.answers), Some(vec!["100.64.0.5".to_owned()]));
        // Another port: Tailscale cannot use it.
        let mut odd = settings(true);
        odd.dns.port = 5353;
        assert_eq!(status(&odd, Some(&seen), &[at("100.64.0.5", 5353)]).map(|s| s.state), Some(State::Error));
        // Signed out: the fix says how to sign in.
        let out = Seen { local: Ok(Local { backend: "NeedsLogin".into(), ..local() }), resolved: None };
        let st = status(&settings(true), Some(&out), &[]).unwrap_or_default();
        assert_eq!(st.state, State::Error);
        assert!(st.fix.is_some_and(|f| f.commands.iter().any(|c| c.contains("tailscale up"))));
        let none = Seen { local: Err("Tailscale is not installed on this machine".into()), resolved: None };
        assert_eq!(status(&settings(true), Some(&none), &[]).and_then(|s| s.error).as_deref(), Some("Tailscale is not installed on this machine"));
    }

    #[test]
    fn desired_routes_come_from_the_runners_that_vouch_for_them() {
        let s = settings(true);
        let reported = |ns: Option<&str>, fingerprint: String| Status {
            settings: fingerprint,
            dns: Listener { state: State::On, ..Listener::default() },
            tailscale: Some(TailnetStatus { nameserver: ns.map(str::to_owned), ..TailnetStatus::default() }),
            ..Status::default()
        };
        let doors = vec![("box".to_owned(), s.clone(), Some(reported(Some("100.64.0.5"), s.fingerprint())))];
        let (want, pending) = desired(&doors);
        assert_eq!(want.get("kiln.dev"), Some(&vec!["100.64.0.5".to_owned()]));
        assert_eq!(want.get("ci.home"), Some(&vec!["100.64.0.5".to_owned()]));
        assert!(pending.is_empty());
        // Two runners for one name: both addresses.
        let mut two = doors.clone();
        two.push(("mac".to_owned(), s.clone(), Some(reported(Some("100.64.0.9"), s.fingerprint()))));
        assert_eq!(desired(&two).0.get("kiln.dev").map(Vec::len), Some(2));
        // Not reported yet, reported for older settings, or no nameserver: settling.
        for st in [None, Some(reported(Some("100.64.0.5"), "old".into())), Some(reported(None, s.fingerprint()))] {
            let (want, pending) = desired(&[("box".to_owned(), s.clone(), st)]);
            assert!(want.is_empty());
            assert!(pending.contains("kiln.dev"));
        }
        // The tailnet off, or DNS off: nothing wanted, nothing pending.
        let off = desired(&[("box".to_owned(), settings(false), None)]);
        assert!(off.0.is_empty() && off.1.is_empty());
    }

    /// A stand-in for Tailscale's API: an OAuth client `kTest`, one access token, a DNS
    /// configuration (or only split DNS, when `configuration` is off), and what was asked.
    #[derive(Default)]
    struct FakeApi {
        configuration: bool,
        config: Mutex<serde_json::Value>,
        tokens: std::sync::atomic::AtomicUsize,
        calls: Mutex<Vec<String>>,
    }

    impl crate::http::server::Handler for FakeApi {
        async fn handle(self: Arc<Self>, req: crate::http::server::Request) -> crate::http::server::Response {
            use crate::http::server as srv;
            let (method, path) = (req.method().clone(), req.uri().path().to_owned());
            let auth = srv::header_str(&req, &http::header::AUTHORIZATION).unwrap_or_default().to_owned();
            let body: serde_json::Value = if method == http::Method::POST && path.ends_with("/oauth/token") {
                serde_json::Value::Null
            } else {
                srv::body_json(req.into_body(), 1 << 16).await.unwrap_or(serde_json::Value::Null)
            };
            self.calls.lock().push(format!("{method} {path}"));
            let deny = |code: StatusCode, m: &str| srv::json(code, &json!({ "message": m }));
            if path == "/api/v2/oauth/token" {
                self.tokens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return srv::json(StatusCode::OK, &json!({ "access_token": "tskey-api-access1", "token_type": "Bearer", "expires_in": 3600, "scope": "dns" }));
            }
            if auth != "Bearer tskey-api-access1" {
                return deny(StatusCode::UNAUTHORIZED, "invalid token");
            }
            let split_of = |c: &serde_json::Value| -> serde_json::Value {
                let routes = split::routes_of_configuration(c);
                json!(routes.iter().map(|(d, r)| (d.clone(), r.iter().map(|x| x.addr.clone()).collect::<Vec<_>>())).collect::<BTreeMap<_, _>>())
            };
            match (method.as_str(), path.as_str()) {
                ("GET", "/api/v2/tailnet/-/dns/configuration") if self.configuration => srv::json(StatusCode::OK, &*self.config.lock()),
                ("POST", "/api/v2/tailnet/-/dns/configuration") if self.configuration => {
                    *self.config.lock() = body.clone();
                    srv::json(StatusCode::OK, &body)
                }
                (_, "/api/v2/tailnet/-/dns/configuration") => deny(StatusCode::FORBIDDEN, "insufficient scope"),
                ("GET", "/api/v2/tailnet/-/dns/split-dns") => srv::json(StatusCode::OK, &split_of(&self.config.lock())),
                ("PATCH", "/api/v2/tailnet/-/dns/split-dns") => {
                    let mut c = self.config.lock();
                    if let (Some(split), Some(changes)) = (c.get_mut("splitDNS").and_then(serde_json::Value::as_object_mut), body.as_object()) {
                        for (d, v) in changes {
                            match v.as_array() {
                                None => {
                                    split.remove(d);
                                }
                                Some(a) => {
                                    split.insert(d.clone(), json!(a.iter().map(|x| json!({ "address": x })).collect::<Vec<_>>()));
                                }
                            }
                        }
                    }
                    srv::json(StatusCode::OK, &split_of(&c))
                }
                _ => deny(StatusCode::NOT_FOUND, "not found"),
            }
        }
    }

    async fn serve(api: Arc<FakeApi>) -> anyhow::Result<String> {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = tcp.local_addr()?;
        let tcp = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(tcp), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let (h, remote) = (Arc::clone(&api), stream.peer_addr().ok());
                crate::rt::spawn(crate::http::server::serve_connection(crate::http::io::RtIo::new(stream), h, remote));
            }
        });
        Ok(format!("http://{addr}/api/v2"))
    }

    fn tailnet_config() -> serde_json::Value {
        json!({
            "nameservers": [{ "address": "8.8.8.8", "useWithExitNode": false }],
            "splitDNS": { "corp.example": [{ "address": "10.1.1.1", "useWithExitNode": false }] },
            "searchPaths": [],
            "preferences": { "overrideLocalDNS": false, "magicDNS": true }
        })
    }

    #[test]
    fn the_tailnets_split_dns_follows_the_front_doors_through_the_api() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let fake = Arc::new(FakeApi { configuration: true, config: Mutex::new(tailnet_config()), ..FakeApi::default() });
            let base = serve(Arc::clone(&fake)).await?;
            let api = Api::with_base(Credential::Oauth { client_id: "kTest".into(), secret: "tskey-client-kTest-secret".into() }, &base)?;
            let want: BTreeMap<String, Vec<String>> = [("kiln.dev".to_owned(), vec!["100.64.0.5".to_owned()])].into();
            let none = BTreeSet::new();

            // First sync: the route is set through the configuration, used with exit nodes too, and
            // everything else in the configuration stays as it was.
            let (plan, after, mode) = reconcile(&api, None, &want, &none, &BTreeMap::new()).await?;
            assert_eq!(mode, Mode::Configuration);
            assert_eq!(plan.set, want);
            assert_eq!(after.get("kiln.dev"), Some(&vec![Resolver { addr: "100.64.0.5".into(), exit_node: true }]));
            let config = fake.config.lock().clone();
            for key in ["nameservers", "searchPaths", "preferences"] {
                assert_eq!(config.get(key), tailnet_config().get(key), "{key}");
            }
            assert!(after.contains_key("corp.example"), "someone else's route stays");
            // Again: nothing to change, no write.
            let writes = || fake.calls.lock().iter().filter(|c| c.starts_with("POST /api/v2/tailnet")).count();
            let (plan, _, _) = reconcile(&api, Some(mode), &want, &none, &want).await?;
            assert!(plan.is_empty());
            assert_eq!(writes(), 1);
            // The front door is turned off: kiln's route goes, the others stay.
            let (plan, after, _) = reconcile(&api, Some(mode), &BTreeMap::new(), &none, &want).await?;
            assert_eq!(plan.clear, ["kiln.dev"]);
            assert!(!after.contains_key("kiln.dev") && after.contains_key("corp.example"));
            // One access token for all of it.
            assert_eq!(fake.tokens.load(std::sync::atomic::Ordering::Relaxed), 1);

            // A key that may not use the configuration endpoint: split DNS alone, no exit-node flag.
            let older = Arc::new(FakeApi { configuration: false, config: Mutex::new(tailnet_config()), ..FakeApi::default() });
            let base = serve(Arc::clone(&older)).await?;
            let api = Api::with_base(Credential::Oauth { client_id: "kTest".into(), secret: "tskey-client-kTest-secret".into() }, &base)?;
            let (plan, after, mode) = reconcile(&api, None, &want, &none, &BTreeMap::new()).await?;
            assert_eq!(mode, Mode::SplitDns);
            assert_eq!(plan.set, want);
            assert_eq!(after.get("kiln.dev"), Some(&vec![Resolver { addr: "100.64.0.5".into(), exit_node: false }]));
            assert!(older.calls.lock().iter().any(|c| c == "PATCH /api/v2/tailnet/-/dns/split-dns"));
            // And settled: the flag it cannot set is not chased.
            assert!(reconcile(&api, Some(mode), &want, &none, &want).await?.0.is_empty());

            // A token the API refuses is an error that says so.
            let bad = Api::with_base(Credential::Token { token: "tskey-api-wrong".into() }, &base)?;
            let e = reconcile(&bad, None, &want, &none, &BTreeMap::new()).await.err();
            assert!(matches!(e, Some(ApiError::Unauthorized(_))), "{e:?}");
            Ok(())
        })
    }

    #[test]
    fn tailscales_resolver_is_asked_and_its_answers_read() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // A stand-in resolver on loopback: kiln.dev has an address, anything else SERVFAILs.
            let sock = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
            let addr = sock.local_addr()?;
            std::thread::spawn(move || {
                let mut buf = [0u8; 1500];
                while let Ok((n, from)) = sock.recv_from(&mut buf) {
                    let Ok(q) = Message::decode(buf.get(..n).unwrap_or_default()) else { continue };
                    let known = q.questions.first().is_some_and(|x| x.name == crate::dns::Name::parse("kiln.dev"));
                    let answers = if known { vec![crate::dns::Record::new(crate::dns::Name::parse("kiln.dev"), 60, false, dns::RData::A(Ipv4Addr::new(100, 64, 0, 5)))] } else { Vec::new() };
                    let rcode = if known { 0 } else { 2 };
                    let m = Message { id: q.id, flags: dns::FLAG_RESPONSE | rcode, questions: q.questions.clone(), answers, ..Message::default() };
                    if let Ok(b) = m.encode() {
                        let _ = sock.send_to(&b, from);
                    }
                }
            });
            let r = resolve_at(addr, "kiln.dev").await;
            assert_eq!((r.answers.clone(), r.error.clone()), (vec!["100.64.0.5".to_owned()], None));
            let r = resolve_at(addr, "other.dev").await;
            assert!(r.answers.is_empty() && r.error.as_deref().is_some_and(|e| e.contains("SERVFAIL")), "{r:?}");
            Ok(())
        })
    }
}
