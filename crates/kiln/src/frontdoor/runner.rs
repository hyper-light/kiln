//! The runner's side of its front door: one task per runner state directory that holds the
//! listeners, applies what the server pushes ([`Push`]), retries what failed, keeps the
//! certificate current, and reports how it all does ([`Status`]).
//!
//! * **Lifetime.** Started with the runner from the settings it stored last, so a restarted
//!   runner (or one whose server is away) serves at once; it outlives reconnects to the server and
//!   restarts of the runner's main loop, and ends with the process.
//! * **Reconciling.** After every push and every [`TICK`]: the LAN interfaces are read again, and
//!   each listener is started, moved (another port, other addresses), kept, or stopped to match
//!   the settings. Names, addresses, upstreams and certificates change in place, without
//!   rebinding. Whatever failed to bind is tried again.
//! * **Certificates.** In `kiln` mode the runner asks for one with a CSR in its status (its key
//!   stays here) when it has none, when the names it serves changed, or when the one it has
//!   expires within 30 days; it asks again every [`ASK_AGAIN`] until the server answers. In
//!   `custom` mode it serves the uploaded one.
//! * **Standby.** While another runner process of the machine is the newest (an upgrade beside
//!   this one), the server tells this one to stand by: its listeners stop so the other can bind.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use parking_lot::Mutex;

use crate::discovery::Iface;
use crate::dns::Name;
use crate::http::tls::ServerTls;
use crate::rt::{Interval, Notify, Shutdown};

use super::certs::Store;
use super::{CertMode, CertStatus, DnsMode, Fix, Listener, Push, Settings, State, Status, netinfo, proxy, tailnet};

/// How often the front door looks again (interfaces, failed binds, the certificate's age).
pub const TICK: Duration = Duration::from_secs(15);
/// A CSR the server has not answered is asked again after this long.
pub const ASK_AGAIN: Duration = Duration::from_secs(300);
/// Renew a kiln certificate with less than this left.
const RENEW_BEFORE: i64 = 30 * 86_400;
/// IP addresses a kiln certificate carries besides the names (the LAN's IPv4 ones: IPv6
/// addresses on a LAN change too often to be worth a certificate).
const CERT_ADDRESSES: usize = 4;

/// What a runner's front door needs to know about the runner.
#[derive(Clone, Debug)]
pub struct Ctx {
    /// `<state>/front-door`.
    pub dir: PathBuf,
    /// The runner's own `--server` URL: where HTTPS forwards unless the settings say otherwise.
    pub server_url: String,
    /// kiln's CA (DER): it issues the front door's certificate, and an `https://` upstream is
    /// trusted with it.
    pub ca_der: Vec<u8>,
}

enum Command {
    Push(String),
    Ctx(Ctx),
}

/// The newest status, for whoever reports it to the server.
#[derive(Default)]
struct Latest {
    json: Mutex<Option<String>>,
    changed: Notify,
}

/// A runner's front door, shared by every main loop of the runner process.
#[derive(Clone)]
pub struct Handle {
    tx: async_channel::Sender<Command>,
    latest: Arc<Latest>,
}

impl Handle {
    /// What the server sent (`ServerMsg::FrontDoor`).
    pub fn push(&self, json: String) {
        if self.tx.try_send(Command::Push(json)).is_err() {
            tracing::warn!("front door: too many updates at once; one was dropped (the next connect sends it again)");
        }
    }

    /// The newest status (JSON [`Status`]).
    pub fn latest(&self) -> Option<String> {
        self.latest.json.lock().clone()
    }

    /// Completes when the status changed since it was last read through here.
    pub async fn changed(&self) {
        self.latest.changed.notified().await;
    }
}

static RUNNING: LazyLock<Mutex<HashMap<PathBuf, Handle>>> = LazyLock::new(Default::default);

/// The front door of the runner whose state is under `ctx.dir`: started on first use (with the
/// settings it stored, if any) and the same one afterwards.
pub fn start(ctx: Ctx, shutdown: &Shutdown) -> Handle {
    let mut running = RUNNING.lock();
    if let Some(h) = running.get(&ctx.dir)
        && !h.tx.is_closed()
    {
        let _ = h.tx.try_send(Command::Ctx(ctx));
        return h.clone();
    }
    let (tx, rx) = async_channel::bounded(32);
    let latest = Arc::new(Latest::default());
    let handle = Handle { tx, latest: Arc::clone(&latest) };
    running.insert(ctx.dir.clone(), handle.clone());
    let sd = shutdown.clone();
    crate::rt::spawn(async move { run(ctx, rx, latest, sd).await });
    handle
}

async fn run(ctx: Ctx, rx: async_channel::Receiver<Command>, latest: Arc<Latest>, shutdown: Shutdown) {
    let mut fd = FrontDoor::new(ctx);
    let mut tick = Interval::new(TICK);
    // What Tailscale says about this machine changed: listen and report accordingly, at once.
    let tailnet_changes = fd.tailnet.changes();
    loop {
        let json = serde_json::to_string(&fd.status()).unwrap_or_default();
        {
            let mut l = latest.json.lock();
            if l.as_deref() != Some(json.as_str()) {
                *l = Some(json);
                latest.changed.notify();
            }
        }
        let cmd = futures::select_biased! {
            () = shutdown.cancelled().fuse() => break,
            c = rx.recv().fuse() => match c {
                Ok(c) => Some(c),
                Err(_) => break,
            },
            () = tick.tick().fuse() => None,
            () = tailnet_changes.changed().fuse() => None,
        };
        match cmd {
            Some(Command::Push(json)) => fd.on_push(&json),
            Some(Command::Ctx(c)) => {
                fd.ctx = c;
                fd.reconcile();
            }
            None => fd.reconcile(),
        }
    }
    fd.stop();
}

/// The `.local` responder: kiln's multicast DNS engine answering just the front door's name.
struct MdnsRun {
    stop: Shutdown,
    hosts: Arc<Mutex<Vec<Name>>>,
    changed: Arc<Notify>,
    health: Arc<Mutex<crate::discovery::status::Health>>,
}

impl Drop for MdnsRun {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl MdnsRun {
    fn start(name: &str) -> Self {
        let opts = crate::discovery::Options {
            advert: None,
            browse: false,
            ipv6_addresses: true,
            ..crate::discovery::Options::browse_only()
        };
        opts.hosts.lock().push(Name::parse(name));
        let run = Self { stop: Shutdown::new(), hosts: Arc::clone(&opts.hosts), changed: Arc::clone(&opts.changed), health: Arc::clone(&opts.health) };
        let sd = run.stop.clone();
        crate::rt::spawn(async move {
            let noop: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
            if let Err(e) = crate::discovery::run(opts, Arc::new(crate::discovery::Table::default()), sd, noop).await {
                tracing::warn!(error = %format!("{e:#}"), "front door: the .local responder stopped");
            }
        });
        run
    }

    fn rename(&self, name: &str) {
        let n = Name::parse(name);
        let mut hosts = self.hosts.lock();
        if hosts.first() != Some(&n) {
            *hosts = vec![n];
            self.changed.notify();
        }
    }
}

/// A listener that could not start, and why.
struct Failure {
    message: String,
    fix: Option<Fix>,
}

struct FrontDoor {
    ctx: Ctx,
    store: Store,
    settings: Settings,
    standby: bool,
    ifaces: Vec<Iface>,

    dns_shared: Arc<super::dns::Shared>,
    dns: Option<super::dns::DnsServer>,
    dns_failures: Vec<Failure>,

    https_shared: Arc<proxy::Shared>,
    https: Option<proxy::Listening>,
    https_failure: Option<Failure>,
    https_wait: Option<String>,
    redirect: Arc<proxy::Redirect>,
    http: Option<proxy::Listening>,
    http_failure: Option<Failure>,

    mdns: Option<MdnsRun>,
    /// What Tailscale says about this machine (asked while the settings want the tailnet).
    tailnet: tailnet::Watcher,

    cert: CertStatus,
    tls_leaf: Option<String>,
    csr: Option<(String, Vec<String>)>,
    asked: Option<Instant>,
    asked_unix: i64,
    upload_error: Option<String>,
}

impl FrontDoor {
    fn new(ctx: Ctx) -> Self {
        let store = Store::new(ctx.dir.clone());
        let settings = store.load_settings().unwrap_or_default();
        let mut fd = Self {
            ctx,
            store,
            settings,
            standby: false,
            ifaces: Vec::new(),
            dns_shared: Arc::default(),
            dns: None,
            dns_failures: Vec::new(),
            https_shared: Arc::default(),
            https: None,
            https_failure: None,
            https_wait: None,
            redirect: Arc::default(),
            http: None,
            http_failure: None,
            mdns: None,
            tailnet: tailnet::Watcher::start(),
            cert: CertStatus::default(),
            tls_leaf: None,
            csr: None,
            asked: None,
            asked_unix: 0,
            upload_error: None,
        };
        if fd.settings.any() {
            tracing::info!(front_door = %fd.settings.summary(), "front door: starting with the settings stored last");
        }
        fd.reconcile();
        fd
    }

    fn on_push(&mut self, json: &str) {
        let push: Push = match serde_json::from_str(json) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "front door: unreadable settings from the server");
                return;
            }
        };
        if let Some(issued) = &push.issued {
            match self.store.take_issued(issued) {
                Ok(()) => {
                    tracing::info!("front door: kiln's CA issued its certificate");
                    self.csr = None;
                    self.asked = None;
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "front door: the certificate from the server was not taken"),
            }
        }
        if let Some(custom) = &push.custom {
            match self.store.store_custom(custom) {
                Ok(info) => {
                    tracing::info!(names = ?info.names, sha256 = %info.sha256, "front door: stored the uploaded certificate");
                    self.upload_error = None;
                }
                Err(e) => self.upload_error = Some(format!("The uploaded certificate was not taken: {e:#}")),
            }
        }
        self.standby = push.standby;
        if !push.standby {
            match push.settings.validated() {
                Ok(s) => {
                    if s != self.settings {
                        tracing::info!(front_door = %s.summary(), "front door: new settings");
                    }
                    if let Err(e) = self.store.save_settings(&s) {
                        tracing::warn!(error = %format!("{e:#}"), "front door: settings not stored; a restart starts without them");
                    }
                    self.settings = s;
                }
                Err(e) => tracing::warn!(error = %e, "front door: the server's settings do not validate here; keeping the ones in use"),
            }
        }
        self.reconcile();
    }

    fn stop(&mut self) {
        self.dns = None;
        self.https = None;
        self.http = None;
        self.mdns = None;
    }

    /// Match the listeners to the settings (and retry what failed).
    fn reconcile(&mut self) {
        let s = self.settings.clone();
        self.ifaces = netinfo::lan();
        // On the tailnet too: its addresses are the front door's like the LAN's.
        let seen = self.tailnet.seen();
        if s.tailscale.enabled
            && let Some(i) = seen.as_ref().and_then(|x| x.local.as_ref().ok()).and_then(tailnet::iface)
        {
            self.ifaces.push(i);
        }
        let active = !self.standby;
        self.reconcile_dns(&s, active);
        let tls = if active && s.https.enabled { self.reconcile_cert(&s) } else {
            self.cert = CertStatus { mode: s.cert.mode, ..CertStatus::default() };
            self.csr = None;
            None
        };
        self.reconcile_https(&s, active, tls);
        self.reconcile_mdns(&s, active);
        let primary = seen.as_ref().and_then(|x| x.local.as_ref().ok()).and_then(crate::tailscale::Local::primary);
        let listening = primary.is_some_and(|ip| self.dns.as_ref().is_some_and(|d| d.bound().contains(&SocketAddr::new(ip, super::DNS_PORT))));
        self.tailnet.want(tailnet::Probe {
            enabled: active && s.tailscale.enabled,
            resolve: (s.dns.enabled && !s.domain.is_empty()).then(|| s.domain.clone()),
            listening,
        });
    }

    fn reconcile_dns(&mut self, s: &Settings, active: bool) {
        self.dns_failures.clear();
        if !(active && s.dns.enabled && !s.domain.is_empty()) {
            self.dns = None;
            return;
        }
        *self.dns_shared.zone.write() = super::zone::Zone::new(s);
        *self.dns_shared.ifaces.write() = self.ifaces.clone();
        let own = netinfo::addresses(&self.ifaces);
        *self.dns_shared.upstream.write() = match s.dns.mode {
            DnsMode::Resolver => netinfo::upstreams(&s.dns.upstream, &own, s.dns.port),
            DnsMode::Authoritative => Vec::new(),
        };
        let binds = netinfo::dns_binds(s.dns.port, &self.ifaces);
        match &mut self.dns {
            Some(server) => server.ensure(&binds),
            None => self.dns = Some(super::dns::DnsServer::start(&binds, &self.dns_shared)),
        }
        if let Some(server) = &self.dns {
            for (addr, e) in &server.failed {
                // An address that went away (or an IPv6 one not ready yet) is not a problem.
                if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                    continue;
                }
                let (message, fix) = netinfo::bind_problem("DNS", addr, e);
                self.dns_failures.push(Failure { message, fix });
            }
        }
    }

    /// The names a kiln certificate is asked for: the names served and the LAN's IPv4 addresses.
    fn want(&self, s: &Settings) -> Vec<String> {
        let mut v = s.cert_names();
        v.extend(netinfo::addresses(&self.ifaces).into_iter().filter(IpAddr::is_ipv4).take(CERT_ADDRESSES).map(|a| a.to_string()));
        // On the tailnet: its address and MagicDNS name, however many LAN addresses came first.
        if s.tailscale.enabled {
            for n in tailnet::cert_names(self.tailnet.seen().as_ref()) {
                if !v.contains(&n) {
                    v.push(n);
                }
            }
        }
        v
    }

    /// The certificate to serve, and its status; asks the CA for one when needed.
    fn reconcile_cert(&mut self, s: &Settings) -> Option<ServerTls> {
        let now = crate::util::now_unix();
        let names = s.cert_names();
        let mut cs = CertStatus { mode: s.cert.mode, ..CertStatus::default() };
        let tls = match s.cert.mode {
            CertMode::Kiln => {
                self.upload_error = None;
                let want = self.want(s);
                // On the tailnet, the certificate carries its names too: ask once Tailscale answered.
                let settling = s.tailscale.enabled && self.tailnet.seen().is_none();
                let current = self.store.kiln().and_then(|(c, k)| {
                    let info = crate::pki::cert_info(&c).ok()?;
                    let tls = ServerTls::from_pem(&c, &k).ok()?;
                    let ours = tls.leaf().is_some_and(|leaf| crate::pki::verify_issued(&self.ctx.ca_der, leaf));
                    Some((info, tls, ours))
                });
                match current {
                    Some((info, tls, true)) if info.not_after > now => {
                        let missing: Vec<String> = want.iter().filter(|n| !info.names.contains(n)).cloned().collect();
                        let renew = info.not_after - now < RENEW_BEFORE;
                        if missing.is_empty() && !renew {
                            self.csr = None;
                        } else if !settling {
                            self.ask(&want);
                        }
                        cs.missing = missing.into_iter().filter(|n| names.contains(n)).collect();
                        cs.state = State::On;
                        cs.kiln = true;
                        cs.info = Some(info);
                        Some(tls)
                    }
                    _ if settling => {
                        cs.state = State::Waiting;
                        cs.error = Some("Asking Tailscale about this machine first: the certificate carries its tailnet names.".to_owned());
                        None
                    }
                    _ => {
                        self.ask(&want);
                        cs.state = State::Waiting;
                        cs.error = Some("Waiting for kiln's CA to issue it (the runner asked the server).".to_owned());
                        None
                    }
                }
            }
            CertMode::Custom => {
                self.csr = None;
                match self.store.custom() {
                    None => {
                        cs.state = State::Error;
                        cs.error = Some(self.upload_error.clone().unwrap_or_else(|| "No certificate uploaded yet: paste or upload one.".to_owned()));
                        None
                    }
                    Some((c, k)) => match (ServerTls::from_pem(&c, &k), crate::pki::cert_info(&c)) {
                        (Ok(tls), Ok(info)) => {
                            cs.missing = names.iter().filter(|n| !crate::pki::covers(&info.names, n)).cloned().collect();
                            cs.kiln = tls.leaf().is_some_and(|leaf| crate::pki::verify_issued(&self.ctx.ca_der, leaf));
                            if info.not_after <= now {
                                cs.state = State::Error;
                                cs.error = Some("It has expired: upload a new one.".to_owned());
                            } else {
                                cs.state = State::On;
                                cs.error = self.upload_error.clone();
                            }
                            cs.info = Some(info);
                            Some(tls)
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            cs.state = State::Error;
                            cs.error = Some(format!("The stored certificate cannot be used: {e:#}"));
                            None
                        }
                    },
                }
            }
        };
        self.cert = cs;
        tls
    }

    /// Ask the kiln CA for a certificate for `want` (again, when the last ask went unanswered
    /// for [`ASK_AGAIN`] or asked for other names).
    fn ask(&mut self, want: &[String]) {
        let fresh = self.csr.as_ref().is_some_and(|(_, w)| w == want) && self.asked.is_some_and(|t| t.elapsed() < ASK_AGAIN);
        if fresh {
            return;
        }
        match self.store.csr(want) {
            Ok(csr) => {
                self.csr = Some((csr, want.to_vec()));
                self.asked = Some(Instant::now());
                self.asked_unix = crate::util::now_unix();
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "front door: cannot make a certificate request");
                self.csr = None;
            }
        }
    }

    fn reconcile_https(&mut self, s: &Settings, active: bool, tls: Option<ServerTls>) {
        self.https_failure = None;
        self.http_failure = None;
        self.https_wait = None;
        if !(active && s.https.enabled) {
            self.https = None;
            self.http = None;
            *self.https_shared.tls.write() = None;
            self.tls_leaf = None;
            return;
        }
        let url = if s.https.upstream.is_empty() { self.ctx.server_url.clone() } else { s.https.upstream.clone() };
        match proxy::Upstream::parse(&url, &self.ctx.ca_der) {
            Ok(u) => *self.https_shared.upstream.write() = Some(u),
            Err(e) => {
                self.https_failure = Some(Failure { message: format!("{e:#}"), fix: None });
                *self.https_shared.upstream.write() = None;
            }
        }
        let Some(tls) = tls else {
            self.https = None;
            self.http = None;
            self.https_wait = Some("Waiting for its certificate.".to_owned());
            return;
        };
        let leaf = tls.leaf().map(crate::util::sha256_hex);
        if leaf != self.tls_leaf {
            *self.https_shared.tls.write() = Some(tls);
            self.tls_leaf = leaf;
        }
        if self.https.as_ref().is_some_and(|l| l.bound.port() != s.https.port) {
            self.https = None;
        }
        if self.https.is_none() {
            match proxy::listen_https(s.https.port, &self.https_shared) {
                Ok(l) => {
                    tracing::info!(on = %l.bound, to = %url, "front door: HTTPS listening");
                    self.https = Some(l);
                }
                Err(e) => {
                    let (message, fix) = netinfo::bind_problem("HTTPS", &SocketAddr::from(([0, 0, 0, 0], s.https.port)), &e);
                    self.https_failure = Some(Failure { message, fix });
                }
            }
        }
        // Plain HTTP redirects there, once HTTPS answers.
        *self.redirect.names.write() = s.cert_names();
        *self.redirect.primary.write() = if s.domain.is_empty() { s.local_name().unwrap_or_default() } else { s.domain.clone() };
        self.redirect.https_port.store(s.https.port, Ordering::Relaxed);
        if s.https.http_port == 0 || self.https.is_none() {
            self.http = None;
            return;
        }
        if self.http.as_ref().is_some_and(|l| l.bound.port() != s.https.http_port) {
            self.http = None;
        }
        if self.http.is_none() {
            match proxy::listen_redirect(s.https.http_port, &self.redirect) {
                Ok(l) => self.http = Some(l),
                Err(e) => {
                    let (message, fix) = netinfo::bind_problem("The HTTP redirect", &SocketAddr::from(([0, 0, 0, 0], s.https.http_port)), &e);
                    self.http_failure = Some(Failure { message, fix });
                }
            }
        }
    }

    fn reconcile_mdns(&mut self, s: &Settings, active: bool) {
        match s.local_name().filter(|_| active) {
            None => self.mdns = None,
            Some(name) => match &self.mdns {
                Some(m) => m.rename(&name),
                None => self.mdns = Some(MdnsRun::start(&name)),
            },
        }
    }

    fn status(&self) -> Status {
        let s = &self.settings;
        let standing_by = |on: bool| -> Option<Listener> {
            (self.standby && on).then(|| Listener { state: State::Waiting, error: Some("Another runner process of this machine runs it.".to_owned()), ..Listener::default() })
        };
        let dns = standing_by(s.dns.enabled).unwrap_or_else(|| match &self.dns {
            None => Listener::default(),
            Some(server) => {
                let bound = server.bound();
                let first = self.dns_failures.first();
                Listener {
                    state: if bound.is_empty() { State::Error } else { State::On },
                    on: bound.iter().map(SocketAddr::to_string).collect(),
                    error: first.map(|f| f.message.clone()),
                    to: self.dns_shared.upstream.read().iter().map(SocketAddr::to_string).collect(),
                    served: self.dns_shared.served.load(Ordering::Relaxed),
                    fix: first.and_then(|f| f.fix.clone()),
                }
            }
        });
        let https = standing_by(s.https.enabled).unwrap_or_else(|| {
            if !s.https.enabled {
                return Listener::default();
            }
            let to = self.https_shared.upstream.read().as_ref().map(|u| u.url.clone()).into_iter().collect();
            let served = self.https_shared.served.load(Ordering::Relaxed);
            match (&self.https, &self.https_failure, &self.https_wait) {
                (Some(l), failure, _) => Listener {
                    state: if failure.is_some() { State::Error } else { State::On },
                    on: vec![l.bound.to_string()],
                    error: failure.as_ref().map(|f| f.message.clone()),
                    to,
                    served,
                    fix: failure.as_ref().and_then(|f| f.fix.clone()),
                },
                (None, Some(f), _) => Listener { state: State::Error, error: Some(f.message.clone()), to, served, fix: f.fix.clone(), ..Listener::default() },
                (None, None, wait) => Listener { state: State::Waiting, error: wait.clone(), to, served, ..Listener::default() },
            }
        });
        let http = standing_by(s.https.enabled && s.https.http_port != 0).unwrap_or_else(|| {
            if !s.https.enabled || s.https.http_port == 0 {
                return Listener::default();
            }
            match (&self.http, &self.http_failure) {
                (Some(l), _) => Listener { state: State::On, on: vec![l.bound.to_string()], ..Listener::default() },
                (None, Some(f)) => Listener { state: State::Error, error: Some(f.message.clone()), fix: f.fix.clone(), ..Listener::default() },
                (None, None) => Listener { state: State::Waiting, error: Some("Waits for HTTPS.".to_owned()), ..Listener::default() },
            }
        });
        let mdns = standing_by(s.mdns.enabled).unwrap_or_else(|| match &self.mdns {
            None => Listener::default(),
            Some(m) => {
                use crate::discovery::status::MdnsState;
                let on = s.local_name().into_iter().collect();
                match m.health.lock().mdns.state.clone() {
                    MdnsState::Running => Listener { state: State::On, on, ..Listener::default() },
                    MdnsState::Starting => Listener { state: State::Waiting, on, error: Some("Starting.".to_owned()), ..Listener::default() },
                    MdnsState::PortBusy(e) | MdnsState::Failed(e) => Listener { state: State::Error, on, error: Some(format!("Multicast DNS: {e}")), ..Listener::default() },
                    MdnsState::Off(e) => Listener { state: State::Error, on, error: Some(e), ..Listener::default() },
                }
            }
        });
        let mut fixes: Vec<Fix> = Vec::new();
        let failures = self.dns_failures.iter().chain(&self.https_failure).chain(&self.http_failure);
        for f in failures.filter_map(|f| f.fix.clone()) {
            if !fixes.contains(&f) {
                fixes.push(f);
            }
        }
        if !self.standby {
            let mut tcp = Vec::new();
            let mut udp = Vec::new();
            if s.dns.enabled {
                tcp.push(s.dns.port);
                udp.push(s.dns.port);
            }
            if s.https.enabled {
                if s.https.http_port != 0 {
                    tcp.push(s.https.http_port);
                }
                tcp.push(s.https.port);
            }
            if s.mdns.enabled {
                udp.push(crate::discovery::MDNS_PORT);
            }
            tcp.sort_unstable();
            udp.sort_unstable();
            if let Some(f) = netinfo::firewall_fix(&tcp, &udp, &crate::discovery::subnets(&[])) {
                fixes.push(f);
            }
        }
        let tailscale = if self.standby { None } else { tailnet::status(s, self.tailnet.seen().as_ref(), &self.dns.as_ref().map(super::dns::DnsServer::bound).unwrap_or_default()) };
        if let Some(f) = tailscale.as_ref().and_then(|t| t.fix.clone())
            && !fixes.contains(&f)
        {
            fixes.push(f);
        }
        Status {
            settings: s.fingerprint(),
            standby: self.standby,
            os: std::env::consts::OS.to_owned(),
            server: self.ctx.server_url.clone(),
            addresses: netinfo::addresses(&self.ifaces).iter().map(IpAddr::to_string).collect(),
            dns,
            https,
            http,
            mdns,
            cert: if self.standby { CertStatus { mode: s.cert.mode, ..CertStatus::default() } } else { self.cert.clone() },
            csr: self.csr.as_ref().map(|(c, _)| c.clone()),
            want: self.csr.as_ref().map(|(_, w)| w.clone()).unwrap_or_default(),
            asked: if self.csr.is_some() { self.asked_unix } else { 0 },
            fixes,
            tailscale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontdoor::{Cert, Dns, Https};

    fn free_port() -> anyhow::Result<u16> {
        Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
    }

    /// The whole runner side, on loopback and high ports: settings in, listeners up, a CSR out;
    /// the CA's certificate in, HTTPS up with it; standby, and back; settings kept for a restart.
    #[test]
    fn a_front_door_follows_its_settings() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(&dir.path().join("pki"))?;
            let ctx = Ctx { dir: dir.path().join("front-door"), server_url: "http://127.0.0.1:9".into(), ca_der: pki.ca_der.clone() };
            let mut fd = FrontDoor::new(ctx.clone());
            let off = fd.status();
            assert_eq!((off.dns.state, off.https.state, off.mdns.state), (State::Off, State::Off, State::Off));

            let (dns_port, https_port) = (free_port()?, free_port()?);
            let settings = Settings {
                domain: "kiln.test".into(),
                dns: Dns { enabled: true, port: dns_port, mode: DnsMode::Authoritative, ..Dns::default() },
                https: Https { enabled: true, port: https_port, http_port: 0, ..Https::default() },
                cert: Cert { mode: CertMode::Kiln },
                ..Settings::default()
            };
            let push = |s: &Settings, issued: Option<String>, standby: bool| serde_json::to_string(&Push { settings: s.clone(), standby, issued, custom: None });
            fd.on_push(&push(&settings, None, false)?);
            let st = fd.status();
            assert_eq!(st.settings, settings.fingerprint());
            assert_eq!(st.dns.state, State::On, "{:?}", st.dns);
            assert!(st.dns.on.contains(&format!("127.0.0.1:{dns_port}")), "{:?}", st.dns.on);
            assert_eq!(st.https.state, State::Waiting, "no certificate yet");
            assert_eq!(st.cert.state, State::Waiting);
            let csr = st.csr.clone().ok_or_else(|| anyhow::anyhow!("a CSR"))?;
            assert!(st.want.starts_with(&["kiln.test".to_owned(), "*.kiln.test".to_owned()]));
            assert!(st.asked > 0);

            // The server signs it; HTTPS comes up with it.
            let issued = pki.sign_front_door(&csr, &st.want, crate::pki::FRONT_DOOR_DAYS)?;
            fd.on_push(&push(&settings, Some(issued), false)?);
            let st = fd.status();
            assert_eq!(st.https.state, State::On, "{:?}", st.https);
            assert_eq!(st.cert.state, State::On);
            assert!(st.cert.kiln && st.cert.missing.is_empty());
            assert!(st.csr.is_none(), "nothing more to ask");
            assert_eq!(st.https.to, ["http://127.0.0.1:9"]);
            // A TLS client trusting kiln's CA gets through the handshake for the domain.
            let roots = crate::http::tls::ClientTls::new(crate::http::client::Roots::Pem(pki.ca_pem.clone().into_bytes()), None)?;
            let tcp = hyper_rt::tcp::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], https_port))).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            crate::http::tls::TlsStream::connect(crate::http::io::RtFuturesIo::new(tcp), roots.config()?, crate::http::tls::server_name("www.kiln.test")?).await?;

            // Standby: everything stops, the settings stay.
            fd.on_push(&push(&Settings::default(), None, true)?);
            let st = fd.status();
            assert!(st.standby);
            assert_eq!((st.dns.state, st.https.state), (State::Waiting, State::Waiting));
            assert!(fd.dns.is_none() && fd.https.is_none());
            assert_eq!(fd.store.load_settings(), Some(settings.clone()));

            // A restarted runner starts from what it stored, certificate and all.
            drop(fd);
            let mut again = None;
            for _ in 0..50 {
                let fd = FrontDoor::new(ctx.clone());
                if fd.status().https.state == State::On && fd.status().dns.state == State::On {
                    again = Some(fd);
                    break;
                }
                crate::rt::sleep(Duration::from_millis(40)).await;
            }
            let mut fd = again.ok_or_else(|| anyhow::anyhow!("the restarted front door did not come up"))?;
            assert!(fd.status().csr.is_none());

            // Turned off, it lets go of its ports.
            fd.on_push(&push(&Settings::default(), None, false)?);
            let st = fd.status();
            assert_eq!((st.dns.state, st.https.state), (State::Off, State::Off));
            Ok(())
        })
    }

    #[test]
    fn a_custom_certificate_is_needed_in_custom_mode() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open(&dir.path().join("pki"))?;
            let ctx = Ctx { dir: dir.path().join("front-door"), server_url: "http://127.0.0.1:9".into(), ca_der: pki.ca_der.clone() };
            let mut fd = FrontDoor::new(ctx);
            let settings = Settings {
                domain: "kiln.test".into(),
                https: Https { enabled: true, port: free_port()?, http_port: 0, ..Https::default() },
                cert: Cert { mode: CertMode::Custom },
                ..Settings::default()
            };
            let push = |custom: Option<crate::frontdoor::CustomCert>| serde_json::to_string(&Push { settings: settings.clone(), standby: false, issued: None, custom });
            fd.on_push(&push(None)?);
            let st = fd.status();
            assert_eq!(st.cert.state, State::Error);
            assert!(st.csr.is_none(), "custom mode asks the CA for nothing");
            // One for another name: served, and the status says which names it misses.
            let key = crate::pki::new_key_pem()?;
            let other = pki.sign_front_door(&crate::pki::front_door_csr(&["other.test".to_owned()], &key)?, &["other.test".to_owned()], 30)?;
            fd.on_push(&push(Some(crate::frontdoor::CustomCert { cert_pem: other, key_pem: key }))?);
            let st = fd.status();
            assert_eq!((st.cert.state, st.https.state), (State::On, State::On));
            assert_eq!(st.cert.missing, ["kiln.test", "*.kiln.test"]);
            assert!(!serde_json::to_string(&st)?.contains("PRIVATE KEY"), "the status never carries the key");
            // A broken upload is refused, and the stored one stays.
            fd.on_push(&push(Some(crate::frontdoor::CustomCert { cert_pem: "junk".into(), key_pem: "junk".into() }))?);
            let st = fd.status();
            assert_eq!(st.cert.state, State::On);
            assert!(st.cert.error.as_deref().is_some_and(|e| e.contains("not taken")), "{:?}", st.cert.error);
            Ok(())
        })
    }
}
