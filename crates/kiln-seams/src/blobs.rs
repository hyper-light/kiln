//! `Blobs`: bytes, with S3's semantics (CP-T10.1): put; get, with byte ranges; head, list and
//! delete; conditional writes (`If-None-Match: *`, `If-Match`); multipart; checksums; presigned
//! URLs. Its real implementations are kiln's CAS, any S3, and Mantle (CP-T10.1 grows the suite past
//! 150 cases); [`SimBlobs`] keeps objects in memory, its presigned URLs signed and timed by its
//! clock. An object's etag is its content's SHA-256, however it was uploaded.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::clock::{Clock, SimClock};
use crate::conformance::{Case, Failure, ensure, fine, refused, same};
use crate::sha256_hex;

/// An object as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub key: String,
    pub size: u64,
    /// Its content's SHA-256, hex: what `If-Match` compares.
    pub etag: String,
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
    /// The content's SHA-256 (hex), checked before anything is stored.
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
    pub objects: Vec<Meta>,
    /// More keys follow the last one.
    pub truncated: bool,
}

/// What a presigned URL lets its holder do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
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
    /// A URL that lets its holder get or put `key` until `expires` from now (at most 7 days).
    fn presign<'a>(&'a self, key: &'a str, method: Method, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>>;
}

/// Longest key, in bytes (S3's).
pub const KEY_MAX: usize = 1024;
/// Most keys a listing gives (S3's).
pub const LIST_MAX: usize = 1000;
/// Part numbers run 1 to this (S3's).
pub const PARTS_MAX: u32 = 10_000;
/// The smallest a part other than the last may be (S3's).
pub const PART_MIN: usize = 5 << 20;
/// The longest a presigned URL lasts (S3's).
pub const PRESIGN_MAX: Duration = Duration::from_secs(7 * 24 * 3600);

fn check_key(key: &str) -> Result<(), Refusal> {
    if key.is_empty() || key.len() > KEY_MAX || key.contains('\0') {
        return Err(Refusal::Invalid { what: "a key", why: format!("1 to {KEY_MAX} bytes of UTF-8, no NUL ({} bytes)", key.len()) });
    }
    Ok(())
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

#[derive(Default)]
struct SimState {
    objects: BTreeMap<String, (Meta, Arc<Vec<u8>>)>,
    bytes: u64,
    uploads: HashMap<String, (String, BTreeMap<u32, (String, Vec<u8>)>)>,
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

    fn sign(&self, method: Method, key: &str, expires_ms: i64) -> String {
        sha256_hex(format!("{}\n{method:?}\n{key}\n{expires_ms}", self.secret).as_bytes())
    }

    fn store(&self, st: &mut SimState, key: &str, bytes: Vec<u8>, cond: &Cond) -> Result<Meta, Refusal> {
        let size = len64(bytes.len());
        if size > self.limits.object {
            return Err(Refusal::TooLarge { what: "an object", size, limit: self.limits.object });
        }
        let old = st.objects.get(key).map(|(m, _)| m.clone());
        match (cond, &old) {
            (Cond::IfNoneMatch, Some(_)) => return Err(Refusal::PreconditionFailed(key.to_owned())),
            (Cond::IfMatch(want), Some(m)) if *want != m.etag => return Err(Refusal::PreconditionFailed(key.to_owned())),
            (Cond::IfMatch(_), None) => return Err(Refusal::PreconditionFailed(key.to_owned())),
            _ => {}
        }
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

    /// Uses a presigned URL as its holder would: a get gives the bytes, a put stores `body`.
    pub fn redeem(&self, url: &str, body: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, Refusal> {
        let rest = url.strip_prefix("sim://blobs/").ok_or_else(|| Refusal::Forbidden("not one of this store's URLs".into()))?;
        let (key_hex, query) = rest.split_once('?').ok_or_else(|| Refusal::Forbidden("no signature".into()))?;
        let field = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(name).and_then(|v| v.strip_prefix('='))).map(str::to_owned);
        let key = hex::decode(key_hex).ok().and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| Refusal::Forbidden("a bad key".into()))?;
        let method = match field("method").as_deref() {
            Some("GET") => Method::Get,
            Some("PUT") => Method::Put,
            _ => return Err(Refusal::Forbidden("no method".into())),
        };
        let expires_ms: i64 = field("expires").and_then(|e| e.parse().ok()).ok_or_else(|| Refusal::Forbidden("no expiry".into()))?;
        if field("sig").as_deref() != Some(self.sign(method, &key, expires_ms).as_str()) {
            return Err(Refusal::Forbidden("the signature doesn't match".into()));
        }
        if self.clock.unix_ms() >= expires_ms {
            return Err(Refusal::Expired);
        }
        let mut st = self.inner.lock();
        match method {
            Method::Get => st.objects.get(&key).map(|(_, b)| Some(b.as_ref().clone())).ok_or(Refusal::NotFound(key)),
            Method::Put => self.store(&mut st, &key, body.unwrap_or_default(), &Cond::Always).map(|_| None),
        }
    }
}

impl Blobs for SimBlobs {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        let out = (|| {
            check_key(key)?;
            if let Some(want) = &put.sha256 {
                let got = sha256_hex(&bytes);
                if !got.eq_ignore_ascii_case(want) {
                    return Err(Refusal::BadChecksum { want: want.clone(), got });
                }
            }
            self.store(&mut self.inner.lock(), key, bytes, &put.cond)
        })();
        Box::pin(async move { out })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        let out = (|| {
            check_key(key)?;
            let (meta, bytes) = self.inner.lock().objects.get(key).cloned().ok_or_else(|| Refusal::NotFound(key.to_owned()))?;
            let Some(r) = range else { return Ok((meta, bytes.as_ref().clone())) };
            let size = meta.size;
            let end = r.end.unwrap_or(size.saturating_sub(1)).min(size.saturating_sub(1));
            if size == 0 || r.start >= size || r.end.is_some_and(|e| e < r.start) {
                return Err(Refusal::InvalidRange { size });
            }
            let (s, e) = (usize::try_from(r.start).unwrap_or(usize::MAX), usize::try_from(end).unwrap_or(usize::MAX));
            let slice = bytes.get(s..=e).map(<[u8]>::to_vec).ok_or(Refusal::InvalidRange { size })?;
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
            if limit == 0 {
                return Err(Refusal::Invalid { what: "a listing's limit", why: "it's 0".into() });
            }
            let limit = limit.min(LIST_MAX);
            let st = self.inner.lock();
            let start = match after {
                Some(a) if a >= prefix => std::ops::Bound::Excluded(a.to_owned()),
                _ => std::ops::Bound::Included(prefix.to_owned()),
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
                objects.push(m.clone());
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
            if part == 0 || part > PARTS_MAX {
                return Err(Refusal::Invalid { what: "a part number", why: format!("{part} (1 to {PARTS_MAX})") });
            }
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
            if parts.is_empty() {
                return Err(Refusal::BadParts("no parts named".into()));
            }
            if !parts.windows(2).all(|w| matches!(w, [a, b] if a.0 < b.0)) {
                return Err(Refusal::BadParts("parts must be named in ascending order".into()));
            }
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

    fn presign<'a>(&'a self, key: &'a str, method: Method, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        let out = (|| {
            check_key(key)?;
            if expires > PRESIGN_MAX || expires.is_zero() {
                return Err(Refusal::Invalid { what: "a presigned URL's lifetime", why: format!("{expires:?} (more than 0, at most 7 days)") });
            }
            let expires_ms = self.clock.unix_ms().saturating_add(i64::try_from(expires.as_millis()).unwrap_or(i64::MAX));
            let m = match method {
                Method::Get => "GET",
                Method::Put => "PUT",
            };
            Ok(format!("sim://blobs/{}?method={m}&expires={expires_ms}&sig={}", hex::encode(key.as_bytes()), self.sign(method, key, expires_ms)))
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
    /// Lets `d` pass by the store's clock.
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()>;
    /// Uses a presigned URL as a client would (a get's bytes; a put stores `body`).
    fn redeem<'a>(&'a self, url: &'a str, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>>;
}

/// The fixture for a [`SimBlobs`], parts as small as 5 bytes.
pub struct SimBlobsFixture {
    pub blobs: SimBlobs,
    pub clock: SimClock,
}

impl Default for SimBlobsFixture {
    fn default() -> Self {
        let clock = SimClock::new();
        Self { blobs: SimBlobs::new(Arc::new(clock.clone()), SimLimits { part_min: 5, ..SimLimits::default() }), clock }
    }
}

impl BlobsFixture for SimBlobsFixture {
    fn blobs(&self) -> &dyn Blobs {
        &self.blobs
    }
    fn part_min(&self) -> usize {
        5
    }
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
        self.clock.advance(d);
        Box::pin(async {})
    }
    fn redeem<'a>(&'a self, url: &'a str, body: Option<Vec<u8>>) -> BoxFuture<'a, Result<Option<Vec<u8>>, Refusal>> {
        let out = self.blobs.redeem(url, body);
        Box::pin(async move { out })
    }
}

async fn put_plain(b: &dyn Blobs, key: &str, bytes: &[u8]) -> Result<Meta, Failure> {
    fine("put", b.put(key, bytes.to_vec(), Put::default()).await)
}

fn c1<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let meta = put_plain(b, "c/a.txt", b"hello").await?;
        same("its size and etag", (meta.size, meta.etag.clone()), (5, sha256_hex(b"hello")))?;
        let (m, bytes) = fine("get", b.get("c/a.txt", None).await)?;
        same("what's read back", (m, bytes), (meta, b"hello".to_vec()))
    })
}

fn c2<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        put_plain(b, "r", b"0123456789").await?;
        let part = |start: u64, end: Option<u64>| b.get("r", Some(Range { start, end }));
        same("bytes 2-4", fine("get", part(2, Some(4)).await)?.1, b"234".to_vec())?;
        same("bytes 7-", fine("get", part(7, None).await)?.1, b"789".to_vec())?;
        same("bytes 8-100", fine("get", part(8, Some(100)).await)?.1, b"89".to_vec())?;
        same("bytes 10-", part(10, None).await.map(|x| x.1), Err(Refusal::InvalidRange { size: 10 }))?;
        same("bytes 5-3", part(5, Some(3)).await.map(|x| x.1), Err(Refusal::InvalidRange { size: 10 }))
    })
}

fn c3<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let meta = put_plain(b, "h", b"abc").await?;
        same("head", fine("head", b.head("h").await)?, meta)?;
        same("head of nothing", b.head("missing").await, Err(Refusal::NotFound("missing".into())))?;
        same("get of nothing", b.get("missing", None).await.map(|x| x.1), Err(Refusal::NotFound("missing".into())))
    })
}

fn c4<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let first = put_plain(b, "o", b"first").await?;
        let second = put_plain(b, "o", b"second").await?;
        ensure(first.etag != second.etag, || "new content kept its etag".into())?;
        same("after the overwrite", fine("get", b.get("o", None).await)?.1, b"second".to_vec())
    })
}

fn c5<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let only_new = Put { cond: Cond::IfNoneMatch, sha256: None };
        fine("a create", b.put("n", b"one".to_vec(), only_new.clone()).await)?;
        same("a second create", b.put("n", b"two".to_vec(), only_new).await.map(|m| m.size), Err(Refusal::PreconditionFailed("n".into())))?;
        same("the object", fine("get", b.get("n", None).await)?.1, b"one".to_vec())
    })
}

fn c6<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let first = put_plain(b, "m", b"one").await?;
        let if_match = |etag: &str| Put { cond: Cond::IfMatch(etag.to_owned()), sha256: None };
        fine("a replace at its etag", b.put("m", b"two".to_vec(), if_match(&first.etag)).await)?;
        same("a replace at a stale etag", b.put("m", b"three".to_vec(), if_match(&first.etag)).await.map(|m| m.size), Err(Refusal::PreconditionFailed("m".into())))?;
        same("a replace of nothing", b.put("none", b"x".to_vec(), if_match(&first.etag)).await.map(|m| m.size), Err(Refusal::PreconditionFailed("none".into())))?;
        same("the object", fine("get", b.get("m", None).await)?.1, b"two".to_vec())
    })
}

fn c7<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        put_plain(b, "d", b"x").await?;
        fine("delete", b.delete("d").await)?;
        same("after the delete", b.head("d").await, Err(Refusal::NotFound("d".into())))?;
        fine("a delete of nothing", b.delete("d").await)
    })
}

fn c8<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        for k in ["p/b", "p/a", "q/a", "p/c", "p/d"] {
            put_plain(b, k, b"v").await?;
        }
        let names = |l: &Listing| l.objects.iter().map(|m| m.key.clone()).collect::<Vec<_>>();
        let first = fine("list", b.list("p/", None, 3).await)?;
        same("the first page", (names(&first), first.truncated), (vec!["p/a".to_owned(), "p/b".to_owned(), "p/c".to_owned()], true))?;
        let rest = fine("list on", b.list("p/", Some("p/c"), 3).await)?;
        same("the rest", (names(&rest), rest.truncated), (vec!["p/d".to_owned()], false))?;
        ensure(matches!(b.list("", None, 0).await, Err(Refusal::Invalid { .. })), || "a listing of 0 wasn't refused Invalid".into())
    })
}

fn c9<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let right = Put { cond: Cond::Always, sha256: Some(sha256_hex(b"data")) };
        fine("a put with its checksum", b.put("s", b"data".to_vec(), right).await)?;
        let wrong = Put { cond: Cond::Always, sha256: Some(sha256_hex(b"other")) };
        let e = refused("a put with a wrong checksum", b.put("t", b"data".to_vec(), wrong).await)?;
        ensure(matches!(e, Refusal::BadChecksum { .. }), || format!("refused as {e:?}"))?;
        same("what the refused put stored", b.head("t").await, Err(Refusal::NotFound("t".into())))
    })
}

fn c10<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let min = f.part_min();
        let up = fine("start", b.start_upload("big").await)?;
        let (one, two, three) = (vec![b'a'; min], vec![b'b'; min], b"end".to_vec());
        // Uploaded out of order: joined in part order.
        let e3 = fine("part 3", b.upload_part(&up, 3, three.clone()).await)?;
        let e1 = fine("part 1", b.upload_part(&up, 1, one.clone()).await)?;
        let e2 = fine("part 2", b.upload_part(&up, 2, two.clone()).await)?;
        let meta = fine("complete", b.complete_upload(&up, vec![(1, e1), (2, e2), (3, e3)]).await)?;
        let whole: Vec<u8> = [one, two, three].concat();
        same("its size and etag", (meta.size, meta.etag), (len64(whole.len()), sha256_hex(&whole)))?;
        same("the object", fine("get", b.get("big", None).await)?.1, whole)
    })
}

fn c11<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let min = f.part_min();
        let up = fine("start", b.start_upload("x").await)?;
        let small = fine("a small first part", b.upload_part(&up, 1, b"tiny".to_vec()).await)?;
        let last = fine("the last part", b.upload_part(&up, 2, vec![b'z'; min]).await)?;
        ensure(matches!(b.complete_upload(&up, vec![(1, small.clone()), (2, last.clone())]).await, Err(Refusal::BadParts(_))), || "a part under the minimum, not last, was taken".into())?;
        ensure(matches!(b.complete_upload(&up, vec![(2, last), (1, small)]).await, Err(Refusal::BadParts(_))), || "parts named out of order were taken".into())?;
        ensure(matches!(b.upload_part(&up, 0, b"x".to_vec()).await, Err(Refusal::Invalid { .. })), || "part 0 was taken".into())?;
        ensure(matches!(b.upload_part(&up, PARTS_MAX.saturating_add(1), b"x".to_vec()).await, Err(Refusal::Invalid { .. })), || "a part past 10,000 was taken".into())?;
        same("a part for no upload", b.upload_part("no-such-upload", 1, b"x".to_vec()).await, Err(Refusal::NoSuchUpload("no-such-upload".into())))?;
        same("what the failed completes left", b.head("x").await, Err(Refusal::NotFound("x".into())))
    })
}

fn c12<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let up = fine("start", b.start_upload("gone").await)?;
        let e1 = fine("part 1", b.upload_part(&up, 1, b"abc".to_vec()).await)?;
        fine("abort", b.abort_upload(&up).await)?;
        same("completing it after", b.complete_upload(&up, vec![(1, e1)]).await.map(|m| m.size), Err(Refusal::NoSuchUpload(up.clone())))?;
        same("the object", b.head("gone").await, Err(Refusal::NotFound("gone".into())))
    })
}

fn c13<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        put_plain(b, "shared", b"for you").await?;
        let url = fine("presign", b.presign("shared", Method::Get, Duration::from_secs(60)).await)?;
        same("a get through it", f.redeem(&url, None).await, Ok(Some(b"for you".to_vec())))?;
        f.pass(Duration::from_secs(61)).await;
        same("a get once it has expired", f.redeem(&url, None).await, Err(Refusal::Expired))?;
        ensure(matches!(b.presign("shared", Method::Get, PRESIGN_MAX.saturating_add(Duration::from_secs(1))).await, Err(Refusal::Invalid { .. })), || "a URL for more than 7 days was given".into())
    })
}

fn c14<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        let url = fine("presign", b.presign("dropped", Method::Put, Duration::from_secs(60)).await)?;
        same("a put through it", f.redeem(&url, Some(b"left here".to_vec())).await, Ok(None))?;
        same("the object", fine("get", b.get("dropped", None).await)?.1, b"left here".to_vec())?;
        // Its last character changed (a presigned URL ends in its signature, S3's as this one's).
        let mut tampered = url.clone();
        let last = tampered.pop();
        tampered.push(if last == Some('0') { '1' } else { '0' });
        ensure(f.redeem(&tampered, Some(b"x".to_vec())).await.is_err(), || "a tampered URL was taken".into())
    })
}

fn c15<F: BlobsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.blobs();
        ensure(matches!(b.put("", b"x".to_vec(), Put::default()).await, Err(Refusal::Invalid { .. })), || "an empty key was taken".into())?;
        ensure(matches!(b.put(&"k".repeat(KEY_MAX.saturating_add(1)), b"x".to_vec(), Put::default()).await, Err(Refusal::Invalid { .. })), || "a key over 1024 bytes was taken".into())?;
        put_plain(b, &"k".repeat(KEY_MAX), b"x").await.map(|_| ())
    })
}

/// The `Blobs` contract, clause by clause (docs/seams.md, "Blobs").
pub fn cases<F: BlobsFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "BLOBS-1", says: "an object put is got back whole, with its size and its content's SHA-256 as its etag", check: c1::<F> },
        Case { id: "BLOBS-2", says: "a get with a byte range gives those bytes (an end past the object, to its end); a range outside it is refused InvalidRange", check: c2::<F> },
        Case { id: "BLOBS-3", says: "head gives an object's metadata; head or get of a key with no object is refused NotFound", check: c3::<F> },
        Case { id: "BLOBS-4", says: "a put over an object replaces it, and new content has a new etag", check: c4::<F> },
        Case { id: "BLOBS-5", says: "a put If-None-Match creates only where there's no object; else it's refused PreconditionFailed and stores nothing", check: c5::<F> },
        Case { id: "BLOBS-6", says: "a put If-Match replaces only the object with that etag; else it's refused PreconditionFailed", check: c6::<F> },
        Case { id: "BLOBS-7", says: "a deleted object is gone; deleting a key with no object succeeds", check: c7::<F> },
        Case { id: "BLOBS-8", says: "a listing gives a prefix's objects in key order, after its cursor, at most its limit, saying when it's truncated; a limit of 0 is refused", check: c8::<F> },
        Case { id: "BLOBS-9", says: "a put with a SHA-256 its content doesn't have is refused BadChecksum and stores nothing", check: c9::<F> },
        Case { id: "BLOBS-10", says: "a multipart upload's parts, uploaded in any order, are joined in part order into one object", check: c10::<F> },
        Case { id: "BLOBS-11", says: "a multipart upload is refused when a part but the last is under the minimum or parts are named out of order; part numbers run 1 to 10,000", check: c11::<F> },
        Case { id: "BLOBS-12", says: "an aborted upload is gone: completing it is refused NoSuchUpload", check: c12::<F> },
        Case { id: "BLOBS-13", says: "a presigned get works until it expires by the store's clock, then is refused Expired; none lasts more than 7 days", check: c13::<F> },
        Case { id: "BLOBS-14", says: "a presigned put stores what its holder sends; a URL changed after signing is refused", check: c14::<F> },
        Case { id: "BLOBS-15", says: "keys are 1 to 1024 bytes: an empty or longer one is refused Invalid", check: c15::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2: the simulator and its suite.
    #[test]
    fn the_simulated_store_passes_the_blobs_suite() -> Result<(), String> {
        futures::executor::block_on(run("Blobs", &cases::<SimBlobsFixture>(), || async { SimBlobsFixture::default() })).into_result().map(|_| ())
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
}
