//! Custody of what kiln's server keeps in its store of bytes (docs/ci.md W27-R08; the Transfers
//! plan's step 12). The store has no identity of kiln's, so kiln's server signs for it, as it signs
//! for a person with no key on the device that asked:
//!
//! - a put: kiln's hand-on of the object to the store, by the SHA-256 of what it sent, and its
//!   receipt for the store, by the etag the store answered;
//! - a whole read (a get of all of it, or one reader's ranges from its first byte to its last, as
//!   `blobs::io::read_stream` asks them): kiln's take for the store, by the etag the store says it
//!   holds, following that receipt, and kiln's receipt of what it read, by its SHA-256.
//!
//! A digest that changes across a hop breaks the chain there, and kiln records the break; a whole
//! get whose bytes aren't what the store says is refused (`BadChecksum`), as `read_stream` ends a
//! stream of them short. Statements say no time, so reading an object again signs the same records,
//! which kiln keeps once.
//!
//! Objects whose key names their content (`cas/<digest>`, `sync/<hash>`) get no records: every
//! read checks them by that name already, a whole get here and a stream in `read_stream`. A store's
//! presigned URL that leads to the store itself (an S3's) carries bytes kiln never sees.
//!
//! A refusal here is custody's, above the store: `blobs::Metered`, inside this wrapper, counts the
//! store's answer (`kiln_blobs_requests_total{outcome="ok"}`), and the refusal is logged on
//! `kiln.custody`. `BadChecksum`, the seam's refusal of a put whose body isn't its checksum, is
//! used here for a get whose bytes aren't the store's etag.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{self, PAYLOAD_CUSTODY, Statement, Subject};
use kiln_seams::blobs::{Access, Blobs, Listing, Meta, Put, Range, Refusal};
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

use crate::store::Store;

/// Readers of whole objects followed at once, at the most: past it, the oldest is let go (a reader
/// gone part-way through leaves its place behind).
const READERS_MAX: usize = 64;

/// A reader whose next range hasn't come in this long is let go: a viewer gone, a range request
/// that never reaches the end. One let go (by this or [`READERS_MAX`]) gives no receipt: as far as
/// kiln saw, its read was never whole.
const READER_IDLE: Duration = Duration::from_secs(600);

/// The server's store of bytes, each object kiln puts in it and reads back whole recorded as it
/// changes hands ([`self`]).
pub struct Attested {
    inner: Arc<dyn Blobs>,
    store: Store,
    pki: Arc<crate::pki::Pki>,
    /// The store, as kiln's records name it: `kiln://<org>/blobs/<local|s3|mantle>`.
    uri: String,
    /// Uploads under way of objects kiln records, by their id: the key, the SHA-256 of their parts
    /// so far, and the part that comes next. A part out of order (sent again, say) lets the hash
    /// go: the etag stands for what was sent then.
    uploads: Mutex<HashMap<String, (String, Option<Sha256>, u32)>>,
    /// Readers of whole objects, a range at a time, oldest first.
    readers: Mutex<Vec<Reader>>,
    /// Keys being put now, and how many times: a read meanwhile follows the put's receipt, which
    /// is on its way.
    putting: Mutex<HashMap<String, usize>>,
}

/// What its records came over, as kiln keeps them.
const CAME_FROM: &str = "kiln's store of bytes";

/// A put under way ([`Attested::putting`]).
struct Putting<'a>(&'a Attested, String);

impl Drop for Putting<'_> {
    fn drop(&mut self) {
        let mut putting = self.0.putting.lock();
        if let Some(n) = putting.get_mut(&self.1) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                putting.remove(&self.1);
            }
        }
    }
}

/// A reader of a whole object, part-way: of `key` at `etag`, read through `at`, hashed so far, its
/// last range when.
struct Reader {
    key: String,
    etag: String,
    at: u64,
    sha: Sha256,
    touched: std::time::Instant,
}

/// Whether kiln records custody of the object at `key`: one whose key doesn't name its content.
fn recorded(key: &str) -> bool {
    !(key.starts_with("cas/") || key.starts_with("sync/"))
}

impl Attested {
    /// `inner`, the server's store (`kind`: `local`, `s3`), its objects recorded in `store` and
    /// signed for with the server's key in `pki`.
    pub fn new(inner: Arc<dyn Blobs>, kind: &str, store: Store, pki: Arc<crate::pki::Pki>) -> Self {
        let uri = format!("kiln://{}/blobs/{kind}", pki.organization());
        Self { inner, store, pki, uri, uploads: Mutex::default(), readers: Mutex::default(), putting: Mutex::default() }
    }

    /// A put of `key` under way, until what it gives is dropped (done, or given up part-way).
    fn putting(&self, key: &str) -> Putting<'_> {
        *self.putting.lock().entry(key.to_owned()).or_default() += 1;
        Putting(self, key.to_owned())
    }

    /// The thing an object is in kiln's records: `object <key>` by its SHA-256.
    fn object(key: &str, sha256: &str) -> Subject {
        Subject { name: format!("object {key}"), digest: std::collections::BTreeMap::from([("sha256".to_owned(), sha256.to_ascii_lowercase())]), ..Subject::default() }
    }

    /// kiln's hand-on of `key`'s object, by what it sent (`sent`), to the store.
    fn hand_on(&self, key: &str, sent: &str) -> Statement {
        let server = super::server_uri(&self.pki.organization());
        let body = Body { to: Some(self.uri.clone()), why: "kept in kiln's store of bytes".into(), ..Body::new(Kind::HandedOn, &server) };
        Statement { subject: vec![Self::object(key, sent)], ..Statement::new(to_value(&body)) }
    }

    /// kiln's receipt for the store of `key`'s object, by the etag it answered, following hand-on
    /// `hand_on`.
    fn kept_by_store(&self, key: &str, etag: &str, hand_on: &str) -> Statement {
        let server = super::server_uri(&self.pki.organization());
        let body = Body { from: Some(server.clone()), taken_for: Some(self.uri.clone()), why: "the store has no key of kiln's: kiln signs its receipt for it".into(), ..Body::new(Kind::Received, &server) };
        Statement { subject: vec![Self::object(key, etag)], follows: vec![hand_on.to_owned()], ..Statement::new(to_value(&body)) }
    }

    /// The digest of `s` as kiln's server signs it ([`super::server_signed`] names it in its body).
    fn digest(&self, s: &Statement) -> String {
        let mut s = s.clone();
        if let Some(o) = s.body.as_object_mut() {
            o.insert("by".into(), serde_json::Value::String(super::server_uri(&self.pki.organization())));
        }
        statement::payload(&s).map(|p| statement::digest(PAYLOAD_CUSTODY, &p)).unwrap_or_default()
    }

    /// Signs `s` as kiln's server and keeps it, as it came from the store's side of kiln.
    async fn keep(&self, s: &Statement) {
        let bundle = match super::server_signed(&self.pki, s) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a record of kiln's store of bytes couldn't be signed");
                return;
            }
        };
        match super::take(&self.store, &self.pki.ca_der, &self.pki.organization(), &bundle, CAME_FROM, crate::util::now_unix()).await {
            Ok(Ok(t)) if t.broke.is_some() => tracing::warn!(target: "kiln.custody", digest = %t.digest, broke = t.broke.as_deref().unwrap_or(""), "an object's chain broke in kiln's store of bytes"),
            Ok(Ok(_)) => {}
            Ok(Err(why)) => tracing::warn!(target: "kiln.custody", %why, "kiln refused its own record of its store of bytes"),
            Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a record of kiln's store of bytes couldn't be kept"),
        }
    }

    /// `key`'s object went into the store: kiln sent `sent`, the store answered `etag`.
    async fn put_kept(&self, key: &str, sent: &str, etag: &str) {
        let hand_on = self.hand_on(key, sent);
        let digest = self.digest(&hand_on);
        self.keep(&hand_on).await;
        self.keep(&self.kept_by_store(key, etag, &digest)).await;
    }

    /// `key`'s object read back whole: the store said `etag`, the bytes hashed to `got`.
    async fn read_whole(&self, key: &str, etag: &str, got: &str) {
        let server = super::server_uri(&self.pki.organization());
        // What the store holds since kiln's put of it: kiln's receipt for the store of the put that
        // left it as the store says (being kept now, if that put is under way). An object that came
        // into the store some other way begins its chain at this take: kept before kiln recorded
        // custody, taken in where it lay, put back by a rollback's reconcile, adopted by an S3
        // store, or copied by `kiln blobs copy`.
        let at_etag = self.digest(&self.kept_by_store(key, etag, &self.digest(&self.hand_on(key, etag))));
        let follows = if self.putting.lock().contains_key(key) {
            vec![at_etag]
        } else {
            match super::kept(&self.store, &at_etag).await {
                Ok(true) => vec![at_etag],
                Ok(false) => Vec::new(),
                Err(e) => {
                    tracing::warn!(target: "kiln.custody", %key, error = %format!("{e:#}"), "kiln couldn't tell whether it kept an object's put");
                    Vec::new()
                }
            }
        };
        let body = Body { to: Some(server.clone()), taken_for: Some(self.uri.clone()), why: "the store has no key of kiln's: kiln hands it on for it".into(), ..Body::new(Kind::Taken, &server) };
        let take = Statement { subject: vec![Self::object(key, etag)], follows, ..Statement::new(to_value(&body)) };
        let digest = self.digest(&take);
        self.keep(&take).await;
        let body = Body { from: Some(self.uri.clone()), why: "read back from kiln's store of bytes".into(), ..Body::new(Kind::Received, &server) };
        self.keep(&Statement { subject: vec![Self::object(key, got)], follows: vec![digest], ..Statement::new(to_value(&body)) }).await;
    }

    /// A whole read of `key` that isn't what the store says: the break, recorded where kiln
    /// records the object (its receipt of what it read breaks at the take), else (its key names
    /// its content, and it has no records) audited as refused.
    async fn read_broke(&self, key: &str, etag: &str, got: &str) {
        if recorded(key) {
            return self.read_whole(key, etag, got).await;
        }
        tracing::warn!(target: "kiln.custody", %key, %etag, %got, "an object read back from kiln's store of bytes isn't what the store says: refused");
        let detail = format!("kiln's store of bytes said sha256 {etag}; its bytes hashed to {got}");
        let entry = crate::audit::Entry { principal: "kiln".into(), action: "blobs.read".into(), target: Some(format!("object {key}")), result: "refused".into(), detail, ..crate::audit::Entry::default() };
        crate::audit::write(&self.store, entry).await;
    }

    /// Follows a reader's range `start..` of `key` (`bytes`, the object's `meta`): a whole read
    /// once one reader's ranges reach its last byte from its first, hashed as they came.
    async fn followed(&self, key: &str, start: u64, bytes: &[u8], meta: &Meta) {
        let done = {
            let mut readers = self.readers.lock();
            readers.retain(|r| r.touched.elapsed() < READER_IDLE);
            if start == 0 {
                if readers.len() >= READERS_MAX && !readers.is_empty() {
                    readers.remove(0);
                }
                readers.push(Reader { key: key.to_owned(), etag: meta.etag.clone(), at: 0, sha: Sha256::new(), touched: std::time::Instant::now() });
            }
            let mut finished = None;
            if let Some(i) = readers.iter().rposition(|r| r.key == key && r.etag == meta.etag && r.at == start)
                && let Some(r) = readers.get_mut(i)
            {
                r.sha.update(bytes);
                r.at = r.at.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                r.touched = std::time::Instant::now();
                if r.at >= meta.size {
                    finished = Some(i);
                }
            }
            finished.filter(|i| *i < readers.len()).map(|i| readers.remove(i))
        };
        if let Some(r) = done {
            let got = hex::encode(r.sha.finalize());
            if got == r.etag {
                self.read_whole(key, &r.etag, &got).await;
            } else {
                self.read_broke(key, &r.etag, &got).await;
            }
        }
    }
}

/// A test build's `blob_changed=<key>`: the object at that key reads back with its first byte
/// changed, as a store whose bytes changed under kiln (the Transfers plan's step 12).
fn changed(key: &str, start: u64, bytes: &mut [u8]) {
    if start == 0
        && crate::runner::chaos::get().is_some_and(|c| c.blob_changed.as_deref() == Some(key))
        && let Some(b) = bytes.first_mut()
    {
        *b ^= 0x01;
    }
}

impl Blobs for Attested {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            if !recorded(key) {
                return self.inner.put(key, bytes, put).await;
            }
            let sent = crate::util::sha256_hex(&bytes);
            let _putting = self.putting(key);
            let r = self.inner.put(key, bytes, put).await;
            if let Ok(meta) = &r {
                self.put_kept(key, &sent, &meta.etag).await;
            }
            r
        })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        Box::pin(async move {
            let start = range.map_or(0, |r| r.start);
            let (meta, mut bytes) = match self.inner.get(key, range).await {
                // A store that checks its bytes itself (Mantle's, CP-T10.2) refuses changed ones
                // before they reach here: the break is recorded all the same (W27-R08).
                Err(Refusal::BadChecksum { want, got }) => {
                    self.read_broke(key, &want, &got).await;
                    return Err(Refusal::BadChecksum { want, got });
                }
                other => other?,
            };
            changed(key, start, &mut bytes);
            let whole = start == 0 && u64::try_from(bytes.len()).is_ok_and(|n| n == meta.size);
            if whole {
                let got = crate::util::sha256_hex(&bytes);
                if got != meta.etag {
                    self.read_broke(key, &meta.etag, &got).await;
                    return Err(Refusal::BadChecksum { want: meta.etag, got });
                }
                if recorded(key) {
                    self.read_whole(key, &meta.etag, &got).await;
                }
            } else if recorded(key) {
                self.followed(key, start, &bytes, &meta).await;
            }
            Ok((meta, bytes))
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        self.inner.head(key)
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.inner.delete(key)
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        self.inner.list(prefix, after, limit)
    }

    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            let id = self.inner.start_upload(key).await?;
            if recorded(key) {
                self.uploads.lock().insert(id.clone(), (key.to_owned(), Some(Sha256::new()), 1));
            }
            Ok(id)
        })
    }

    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            if let Some((_, sha, next)) = self.uploads.lock().get_mut(upload) {
                match sha.as_mut() {
                    Some(s) if part == *next => {
                        s.update(&bytes);
                        *next = next.saturating_add(1);
                    }
                    _ => *sha = None,
                }
            }
            self.inner.upload_part(upload, part, bytes).await
        })
    }

    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            let Some((key, sha, _)) = self.uploads.lock().remove(upload) else { return self.inner.complete_upload(upload, parts).await };
            let _putting = self.putting(&key);
            let r = self.inner.complete_upload(upload, parts).await;
            if let Ok(meta) = &r {
                let sent = sha.map_or_else(|| meta.etag.clone(), |s| hex::encode(s.finalize()));
                self.put_kept(&key, &sent, &meta.etag).await;
            }
            r
        })
    }

    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.uploads.lock().remove(upload);
        self.inner.abort_upload(upload)
    }

    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        self.inner.presign(key, access, expires)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::blobs::{SimBlobs, SimLimits};
    use serde_json::Value;

    /// A store whose object at `lies` reads back with its first byte changed and its etag as it
    /// was: a store whose bytes changed under kiln.
    struct Lying {
        inner: Arc<dyn Blobs>,
        lies: String,
    }

    impl Blobs for Lying {
        fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
            self.inner.put(key, bytes, put)
        }

        fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
            Box::pin(async move {
                let (meta, mut bytes) = self.inner.get(key, range).await?;
                if key == self.lies
                    && range.is_none_or(|r| r.start == 0)
                    && let Some(b) = bytes.first_mut()
                {
                    *b ^= 0x01;
                }
                Ok((meta, bytes))
            })
        }

        fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
            self.inner.head(key)
        }

        fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
            self.inner.delete(key)
        }

        fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
            self.inner.list(prefix, after, limit)
        }

        fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
            self.inner.start_upload(key)
        }

        fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
            self.inner.upload_part(upload, part, bytes)
        }

        fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
            self.inner.complete_upload(upload, parts)
        }

        fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
            self.inner.abort_upload(upload)
        }

        fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
            self.inner.presign(key, access, expires)
        }
    }

    /// kiln's records of its store of bytes, in a fresh store and PKI: the store under them, and
    /// the store's records (`custody`) and breaks.
    struct Kept {
        _dir: tempfile::TempDir,
        store: Store,
        pki: Arc<crate::pki::Pki>,
        sim: Arc<dyn Blobs>,
    }

    impl Kept {
        async fn new() -> anyhow::Result<Self> {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = Arc::new(crate::pki::Pki::open_scratch(&dir.path().join("pki"))?);
            crate::identities::own_identities(&store, &pki).await?;
            let sim: Arc<dyn Blobs> = Arc::new(SimBlobs::new(crate::rt::clock(), SimLimits::default()));
            Ok(Self { _dir: dir, store, pki, sim })
        }

        fn over(&self, b: Arc<dyn Blobs>) -> Arc<dyn Blobs> {
            Arc::new(Attested::new(b, "local", self.store.clone(), Arc::clone(&self.pki)))
        }

        async fn count(&self, table: &'static str) -> anyhow::Result<i64> {
            self.store.call(move |c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)).await
        }

        /// The chain from the newest record of the thing `sha256` names.
        async fn chain(&self, sha256: &str) -> anyhow::Result<Value> {
            let v = super::super::chain(&self.store, &self.pki.ca_der, &self.pki.organization(), &format!("sha256:{sha256}"), crate::util::now_unix()).await?;
            v.ok_or_else(|| anyhow::anyhow!("kiln has no record of {sha256}"))
        }
    }

    fn hops(v: &Value) -> Vec<(String, Option<String>)> {
        v.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| (h.get("kind").and_then(Value::as_str).unwrap_or_default().to_owned(), h.get("taken_for").and_then(Value::as_str).map(str::to_owned))).collect()
    }

    /// W27-R08 (the Transfers plan's step 12): kiln signs for its store of bytes. A put is kiln's
    /// hand-on to the store and its receipt for it; a whole read, kiln's take for the store and its
    /// receipt of what it read, which walks back through both to the put, unbroken. Read again, the
    /// same records, kept once. A stream of ranges read from the first byte to the last is a whole
    /// read, and so is a file put in parts; an object whose key names its content gets no records.
    #[test]
    fn w27_r08_kilns_store_of_bytes_is_signed_for_at_each_put_and_whole_read() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let k = Kept::new().await?;
            let b = k.over(Arc::clone(&k.sim));
            let store = format!("kiln://{}/blobs/local", k.pki.organization());
            let bytes = b"kiln for linux/amd64".to_vec();
            let etag = crate::util::sha256_hex(&bytes);
            b.put("binaries/kiln-x86_64-linux", bytes.clone(), Put::default()).await?;
            assert_eq!(k.count("custody").await?, 2, "kiln's hand-on to the store and its receipt for it");
            let (_, got) = b.get("binaries/kiln-x86_64-linux", None).await?;
            assert_eq!(got, bytes);
            let v = k.chain(&etag).await?;
            assert_eq!(v.get("unbroken"), Some(&Value::Bool(true)), "{v}");
            assert_eq!(hops(&v), vec![("received".into(), None), ("taken".into(), Some(store.clone())), ("received".into(), Some(store.clone())), ("handed-on".into(), None)], "{v}");
            // Read again: the same records, kept once.
            b.get("binaries/kiln-x86_64-linux", None).await?;
            assert_eq!(k.count("custody").await?, 4);
            // Streamed a range at a time, from the first byte to the last: a whole read.
            let big = kiln_seams::blobs::noise((9 << 20) + 7, 5);
            let big_etag = crate::util::sha256_hex(&big);
            b.put("transcripts/s1/cc-1/0-99.jsonl.br", big.clone(), Put::default()).await?;
            let rx = crate::blobs::io::read_stream(Arc::clone(&b), "transcripts/s1/cc-1/0-99.jsonl.br".into(), Some(big_etag.clone()), 0, big.len() as u64 - 1);
            let mut streamed = Vec::new();
            while let Ok(piece) = rx.recv().await {
                streamed.extend_from_slice(&piece);
            }
            assert!(streamed == big, "the stream gave {} of {} bytes", streamed.len(), big.len());
            let v = k.chain(&big_etag).await?;
            assert!(v.get("unbroken") == Some(&Value::Bool(true)) && hops(&v).len() == 4, "{v}");
            // A file put in parts: its hand-on by what went up, part by part.
            let dir = tempfile::tempdir()?;
            let parts = kiln_seams::blobs::noise((20 << 20) + 3, 9);
            std::fs::write(dir.path().join("dist"), &parts)?;
            let parts_etag = crate::util::sha256_hex(&parts);
            crate::blobs::io::put_file(b.as_ref(), "dist/kiln-x86_64-linux", &dir.path().join("dist"), Some(&parts_etag)).await?;
            let v = k.chain(&parts_etag).await?;
            assert!(v.get("unbroken") == Some(&Value::Bool(true)) && hops(&v) == vec![("received".into(), Some(store.clone())), ("handed-on".into(), None)], "{v}");
            // Named by its content: no records.
            let before = k.count("custody").await?;
            let d = crate::util::sha256_hex(b"an rlib");
            b.put(&format!("cas/{d}"), b"an rlib".to_vec(), Put::default()).await?;
            b.get(&format!("cas/{d}"), None).await?;
            assert_eq!(k.count("custody").await?, before);
            assert_eq!(k.count("custody_breaks").await?, 0);
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 12): an object that reads back other than the store
    /// says is refused, and the break recorded at the take: kiln's receipt of what it read isn't
    /// what it took for the store. One that came into the store past kiln's puts begins its chain
    /// at kiln's take, with no break. One named by its content is refused and audited.
    #[test]
    fn w27_r08_an_object_read_back_changed_is_refused_and_its_break_recorded() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let k = Kept::new().await?;
            let lying: Arc<dyn Blobs> = Arc::new(Lying { inner: Arc::clone(&k.sim), lies: "artifacts/7/app".into() });
            let b = k.over(lying);
            b.put("artifacts/7/app", b"the app, built".to_vec(), Put::default()).await?;
            let r = b.get("artifacts/7/app", None).await;
            assert!(matches!(r, Err(Refusal::BadChecksum { .. })), "refused: {r:?}");
            let breaks = super::super::breaks(&k.store, 10).await?;
            assert!(breaks.iter().any(|x| x.get("kind").and_then(Value::as_str) == Some("changed") && x.get("detail").and_then(Value::as_str).is_some_and(|d| d.contains("object artifacts/7/app"))), "{breaks:?}");
            // Come into the store past kiln's puts (taken in where it lay, a rollback's reconcile, an
            // adopt): no break, and its chain begins at kiln's take for the store, not at a put.
            b.put("binaries/kiln-x86_64-linux", b"one".to_vec(), Put::default()).await?;
            let two = b"two".to_vec();
            k.sim.put("binaries/kiln-x86_64-linux", two.clone(), Put::default()).await?;
            let before = k.count("custody_breaks").await?;
            b.get("binaries/kiln-x86_64-linux", None).await?;
            assert_eq!(k.count("custody_breaks").await?, before);
            let v = k.chain(&crate::util::sha256_hex(&two)).await?;
            assert!(v.get("unbroken") == Some(&Value::Bool(true)) && hops(&v).iter().map(|h| h.0.as_str()).collect::<Vec<_>>() == ["received", "taken"], "{v}");
            // Named by its content: refused, and audited.
            let d = crate::util::sha256_hex(b"an rlib");
            let lying: Arc<dyn Blobs> = Arc::new(Lying { inner: Arc::clone(&k.sim), lies: format!("cas/{d}") });
            let b = k.over(lying);
            b.put(&format!("cas/{d}"), b"an rlib".to_vec(), Put::default()).await?;
            assert!(matches!(b.get(&format!("cas/{d}"), None).await, Err(Refusal::BadChecksum { .. })));
            let audited: i64 = k.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM audit2 WHERE action = 'blobs.read' AND result = 'refused'", [], |r| r.get(0))?)).await?;
            assert_eq!(audited, 1);
            Ok(())
        })
    }
}
