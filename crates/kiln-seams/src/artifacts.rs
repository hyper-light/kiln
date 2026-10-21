//! `Artifacts`: OCI repositories (the Distribution Specification v1.1's model): blobs by digest,
//! manifests by tag or digest, kept byte for byte, tags that move, and referrers (manifests whose
//! `subject` names another). A manifest is taken only when what it references is there. Reliquary
//! is the real one (CP-T9.1, which passes the OCI conformance suite); [`SimArtifacts`] keeps
//! repositories in memory.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;

use crate::conformance::{Case, Failure, ensure, fine, refused, same};
use crate::sha256_hex;

/// A manifest as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub media_type: String,
    /// `sha256:<hex>` of its exact bytes.
    pub digest: String,
    pub bytes: Vec<u8>,
}

/// A manifest whose `subject` is the one asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    pub digest: String,
    pub media_type: String,
    pub artifact_type: Option<String>,
}

/// Why a request was refused (the Distribution Specification's error codes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("NAME_INVALID: {0:?}")]
    NameInvalid(String),
    #[error("TAG_INVALID: {0:?}")]
    TagInvalid(String),
    #[error("DIGEST_INVALID: {0}")]
    DigestInvalid(String),
    #[error("BLOB_UNKNOWN: {0}")]
    BlobUnknown(String),
    #[error("MANIFEST_UNKNOWN: {0}")]
    ManifestUnknown(String),
    #[error("MANIFEST_INVALID: {0}")]
    ManifestInvalid(String),
    #[error("MANIFEST_BLOB_UNKNOWN: {0}")]
    ManifestBlobUnknown(String),
    #[error("SIZE_INVALID: {what} is {size}, over its limit of {limit}")]
    TooLarge { what: &'static str, size: usize, limit: usize },
    #[error("the registry holds its most ({what}: {limit})")]
    Full { what: &'static str, limit: usize },
    #[error("DENIED: {0}")]
    Denied(String),
    #[error("the registry is unavailable: {0}")]
    Unavailable(String),
}

/// OCI repositories.
pub trait Artifacts: Send + Sync + 'static {
    /// Stores a blob; its digest. With `expect`, refused `DigestInvalid` unless that's its digest.
    fn push_blob<'a>(&'a self, repo: &'a str, bytes: Vec<u8>, expect: Option<&'a str>) -> BoxFuture<'a, Result<String, Refusal>>;
    fn blob<'a>(&'a self, repo: &'a str, digest: &'a str) -> BoxFuture<'a, Result<Vec<u8>, Refusal>>;
    /// Stores a manifest under `reference` (a tag, or its own digest); its digest. What it
    /// references (config, layers, an index's manifests, its subject excepted) must be there.
    fn push_manifest<'a>(&'a self, repo: &'a str, reference: &'a str, media_type: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>>;
    /// A manifest, by tag or digest.
    fn manifest<'a>(&'a self, repo: &'a str, reference: &'a str) -> BoxFuture<'a, Result<Manifest, Refusal>>;
    /// Tags in order, after `after`, at most `limit`.
    fn tags<'a>(&'a self, repo: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Vec<String>, Refusal>>;
    /// Deletes a manifest by digest, and every tag on it.
    fn delete_manifest<'a>(&'a self, repo: &'a str, digest: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Manifests whose subject is `digest`, of `artifact_type` when given.
    fn referrers<'a>(&'a self, repo: &'a str, digest: &'a str, artifact_type: Option<&'a str>) -> BoxFuture<'a, Result<Vec<Referrer>, Refusal>>;
}

/// Largest manifest (the Distribution Specification asks registries to take at least 4 MiB).
pub const MANIFEST_MAX: usize = 4 << 20;
/// Longest repository name.
pub const NAME_MAX: usize = 255;
/// Most tags a listing gives.
pub const TAGS_MAX: usize = 1000;

/// A repository name, as the Distribution Specification's grammar has it:
/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*(/[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*)*`.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > NAME_MAX {
        return false;
    }
    name.split('/').all(|part| {
        let b = part.as_bytes();
        let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
        if b.first().is_none_or(|c| !alnum(*c)) || b.last().is_none_or(|c| !alnum(*c)) {
            return false;
        }
        // Separators: one '.', one or two '_', or any run of '-', always between alphanumerics.
        let mut run: Vec<u8> = Vec::new();
        for c in b {
            if alnum(*c) {
                let ok = run.is_empty() || run == b"." || run == b"_" || run == b"__" || run.iter().all(|x| *x == b'-');
                if !ok {
                    return false;
                }
                run.clear();
            } else if matches!(c, b'.' | b'_' | b'-') {
                run.push(*c);
            } else {
                return false;
            }
        }
        true
    })
}

/// A tag: `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
pub fn valid_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    !b.is_empty() && b.len() <= 128 && b.first().is_some_and(word) && b.iter().all(|c| word(c) || matches!(c, b'.' | b'-'))
}

/// A digest this registry names content by: `sha256:` and 64 lowercase hex digits.
pub fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

/// `sha256:<hex>` of `bytes`.
pub fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// What a manifest references: (blobs, manifests, subject, artifact type).
type References = (Vec<String>, Vec<String>, Option<String>, Option<String>);

fn references(media_type: &str, bytes: &[u8]) -> Result<References, Refusal> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| Refusal::ManifestInvalid(format!("not JSON: {e}")))?;
    let digest_at = |x: &Value| x.get("digest").and_then(Value::as_str).map(str::to_owned);
    let mut blobs = Vec::new();
    let mut manifests = Vec::new();
    if let Some(c) = v.get("config") {
        blobs.push(digest_at(c).ok_or_else(|| Refusal::ManifestInvalid("its config has no digest".into()))?);
    }
    for l in v.get("layers").and_then(Value::as_array).into_iter().flatten() {
        blobs.push(digest_at(l).ok_or_else(|| Refusal::ManifestInvalid("a layer has no digest".into()))?);
    }
    for m in v.get("manifests").and_then(Value::as_array).into_iter().flatten() {
        manifests.push(digest_at(m).ok_or_else(|| Refusal::ManifestInvalid("an index entry has no digest".into()))?);
    }
    if let Some(mt) = v.get("mediaType").and_then(Value::as_str)
        && mt != media_type
    {
        return Err(Refusal::ManifestInvalid(format!("its mediaType is {mt}, sent as {media_type}")));
    }
    for d in blobs.iter().chain(manifests.iter()) {
        if !valid_digest(d) {
            return Err(Refusal::DigestInvalid(d.clone()));
        }
    }
    let subject = v.get("subject").and_then(digest_at);
    let artifact_type = v.get("artifactType").and_then(Value::as_str).map(str::to_owned).or_else(|| v.pointer("/config/mediaType").and_then(Value::as_str).map(str::to_owned));
    Ok((blobs, manifests, subject, artifact_type))
}

// ---------------------------------------------------------------------------------- simulator

/// Most repositories a [`SimArtifacts`] holds, and most blobs, manifests and tags in one.
pub const SIM_ITEMS: usize = 100_000;
/// Most bytes it holds, blobs and manifests together.
pub const SIM_BYTES: u64 = 1 << 30;

/// Repositories in memory. Clones share them.
#[derive(Clone, Default)]
pub struct SimArtifacts {
    inner: Arc<Mutex<SimState>>,
}

#[derive(Default)]
struct SimState {
    repos: BTreeMap<String, Repo>,
    bytes: u64,
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[derive(Default)]
struct Repo {
    blobs: HashMap<String, Arc<Vec<u8>>>,
    manifests: HashMap<String, (Manifest, Option<String>, Option<String>)>,
    tags: BTreeMap<String, String>,
}

impl SimArtifacts {
    pub fn new() -> Self {
        Self::default()
    }
}

fn check_repo(repo: &str) -> Result<(), Refusal> {
    if valid_name(repo) { Ok(()) } else { Err(Refusal::NameInvalid(repo.to_owned())) }
}

impl Artifacts for SimArtifacts {
    fn push_blob<'a>(&'a self, repo: &'a str, bytes: Vec<u8>, expect: Option<&'a str>) -> BoxFuture<'a, Result<String, Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            let digest = digest_of(&bytes);
            if let Some(e) = expect
                && e != digest
            {
                return Err(Refusal::DigestInvalid(format!("sent as {e}, it's {digest}")));
            }
            let mut st = self.inner.lock();
            if !st.repos.contains_key(repo) && st.repos.len() >= SIM_ITEMS {
                return Err(Refusal::Full { what: "repositories", limit: SIM_ITEMS });
            }
            let held = st.bytes;
            let r = st.repos.entry(repo.to_owned()).or_default();
            if r.blobs.contains_key(&digest) {
                return Ok(digest);
            }
            if r.blobs.len() >= SIM_ITEMS {
                return Err(Refusal::Full { what: "blobs", limit: SIM_ITEMS });
            }
            let total = held.saturating_add(len64(bytes.len()));
            if total > SIM_BYTES {
                return Err(Refusal::Full { what: "bytes", limit: usize::try_from(SIM_BYTES).unwrap_or(usize::MAX) });
            }
            r.blobs.insert(digest.clone(), Arc::new(bytes));
            st.bytes = total;
            Ok(digest)
        })();
        Box::pin(async move { out })
    }

    fn blob<'a>(&'a self, repo: &'a str, digest: &'a str) -> BoxFuture<'a, Result<Vec<u8>, Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            if !valid_digest(digest) {
                return Err(Refusal::DigestInvalid(digest.to_owned()));
            }
            self.inner.lock().repos.get(repo).and_then(|r| r.blobs.get(digest)).map(|b| b.as_ref().clone()).ok_or_else(|| Refusal::BlobUnknown(digest.to_owned()))
        })();
        Box::pin(async move { out })
    }

    fn push_manifest<'a>(&'a self, repo: &'a str, reference: &'a str, media_type: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<String, Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            if bytes.len() > MANIFEST_MAX {
                return Err(Refusal::TooLarge { what: "a manifest", size: bytes.len(), limit: MANIFEST_MAX });
            }
            let digest = digest_of(&bytes);
            let by_digest = reference.starts_with("sha256:");
            if by_digest && reference != digest {
                return Err(Refusal::DigestInvalid(format!("pushed as {reference}, it's {digest}")));
            }
            if !by_digest && !valid_tag(reference) {
                return Err(Refusal::TagInvalid(reference.to_owned()));
            }
            let (blobs, manifests, subject, artifact_type) = references(media_type, &bytes)?;
            let mut st = self.inner.lock();
            if !st.repos.contains_key(repo) && st.repos.len() >= SIM_ITEMS {
                return Err(Refusal::Full { what: "repositories", limit: SIM_ITEMS });
            }
            let held = st.bytes;
            let r = st.repos.entry(repo.to_owned()).or_default();
            if let Some(missing) = blobs.iter().find(|d| !r.blobs.contains_key(*d)) {
                return Err(Refusal::ManifestBlobUnknown(missing.clone()));
            }
            if let Some(missing) = manifests.iter().find(|d| !r.manifests.contains_key(*d)) {
                return Err(Refusal::ManifestBlobUnknown(missing.clone()));
            }
            let new = !r.manifests.contains_key(&digest);
            if new && r.manifests.len() >= SIM_ITEMS {
                return Err(Refusal::Full { what: "manifests", limit: SIM_ITEMS });
            }
            if !by_digest && !r.tags.contains_key(reference) && r.tags.len() >= SIM_ITEMS {
                return Err(Refusal::Full { what: "tags", limit: SIM_ITEMS });
            }
            let total = if new { held.saturating_add(len64(bytes.len())) } else { held };
            if total > SIM_BYTES {
                return Err(Refusal::Full { what: "bytes", limit: usize::try_from(SIM_BYTES).unwrap_or(usize::MAX) });
            }
            let m = Manifest { media_type: media_type.to_owned(), digest: digest.clone(), bytes };
            r.manifests.insert(digest.clone(), (m, subject, artifact_type));
            if !by_digest {
                r.tags.insert(reference.to_owned(), digest.clone());
            }
            st.bytes = total;
            Ok(digest)
        })();
        Box::pin(async move { out })
    }

    fn manifest<'a>(&'a self, repo: &'a str, reference: &'a str) -> BoxFuture<'a, Result<Manifest, Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            let st = self.inner.lock();
            let r = st.repos.get(repo).ok_or_else(|| Refusal::ManifestUnknown(reference.to_owned()))?;
            let digest = if reference.starts_with("sha256:") { reference.to_owned() } else { r.tags.get(reference).cloned().ok_or_else(|| Refusal::ManifestUnknown(reference.to_owned()))? };
            r.manifests.get(&digest).map(|(m, ..)| m.clone()).ok_or_else(|| Refusal::ManifestUnknown(reference.to_owned()))
        })();
        Box::pin(async move { out })
    }

    fn tags<'a>(&'a self, repo: &'a str, after: Option<&'a str>, limit: usize) -> BoxFuture<'a, Result<Vec<String>, Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            let st = self.inner.lock();
            let Some(r) = st.repos.get(repo) else { return Ok(Vec::new()) };
            let start = after.map_or(std::ops::Bound::Unbounded, |a| std::ops::Bound::Excluded(a.to_owned()));
            Ok(r.tags.range((start, std::ops::Bound::Unbounded)).take(limit.min(TAGS_MAX)).map(|(t, _)| t.clone()).collect())
        })();
        Box::pin(async move { out })
    }

    fn delete_manifest<'a>(&'a self, repo: &'a str, digest: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            if !valid_digest(digest) {
                return Err(Refusal::DigestInvalid(digest.to_owned()));
            }
            let mut st = self.inner.lock();
            let r = st.repos.get_mut(repo).ok_or_else(|| Refusal::ManifestUnknown(digest.to_owned()))?;
            let (m, ..) = r.manifests.remove(digest).ok_or_else(|| Refusal::ManifestUnknown(digest.to_owned()))?;
            r.tags.retain(|_, d| d != digest);
            st.bytes = st.bytes.saturating_sub(len64(m.bytes.len()));
            Ok(())
        })();
        Box::pin(async move { out })
    }

    fn referrers<'a>(&'a self, repo: &'a str, digest: &'a str, artifact_type: Option<&'a str>) -> BoxFuture<'a, Result<Vec<Referrer>, Refusal>> {
        let out = (|| {
            check_repo(repo)?;
            if !valid_digest(digest) {
                return Err(Refusal::DigestInvalid(digest.to_owned()));
            }
            let st = self.inner.lock();
            let Some(r) = st.repos.get(repo) else { return Ok(Vec::new()) };
            let mut out: Vec<Referrer> = r
                .manifests
                .values()
                .filter(|(_, subject, at)| subject.as_deref() == Some(digest) && artifact_type.is_none_or(|want| at.as_deref() == Some(want)))
                .map(|(m, _, at)| Referrer { digest: m.digest.clone(), media_type: m.media_type.clone(), artifact_type: at.clone() })
                .collect();
            out.sort_by(|a, b| a.digest.cmp(&b.digest));
            Ok(out)
        })();
        Box::pin(async move { out })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What an `Artifacts` conformance suite runs against: an empty registry per case.
pub trait ArtifactsFixture: Send + Sync {
    fn artifacts(&self) -> &dyn Artifacts;
}

/// The fixture for a [`SimArtifacts`].
pub struct SimArtifactsFixture(pub SimArtifacts);

impl ArtifactsFixture for SimArtifactsFixture {
    fn artifacts(&self) -> &dyn Artifacts {
        &self.0
    }
}

const R: &str = "conformance/app";
const IMAGE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";

fn image(config: &str, layers: &[&str], subject: Option<&str>, artifact_type: Option<&str>) -> Vec<u8> {
    let mut v = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": IMAGE,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config, "size": 2},
        "layers": layers.iter().map(|d| serde_json::json!({"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": d, "size": 1})).collect::<Vec<_>>(),
    });
    if let (Some(s), Some(obj)) = (subject, v.as_object_mut()) {
        obj.insert("subject".into(), serde_json::json!({"mediaType": IMAGE, "digest": s, "size": 1}));
    }
    if let (Some(t), Some(obj)) = (artifact_type, v.as_object_mut()) {
        obj.insert("artifactType".into(), Value::String(t.to_owned()));
    }
    serde_json::to_vec(&v).unwrap_or_default()
}

async fn base(a: &dyn Artifacts) -> Result<(String, String), Failure> {
    let config = fine("push the config", a.push_blob(R, b"{}".to_vec(), None).await)?;
    let layer = fine("push a layer", a.push_blob(R, b"L".to_vec(), None).await)?;
    Ok((config, layer))
}

fn c1<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let d = fine("push", a.push_blob(R, b"layer bytes".to_vec(), None).await)?;
        same("its digest", d.clone(), digest_of(b"layer bytes"))?;
        same("the blob read back", fine("blob", a.blob(R, &d).await)?, b"layer bytes".to_vec())?;
        same("a blob never pushed", a.blob(R, &digest_of(b"other")).await, Err(Refusal::BlobUnknown(digest_of(b"other"))))
    })
}

fn c2<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let wrong = digest_of(b"something else");
        ensure(matches!(a.push_blob(R, b"bytes".to_vec(), Some(&wrong)).await, Err(Refusal::DigestInvalid(_))), || "a blob under the wrong digest was taken".into())?;
        same("what that push left", a.blob(R, &digest_of(b"bytes")).await, Err(Refusal::BlobUnknown(digest_of(b"bytes"))))
    })
}

fn c3<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, layer) = base(a).await?;
        let bytes = image(&config, &[&layer], None, None);
        let d = fine("push the manifest", a.push_manifest(R, "v1", IMAGE, bytes.clone()).await)?;
        same("its digest", d.clone(), digest_of(&bytes))?;
        let by_tag = fine("by tag", a.manifest(R, "v1").await)?;
        let by_digest = fine("by digest", a.manifest(R, &d).await)?;
        same("by tag", by_tag.clone(), Manifest { media_type: IMAGE.into(), digest: d, bytes })?;
        same("by digest", by_digest, by_tag)
    })
}

fn c4<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, _) = base(a).await?;
        let missing = digest_of(b"never pushed");
        let e = refused("a manifest with a layer never pushed", a.push_manifest(R, "v1", IMAGE, image(&config, &[&missing], None, None)).await)?;
        same("the refusal", e, Refusal::ManifestBlobUnknown(missing))?;
        same("the tag after it", a.manifest(R, "v1").await.map(|m| m.digest), Err(Refusal::ManifestUnknown("v1".into())))?;
        let index = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "mediaType": INDEX, "manifests": [{"mediaType": IMAGE, "digest": digest_of(b"no such manifest"), "size": 1}]})).unwrap_or_default();
        ensure(matches!(a.push_manifest(R, "all", INDEX, index).await, Err(Refusal::ManifestBlobUnknown(_))), || "an index of a manifest never pushed was taken".into())
    })
}

fn c5<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, layer) = base(a).await?;
        let one = fine("push", a.push_manifest(R, "latest", IMAGE, image(&config, &[], None, None)).await)?;
        let two = fine("push", a.push_manifest(R, "latest", IMAGE, image(&config, &[&layer], None, None)).await)?;
        same("the tag, moved", fine("by tag", a.manifest(R, "latest").await)?.digest, two)?;
        same("the first, by its digest", fine("by digest", a.manifest(R, &one).await)?.digest, one.clone())
    })
}

fn c6<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, _) = base(a).await?;
        let bytes = image(&config, &[], None, None);
        for t in ["v2", "v1", "v10", "latest"] {
            fine("push", a.push_manifest(R, t, IMAGE, bytes.clone()).await)?;
        }
        same("the first page", fine("tags", a.tags(R, None, 2).await)?, vec!["latest".to_owned(), "v1".to_owned()])?;
        same("the next", fine("tags", a.tags(R, Some("v1"), 10).await)?, vec!["v10".to_owned(), "v2".to_owned()])
    })
}

fn c7<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, _) = base(a).await?;
        let d = fine("push", a.push_manifest(R, "v1", IMAGE, image(&config, &[], None, None)).await)?;
        fine("push the same under another tag", a.push_manifest(R, "stable", IMAGE, image(&config, &[], None, None)).await)?;
        fine("delete", a.delete_manifest(R, &d).await)?;
        same("by digest after", a.manifest(R, &d).await.map(|m| m.digest), Err(Refusal::ManifestUnknown(d.clone())))?;
        same("its tags after", fine("tags", a.tags(R, None, 10).await)?, Vec::<String>::new())?;
        same("deleting it again", a.delete_manifest(R, &d).await, Err(Refusal::ManifestUnknown(d)))
    })
}

fn c8<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, _) = base(a).await?;
        let subject = fine("push the subject", a.push_manifest(R, "v1", IMAGE, image(&config, &[], None, None)).await)?;
        let sig_bytes = image(&config, &[], Some(&subject), Some("application/vnd.dev.sigstore.bundle+json"));
        let sig = fine("push a signature", a.push_manifest(R, &digest_of(&sig_bytes), IMAGE, sig_bytes).await)?;
        let sbom_bytes = image(&config, &[], Some(&subject), Some("application/spdx+json"));
        let sbom = fine("push an SBOM", a.push_manifest(R, &digest_of(&sbom_bytes), IMAGE, sbom_bytes).await)?;
        let all: Vec<String> = fine("referrers", a.referrers(R, &subject, None).await)?.into_iter().map(|r| r.digest).collect();
        let mut want = vec![sig.clone(), sbom];
        want.sort();
        same("its referrers", all, want)?;
        let sigs: Vec<String> = fine("referrers of a type", a.referrers(R, &subject, Some("application/vnd.dev.sigstore.bundle+json")).await)?.into_iter().map(|r| r.digest).collect();
        same("its signatures", sigs, vec![sig])
    })
}

fn c9<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        for bad in ["Upper/case", "-leading", "trailing-", "a..b", "a/", "", "a___b"] {
            ensure(matches!(a.push_blob(bad, b"x".to_vec(), None).await, Err(Refusal::NameInvalid(_))), || format!("the name {bad:?} was taken"))?;
        }
        for good in ["a", "a/b-c", "a__b/c.d", "x0/y1/z2", "a--b"] {
            fine(&format!("the name {good:?}"), a.push_blob(good, b"x".to_vec(), None).await)?;
        }
        let (config, _) = base(a).await?;
        let long = "t".repeat(129);
        for bad in [".tag", "-tag", "t a g", long.as_str()] {
            ensure(matches!(a.push_manifest(R, bad, IMAGE, image(&config, &[], None, None)).await, Err(Refusal::TagInvalid(_))), || format!("the tag {bad:?} was taken"))?;
        }
        ensure(matches!(a.blob(R, "md5:abc").await, Err(Refusal::DigestInvalid(_))), || "a digest that isn't sha256 was taken".into())
    })
}

fn c10<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let d = fine("push to one", a.push_blob("conformance/one", b"only here".to_vec(), None).await)?;
        same("the blob in another", a.blob("conformance/two", &d).await, Err(Refusal::BlobUnknown(d.clone())))?;
        let config = fine("push the config", a.push_blob("conformance/two", b"{}".to_vec(), None).await)?;
        ensure(matches!(a.push_manifest("conformance/two", "v1", IMAGE, image(&config, &[&d], None, None)).await, Err(Refusal::ManifestBlobUnknown(_))), || "a manifest took a blob from another repository".into())
    })
}

fn c11<F: ArtifactsFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = f.artifacts();
        let (config, _) = base(a).await?;
        let bytes = image(&config, &[], None, None);
        let wrong = digest_of(b"not these bytes");
        ensure(matches!(a.push_manifest(R, &wrong, IMAGE, bytes.clone()).await, Err(Refusal::DigestInvalid(_))), || "a manifest pushed under another's digest was taken".into())?;
        ensure(matches!(a.push_manifest(R, "v1", IMAGE, b"not json".to_vec()).await, Err(Refusal::ManifestInvalid(_))), || "a manifest that isn't JSON was taken".into())?;
        ensure(matches!(a.push_manifest(R, "v1", INDEX, bytes).await, Err(Refusal::ManifestInvalid(_))), || "a manifest sent as another media type was taken".into())?;
        ensure(matches!(a.push_manifest(R, "v1", IMAGE, vec![b' '; MANIFEST_MAX.saturating_add(1)]).await, Err(Refusal::TooLarge { .. })), || "a manifest over 4 MiB was taken".into())
    })
}

/// The `Artifacts` contract, clause by clause (docs/seams.md, "Artifacts").
pub fn cases<F: ArtifactsFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "ARTIFACTS-1", says: "a blob pushed is named by the SHA-256 of its bytes and read back by it; one never pushed is refused BLOB_UNKNOWN", check: c1::<F> },
        Case { id: "ARTIFACTS-2", says: "a blob pushed under a digest that isn't its own is refused DIGEST_INVALID and stored nowhere", check: c2::<F> },
        Case { id: "ARTIFACTS-3", says: "a manifest is read back by tag and by digest, byte for byte, with its media type; its digest is its bytes' SHA-256", check: c3::<F> },
        Case { id: "ARTIFACTS-4", says: "a manifest that references a blob or manifest not in the repository is refused MANIFEST_BLOB_UNKNOWN, and its tag isn't set", check: c4::<F> },
        Case { id: "ARTIFACTS-5", says: "a tag moves to the manifest last pushed under it; the one before stays, by its digest", check: c5::<F> },
        Case { id: "ARTIFACTS-6", says: "a repository's tags are listed in order, after a cursor, at most a limit", check: c6::<F> },
        Case { id: "ARTIFACTS-7", says: "a manifest deleted by digest is gone, and every tag on it with it; deleting it again is refused MANIFEST_UNKNOWN", check: c7::<F> },
        Case { id: "ARTIFACTS-8", says: "the referrers of a manifest are the manifests whose subject it is, filtered by artifact type when asked", check: c8::<F> },
        Case { id: "ARTIFACTS-9", says: "names, tags and digests outside the Distribution Specification's grammar are refused NAME_INVALID, TAG_INVALID and DIGEST_INVALID", check: c9::<F> },
        Case { id: "ARTIFACTS-10", says: "a repository sees only what was pushed to it", check: c10::<F> },
        Case { id: "ARTIFACTS-11", says: "a manifest pushed under another digest, not JSON, of another media type than it says, or over 4 MiB is refused", check: c11::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_registry_passes_the_artifacts_suite() -> Result<(), String> {
        futures::executor::block_on(run("Artifacts", &cases::<SimArtifactsFixture>(), || async { SimArtifactsFixture(SimArtifacts::new()) })).into_result().map(|_| ())
    }

    #[test]
    fn the_grammars_are_the_distribution_specification_s() {
        assert!(valid_name("library/ubuntu") && valid_name("a.b_c__d-e---f") && !valid_name("a_.b") && !valid_name(&"a".repeat(256)));
        assert!(valid_tag("v1.2.3-rc_1") && valid_tag("_x") && !valid_tag("") && !valid_tag("é"));
        assert!(valid_digest(&digest_of(b"")) && !valid_digest("sha256:ABC") && !valid_digest("sha512:00"));
    }
}
