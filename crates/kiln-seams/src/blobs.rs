//! `Blobs`: bytes, with S3's semantics (CP-T10.1): put; get, with byte ranges; head, list and
//! delete; conditional writes (`If-None-Match: *`, `If-Match`); multipart; checksums; presigned
//! URLs. Its real implementations are kiln's: the local store (kiln's content-addressed store, its
//! data where it was), any S3, and Mantle once it's vendored (CP-T10.2); [`SimBlobs`] keeps objects
//! in memory, its presigned URLs signed and timed by its clock. An object's etag is its content's
//! SHA-256, however it was uploaded.
//!
//! What every implementation takes is checked by the functions here ([`check_key`],
//! [`check_sha256`], [`byte_range`], [`check_parts`], [`lifetime_secs`] and the rest), so a key,
//! a range or a lifetime one store takes, every store takes. The keys are narrower than S3's: they
//! hold no control characters and no backslash, and their `/`-separated segments are 1 to 255
//! bytes, none `.` or `..`, so they're the same keys on every S3 (MinIO keeps a key's segments as
//! file names) and on every platform.

mod suite;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::clock::{Clock, SimClock};
use crate::sha256_hex;

pub use suite::{cases, noise};

/// An object as stored: what a put answers, and head and get say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub key: String,
    pub size: u64,
    /// Its content's SHA-256, 64 lowercase hex digits: what `If-Match` compares.
    pub etag: String,
    /// When the store last wrote it: milliseconds since the Unix epoch, by the store's clock and to
    /// its resolution (an S3's is a whole second). Head, get and list say the same; a put's answer
    /// says when the store acknowledged it, as near as the store tells.
    pub modified_ms: i64,
}

/// An object as a listing names it. A listing says no etag: an S3's listing names objects by its
/// own ETags (MD5s, or digests of parts), which aren't the content's SHA-256; head says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub size: u64,
    /// As head says it.
    pub modified_ms: i64,
}

/// When a put may go ahead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Cond {
    #[default]
    Always,
    /// Only when no object is at the key (`If-None-Match: *`).
    IfNoneMatch,
    /// Only when the object at the key has this etag (`If-Match`).
    IfMatch(String),
}

/// How to put.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Put {
    pub cond: Cond,
    /// The content's SHA-256 (hex, either case), checked before anything is stored.
    pub sha256: Option<String>,
}

/// Bytes `start` to `end` inclusive (to the last without `end`), as HTTP's `Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    pub end: Option<u64>,
}

/// A page of a listing, in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub objects: Vec<Listed>,
    /// More keys follow the last one.
    pub truncated: bool,
}

/// How a presigned URL is used: the HTTP method its holder sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
}

/// What a presigned URL lets its holder do: get the object, or put one of exactly `length` bytes
/// (a body longer or shorter is refused). The signature covers the method, the exact key, the
/// expiry and, for a put, the length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Get,
    Put { length: u64 },
}

impl Access {
    /// The method its URL is used with.
    pub fn method(self) -> Method {
        match self {
            Self::Get => Method::Get,
            Self::Put { .. } => Method::Put,
        }
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no object at {0:?}")]
    NotFound(String),
    #[error("the condition didn't hold for {0:?}")]
    PreconditionFailed(String),
    #[error("the range isn't within the object's {size} bytes")]
    InvalidRange { size: u64 },
    #[error("{what} is {size}, over its limit of {limit}")]
    TooLarge { what: &'static str, size: u64, limit: u64 },
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("the content's SHA-256 is {got}, not {want}")]
    BadChecksum { want: String, got: String },
    #[error("no upload {0:?}")]
    NoSuchUpload(String),
    #[error("the upload's parts aren't right: {0}")]
    BadParts(String),
    #[error("the store holds its most ({what}: {limit})")]
    Full { what: &'static str, limit: u64 },
    #[error("the presigned URL has expired")]
    Expired,
    #[error("the presigned URL isn't valid here: {0}")]
    Forbidden(String),
    #[error("the store is unavailable: {0}")]
    Unavailable(String),
}

/// Bytes, with S3's semantics.
pub trait Blobs: Send + Sync + 'static {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>>;
    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>>;
    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>>;
    /// Deletes the object at `key`; one that isn't there is deleted already (S3's way).
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Objects whose keys start with `prefix`, after `after`, at most `limit`, in key order.
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>>;
    /// Starts a multipart upload to `key`; its id.
    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>>;
    /// Uploads part `part` (1 to 10,000; again, it replaces the one before); the part's etag.
    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>>;
    /// Joins the parts named (ascending, each with its etag) into the object.
    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>>;
    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    /// A URL that lets its holder get `key`, or put it with a body of the length `access` names,
    /// until `expires` from now (rounded up to a whole second, at most an hour).
    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>>;
}

/// A store shared: what it answers, an `Arc` of it answers.
impl<B: Blobs + ?Sized> Blobs for Arc<B> {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        self.as_ref().put(key, bytes, put)
    }
    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        self.as_ref().get(key, range)
    }
    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        self.as_ref().head(key)
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.as_ref().delete(key)
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        self.as_ref().list(prefix, after, limit)
    }
    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        self.as_ref().start_upload(key)
    }
    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        self.as_ref().upload_part(upload, part, bytes)
    }
    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        self.as_ref().complete_upload(upload, parts)
    }
    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.as_ref().abort_upload(upload)
    }
    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        self.as_ref().presign(key, access, expires)
    }
}

/// Longest key, in bytes: S3's 1024, less 64 a store may put before a key (an S3 store's prefix
/// in its bucket), so a key one store takes, every store takes.
pub const KEY_MAX: usize = 960;
/// Longest segment of a key between `/`s, in bytes: a file name's 255, where a store keeps keys as
/// paths (MinIO does), less the one an S3 store marks a key's end with.
pub const SEGMENT_MAX: usize = 254;
/// Most keys a listing gives (S3's).
pub const LIST_MAX: usize = 1000;
/// Part numbers run 1 to this (S3's).
pub const PARTS_MAX: u32 = 10_000;
/// The smallest a part other than the last may be (S3's).
pub const PART_MIN: usize = 5 << 20;
/// The longest a presigned URL lasts: an hour, for every store (S3 allows 7 days; a URL is a
/// credential, and kiln keeps its lifetime short).
pub const PRESIGN_MAX: Duration = Duration::from_secs(3600);

/// Whether a store takes `key`: 1 to [`KEY_MAX`] bytes of UTF-8 with no control character and no
/// backslash, its `/`-separated segments each 1 to [`SEGMENT_MAX`] bytes, none `.` or `..` (so no
/// leading, trailing or doubled `/`) and none ending in a space (an S3 store marks a key's end with
/// one: `p ` and `p /x` would be a file and a folder of one name where keys are kept as paths).
pub fn check_key(key: &str) -> Result<(), Refusal> {
    let bad = |why: String| Err(Refusal::Invalid { what: "a key", why });
    if key.is_empty() || key.len() > KEY_MAX {
        return bad(format!("it's {} bytes: a key is 1 to {KEY_MAX}", key.len()));
    }
    if let Some(c) = key.chars().find(|c| c.is_control() || *c == '\\') {
        return bad(format!("it holds {c:?}: a key holds no control characters and no backslash"));
    }
    for segment in key.split('/') {
        if segment.is_empty() {
            return bad("it has an empty segment: no leading, trailing or doubled '/'".into());
        }
        if segment.len() > SEGMENT_MAX {
            return bad(format!("a segment is {} bytes: each is 1 to {SEGMENT_MAX}", segment.len()));
        }
        if segment == "." || segment == ".." {
            return bad(format!("it has a {segment:?} segment"));
        }
        if segment.ends_with(' ') {
            return bad("a segment ends with a space".into());
        }
    }
    Ok(())
}

/// Whether `sha` is a SHA-256 as a put names its content's: 64 hex digits, either case.
pub fn check_sha256(sha: &str) -> Result<(), Refusal> {
    if sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Refusal::Invalid { what: "a SHA-256", why: format!("{sha:?} isn't 64 hex digits") })
    }
}

/// Checks a put's checksum against its content: `Invalid` for one that isn't a SHA-256,
/// `BadChecksum` for one the content doesn't have.
pub fn check_content(put: &Put, bytes: &[u8]) -> Result<(), Refusal> {
    let Some(want) = &put.sha256 else { return Ok(()) };
    check_sha256(want)?;
    let got = sha256_hex(bytes);
    if got.eq_ignore_ascii_case(want) { Ok(()) } else { Err(Refusal::BadChecksum { want: want.clone(), got }) }
}

/// Whether a put's condition holds against what's at its key (`None`: nothing).
pub fn holds(cond: &Cond, key: &str, current: Option<&str>) -> Result<(), Refusal> {
    match (cond, current) {
        (Cond::Always, _) | (Cond::IfNoneMatch, None) => Ok(()),
        (Cond::IfMatch(want), Some(etag)) if want == etag => Ok(()),
        _ => Err(Refusal::PreconditionFailed(key.to_owned())),
    }
}

/// The bytes `range` names of an object of `size`, first and last, inclusive: an end past the
/// object is its last byte; an empty object, a start at or past the end, or an end before the
/// start is `InvalidRange`.
pub fn byte_range(range: Range, size: u64) -> Result<(u64, u64), Refusal> {
    let last = size.checked_sub(1).ok_or(Refusal::InvalidRange { size })?;
    if range.start > last || range.end.is_some_and(|e| e < range.start) {
        return Err(Refusal::InvalidRange { size });
    }
    Ok((range.start, range.end.map_or(last, |e| e.min(last))))
}

/// A listing's limit as a store keeps it: 0 is refused, more than [`LIST_MAX`] is [`LIST_MAX`].
pub fn list_limit(limit: usize) -> Result<usize, Refusal> {
    if limit == 0 {
        return Err(Refusal::Invalid { what: "a listing's limit", why: "it's 0".into() });
    }
    Ok(limit.min(LIST_MAX))
}

/// Where a listing starts: past `after` when that's at or past `prefix`, else at `prefix`.
pub fn list_from<'a>(prefix: &'a str, after: Option<&'a str>) -> std::ops::Bound<&'a str> {
    match after {
        Some(a) if a >= prefix => std::ops::Bound::Excluded(a),
        _ => std::ops::Bound::Included(prefix),
    }
}

/// Whether `part` is a part number (1 to [`PARTS_MAX`]).
pub fn check_part(part: u32) -> Result<(), Refusal> {
    if part == 0 || part > PARTS_MAX {
        return Err(Refusal::Invalid { what: "a part number", why: format!("{part} (1 to {PARTS_MAX})") });
    }
    Ok(())
}

/// The parts a completion names: at least one, ascending with none twice, each a part number.
pub fn check_parts(parts: &[(u32, String)]) -> Result<(), Refusal> {
    if parts.is_empty() {
        return Err(Refusal::BadParts("no parts named".into()));
    }
    if !parts.windows(2).all(|w| matches!(w, [a, b] if a.0 < b.0)) {
        return Err(Refusal::BadParts("parts must be named in ascending order, each once".into()));
    }
    if let Some((n, _)) = parts.iter().find(|(n, _)| *n == 0 || *n > PARTS_MAX) {
        return Err(Refusal::BadParts(format!("part {n} isn't a part number (1 to {PARTS_MAX})")));
    }
    Ok(())
}

/// A presigned URL's lifetime in whole seconds, rounded up: more than 0, at most [`PRESIGN_MAX`].
pub fn lifetime_secs(expires: Duration) -> Result<u64, Refusal> {
    let whole = expires.as_secs().saturating_add(u64::from(expires.subsec_nanos() > 0));
    if whole == 0 || whole > PRESIGN_MAX.as_secs() {
        return Err(Refusal::Invalid { what: "a presigned URL's lifetime", why: format!("{expires:?} (more than 0, at most an hour)") });
    }
    Ok(whole)
}

/// What a presigned URL's signature covers, as one string: its method, its exact key, its expiry
/// (Unix milliseconds) and, for a put, the length of the body it takes.
pub fn signed(access: Access, key: &str, expires_ms: i64) -> String {
    match access {
        Access::Get => format!("GET\n{key}\n{expires_ms}"),
        Access::Put { length } => format!("PUT\n{key}\n{expires_ms}\n{length}"),
    }
}

/// A key as a URL's path carries it: RFC 3986's unreserved characters and `/` as they are, every
/// other byte `%XX` (S3's encoding of an object's key).
pub fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A key back from [`encode_key`]'s form (`None`: not one).
pub fn decode_key(path: &str) -> Option<String> {
    let mut out = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            out.push(u8::try_from(hi.checked_mul(16)?.checked_add(lo)?).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------------- simulator

/// What a [`SimBlobs`] holds: the largest object, the bytes in all (objects and the parts of
/// uploads under way), the most objects, the most uploads under way, and the smallest part but
/// the last.
#[derive(Debug, Clone, Copy)]
pub struct SimLimits {
    pub object: u64,
    pub total: u64,
    pub objects: usize,
    pub uploads: usize,
    pub part_min: usize,
}

impl Default for SimLimits {
    fn default() -> Self {
        Self { object: 64 << 20, total: 1 << 30, objects: 1_000_000, uploads: 1000, part_min: PART_MIN }
    }
}

/// Objects in memory. Clones share them.
#[derive(Clone)]
pub struct SimBlobs {
    inner: Arc<Mutex<SimState>>,
    clock: Arc<dyn Clock>,
    limits: SimLimits,
    /// What presigned URLs are signed with.
    secret: String,
}

/// An upload under way: its key, and its parts by number (etag, bytes).
type SimUpload = (String, BTreeMap<u32, (String, Vec<u8>)>);

#[derive(Default)]
struct SimState {
    objects: BTreeMap<String, (Meta, Arc<Vec<u8>>)>,
    bytes: u64,
    uploads: HashMap<String, SimUpload>,
    next: u64,
}

impl Default for SimBlobs {
    fn default() -> Self {
        Self::new(Arc::new(SimClock::new()), SimLimits::default())
    }
}

impl SimBlobs {
    pub fn new(clock: Arc<dyn Clock>, limits: SimLimits) -> Self {
        let secret = sha256_hex(format!("sim-blobs {}", clock.unix_ms()).as_bytes());
        Self { inner: Arc::default(), clock, limits, secret }
    }

    fn sign(&self, access: Access, key: &str, expires_ms: i64) -> String {
        sha256_hex(format!("{}\n{}", self.secret, signed(access, key, expires_ms)).as_bytes())
    }

    fn store(&self, st: &mut SimState, key: &str, bytes: Vec<u8>, cond: &Cond) -> Result<Meta, Refusal> {
        let size = len64(bytes.len());
        if size > self.limits.object {
            return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
        }
        let old = st.objects.get(key).map(|(m, _)| m.clone());
        holds(cond, key, old.as_ref().map(|m| m.etag.as_str()))?;
        if old.is_none() && st.objects.len() >= self.limits.objects {
            return Err(Refusal::Full { what: "objects", limit: len64(self.limits.objects) });
        }
        let total = st.bytes.saturating_sub(old.as_ref().map_or(0, |m| m.size)).saturating_add(size);
        if total > self.limits.total {
            return Err(Refusal::Full { what: "bytes", limit: self.limits.total });
        }
        let meta = Meta { key: key.to_owned(), size, etag: sha256_hex(&bytes), modified_ms: self.clock.unix_ms() };
        st.objects.insert(key.to_owned(), (meta.clone(), Arc::new(bytes)));
        st.bytes = total;
        Ok(meta)
    }

    /// Uses a presigned URL as its holder would, with `method`: a get gives the bytes, a put
    /// stores `body`. A URL used with the other method, changed after signing, or a put's body of
    /// another length than its URL declares, is `Forbidden`.
    pub fn redeem(&self, url: &str, method: Method, body: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, Refusal> {
        let rest = url.strip_prefix("sim://blobs/").ok_or_else(|| Refusal::Forbidden("not one of this store's URLs".into()))?;
        let (path, query) = rest.split_once('?').ok_or_else(|| Refusal::Forbidden("no signature".into()))?;
        let field = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('='))).map(str::to_owned);
        let key = decode_key(path).ok_or_else(|| Refusal::Forbidden("a bad key".into()))?;
        let expires_ms: i64 = field("expires").and_then(|e| e.parse().ok()).ok_or_else(|| Refusal::Forbidden("no expiry".into()))?;
        let access = match method {
            Method::Get => Access::Get,
            Method::Put => Access::Put { length: field("length").and_then(|l| l.parse().ok()).ok_or_else(|| Refusal::Forbidden("no length".into()))? },
        };
        if field("sig").as_deref() != Some(self.sign(access, &key, expires_ms).as_str()) {
            return Err(Refusal::Forbidden("the signature doesn't match".into()));
        }
        if self.clock.unix_ms() >= expires_ms {
            return Err(Refusal::Expired);
        }
        let mut st = self.inner.lock();
        match access {
            Access::Get => st.objects.get(&key).map(|(_, b)| Some(b.as_ref().clone())).ok_or(Refusal::NotFound(key)),
            Access::Put { length } => {
                let body = body.unwrap_or_default();
                if len64(body.len()) != length {
                    return Err(Refusal::Forbidden(format!("the body is {} bytes; its URL takes {length}", body.len())));
                }
                self.store(&mut st, &key, body, &Cond::Always).map(|_| None)
            }
        }
    }
}

impl Blobs for SimBlobs {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        let out = (|| {
            check_key(key)?;
            check_content(&put, &bytes)?;
            self.store(&mut self.inner.lock(), key, bytes, &put.cond)
        })();
        Box::pin(async move { out })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        let out = (|| {
            check_key(key)?;
            let (meta, bytes) = self.inner.lock().objects.get(key).cloned().ok_or_else(|| Refusal::NotFound(key.to_owned()))?;
            let Some(r) = range else { return Ok((meta, bytes.as_ref().clone())) };
            let (first, last) = byte_range(r, meta.size)?;
            let (s, e) = (usize::try_from(first).unwrap_or(usize::MAX), usize::try_from(last).unwrap_or(usize::MAX));
            let slice = bytes.get(s..=e).map(<[u8]>::to_vec).ok_or(Refusal::InvalidRange { size: meta.size })?;
            Ok((meta, slice))
        })();
        Box::pin(async move { out })
    }

    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        let out = check_key(key).and_then(|()| self.inner.lock().objects.get(key).map(|(m, _)| m.clone()).ok_or_else(|| Refusal::NotFound(key.to_owned())));
        Box::pin(async move { out })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = check_key(key).map(|()| {
            let mut st = self.inner.lock();
            if let Some((m, _)) = st.objects.remove(key) {
                st.bytes = st.bytes.saturating_sub(m.size);
            }
        });
        Box::pin(async move { out })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        let out = (|| {
            let limit = list_limit(limit)?;
            let st = self.inner.lock();
            let start = match list_from(prefix, after) {
                std::ops::Bound::Excluded(a) => std::ops::Bound::Excluded(a.to_owned()),
                std::ops::Bound::Included(p) => std::ops::Bound::Included(p.to_owned()),
                std::ops::Bound::Unbounded => std::ops::Bound::Unbounded,
            };
            let mut objects = Vec::new();
            let mut truncated = false;
            for (k, (m, _)) in st.objects.range((start, std::ops::Bound::Unbounded)) {
                if !k.starts_with(prefix) {
                    break;
                }
                if objects.len() >= limit {
                    truncated = true;
                    break;
                }
                objects.push(Listed { key: m.key.clone(), size: m.size, modified_ms: m.modified_ms });
            }
            Ok(Listing { objects, truncated })
        })();
        Box::pin(async move { out })
    }

    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        let out = (|| {
            check_key(key)?;
            let mut st = self.inner.lock();
            if st.uploads.len() >= self.limits.uploads {
                return Err(Refusal::Full { what: "uploads", limit: len64(self.limits.uploads) });
            }
            st.next = st.next.saturating_add(1);
            let id = format!("upload-{}", st.next);
            st.uploads.insert(id.clone(), (key.to_owned(), BTreeMap::new()));
            Ok(id)
        })();
        Box::pin(async move { out })
    }

    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        let out = (|| {
            check_part(part)?;
            if len64(bytes.len()) > self.limits.object {
                return Err(Refusal::TooLarge { what: "a part", size: len64(bytes.len()), limit: self.limits.object });
            }
            let mut st = self.inner.lock();
            // A part's bytes count toward the store's until its upload completes or is aborted.
            let (_, parts) = st.uploads.get(upload).ok_or_else(|| Refusal::NoSuchUpload(upload.to_owned()))?;
            let replaced = parts.get(&part).map_or(0, |(_, b)| len64(b.len()));
            let total = st.bytes.saturating_sub(replaced).saturating_add(len64(bytes.len()));
            if total > self.limits.total {
                return Err(Refusal::Full { what: "bytes", limit: self.limits.total });
            }
            let etag = sha256_hex(&bytes);
            if let Some((_, parts)) = st.uploads.get_mut(upload) {
                parts.insert(part, (etag.clone(), bytes));
            }
            st.bytes = total;
            Ok(etag)
        })();
        Box::pin(async move { out })
    }

    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        let out = (|| {
            let mut st = self.inner.lock();
            let (key, have) = st.uploads.get(upload).cloned().ok_or_else(|| Refusal::NoSuchUpload(upload.to_owned()))?;
            check_parts(&parts)?;
            let mut bytes = Vec::new();
            let last = parts.len().saturating_sub(1);
            for (i, (n, etag)) in parts.iter().enumerate() {
                let (have_etag, data) = have.get(n).ok_or_else(|| Refusal::BadParts(format!("part {n} wasn't uploaded")))?;
                if have_etag != etag {
                    return Err(Refusal::BadParts(format!("part {n}'s etag doesn't match")));
                }
                if i < last && data.len() < self.limits.part_min {
                    return Err(Refusal::BadParts(format!("part {n} is {} bytes: every part but the last is at least {}", data.len(), self.limits.part_min)));
                }
                if len64(bytes.len()).saturating_add(len64(data.len())) > self.limits.object {
                    let size = parts.iter().filter_map(|(n, _)| have.get(n)).map(|(_, b)| len64(b.len())).fold(0, u64::saturating_add);
                    return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
                }
                bytes.extend_from_slice(data);
            }
            // Its parts' bytes become the object's.
            let held: u64 = have.values().map(|(_, b)| len64(b.len())).fold(0, u64::saturating_add);
            st.bytes = st.bytes.saturating_sub(held);
            match self.store(&mut st, &key, bytes, &Cond::Always) {
                Ok(meta) => {
                    st.uploads.remove(upload);
                    Ok(meta)
                }
                Err(e) => {
                    st.bytes = st.bytes.saturating_add(held);
                    Err(e)
                }
            }
        })();
        Box::pin(async move { out })
    }

    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = {
            let mut st = self.inner.lock();
            match st.uploads.remove(upload) {
                Some((_, parts)) => {
                    let held: u64 = parts.values().map(|(_, b)| len64(b.len())).fold(0, u64::saturating_add);
                    st.bytes = st.bytes.saturating_sub(held);
                    Ok(())
                }
                None => Err(Refusal::NoSuchUpload(upload.to_owned())),
            }
        };
        Box::pin(async move { out })
    }

    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        let out = (|| {
            check_key(key)?;
            let secs = lifetime_secs(expires)?;
            let expires_ms = self.clock.unix_ms().saturating_add(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX));
            let length = match access {
                Access::Get => String::new(),
                Access::Put { length } => format!("&length={length}"),
            };
            Ok(format!("sim://blobs/{}?expires={expires_ms}{length}&sig={}", encode_key(key), self.sign(access, key, expires_ms)))
        })();
        Box::pin(async move { out })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Blobs` conformance suite runs against: an empty store per case.
pub trait BlobsFixture: Send + Sync {
    fn blobs(&self) -> &dyn Blobs;
    /// The smallest a part other than the last may be, in this store.
    fn part_min(&self) -> usize;
    /// The most an object (or a part) may hold in this store, as the fixture made it: small
    /// enough that a case can pass it.
    fn object_max(&self) -> u64;
    /// How finely the store keeps when it wrote an object (1 ms; an S3's is 1 s).
    fn resolution(&self) -> Duration;
    /// Lets `d` pass by the store's clock.
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()>;
    /// Uses a presigned URL as a client would, with `method` (a get's bytes; a put stores `body`).
    fn redeem<'a>(&'a self, url: &'a str, method: Method, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>>;
}

/// The fixture for a [`SimBlobs`]: parts as small as 5 bytes, objects to 24 MiB.
pub struct SimBlobsFixture {
    pub blobs: SimBlobs,
    pub clock: SimClock,
}

/// The largest object a [`SimBlobsFixture`]'s store takes.
pub const SIM_FIXTURE_OBJECT: u64 = 24 << 20;

impl Default for SimBlobsFixture {
    fn default() -> Self {
        let clock = SimClock::new();
        Self { blobs: SimBlobs::new(Arc::new(clock.clone()), SimLimits { part_min: 5, object: SIM_FIXTURE_OBJECT, ..SimLimits::default() }), clock }
    }
}

impl BlobsFixture for SimBlobsFixture {
    fn blobs(&self) -> &dyn Blobs {
        &self.blobs
    }
    fn part_min(&self) -> usize {
        5
    }
    fn object_max(&self) -> u64 {
        SIM_FIXTURE_OBJECT
    }
    fn resolution(&self) -> Duration {
        Duration::from_millis(1)
    }
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
        self.clock.advance(d);
        Box::pin(async {})
    }
    fn redeem<'a>(&'a self, url: &'a str, method: Method, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>> {
        let out = self.blobs.redeem(url, method, body);
        Box::pin(async move { out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2; CP-T10.1, AC-T10.1.2: the simulator passes every clause.
    #[test]
    fn the_simulated_store_passes_the_blobs_suite() -> Result<(), String> {
        let passed = futures::executor::block_on(run("Blobs", &cases::<SimBlobsFixture>(), || async { SimBlobsFixture::default() })).into_result()?;
        assert!(passed >= 150, "{passed} cases");
        Ok(())
    }

    /// CP-M2, AC-M2.2: the simulator, bounded, refusing typed past its bounds.
    #[test]
    fn its_bytes_and_objects_are_bounded() {
        let b = SimBlobs::new(Arc::new(SimClock::new()), SimLimits { object: 4, total: 6, objects: 3, uploads: 1, part_min: 1 });
        let put = |k: &str, v: &[u8]| futures::executor::block_on(b.put(k, v.to_vec(), Put::default())).map(|m| m.size);
        assert_eq!(put("a", b"12345"), Err(Refusal::TooLarge { what: "an object", size: 5, limit: 4 }));
        assert_eq!(put("a", b"1234"), Ok(4));
        assert_eq!(put("b", b"123"), Err(Refusal::Full { what: "bytes", limit: 6 }));
        assert_eq!(put("a", b"12"), Ok(2), "replacing frees what it replaced");
        assert_eq!(put("b", b"1234"), Ok(4));
        let up = futures::executor::block_on(b.start_upload("u")).unwrap_or_default();
        assert!(matches!(futures::executor::block_on(b.start_upload("v")), Err(Refusal::Full { .. })));
        // Its parts count toward the bytes: 6 held by a and b already.
        assert!(matches!(futures::executor::block_on(b.upload_part(&up, 1, b"x".to_vec())), Err(Refusal::Full { what: "bytes", .. })));
        assert!(futures::executor::block_on(b.delete("b")).is_ok());
        assert!(futures::executor::block_on(b.upload_part(&up, 1, b"xy".to_vec())).is_ok());
        assert!(futures::executor::block_on(b.abort_upload(&up)).is_ok());
        // Empty objects count toward the most objects.
        assert_eq!(put("e1", b""), Ok(0));
        assert_eq!(put("e2", b""), Ok(0));
        assert!(matches!(futures::executor::block_on(b.put("e3", Vec::new(), Put::default())), Err(Refusal::Full { what: "objects", .. })));
    }

    /// CP-T10.1: what every store takes is checked alike; a key, range or lifetime one store
    /// takes, every store takes.
    #[test]
    fn keys_ranges_and_lifetimes_are_checked_alike() {
        for good in ["a", "a/b", "ü/日本/😀.txt", "q?x=1&y=2", "a.b/..c/...", "k".repeat(255).as_str()] {
            assert_eq!(check_key(good), Ok(()), "{good:?}");
        }
        for bad in ["", "/a", "a/", "a//b", ".", "a/../b", "a\\b", "a\0b", "tab\there", "del\u{7f}", "c1\u{85}", "k".repeat(256).as_str(), vec!["k".repeat(200); 5].join("/").as_str()] {
            assert!(matches!(check_key(bad), Err(Refusal::Invalid { .. })), "{bad:?}");
        }
        assert_eq!(byte_range(Range { start: 2, end: Some(100) }, 10), Ok((2, 9)));
        assert_eq!(byte_range(Range { start: 9, end: None }, 10), Ok((9, 9)));
        assert_eq!(byte_range(Range { start: 0, end: None }, 0), Err(Refusal::InvalidRange { size: 0 }));
        assert_eq!(byte_range(Range { start: 5, end: Some(4) }, 10), Err(Refusal::InvalidRange { size: 10 }));
        assert_eq!(lifetime_secs(Duration::from_millis(1500)), Ok(2));
        assert_eq!(lifetime_secs(PRESIGN_MAX), Ok(3600));
        assert!(lifetime_secs(PRESIGN_MAX.saturating_add(Duration::from_millis(1))).is_err());
        assert!(lifetime_secs(Duration::ZERO).is_err());
        assert_eq!(decode_key(&encode_key("a b+c%/ü?#")).as_deref(), Some("a b+c%/ü?#"));
        assert_eq!(encode_key("a b+c"), "a%20b%2Bc");
    }
}
