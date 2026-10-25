//! The runner's cache service (docs/ci.md W24-R08): one cache for a runner's legs, its `kiln run`
//! commands and its agents, over the local content store ([`super::store`]).
//!
//! - **Where**: HTTP on loopback and on a unix socket (owner-only), which a leg's container gets
//!   mounted where loopback isn't its host's.
//! - **Who**: every request carries a token of this runner's ([`super::token`]): one repository,
//!   one trust domain, read or write. As a bearer token, or as Basic's password for tools that
//!   only send that (Gradle, a WebDAV client given a user and password).
//! - **What a token sees**: the entries and objects of the scopes it reads, and nothing of anyone
//!   else's. An object another repository stored is "not found" to it, and is asked for when it
//!   writes it (existence checks are scoped by repository, as `ci/services/chunks.rs` does).
//! - **Up**: a token minted under a grant with an upstream ([`super::upstream`]) sends what it
//!   writes up to the server, by scope, in the background, and a read this runner can't answer
//!   is asked of the server before it's a miss.
//!
//! kiln's own API, under `/_kiln/cas/v1/`:
//!
//! - `GET|HEAD|PUT /objects/<sha256>`: an object by its digest (a PUT's bytes are checked).
//! - `POST /missing` `{ digests }`: those the token's scopes don't hold → `{ missing }`.
//! - `GET|HEAD|PUT|DELETE /entries/<namespace>/<name>`: a name for an object; a hit says its
//!   digest (`x-kiln-digest`) and when it was put (`last-modified`).
//! - `GET /whoami`: the token's scopes.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, StatusCode, header};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::store::{Entry, Kept, Scope, Store, WHOLE_MAX, is_digest};
use super::token::{Claims, Keyring, known_domain};
use super::upstream::{Up, Upstream};
use crate::http::server::{self as srv, Incoming, Request, Response};

/// The largest object the service takes.
pub const MAX_OBJECT: u64 = 16 << 30;
/// Uploads to the server under way at once.
const UPLOADERS: usize = 4;

/// What a token is minted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub repo: String,
    /// `trusted`, `protected`, `untrusted` or `person:<id>` (docs/ci.md W24-R21).
    pub domain: String,
    pub read: bool,
    pub write: bool,
    /// How long it works.
    pub ttl: Duration,
}

/// Where the service listens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoints {
    pub http: Option<SocketAddr>,
    pub unix: Option<PathBuf>,
}

impl Endpoints {
    /// `http://127.0.0.1:<port>`, what tools are given.
    pub fn url(&self) -> Option<String> {
        self.http.map(|a| format!("http://{a}"))
    }
}

/// An upload to the server, and the grant it's for.
struct Job {
    up: Up,
    grant: String,
    upstream: Arc<dyn Upstream>,
}

/// Uploads under way, by grant.
#[derive(Default)]
struct Pending {
    by_grant: Mutex<HashMap<String, usize>>,
    changed: event_listener::Event,
    sent: AtomicU64,
    failed: AtomicU64,
}

impl Pending {
    fn add(&self, grant: &str) {
        *self.by_grant.lock().entry(grant.to_owned()).or_default() += 1;
    }

    fn done(&self, grant: &str) {
        {
            let mut g = self.by_grant.lock();
            if let Some(n) = g.get_mut(grant) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    g.remove(grant);
                }
            }
        }
        self.changed.notify(usize::MAX);
    }

    /// Once nothing `grant` (any, with `None`) wrote is still going up.
    async fn settled(&self, grant: Option<&str>) {
        let idle = || {
            let g = self.by_grant.lock();
            match grant {
                Some(id) => !g.contains_key(id),
                None => g.is_empty(),
            }
        };
        loop {
            if idle() {
                return;
            }
            let wait = self.changed.listen();
            if idle() {
                return;
            }
            wait.await;
        }
    }
}

/// What uploads have done, for the runner's status and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent {
    pub sent: u64,
    pub failed: u64,
}

/// The service.
pub struct Service {
    store: Arc<Store>,
    keys: Keyring,
    /// Grants' upstreams, by grant id.
    grants: Mutex<HashMap<String, Arc<dyn Upstream>>>,
    jobs: async_channel::Sender<Job>,
    pending: Arc<Pending>,
    at: Mutex<Endpoints>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").field("store", &self.store).field("at", &*self.at.lock()).finish_non_exhaustive()
    }
}

static RUNNING: OnceLock<Arc<Service>> = OnceLock::new();

/// This runner's service, once it serves one.
pub fn running() -> Option<&'static Arc<Service>> {
    RUNNING.get()
}

/// Makes `s` this runner's service, for what reaches it without a handle (the harness).
pub fn set_running(s: Arc<Service>) {
    let _ = RUNNING.set(s);
}

impl Service {
    /// The service over the store under `work` (`<work>/_cas`), its tokens signed with the key kept
    /// there.
    pub fn open(work: &Path) -> anyhow::Result<Arc<Service>> {
        let store = Arc::new(Store::open(work)?);
        let keys = Keyring::open(store.root())?;
        Ok(Service::with(store, keys))
    }

    /// The service over `store`, its tokens signed with `keys`.
    pub fn with(store: Arc<Store>, keys: Keyring) -> Arc<Service> {
        let (jobs, rx) = async_channel::unbounded::<Job>();
        let pending = Arc::new(Pending::default());
        for _ in 0..UPLOADERS {
            crate::rt::spawn(uploader(Arc::clone(&store), rx.clone(), Arc::clone(&pending)));
        }
        Arc::new(Service { store, keys, grants: Mutex::new(HashMap::new()), jobs, pending, at: Mutex::new(Endpoints::default()) })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn endpoints(&self) -> Endpoints {
        self.at.lock().clone()
    }

    fn claims_for(a: &Access, grant: Option<String>) -> anyhow::Result<Claims> {
        anyhow::ensure!(!a.repo.trim().is_empty(), "a cache token names its repository");
        anyhow::ensure!(known_domain(&a.domain), "`{}` isn't a trust domain (trusted, protected, untrusted or person:<id>)", a.domain);
        let exp = crate::util::now_unix().saturating_add(i64::try_from(a.ttl.as_secs()).unwrap_or(i64::MAX));
        Ok(Claims { repo: a.repo.clone(), domain: a.domain.clone(), read: a.read, write: a.write, exp, grant })
    }

    /// A token for `a`, whose writes stay on this runner.
    pub fn mint(&self, a: &Access) -> anyhow::Result<String> {
        Ok(self.keys.mint(&Self::claims_for(a, None)?))
    }

    /// A token for `a` whose writes also go up through `upstream` (a leg's: the server, as the
    /// leg), and the grant's id for [`Self::finish`].
    pub fn grant(&self, a: &Access, upstream: Arc<dyn Upstream>) -> anyhow::Result<(String, String)> {
        let id = crate::util::new_id();
        let token = self.keys.mint(&Self::claims_for(a, Some(id.clone()))?);
        self.grants.lock().insert(id.clone(), upstream);
        Ok((token, id))
    }

    /// The grant's uploads finished, then the grant forgotten: its tokens' writes stay here from
    /// then on (a leg's server token ends with the leg).
    pub async fn finish(&self, grant: &str) {
        self.pending.settled(Some(grant)).await;
        self.grants.lock().remove(grant);
    }

    /// Once no upload is under way.
    pub async fn settled(&self) {
        self.pending.settled(None).await;
    }

    pub fn sent(&self) -> Sent {
        Sent { sent: self.pending.sent.load(Ordering::Relaxed), failed: self.pending.failed.load(Ordering::Relaxed) }
    }

    fn upstream_of(&self, c: &Claims) -> Option<(String, Arc<dyn Upstream>)> {
        let id = c.grant.as_ref()?;
        self.grants.lock().get(id).map(|u| (id.clone(), Arc::clone(u)))
    }

    /// What the request's token lets it do, or why it can't do anything ([`unauthorized`]).
    pub(crate) fn claims(&self, req: &Request) -> Result<Claims, &'static str> {
        let Some(token) = token_of(req.headers()) else { return Err("this runner's cache needs a token (KILN_CACHE_TOKEN)") };
        self.keys.verify(&token, crate::util::now_unix())
    }

    // ------------------------------------------------------------------------------ listening

    /// Serves HTTP at `addr` (loopback; port 0 for any): the address it took.
    pub async fn listen_tcp(self: &Arc<Self>, addr: SocketAddr) -> anyhow::Result<SocketAddr> {
        let (bound, listening) = futures::channel::oneshot::channel::<Result<SocketAddr, String>>();
        let me = Arc::clone(self);
        crate::rt::spawn(async move {
            let tcp = match hyper_rt::tcp::TcpListener::bind(addr, 256) {
                Ok(t) => t,
                Err(e) => {
                    let _ = bound.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = bound.send(tcp.local_addr().map_err(|e| e.to_string()));
            while let Ok(stream) = tcp.accept().await {
                let (h, remote) = (Arc::clone(&me), stream.peer_addr().ok());
                crate::rt::spawn(async move { srv::serve_accepted(stream, h, remote, None).await });
            }
        });
        let at = match listening.await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => anyhow::bail!("the cache service can't listen at {addr}: {e}"),
            Err(_) => anyhow::bail!("the cache service can't listen at {addr}: the runtime took no task"),
        };
        self.at.lock().http = Some(at);
        Ok(at)
    }

    /// Serves HTTP on a unix socket at `path` (owner-only; one there already is replaced).
    #[cfg(unix)]
    pub async fn listen_unix(self: &Arc<Self>, path: &Path) -> anyhow::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _ = std::fs::remove_file(path);
        let (bound, listening) = futures::channel::oneshot::channel::<Result<(), String>>();
        let (me, at) = (Arc::clone(self), path.to_path_buf());
        crate::rt::spawn(async move {
            let listener = match hyper_rt::local::LocalListener::bind(&at, 256) {
                Ok(l) => l,
                Err(e) => {
                    let _ = bound.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = bound.send(Ok(()));
            while let Ok(stream) = listener.accept().await {
                let h = Arc::clone(&me);
                if let Err(e) = crate::rt::spawn_local(local_connection(stream, h)) {
                    tracing::warn!(target: "kiln.work", "a cache connection was dropped: {e}");
                }
            }
        });
        match listening.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => anyhow::bail!("the cache service can't listen at {}: {e}", path.display()),
            Err(_) => anyhow::bail!("the cache service can't listen at {}: the runtime took no task", path.display()),
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        self.at.lock().unix = Some(path.to_path_buf());
        Ok(path.to_path_buf())
    }

    // --------------------------------------------------------------------------- operations

    /// The entry `name` of `ns` in the first of the token's scopes that has it: here, else from
    /// the server through the token's grant (kept here then).
    pub(crate) async fn find_entry(&self, c: &Claims, ns: &str, name: &str) -> anyhow::Result<Option<(Scope, Entry)>> {
        let scopes = c.reads();
        if scopes.is_empty() {
            return Ok(None);
        }
        if let Some((scope, e)) = self.store.entry(&scopes, ns, name)
            && self.store.has(&e.digest)
        {
            return Ok(Some((scope, e)));
        }
        let Some((_, upstream)) = self.upstream_of(c) else { return Ok(None) };
        // What the server answers through a token's grant is only as trusted as the token: it's
        // kept in the token's own scope, never in one it merely reads (an untrusted leg's view of
        // the server can't put an entry where trusted builds would read it).
        let own = c.scope();
        for scope in &scopes {
            let tmp = self.store.temp();
            let got = upstream.get(scope, ns, name, &tmp).await;
            if !matches!(got, Ok(true)) {
                let _ = std::fs::remove_file(&tmp);
                if let Err(e) = got {
                    tracing::debug!(target: "kiln.work", error = %format!("{e:#}"), %scope, ns, "the server couldn't be asked for a cache entry");
                }
                continue;
            }
            let (store, t) = (Arc::clone(&self.store), tmp.clone());
            let kept = crate::rt::blocking(move || store.put_file(&t)).await;
            let _ = std::fs::remove_file(&tmp);
            let (digest, size) = kept??;
            let e = Entry { digest, size, at: crate::util::now_unix(), meta: BTreeMap::new() };
            self.store.set_entry(&own, ns, name, e.clone())?;
            return Ok(Some((own, e)));
        }
        Ok(None)
    }

    /// The first of the token's scopes holding the object `digest`: here, else from the server
    /// (an object is the entry of namespace `cas` named by its digest there), checked, and kept in
    /// the token's own scope.
    pub(crate) async fn find_object(&self, c: &Claims, digest: &str) -> anyhow::Result<Option<Scope>> {
        let scopes = c.reads();
        if scopes.is_empty() || !is_digest(digest) {
            return Ok(None);
        }
        if let Some(s) = self.store.holder(&scopes, digest) {
            return Ok(Some(s));
        }
        let Some((_, upstream)) = self.upstream_of(c) else { return Ok(None) };
        let own = c.scope();
        for scope in &scopes {
            let tmp = self.store.temp();
            if !matches!(upstream.get(scope, "cas", digest, &tmp).await, Ok(true)) {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            let (store, t) = (Arc::clone(&self.store), tmp.clone());
            let kept = crate::rt::blocking(move || store.put_file(&t)).await;
            let _ = std::fs::remove_file(&tmp);
            let (got, _) = kept??;
            if got != digest {
                tracing::warn!(target: "kiln.work", object = %digest, got = %got, "the server's copy of an object wasn't what its name says: refused");
                continue;
            }
            self.store.hold(&own, digest)?;
            return Ok(Some(own));
        }
        Ok(None)
    }

    /// The object as a response: its bytes (none for `head`), `extra` headers added.
    pub(crate) async fn object_response(&self, digest: &str, head: bool, extra: &[(HeaderName, String)]) -> anyhow::Result<Option<Response>> {
        let (store, d) = (Arc::clone(&self.store), digest.to_owned());
        let kept = crate::rt::blocking(move || store.kept(&d)).await?;
        let Some(kept) = kept else { return Ok(None) };
        let mut r = match (kept, head) {
            (Kept::Whole(path), true) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                srv::with_header(srv::status(StatusCode::OK), header::CONTENT_LENGTH, &size.to_string())
            }
            (Kept::Chunks(list), true) => srv::with_header(srv::status(StatusCode::OK), header::CONTENT_LENGTH, &list.iter().map(|(_, n)| n).sum::<u64>().to_string()),
            (Kept::Whole(_), false) => {
                let (store, d) = (Arc::clone(&self.store), digest.to_owned());
                let Some(data) = crate::rt::blocking(move || store.read(&d)).await?? else { return Ok(None) };
                srv::response(StatusCode::OK, "application/octet-stream", srv::full(Bytes::from(data)))
            }
            (Kept::Chunks(list), false) => {
                let size: u64 = list.iter().map(|(_, n)| n).sum();
                let (tx, rx) = async_channel::bounded::<Bytes>(2);
                let store = Arc::clone(&self.store);
                crate::rt::spawn(async move {
                    for (d, _) in list {
                        let s = Arc::clone(&store);
                        // A chunk that's gone or wrong ends the body early: the reader sees fewer
                        // bytes than promised, never wrong ones.
                        let Ok(Some(piece)) = crate::rt::blocking(move || s.chunk(&d)).await else { return };
                        if tx.send(Bytes::from(piece)).await.is_err() {
                            return;
                        }
                    }
                });
                srv::with_header(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(rx)), header::CONTENT_LENGTH, &size.to_string())
            }
        };
        r = srv::with_header(r, HeaderName::from_static("x-kiln-digest"), digest);
        for (k, v) in extra {
            if let Ok(v) = HeaderValue::from_str(v) {
                r.headers_mut().insert(k.clone(), v);
            }
        }
        Ok(Some(r))
    }

    /// An entry as a response: its object, its digest, when it was put, and what the tool sent
    /// with it.
    pub(crate) async fn entry_response(&self, e: &Entry, head: bool) -> anyhow::Result<Option<Response>> {
        let mut extra = vec![(header::LAST_MODIFIED, http_date(e.at))];
        for (k, v) in &e.meta {
            if let Ok(name) = HeaderName::from_bytes(k.as_bytes()) {
                extra.push((name, v.clone()));
            }
        }
        self.object_response(&e.digest, head, &extra).await
    }

    /// Takes a request's body into the store: the object's digest and size. `want`: the digest it
    /// must have (else nothing is kept and the answer says why).
    pub(crate) async fn take_body(&self, body: Incoming, want: Option<&str>) -> anyhow::Result<Result<(String, u64), Response>> {
        let got = match receive(&self.store, body).await {
            Ok(g) => g,
            Err(e) => return Ok(Err(msg(StatusCode::BAD_REQUEST, &format!("{e:#}")))),
        };
        let store = Arc::clone(&self.store);
        let kept = match got {
            Received::Bytes(b) => {
                let w = want.map(str::to_owned);
                crate::rt::blocking(move || -> anyhow::Result<Result<(String, u64), String>> {
                    let digest = crate::util::sha256_hex(&b);
                    if let Some(w) = w.filter(|w| *w != digest) {
                        return Ok(Err(format!("the bytes hash to sha256:{digest}, not sha256:{w}")));
                    }
                    Ok(Ok(store.put_bytes(&b)?))
                })
                .await??
            }
            Received::File(path) => {
                let (p, w) = (path.clone(), want.map(str::to_owned));
                let r = crate::rt::blocking(move || -> anyhow::Result<Result<(String, u64), String>> {
                    let digest = crate::sync::blobs::hash_file(&p)?;
                    if let Some(w) = w.filter(|w| *w != digest) {
                        return Ok(Err(format!("the bytes hash to sha256:{digest}, not sha256:{w}")));
                    }
                    Ok(Ok(store.put_file(&p)?))
                })
                .await;
                let _ = std::fs::remove_file(&path);
                r??
            }
        };
        Ok(kept.map_err(|why| msg(StatusCode::BAD_REQUEST, &why)))
    }

    /// `ns`/`name` set to the object `digest` in the token's scope, and sent up through its grant.
    pub(crate) fn set(&self, c: &Claims, ns: &str, name: &str, digest: &str, size: u64, meta: BTreeMap<String, String>) -> anyhow::Result<Entry> {
        let scope = c.writes().ok_or_else(|| anyhow::anyhow!("this token doesn't write"))?;
        let e = Entry { digest: digest.to_owned(), size, at: crate::util::now_unix(), meta };
        if self.store.set_entry(&scope, ns, name, e.clone())? {
            self.send_up(c, Up { scope, ns: ns.to_owned(), name: name.to_owned(), digest: digest.to_owned(), size });
        }
        Ok(e)
    }

    /// The object `digest` held by the token's scope, and sent up through its grant.
    pub(crate) fn hold(&self, c: &Claims, digest: &str, size: u64) -> anyhow::Result<()> {
        let scope = c.writes().ok_or_else(|| anyhow::anyhow!("this token doesn't write"))?;
        if self.store.hold(&scope, digest)? {
            self.send_up(c, Up { scope, ns: "cas".to_owned(), name: digest.to_owned(), digest: digest.to_owned(), size });
        }
        Ok(())
    }

    fn send_up(&self, c: &Claims, up: Up) {
        let Some((grant, upstream)) = self.upstream_of(c) else { return };
        self.pending.add(&grant);
        if let Err(e) = self.jobs.try_send(Job { up, grant: grant.clone(), upstream }) {
            self.pending.done(&grant);
            tracing::warn!(target: "kiln.work", error = %e, "a cache write couldn't be queued for the server");
        }
    }

    // ------------------------------------------------------------------------------ requests

    async fn route(self: &Arc<Self>, req: Request) -> anyhow::Result<Response> {
        let raw = req.uri().path().to_owned();
        let segs: Vec<String> = raw.split('/').filter(|s| !s.is_empty()).map(unescape).collect();
        let s: Vec<&str> = segs.iter().map(String::as_str).collect();
        match s.as_slice() {
            ["_kiln", "cas", "v1", rest @ ..] => self.kiln(req, rest).await,
            _ => Ok(msg(StatusCode::NOT_FOUND, "not a path of this runner's cache")),
        }
    }

    /// kiln's own API.
    async fn kiln(&self, req: Request, rest: &[&str]) -> anyhow::Result<Response> {
        let c = match self.claims(&req) {
            Ok(c) => c,
            Err(why) => return Ok(unauthorized(why)),
        };
        let method = req.method().clone();
        match (&method, rest) {
            (&Method::GET, ["whoami"]) => {
                let reads: Vec<Value> = c.reads().iter().map(|s| json!({ "repo": s.repo, "domain": s.domain })).collect();
                Ok(srv::json(StatusCode::OK, &json!({ "repo": c.repo, "domain": c.domain, "reads": reads, "writes": c.write, "upstream": self.upstream_of(&c).is_some() })))
            }
            (&Method::GET | &Method::HEAD, ["objects", d]) => {
                if c.reads().is_empty() {
                    return Ok(msg(StatusCode::FORBIDDEN, "this token doesn't read"));
                }
                if self.find_object(&c, d).await?.is_none() {
                    return Ok(msg(StatusCode::NOT_FOUND, "no such object"));
                }
                Ok(self.object_response(d, method == Method::HEAD, &[]).await?.unwrap_or_else(|| msg(StatusCode::NOT_FOUND, "no such object")))
            }
            (&Method::PUT, ["objects", d]) => {
                if c.writes().is_none() {
                    return Ok(msg(StatusCode::FORBIDDEN, "this token doesn't write"));
                }
                if !is_digest(d) {
                    return Ok(msg(StatusCode::BAD_REQUEST, "an object is named by its SHA-256, in lowercase hex"));
                }
                let (digest, size) = match self.take_body(req.into_body(), Some(d)).await? {
                    Ok(k) => k,
                    Err(r) => return Ok(r),
                };
                self.hold(&c, &digest, size)?;
                Ok(srv::json(StatusCode::CREATED, &json!({ "digest": digest, "size": size })))
            }
            (&Method::POST, ["missing"]) => {
                let v: Value = srv::body_json(req.into_body(), 16 << 20).await.unwrap_or(Value::Null);
                let asked: Vec<String> = v.get("digests").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                let mut scopes = c.reads();
                scopes.extend(c.writes());
                let missing: Vec<&String> = asked.iter().filter(|d| self.store.holder(&scopes, d).is_none()).collect();
                Ok(srv::json(StatusCode::OK, &json!({ "missing": missing })))
            }
            (&Method::GET | &Method::HEAD, ["entries", ns, name @ ..]) if !name.is_empty() => {
                if c.reads().is_empty() {
                    return Ok(msg(StatusCode::FORBIDDEN, "this token doesn't read"));
                }
                let Some((_, e)) = self.find_entry(&c, ns, &name.join("/")).await? else { return Ok(msg(StatusCode::NOT_FOUND, "no such entry")) };
                Ok(self.entry_response(&e, method == Method::HEAD).await?.unwrap_or_else(|| msg(StatusCode::NOT_FOUND, "no such entry")))
            }
            (&Method::PUT, ["entries", ns, name @ ..]) if !name.is_empty() => {
                if c.writes().is_none() {
                    return Ok(msg(StatusCode::FORBIDDEN, "this token doesn't write"));
                }
                let (digest, size) = match self.take_body(req.into_body(), None).await? {
                    Ok(k) => k,
                    Err(r) => return Ok(r),
                };
                self.set(&c, ns, &name.join("/"), &digest, size, BTreeMap::new())?;
                Ok(srv::json(StatusCode::CREATED, &json!({ "digest": digest, "size": size })))
            }
            (&Method::DELETE, ["entries", ns, name @ ..]) if !name.is_empty() => {
                let Some(scope) = c.writes() else { return Ok(msg(StatusCode::FORBIDDEN, "this token doesn't write")) };
                let gone = self.store.remove_entry(&scope, ns, &name.join("/"))?;
                Ok(srv::status(if gone { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }))
            }
            _ => Ok(msg(StatusCode::NOT_FOUND, "not a path of this runner's cache")),
        }
    }
}

impl srv::Handler for Service {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        match self.route(req).await {
            Ok(r) => r,
            Err(e) => msg(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
        }
    }
}

/// Sends queued writes up until the service is gone.
async fn uploader(store: Arc<Store>, rx: async_channel::Receiver<Job>, pending: Arc<Pending>) {
    while let Ok(job) = rx.recv().await {
        let (s, d) = (Arc::clone(&store), job.up.digest.clone());
        let kept = crate::rt::blocking(move || s.kept(&d)).await.ok().flatten();
        // A whole object goes from where it is; a chunked one is put together first.
        let (path, temp) = match kept {
            Some(Kept::Whole(p)) => (Some(p), None),
            Some(Kept::Chunks(_)) => {
                let (s, d, t) = (Arc::clone(&store), job.up.digest.clone(), store.temp());
                let to = t.clone();
                match crate::rt::blocking(move || s.copy_to(&d, &to)).await {
                    Ok(Ok(true)) => (Some(t.clone()), Some(t)),
                    _ => (None, Some(t)),
                }
            }
            None => (None, None),
        };
        let result = match &path {
            Some(p) => job.upstream.put(&job.up, p).await,
            None => Err(anyhow::anyhow!("the object is no longer on this runner")),
        };
        if let Some(t) = temp {
            let _ = std::fs::remove_file(t);
        }
        match result {
            Ok(_) => {
                pending.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                pending.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "kiln.work", error = %format!("{e:#}"), scope = %job.up.scope, ns = %job.up.ns, "a cache write didn't go up to the server");
            }
        }
        pending.done(&job.grant);
    }
}

/// A request's body: in memory when small, in a file of the store's when large.
enum Received {
    Bytes(Bytes),
    File(PathBuf),
}

/// Takes `body`: up to [`WHOLE_MAX`] in memory, more into a file (at most [`MAX_OBJECT`]).
async fn receive(store: &Store, mut body: Incoming) -> anyhow::Result<Received> {
    let mut held: Vec<Bytes> = Vec::new();
    let mut size = 0u64;
    while let Some(chunk) = srv::next_chunk(&mut body).await? {
        size = size.saturating_add(chunk.len() as u64);
        held.push(chunk);
        if size > WHOLE_MAX {
            let path = store.temp();
            let (tx, done) = crate::rt::file_sink(path.clone(), MAX_OBJECT)?;
            let sent = async {
                for b in held.drain(..) {
                    tx.send(b).await.map_err(|_| anyhow::anyhow!("the body couldn't be written"))?;
                }
                while let Some(chunk) = srv::next_chunk(&mut body).await? {
                    tx.send(chunk).await.map_err(|_| anyhow::anyhow!("the body couldn't be written"))?;
                }
                anyhow::Ok(())
            }
            .await;
            drop(tx);
            let written = done.await;
            if let Err(e) = sent.and(written.map(|_| ())) {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            return Ok(Received::File(path));
        }
    }
    if held.len() == 1 {
        return Ok(Received::Bytes(held.pop().unwrap_or_default()));
    }
    let mut all = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    for b in held {
        all.extend_from_slice(&b);
    }
    Ok(Received::Bytes(Bytes::from(all)))
}

/// One connection on the unix socket: kiln's HTTP over it, its bytes carried through channels.
#[cfg(unix)]
async fn local_connection(stream: hyper_rt::local::LocalStream, handler: Arc<Service>) {
    use futures::{SinkExt as _, StreamExt as _};
    let (mut in_tx, in_rx) = futures::channel::mpsc::channel::<Bytes>(4);
    let (out_tx, mut out_rx) = futures::channel::mpsc::channel::<Bytes>(4);
    let (done_tx, done_rx) = futures::channel::oneshot::channel::<()>();
    let io = Piped { from: in_rx, pending: Bytes::new(), to: out_tx };
    let serve = async move {
        srv::serve_connection(io, handler, None).await;
        let _ = done_tx.send(());
    };
    let s = &stream;
    let reading = async {
        // The client's end of the stream ends the request stream (the sender goes with it).
        let read = async move {
            let mut buf = vec![0u8; 64 << 10];
            while let Ok(n @ 1..) = s.read(&mut buf).await {
                if in_tx.send(Bytes::copy_from_slice(buf.get(..n).unwrap_or_default())).await.is_err() {
                    break;
                }
            }
        };
        futures::pin_mut!(read);
        // The connection served to its end stops the reading too.
        futures::future::select(read, done_rx).await;
    };
    let writing = async {
        while let Some(b) = out_rx.next().await {
            if stream.write_all(&b).await.is_err() {
                break;
            }
        }
        let _ = stream.shutdown(hyper_rt::tcp::Shutdown::Write);
    };
    futures::join!(serve, reading, writing);
}

/// A connection's bytes as channels, as kiln's HTTP serves over them.
#[cfg(unix)]
struct Piped {
    from: futures::channel::mpsc::Receiver<Bytes>,
    pending: Bytes,
    to: futures::channel::mpsc::Sender<Bytes>,
}

#[cfg(unix)]
impl futures::io::AsyncRead for Piped {
    fn poll_read(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut [u8]) -> std::task::Poll<std::io::Result<usize>> {
        use futures::StreamExt as _;
        let this = self.get_mut();
        if this.pending.is_empty() {
            match this.from.poll_next_unpin(cx) {
                std::task::Poll::Ready(Some(b)) => this.pending = b,
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(0)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        let n = this.pending.len().min(buf.len());
        let piece = this.pending.split_to(n);
        if let Some(to) = buf.get_mut(..n) {
            to.copy_from_slice(&piece);
        }
        std::task::Poll::Ready(Ok(n))
    }
}

#[cfg(unix)]
impl futures::io::AsyncWrite for Piped {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.to.poll_ready(cx) {
            std::task::Poll::Ready(Ok(())) => match this.to.start_send(Bytes::copy_from_slice(buf)) {
                Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
                Err(_) => std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            },
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_close(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        self.get_mut().to.close_channel();
        std::task::Poll::Ready(Ok(()))
    }
}

/// The token a request carries: a bearer token, or Basic's password.
pub(crate) fn token_of(headers: &http::HeaderMap) -> Option<String> {
    use base64::Engine as _;
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, rest) = v.split_once(' ')?;
    match scheme.to_ascii_lowercase().as_str() {
        "bearer" | "token" => Some(rest.trim().to_owned()).filter(|t| !t.is_empty()),
        "basic" => {
            let raw = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
            let text = String::from_utf8(raw).ok()?;
            text.split_once(':').map(|(_, pass)| pass.to_owned()).filter(|t| !t.is_empty())
        }
        _ => None,
    }
}

/// `%XX` decoded in a path segment (`+` stays a plus).
pub(crate) fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'%'
            && let Some(v) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `secs` since the epoch as an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub(crate) fn http_date(secs: i64) -> String {
    let fmt = time::macros::format_description!("[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT");
    time::OffsetDateTime::from_unix_timestamp(secs).ok().and_then(|t| t.format(&fmt).ok()).unwrap_or_else(|| "Thu, 01 Jan 1970 00:00:00 GMT".to_owned())
}

pub(crate) fn msg(status: StatusCode, message: &str) -> Response {
    srv::json(status, &json!({ "message": message }))
}

/// A refusal for want of a good token, with Basic's challenge (Gradle's client answers one).
pub(crate) fn unauthorized(why: &str) -> Response {
    srv::with_header(msg(StatusCode::UNAUTHORIZED, why), header::WWW_AUTHENTICATE, "Basic realm=\"kiln cache\"")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runner::work::client::{Conn, Endpoint, Reply};
    use futures::future::BoxFuture;

    pub(crate) fn access(repo: &str, domain: &str, read: bool, write: bool) -> Access {
        Access { repo: repo.into(), domain: domain.into(), read, write, ttl: Duration::from_secs(3600) }
    }

    /// The service on a free loopback port, over a store in `dir`.
    pub(crate) async fn serve(dir: &Path) -> anyhow::Result<(Arc<Service>, SocketAddr)> {
        let svc = Service::open(dir)?;
        let at = svc.listen_tcp(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        Ok((svc, at))
    }

    pub(crate) fn conn(at: SocketAddr) -> Conn {
        Conn::new(Endpoint::Tcp { addr: at.to_string(), prefix: String::new() })
    }

    pub(crate) fn call(c: &mut Conn, method: &str, path: &str, token: Option<&str>, body: &[u8]) -> anyhow::Result<Reply> {
        let auth = token.map(|t| format!("Bearer {t}"));
        let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
        c.request(method, path, &headers, body)
    }

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            out.extend_from_slice(&s.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// An upstream that keeps what it's sent, by scope, and answers reads from it.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub(crate) sent: Mutex<Vec<Up>>,
        pub(crate) held: Mutex<HashMap<(Scope, String, String), Vec<u8>>>,
    }

    impl Upstream for Recorder {
        fn put<'a>(&'a self, up: &'a Up, path: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                let data = std::fs::read(path)?;
                anyhow::ensure!(crate::util::sha256_hex(&data) == up.digest, "sent the wrong bytes");
                self.sent.lock().push(up.clone());
                self.held.lock().insert((up.scope.clone(), up.ns.clone(), up.name.clone()), data);
                Ok(true)
            })
        }

        fn get<'a>(&'a self, scope: &'a Scope, ns: &'a str, name: &'a str, into: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                let found = self.held.lock().get(&(scope.clone(), ns.to_owned(), name.to_owned())).cloned();
                match found {
                    Some(d) => {
                        std::fs::write(into, d)?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            })
        }
    }

    /// W24-R08's acceptance: a token reads and writes only its scopes. Two repositories, two
    /// domains each: what one stores, another never reads nor learns exists, even though the
    /// bytes sit once on this runner; reads go down only (W24-R21).
    #[test]
    fn w24_r08_a_token_reads_and_writes_only_its_scopes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let a_tr = svc.mint(&access("acme/app", "trusted", true, true))?;
            let a_un = svc.mint(&access("acme/app", "untrusted", true, true))?;
            let a_ro = svc.mint(&access("acme/app", "trusted", true, false))?;
            let b_tr = svc.mint(&access("acme/other", "trusted", true, true))?;
            let b_pe = svc.mint(&access("acme/other", "person:ada", true, true))?;
            anyhow::ensure!(svc.mint(&access("acme/app", "everyone", true, true)).is_err(), "only W24-R21's domains");
            crate::rt::blocking(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                let status = |c: &mut Conn, m: &str, p: &str, t: &str, b: &[u8]| call(c, m, p, Some(t), b).map(|r| r.status);
                // A trusted entry of acme/app: its untrusted builds read it; acme/other's never.
                anyhow::ensure!(status(&mut c, "PUT", "/_kiln/cas/v1/entries/sccache/k1", &a_tr, b"trusted build")? == 201);
                let r = call(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", Some(&a_un), b"")?;
                anyhow::ensure!(r.status == 200 && r.body == b"trusted build" && r.header("x-kiln-digest") == Some(crate::util::sha256_hex(b"trusted build").as_str()), "{r:?}");
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", &a_ro, b"")? == 200);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", &b_tr, b"")? == 404);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", &b_pe, b"")? == 404);
                // An untrusted write: trusted never reads it.
                anyhow::ensure!(status(&mut c, "PUT", "/_kiln/cas/v1/entries/sccache/u1", &a_un, b"untrusted build")? == 201);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/u1", &a_tr, b"")? == 404);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/u1", &a_un, b"")? == 200);
                // A person's write: their repository's trusted builds never read it; they read
                // their repository's trusted entries, and no other repository's.
                anyhow::ensure!(status(&mut c, "PUT", "/_kiln/cas/v1/entries/sccache/p1", &b_pe, b"ada's build")? == 201);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/p1", &b_tr, b"")? == 404);
                anyhow::ensure!(status(&mut c, "PUT", "/_kiln/cas/v1/entries/sccache/t2", &b_tr, b"other's trusted")? == 201);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/t2", &b_pe, b"")? == 200);
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", &b_pe, b"")? == 404);
                // Objects: stored once, but another repository can't probe for them; it sends the
                // bytes, and holds them then.
                let blob = b"an rlib".to_vec();
                let d = crate::util::sha256_hex(&blob);
                let obj = format!("/_kiln/cas/v1/objects/{d}");
                anyhow::ensure!(status(&mut c, "PUT", &obj, &a_tr, &blob)? == 201);
                anyhow::ensure!(status(&mut c, "HEAD", &obj, &a_tr, b"")? == 200);
                anyhow::ensure!(status(&mut c, "HEAD", &obj, &b_tr, b"")? == 404, "no probing across repositories");
                anyhow::ensure!(status(&mut c, "GET", &obj, &b_tr, b"")? == 404);
                let m = call(&mut c, "POST", "/_kiln/cas/v1/missing", Some(&b_tr), serde_json::to_string(&json!({ "digests": [d] }))?.as_bytes())?;
                anyhow::ensure!(serde_json::from_slice::<Value>(&m.body)?.get("missing") == Some(&json!([d])), "{m:?}");
                let m = call(&mut c, "POST", "/_kiln/cas/v1/missing", Some(&a_tr), serde_json::to_string(&json!({ "digests": [d] }))?.as_bytes())?;
                anyhow::ensure!(serde_json::from_slice::<Value>(&m.body)?.get("missing") == Some(&json!([])), "{m:?}");
                anyhow::ensure!(status(&mut c, "PUT", &obj, &b_tr, &blob)? == 201);
                anyhow::ensure!(status(&mut c, "GET", &obj, &b_tr, b"")? == 200);
                // Bytes that aren't what the name says are refused.
                anyhow::ensure!(status(&mut c, "PUT", &obj, &a_un, b"poison")? == 400);
                // Read-only writes nothing; no token, or a forged one, nothing at all.
                anyhow::ensure!(status(&mut c, "PUT", "/_kiln/cas/v1/entries/sccache/x", &a_ro, b"x")? == 403);
                anyhow::ensure!(call(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", None, b"")?.status == 401);
                let forged = format!("{}x", a_tr.trim_end_matches(|ch: char| ch != '.'));
                anyhow::ensure!(status(&mut c, "GET", "/_kiln/cas/v1/entries/sccache/k1", &forged, b"")? == 401);
                let who: Value = serde_json::from_slice(&call(&mut c, "GET", "/_kiln/cas/v1/whoami", Some(&b_pe), b"")?.body)?;
                anyhow::ensure!(who.get("reads") == Some(&json!([{ "repo": "acme/other", "domain": "person:ada" }, { "repo": "acme/other", "domain": "trusted" }])), "{who}");
                Ok(())
            })
            .await??;
            Ok(())
        })
    }

    /// W24-R08: writes go up to the server by scope (in the background, once each), and a read
    /// this runner can't answer is asked of the server and kept; a token without a grant sends
    /// nothing up.
    #[test]
    fn w24_r08_writes_go_up_by_scope_and_misses_are_asked_of_the_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let up = Arc::new(Recorder::default());
            let (tr, grant) = svc.grant(&access("acme/app", "trusted", true, true), up.clone())?;
            let (un, _) = svc.grant(&access("acme/app", "untrusted", true, true), up.clone())?;
            let local = svc.mint(&access("acme/app", "trusted", true, true))?;
            let big = noise(12 << 20, 9);
            let (big2, tr2, un2, local2) = (big.clone(), tr.clone(), un.clone(), local.clone());
            crate::rt::blocking(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                for (t, name, body) in [(&tr2, "a", b"one".to_vec()), (&tr2, "a", b"one".to_vec()), (&un2, "b", b"two".to_vec()), (&local2, "c", b"three".to_vec()), (&tr2, "big", big2.clone())] {
                    let r = call(&mut c, "PUT", &format!("/_kiln/cas/v1/entries/turbo/{name}"), Some(t), &body)?;
                    anyhow::ensure!(r.status == 201, "{r:?}");
                }
                Ok(())
            })
            .await??;
            svc.finish(&grant).await;
            svc.settled().await;
            let mut sent: Vec<(String, String, String)> = up.sent.lock().iter().map(|u| (u.scope.key(), u.ns.clone(), u.name.clone())).collect();
            sent.sort();
            anyhow::ensure!(
                sent == vec![("acme/app|trusted|".to_owned(), "turbo".to_owned(), "a".to_owned()), ("acme/app|trusted|".to_owned(), "turbo".to_owned(), "big".to_owned()), ("acme/app|untrusted|".to_owned(), "turbo".to_owned(), "b".to_owned())],
                "{sent:?}"
            );
            anyhow::ensure!(svc.sent() == Sent { sent: 3, failed: 0 });
            // What the server holds and this runner doesn't is fetched, checked and kept.
            up.held.lock().insert((Scope::new("acme/app", "trusted"), "turbo".into(), "remote".into()), b"from the fleet".to_vec());
            let un3 = un.clone();
            crate::rt::blocking(move || -> anyhow::Result<()> {
                let mut c = conn(at);
                let r = call(&mut c, "GET", "/_kiln/cas/v1/entries/turbo/remote", Some(&un3), b"")?;
                anyhow::ensure!(r.status == 200 && r.body == b"from the fleet", "{r:?}");
                Ok(())
            })
            .await??;
            // Kept in the untrusted reader's own scope: its view of the server never becomes a
            // trusted entry here.
            anyhow::ensure!(svc.store().entry(&[Scope::new("acme/app", "untrusted")], "turbo", "remote").is_some(), "kept here");
            anyhow::ensure!(svc.store().entry(&[Scope::new("acme/app", "trusted")], "turbo", "remote").is_none(), "never as trusted");
            let r = crate::rt::blocking(move || call(&mut conn(at), "GET", "/_kiln/cas/v1/entries/turbo/big", Some(&tr), b"")).await??;
            anyhow::ensure!(r.status == 200 && r.body == big, "a large entry streams back whole ({} bytes)", r.body.len());
            Ok(())
        })
    }

    /// W24-R08: the unix socket serves the same API.
    #[cfg(unix)]
    #[test]
    fn w24_r08_the_unix_socket_serves_alike() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, _) = serve(&root).await?;
            let sock = svc.listen_unix(&root.join("cas.sock")).await?;
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            crate::rt::blocking(move || -> anyhow::Result<()> {
                let mut c = Conn::new(Endpoint::Unix(sock));
                anyhow::ensure!(call(&mut c, "PUT", "/_kiln/cas/v1/entries/go/a1", Some(&t), b"output")?.status == 201);
                let r = call(&mut c, "GET", "/_kiln/cas/v1/entries/go/a1", Some(&t), b"")?;
                anyhow::ensure!(r.status == 200 && r.body == b"output", "{r:?}");
                Ok(())
            })
            .await??;
            Ok(())
        })
    }

    /// W24-R08's acceptance: a local hit for an object under 1 MiB takes p99 under 2 ms, over
    /// loopback with a kept-alive connection, as a tool's client does.
    #[test]
    fn w24_r08_local_hits_p99_under_2ms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            let (p50, p99, n) = crate::rt::blocking(move || -> anyhow::Result<(Duration, Duration, usize)> {
                let mut c = conn(at);
                // 64 objects from 1 KiB to just under 1 MiB.
                let objects: Vec<Vec<u8>> = (0..64u64).map(|i| noise(1024 + usize::try_from(i * 16_350).unwrap_or(0), i + 1)).collect();
                for (i, o) in objects.iter().enumerate() {
                    anyhow::ensure!(call(&mut c, "PUT", &format!("/_kiln/cas/v1/entries/bench/{i}"), Some(&t), o)?.status == 201);
                }
                let mut took = Vec::new();
                for round in 0..40 {
                    for (i, o) in objects.iter().enumerate() {
                        let started = std::time::Instant::now();
                        let r = call(&mut c, "GET", &format!("/_kiln/cas/v1/entries/bench/{i}"), Some(&t), b"")?;
                        let elapsed = started.elapsed();
                        anyhow::ensure!(r.status == 200 && r.body.len() == o.len(), "object {i}: {}", r.status);
                        // The first round warms the page cache and the connection.
                        if round > 0 {
                            took.push(elapsed);
                        }
                    }
                }
                took.sort();
                let at_q = |q: f64| took.get(((took.len() as f64 * q).ceil() as usize).saturating_sub(1)).copied().unwrap_or_default();
                Ok((at_q(0.50), at_q(0.99), took.len()))
            })
            .await??;
            println!("w24_r08: {n} local hits under 1 MiB, p50 {p50:?}, p99 {p99:?}");
            anyhow::ensure!(p99 < Duration::from_millis(2), "p99 {p99:?} is 2 ms or more");
            Ok(())
        })
    }

    #[test]
    fn w24_r08_tokens_ride_bearer_or_basic() {
        use base64::Engine as _;
        let mut h = http::HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer abc.def"));
        assert_eq!(token_of(&h).as_deref(), Some("abc.def"));
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("kiln:tok.en"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&basic).unwrap_or(HeaderValue::from_static("")));
        assert_eq!(token_of(&h).as_deref(), Some("tok.en"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Digest x"));
        assert_eq!(token_of(&h), None);
        assert_eq!(unescape("a%2Fb+c%zz"), "a/b+c%zz");
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }
}
