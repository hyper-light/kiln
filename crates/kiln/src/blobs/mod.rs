//! kiln's bytes, behind kiln-seams' `Blobs` (CP-T10.1, docs/blobs.md): the content-addressed
//! store's objects (CI caches, artifacts, registry blobs, release assets and test reports; CD reads
//! its artifacts there), sync's contents, live pages' recordings, ended agents' transcripts and
//! runner upgrade binaries. `[blobs] store` picks where: `local` (the default: the server's data
//! directory, [`local::LocalBlobs`], with what kiln kept before where it was) or `s3` (any
//! S3-compatible store, [`s3::S3Blobs`]). Every request goes through [`Metered`] (`kiln_blobs_*`).
//!
//! The keys kiln keeps, by what keeps them:
//! - `cas/<sha256>`: the content-addressed store's objects (`crate::ci::services::cas`);
//! - `sync/<sha256>`: sync's contents (`crate::sync::server`);
//! - `binaries/kiln-<target>` and `binaries/kiln-<target>.json`: runner upgrade binaries
//!   (`crate::updates`);
//! - `recordings/<session>/<recording>/<file>`: live pages' recordings (`crate::harness::streams`);
//! - `transcripts/<session>/<source>/<first>-<last>.jsonl.br`: ended agents' transcripts, in pieces
//!   (`crate::harness::archive`).

pub mod chunks;
pub mod cli;
pub mod io;
pub mod local;
pub mod s3;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use kiln_seams::Clock;
use kiln_seams::blobs::{Access, Blobs, Listing, Meta, Put, Range, Refusal};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The largest object kiln keeps, unless `[blobs] object_max` says otherwise.
pub const OBJECT_MAX: u64 = 64 << 30;

/// Which store keeps kiln's bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The server's data directory.
    #[default]
    Local,
    /// Any S3-compatible store (`[blobs.s3]`).
    S3,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
        }
    }
}

/// `[blobs]`: where kiln keeps its bytes (docs/blobs.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `local` (the server's data directory, the default) or `s3`.
    pub store: Kind,
    /// The largest object kept, in bytes.
    pub object_max: u64,
    /// `[blobs.s3]`: the S3-compatible store, for `store = "s3"`.
    pub s3: s3::Config,
}

impl Default for Config {
    fn default() -> Self {
        Self { store: Kind::Local, object_max: OBJECT_MAX, s3: s3::Config::default() }
    }
}

/// The store kiln keeps its bytes in, as opened.
#[derive(Clone)]
pub struct Opened {
    /// Every request, metered.
    pub blobs: Arc<dyn Blobs>,
    pub kind: Kind,
    /// The local store, when it's the one (its upkeep, its presigned URLs, files it takes in).
    pub local: Option<Arc<local::LocalBlobs>>,
    /// The S3 store, when it's the one (its upkeep).
    pub s3: Option<Arc<s3::S3Blobs>>,
}

/// The key a local store signs its presigned URLs with: `<data>/blobs.key`, made once (32 random
/// bytes, readable by the server alone).
fn presign_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    if let Ok(bytes) = std::fs::read(path) {
        return <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| anyhow::anyhow!("{} isn't 32 bytes", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let key: [u8; 32] = rand::random();
    crate::util::write_secret(path, &key, 0o600)?;
    Ok(key)
}

/// Opens the store `[blobs]` names: the local one in `<data>/cas`, its presigned URLs under the
/// server's `/api/blobs`; or the S3 one.
pub fn open(cfg: &crate::config::Config, store: &crate::store::Store, clock: Arc<dyn Clock>) -> anyhow::Result<Opened> {
    let b = &cfg.blobs;
    match b.store {
        Kind::Local => {
            let presign = local::Presign { base: format!("{}/api/blobs", cfg.server.public_url.trim_end_matches('/')), key: presign_key(&cfg.data("blobs.key"))? };
            let limits = local::Limits { object: b.object_max, ..local::Limits::default() };
            let l = Arc::new(local::LocalBlobs::new(store.clone(), cfg.data("cas"), clock, limits, presign));
            let blobs: Arc<dyn Blobs> = Arc::new(Metered::new(Arc::clone(&l), Kind::Local));
            Ok(Opened { blobs, kind: Kind::Local, local: Some(l), s3: None })
        }
        Kind::S3 => {
            let s = Arc::new(s3::S3Blobs::new(&b.s3, clock, s3::Limits { object: b.object_max, ..s3::Limits::default() })?);
            let blobs: Arc<dyn Blobs> = Arc::new(Metered::new(Arc::clone(&s), Kind::S3));
            Ok(Opened { blobs, kind: Kind::S3, local: None, s3: Some(s) })
        }
    }
}

/// How often the store's upkeep lets go of what nothing holds.
const UPKEEP_EVERY: Duration = Duration::from_secs(3600);

/// The store at the server's start, before anything reads from it: what an older kiln did to the
/// local store taken in ([`local::LocalBlobs::ready`]), the upgrade binaries kept as files taken in
/// where they lie, and what kiln knows of the kept binaries read. Sync's contents are listed by
/// their own ([`crate::sync::contents`]).
pub async fn start(hub: &Arc<crate::hub::Hub>) {
    if let Some(s3) = &hub.blobs.s3
        && hub.cfg.blobs.s3.create_bucket
        && let Err(e) = s3.ensure_bucket().await
    {
        tracing::warn!(target: "kiln.blobs", event = "put", error = %e, "the S3 bucket kiln keeps its bytes in couldn't be made");
    }
    if let Some(local) = &hub.blobs.local {
        local.ready().await;
        if let Err(e) = crate::updates::take_in(hub, local).await {
            tracing::warn!(target: "kiln.blobs", event = "adopt", error = %format!("{e:#}"), "runner binaries kept as files couldn't be taken in");
        }
    }
    if !crate::updates::load(hub).await {
        crate::rt::spawn(crate::updates::load_until_read(Arc::clone(hub)));
    }
}

/// The store's upkeep for as long as the server runs: every hour, uploads left two days, and
/// objects no key has held for an hour, let go with the chunks nothing uses. Each was counted as its
/// key went (`kiln_blobs_collected_total`, by what kept it).
pub async fn run(hub: Arc<crate::hub::Hub>) -> anyhow::Result<()> {
    loop {
        crate::rt::sleep(UPKEEP_EVERY).await;
        if let Some(local) = &hub.blobs.local {
            match local.collect().await {
                Ok(c) => {
                    if c.uploads > 0 || c.objects > 0 || c.chunks > 0 {
                        tracing::info!(target: "kiln.blobs", event = "collect", uploads = c.uploads, objects = c.objects, chunks = c.chunks, bytes = c.bytes, "the local store let go of what nothing holds");
                    }
                }
                Err(e) => tracing::warn!(target: "kiln.blobs", event = "collect", error = %format!("{e:#}"), "the local store's upkeep failed"),
            }
        }
        if let Some(s3) = &hub.blobs.s3 {
            match s3.collect().await {
                Ok(n) if n > 0 => tracing::info!(target: "kiln.blobs", event = "collect", uploads = n, "uploads left in S3 let go"),
                Ok(_) => {}
                Err(e) => tracing::warn!(target: "kiln.blobs", event = "collect", error = %format!("{e:#}"), "the S3 store's upkeep failed"),
            }
        }
    }
}

// ------------------------------------------------------------------------------------- metering

/// A store, its requests counted and timed (`kiln_blobs_*`), its refusals logged on `kiln.blobs`.
pub struct Metered<B> {
    inner: B,
    store: &'static str,
}

impl<B: Blobs> Metered<B> {
    pub fn new(inner: B, kind: Kind) -> Self {
        Self { inner, store: kind.as_str() }
    }

    fn seen<T>(&self, op: &'static str, began: Instant, r: &Result<T, Refusal>, written: u64, read: u64) {
        let m = crate::metrics::blobs();
        let outcome = match r {
            Ok(_) => "ok",
            Err(Refusal::Unavailable(why)) => {
                tracing::warn!(target: "kiln.blobs", event = "refused", store = self.store, op, why = %why, "the store is unavailable");
                "unavailable"
            }
            Err(e) => {
                tracing::debug!(target: "kiln.blobs", event = "refused", store = self.store, op, refusal = %e, "refused");
                "refused"
            }
        };
        m.requests.inc(&[self.store, op, outcome]);
        m.seconds.observe(&[self.store, op], began.elapsed().as_secs_f64());
        if r.is_ok() {
            if written > 0 {
                m.bytes.add(&[self.store, "written"], written);
            }
            if read > 0 {
                m.bytes.add(&[self.store, "read"], read);
            }
        }
    }
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

impl<B: Blobs> Blobs for Metered<B> {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>, put: Put) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            let (began, n) = (Instant::now(), len64(bytes.len()));
            let r = self.inner.put(key, bytes, put).await;
            self.seen("put", began, &r, n, 0);
            r
        })
    }

    fn get<'a>(&'a self, key: &'a str, range: Option<Range>) -> BoxFuture<'a, Result<(Meta, Vec<u8>), Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.get(key, range).await;
            let n = r.as_ref().map_or(0, |(_, b)| len64(b.len()));
            self.seen("get", began, &r, 0, n);
            r
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.head(key).await;
            self.seen("head", began, &r, 0, 0);
            r
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.delete(key).await;
            self.seen("delete", began, &r, 0, 0);
            r
        })
    }

    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Listing, Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.list(prefix, after, limit).await;
            self.seen("list", began, &r, 0, 0);
            r
        })
    }

    fn start_upload<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.start_upload(key).await;
            self.seen("start_upload", began, &r, 0, 0);
            r
        })
    }

    fn upload_part<'a>(&'a self, upload: &'a str, part: u32, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            let (began, n) = (Instant::now(), len64(bytes.len()));
            let r = self.inner.upload_part(upload, part, bytes).await;
            self.seen("upload_part", began, &r, n, 0);
            r
        })
    }

    fn complete_upload<'a>(&'a self, upload: &'a str, parts: Vec<(u32, String)>) -> BoxFuture<'a, Result<Meta, Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.complete_upload(upload, parts).await;
            self.seen("complete_upload", began, &r, 0, 0);
            r
        })
    }

    fn abort_upload<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.abort_upload(upload).await;
            self.seen("abort_upload", began, &r, 0, 0);
            r
        })
    }

    fn presign<'a>(&'a self, key: &'a str, access: Access, expires: Duration) -> BoxFuture<'a, Result<String, Refusal>> {
        Box::pin(async move {
            let began = Instant::now();
            let r = self.inner.presign(key, access, expires).await;
            self.seen("presign", began, &r, 0, 0);
            r
        })
    }
}

// -------------------------------------------------------------------- uploads' SHA-256, in order

/// Uploads whose SHA-256 is followed as their parts come, at most; past that, an upload's is read
/// back at completion.
const FOLLOWED_MAX: usize = 256;

/// An upload's SHA-256 so far: its parts 1 to `next - 1`, taken in order. `sha` is out while a part
/// is hashed; a part that comes then, out of order, or again, spoils it.
struct Followed {
    next: u32,
    sha: Option<Sha256>,
    spoiled: bool,
}

/// Uploads' SHA-256, taken as their parts come in order (most uploads' way, kiln's own all), so a
/// completion needn't read its object back to name it. Bounded: an upload past
/// [`FOLLOWED_MAX`] isn't followed and is read back.
#[derive(Default)]
pub(crate) struct Following {
    uploads: Mutex<HashMap<String, Followed>>,
}

impl Following {
    /// Follows a new upload, when there's room.
    pub(crate) fn start(&self, upload: &str) {
        let mut g = self.uploads.lock();
        if g.len() < FOLLOWED_MAX {
            g.insert(upload.to_owned(), Followed { next: 1, sha: Some(Sha256::new()), spoiled: false });
        }
    }

    /// Takes part `part`'s bytes into its upload's SHA-256 when it's the next in order, off the
    /// runtime's threads; else spoils it.
    pub(crate) async fn part(&self, upload: &str, part: u32, bytes: Arc<Vec<u8>>) {
        let taken = {
            let mut g = self.uploads.lock();
            match g.get_mut(upload) {
                Some(f) if !f.spoiled && f.next == part => f.sha.take().or_else(|| {
                    f.spoiled = true;
                    None
                }),
                Some(f) => {
                    f.spoiled = true;
                    None
                }
                None => None,
            }
        };
        let Some(mut sha) = taken else { return };
        let hashed = crate::rt::blocking(move || {
            sha.update(bytes.as_slice());
            sha
        })
        .await;
        let mut g = self.uploads.lock();
        if let Some(f) = g.get_mut(upload) {
            match hashed {
                Ok(sha) if !f.spoiled => {
                    f.sha = Some(sha);
                    f.next = f.next.saturating_add(1);
                }
                _ => f.spoiled = true,
            }
        }
    }

    /// The upload's SHA-256 as its parts came, when they came in order and are what's `named`;
    /// it's followed no more either way.
    pub(crate) fn finish(&self, upload: &str, named: &[(u32, String)]) -> Option<String> {
        let f = self.uploads.lock().remove(upload)?;
        let in_order = named.iter().enumerate().all(|(i, (n, _))| u32::try_from(i).ok().and_then(|i| i.checked_add(1)) == Some(*n));
        let count = u32::try_from(named.len()).ok()?;
        if f.spoiled || !in_order || f.next != count.checked_add(1)? {
            return None;
        }
        Some(hex::encode(f.sha?.finalize()))
    }

    pub(crate) fn forget(&self, upload: &str) {
        self.uploads.lock().remove(upload);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::future::Future;
    use std::path::PathBuf;

    use kiln_seams::blobs::BlobsFixture;
    use kiln_seams::conformance::Report;

    /// Runs the Blobs suite, each case against a store `make` makes for it, each given `deadline`.
    pub(crate) async fn run_suite<F, M, Fut>(mut make: M, deadline: std::time::Duration) -> anyhow::Result<Report>
    where
        F: BlobsFixture,
        M: FnMut() -> Fut,
        Fut: Future<Output = anyhow::Result<F>>,
    {
        let cases = kiln_seams::blobs::cases::<F>();
        let mut report = Report { seam: "Blobs", ..Report::default() };
        for case in &cases {
            let fixture = make().await?;
            match crate::rt::timeout(deadline, (case.check)(&fixture)).await {
                Ok(Ok(())) => report.passed.push(case.id),
                Ok(Err(why)) => report.failed.push((case.id, why)),
                Err(_) => report.failed.push((case.id, format!("it didn't end in {deadline:?}"))),
            }
        }
        Ok(report)
    }

    /// CP-T10.1: docs/blobs.md says every `[blobs]` and `[blobs.s3]` setting the code reads, and
    /// every family of keys kiln keeps, as the code makes them.
    #[test]
    fn docs_blobs_md_says_every_setting_and_key_family() -> anyhow::Result<()> {
        let doc = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/blobs.md"))?;
        let mut s3 = super::s3::Config::default();
        (s3.secret_access_key_file, s3.session_token_file, s3.ca_file) = (Some("f".into()), Some("f".into()), Some("f".into()));
        let cfg = toml::Value::try_from(super::Config { s3, ..super::Config::default() })?;
        let mut settings: Vec<String> = Vec::new();
        for (k, v) in cfg.as_table().into_iter().flatten() {
            match v.as_table() {
                Some(t) => settings.extend(t.keys().cloned()),
                None => settings.push(k.clone()),
            }
        }
        let missing: Vec<&String> = settings.iter().filter(|k| !doc.contains(&format!("\n{k} =")) && !doc.contains(&format!("# {k} ="))).collect();
        anyhow::ensure!(missing.is_empty(), "docs/blobs.md doesn't show the settings {missing:?}");
        let families = [
            crate::ci::services::cas::key("<sha256>"),
            crate::sync::contents::key("<sha256>"),
            crate::updates::binary_key("<target>"),
            crate::harness::streams::recording_key("session", "recording", "file"),
            crate::harness::archive::key("session", "source", 0, 9),
        ];
        for k in families {
            let family = k.split('/').next().unwrap_or_default();
            anyhow::ensure!(doc.contains(&format!("`{family}/")), "docs/blobs.md doesn't name the keys {family}/ ({k})");
        }
        Ok(())
    }
}
