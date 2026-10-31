//! The runner's local content store (docs/ci.md W24-R08): metadata at `<work>/_cas`, bytes
//! in the shared runner chunk tier at `<work>/_cache`:
//!
//! - **Objects**, named by their content's SHA-256: whole files up to [`WHOLE_MAX`], larger ones
//!   as the store's content-defined chunks (`kiln_ci::store::cdc`, the cuts the server makes, kept
//!   as `kiln/cache@v1`'s local tier keeps them), so two versions of a large object share every
//!   chunk the change didn't touch. Every read is checked against the name; a copy that isn't
//!   what its name says is discarded and reads as missing.
//! - **Entries**: a tool's name for an object (sccache's key, Bazel's action digest, a Turborepo
//!   hash) in a namespace, under a [`Scope`]: a repository and a trust domain (W24-R21). They're
//!   the work ledger's (`crate::ci::work`, in the host Amphora database): each is a unit of kind `action` whose
//!   definition is the tool's own key ([`entry_key`]), kept by its weak key as a name
//!   (`work.entry`): a newer value or a removal replaces it, as a tool's cache expects, where the
//!   ledger's results are never overwritten (W24-R22).
//! - **Holdings**: the objects each scope put, so an existence check answers for the asker's own
//!   scopes only. Bytes are kept once whoever sends them, but a repository never learns what
//!   another stored: it sends the bytes again (`ci/services/chunks.rs` refuses probing alike).
//!
//! Scoped holdings and entries share the host Amphora database. The in-memory holdings map is
//! rebuilt from it; old `index.log` and `ledger.db` files are read-only migration inputs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use kiln_ci::store::cdc;
use kiln_ci::work::key::{self, Key, Kind, Platform, Unit};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ci::work::{Provenance, Recorded};
use crate::runner::cache::{CacheGuard, Local};

/// Objects up to this size are kept whole, larger ones in content-defined chunks: one no larger
/// than the store's largest chunk would be a chunk or two anyway.
pub const WHOLE_MAX: u64 = cdc::MAX as u64;
// U07: manifests are derived physical assembly, bounded before JSON allocation.
const MAX_ASSEMBLY: u64 = 8 << 20;
const MAX_PARTS: usize = 65_535;

/// W24-R08, U07: byte/file publication uses blocking regular-file IO except Local's
/// explicit nonblocking mutation fence. Only that fence synthesizes WouldBlock without
/// an OS error code; ordinary filesystem errors and durable metadata refusals are not busy.
pub(super) fn publication_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock && e.raw_os_error().is_none())
}

/// A repository and a trust domain (docs/ci.md W24-R21): what an entry belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Scope {
    pub repo: String,
    /// `trusted`, `protected`, `untrusted` or `person:<id>`.
    pub domain: String,
}

impl Scope {
    pub fn new(repo: &str, domain: &str) -> Scope {
        Scope { repo: repo.to_owned(), domain: domain.to_owned() }
    }

    /// As the work ledger keys its rows (`owner/repo|domain|`, `crate::ci::work::scope`).
    pub fn key(&self) -> String {
        crate::ci::work::scope(&self.repo, &self.domain)
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.repo, self.domain)
    }
}

/// What a name stands for in a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub digest: String,
    pub size: u64,
    /// When it was put, in seconds since the epoch.
    pub at: i64,
    /// What the tool sent with it and wants back with it (Turborepo's `x-artifact-tag`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

/// How an object is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// One file.
    Whole(PathBuf),
    /// Its chunks, in order: each one's digest and size.
    Chunks(Vec<(String, u64)>),
}

/// U07: physical publication ownership crosses logical commit and upload queue handoff.
#[derive(Debug)]
pub(crate) struct Published {
    pub digest: String,
    pub size: u64,
    pub guard: CacheGuard,
}

/// Whether `d` names an object: 64 lowercase hex digits.
pub fn is_digest(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One line of the journal.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Line {
    /// An object a scope put.
    Hold { scope: Scope, digest: String },
}

#[derive(Debug, Default)]
struct Index {
    held: HashMap<Scope, HashSet<String>>,
}

impl Index {
    fn apply(&mut self, line: Line) {
        let Line::Hold { scope, digest } = line;
        self.held.entry(scope).or_default().insert(digest);
    }

    fn lines(&self) -> usize {
        self.held.values().map(HashSet::len).sum::<usize>()
    }
}

/// A tool's name for an object as a unit of work (W24-R30: every key on the ledger's one path):
/// an action whose definition is the tool's own key in its namespace, in the scope's domain.
pub fn entry_key(ns: &str, name: &str, domain: &str) -> Key {
    key::weak(&entry_unit(ns, name, domain))
}

/// The unit of `ns`/`name` in `domain` that [`entry_key`] keys: what the ledger looks up and
/// records (W24-R30).
pub fn entry_unit(ns: &str, name: &str, domain: &str) -> Unit {
    Unit {
        kind: Kind::Action,
        definition: format!("kiln/cas {ns}\n{name}"),
        secrets: Default::default(),
        env: Default::default(),
        platform: Platform::default(),
        inputs: Default::default(),
        upstream: Default::default(),
        trust: domain.to_owned(),
    }
}

/// An entry as the ledger records it: its object and size among the outputs, what the tool sent
/// with it as `meta:<header>`, when it was put as when it finished. A removed entry is recorded
/// with no object.
fn recorded(e: Option<&Entry>, domain: &str, at: i64) -> Recorded {
    let mut outputs = BTreeMap::new();
    if let Some(e) = e {
        outputs.insert("object".to_owned(), e.digest.clone());
        outputs.insert("size".to_owned(), e.size.to_string());
        for (k, v) in &e.meta {
            outputs.insert(format!("meta:{k}"), v.clone());
        }
    }
    let outcome = if e.is_some() { "success" } else { "removed" };
    Recorded { outcome: outcome.to_owned(), outputs, provenance: Provenance { finished_at: at, ..Provenance::default() }, domain: domain.to_owned(), ..Recorded::default() }
}

fn entry_of(r: &Recorded) -> Option<Entry> {
    let digest = r.outputs.get("object").filter(|d| is_digest(d))?.clone();
    let size = r.outputs.get("size").and_then(|s| s.parse().ok()).unwrap_or(0);
    let meta = r.outputs.iter().filter_map(|(k, v)| k.strip_prefix("meta:").map(|k| (k.to_owned(), v.clone()))).collect();
    Some(Entry { digest, size, at: r.provenance.finished_at, meta })
}

/// What `scope` records for `ns`/`name` on the ledger: an entry, a removal (`Some(None)`), or
/// nothing.
fn ledger_entry(c: &crate::sql::Connection, scope: &Scope, ns: &str, name: &str) -> anyhow::Result<Option<Option<Entry>>> {
    let at = format!("{}{}", scope.key(), entry_key(ns, name, &scope.domain).hex());
    match crate::ci::work::get(c, crate::ci::work::Family::Entry, &at)? {
        Some(v) => Ok(Some(entry_of(&serde_json::from_slice(&v)?))),
        None => Ok(None),
    }
}

/// Keeps `r` as what `scope` names `ns`/`name` on the ledger, in place of what it named before.
fn put_entry(c: &crate::sql::Connection, scope: &Scope, ns: &str, name: &str, r: &Recorded) -> anyhow::Result<()> {
    let at = format!("{}{}", scope.key(), entry_key(ns, name, &scope.domain).hex());
    crate::ci::work::put(c, crate::ci::work::Family::Entry, &at, &serde_json::to_vec(r)?, r.provenance.finished_at)?;
    Ok(())
}

mod metadata;

/// The store.
pub struct Store {
    root: PathBuf,
    chunks: Local,
    index: RwLock<Index>,
    /// Scoped references and ledger records share the injected host database.
    metadata: metadata::Metadata,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("root", &self.root).finish_non_exhaustive()
    }
}

impl Store {
    /// The store under `work` (`<work>/_cas`), its index read back.
    #[cfg(test)]
    pub fn open(work: &Path) -> anyhow::Result<Store> {
        let ledger=crate::store::Store::open(&work.join("kiln.db"))?;
        Self::with_ledger(work,ledger,"standalone")
    }

    /// U05/U07: production consumers inject the host database; cache bytes stay under work.
    pub fn with_ledger(work:&Path,ledger:crate::store::Store,host_key:&str)->anyhow::Result<Store> {
        Self::with_ledger_max(work, ledger, host_key, None)
    }

    pub(crate) fn with_ledger_max(work: &Path, ledger: crate::store::Store, host_key: &str, max: Option<u64>) -> anyhow::Result<Store> {
        // U07: legacy writers have no operation leases. Their retirement requires an explicit
        // offline attestation, even when the new shared tier has not yet been created.
        let legacy = crate::runner::cache::files(&work.join("_cas/chunks")).into_iter()
            .chain(crate::runner::cache::files(&work.join("_cas/objects")))
            .any(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_digest));
        if legacy {
            anyhow::ensure!(std::fs::read(work.join("_cache/.u07-quiescent")).ok().as_deref() == Some(b"U07: all pre-format cache writers are quiescent\n".as_slice()), "U07 legacy cache import requires verified offline old-writer quiescence");
        }
        let chunks = Local::with_ledger(work, ledger.clone(), max)?;
        let root = work.join("_cas");
        for d in ["objects", "tmp"] {
            std::fs::create_dir_all(root.join(d)).with_context(|| format!("create {}", root.join(d).display()))?;
        }
        let (metadata,index)=metadata::Metadata::open(work,&root,ledger,host_key).context("the runner's shared work metadata")?;
        chunks.import(&root.join("chunks"))?;
        let store = Store { chunks, root, index: RwLock::new(index), metadata };
        // U07: whole legacy objects join the same physical chunk tier. Publish a manifest
        // before removing the old copy, so reopening after an interrupted import is harmless.
        for path in crate::runner::cache::files(&store.root.join("objects")) {
            let Some(digest) = path.file_name().and_then(|n| n.to_str()).filter(|d| is_digest(d)) else { continue };
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let mut data = Vec::new();
            file.take(WHOLE_MAX + 1).read_to_end(&mut data)?;
            anyhow::ensure!(data.len() as u64 <= WHOLE_MAX, "legacy whole cache object exceeds its 8 MiB source contract");
            if hex::encode(Sha256::digest(&data)) != digest { store.discard_corrupt_copy(&path, digest); continue; }
            let publication = match store.put_bytes_guarded(&data) {
                Ok(publication) => publication,
                Err(error) if Local::declined(&error) => {
                    // U07: legacy bytes are optional idle cache after proven cutover. A
                    // smaller declared tier must not prevent the runner from becoming ready.
                    store.chunks.discard_legacy_idle(&path, digest)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            anyhow::ensure!(publication.digest == digest, "legacy cache digest changed");
            store.chunks.discard_legacy_idle(&path, digest)?;
        }
        store.chunks.maintain();
        Ok(store)
    }

    pub fn local(&self) -> Local { self.chunks.clone() }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This runner's work ledger (`crate::ci::work`'s tables in the host database): entries here,
    /// and `kiln run`'s results (`crate::reach::reuse`).
    pub fn ledger(&self) -> &crate::store::Store {
        self.metadata.ledger()
    }

    /// Where a temporary file is made: in the store, so it's renamed into place.
    pub fn temp(&self) -> PathBuf {
        self.root.join("tmp").join(crate::util::new_id())
    }

    fn object_path(&self, digest: &str) -> PathBuf {
        self.root.join("objects").join(digest.get(0..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest)
    }

    fn list_path(&self, digest: &str) -> PathBuf {
        self.object_path(digest).with_extension("chunks")
    }

    fn read_list(&self, digest: &str) -> Option<Vec<(String, u64)>> {
        let file = std::fs::File::open(self.list_path(digest)).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_ASSEMBLY + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_ASSEMBLY { return None; }
        let list: Vec<(String, u64)> = serde_json::from_slice(&bytes).ok()?;
        let total = list.iter().try_fold(0u64, |total, (digest, size)| {
            if !is_digest(digest) || *size > WHOLE_MAX { return None; }
            total.checked_add(*size)
        })?;
        ((1..=MAX_PARTS).contains(&list.len()) && total <= super::service::MAX_OBJECT).then_some(list)
    }

    /// How the object is kept here, when it is.
    pub fn kept(&self, digest: &str) -> Option<Kept> {
        if !is_digest(digest) {
            return None;
        }
        let whole = self.object_path(digest);
        if whole.is_file() {
            return Some(Kept::Whole(whole));
        }
        let list = self.read_list(digest)?;
        if !list.iter().all(|(d, n)| self.chunks.contains(d, *n)) {
            // W24-R08, U07: size rejection must not hide a poisoned whole object's shared
            // backing file from quarantine. A missing file remains a miss; verify before
            // moving anything so a bad manifest cannot quarantine valid shared bytes.
            if let [(d, _)] = list.as_slice()
                && d == digest {
                let path = self.chunks.path(d);
                if let Ok(data) = std::fs::read(&path)
                    && hex::encode(Sha256::digest(&data)) != digest {
                    self.discard_corrupt_copy(&path, digest);
                }
            }
            return None;
        }
        if let [(d, _)] = list.as_slice()
            && d == digest { return Some(Kept::Whole(self.chunks.path(d))); }
        Some(Kept::Chunks(list))
    }

    /// Whether the object is here.
    pub fn has(&self, digest: &str) -> bool {
        self.kept(digest).is_some()
    }

    /// U07: reserve the complete unique footprint before writing the first chunk.
    pub(crate) fn put_bytes_guarded(&self, data: &[u8]) -> anyhow::Result<Published> {
        let size = u64::try_from(data.len())?;
        if size > super::service::MAX_OBJECT {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache object exceeds 16 GiB").into());
        }
        let digest = hex::encode(Sha256::digest(data));
        let mut list = Vec::new();
        let cuts = if size <= WHOLE_MAX { vec![data.len()] } else { cdc::cuts(data) };
        let mut at = 0usize;
        for n in cuts {
            let end = at.checked_add(n).context("cache chunk offset overflow")?;
            let piece = data.get(at..end).context("invalid cache chunk boundary")?;
            list.push((hex::encode(Sha256::digest(piece)), n as u64));
            at = end;
        }
        anyhow::ensure!(at == data.len(), "incomplete cache chunk plan");
        let manifest = serde_json::to_vec(&list)?;
        let mut publication = self.chunks.reserve_object(&list, &digest, &manifest)?;
        let mut at = 0usize;
        for (d, n) in &list {
            let end = at.checked_add(usize::try_from(*n)?).context("cache chunk offset overflow")?;
            publication.put(d, data.get(at..end).context("invalid cache chunk boundary")?)?;
            at = end;
        }
        publication.write_manifest(&digest, &manifest)?;
        let guard = publication.finish()?;
        Ok(Published { digest, size, guard })
    }

    pub fn put_bytes(&self, data: &[u8]) -> anyhow::Result<(String, u64)> {
        let p = self.put_bytes_guarded(data)?;
        Ok((p.digest, p.size))
    }

    /// Two bounded passes: plan first, then publish while owning the complete reservation.
    pub(crate) fn put_file_guarded(&self, path: &Path) -> anyhow::Result<Published> {
        let len = std::fs::metadata(path).with_context(|| format!("read {}", path.display()))?.len();
        if len > super::service::MAX_OBJECT {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache object exceeds 16 GiB").into());
        }
        if len <= WHOLE_MAX { return self.put_bytes_guarded(&std::fs::read(path)?); }
        let (digest, size, plan) = crate::runner::cache::chunks(path, None)?;
        let list: Vec<(String, u64)> = plan.iter().map(|c| (c.digest.clone(), c.size)).collect();
        let manifest = serde_json::to_vec(&list)?;
        let mut publication = self.chunks.reserve_object(&list, &digest, &manifest)?;
        let mut file = std::fs::File::open(path)?;
        let mut whole = Sha256::new();
        for chunk in plan {
            file.seek(std::io::SeekFrom::Start(chunk.offset))?;
            let mut data = vec![0; usize::try_from(chunk.size)?];
            file.read_exact(&mut data)?;
            whole.update(&data);
            publication.put(&chunk.digest, &data)?;
        }
        anyhow::ensure!(file.metadata()?.len() == size && hex::encode(whole.finalize()) == digest, "cache source changed during publication");
        publication.write_manifest(&digest, &manifest)?;
        let guard = publication.finish()?;
        Ok(Published { digest, size, guard })
    }

    pub fn put_file(&self, path: &Path) -> anyhow::Result<(String, u64)> {
        let p = self.put_file_guarded(path)?;
        Ok((p.digest, p.size))
    }

    /// Snapshot assembly and acquire exact protection together under the mutation fence.
    pub(crate) fn guard_object(&self, digest: &str) -> anyhow::Result<Option<(Kept, CacheGuard)>> {
        if !is_digest(digest) { return Ok(None); }
        let (kept, guard) = self.chunks.protect_using(|| {
            // Only snapshot assembly here: `kept` may quarantine a corrupt backing file,
            // which must acquire the mutation fence after this closure has released it.
            let whole = self.object_path(digest);
            let kept = if whole.is_file() { Some(Kept::Whole(whole)) }
            else {
                let list = self.read_list(digest);
                list.map(|list| {
                    if let [(d, _)] = list.as_slice() && d == digest { Kept::Whole(self.chunks.path(d)) }
                    else { Kept::Chunks(list) }
                })
            };
            let digests = match &kept {
                Some(Kept::Whole(_)) => vec![digest.to_owned()],
                Some(Kept::Chunks(list)) => std::iter::once(digest.to_owned()).chain(list.iter().map(|(d, _)| d.clone())).collect(),
                None => Vec::new(),
            };
            Ok((kept, digests))
        })?;
        if self.kept(digest).is_none() { return Ok(None); }
        if let Ok(file) = std::fs::File::options().write(true).open(self.list_path(digest)) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        Ok(kept.map(|kept| (kept, guard)))
    }

    /// U07: discard corrupt bytes under physical ownership; never retain an unbudgeted copy.
    fn discard_corrupt_copy(&self, path: &Path, digest: &str) {
        // U07: a shared chunk may have been repaired since the failed read. Recheck and
        // retire its accounting under the same physical fence as publication/eviction.
        if path == self.chunks.path(digest) {
            match self.chunks.discard_corrupt(digest) {
                Ok(true) => tracing::warn!(target: "kiln.work", object = %digest, "a corrupt shared cache chunk was discarded"),
                Ok(false) => {},
                Err(error) => tracing::warn!(target: "kiln.work", object = %digest, error = %format!("{error:#}"), "a corrupt shared cache chunk could not be discarded"),
            }
            return;
        }
        if let Err(error) = self.chunks.discard_legacy_corrupt(path, digest) {
            tracing::warn!(target: "kiln.work", object = %digest, error = %format!("{error:#}"), "corrupt legacy cache bytes could not be discarded");
        }
    }

    fn discard_manifest(&self, digest: &str, list: &[(String, u64)]) {
        if let Err(error) = self.chunks.discard_manifest_if_matches(digest, list) {
            tracing::warn!(target: "kiln.work", object = %digest, error = %format!("{error:#}"), "an invalid cache assembly could not be discarded");
        }
    }

    /// The object's bytes, checked against its name; `None` when it isn't here (or wasn't what
    /// its name says, and has been discarded).
    pub fn read(&self, digest: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let Some((kept, _guard)) = self.guard_object(digest)? else { return Ok(None) };
        self.read_kept(digest, kept, &_guard)
    }

    fn read_kept(&self, digest: &str, kept: Kept, guard: &CacheGuard) -> anyhow::Result<Option<Vec<u8>>> {
        let data = match kept {
            Kept::Whole(path) => {
                if !guard.covers(&self.chunks.dir, digest) { return Ok(None); }
                let Ok(data) = std::fs::read(&path) else { return Ok(None) };
                if hex::encode(Sha256::digest(&data)) != digest {
                    self.discard_corrupt_copy(&path, digest);
                    return Ok(None);
                }
                return Ok(Some(data));
            }
            Kept::Chunks(list) => {
                let mut data = Vec::with_capacity(usize::try_from(list.iter().map(|(_, n)| n).sum::<u64>()).unwrap_or(0));
                for (d, n) in &list {
                    // A chunk is checked as it's read; one that's gone or wrong leaves the object
                    // incomplete, and so missing.
                    let Some(piece) = self.chunks.get_protected(d, guard).filter(|p| p.len() as u64 == *n) else { return Ok(None) };
                    data.extend_from_slice(&piece);
                }
                if hex::encode(Sha256::digest(&data)) != digest {
                    self.discard_manifest(digest, &list);
                    return Ok(None);
                }
                data
            }
        };
        Ok(Some(data))
    }

    /// The object written to `out`, checked; `false` when it isn't here.
    pub fn copy_to(&self, digest: &str, out: &Path) -> anyhow::Result<bool> {
        let Some((kept, _guard)) = self.guard_object(digest)? else { return Ok(false) };
        self.copy_kept(digest, out, kept, &_guard)
    }

    fn copy_kept(&self, digest: &str, out: &Path, kept: Kept, guard: &CacheGuard) -> anyhow::Result<bool> {
        match kept {
            Kept::Whole(path) => {
                if !guard.covers(&self.chunks.dir, digest) { return Ok(false); }
                let mut f = std::fs::File::open(&path)?;
                let mut o = std::fs::File::create(out)?;
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    let piece = buf.get(..n).unwrap_or_default();
                    h.update(piece);
                    o.write_all(piece)?;
                }
                if hex::encode(h.finalize()) != digest {
                    drop(o);
                    let _ = std::fs::remove_file(out);
                    self.discard_corrupt_copy(&path, digest);
                    return Ok(false);
                }
                Ok(true)
            }
            Kept::Chunks(list) => {
                let mut o = std::fs::File::create(out)?;
                let mut h = Sha256::new();
                for (d, n) in &list {
                    let Some(piece) = self.chunks.get_protected(d, guard).filter(|p| p.len() as u64 == *n) else {
                        drop(o);
                        let _ = std::fs::remove_file(out);
                        return Ok(false);
                    };
                    h.update(&piece);
                    o.write_all(&piece)?;
                }
                if hex::encode(h.finalize()) != digest {
                    drop(o);
                    let _ = std::fs::remove_file(out);
                    self.discard_manifest(digest, &list);
                    return Ok(false);
                }
                Ok(true)
            }
        }
    }

    /// A chunk of a large object, checked (for a reader that streams one).
    pub fn chunk(&self, digest: &str) -> Option<Vec<u8>> {
        self.chunks.get(digest)
    }

    pub(crate) fn chunk_protected(&self, digest: &str, guard: &CacheGuard) -> Option<Vec<u8>> {
        self.chunks.get_protected(digest, guard)
    }

    pub(crate) fn read_protected(&self, digest: &str, guard: &CacheGuard) -> anyhow::Result<Option<Vec<u8>>> {
        if !guard.covers(&self.chunks.dir, digest) { return Ok(None); }
        let Some(kept) = self.kept(digest) else { return Ok(None) };
        self.read_kept(digest, kept, guard)
    }

    /// The first of `scopes` with an entry for `name` in `ns`, and the entry: looked up on the
    /// ledger (`crate::ci::work::lookup`). A scope that removed its entry passes to the next.
    pub async fn entry(&self, scopes: &[Scope], ns: &str, name: &str) -> anyhow::Result<Option<(Scope, Entry)>> {
        let (scopes, ns, name) = (scopes.to_vec(), ns.to_owned(), name.to_owned());
        self.ledger()
            .call(move |c| {
                for s in scopes {
                    if let Some(Some(e)) = ledger_entry(c, &s, &ns, &name)? {
                        return Ok(Some((s, e)));
                    }
                }
                Ok(None)
            })
            .await
    }

    /// Sets `name` in `ns` of `scope` to `entry` (the object is held by the scope too), recorded
    /// on the ledger (`crate::ci::work::record`). Whether it changed anything.
    pub async fn set_entry(&self, scope: &Scope, ns: &str, name: &str, entry: Entry) -> anyhow::Result<bool> {
        self.set_entry_inner(scope, ns, name, entry, None).await
    }

    pub(crate) async fn set_entry_guarded(&self, scope: &Scope, ns: &str, name: &str, entry: Entry, guard: CacheGuard) -> anyhow::Result<bool> {
        self.set_entry_inner(scope, ns, name, entry, Some(guard)).await
    }

    async fn set_entry_inner(&self, scope: &Scope, ns: &str, name: &str, entry: Entry, guard: Option<CacheGuard>) -> anyhow::Result<bool> {
        if let Some(g) = &guard { self.hold_async_guarded(scope, &entry.digest, g.clone()).await?; }
        else { self.hold_async(scope, &entry.digest).await?; }
        let (s, ns, name) = (scope.clone(), ns.to_owned(), name.to_owned());
        self.ledger()
            .call(move |c| {
                if let Some(Some(e)) = ledger_entry(c, &s, &ns, &name)?
                    && e.digest == entry.digest
                    && e.meta == entry.meta
                {
                    return Ok((false, guard));
                }
                put_entry(c, &s, &ns, &name, &recorded(Some(&entry), &s.domain, entry.at))?;
                Ok((true, guard))
            })
            .await.map(|(changed, _guard)| changed)
    }

    /// Removes `name` from `ns` of `scope` (recorded as removed: the ledger keeps no gap in a
    /// key's history); whether it was there.
    pub async fn remove_entry(&self, scope: &Scope, ns: &str, name: &str) -> anyhow::Result<bool> {
        let (s, ns, name) = (scope.clone(), ns.to_owned(), name.to_owned());
        self.ledger()
            .call(move |c| {
                if !matches!(ledger_entry(c, &s, &ns, &name)?, Some(Some(_))) {
                    return Ok(false);
                }
                put_entry(c, &s, &ns, &name, &recorded(None, &s.domain, crate::util::now_unix()))?;
                Ok(true)
            })
            .await
    }

    /// The first of `scopes` that holds the object, if it's here.
    pub fn holder(&self, scopes: &[Scope], digest: &str) -> Option<Scope> {
        let found = {
            let index = self.index.read();
            scopes.iter().find(|s| index.held.get(*s).is_some_and(|h| h.contains(digest))).cloned()
        };
        found.filter(|_| self.has(digest))
    }

    /// U05/U07: durable scoped admission without blocking an async runtime shard.
    pub async fn hold_async(&self,scope:&Scope,digest:&str)->anyhow::Result<bool> {
        if self.index.read().held.get(scope).is_some_and(|h|h.contains(digest)) { return Ok(false); }
        let added=self.metadata.hold_async(scope,digest).await?;
        self.index.write().held.entry(scope.clone()).or_default().insert(digest.to_owned());
        Ok(added)
    }

    pub(crate) async fn hold_async_guarded(&self, scope: &Scope, digest: &str, guard: CacheGuard) -> anyhow::Result<bool> {
        let added = self.metadata.hold_async_guarded(scope, digest, guard).await?;
        self.index.write().held.entry(scope.clone()).or_default().insert(digest.to_owned());
        Ok(added)
    }

    /// Records that `scope` holds the object; whether it's new. Blocking callers only; async
    /// consumers use `hold_async` so the durable wait stays off the runtime shard.
    pub fn hold(&self, scope: &Scope, digest: &str) -> anyhow::Result<bool> {
        if self.index.read().held.get(scope).is_some_and(|h| h.contains(digest)) {
            return Ok(false);
        }
        let added=self.metadata.hold(scope,digest)?;
        self.index.write().held.entry(scope.clone()).or_default().insert(digest.to_owned());
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_quiescent(work: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(work.join("_cache"))?;
        std::fs::write(work.join("_cache/.u07-quiescent"), b"U07: all pre-format cache writers are quiescent\n")?;
        Ok(())
    }

    #[test]
    fn u05_unfinished_runner_metadata_import_requires_its_pinned_source() -> anyhow::Result<()> {
        let dir=tempfile::tempdir()?;
        let work=dir.path().join("work");
        let root=work.join("_cas");
        std::fs::create_dir_all(&root)?;
        let host=crate::store::Store::in_memory()?;
        let source=format!("runner-ledger:{}",serde_json::to_string(&("host-a",root.canonicalize()?.join("ledger.db").to_string_lossy()))?);
        host.now(move|c| { c.execute("INSERT INTO runner_work_imports VALUES(?1,'snapshot',?2,0)",crate::sql::params![source,Vec::<u8>::new()])?; Ok(()) })?;
        let error=Store::with_ledger(&work,host.clone(),"host-a").err().ok_or_else(||anyhow::anyhow!("partial ledger was accepted without source"))?;
        assert!(format!("{error:#}").contains("ledger import is incomplete"));
        assert!(Store::with_ledger(&work,host.clone(),"host-b").is_ok(),"another authenticated host's receipt is independent");
        let source=format!("runner-holds:{}",serde_json::to_string(&("host-a",work.canonicalize()?.to_string_lossy()))?);
        host.now(move|c| {
            c.execute("DELETE FROM runner_work_imports",[])?;
            c.execute("INSERT INTO runner_work_imports VALUES(?1,'snapshot',?2,0)",crate::sql::params![source,postcard::to_allocvec(&0u64)?])?;
            Ok(())
        })?;
        let error=Store::with_ledger(&work,host,"host-a").err().ok_or_else(||anyhow::anyhow!("partial holdings were accepted without source"))?;
        assert!(format!("{error:#}").contains("holdings import is incomplete"));
        Ok(())
    }

    #[test]
    fn u05_runner_work_uses_host_database_and_preserves_legacy_scopes_and_audit() -> anyhow::Result<()> {
        let dir=tempfile::tempdir()?;
        let work=dir.path().join("work");
        let root=work.join("_cas");
        std::fs::create_dir_all(&root)?;
        let scope=Scope::new("acme/app","trusted");
        let data=b"legacy scoped object";
        let digest=crate::util::sha256_hex(data);
        let object=root.join("objects").join(digest.get(..2).unwrap_or("")).join(digest.get(2..4).unwrap_or("")).join(&digest);
        std::fs::create_dir_all(object.parent().ok_or_else(||anyhow::anyhow!("object has no parent"))?)?;
        std::fs::write(object,data)?;
        let mut journal=serde_json::to_vec(&Line::Hold{scope:scope.clone(),digest:digest.clone()})?;
        journal.extend_from_slice(b"\n{partial crash tail");
        std::fs::write(root.join("index.log"),&journal)?;
        let source=rusqlite::Connection::open(root.join("ledger.db"))?;
        source.execute_batch("CREATE TABLE work_records(family TEXT,key TEXT,value BLOB,at INTEGER,PRIMARY KEY(family,key)) WITHOUT ROWID; CREATE TABLE audit2(id INTEGER PRIMARY KEY,ts INTEGER,principal TEXT,action TEXT,result TEXT,detail TEXT); INSERT INTO audit2 VALUES(7,3,'runner','work.verify','refused','legacy-ledger-audit');")?;
        source.execute("INSERT INTO work_records VALUES('work.verify','acme/app|trusted|proof',?1,9)",[b"opaque verification".as_slice()])?;
        drop(source);
        legacy_quiescent(&work)?;
        let host=crate::store::Store::open(&dir.path().join("state/kiln.db"))?;
        {
            let store=Store::with_ledger(&work,host.clone(),"host-a")?;
            assert_eq!(store.holder(std::slice::from_ref(&scope),&digest),Some(scope.clone()));
            assert!(store.holder(&[Scope::new("other/repo","trusted")],&digest).is_none());
            let (new,_)=store.put_bytes(b"new scoped object")?;
            assert!(store.hold(&scope,&new)?);
            assert!(!store.hold(&scope,&new)?);
        }
        let store=Store::with_ledger(&work,host.clone(),"host-a")?;
        let new=crate::util::sha256_hex(b"new scoped object");
        assert_eq!(store.holder(std::slice::from_ref(&scope),&new),Some(scope));
        host.now(|c| {
            assert_eq!(c.query_row("SELECT value FROM work_records WHERE family='work.verify' AND key='acme/app|trusted|proof'",[],|r|r.get::<_,Vec<u8>>(0))?,b"opaque verification");
            assert_eq!(c.query_row("SELECT count(*) FROM audit2 WHERE detail='legacy-ledger-audit'",[],|r|r.get::<_,i64>(0))?,1);
            assert_eq!(c.query_row("SELECT count(*) FROM runner_work_holds",[],|r|r.get::<_,i64>(0))?,2);
            Ok(())
        })?;
        let other=Store::with_ledger(&work,host,"host-b")?;
        assert!(other.holder(&[Scope::new("acme/app","trusted")],&new).is_none(),"matching local paths on different authenticated hosts do not share scoped holdings");
        assert_eq!(std::fs::read(root.join("index.log"))?,journal,"retired journal is retained unchanged");
        assert!(!root.join(crate::store::FILE).exists());
        assert!(!work.join(crate::store::FILE).exists(),"injection creates one database under host state");
        Ok(())
    }

    /// U07/W08-R09: legacy data is imported only offline; authorization is independent
    /// of pressure-evictable residency, including current names and historical holdings.
    #[test]
    fn u07_shared_chunks_import_and_idle_holdings_are_evictable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let whole = b"legacy whole object";
        let digest = crate::util::sha256_hex(whole);
        let legacy = dir.path().join("_cas/objects").join(digest.get(..2).unwrap_or("")).join(digest.get(2..4).unwrap_or("")).join(&digest);
        std::fs::create_dir_all(legacy.parent().context("legacy parent")?)?;
        std::fs::write(&legacy, whole)?;
        let host = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        assert!(Store::with_ledger(dir.path(), host.clone(), "standalone").is_err());
        legacy_quiescent(dir.path())?;
        let store = Store::with_ledger_max(dir.path(), host, "standalone", Some(whole.len() as u64 + serde_json::to_vec(&vec![(digest.clone(), whole.len() as u64)])?.len() as u64))?;
        let scope = Scope::new("acme/app", "trusted");
        assert_eq!(store.read(&digest)?.as_deref(), Some(whole.as_slice()));
        assert!(!legacy.exists());
        store.hold(&scope, &digest)?;
        let held = store.guard_object(&digest)?.context("guard imported object")?;
        let other = vec![7; whole.len()];
        let error = store.put_bytes_guarded(&other).err().context("active object unexpectedly evicted")?;
        assert!(Local::declined(&error));
        assert_eq!(store.read_protected(&digest, &held.1)?.as_deref(), Some(whole.as_slice()));
        drop(held);
        let replacement = store.put_bytes_guarded(&other)?;
        assert!(store.holder(std::slice::from_ref(&scope), &digest).is_none(), "authorization permits a physical miss");
        assert_eq!(store.read_protected(&replacement.digest, &replacement.guard)?.as_deref(), Some(other.as_slice()));
        Ok(())
    }

    #[test]
    fn u07_oversized_legacy_whole_source_is_refused_before_unbounded_read() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let digest = "a".repeat(64);
        let path = dir.path().join("_cas/objects/aa/aa").join(digest);
        std::fs::create_dir_all(path.parent().context("legacy parent")?)?;
        std::fs::File::create(&path)?.set_len(WHOLE_MAX + 1)?;
        legacy_quiescent(dir.path())?;
        let error = Store::open(dir.path()).err().context("oversized legacy whole source was accepted")?;
        assert!(format!("{error:#}").contains("legacy whole cache object exceeds"));
        assert!(path.exists(), "invalid-source refusal does not silently discard evidence");
        Ok(())
    }

    #[test]
    fn u07_zero_budget_legacy_import_is_ready_without_retaining_bytes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let bytes = b"idle legacy cache";
        let digest = crate::util::sha256_hex(bytes);
        let whole = dir.path().join("_cas/objects").join(digest.get(..2).unwrap_or("")).join(digest.get(2..4).unwrap_or("")).join(&digest);
        let chunk_bytes = b"idle legacy chunk";
        let chunk_digest = crate::util::sha256_hex(chunk_bytes);
        let chunk = dir.path().join("_cas/chunks").join(chunk_digest.get(..2).unwrap_or("")).join(chunk_digest.get(2..4).unwrap_or("")).join(&chunk_digest);
        std::fs::create_dir_all(whole.parent().context("legacy object parent")?)?;
        std::fs::create_dir_all(chunk.parent().context("legacy chunk parent")?)?;
        std::fs::write(&whole, bytes)?;
        std::fs::write(&chunk, chunk_bytes)?;
        let scope = Scope::new("acme/app", "trusted");
        std::fs::write(dir.path().join("_cas/index.log"), serde_json::to_vec(&Line::Hold { scope: scope.clone(), digest: digest.clone() })?)?;
        legacy_quiescent(dir.path())?;
        let host = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        let store = Store::with_ledger_max(dir.path(), host.clone(), "standalone", Some(0))?;
        assert!(!whole.exists() && !chunk.exists());
        assert!(!store.has(&digest) && !store.has(&chunk_digest));
        assert!(store.holder(std::slice::from_ref(&scope), &digest).is_none());
        host.now(|c| {
            assert_eq!(c.query_row("SELECT count(*) FROM runner_work_holds", [], |r| r.get::<_, i64>(0))?, 1, "authorization history remains canonical despite a physical miss");
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn u07_current_entries_and_history_do_not_promise_residency() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let host = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        let manifest_size = serde_json::to_vec(&vec![(crate::util::sha256_hex(b"version1"), 8u64)])?.len() as u64;
        let store = Store::with_ledger_max(dir.path(), host, "standalone", Some(8 + manifest_size))?;
        let scope = Scope::new("acme/app", "trusted");
        crate::rt::block_on(async move {
            let first = store.put_bytes_guarded(b"version1")?;
            let old = first.digest.clone();
            store.set_entry_guarded(&scope, "tool", "key", Entry { digest: old.clone(), size: 8, at: 1, meta: BTreeMap::new() }, first.guard.clone()).await?;
            drop(first);
            let second = store.put_bytes_guarded(b"version2")?;
            assert!(!store.has(&old), "a current name does not pin idle bytes");
            store.set_entry_guarded(&scope, "tool", "key", Entry { digest: second.digest.clone(), size: 8, at: 2, meta: BTreeMap::new() }, second.guard.clone()).await?;
            let historical = second.digest.clone();
            store.remove_entry(&scope, "tool", "key").await?;
            drop(second);
            let third = store.put_bytes_guarded(b"version3")?;
            assert!(!store.has(&historical), "a removed entry's history does not pin bytes");
            assert!(store.holder(std::slice::from_ref(&scope), &historical).is_none());
            assert_eq!(store.read_protected(&third.digest, &third.guard)?.as_deref(), Some(b"version3".as_slice()));
            anyhow::Ok(())
        })
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

    #[test]
    fn u07_guarded_chunked_reads_reuse_operation_at_full_slot_capacity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        // ctl-10: an unrelated actual hit succeeds BEFORE the open publication finishes.
        let (hit, _) = store.put_bytes(b"already published Go output")?;
        let bytes = b"another object still being published";
        let digest = crate::util::sha256_hex(bytes);
        let mut open = store.local().reserve(&[(digest.clone(), bytes.len() as u64)])?;
        assert_eq!(store.read(&hit)?.as_deref(), Some(b"already published Go output".as_slice()));
        open.put(&digest, bytes)?;
        assert_eq!(store.read(&hit)?.as_deref(), Some(b"already published Go output".as_slice()));
        drop(open.finish()?);
        let data = noise(WHOLE_MAX as usize + 1, 11);
        let publication = store.put_bytes_guarded(&data)?;
        assert!(matches!(store.kept(&publication.digest), Some(Kept::Chunks(_))));
        let local = store.local();
        let mut others = Vec::new();
        for _ in 1..64 { others.push(local.guard(&[])?); }
        assert!(local.guard(&[]).is_err(), "all operation slots are owned");
        let wrong = others.first().context("missing empty operation guard")?;
        assert!(store.read_protected(&publication.digest, wrong)?.is_none());
        let kept = store.kept(&publication.digest).context("protected assembly is missing")?;
        assert!(!store.copy_kept(&publication.digest, &dir.path().join("wrong-copy"), kept.clone(), wrong)?);
        assert_eq!(store.read_protected(&publication.digest, &publication.guard)?.as_deref(), Some(data.as_slice()));
        let out = dir.path().join("copy");
        assert!(store.copy_kept(&publication.digest, &out, kept, &publication.guard)?);
        assert_eq!(std::fs::read(out)?, data);
        drop(others);
        Ok(())
    }

    /// W24-R08: small objects are kept whole and large ones in the store's chunks; either reads
    /// back as it went in, and two versions of a large object share their unchanged chunks.
    #[test]
    fn w24_r08_objects_whole_and_chunked() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        let small = noise(300 << 10, 1);
        let (d, n) = store.put_bytes(&small)?;
        assert_eq!((d.clone(), n), (crate::util::sha256_hex(&small), small.len() as u64));
        assert!(matches!(store.kept(&d), Some(Kept::Whole(_))));
        assert_eq!(store.read(&d)?.as_deref(), Some(small.as_slice()));
        // Large: cut into chunks as the server cuts them, from a file, never whole in memory.
        let big = noise(20 << 20, 2);
        let f = dir.path().join("big");
        std::fs::write(&f, &big)?;
        let (bd, bn) = store.put_file(&f)?;
        assert_eq!((bd.clone(), bn), (crate::util::sha256_hex(&big), big.len() as u64));
        let Some(Kept::Chunks(list)) = store.kept(&bd) else { anyhow::bail!("a large object is kept in chunks") };
        assert_eq!(list.iter().map(|(_, n)| *n as usize).collect::<Vec<_>>(), cdc::cuts(&big));
        assert!(store.read(&bd)?.as_deref() == Some(big.as_slice()));
        let out = dir.path().join("out");
        assert!(store.copy_to(&bd, &out)?);
        assert!(std::fs::read(&out)? == big);
        // A second version, one edit in the middle: most chunks are shared.
        let mut edited = big.clone();
        edited.splice(10 << 20..10 << 20, b"an edit".iter().copied());
        let (ed, _) = store.put_bytes(&edited)?;
        let Some(Kept::Chunks(list2)) = store.kept(&ed) else { anyhow::bail!("chunked") };
        let shared = list2.iter().filter(|c| list.contains(c)).count();
        assert!(shared + 3 >= list.len(), "{shared} of {} chunks shared", list.len());
        // Bytes put again are the same object.
        assert_eq!(store.put_bytes(&small)?.0, d);
        assert!(store.read(&"0".repeat(64))?.is_none() && !store.has("not a digest"));
        Ok(())
    }

    /// W24-R08: a copy that isn't what its name says is never served: it's discarded and reads as
    /// missing, so the next put keeps it again.
    #[test]
    fn w24_r08_a_corrupted_copy_is_refused() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path())?;
        let data = noise(64 << 10, 3);
        let (d, _) = store.put_bytes(&data)?;
        let Some(Kept::Whole(path)) = store.kept(&d) else { anyhow::bail!("whole") };
        std::fs::write(&path, b"poison")?;
        assert!(store.read(&d)?.is_none());
        assert!(!store.has(&d) && !path.exists(), "corrupt bytes are discarded, never retained outside the budget");
        store.put_bytes(&data)?;
        assert_eq!(store.read(&d)?.as_deref(), Some(data.as_slice()));
        Ok(())
    }

    /// W24-R08, R30: entries are per scope, on the work ledger, and survive the store opening
    /// again; so do holdings. A removed entry lets the next scope's show.
    #[test]
    fn w24_r08_entries_and_holdings_are_per_scope_and_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (a, b, t) = (Scope::new("acme/app", "untrusted"), Scope::new("acme/other", "trusted"), Scope::new("acme/app", "trusted"));
            let digest = {
                let store = Store::open(&root)?;
                let (d, n) = store.put_bytes(b"compiled")?;
                let e = Entry { digest: d.clone(), size: n, at: 1, meta: BTreeMap::from([("x-artifact-tag".to_owned(), "sig".to_owned())]) };
                anyhow::ensure!(store.set_entry(&a, "sccache", "k1", e.clone()).await?);
                anyhow::ensure!(!store.set_entry(&a, "sccache", "k1", e.clone()).await?, "the same again changes nothing");
                anyhow::ensure!(store.entry(std::slice::from_ref(&a), "sccache", "k1").await?.map(|(_, e)| e.digest) == Some(d.clone()));
                anyhow::ensure!(store.entry(std::slice::from_ref(&b), "sccache", "k1").await?.is_none());
                anyhow::ensure!(store.entry(std::slice::from_ref(&t), "sccache", "k1").await?.is_none(), "another domain's ledger holds nothing of it");
                anyhow::ensure!(store.holder(&[b.clone(), a.clone()], &d) == Some(a.clone()));
                anyhow::ensure!(store.holder(std::slice::from_ref(&b), &d).is_none(), "another repository's object isn't its to know of");
                // Removed from its own scope, the trusted entry under it shows.
                let (td, tn) = store.put_bytes(b"trusted build")?;
                store.set_entry(&t, "sccache", "gone", Entry { digest: td.clone(), size: tn, at: 2, meta: BTreeMap::new() }).await?;
                store.set_entry(&a, "sccache", "gone", e).await?;
                anyhow::ensure!(store.entry(&[a.clone(), t.clone()], "sccache", "gone").await?.map(|(s, _)| s) == Some(a.clone()));
                anyhow::ensure!(store.remove_entry(&a, "sccache", "gone").await? && !store.remove_entry(&a, "sccache", "gone").await?);
                anyhow::ensure!(store.entry(&[a.clone(), t.clone()], "sccache", "gone").await?.map(|(s, e)| (s, e.digest)) == Some((t.clone(), td)));
                // On the ledger: a unit whose definition is the tool's key.
                let at = format!("{}{}", a.key(), entry_key("sccache", "k1", "untrusted").hex());
                anyhow::ensure!(store.ledger().call(move |c| Ok(crate::ci::work::get(c, crate::ci::work::Family::Entry, &at)?)).await?.is_some(), "recorded on the ledger");
                d
            };
            let store = Store::open(&root)?;
            let (scope, e) = store.entry(&[b.clone(), a.clone()], "sccache", "k1").await?.ok_or_else(|| anyhow::anyhow!("kept across opening"))?;
            anyhow::ensure!((scope, e.digest.as_str(), e.meta.get("x-artifact-tag").map(String::as_str), e.at) == (a.clone(), digest.as_str(), Some("sig"), 1));
            anyhow::ensure!(store.entry(std::slice::from_ref(&a), "sccache", "gone").await?.is_none());
            anyhow::ensure!(store.holder(std::slice::from_ref(&a), &digest) == Some(a));
            anyhow::ensure!(entry_key("sccache", "k1", "trusted") != entry_key("sccache", "k1", "untrusted") && entry_key("go", "k1", "trusted") != entry_key("sccache", "k1", "trusted"));
            Ok(())
        })
    }
}
