//! Native QUIC bytes on Mantle (CP-T10.2, W27-R08). Each key is a small pointer to immutable
//! content, naming its SHA-256, size and Mantle ETag. The pointer is committed with Mantle's
//! preconditions, so the seam's SHA-256 conditions remain atomic. Multipart parts also live in
//! Mantle; completion streams them, never spooling bytes to kiln's disk. Presigned URLs use
//! kiln's HTTP door, while every storage operation uses mantle-client.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::blobs::{self as seam, Access, Blobs, Cond, Listed, Listing, Meta, Method, Put, Range, Refusal};
use mantle_client::{Client, ClientError, Settings, Sink, Source};
use mantle_protocol::object::{Headers, List, Listing as NativeListing, Op, Reply, Status};
use mantle_s3::checksum::{Algorithm, Checksum};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Requests waiting for the cell's clients. A full queue refuses before retaining another body.
const REQUESTS: usize = 64;
/// A native upload's run, held whole before it's sent (and sent again from there), and a part's
/// piece a completion reads for one: 8 MiB.
const PIECE: u64 = 8 << 20;
/// Pointer and upload documents; even 10,000 selected parts fit, without an unbounded JSON read.
const DOCUMENT: usize = 2 << 20;
const NODES: usize = 64;
/// A request the cell didn't answer for now (its answer lost on the way, its transport's budget
/// full, the cell asking clients to slow down) is asked again until its deadline: at once after a
/// first answer lost, else after a pause that doubles, from a quarter of a second to eight.
const PAUSE: Duration = Duration::from_millis(250);
const PAUSE_MOST: Duration = Duration::from_secs(8);
/// The clients kiln holds: one a node of the cell, each first asking a node of its own, at least
/// two (one writes, one reads a completion's parts as it goes) and at most four. Requests that come
/// together go through them at once, one each, and a listing reads its keys' pointers through all
/// of them. A node (17eb8032) admits two native clients at once and runs two operations at once
/// (`gateway.clients`, `gateway.objects`, fixed), so one a node takes half of what each admits and
/// leaves the rest to its other clients.
const CLIENTS_MOST: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The directory `mantle init client` made: config and its DER credentials.
    pub client: PathBuf,
    /// Optional replacement address of the first seed in that config (host:port).
    pub endpoint: String,
    pub bucket: String,
    /// At most 60 bytes; every seam key still fits Mantle's 1,024-byte key.
    pub prefix: String,
    pub create_bucket: bool,
    /// Each logical request's deadline, including every native exchange it needs.
    pub timeout_s: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { client: PathBuf::new(), endpoint: String::new(), bucket: "kiln".into(), prefix: "kiln".into(), create_bucket: false, timeout_s: 120 }
    }
}

#[derive(Clone)]
struct Scope {
    bucket: String,
    root: String,
    object_max: u64,
    timeout_s: u64,
}

impl Scope {
    fn new(cfg: &Config, object_max: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(!cfg.bucket.is_empty() && cfg.bucket.len() <= 63 && cfg.bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'), "blobs.mantle.bucket is not a bucket name");
        anyhow::ensure!(cfg.prefix.len() <= 60, "blobs.mantle.prefix is longer than 60 bytes");
        if !cfg.prefix.is_empty() { seam::check_key(&cfg.prefix)?; }
        anyhow::ensure!((1..=3600).contains(&cfg.timeout_s), "blobs.mantle.timeout_s must be 1 to 3600");
        Ok(Self { bucket: cfg.bucket.clone(), root: if cfg.prefix.is_empty() { String::new() } else { format!("{}/", cfg.prefix) }, object_max, timeout_s: cfg.timeout_s })
    }
    fn object(&self, key: &str) -> String { format!("{}o/{key}", self.root) }
    fn data(&self, sha: &str) -> String { format!("{}d/{sha}", self.root) }
    fn upload(&self, id: &str) -> String { format!("{}u/{id}/", self.root) }
    fn deadline(&self) -> u64 { hyper_rt::futures::now_ns().saturating_add(self.timeout_s.saturating_mul(1_000_000_000)) }
    fn size(&self, n: u64) -> Result<(), Refusal> {
        if n > self.object_max { Err(Refusal::TooLarge { what: "an object", size: n, limit: self.object_max }) } else { Ok(()) }
    }
}

/// Credentials are read once, never formatted or logged. Clients stay on one hyper-rt shard;
/// the Send + Sync seam exchanges owned requests with that shard through a bounded channel.
struct Loaded {
    id: u64,
    nodes: Vec<(u64, SocketAddr)>,
    certificate: Vec<u8>,
    key: Vec<u8>,
    authority: Vec<u8>,
}

fn bounded_file(path: &Path, most: u64) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(std::fs::metadata(path)?.len() <= most, "{} is larger than {most} bytes", path.display());
    Ok(std::fs::read(path)?)
}

fn address(s: &str) -> anyhow::Result<SocketAddr> {
    s.to_socket_addrs()?.next().ok_or_else(|| anyhow::anyhow!("Mantle endpoint has no address"))
}

impl Loaded {
    fn read(cfg: &Config) -> anyhow::Result<Self> {
        anyhow::ensure!(!cfg.client.as_os_str().is_empty(), "blobs.mantle.client must name a directory made by mantle init client");
        let text = String::from_utf8(bounded_file(&cfg.client.join("config"), 64 << 10)?)?;
        let mut values = std::collections::BTreeMap::new();
        let mut nodes = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (name, value) = line.split_once('=').ok_or_else(|| anyhow::anyhow!("Mantle client config has a line without ="))?;
            let (name, value) = (name.trim(), value.trim());
            if name == "node" {
                anyhow::ensure!(nodes.len() < NODES, "Mantle client config names more than {NODES} nodes");
                let mut words = value.split_whitespace();
                let id = words.next().ok_or_else(|| anyhow::anyhow!("Mantle node is missing its id"))?.parse()?;
                let at = words.next().ok_or_else(|| anyhow::anyhow!("Mantle node is missing its address"))?;
                anyhow::ensure!(words.next().is_none(), "Mantle node has extra fields");
                nodes.push((id, address(at)?));
            } else {
                anyhow::ensure!(matches!(name, "id" | "certificate" | "key" | "authority"), "unknown Mantle client config field {name}");
                anyhow::ensure!(values.insert(name, value).is_none(), "duplicate Mantle client config field {name}");
            }
        }
        anyhow::ensure!(!nodes.is_empty(), "Mantle client config names no node");
        if !cfg.endpoint.is_empty() && let Some((_, at)) = nodes.first_mut() { *at = address(&cfg.endpoint)?; }
        let field = |key: &str| values.get(key).copied().ok_or_else(|| anyhow::anyhow!("Mantle client config is missing {key}"));
        Ok(Self { id: field("id")?.parse()?, nodes, certificate: bounded_file(&cfg.client.join(field("certificate")?), 64 << 10)?, key: bounded_file(&cfg.client.join(field("key")?), 64 << 10)?, authority: bounded_file(&cfg.client.join(field("authority")?), 64 << 10)? })
    }
    /// The `n`th client's settings: the cell's nodes, the `n`th (round) first.
    fn settings(&self, n: usize) -> Result<Settings, Refusal> {
        let mut nodes = self.nodes.clone();
        if !nodes.is_empty() {
            let first = n % nodes.len();
            nodes.rotate_left(first);
        }
        Ok(Settings { id: self.id, bind: if self.nodes.first().is_some_and(|(_, at)| at.is_ipv6()) { "[::]:0" } else { "0.0.0.0:0" }.parse().map_err(unavailable)?, credentials: mantle_client::Credentials { chain: vec![mantle_client::CertificateDer::from(self.certificate.clone())], key: mantle_client::PrivateKeyDer::Pkcs8(mantle_client::PrivatePkcs8KeyDer::from(self.key.clone())), roots: vec![mantle_client::CertificateDer::from(self.authority.clone())] }, nodes, transport_bytes: mantle_client::least_transport(self.nodes.len()).map_err(unavailable)? })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record { sha: String, size: u64, etag: String }
#[derive(Serialize, Deserialize)]
struct Upload {
    key: String,
    #[serde(default)]
    completing: Option<Vec<(u32, String)>>,
}
#[derive(Clone)]
struct Part { key: String, record: Record }

type Answer<T> = Sender<Result<T, Refusal>>;
/// A request of the cell: most go through one client ([`One`]); a listing reads through all of
/// them, and a completion reads its parts through one as another writes it ([`Whole`]).
enum Call {
    One(One),
    Whole(Whole),
}
enum One {
    Put(String, Vec<u8>, Put, Answer<Meta>),
    Get(String, Option<Range>, Answer<(Meta, Vec<u8>)>),
    Head(String, Answer<Meta>),
    Delete(String, Answer<()>),
    Start(String, Answer<String>),
    Part(String, u32, Vec<u8>, Answer<String>),
    Abort(String, Answer<()>),
}
enum Whole {
    List(String, Option<String>, usize, Answer<Listing>),
    Complete(String, Vec<(u32, String)>, Answer<Meta>),
}
struct Task { scope: Arc<Scope>, call: Call }

#[derive(Clone)]
pub struct MantleBlobs {
    scope: Arc<Scope>,
    sender: Sender<Task>,
    clock: Arc<dyn Clock>,
    presign: super::local::Presign,
}

impl MantleBlobs {
    pub fn new(cfg: &Config, clock: Arc<dyn Clock>, object_max: u64, presign: super::local::Presign) -> anyhow::Result<Self> {
        let scope = Arc::new(Scope::new(cfg, object_max)?);
        let loaded = Loaded::read(cfg)?;
        let (sender, receiver) = async_channel::bounded(REQUESTS);
        let (bucket, create) = (cfg.bucket.clone(), cfg.create_bucket);
        crate::rt::spawn(async move {
            if let Err(e) = crate::rt::spawn_local(serve(loaded, bucket, create, receiver)) {
                tracing::warn!(target: "kiln.blobs", event = "refused", error = %e, "Mantle's client could not start");
            }
        });
        Ok(Self { scope, sender, clock, presign })
    }

    /// A separate namespace on the same bounded native client (CP-T10.2 conformance fixtures).
    pub fn with_prefix(&self, prefix: &str) -> anyhow::Result<Self> {
        let cfg = Config { bucket: self.scope.bucket.clone(), prefix: prefix.to_owned(), timeout_s: self.scope.timeout_s, ..Config::default() };
        Ok(Self { scope: Arc::new(Scope::new(&cfg, self.scope.object_max)?), ..self.clone() })
    }

    async fn request<T: Send>(&self, make: impl FnOnce(Answer<T>) -> Call) -> Result<T, Refusal> {
        let (tx, rx) = async_channel::bounded(1);
        self.sender.try_send(Task { scope: Arc::clone(&self.scope), call: make(tx) }).map_err(|e| match e {
            async_channel::TrySendError::Full(_) => Refusal::Full { what: "Mantle requests", limit: REQUESTS as u64 },
            async_channel::TrySendError::Closed(_) => unavailable("Mantle's client has ended"),
        })?;
        rx.recv().await.map_err(|_| unavailable("Mantle's request ended without an answer"))?
    }

    pub fn check_presigned(&self, key: &str, query: &str, method: Method) -> Result<(String, Access), Refusal> {
        self.presign.check(self.clock.as_ref(), key, query, method)
    }

    pub async fn redeem(&self, url: &str, method: Method, body: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, Refusal> {
        let rest = url.strip_prefix(&format!("{}/", self.presign.base)).ok_or_else(|| Refusal::Forbidden("not one of this store's URLs".into()))?;
        let (path, query) = rest.split_once('?').ok_or_else(|| Refusal::Forbidden("no signature".into()))?;
        let (key, access) = self.check_presigned(path, query, method)?;
        match access {
            Access::Get => self.get(&key, None).await.map(|(_, bytes)| Some(bytes)),
            Access::Put { length } => {
                let bytes = body.unwrap_or_default();
                if length != len(bytes.len()) { return Err(Refusal::Forbidden("the body's length differs from its URL".into())); }
                self.put(&key, bytes, Put::default()).await.map(|_| None)
            }
        }
    }
}

fn unavailable(e: impl std::fmt::Display) -> Refusal { Refusal::Unavailable(format!("Mantle: {e}")) }
fn len(n: usize) -> u64 { u64::try_from(n).unwrap_or(u64::MAX) }
fn identity() -> Result<u128, Refusal> { mantle_client::identity().map_err(unavailable) }
fn quoted(s: &str) -> String { format!("\"{s}\"") }
fn modified(r: &Reply) -> i64 { i64::try_from(r.modified).unwrap_or(i64::MAX).saturating_mul(1000) }
fn meta(key: &str, r: &Record, reply: &Reply) -> Meta { Meta { key: key.to_owned(), size: r.size, etag: r.sha.clone(), modified_ms: modified(reply) } }

fn checked(r: Reply, key: &str) -> Result<Reply, Refusal> {
    match r.status {
        Status::Done | Status::BucketAlreadyOwnedByYou => Ok(r),
        Status::NoSuchKey | Status::NoSuchBucket => Err(Refusal::NotFound(key.to_owned())),
        Status::NoSuchUpload => Err(Refusal::NoSuchUpload(key.to_owned())),
        Status::PreconditionFailed | Status::NotModified => Err(Refusal::PreconditionFailed(key.to_owned())),
        Status::InvalidRange => Err(Refusal::InvalidRange { size: r.size }),
        Status::InvalidPart | Status::InvalidPartOrder | Status::EntityTooSmall => Err(Refusal::BadParts(format!("{:?}", r.status))),
        Status::AccessDenied => Err(Refusal::Forbidden("Mantle refused this client's access".into())),
        other => Err(unavailable(format!("{other:?}"))),
    }
}

/// Whether, and why, asking a native exchange again may get it answered: its answer was lost on
/// the way, or no node took it, the transport's budget was full for the moment, or the cell said
/// to slow down. mantle-client asks again only where no node can have taken a request, and a
/// slowed one a few times; what's left is the caller's (its module's "What it asks again"). A
/// mutation asked again may have been applied: the caller tells from the key (see [`json`]).
fn again(asked: &Result<Reply, ClientError>) -> Again {
    match asked {
        Ok(reply) if reply.status == Status::SlowDown => Again::Busy,
        Ok(_) => Again::No,
        Err(ClientError::Lost { .. }) => Again::Lost,
        Err(ClientError::Untaken(_) | ClientError::Transport(mantle_client::Refusal::Budget)) => Again::Busy,
        Err(_) => Again::No,
    }
}

/// Why a native exchange is asked again ([`again`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Again {
    /// Answered for good.
    No,
    /// Its answer was lost on the way, after the client waited its whole period for it.
    Lost,
    /// The cell or the client had no room for it.
    Busy,
}

/// The waits between asks of one request: none after a first answer lost, else [`PAUSE`] doubling
/// to [`PAUSE_MOST`].
struct Pauses {
    next: Duration,
    lost: bool,
}

impl Pauses {
    fn new() -> Self {
        Self { next: PAUSE, lost: false }
    }

    /// The wait before asking again for `why`: `None` where it isn't asked again, or `until` leaves
    /// no room for the wait.
    fn after(&mut self, why: Again, until: u64) -> Option<Duration> {
        let wait = match why {
            Again::No => return None,
            Again::Lost if !self.lost => Duration::ZERO,
            Again::Lost | Again::Busy => {
                let wait = self.next;
                self.next = self.next.saturating_mul(2).min(PAUSE_MOST);
                wait
            }
        };
        self.lost = why == Again::Lost;
        let ns = u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX);
        (hyper_rt::futures::now_ns().saturating_add(ns) < until).then_some(wait)
    }
}

/// Whether this test build loses the cell's first answer to each of kiln's mutations, after the
/// cell applied it (chaos `mantle_lost`): kiln asks again as it would had it been lost on the way.
fn lose() -> bool {
    crate::runner::chaos::get().is_some_and(|c| c.mantle_lost)
}

/// Asks `ask` until it's answered for good, or until `until` leaves no room for the next pause. A
/// `mutation`'s first answer is lost where this test build says so ([`lose`]).
async fn patiently(until: u64, mutation: bool, mut ask: impl AsyncFnMut() -> Result<Reply, ClientError>) -> Result<Reply, ClientError> {
    let mut pauses = Pauses::new();
    let mut losing = mutation && lose();
    loop {
        let asked = ask().await;
        let lost = std::mem::take(&mut losing) && asked.as_ref().is_ok_and(|r| r.status == Status::Done);
        let Some(wait) = pauses.after(if lost { Again::Lost } else { again(&asked) }, until) else { return asked };
        if !wait.is_zero() {
            crate::rt::sleep(wait).await;
        }
    }
}

async fn call(client: &mut Client, op: Op, key: &str, until: u64) -> Result<Reply, Refusal> {
    let mutation = matches!(op, Op::Create { .. } | Op::Delete { .. } | Op::Abort { .. });
    checked(patiently(until, mutation, async || client.call(&op, until).await).await.map_err(unavailable)?, key)
}

struct Bounded { bytes: Vec<u8>, most: usize }
impl Sink for Bounded {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| io::Error::other("Mantle body length overflow"))?;
        if next > self.most { return Err(io::Error::other("Mantle body exceeds its bound")); }
        self.bytes.try_reserve(bytes.len()).map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}
#[derive(Default)]
struct Hashed { sha: Sha256, count: u64 }
impl Sink for Hashed {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.count = self.count.checked_add(len(bytes.len())).ok_or_else(|| io::Error::other("Mantle body length overflow"))?;
        self.sha.update(bytes);
        Ok(())
    }
}
/// A part's bytes, checked against its own SHA-256 as they go into the whole object's.
struct Hashes<'a> { part: &'a mut Hashed, whole: &'a mut Sha256 }
impl Sink for Hashes<'_> {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.whole.update(bytes);
        self.part.write(bytes).await
    }
}

/// A source read again from its first byte: an exchange asked again sends the same bytes.
trait Restart: Source {
    /// Whether its reads finish as they're polled. mantle-client races a put's reads against its
    /// transport and drops the one that loses, which a source must survive having taken nothing
    /// (`mantle_client::Source`): one that reads the cell itself can't, as its own exchange would
    /// be left open, so it's read only a whole run at a time, ahead of the exchange sending it.
    const AT_ONCE: bool;
    fn restart(&mut self);
}

/// Bytes in memory, read from the front.
struct Memory<'a> { bytes: &'a [u8], at: usize }
impl<'a> Memory<'a> {
    fn new(bytes: &'a [u8]) -> Self { Self { bytes, at: 0 } }
}
impl Source for Memory<'_> {
    async fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let rest = self.bytes.get(self.at..).unwrap_or_default();
        let n = into.len().min(rest.len());
        let (src, dst) = (rest.get(..n), into.get_mut(..n));
        let (Some(src), Some(dst)) = (src, dst) else { return Err(io::Error::other("Mantle source range")) };
        dst.copy_from_slice(src);
        self.at = self.at.saturating_add(n);
        Ok(n)
    }
}
impl Restart for Memory<'_> {
    const AT_ONCE: bool = true;
    fn restart(&mut self) { self.at = 0; }
}
/// A native upload's ID once its Open was answered: what asking the upload again resumes.
#[derive(Default)]
struct Opened(Option<String>);
impl mantle_client::Watch for Opened {
    async fn opened(&mut self, upload: &str) -> io::Result<()> {
        self.0 = Some(upload.to_owned());
        Ok(())
    }
    async fn progressed(&mut self, _: &mantle_protocol::object::Progress) -> io::Result<()> { Ok(()) }
}

fn get(scope: &Scope, key: &str, headers: Headers) -> Op { Op::Get { bucket: scope.bucket.clone(), key: key.to_owned(), headers } }

async fn read(client: &mut Client, scope: &Scope, key: &str, headers: Headers, most: usize, until: u64) -> Result<(Reply, Vec<u8>), Refusal> {
    let op = get(scope, key, headers);
    let mut sink = Bounded { bytes: Vec::new(), most };
    let reply = patiently(until, false, async || {
        sink.bytes.clear();
        client.get(&op, &mut sink, until).await
    })
    .await;
    Ok((checked(reply.map_err(unavailable)?, key)?, sink.bytes))
}

/// The SHA-256 and length of `key`'s bytes as the cell gives them, under `headers`.
async fn hashed(client: &mut Client, scope: &Scope, key: &str, headers: Headers, until: u64) -> Result<(Reply, String, u64), Refusal> {
    let op = get(scope, key, headers);
    let mut sink = Hashed::default();
    let reply = patiently(until, false, async || {
        sink = Hashed::default();
        client.get(&op, &mut sink, until).await
    })
    .await;
    let reply = checked(reply.map_err(unavailable)?, key)?;
    Ok((reply, hex::encode(sink.sha.finalize()), sink.count))
}

async fn document<T: serde::de::DeserializeOwned>(client: &mut Client, scope: &Scope, key: &str, until: u64) -> Result<(T, Reply), Refusal> {
    let (reply, bytes) = read(client, scope, key, Headers::default(), DOCUMENT, until).await?;
    Ok((serde_json::from_slice(&bytes).map_err(unavailable)?, reply))
}

/// Writes `size` bytes of `source`, whose SHA-256 is `sha`, at `key` under `headers`'
/// preconditions: what one run holds, read at once, in one put ([`put_once`]); else as a native
/// upload ([`upload`]).
/// `held` says whether the key holds this write, once a put's answer is lost ([`put_once`]).
async fn write<S: Restart>(client: &mut Client, scope: &Scope, key: &str, body: (&mut S, u64, &str), headers: Headers, until: u64, held: impl AsyncFnMut(&mut Client) -> Result<Option<Reply>, Refusal>) -> Result<Reply, Refusal> {
    if one_put::<S>(body.1) { put_once(client, scope, key, body, headers, until, held).await } else { upload(client, scope, key, body, headers, until).await }
}

/// Whether `size` bytes of an `S` go in one put ([`write`]).
fn one_put<S: Restart>(size: u64) -> bool {
    S::AT_ONCE && size <= mantle_s3::body::MIN_PART
}

/// `size` bytes of `source` (`sha` their SHA-256) in one put at `key`, the SHA-256 stated for the
/// node to check before anything commits. Its answer lost, the key says whether the cell applied it
/// (`held`: its reply where it holds this write), so a write applied isn't sent again and leaves no
/// second version; one it didn't is asked again ([`Pauses`]).
async fn put_once<S: Restart>(client: &mut Client, scope: &Scope, key: &str, (source, size, sha): (&mut S, u64, &str), headers: Headers, until: u64, mut held: impl AsyncFnMut(&mut Client) -> Result<Option<Reply>, Refusal>) -> Result<Reply, Refusal> {
    let checksum = Checksum { algorithm: Algorithm::Sha256, bytes: hex::decode(sha).map_err(unavailable)? };
    let op = Op::Put { bucket: scope.bucket.clone(), key: key.to_owned(), id: identity()?, length: size, headers, checksum: Some(checksum), md5: None, trailing: None };
    let mut pauses = Pauses::new();
    let mut losing = lose();
    loop {
        source.restart();
        let asked = client.put(&op, &mut *source, until).await;
        let lost = std::mem::take(&mut losing) && asked.as_ref().is_ok_and(|r| r.status == Status::Done);
        let why = if lost { Again::Lost } else { again(&asked) };
        if why == Again::Lost
            && let Some(reply) = held(&mut *client).await?
        {
            return Ok(reply);
        }
        let Some(wait) = pauses.after(why, until) else { return checked(asked.map_err(unavailable)?, key) };
        if !wait.is_zero() {
            crate::rt::sleep(wait).await;
        }
    }
}

/// `key`'s reply where it holds `size` bytes whose SHA-256 is `sha` ([`put_once`]'s check of a
/// write whose answer was lost).
async fn holds(client: &mut Client, scope: &Scope, key: &str, (size, sha): (u64, &str), until: u64) -> Result<Option<Reply>, Refusal> {
    match hashed(client, scope, key, Headers::default(), until).await {
        Ok((reply, got, count)) if got == sha && count == size => Ok(Some(reply)),
        Ok(_) | Err(Refusal::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `size` bytes of `source` (`sha` their SHA-256) as Mantle's native upload at `key`, held to a
/// CRC-64/NVME: each run, 8 MiB at the most, read whole before it's sent, so no exchange waits on
/// the source, and sent again from memory where its answer is lost (mantle-client's own asks).
/// One given up is resumed with its identity from the node's durable offset, and a Finish the node
/// applied is answered from the version it made. Where the upload is gone (finished, its answer
/// lost, or let go) the key's bytes say which: `sha`'s are done, else a new upload begins. One the
/// node refused (its condition, say) is aborted: it leaves nothing behind.
async fn upload<S: Restart>(client: &mut Client, scope: &Scope, key: &str, (source, size, sha): (&mut S, u64, &str), headers: Headers, until: u64) -> Result<Reply, Refusal> {
    let (mut id, mut opened) = (identity()?, Opened::default());
    let mut pauses = Pauses::new();
    let mut losing = lose();
    loop {
        source.restart();
        let native = mantle_client::Native { bucket: scope.bucket.clone(), key: key.to_owned(), id, upload: opened.0.clone(), checksum: Algorithm::Crc64Nvme, headers: headers.clone() };
        let asked = client.upload_once(&native, (&mut *source, None), (PIECE, &mut opened), until).await;
        let status = asked.as_ref().ok().map(|r| r.status);
        let lost = std::mem::take(&mut losing) && status == Some(Status::Done);
        if status == Some(Status::NoSuchUpload) && native.upload.is_some() {
            match hashed(client, scope, key, Headers::default(), until).await {
                Ok((reply, got, count)) if got == sha && count == size => return Ok(reply),
                Ok(_) | Err(Refusal::NotFound(_)) => (id, opened) = (identity()?, Opened::default()),
                Err(e) => return Err(e),
            }
            continue;
        }
        if status.is_some_and(|s| s != Status::Done && s != Status::SlowDown) {
            if let Some(upload) = opened.0.take() {
                let abort = Op::Abort { bucket: scope.bucket.clone(), key: key.to_owned(), upload, id: identity()? };
                if let Err(e) = call(client, abort, key, until).await {
                    tracing::warn!(target: "kiln.blobs", event = "collect", %key, error = %e, "a Mantle upload the cell refused couldn't be aborted");
                }
            }
            return checked(asked.map_err(unavailable)?, key);
        }
        let Some(wait) = pauses.after(if lost { Again::Lost } else { again(&asked) }, until) else {
            return checked(asked.map_err(unavailable)?, key);
        };
        if !wait.is_zero() {
            crate::rt::sleep(wait).await;
        }
    }
}

/// A document as kiln writes it: `value`, and the write's own id.
#[derive(Serialize)]
struct Written<'a, T> {
    #[serde(flatten)]
    value: &'a T,
    write: String,
}

/// Writes `value` as JSON at `key` under `headers`' preconditions. Mantle judges a put asked again
/// afresh (at 17eb8032 a write is known again by the file the gateway wrote for it, which an ask
/// again doesn't carry), so one whose answer was lost and that was applied comes back refused for
/// its condition. The write's own id in the document says which: a refused write that the key
/// holds now was done.
async fn json<T: Serialize>(client: &mut Client, scope: &Scope, key: &str, value: &T, headers: Headers, until: u64) -> Result<Reply, Refusal> {
    let bytes = serde_json::to_vec(&Written { value, write: format!("{:032x}", identity()?) }).map_err(unavailable)?;
    if bytes.len() > DOCUMENT { return Err(unavailable("a document exceeds its bound")); }
    let held = async |c: &mut Client| match read(c, scope, key, Headers::default(), DOCUMENT, until).await {
        Ok((reply, there)) if there == bytes => Ok(Some(reply)),
        Ok(_) | Err(Refusal::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    };
    match write(client, scope, key, (&mut Memory::new(&bytes), len(bytes.len()), &crate::util::sha256_hex(&bytes)), headers, until, held).await {
        Err(Refusal::PreconditionFailed(refused)) => match read(client, scope, key, Headers::default(), DOCUMENT, until).await {
            Ok((reply, held)) if held == bytes => Ok(reply),
            Ok(_) | Err(Refusal::NotFound(_)) => Err(Refusal::PreconditionFailed(refused)),
            Err(e) => Err(e),
        },
        written => written,
    }
}

/// Bytes kept by their SHA-256 at `key`, once: content kept already isn't sent again (it's looked
/// for first where it would go as a native upload), and what's there is held to `sha` before it's
/// taken as kept.
async fn immutable<S: Restart>(client: &mut Client, scope: &Scope, key: &str, source: &mut S, size: u64, sha: &str, until: u64) -> Result<Record, Refusal> {
    let found = if one_put::<S>(size) { attested(client, scope, key, (size, sha), until).await? } else { kept(client, scope, key, (size, sha), until).await? };
    if let Some(r) = found {
        return Ok(r);
    }
    let held = async |c: &mut Client| holds(c, scope, key, (size, sha), until).await;
    match write(client, scope, key, (source, size, sha), Headers { if_none_match: Some("*".into()), ..Headers::default() }, until, held).await {
        Ok(reply) => Ok(Record { sha: sha.to_owned(), size, etag: reply.etag }),
        Err(Refusal::PreconditionFailed(_)) => kept(client, scope, key, (size, sha), until).await?.ok_or_else(|| unavailable(format!("{key} was there, then wasn't"))),
        Err(e) => Err(e),
    }
}

/// What `key` keeps, where a head says it's `size` bytes whose SHA-256, checked by the node as the
/// one put that wrote them committed, is `sha`: kept without reading them. Anything else there
/// (other bytes, or bytes held to another checksum) is for [`kept`] to judge, once a create
/// (`If-None-Match`) is refused.
async fn attested(client: &mut Client, scope: &Scope, key: &str, (size, sha): (u64, &str), until: u64) -> Result<Option<Record>, Refusal> {
    let op = Op::Head { bucket: scope.bucket.clone(), key: key.to_owned(), headers: Headers::default() };
    let reply = match call(client, op, key, until).await {
        Ok(reply) => reply,
        Err(Refusal::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let sha256 = hex::decode(sha).map_err(unavailable)?;
    let held = reply.checksum.as_ref().is_some_and(|c| c.algorithm == Algorithm::Sha256.code() && c.parts == 0 && c.value == sha256);
    Ok((held && reply.size == size).then(|| Record { sha: sha.to_owned(), size, etag: reply.etag }))
}

/// What `key` keeps, where it's `size` bytes whose SHA-256 is `sha`: `None` where nothing is, and
/// `BadChecksum` where other bytes are.
async fn kept(client: &mut Client, scope: &Scope, key: &str, (size, sha): (u64, &str), until: u64) -> Result<Option<Record>, Refusal> {
    match hashed(client, scope, key, Headers::default(), until).await {
        Ok((reply, got, count)) if got == sha && count == size => Ok(Some(Record { sha: sha.to_owned(), size, etag: reply.etag })),
        Ok((_, got, _)) => Err(Refusal::BadChecksum { want: sha.to_owned(), got }),
        Err(Refusal::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn remove(client: &mut Client, scope: &Scope, key: &str, until: u64) -> Result<(), Refusal> {
    match call(client, Op::Delete { bucket: scope.bucket.clone(), key: key.to_owned(), id: identity()?, headers: Headers::default() }, key, until).await {
        Ok(_) | Err(Refusal::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

struct PartsSource<'a> {
    reader: &'a mut Client,
    scope: &'a Scope,
    parts: &'a [Part],
    index: usize,
    offset: u64,
    bytes: Vec<u8>,
    at: usize,
    until: u64,
    refused: Option<Refusal>,
}
impl Source for PartsSource<'_> {
    async fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if into.is_empty() { return Ok(0); }
        while self.at == self.bytes.len() {
            let Some(part) = self.parts.get(self.index) else { return Ok(0) };
            if self.offset == part.record.size {
                self.index = self.index.saturating_add(1);
                self.offset = 0;
                continue;
            }
            let end = self.offset.saturating_add(PIECE).min(part.record.size).saturating_sub(1);
            let headers = Headers { if_match: Some(quoted(&part.record.etag)), range: Some(format!("bytes={}-{end}", self.offset)), ..Headers::default() };
            match read(self.reader, self.scope, &part.key, headers, PIECE as usize, self.until).await {
                Ok((_, bytes)) if !bytes.is_empty() => {
                    self.offset = self.offset.saturating_add(len(bytes.len()));
                    self.bytes = bytes;
                    self.at = 0;
                }
                Ok(_) => return Err(io::Error::other("Mantle returned an empty part range")),
                Err(e) => { let why = e.to_string(); self.refused = Some(e); return Err(io::Error::other(why)); }
            }
        }
        let n = into.len().min(self.bytes.len().saturating_sub(self.at));
        let end = self.at.saturating_add(n);
        let src = self.bytes.get(self.at..end).ok_or_else(|| io::Error::other("Mantle source range"))?;
        let dst = into.get_mut(..n).ok_or_else(|| io::Error::other("Mantle destination range"))?;
        dst.copy_from_slice(src);
        self.at = end;
        Ok(n)
    }
}
impl Restart for PartsSource<'_> {
    const AT_ONCE: bool = false;
    fn restart(&mut self) {
        (self.index, self.offset, self.at, self.refused) = (0, 0, 0, None);
        self.bytes.clear();
    }
}

/// `key`'s pointer, through `client`: the record it holds, and the reply that read it.
async fn record(client: &mut Client, s: &Scope, key: &str, until: u64) -> Result<(Record, Reply), Refusal> {
    let (r, reply): (Record, Reply) = document(client, s, &s.object(key), until).await.map_err(|e| match e { Refusal::NotFound(_) => Refusal::NotFound(key.to_owned()), e => e })?;
    seam::check_sha256(&r.sha).map_err(unavailable)?;
    s.size(r.size)?;
    Ok((r, reply))
}

/// A listed key's object, from its pointer.
async fn listed(client: &mut Client, s: &Scope, key: &str, until: u64) -> Result<Listed, Refusal> {
    let (r, pointer) = record(client, s, key, until).await?;
    Ok(Listed { key: key.to_owned(), size: r.size, modified_ms: modified(&pointer) })
}

/// `key`'s object, written as `how` says, through `client`.
async fn put(client: &mut Client, s: &Scope, key: &str, bytes: Vec<u8>, how: Put, until: u64) -> Result<Meta, Refusal> {
    let headers = match how.cond {
        Cond::Always => Headers::default(),
        Cond::IfNoneMatch => Headers { if_none_match: Some("*".into()), ..Headers::default() },
        Cond::IfMatch(want) => {
            let (r, old) = record(client, s, key, until).await.map_err(|e| match e { Refusal::NotFound(_) => Refusal::PreconditionFailed(key.to_owned()), e => e })?;
            seam::holds(&Cond::IfMatch(want), key, Some(&r.sha))?;
            Headers { if_match: Some(quoted(&old.etag)), ..Headers::default() }
        }
    };
    let sha = crate::util::sha256_hex(&bytes);
    let r = immutable(client, s, &s.data(&sha), &mut Memory::new(&bytes), len(bytes.len()), &sha, until).await?;
    let reply = json(client, s, &s.object(key), &r, headers, until).await.map_err(|e| match e { Refusal::PreconditionFailed(_) => Refusal::PreconditionFailed(key.to_owned()), e => e })?;
    Ok(meta(key, &r, &reply))
}

/// `key`'s object, or `range` of it, through `client`.
async fn fetch(client: &mut Client, s: &Scope, key: &str, range: Option<Range>, until: u64) -> Result<(Meta, Vec<u8>), Refusal> {
    let (r, pointer) = record(client, s, key, until).await?;
    let headers = match range {
        Some(range) => {
            let (first, last) = seam::byte_range(range, r.size)?;
            Headers { range: Some(format!("bytes={first}-{last}")), ..Headers::default() }
        }
        None => Headers::default(),
    };
    let (reply, bytes) = read(client, s, &s.data(&r.sha), headers, usize::try_from(s.object_max).unwrap_or(usize::MAX), until).await?;
    if range.is_none() {
        let got = crate::util::sha256_hex(&bytes);
        if got != r.sha || len(bytes.len()) != r.size { return Err(Refusal::BadChecksum { want: r.sha, got }); }
    } else if reply.etag != r.etag {
        let headers = Headers { if_match: Some(quoted(&reply.etag)), ..Headers::default() };
        let (_, got, count) = hashed(client, s, &s.data(&r.sha), headers, until).await?;
        if got != r.sha || count != r.size { return Err(Refusal::BadChecksum { want: r.sha, got }); }
    }
    Ok((meta(key, &r, &pointer), bytes))
}

/// A page of the keys under `prefix`, their pointers read through each of `clients` at once.
async fn list(clients: &mut [&mut Client], s: &Scope, prefix: &str, after: Option<String>, limit: usize, until: u64) -> Result<Listing, Refusal> {
    let Some(first) = clients.first_mut() else { return Err(unavailable("Mantle's client has ended")) };
    let start = after.map(|a| mantle_s3::list::Resume::After(s.object(&a)));
    let reply = call(first, Op::List(List { bucket: s.bucket.clone(), prefix: s.object(prefix), delimiter: String::new(), start, max_keys: u32::try_from(limit).unwrap_or(1000), fetch_owner: false }), prefix, until).await?;
    let Some(NativeListing::Keys(page)) = reply.listing else { return Err(unavailable("a listing has no keys page")) };
    let root = s.object("");
    let keys = page.contents.iter().map(|item| item.key.strip_prefix(&root).ok_or_else(|| unavailable("a listing escaped its namespace"))).collect::<Result<Vec<_>, _>>()?;
    // Each key's size is in its pointer (Mantle's listing names the pointer's own): read as
    // many at once as there are clients, one through each.
    let mut objects = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(clients.len().max(1)) {
        let reads = clients.iter_mut().zip(chunk).map(|(client, key)| listed(client, s, key, until));
        for r in futures::future::join_all(reads).await {
            objects.push(r?);
        }
    }
    Ok(Listing { objects, truncated: page.truncated })
}

/// Upload `id`'s document, through `client`.
async fn staged(client: &mut Client, s: &Scope, id: &str, until: u64) -> Result<(Upload, Reply), Refusal> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(Refusal::NoSuchUpload(id.to_owned())); }
    document(client, s, &format!("{}meta", s.upload(id)), until).await.map_err(|e| match e { Refusal::NotFound(_) => Refusal::NoSuchUpload(id.to_owned()), e => e })
}

async fn start(client: &mut Client, s: &Scope, key: String, until: u64) -> Result<String, Refusal> {
    let id = format!("{:032x}", identity()?);
    json(client, s, &format!("{}meta", s.upload(&id)), &Upload { key, completing: None }, Headers { if_none_match: Some("*".into()), ..Headers::default() }, until).await?;
    Ok(id)
}

async fn part(client: &mut Client, s: &Scope, id: &str, number: u32, bytes: Vec<u8>, until: u64) -> Result<String, Refusal> {
    let (upload, _) = staged(client, s, id, until).await?;
    if upload.completing.is_some() { return Err(Refusal::BadParts("the upload is already completing".into())); }
    let sha = crate::util::sha256_hex(&bytes);
    let at = format!("{}d/{number}", s.upload(id));
    let size = len(bytes.len());
    let held = async |c: &mut Client| holds(c, s, &at, (size, &sha), until).await;
    let reply = write(client, s, &at, (&mut Memory::new(&bytes), size, &sha), Headers::default(), until, held).await?;
    let r = Record { sha: sha.clone(), size, etag: reply.etag };
    json(client, s, &format!("{}p/{number}", s.upload(id)), &r, Headers::default(), until).await?;
    Ok(sha)
}

async fn clean(client: &mut Client, s: &Scope, id: &str, until: u64) -> Result<(), Refusal> {
    let prefix = s.upload(id);
    let marker = format!("{prefix}meta");
    // Two keys per part and its marker; delete a bounded page, then ask for the first again.
    for _ in 0..24 {
        let reply = call(client, Op::List(List { bucket: s.bucket.clone(), prefix: prefix.clone(), delimiter: String::new(), start: None, max_keys: 1000, fetch_owner: false }), id, until).await?;
        let Some(NativeListing::Keys(page)) = reply.listing else { return Err(unavailable("an upload listing has no page")) };
        let mut removed = false;
        for item in page.contents {
            if item.key != marker { remove(client, s, &item.key, until).await?; removed = true; }
        }
        if !removed { return remove(client, s, &marker, until).await; }
    }
    Err(unavailable("an upload has more than 10,000 parts' keys"))
}

/// Upload `id` completed from its `selected` parts: written through `main` as `reader` reads them.
async fn complete(main: &mut Client, reader: &mut Client, s: &Scope, id: &str, selected: Vec<(u32, String)>, until: u64) -> Result<Meta, Refusal> {
    let (mut upload, marker) = staged(main, s, id, until).await?;
    if upload.completing.as_ref().is_some_and(|old| *old != selected) { return Err(Refusal::BadParts("completion already names other parts".into())); }
    let mut parts = Vec::with_capacity(selected.len());
    let mut size = 0u64;
    for (i, (number, want)) in selected.iter().enumerate() {
        let (r, _): (Record, Reply) = document(main, s, &format!("{}p/{number}", s.upload(id)), until).await.map_err(|e| match e { Refusal::NotFound(_) => Refusal::BadParts(format!("part {number} is missing")), e => e })?;
        if r.sha != *want { return Err(Refusal::BadParts(format!("part {number} has another etag"))); }
        if i.saturating_add(1) < selected.len() && r.size < seam::PART_MIN as u64 { return Err(Refusal::BadParts(format!("part {number} is smaller than 5 MiB"))); }
        size = size.checked_add(r.size).ok_or(Refusal::TooLarge { what: "an object", size: u64::MAX, limit: s.object_max })?;
        s.size(size)?;
        parts.push(Part { key: format!("{}d/{number}", s.upload(id)), record: r });
    }
    if upload.completing.is_none() {
        upload.completing = Some(selected);
        json(main, s, &format!("{}meta", s.upload(id)), &upload, Headers { if_match: Some(quoted(&marker.etag)), ..Headers::default() }, until).await?;
    }
    let mut whole = Sha256::new();
    for part in &parts {
        // Each part checked against the SHA-256 its upload answered as it goes into the
        // whole's: asked again, from the whole as it stood before the part.
        let before = whole.clone();
        let op = get(s, &part.key, Headers { if_match: Some(quoted(&part.record.etag)), ..Headers::default() });
        let mut sink = Hashed::default();
        let reply = patiently(until, false, async || {
            (whole, sink) = (before.clone(), Hashed::default());
            reader.get(&op, &mut Hashes { part: &mut sink, whole: &mut whole }, until).await
        })
        .await;
        checked(reply.map_err(unavailable)?, id)?;
        let got = hex::encode(sink.sha.finalize());
        if got != part.record.sha || sink.count != part.record.size { return Err(Refusal::BadChecksum { want: part.record.sha.clone(), got }); }
    }
    let sha = hex::encode(whole.finalize());
    let mut source = PartsSource { reader, scope: s, parts: &parts, index: 0, offset: 0, bytes: Vec::new(), at: 0, until, refused: None };
    let result = immutable(main, s, &s.data(&sha), &mut source, size, &sha, until).await;
    if let Some(e) = source.refused { return Err(e); }
    let r = result?;
    let pointer = json(main, s, &s.object(&upload.key), &r, Headers::default(), until).await?;
    if let Err(e) = clean(main, s, id, until).await { tracing::warn!(target: "kiln.blobs", event = "collect", error = %e, "a committed Mantle upload's staging keys could not all be removed"); }
    Ok(meta(&upload.key, &r, &pointer))
}

/// Answers `call` through `client`. An answer whose asker has gone is dropped: there's no one left
/// to tell.
async fn one(client: &mut Client, scope: Arc<Scope>, call: One) {
    let s = scope.as_ref();
    let until = s.deadline();
    match call {
        One::Put(key, bytes, how, tx) => {
            let _ = tx.send(put(client, s, &key, bytes, how, until).await).await;
        }
        One::Get(key, range, tx) => {
            let _ = tx.send(fetch(client, s, &key, range, until).await).await;
        }
        One::Head(key, tx) => {
            let r = record(client, s, &key, until).await.map(|(r, pointer)| meta(&key, &r, &pointer));
            let _ = tx.send(r).await;
        }
        One::Delete(key, tx) => {
            let _ = tx.send(remove(client, s, &s.object(&key), until).await).await;
        }
        One::Start(key, tx) => {
            let _ = tx.send(start(client, s, key, until).await).await;
        }
        One::Part(upload, number, bytes, tx) => {
            let _ = tx.send(part(client, s, &upload, number, bytes, until).await).await;
        }
        One::Abort(upload, tx) => {
            let r = match staged(client, s, &upload, until).await {
                Ok(_) => clean(client, s, &upload, until).await,
                Err(e) => Err(e),
            };
            let _ = tx.send(r).await;
        }
    }
}

/// Answers `call` through `clients`: a listing through each, a completion through the first two.
async fn whole(clients: &mut [&mut Client], scope: Arc<Scope>, call: Whole) {
    let s = scope.as_ref();
    let until = s.deadline();
    match call {
        Whole::List(prefix, after, limit, tx) => {
            let _ = tx.send(list(clients, s, &prefix, after, limit, until).await).await;
        }
        Whole::Complete(upload, parts, tx) => {
            let r = match clients {
                [main, reader, ..] => complete(main, reader, s, &upload, parts, until).await,
                _ => Err(unavailable("a completion takes two of Mantle's clients")),
            };
            let _ = tx.send(r).await;
        }
    }
}

async fn serve(loaded: Loaded, bucket: String, create: bool, receiver: Receiver<Task>) {
    let opened = async {
        let mut clients = Vec::new();
        for n in 0..loaded.nodes.len().clamp(2, CLIENTS_MOST) {
            clients.push(Client::open(loaded.settings(n)?).await.map_err(unavailable)?);
        }
        Ok::<_, Refusal>(clients)
    }
    .await;
    let mut clients = match opened {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "kiln.blobs", event = "refused", error = %e, "Mantle's client could not open");
            return;
        }
    };
    let mut ready = false;
    while let Ok(first) = receiver.recv().await {
        let until = first.scope.deadline();
        // Those queued with it, up to one a client: a caller waits for each answer before it asks
        // again, so requests queued together are concurrent ones, answered at once, each through a
        // client of its own. A listing or a completion takes more than one: it goes after them.
        let mut tasks = vec![first];
        while tasks.len() < clients.len()
            && tasks.last().is_some_and(|t| matches!(t.call, Call::One(_)))
            && let Ok(t) = receiver.try_recv()
        {
            tasks.push(t);
        }
        if !ready {
            let Some(client) = clients.first_mut() else { return };
            if let Err(e) = bucket_ready(client, &bucket, create, until).await {
                for t in tasks {
                    fail(t.call, e.clone()).await;
                }
                continue;
            }
            ready = true;
        }
        let (mut ones, mut alone) = (Vec::with_capacity(tasks.len()), None);
        for t in tasks {
            match t.call {
                Call::One(call) => ones.push((t.scope, call)),
                Call::Whole(call) => alone = Some((t.scope, call)),
            }
        }
        let mut each: Vec<&mut Client> = clients.iter_mut().collect();
        futures::future::join_all(ones.into_iter().zip(each.iter_mut()).map(|((scope, call), client)| one(client, scope, call))).await;
        if let Some((scope, call)) = alone {
            whole(&mut each, scope, call).await;
        }
    }
}

/// The bucket, there before the first request is asked of it: made, where `create` says so.
async fn bucket_ready(client: &mut Client, bucket: &str, create: bool, until: u64) -> Result<(), Refusal> {
    match call(client, Op::HeadBucket { bucket: bucket.to_owned() }, bucket, until).await {
        Ok(_) => Ok(()),
        Err(Refusal::NotFound(_)) if create => call(client, Op::Create { bucket: bucket.to_owned(), id: identity()? }, bucket, until).await.map(|_| ()),
        Err(e) => Err(e),
    }
}

/// Answers `call` with `why`, asking nothing of the cell.
async fn fail(call: Call, why: Refusal) {
    match call {
        Call::One(One::Put(.., tx) | One::Head(_, tx)) | Call::Whole(Whole::Complete(.., tx)) => {
            let _ = tx.send(Err(why)).await;
        }
        Call::One(One::Get(.., tx)) => {
            let _ = tx.send(Err(why)).await;
        }
        Call::One(One::Delete(_, tx) | One::Abort(_, tx)) => {
            let _ = tx.send(Err(why)).await;
        }
        Call::Whole(Whole::List(.., tx)) => {
            let _ = tx.send(Err(why)).await;
        }
        Call::One(One::Start(_, tx) | One::Part(.., tx)) => {
            let _ = tx.send(Err(why)).await;
        }
    }
}

impl Blobs for MantleBlobs {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            self.scope.size(len(bytes.len()))?;
            seam::check_content(&put, &bytes)?;
            self.request(|tx| Call::One(One::Put(key.into(), bytes, put, tx))).await
        })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            self.request(|tx| Call::One(One::Get(key.into(), range, tx))).await
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            self.request(|tx| Call::One(One::Head(key.into(), tx))).await
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            self.request(|tx| Call::One(One::Delete(key.into(), tx))).await
        })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        Box::pin(async move {
            let limit = seam::list_limit(limit)?;
            self.request(|tx| Call::Whole(Whole::List(prefix.into(), after.map(str::to_owned), limit, tx))).await
        })
    }

    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_key(key)?;
            self.request(|tx| Call::One(One::Start(key.into(), tx))).await
        })
    }

    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            seam::check_part(part)?;
            self.scope.size(len(bytes.len()))?;
            self.request(|tx| Call::One(One::Part(upload.into(), part, bytes, tx))).await
        })
    }

    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            seam::check_parts(&parts)?;
            self.request(|tx| Call::Whole(Whole::Complete(upload.into(), parts, tx))).await
        })
    }

    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move { self.request(|tx| Call::One(One::Abort(upload.into(), tx))).await })
    }

    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move { self.presign.url(self.clock.as_ref(), key, access, expires) })
    }
}
