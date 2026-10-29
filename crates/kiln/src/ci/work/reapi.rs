//! The Remote Execution API's cache, served (docs/ci.md W24-R26): `ContentAddressableStorage`,
//! `ActionCache` and `Capabilities` of `build.bazel.remote.execution.v2`, and `google.bytestream`
//! for blobs past a batch, as gRPC over kiln-http's HTTP/2, whose trailers carry each call's
//! status. `Execution` isn't served: remote execution waits on Hex's placement and sandboxes
//! (CP-T7), and a client asking for it is told so, `UNIMPLEMENTED`.
//!
//! - **Who.** A leg, by its token (`authorization: Bearer $ACTIONS_RUNTIME_TOKEN`), as the cache
//!   services take it: it reads and writes as its cache mode lets it (W08-R14), in its repository's
//!   trust domain (W24-R21). An untrusted leg's action results go to its domain's cache, and it
//!   reads that, then the trusted one; a trusted leg reads only its own. `instance_name` is taken
//!   and not used: the token says whose cache it is.
//! - **Blobs** are kiln's content store's (`cas/<sha256>`, `ci::services::cas`), each checked
//!   against its digest as it arrives. A repository is told of, and reads, only the blobs it has
//!   put through this API (`reapi_blobs`), so no one probes for another's content through
//!   deduplication (as W24-R08 has it for the runner's cache); the empty blob is everyone's. A
//!   blob is held while the store keeps it: every lookup touches it, and the store lets go of what
//!   no one has used for an hour and nothing refers to.
//! - **Action results** are the work ledger's, on its one path (W24-R30, through
//!   [`super::ledger::Ledger`]): each the result of a unit of kind `action` whose definition is the
//!   action's digest ([`action_unit`]), a [`super::Recorded`] that names its blobs and keeps the
//!   client's `ActionResult` as it came. Only successes are kept (W24-R07), and an entry is never
//!   overwritten (W24-R22): the same outputs again reproduce it, other ones are refused, kept as
//!   evidence and have the entry judged again (W24-R23). A leg reads the domains its own reads, the
//!   most trusted first (W24-R21). One is answered only while every blob it names (its trees' files
//!   too) is still held: else it's a miss, and the client runs the action again.
//! - **gRPC.** Messages are length-prefixed and uncompressed (a compressed one is refused
//!   `UNIMPLEMENTED`), at most 16 MiB each; batches are advertised at 4 MiB. An answer ends with
//!   `grpc-status` and `grpc-message` in its trailers, or in its headers when there's nothing else
//!   to say (gRPC's trailers-only answer).

pub mod pb;

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use base64::Engine as _;
use bytes::{Buf as _, Bytes, BytesMut};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use http_body::Frame;
use http_body_util::BodyExt as _;
use kiln_ci::work::key::{Kind, PathSet, Platform, Unit};
use kiln_ci::work::trust::Domain;
use pb::{Digest, Method};
use rusqlite::{OptionalExtension as _, params};
use sha2::Digest as _;

use super::ledger::Ledger;
use super::{Provenance, Recorded, Written};
use crate::ci::Fail;
use crate::ci::services::cas;
use crate::ci::token::{self, Claims};
use crate::http::server::{self as srv, Incoming, Request, Response};
use crate::hub::Hub;

const CAS: &str = "build.bazel.remote.execution.v2.ContentAddressableStorage";
const AC: &str = "build.bazel.remote.execution.v2.ActionCache";
const CAPABILITIES: &str = "build.bazel.remote.execution.v2.Capabilities";
const EXECUTION: &str = "build.bazel.remote.execution.v2.Execution";
const BYTESTREAM: &str = "google.bytestream.ByteStream";

/// Whether `service`, a gRPC call's path's first segment, is one of this API's.
pub fn serves(service: &str) -> bool {
    matches!(service, CAS | AC | CAPABILITIES | EXECUTION | BYTESTREAM)
}

/// The largest message taken.
const MESSAGE_MAX: usize = 16 << 20;
/// The batch size advertised, and the most a batch read answers with.
const BATCH_MAX: usize = 4 << 20;
/// The most bytes one of a ByteStream read's messages carries.
const READ_PIECE: usize = 1 << 20;
/// The most directories a page of GetTree holds when its client names no size.
const TREE_PAGE: usize = 10_000;

// ------------------------------------------------------------------------------------------- gRPC

/// gRPC's status codes, those this service ends calls with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Ok = 0,
    InvalidArgument = 3,
    NotFound = 5,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    DataLoss = 15,
    Unauthenticated = 16,
}

/// How a call ended, when not well: its code and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Status {
    pub code: Code,
    pub message: String,
}

impl Status {
    pub fn new(code: Code, message: impl Into<String>) -> Status {
        Status { code, message: message.into() }
    }
}

fn invalid(why: impl std::fmt::Display) -> Status {
    Status::new(Code::InvalidArgument, why.to_string())
}

fn internal(e: anyhow::Error) -> Status {
    Status::new(Code::Internal, format!("{e:#}"))
}

/// A unary call's answer: its one message, or how it failed.
type Answer = Result<Vec<u8>, Status>;

/// `grpc-message`'s encoding: printable ASCII as it is, but `%`, which with every other byte is
/// percent-encoded.
fn grpc_message(m: &str) -> String {
    let mut out = String::with_capacity(m.len());
    for b in m.bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn put_status(h: &mut HeaderMap, code: Code, message: &str) {
    h.insert("grpc-status", HeaderValue::from(code as u32));
    if !message.is_empty()
        && let Ok(v) = HeaderValue::from_str(&grpc_message(message))
    {
        h.insert("grpc-message", v);
    }
}

fn grpc_response(body: srv::Body) -> Response {
    srv::response(StatusCode::OK, "application/grpc", body)
}

/// An answer with nothing to say but how the call ended: the status in its headers.
pub fn trailers_only(s: Status) -> Response {
    let mut r = grpc_response(srv::empty());
    put_status(r.headers_mut(), s.code, &s.message);
    r
}

/// A message as gRPC sends it: uncompressed (a 0), its length (4 bytes, big-endian), itself.
pub fn frame(msg: &[u8]) -> Bytes {
    let mut b = BytesMut::with_capacity(msg.len().saturating_add(5));
    b.extend_from_slice(&[0]);
    b.extend_from_slice(&u32::try_from(msg.len()).unwrap_or(u32::MAX).to_be_bytes());
    b.extend_from_slice(msg);
    b.freeze()
}

/// The first whole message in `buf`, taken off it; `None` until one has all come.
pub fn take_message(buf: &mut BytesMut) -> Result<Option<Bytes>, Status> {
    let Some((&flag, rest)) = buf.split_first() else { return Ok(None) };
    let Some(len) = rest.first_chunk::<4>().map(|l| u32::from_be_bytes(*l)) else { return Ok(None) };
    if flag != 0 {
        return Err(Status::new(Code::Unimplemented, "a compressed message: this server takes them uncompressed (it offers no grpc-encoding)"));
    }
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    if len > MESSAGE_MAX {
        return Err(Status::new(Code::ResourceExhausted, format!("a message of {len} bytes, over the {MESSAGE_MAX} taken")));
    }
    if buf.len() < len.saturating_add(5) {
        return Ok(None);
    }
    buf.advance(5);
    Ok(Some(buf.split_to(len).freeze()))
}

/// Where a client-streaming call's messages come from: a request's body ([`Messages`]), or a test's
/// list.
trait Source: Send {
    fn message(&mut self) -> impl std::future::Future<Output = Result<Option<Bytes>, Status>> + Send;
}

/// The messages of a request's body, as they come.
struct Messages {
    body: Incoming,
    buf: BytesMut,
    ended: bool,
}

impl Source for Messages {
    fn message(&mut self) -> impl std::future::Future<Output = Result<Option<Bytes>, Status>> + Send {
        self.next()
    }
}

impl Messages {
    fn new(body: Incoming) -> Messages {
        Messages { body, buf: BytesMut::new(), ended: false }
    }

    async fn next(&mut self) -> Result<Option<Bytes>, Status> {
        loop {
            if let Some(m) = take_message(&mut self.buf)? {
                return Ok(Some(m));
            }
            if self.ended {
                return if self.buf.is_empty() { Ok(None) } else { Err(invalid("the request ends inside a message")) };
            }
            match srv::next_chunk(&mut self.body).await {
                Ok(Some(chunk)) => self.buf.extend_from_slice(&chunk),
                Ok(None) => self.ended = true,
                Err(e) => return Err(Status::new(Code::Internal, format!("{e:#}"))),
            }
        }
    }

    /// A unary call's one message.
    async fn one(mut self) -> Result<Bytes, Status> {
        let m = self.next().await?.ok_or_else(|| invalid("the request holds no message"))?;
        match self.next().await? {
            None => Ok(m),
            Some(_) => Err(invalid("a unary call's request holds one message, not more")),
        }
    }
}

/// A call's messages, then its status in the trailers.
fn streamed(rx: async_channel::Receiver<Frame<Bytes>>) -> Response {
    use futures::StreamExt as _;
    grpc_response(http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(rx.map(Ok::<_, std::io::Error>))))
}

/// A unary call's answer: its message and `OK`, or its failure alone.
fn answer(a: Answer) -> Response {
    match a {
        Ok(msg) => {
            let mut trailers = HeaderMap::new();
            put_status(&mut trailers, Code::Ok, "");
            let frames = [Ok::<_, std::io::Error>(Frame::data(frame(&msg))), Ok(Frame::trailers(trailers))];
            grpc_response(http_body_util::StreamBody::new(futures::stream::iter(frames)).boxed())
        }
        Err(s) => trailers_only(s),
    }
}

fn is_grpc(h: &HeaderMap) -> bool {
    h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t == "application/grpc" || t.starts_with("application/grpc+proto") || t.starts_with("application/grpc;"))
}

// ------------------------------------------------------------------------------------ who's asking

/// Whose cache a call is (W24-R21): a repository's, in its leg's trust domain, read and written as
/// the leg's cache mode lets it (W08-R14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// `owner/repo`.
    pub repo: String,
    /// `trusted` or `untrusted`.
    pub domain: &'static str,
    pub read: bool,
    pub write: bool,
    pub run: String,
    pub leg: String,
}

impl Caller {
    /// A leg's, from its token's claims and its repository's name. Its own cache scope is
    /// `untrusted/…` when its run is untrusted (W18-R07).
    pub fn of_leg(claims: &Claims, repo: &str) -> Caller {
        use kiln_ci::store::cache::Mode;
        let mode = Mode::parse(&claims.cm).unwrap_or(Mode::None);
        let untrusted = claims.ac.first().is_some_and(|a| a.scope.starts_with("untrusted/"));
        Caller { repo: repo.to_owned(), domain: crate::ci::reuse::domain(!untrusted), read: mode.restores(), write: mode.saves(), run: claims.run.clone(), leg: claims.sub.clone() }
    }

    /// The ledger's scope it writes action results to.
    pub fn scope(&self) -> String {
        super::scope(&self.repo, self.domain)
    }

    /// Its trust domain, whose reads go down only (W24-R21).
    fn reader(&self) -> Result<Domain, Status> {
        Domain::parse(self.domain).map_err(|e| Status::new(Code::Internal, format!("the leg's domain: {e}")))
    }
}

/// An action as the ledger keys it (W24-R30): a unit of kind `action` whose definition is the
/// action's digest, which covers its command, its inputs and its platform as the client sends
/// them, in `domain`.
pub fn action_unit(action: &Digest, domain: &str) -> Unit {
    Unit {
        kind: Kind::Action,
        definition: format!("reapi {}/{}", action.hash, action.size),
        secrets: Default::default(),
        env: Default::default(),
        platform: Platform::default(),
        inputs: Default::default(),
        upstream: Default::default(),
        trust: domain.to_owned(),
    }
}

/// What a call works on: kiln's hub (its content store, and the store that says which blobs each
/// repository has put) and the ledger action results are kept in.
#[derive(Clone)]
pub struct Cache {
    pub hub: Arc<Hub>,
    pub ledger: Ledger,
}

/// `POST /<service>/<method>`, a gRPC call (open: the leg's token is its credential).
pub async fn route(hub: &Arc<Hub>, service: &str, method: &str, req: Request) -> Result<Response, Fail> {
    if req.method() != http::Method::POST || !is_grpc(req.headers()) {
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("/{service}/{method} is a gRPC method of the Remote Execution API: POST, with content-type application/grpc")));
    }
    let Some(claims) = token::of_request(hub, req.headers()) else {
        return Ok(trailers_only(Status::new(Code::Unauthenticated, "a leg's token is needed (authorization: Bearer $ACTIONS_RUNTIME_TOKEN)")));
    };
    let id = claims.repo.clone();
    let repo = hub.store.call(move |c| Ok(c.query_row("SELECT name FROM repos WHERE id = ?1", [&id], |r| r.get::<_, String>(0)).optional()?)).await;
    let repo = match repo {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(trailers_only(Status::new(Code::PermissionDenied, "the token's repository is gone"))),
        Err(e) => return Ok(trailers_only(internal(e))),
    };
    let cache = Cache { hub: Arc::clone(hub), ledger: Ledger::Table(hub.store.clone()) };
    Ok(serve(&cache, &Caller::of_leg(&claims, &repo), service, method, req).await)
}

/// A call, for `caller`: its answer, however it went.
pub async fn serve(cache: &Cache, caller: &Caller, service: &str, method: &str, req: Request) -> Response {
    let messages = Messages::new(req.into_body());
    match (service, method) {
        (BYTESTREAM, "Read") => match messages.one().await {
            Ok(m) => read_stream(cache, caller, &m).await.unwrap_or_else(trailers_only),
            Err(s) => trailers_only(s),
        },
        (BYTESTREAM, "Write") => answer(write_stream(cache, caller, messages).await),
        _ => match messages.one().await {
            Ok(m) => answer(unary(cache, caller, service, method, &m).await),
            Err(s) => trailers_only(s),
        },
    }
}

async fn unary(cache: &Cache, caller: &Caller, service: &str, method: &str, body: &[u8]) -> Answer {
    let m = match (service, method) {
        (CAPABILITIES, "GetCapabilities") => Method::GetCapabilities,
        (CAS, "FindMissingBlobs") => Method::FindMissingBlobs,
        (CAS, "BatchUpdateBlobs") => Method::BatchUpdateBlobs,
        (CAS, "BatchReadBlobs") => Method::BatchReadBlobs,
        (CAS, "GetTree") => Method::GetTree,
        (AC, "GetActionResult") => Method::GetActionResult,
        (AC, "UpdateActionResult") => Method::UpdateActionResult,
        (BYTESTREAM, "QueryWriteStatus") => return query_write(cache, caller, body).await,
        (EXECUTION, _) => {
            return Err(Status::new(Code::Unimplemented, "kiln doesn't execute actions yet, only caches them (docs/ci.md W24-R26): use it as --remote_cache, not --remote_executor"));
        }
        _ => return Err(Status::new(Code::Unimplemented, format!("no method {service}/{method}"))),
    };
    let ask = pb::Ask::decode(m, body).map_err(invalid)?;
    if !matches!(ask.digest_function, 0 | pb::SHA256) {
        return Err(invalid(format!("digest function {}: kiln's store keeps content by SHA-256 alone", ask.digest_function)));
    }
    match m {
        Method::GetCapabilities => Ok(pb::capabilities(caller.write, i64::try_from(BATCH_MAX).unwrap_or(i64::MAX))),
        Method::FindMissingBlobs => {
            if !caller.read && !caller.write {
                return Err(Status::new(Code::PermissionDenied, "this leg's cache mode is `none`: it neither reads nor writes the cache"));
            }
            find_missing(cache, caller, &ask.digests).await.map(|m| pb::missing(&m))
        }
        Method::BatchUpdateBlobs => batch_update(cache, caller, ask.blobs).await,
        Method::BatchReadBlobs => batch_read(cache, caller, &ask.digests).await,
        Method::GetTree => get_tree(cache, caller, ask.digest.as_ref(), ask.page_size, &ask.page_token).await,
        Method::GetActionResult => get_action_result(cache, caller, ask.digest.as_ref()).await,
        Method::UpdateActionResult => update_action_result(cache, caller, ask.digest.as_ref(), ask.action_result).await,
    }
}

// ------------------------------------------------------------------------------------------- blobs

/// Which of `digests` `repo` holds: put through this API by it, and whole in kiln's store now
/// (its index has it at that size). Each one held is touched, as `cas::object` touches what it
/// finds, so the store's collection leaves it another hour. The empty blob is always held.
async fn held(hub: &Hub, repo: &str, digests: &[Digest]) -> anyhow::Result<HashSet<Digest>> {
    let (repo, digests, now) = (repo.to_owned(), digests.to_vec(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let mut mine = c.prepare_cached("SELECT EXISTS(SELECT 1 FROM reapi_blobs WHERE repo = ?1 AND digest = ?2)")?;
            let mut kept = c.prepare_cached("SELECT size FROM cas_index WHERE digest = ?1")?;
            let mut touch = c.prepare_cached("UPDATE cas_index SET last_used_at = ?2 WHERE digest = ?1")?;
            let mut out = HashSet::new();
            for d in digests {
                if d.is_empty_blob() {
                    out.insert(d);
                    continue;
                }
                let has: bool = mine.query_row(params![repo, d.hash], |r| r.get(0))?;
                let size: Option<i64> = if has { kept.query_row([&d.hash], |r| r.get(0)).optional()? } else { None };
                if size == Some(d.size) {
                    touch.execute(params![d.hash, now])?;
                    out.insert(d);
                }
            }
            Ok(out)
        })
        .await
}

/// Records that `repo` has put `digests`.
async fn mine(hub: &Hub, repo: &str, digests: Vec<Digest>) -> anyhow::Result<()> {
    let (repo, now) = (repo.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            for d in digests {
                tx.execute(
                    "INSERT INTO reapi_blobs (repo, digest, size, at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (repo, digest) DO UPDATE SET size = excluded.size, at = excluded.at",
                    params![repo, d.hash, d.size, now],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// A file in the store's staging folder, gone with this.
struct Staged(std::path::PathBuf);

impl Staged {
    fn new(hub: &Hub) -> Staged {
        Staged(cas::staging(hub).join(format!("reapi-{}", crate::util::new_id())))
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Keeps the file at `staged`, whose content the caller checked is `d`'s, in kiln's store for
/// `repo`.
async fn keep(hub: &Hub, repo: &str, d: &Digest, staged: &Staged) -> anyhow::Result<()> {
    let obj = cas::put_file(hub, &staged.0).await?;
    anyhow::ensure!(obj.digest == d.hash && obj.size == d.bytes(), "the store kept {} ({} bytes) for {}", obj.digest, obj.size, d.hash);
    mine(hub, repo, vec![d.clone()]).await
}

/// Of `digests`, those `caller`'s repository doesn't hold, each once, in the order asked.
async fn find_missing(cache: &Cache, caller: &Caller, digests: &[Digest]) -> Result<Vec<Digest>, Status> {
    for d in digests {
        d.check().map_err(invalid)?;
    }
    let held = held(&cache.hub, &caller.repo, digests).await.map_err(internal)?;
    let mut said = HashSet::new();
    Ok(digests.iter().filter(|d| !held.contains(*d) && said.insert(*d)).cloned().collect())
}

async fn batch_update(cache: &Cache, caller: &Caller, blobs: Vec<(Digest, Vec<u8>, u64)>) -> Answer {
    if !caller.write {
        return Err(Status::new(Code::PermissionDenied, "this leg's cache mode doesn't write the cache"));
    }
    let mut answers = Vec::with_capacity(blobs.len());
    for (d, data, compressor) in blobs {
        let (code, msg) = match put(cache, caller, &d, data, compressor).await {
            Ok(()) => (Code::Ok, String::new()),
            Err(s) => (s.code, s.message),
        };
        answers.push((d, code as u32, msg));
    }
    Ok(pb::updated(&answers))
}

/// Keeps `data` as blob `d`, its digest checked first.
async fn put(cache: &Cache, caller: &Caller, d: &Digest, data: Vec<u8>, compressor: u64) -> Result<(), Status> {
    d.check().map_err(invalid)?;
    if compressor != 0 {
        return Err(invalid("a compressed blob: this server offers no compressors"));
    }
    let got = Digest::of(&data);
    if got != *d {
        return Err(invalid(format!("the data's digest is {}/{}, not {}/{}", got.hash, got.size, d.hash, d.size)));
    }
    if held(&cache.hub, &caller.repo, std::slice::from_ref(d)).await.map_err(internal)?.contains(d) {
        return Ok(());
    }
    let staged = Staged::new(&cache.hub);
    let path = staged.0.clone();
    crate::rt::blocking(move || {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, data)
    })
    .await
    .and_then(|r| r.map_err(anyhow::Error::from))
    .map_err(internal)?;
    keep(&cache.hub, &caller.repo, d, &staged).await.map_err(internal)
}

/// A blob's bytes from kiln's store, whole, checked against its digest.
async fn read_all(hub: &Hub, d: &Digest) -> anyhow::Result<Vec<u8>> {
    if d.is_empty_blob() {
        return Ok(Vec::new());
    }
    let obj = cas::Object { digest: d.hash.clone(), size: d.bytes() };
    let rx = cas::read(hub, &obj, 0, obj.size);
    let mut out = Vec::with_capacity(usize::try_from(obj.size).unwrap_or(0).min(MESSAGE_MAX));
    while let Ok(piece) = rx.recv().await {
        out.extend_from_slice(&piece);
    }
    let got = Digest::of(&out);
    anyhow::ensure!(got == *d, "kiln's store gave {} bytes for {}, not its content", got.size, d.hash);
    Ok(out)
}

async fn batch_read(cache: &Cache, caller: &Caller, digests: &[Digest]) -> Answer {
    if !caller.read {
        return Err(Status::new(Code::PermissionDenied, "this leg's cache mode doesn't read the cache"));
    }
    let mut total = 0u64;
    for d in digests {
        d.check().map_err(invalid)?;
        total = total.saturating_add(d.bytes());
    }
    if total > BATCH_MAX as u64 {
        return Err(invalid(format!("{total} bytes asked of one batch, over the {BATCH_MAX} it takes: read the larger blobs with ByteStream")));
    }
    let held = held(&cache.hub, &caller.repo, digests).await.map_err(internal)?;
    let mut answers = Vec::with_capacity(digests.len());
    for d in digests {
        if !held.contains(d) {
            answers.push((d.clone(), Vec::new(), Code::NotFound as u32, format!("{} isn't in the store", d.hash)));
            continue;
        }
        match read_all(&cache.hub, d).await {
            Ok(data) => answers.push((d.clone(), data, Code::Ok as u32, String::new())),
            Err(e) => answers.push((d.clone(), Vec::new(), Code::DataLoss as u32, format!("{e:#}"))),
        }
    }
    Ok(pb::read(&answers))
}

/// GetTree: the directories under `root`, breadth first, each once, in pages; a page's token is
/// how many directories came before it.
async fn get_tree(cache: &Cache, caller: &Caller, root: Option<&Digest>, page_size: i64, token: &str) -> Answer {
    if !caller.read {
        return Err(Status::new(Code::PermissionDenied, "this leg's cache mode doesn't read the cache"));
    }
    let root = root.ok_or_else(|| invalid("no root_digest"))?;
    root.check().map_err(invalid)?;
    let skip: usize = if token.is_empty() { 0 } else { token.parse().map_err(|_| invalid(format!("page_token `{token}` isn't one this server gave")))? };
    let page = usize::try_from(page_size).ok().filter(|n| *n > 0).unwrap_or(TREE_PAGE).min(TREE_PAGE);
    let mut queue = VecDeque::from([root.clone()]);
    let mut seen: HashSet<Digest> = HashSet::from([root.clone()]);
    let (mut out, mut bytes, mut n) = (Vec::new(), 0usize, 0usize);
    while let Some(d) = queue.pop_front() {
        if !held(&cache.hub, &caller.repo, std::slice::from_ref(&d)).await.map_err(internal)?.contains(&d) {
            return Err(Status::new(Code::NotFound, format!("directory {}/{} isn't in the store", d.hash, d.size)));
        }
        let data = read_all(&cache.hub, &d).await.map_err(|e| Status::new(Code::DataLoss, format!("{e:#}")))?;
        let refs = pb::DirectoryRefs::decode(&data).map_err(|e| invalid(format!("directory {}: {e}", d.hash)))?;
        for child in refs.directories {
            child.check().map_err(invalid)?;
            if seen.insert(child.clone()) {
                queue.push_back(child);
            }
        }
        if n >= skip {
            if out.len() >= page || (!out.is_empty() && bytes.saturating_add(data.len()) > BATCH_MAX) {
                return Ok(pb::tree(&out, &n.to_string()));
            }
            bytes = bytes.saturating_add(data.len());
            out.push(data);
        }
        n = n.saturating_add(1);
    }
    Ok(pb::tree(&out, ""))
}

/// Where a ByteStream resource names its blob: `[<instance>/]blobs/<hash>/<size>` to read, and
/// `[<instance>/]uploads/<uuid>/blobs/<hash>/<size>[/<anything>]` to write.
pub fn blob_of(resource: &str, upload: bool) -> Result<Digest, Status> {
    let segs: Vec<&str> = resource.split('/').collect();
    let at = segs.iter().position(|s| *s == "blobs" || *s == "compressed-blobs").ok_or_else(|| invalid(format!("`{resource}` names no blob")))?;
    if segs.get(at) == Some(&"compressed-blobs") {
        return Err(invalid("compressed blobs: this server offers no compressors"));
    }
    let uploads = at.checked_sub(2).and_then(|i| segs.get(i)) == Some(&"uploads");
    if upload != uploads {
        return Err(invalid(if upload { format!("`{resource}` isn't an upload's (`uploads/<uuid>/blobs/<hash>/<size>`)") } else { format!("`{resource}` is an upload's, not a blob to read") }));
    }
    let (Some(hash), Some(size)) = (segs.get(at.saturating_add(1)), segs.get(at.saturating_add(2))) else { return Err(invalid(format!("`{resource}` names no blob"))) };
    if !upload && segs.len() != at.saturating_add(3) {
        return Err(invalid(format!("`{resource}` names no blob")));
    }
    let d = Digest { hash: (*hash).to_owned(), size: size.parse().map_err(|_| invalid(format!("`{resource}`: `{size}` isn't a size")))? };
    d.check().map_err(invalid)?;
    Ok(d)
}

/// ByteStream's Read: the blob from `read_offset`, at most `read_limit` bytes (none: to its end),
/// a megabyte a message.
async fn read_stream(cache: &Cache, caller: &Caller, body: &[u8]) -> Result<Response, Status> {
    if !caller.read {
        return Err(Status::new(Code::PermissionDenied, "this leg's cache mode doesn't read the cache"));
    }
    let r = pb::Stream::read_request(body).map_err(invalid)?;
    let d = blob_of(&r.resource, false)?;
    let (offset, limit) = (u64::try_from(r.offset).map_err(|_| invalid("a read_offset below 0"))?, u64::try_from(r.limit).map_err(|_| invalid("a read_limit below 0"))?);
    if offset > d.bytes() {
        return Err(Status::new(Code::OutOfRange, format!("read_offset {offset} is past the blob's {} bytes", d.size)));
    }
    if !held(&cache.hub, &caller.repo, std::slice::from_ref(&d)).await.map_err(internal)?.contains(&d) {
        return Err(Status::new(Code::NotFound, format!("{} isn't in the store", d.hash)));
    }
    let end = if limit == 0 { d.bytes() } else { offset.saturating_add(limit).min(d.bytes()) };
    let (tx, rx) = async_channel::bounded::<Frame<Bytes>>(4);
    let hub = Arc::clone(&cache.hub);
    crate::rt::spawn(async move {
        let mut sent = 0u64;
        if end > offset {
            let pieces = cas::read(&hub, &cas::Object { digest: d.hash.clone(), size: d.bytes() }, offset, end);
            while let Ok(mut piece) = pieces.recv().await {
                sent = sent.saturating_add(piece.len() as u64);
                while !piece.is_empty() {
                    let part = piece.split_to(piece.len().min(READ_PIECE));
                    if tx.send(Frame::data(frame(&pb::read_response(&part)))).await.is_err() {
                        return;
                    }
                }
            }
        }
        let mut trailers = HeaderMap::new();
        if sent == end.saturating_sub(offset) {
            put_status(&mut trailers, Code::Ok, "");
        } else {
            put_status(&mut trailers, Code::DataLoss, &format!("kiln's store gave {sent} of {} bytes", end.saturating_sub(offset)));
        }
        let _ = tx.send(Frame::trailers(trailers)).await;
    });
    Ok(streamed(rx))
}

/// ByteStream's Write: a blob in pieces, each at the offset the last left, until `finish_write`;
/// checked against its digest, then kept. A blob the repository holds already is answered at once,
/// its whole size committed, as the API has it for an upload another finished. Uploads don't
/// resume: one cut short starts again from 0, as QueryWriteStatus says.
async fn write_stream(cache: &Cache, caller: &Caller, mut messages: impl Source) -> Answer {
    if !caller.write {
        return Err(Status::new(Code::PermissionDenied, "this leg's cache mode doesn't write the cache"));
    }
    let first = messages.message().await?.ok_or_else(|| invalid("no WriteRequest"))?;
    let w = pb::Stream::write_request(&first).map_err(invalid)?;
    let d = blob_of(&w.resource, true)?;
    if w.offset != 0 {
        return Err(invalid(format!("write_offset {}: an upload starts at 0 here (QueryWriteStatus says 0 is committed)", w.offset)));
    }
    if held(&cache.hub, &caller.repo, std::slice::from_ref(&d)).await.map_err(internal)?.contains(&d) {
        return Ok(pb::write_response(d.size));
    }
    let staged = Staged::new(&cache.hub);
    let max = cache.hub.cfg.ci.cache.max_entry_gb.saturating_mul(1 << 30).max(1 << 30);
    if d.bytes() > max {
        return Err(Status::new(Code::ResourceExhausted, format!("a blob of {} bytes, over the {max} kept (`ci.cache.max_entry_gb`)", d.size)));
    }
    let (sink, done) = crate::rt::file_sink(staged.0.clone(), d.bytes()).map_err(internal)?;
    let (mut hash, mut got, mut finished) = (sha2::Sha256::new(), 0u64, false);
    let resource = w.resource.clone();
    let mut next = Some(w);
    loop {
        let w = match next.take() {
            Some(w) => w,
            None => match messages.message().await? {
                Some(m) => pb::Stream::write_request(&m).map_err(invalid)?,
                None => break,
            },
        };
        if finished {
            return Err(invalid("a WriteRequest after finish_write"));
        }
        if !w.resource.is_empty() && w.resource != resource {
            return Err(invalid(format!("one upload names two resources: `{resource}` and `{}`", w.resource)));
        }
        if u64::try_from(w.offset).ok() != Some(got) {
            return Err(invalid(format!("write_offset {} where {got} bytes have come", w.offset)));
        }
        got = got.saturating_add(w.data.len() as u64);
        if got > d.bytes() {
            return Err(invalid(format!("more than the {} bytes its digest says", d.size)));
        }
        hash.update(&w.data);
        if !w.data.is_empty() && sink.send(w.data).await.is_err() {
            break;
        }
        finished = w.finish;
    }
    drop(sink);
    let written = done.await.map_err(internal)?;
    if !finished {
        return Err(invalid("the upload ended before finish_write"));
    }
    if got != d.bytes() || written != got || hex::encode(hash.finalize()) != d.hash {
        return Err(invalid(format!("the bytes sent ({got}) aren't {}/{}", d.hash, d.size)));
    }
    keep(&cache.hub, &caller.repo, &d, &staged).await.map_err(internal)?;
    Ok(pb::write_response(d.size))
}

/// ByteStream's QueryWriteStatus: an upload is all there or not begun (uploads don't resume).
async fn query_write(cache: &Cache, caller: &Caller, body: &[u8]) -> Answer {
    let q = pb::Stream::query_request(body).map_err(invalid)?;
    let d = blob_of(&q.resource, true)?;
    let done = held(&cache.hub, &caller.repo, std::slice::from_ref(&d)).await.map_err(internal)?.contains(&d);
    Ok(if done { pb::write_status(d.size, true) } else { pb::write_status(0, false) })
}

// ---------------------------------------------------------------------------------- action results

/// The first blob `result` names that `repo` doesn't hold: an output file, an output directory's
/// tree or any file in it, its stdout or stderr.
async fn first_missing(hub: &Hub, repo: &str, result: &pb::ActionResult) -> anyhow::Result<Option<Digest>> {
    let mut named = result.blobs();
    let trees: Vec<Digest> = result.directories.iter().map(|(_, t)| t.clone()).collect();
    let held_now = held(hub, repo, &named).await?;
    if let Some(gone) = named.iter().find(|d| !held_now.contains(*d)) {
        return Ok(Some(gone.clone()));
    }
    named.clear();
    for t in &trees {
        named.extend(pb::tree_files(&read_all(hub, t).await?)?);
    }
    named.sort();
    named.dedup();
    let held_now = held(hub, repo, &named).await?;
    Ok(named.into_iter().find(|d| !held_now.contains(d)))
}

async fn get_action_result(cache: &Cache, caller: &Caller, action: Option<&Digest>) -> Answer {
    let action = action.ok_or_else(|| invalid("no action_digest"))?;
    action.check().map_err(invalid)?;
    if !caller.read {
        return Err(Status::new(Code::NotFound, "this leg's cache mode doesn't read the cache: no result is looked up"));
    }
    let (repo, reader, unit) = (caller.repo.clone(), caller.reader()?, action_unit(action, caller.domain));
    let found = cache.ledger.run(move |rows| super::lookup_as(rows, None, &repo, &reader, &unit, &|p: &PathSet| p.clone())).await.map_err(internal)?;
    let miss = || Status::new(Code::NotFound, format!("no result for action {}", action.hash));
    let Some(hit) = found.hit else { return Err(miss()) };
    let Some(bytes) = hit.recorded.action_result.as_deref().and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()) else { return Err(miss()) };
    let result = pb::ActionResult::decode(&bytes).map_err(|e| internal(e.into()))?;
    match first_missing(&cache.hub, &caller.repo, &result).await {
        Ok(None) => Ok(bytes),
        Ok(Some(gone)) => {
            tracing::debug!(domain = %hit.domain, action = %action.hash, blob = %gone.hash, "an action's result names a blob the store no longer holds: a miss");
            Err(miss())
        }
        Err(e) => {
            tracing::debug!(domain = %hit.domain, action = %action.hash, error = %format!("{e:#}"), "an action's result couldn't be checked: a miss");
            Err(miss())
        }
    }
}

async fn update_action_result(cache: &Cache, caller: &Caller, action: Option<&Digest>, sent: Option<Vec<u8>>) -> Answer {
    let action = action.ok_or_else(|| invalid("no action_digest"))?;
    action.check().map_err(invalid)?;
    if !caller.write {
        return Err(Status::new(Code::PermissionDenied, "this leg's cache mode doesn't write the cache"));
    }
    let bytes = sent.ok_or_else(|| invalid("no action_result"))?;
    let result = pb::ActionResult::decode(&bytes).map_err(invalid)?;
    if result.exit_code != 0 {
        return Err(Status::new(Code::FailedPrecondition, format!("only successes are kept (docs/ci.md W24-R07); this one exited {}", result.exit_code)));
    }
    for d in result.blobs() {
        d.check().map_err(invalid)?;
    }
    if let Some(gone) = first_missing(&cache.hub, &caller.repo, &result).await.map_err(internal)? {
        return Err(Status::new(Code::FailedPrecondition, format!("it names {}/{}, which this repository hasn't put in the store: upload its outputs first", gone.hash, gone.size)));
    }
    let now = crate::util::now_unix();
    let sha = |d: &Digest| format!("sha256:{}", d.hash);
    let recorded = Recorded {
        outcome: "success".into(),
        exit: Some(0),
        outputs: result.files.iter().map(|(p, d, _)| (p.clone(), sha(d))).chain(result.directories.iter().map(|(p, t)| (p.clone(), sha(t)))).collect(),
        logs: result.stdout.as_ref().map(|d| d.hash.clone()),
        provenance: Provenance { repo: caller.repo.clone(), run: caller.run.clone(), leg: caller.leg.clone(), finished_at: now, ..Provenance::default() },
        domain: caller.domain.to_owned(),
        action_result: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
        ..Recorded::default()
    };
    let (scope, unit) = (caller.scope(), action_unit(action, caller.domain));
    match cache.ledger.run(move |rows| super::write(rows, None, &scope, &unit, &PathSet::new(), &recorded)).await.map_err(internal)? {
        Written::New(_) | Written::Reproduced { .. } => Ok(bytes),
        Written::Conflict { verdict, .. } => Err(Status::new(
            Code::FailedPrecondition,
            format!("the action cache holds another result for action {}, which stands: entries are never overwritten (docs/ci.md W24-R22); it's judged again: {}", action.hash, verdict.why()),
        )),
        Written::Refused(why) => Err(Status::new(Code::FailedPrecondition, why)),
    }
}

#[cfg(test)]
mod tests;
