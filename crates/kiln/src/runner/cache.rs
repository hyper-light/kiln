//! `kiln/cache@v1` (docs/ci.md W08-R09): the job's caches through kiln's chunked protocol
//! (`/_kiln/cache`, `crate::ci::services::chunks`). The paths are kept in one uncompressed tar,
//! each under its index (`0/…`, `1/…`: no absolute path is ever in it), cut into the store's
//! content-defined chunks, so a cache that changed by 1 % moves about 1 % of its bytes. With
//! `local`, the runner keeps the chunks it has seen in `<work>/_cache` (least recently used go
//! first past `[agent] cache_max`, 10 GiB by default): a restore downloads only those it lacks,
//! and a save uploads only those the server's copy of the repository's caches lacks.
//!
//! U07: the budget charges unique chunks, derived object assemblies and owned publication /
//! repair staging. Input transport staging has its existing separate 16 GiB request bound.
//! Exact native host files share this inventory and reserve finite in-process staging; their
//! handles expose leased host reads, never external process paths. Generated/VMM output
//! bounds and Execution-backed residency remain unsupported by this host-only API.
//! Fixed physical guard metadata is bounded to 64 slots of at most 65,536 digests each; slot
//! exhaustion is an explicit admission refusal. Reads need no SQL durability round trip.
//! Legacy activation is offline: all pre-format writers must be quiescent before the operator
//! writes `_cache/.u07-quiescent` with `U07: all pre-format cache writers are quiescent\n`.
//! This code neither establishes that operational proof nor restarts a runner.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Instant, SystemTime};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kiln_ci::store::cdc::{self, Cdc};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::http::client::Client;

/// Where a leg reaches kiln's cache, and as whom.
pub struct Service<'a> {
    pub http: &'a Client,
    /// The results service's origin.
    pub base: String,
    pub token: &'a str,
}

/// What `kiln/cache@v1` keeps: its paths, as given and as found on this machine.
#[derive(Debug, Clone)]
pub struct Paths {
    pub given: Vec<String>,
    pub resolved: Vec<PathBuf>,
}

impl Paths {
    /// `~` is the home directory; a relative path is the workspace's.
    pub fn new(given: &[String], workspace: &Path, home: Option<&Path>) -> Paths {
        let resolved = given
            .iter()
            .map(|p| match (p.strip_prefix("~/"), home) {
                (Some(rest), Some(h)) => h.join(rest),
                _ if p == "~" => home.map(Path::to_path_buf).unwrap_or_else(|| workspace.to_path_buf()),
                _ if Path::new(p).is_absolute() => PathBuf::from(p),
                _ => workspace.join(p),
            })
            .collect();
        Paths { given: given.to_vec(), resolved }
    }

    /// The cache's version: the paths it keeps, in kiln's format (never mistaken for
    /// `actions/cache`'s).
    pub fn version(&self) -> String {
        hex::encode(Sha256::digest(format!("kiln/cache v1\n{}", self.given.join("\n")).as_bytes()))
    }
}

/// One chunk of a cache's content.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub digest: String,
    pub size: u64,
    pub offset: u64,
}

mod accounting;
mod guards;
mod native;

pub use native::{NativeFile, NativePlan, NativePublication, NativeReader};

pub use guards::Guard as CacheGuard;

/// U07 / W08-R09: one bounded physical tier. Scoped holdings authorize access, never promise
/// permanent residency. Only OS-owned operations protect chunks from the common LRU.
#[derive(Clone)]
pub struct Local {
    pub dir: PathBuf,
    state: Arc<parking_lot::Mutex<Tier>>,
}

// The same weak handle registry also resolves the initialized owner through work aliases.
static TIERS: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Weak<parking_lot::Mutex<Tier>>>>> = OnceLock::new();

struct Tier {
    // Every alias retains one spelling for exact Guard/Mutation ownership comparisons.
    dir: PathBuf,
    pending_max: Option<u64>,
    accounting: Option<accounting::Accounting>,
    maintained: Option<Instant>,
}

impl Local {
    /// A lightweight handle; writes require `with_ledger` to have installed the host owner.
    /// Opening a handle never creates a database or turns an old pin into a live reference.
    pub fn new(work: &Path, max: Option<u64>) -> Local {
        // Resolve existing work aliases before a shared handle can escape. Missing work keeps
        // its original spelling until initialization can bind it without replacing that owner.
        let spelled = work.join("_cache");
        let dir = work.canonicalize().map(|work| work.join("_cache")).unwrap_or_else(|_| spelled.clone());
        let mut tiers = TIERS.get_or_init(Default::default).lock();
        // A handle returned while work was absent cannot silently move to a different owner
        // after it appears. with_ledger diagnoses conflicting live canonical/raw owners.
        let state = match tiers.get(&spelled).and_then(Weak::upgrade).or_else(|| tiers.get(&dir).and_then(Weak::upgrade)) {
            Some(state) => state,
            None => {
                tiers.retain(|_, value| value.strong_count() > 0);
                let state = Arc::new(parking_lot::Mutex::new(Tier { dir: dir.clone(), pending_max: None, accounting: None, maintained: None }));
                tiers.insert(dir.clone(), Arc::downgrade(&state));
                state
            }
        };
        if tiers.get(&dir).and_then(Weak::upgrade).is_none() { tiers.insert(dir, Arc::downgrade(&state)); }
        drop(tiers);
        let dir = {
            let mut owner = state.lock();
            if max.is_some() { owner.pending_max = max; }
            owner.dir.clone()
        };
        Local { dir, state }
    }

    pub(crate) fn with_ledger(work: &Path, ledger: crate::store::Store, max: Option<u64>) -> anyhow::Result<Self> {
        let local = Self::new(work, max);
        std::fs::create_dir_all(&local.dir)?;
        // Canonicalize the work owner, preserving _cache's relationship to sibling _cas
        // metadata even when the cache directory itself is a configured filesystem link.
        let canonical = work.canonicalize()?.join("_cache");
        {
            let mut tiers = TIERS.get_or_init(Default::default).lock();
            if let Some(state) = tiers.get(&canonical).and_then(Weak::upgrade) {
                anyhow::ensure!(Arc::ptr_eq(&state, &local.state), "U07 cache work aliases resolved to conflicting live owners; drop the uninitialized alias handles and reopen through the initialized canonical work directory");
            }
            tiers.insert(canonical, Arc::downgrade(&local.state));
            tiers.insert(local.dir.clone(), Arc::downgrade(&local.state));
        }
        let fence = Arc::new(guards::Mutation::acquire(&local.dir)?);
        let policy = {
            let mut state = local.state.lock();
            if max.is_some() { state.pending_max = max; }
            state.pending_max.take()
        };
        let accounting = match accounting::Accounting::open(&local.dir, ledger, policy, fence.clone()) {
            Ok(accounting) => accounting,
            Err(error) => {
                let mut state = local.state.lock();
                if state.pending_max.is_none() { state.pending_max = policy; }
                return Err(error);
            }
        };
        local.state.lock().accounting = Some(accounting);
        Ok(local)
    }

    /// Resolve only an existing initialized host owner. This creates neither a tier nor a
    /// database; relative/symlink work aliases share the same existing weak-handle registry.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn initialized(work: &Path) -> anyhow::Result<Self> {
        let dir = work.canonicalize()?.join("_cache");
        let state = TIERS.get().and_then(|tiers| tiers.lock().get(&dir).and_then(Weak::upgrade))
            .ok_or_else(|| anyhow::anyhow!("U07 native input requires this work directory's initialized host cache owner"))?;
        let dir = {
            let owner = state.lock();
            anyhow::ensure!(owner.accounting.is_some(), "U07 native input cache initialization is incomplete; wait for the existing host owner");
            owner.dir.clone()
        };
        Ok(Self { dir, state })
    }

    fn owner(&self, fence: Arc<guards::Mutation>) -> anyhow::Result<accounting::Accounting> {
        let (owner, max) = {
            let mut state = self.state.lock();
            (state.accounting.clone(), state.pending_max.take())
        };
        let owner = owner.ok_or_else(|| anyhow::anyhow!("U07 cache has no initialized host accounting owner"))?;
        if let Some(max) = max && let Err(error) = owner.set_max(max, fence) {
            self.state.lock().pending_max = Some(max);
            return Err(error);
        }
        Ok(owner)
    }

    pub(crate) fn path(&self, digest: &str) -> PathBuf {
        self.dir.join(digest.get(0..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest)
    }

    fn valid(digest: &str) -> bool {
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    /// U07: no SQL round trip on a local hit. Guard only until verified owned bytes exist.
    pub(crate) fn get(&self, digest: &str) -> Option<Vec<u8>> {
        if !Self::valid(digest) { return None; }
        let guard = self.guard(&[digest.to_owned()]).ok()?;
        self.get_protected(digest, &guard)
    }

    /// Nested reads reuse their admitted operation, including at the 64-operation limit.
    pub(crate) fn get_protected(&self, digest: &str, guard: &CacheGuard) -> Option<Vec<u8>> {
        if !Self::valid(digest) || !guard.covers(&self.dir, digest) { return None; }
        let path = self.path(digest);
        if path.metadata().ok()?.len() > cdc::MAX as u64 { return None; }
        let data = std::fs::read(&path).ok()?;
        if hex::encode(Sha256::digest(&data)) != digest { return None; }
        if let Ok(file) = std::fs::File::options().write(true).open(path) { let _ = file.set_modified(SystemTime::now()); }
        Some(data)
    }

    /// Existence is a hint, not a retention promise; actual readers acquire an operation guard.
    pub(crate) fn contains(&self, digest: &str, size: u64) -> bool {
        Self::valid(digest) && self.path(digest).metadata().is_ok_and(|m| m.is_file() && m.len() == size)
    }

    pub(crate) fn guard(&self, digests: &[String]) -> std::io::Result<CacheGuard> {
        let fence = guards::Mutation::acquire(&self.dir)?;
        guards::protect(&self.dir, &fence, digests)
    }

    /// Snapshot assembly and acquire protection atomically with respect to eviction.
    pub(crate) fn protect_using<T>(&self, snapshot: impl FnOnce() -> std::io::Result<(T, Vec<String>)>) -> std::io::Result<(T, CacheGuard)> {
        let fence = guards::Mutation::acquire(&self.dir)?;
        let (value, digests) = snapshot()?;
        let guard = guards::protect(&self.dir, &fence, &digests)?;
        Ok((value, guard))
    }

    /// Reserve the complete unique footprint before the first chunk, including repair peak.
    pub(crate) fn reserve(&self, want: &[(String, u64)]) -> anyhow::Result<Publication> {
        self.reserve_plan(want, None)
    }

    /// Assembly is bounded physical metadata, charged and retired by the same LRU as chunks.
    pub(crate) fn reserve_object(&self, want: &[(String, u64)], digest: &str, manifest: &[u8]) -> anyhow::Result<Publication> {
        anyhow::ensure!(Self::valid(digest), "invalid U07 object digest");
        anyhow::ensure!(want.len() < 65_536 && manifest.len() <= 8 << 20, "U07 assembly exceeds the physical metadata bound");
        let parsed: Vec<(String, u64)> = serde_json::from_slice(manifest)?;
        anyhow::ensure!(parsed == want, "assembly differs from admitted chunk plan");
        let mut plan = want.to_vec();
        plan.push((format!("manifest:{digest}"), u64::try_from(manifest.len())?));
        self.reserve_plan(&plan, Some((digest.to_owned(), manifest.to_vec())))
    }

    fn reserve_plan(&self, want: &[(String, u64)], manifest: Option<(String, Vec<u8>)>) -> anyhow::Result<Publication> {
        anyhow::ensure!(want.len() <= 65_536, "U07 object has too many physical parts");
        let mut unique = std::collections::BTreeMap::new();
        let mut names = HashSet::new();
        for (key, size) in want {
            let digest = key.strip_prefix("manifest:").unwrap_or(key);
            anyhow::ensure!(Self::valid(digest), "invalid U07 physical part digest");
            anyhow::ensure!(if key.starts_with("manifest:") { *size <= 8 << 20 } else { *size <= cdc::MAX as u64 }, "U07 physical part exceeds its format bound");
            if let Some(old) = unique.insert(key.clone(), *size) { anyhow::ensure!(old == *size, "conflicting sizes for U07 physical part"); }
            names.insert(digest.to_owned());
        }
        let id = crate::util::new_id();
        let (owner, guard) = {
            let fence = Arc::new(guards::Mutation::acquire(&self.dir)?);
            let owner = self.owner(fence.clone())?;
            let guard = guards::protect_cdc(&self.dir, &fence, &names.into_iter().collect::<Vec<_>>(), &id)?;
            (owner, guard)
        };
        // ctl-10: exact protection precedes verification, but no whole-object fence does.
        // Every owned PRESENT replacement rechecks other guards under Mutation.
        let mut observed = HashMap::new();
        let mut write_parts = HashSet::new();
        let mut repair = HashSet::new();
        for (key, expected_size) in &unique {
            let path = accounting::physical_path(&self.dir, key)?;
            let present = physical_size(&path)?;
            observed.insert(key.clone(), present);
            let valid = if present == Some(*expected_size) {
                let data = std::fs::read(&path)?;
                match key.strip_prefix("manifest:") {
                    Some(name) => manifest.as_ref().is_some_and(|(digest, expected)| digest == name && *expected == data),
                    None => hex::encode(Sha256::digest(&data)) == *key,
                }
            } else { false };
            if !valid {
                write_parts.insert(key.clone());
                if present.is_some() { repair.insert(key.clone()); }
            }
        }
        let fence = Arc::new(guards::Mutation::acquire(&self.dir)?);
        for (key, previous) in &observed {
            if physical_size(&accounting::physical_path(&self.dir, key)?)? != *previous { return Err(publication_changed()); }
        }
        let protected = guards::activity(&self.dir, &fence, Some(&guard))?.digests;
        if repair.iter().any(|key| protected.contains(key.strip_prefix("manifest:").unwrap_or(key))) { return Err(publication_changed()); }
        let reservation = owner.prepare_live(&self.dir, want, &repair, &protected, (id, guard.clone()), fence)?;
        let pending = write_parts.clone();
        Ok(Publication { local: self.clone(), owner, reservation, guard, manifest, observed, write_parts, pending })
    }

    fn put_accounted(&self, digest: &str, data: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(Self::valid(digest) && hex::encode(Sha256::digest(data)) == digest, "chunk bytes do not match their digest");
        let mut publication = self.reserve(&[(digest.to_owned(), u64::try_from(data.len())?)])?;
        publication.put(digest, data)?;
        drop(publication.finish()?);
        Ok(())
    }

    fn put_optional(&self, digest: &str, data: &[u8]) -> anyhow::Result<()> {
        match self.put_accounted(digest, data) {
            Err(error) if Self::declined(&error) || error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock && e.raw_os_error().is_none()) => Ok(()),
            result => result,
        }
    }

    /// Only explicit cache admission declines are optional; ordinary corruption/IO/metadata
    /// failures remain errors. Physical Busy is bounded by its caller's existing retry policy.
    pub(crate) fn declined(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| cause.is::<accounting::Capacity>() || cause.is::<accounting::MaintenancePending>() || cause.is::<guards::GuardCapacity>()
            || cause.downcast_ref::<std::io::Error>().and_then(std::io::Error::get_ref).is_some_and(|inner| inner.is::<accounting::Capacity>() || inner.is::<accounting::MaintenancePending>() || inner.is::<guards::GuardCapacity>()))
    }

    /// Legacy migration is offline and restartable; absent capacity leaves source bytes intact.
    pub(crate) fn import(&self, old: &Path) -> anyhow::Result<()> {
        let legacy = files(old);
        if !legacy.is_empty() {
            anyhow::ensure!(std::fs::read_to_string(self.dir.join(".u07-quiescent")).is_ok_and(|value| value == "U07: all pre-format cache writers are quiescent\n"), "U07 legacy import requires verified old-writer quiescence");
        }
        for file in legacy {
            let Some(digest) = file.file_name().and_then(|name| name.to_str()).filter(|digest| Self::valid(digest)) else { continue };
            let source = match std::fs::File::open(&file) {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let mut data = Vec::new();
            source.take(cdc::MAX as u64 + 1).read_to_end(&mut data)?;
            anyhow::ensure!(data.len() <= cdc::MAX, "legacy cache chunk exceeds its format bound");
            if hex::encode(Sha256::digest(&data)) != digest {
                self.discard_legacy_corrupt(&file, digest)?;
                continue;
            }
            match self.put_accounted(digest, &data) {
                Err(error) if Self::declined(&error) => { self.discard_legacy_idle(&file, digest)?; }
                Err(error) => return Err(error),
                Ok(()) => { self.discard_legacy_idle(&file, digest)?; }
            }
        }
        Ok(())
    }

    /// U07: discard only freshly verified corrupt bytes, under the mutation fence. Historical
    /// diagnostic byte copies are not retained outside the common budget.
    pub(crate) fn discard_corrupt(&self, digest: &str) -> anyhow::Result<bool> {
        anyhow::ensure!(Self::valid(digest), "invalid U07 corrupt digest");
        native::discard_corrupt(self, digest)
    }

    pub(crate) fn discard_manifest_if_matches(&self, digest: &str, expected: &[(String, u64)]) -> anyhow::Result<bool> {
        anyhow::ensure!(Self::valid(digest), "invalid U07 corrupt assembly digest");
        let fence = Arc::new(guards::Mutation::acquire(&self.dir)?);
        let owner = self.owner(fence.clone())?;
        let key = format!("manifest:{digest}");
        let file = match std::fs::File::open(accounting::physical_path(&self.dir, &key)?) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => { owner.forget(&self.dir, &key, fence)?; return Ok(false); }
            Err(error) => return Err(error.into()),
        };
        let mut data = Vec::new();
        file.take((8 << 20) + 1).read_to_end(&mut data)?;
        anyhow::ensure!(data.len() <= 8 << 20, "oversized U07 assembly cannot be compared safely");
        let current: Vec<(String, u64)> = serde_json::from_slice(&data)?;
        if current != expected || guards::protected(&self.dir, &fence)?.contains(digest) { return Ok(false); }
        owner.discard(&self.dir, &key, fence)?;
        tracing::warn!(target: "kiln.work", object = digest, "U07 invalid cache assembly discarded after fenced comparison");
        Ok(true)
    }

    pub(crate) fn discard_legacy_corrupt(&self, path: &Path, digest: &str) -> anyhow::Result<bool> {
        self.discard_legacy(path, digest, true)
    }

    pub(crate) fn discard_legacy_idle(&self, path: &Path, digest: &str) -> anyhow::Result<bool> {
        self.discard_legacy(path, digest, false)
    }

    fn discard_legacy(&self, path: &Path, digest: &str, corrupt_only: bool) -> anyhow::Result<bool> {
        anyhow::ensure!(Self::valid(digest), "invalid legacy cache digest");
        let _fence = guards::Mutation::acquire(&self.dir)?;
        anyhow::ensure!(std::fs::read_to_string(self.dir.join(".u07-quiescent")).is_ok_and(|value| value == "U07: all pre-format cache writers are quiescent\n"), "legacy cache retirement requires verified offline quiescence");
        let work = self.dir.parent().ok_or_else(|| anyhow::anyhow!("cache root has no parent"))?;
        let suffix = Path::new(digest.get(..2).unwrap_or("00")).join(digest.get(2..4).unwrap_or("00")).join(digest);
        anyhow::ensure!(path == work.join("_cas/objects").join(&suffix) || path == work.join("_cas/chunks").join(&suffix), "legacy cache retirement path is outside the old physical tier");
        let bytes = match path.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if corrupt_only && crate::sync::blobs::hash_file(path)? == digest { return Ok(false); }
        std::fs::remove_file(path)?;
        if let Some(parent) = path.parent() { crate::blobs::chunks::sync_dir(parent)?; }
        tracing::warn!(target: "kiln.work", object = digest, bytes, "U07 idle or corrupt legacy cache bytes discarded during offline import");
        Ok(true)
    }

    pub(crate) fn maintain(&self) {
        let due = {
            let mut state = self.state.lock();
            if state.maintained.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) { false }
            else { state.maintained = Some(Instant::now()); true }
        };
        if due { self.trim(); }
    }

    pub fn trim(&self) {
        if let Err(error) = self.trim_checked() { tracing::debug!(target: "kiln.work", error = %format!("{error:#}"), "U07 cache maintenance deferred"); }
    }

    pub(crate) fn trim_checked(&self) -> anyhow::Result<()> {
        let fence = Arc::new(guards::Mutation::acquire(&self.dir)?);
        let owner = self.owner(fence.clone())?;
        let protected = guards::protected(&self.dir, &fence)?;
        owner.trim(&self.dir, &protected, fence)
    }
}

/// U07/ctl-10: one exact intent slot owns the whole CDC publication. Mutation protects
/// only finite metadata/publication sections; verification, staging and file sync run outside it.
pub(crate) struct Publication {
    local: Local,
    owner: accounting::Accounting,
    reservation: accounting::Reservation,
    guard: CacheGuard,
    manifest: Option<(String, Vec<u8>)>,
    observed: HashMap<String, Option<u64>>,
    write_parts: HashSet<String>,
    pending: HashSet<String>,
}

fn physical_size(path: &Path) -> anyhow::Result<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file(), "U07 physical part is not a regular file");
            Ok(Some(metadata.len()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn publication_changed() -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock, "U07 physical publication changed or remains protected; retry").into()
}

impl Publication {
    pub(crate) fn put(&mut self, digest: &str, data: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(self.guard.covers(&self.local.dir, digest), "chunk is outside admitted publication");
        let expected = self.reservation.want.binary_search_by(|(name, _)| name.as_str().cmp(digest)).ok().and_then(|index| self.reservation.want.get(index)).map(|(_, size)| *size);
        anyhow::ensure!(expected == Some(u64::try_from(data.len())?) && hex::encode(Sha256::digest(data)) == digest, "chunk bytes do not match their admitted digest/size");
        self.write_part(digest, data)
    }

    pub(crate) fn write_manifest(&mut self, digest: &str, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(self.manifest.as_ref().is_some_and(|(name, expected)| name == digest && expected.as_slice() == bytes), "assembly differs from admitted metadata");
        self.write_part(&format!("manifest:{digest}"), bytes)
    }

    fn write_part(&mut self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let path = accounting::physical_path(&self.local.dir, key)?;
        // Includes a repeated chunk already published by this same operation. Comparing
        // bounded content outside Mutation never authorizes a replacement of a stable part.
        if physical_size(&path)? == Some(u64::try_from(bytes.len())?) && std::fs::read(&path)? == bytes {
            self.pending.remove(key);
            return Ok(());
        }
        anyhow::ensure!(self.write_parts.contains(key), "U07 verified resident part changed; no replacement staging was reserved");
        let parent = path.parent().ok_or_else(|| anyhow::anyhow!("physical part has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let temporary = path.with_extension(format!("{}.tmp", self.reservation.id));
        // A second caller never truncates an already admitted writer's exact temporary.
        let mut file = std::fs::File::options().create_new(true).write(true).open(&temporary)?;
        let staged = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = staged {
            // The durable intent keeps failed cleanup charged for bounded recovery.
            let _ = accounting::remove_cdc_staging(&self.local.dir, key, &self.reservation.id);
            return Err(error.into());
        }
        let published = (|| -> anyhow::Result<()> {
            let fence = guards::Mutation::acquire(&self.local.dir)?;
            let previous = self.observed.get(key).ok_or_else(|| anyhow::anyhow!("physical part is outside admitted observation"))?;
            let current = physical_size(&path)?;
            if current != *previous { return Err(publication_changed()); }
            if current.is_some() {
                let digest = key.strip_prefix("manifest:").unwrap_or(key);
                if guards::activity(&self.local.dir, &fence, Some(&self.guard))?.digests.contains(digest) { return Err(publication_changed()); }
            }
            std::fs::rename(&temporary, &path)?;
            crate::blobs::chunks::sync_dir(parent)?;
            Ok(())
        })();
        if published.is_err() {
            // Same intent/guard still owns the path; absence retries still sync its parent.
            let _ = accounting::remove_cdc_staging(&self.local.dir, key, &self.reservation.id);
        }
        if published.is_ok() { self.pending.remove(key); }
        published
    }

    pub(crate) fn finish(self) -> anyhow::Result<CacheGuard> {
        anyhow::ensure!(self.pending.is_empty(), "U07 publication still has unverified or unpublished parts");
        // No whole-object fence during cleanup barriers. Only this intent may name these
        // temporaries; no reservation credit is returned until all are durably absent.
        for key in &self.write_parts { accounting::remove_cdc_staging(&self.local.dir, key, &self.reservation.id)?; }
        let fence = Arc::new(guards::Mutation::acquire(&self.local.dir)?);
        for (key, size) in &self.reservation.want {
            anyhow::ensure!(physical_size(&accounting::physical_path(&self.local.dir, key)?)? == Some(*size), "admitted physical part is missing or incomplete");
        }
        self.owner.settle_live(&self.local.dir, &self.reservation, fence, self.guard.clone())?;
        // The same slot is now a logical read lease; no mode/scan/downgrade is introduced.
        Ok(self.guard)
    }
}

/// Only the tier's fixed two-level digest layout is data. Pins, temporary writes and other
/// metadata never count as chunks, and symlinks are not followed.
pub(crate) fn files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if !a.file_type().is_ok_and(|t| t.is_dir()) || a.file_name().len() != 2 { continue; }
        for b in std::fs::read_dir(a.path()).into_iter().flatten().flatten() {
            if !b.file_type().is_ok_and(|t| t.is_dir()) || b.file_name().len() != 2 { continue; }
            out.extend(std::fs::read_dir(b.path()).into_iter().flatten().flatten()
                .filter(|c| c.file_type().is_ok_and(|t| t.is_file())).map(|c| c.path()));
        }
    }
    out
}

/// `10G` (or `10GB`, `10GiB`), `512M`, `1048576`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let s = s.strip_suffix("iB").or_else(|| s.strip_suffix("ib")).or_else(|| s.strip_suffix(['B', 'b'])).unwrap_or(s);
    let (n, mult) = match s.char_indices().last()? {
        (i, 'G' | 'g') => (s.get(..i)?, 1u64 << 30),
        (i, 'M' | 'm') => (s.get(..i)?, 1 << 20),
        (i, 'K' | 'k') => (s.get(..i)?, 1 << 10),
        _ => (s, 1),
    };
    n.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// The paths in one tar, each under its index; those that don't exist are left out.
pub fn archive(paths: &Paths, out: &Path) -> anyhow::Result<usize> {
    let mut b = tar::Builder::new(std::fs::File::create(out)?);
    b.follow_symlinks(false);
    let mut kept = 0;
    for (i, p) in paths.resolved.iter().enumerate() {
        if p.is_dir() {
            append_sorted(&mut b, Path::new(&i.to_string()), p)?;
            kept += 1;
        } else if p.is_file() {
            b.append_path_with_name(p, i.to_string())?;
            kept += 1;
        }
    }
    b.finish()?;
    Ok(kept)
}

/// `dir` into the tar as `name`, its entries in name order: the same files make the same archive,
/// and so the same chunks, whatever order the file system lists them in. Files, directories and
/// links are kept; sockets and devices aren't.
fn append_sorted(b: &mut tar::Builder<std::fs::File>, name: &Path, dir: &Path) -> anyhow::Result<()> {
    b.append_dir(name, dir)?;
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for e in entries {
        let (from, to, kind) = (e.path(), name.join(e.file_name()), e.file_type()?);
        if kind.is_dir() {
            append_sorted(b, &to, &from)?;
        } else if kind.is_file() || kind.is_symlink() {
            b.append_path_with_name(&from, &to)?;
        }
    }
    Ok(())
}

/// A tar back where its paths were named: unpacked in `staging` first (nothing in it can land
/// outside), then each index moved into place over what's there.
pub fn unpack(archive: &Path, paths: &Paths, staging: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging)?;
    tar::Archive::new(std::fs::File::open(archive)?).unpack(staging)?;
    for (i, to) in paths.resolved.iter().enumerate() {
        let from = staging.join(i.to_string());
        if from.symlink_metadata().is_ok() {
            place(&from, to)?;
        }
    }
    let _ = std::fs::remove_dir_all(staging);
    Ok(())
}

/// `from` at `to`: renamed when it can be, else copied; a directory merged into one that's there.
fn place(from: &Path, to: &Path) -> anyhow::Result<()> {
    let meta = from.symlink_metadata()?;
    if meta.is_dir() && to.is_dir() {
        for e in std::fs::read_dir(from)? {
            let e = e?;
            place(&e.path(), &to.join(e.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if to.symlink_metadata().is_ok() && !to.is_dir() {
        std::fs::remove_file(to)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Another file system: copied.
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            place(&e.path(), &to.join(e.file_name()))?;
        }
    } else if meta.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// A file's content-defined chunks (the store's cuts), and its SHA-256; each kept in `local`.
pub fn chunks(path: &Path, local: Option<&Local>) -> anyhow::Result<(String, u64, Vec<Chunk>)> {
    let mut f = std::fs::File::open(path)?;
    let mut whole = Sha256::new();
    let mut cut = Cdc::new();
    let mut pending: Vec<u8> = Vec::with_capacity(cdc::MAX * 2);
    let (mut start, mut eof, mut size, mut offset) = (0usize, false, 0u64, 0u64);
    let mut buf = vec![0u8; 4 << 20];
    let mut out = Vec::new();
    loop {
        while !eof && pending.len() - start < cdc::MAX {
            if start > 0 {
                pending.drain(..start);
                start = 0;
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                eof = true;
            } else {
                let got = buf.get(..n).unwrap_or_default();
                whole.update(got);
                pending.extend_from_slice(got);
                size += n as u64;
            }
        }
        let rest = pending.get(start..).unwrap_or_default();
        match cut.next(rest, eof) {
            Some(n) if n > 0 => {
                let data = rest.get(..n).unwrap_or_default();
                let digest = hex::encode(Sha256::digest(data));
                if let Some(l) = local {
                    l.put_optional(&digest, data)?;
                }
                out.push(Chunk { digest, size: n as u64, offset });
                offset += n as u64;
                start += n;
            }
            _ => break,
        }
    }
    Ok((hex::encode(whole.finalize()), size, out))
}

impl Service<'_> {
    async fn post(&self, what: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
        let url = format!("{}/_kiln/cache/{what}", self.base.trim_end_matches('/'));
        let resp = self.http.post(&url).bearer(self.token).header("content-type", "application/json").body(serde_json::to_vec(body)?).timeout(Duration::from_secs(300)).send().await?;
        let status = resp.status();
        let v: Value = if status == 204 { Value::Null } else { resp.json(64 << 20).await.unwrap_or_default() };
        Ok((status, v))
    }
}

/// What a restore found.
pub struct Found {
    pub key: String,
    pub downloaded: u64,
    pub local_bytes: u64,
}

/// The first entry `keys` name (the first key, then each restore key, as GitHub looks), its
/// chunks from the local tier or the server, into `archive`, checked whole. `None` on a miss.
pub async fn restore(svc: &Service<'_>, keys: &[String], version: &str, scope: &str, local: Option<&Local>, archive: &Path, look_only: bool) -> anyhow::Result<Option<Found>> {
    restore_bounded(svc, keys, version, scope, local, archive, look_only, None).await
}

/// U07: work fills apply their object bound before network/disk growth; ordinary CI archives
/// keep their existing policy. Checked actual writes also defend against a false size plan.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn restore_bounded(svc: &Service<'_>, keys: &[String], version: &str, scope: &str, local: Option<&Local>, archive: &Path, look_only: bool, max: Option<u64>) -> anyhow::Result<Option<Found>> {
    let (status, v) = svc.post("lookup", &json!({ "keys": keys, "version": version, "scope": scope })).await?;
    if status == 204 {
        return Ok(None);
    }
    anyhow::ensure!((200..300).contains(&status), "{}", v.get("message").and_then(Value::as_str).unwrap_or("the cache lookup failed"));
    let id = v.get("id").and_then(Value::as_i64).unwrap_or_default();
    let key = v.get("key").and_then(Value::as_str).unwrap_or_default().to_owned();
    if look_only {
        return Ok(Some(Found { key, downloaded: 0, local_bytes: 0 }));
    }
    let want = v.pointer("/object/digest").and_then(Value::as_str).unwrap_or_default().to_owned();
    let list: Vec<(String, u64)> = v.pointer("/object/chunks").cloned().map(serde_json::from_value).transpose()?.unwrap_or_default();
    let declared = list.iter().try_fold(0u64, |total, (_, size)| total.checked_add(*size).ok_or_else(|| anyhow::anyhow!("cache object declared size overflow")))?;
    anyhow::ensure!(max.is_none_or(|limit| declared <= limit), "work cache object exceeds its fill limit before transfer");
    let mut f = std::fs::File::create(archive)?;
    let mut written = 0u64;
    let mut whole = Sha256::new();
    let (mut downloaded, mut local_bytes) = (0u64, 0u64);
    for (digest, size) in &list {
        let data = match local.and_then(|l| l.get(digest)) {
            Some(d) => {
                local_bytes += d.len() as u64;
                d
            }
            None => {
                let url = format!("{}/_kiln/cache/entries/{id}/chunks/{digest}", svc.base.trim_end_matches('/'));
                let resp = svc.http.get(&url).bearer(svc.token).timeout(Duration::from_secs(600)).send().await?;
                anyhow::ensure!(resp.is_success(), "chunk {digest}: HTTP {}", resp.status());
                let mut d = resp.bytes(cdc::MAX + 1).await?.to_vec();
                anyhow::ensure!(hex::encode(Sha256::digest(&d)) == *digest, "chunk {digest} arrived corrupted");
                if let Some(l) = local {
                    let (keep, name) = (l.clone(), digest.clone());
                    d = crate::rt::blocking(move || -> anyhow::Result<Vec<u8>> {
                        keep.put_optional(&name, &d)?;
                        Ok(d)
                    }).await??;
                }
                downloaded += d.len() as u64;
                d
            }
        };
        anyhow::ensure!(data.len() as u64 == *size, "chunk {digest} is {} bytes, not {size}", data.len());
        written = written.checked_add(u64::try_from(data.len())?).ok_or_else(|| anyhow::anyhow!("cache object actual size overflow"))?;
        anyhow::ensure!(max.is_none_or(|limit| written <= limit), "work cache object exceeds its fill limit");
        whole.update(&data);
        f.write_all(&data)?;
    }
    f.flush()?;
    let got = hex::encode(whole.finalize());
    anyhow::ensure!(got == want, "the cache's content hashes to sha256:{got}, not sha256:{want}");
    Ok(Some(Found { key, downloaded, local_bytes }))
}

/// What a save sent.
pub struct Saved {
    pub chunks: usize,
    pub uploaded: u64,
    pub size: u64,
}

/// `archive` as the entry `key`: its chunks cut, those the repository's caches lack uploaded
/// (from the local tier or the archive itself), then the entry committed. `None` when another
/// save of the key got there first.
pub async fn save(svc: &Service<'_>, key: &str, version: &str, scope: &str, local: Option<&Local>, archive: &Path) -> anyhow::Result<Option<Saved>> {
    save_guarded(svc, key, version, scope, local, archive, None).await
}

/// U07: planner ownership follows the dispatched read even if the upload future is cancelled.
#[allow(clippy::too_many_arguments)] // U07: existing cache protocol arguments plus worker ownership.
pub(crate) async fn save_guarded(svc: &Service<'_>, key: &str, version: &str, scope: &str, local: Option<&Local>, archive: &Path, guard: Option<CacheGuard>) -> anyhow::Result<Option<Saved>> {
    let a = archive.to_path_buf();
    let keep = local.cloned();
    let planner_guard = guard.clone();
    let (digest, size, list) = crate::rt::blocking(move || {
        let _owned = planner_guard;
        chunks(&a, keep.as_ref())
    }).await??;
    let digests: Vec<&String> = list.iter().map(|c| &c.digest).collect();
    let (status, v) = svc.post("missing", &json!({ "chunks": digests, "scope": scope })).await?;
    anyhow::ensure!((200..300).contains(&status), "{}", v.get("message").and_then(Value::as_str).unwrap_or("the cache refused the save"));
    let missing: HashSet<String> = v.get("missing").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let mut f = std::fs::File::open(archive)?;
    let mut uploaded = 0u64;
    let mut sent: HashSet<&str> = HashSet::new();
    for c in list.iter().filter(|c| missing.contains(&c.digest)) {
        if !sent.insert(&c.digest) {
            continue;
        }
        let data = match local.and_then(|l| l.get(&c.digest)) {
            Some(d) => d,
            None => {
                let mut d = vec![0u8; usize::try_from(c.size).unwrap_or(0)];
                f.seek(SeekFrom::Start(c.offset))?;
                f.read_exact(&mut d)?;
                d
            }
        };
        let url = format!("{}/_kiln/cache/chunks/{}", svc.base.trim_end_matches('/'), c.digest);
        let resp = svc.http.put(&url).bearer(svc.token).body(data).timeout(Duration::from_secs(600)).send().await?;
        anyhow::ensure!(resp.is_success(), "chunk {}: HTTP {}", c.digest, resp.status());
        uploaded += c.size;
    }
    let pairs: Vec<(String, u64)> = list.iter().map(|c| (c.digest.clone(), c.size)).collect();
    let (status, v) = svc.post("entries", &json!({ "key": key, "version": version, "scope": scope, "object": { "digest": digest, "size": size, "chunks": pairs } })).await?;
    if status == 409 {
        return Ok(None);
    }
    anyhow::ensure!((200..300).contains(&status), "{}", v.get("message").and_then(Value::as_str).unwrap_or("the cache refused the save"));
    Ok(Some(Saved { chunks: list.len(), uploaded, size }))
}

/// U07: upload an admitted physical assembly directly. No unaccounted whole-object copy,
/// and no nested guard allocation when all operation slots are in use.
#[allow(clippy::too_many_arguments)] // U07: existing wire fields plus protected physical assembly.
pub(crate) async fn save_local_chunks(svc: &Service<'_>, key: &str, version: &str, scope: &str, digest: &str, size: u64, list: &[(String, u64)], local: &Local, guard: CacheGuard) -> anyhow::Result<Option<Saved>> {
    anyhow::ensure!(Local::valid(digest) && list.len() <= 65_536, "invalid U07 upload assembly");
    let total = list.iter().try_fold(0u64, |total, (name, bytes)| -> anyhow::Result<u64> {
        anyhow::ensure!(Local::valid(name) && guard.covers(&local.dir, name), "upload chunk lacks operation ownership");
        total.checked_add(*bytes).ok_or_else(|| anyhow::anyhow!("upload size overflow"))
    })?;
    anyhow::ensure!(total == size, "upload assembly size differs from object");
    let digests: Vec<_> = list.iter().map(|(name, _)| name).collect();
    let (status, value) = svc.post("missing", &json!({ "chunks": digests, "scope": scope })).await?;
    anyhow::ensure!((200..300).contains(&status), "{}", value.get("message").and_then(Value::as_str).unwrap_or("the cache refused the save"));
    let missing: HashSet<String> = value.get("missing").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let mut sent = HashSet::new();
    let mut uploaded = 0u64;
    for (name, bytes) in list {
        if !missing.contains(name) || !sent.insert(name) { continue; }
        let (keep, hash, owned) = (local.clone(), name.clone(), guard.clone());
        let data = crate::rt::blocking(move || keep.get_protected(&hash, &owned)).await?
            .ok_or_else(|| anyhow::anyhow!("protected upload chunk {name} is missing or corrupt"))?;
        anyhow::ensure!(u64::try_from(data.len())? == *bytes, "protected upload chunk has wrong size");
        let url = format!("{}/_kiln/cache/chunks/{name}", svc.base.trim_end_matches('/'));
        let response = svc.http.put(&url).bearer(svc.token).body(data).timeout(Duration::from_secs(600)).send().await?;
        anyhow::ensure!(response.is_success(), "chunk {name}: HTTP {}", response.status());
        uploaded = uploaded.checked_add(*bytes).ok_or_else(|| anyhow::anyhow!("upload size overflow"))?;
    }
    let (status, value) = svc.post("entries", &json!({ "key": key, "version": version, "scope": scope, "object": { "digest": digest, "size": size, "chunks": list } })).await?;
    if status == 409 { return Ok(None); }
    anyhow::ensure!((200..300).contains(&status), "{}", value.get("message").and_then(Value::as_str).unwrap_or("the cache refused the save"));
    Ok(Some(Saved { chunks: list.len(), uploaded, size }))
}

/// Where this runner keeps one of a job's v1 `cache:` folders (docs/ci.md W02-R06, W24-R11):
/// `<work>/_kept/<repository>/<job>/<folder>`, each name hashed.
pub fn kept_at(work: &Path, repo: &str, job: &str, dir: &str) -> PathBuf {
    let h = |s: &str| -> String { crate::util::sha256_hex(s.as_bytes()).chars().take(16).collect() };
    work.join("_kept").join(h(repo)).join(h(job)).join(h(dir))
}

/// Puts a kept folder in place for a leg. A trusted leg's is what the job's last leg on this
/// runner kept, moved rather than copied; an untrusted leg's starts empty. Says which.
pub fn put_kept(kept: &Path, at: &Path, trusted: bool) -> anyhow::Result<&'static str> {
    use anyhow::Context as _;
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if trusted && kept.is_dir() {
        if std::fs::symlink_metadata(at).is_ok() {
            remove_any(at).with_context(|| format!("clear {}", at.display()))?;
        }
        std::fs::rename(kept, at).with_context(|| format!("move {} into place", kept.display()))?;
        return Ok("restored from this runner");
    }
    std::fs::create_dir_all(at).with_context(|| format!("create {}", at.display()))?;
    Ok(if trusted { "empty: nothing kept here yet" } else { "empty: an untrusted leg gets no kept folders" })
}

/// Keeps a trusted leg's folder for the job's next leg on this runner, in place of what was kept.
/// `false` when the leg left no folder there.
pub fn keep(at: &Path, kept: &Path) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    if !std::fs::symlink_metadata(at).is_ok_and(|m| m.is_dir()) {
        return Ok(false);
    }
    if let Some(parent) = kept.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if std::fs::symlink_metadata(kept).is_ok() {
        remove_any(kept).with_context(|| format!("clear {}", kept.display()))?;
    }
    std::fs::rename(at, kept).with_context(|| format!("keep {}", at.display()))?;
    Ok(true)
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w24_r11_v1_dirs_are_kept_for_trusted_legs_only() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let kept = kept_at(d.path(), "acme/app", "build", "target");
        let (one, two, three) = (d.path().join("w/1/target"), d.path().join("w/2/target"), d.path().join("w/3/target"));
        // The job's first leg starts empty and writes; its folder is kept after its steps.
        assert_eq!(put_kept(&kept, &one, true)?, "empty: nothing kept here yet");
        std::fs::write(one.join("f"), "first")?;
        assert!(keep(&one, &kept)?);
        // The job's next trusted leg reads what the first wrote, and keeps it again.
        assert_eq!(put_kept(&kept, &two, true)?, "restored from this runner");
        assert_eq!(std::fs::read_to_string(two.join("f"))?, "first");
        assert!(keep(&two, &kept)?);
        // An untrusted leg sees none of it, and what it writes never reaches the kept folder.
        assert_eq!(put_kept(&kept, &three, false)?, "empty: an untrusted leg gets no kept folders");
        assert!(!three.join("f").exists());
        std::fs::write(three.join("poison"), "x")?;
        assert!(kept.join("f").exists() && !kept.join("poison").exists());
        // Each repository, job and folder has its own.
        assert_ne!(kept, kept_at(d.path(), "acme/other", "build", "target"));
        assert_ne!(kept, kept_at(d.path(), "acme/app", "test", "target"));
        assert_ne!(kept, kept_at(d.path(), "acme/app", "build", "node_modules"));
        Ok(())
    }

    #[test]
    fn w08_r09_archive_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ws = dir.path().join("ws");
        let home = dir.path().join("home");
        std::fs::create_dir_all(ws.join("target/debug"))?;
        std::fs::create_dir_all(home.join(".cargo/registry"))?;
        std::fs::write(ws.join("target/debug/app"), "binary")?;
        std::fs::write(home.join(".cargo/registry/index"), "crates")?;
        std::fs::write(ws.join("lock.txt"), "one file")?;
        let paths = Paths::new(&["target".into(), "~/.cargo/registry".into(), "lock.txt".into(), "missing".into()], &ws, Some(&home));
        for name in ["zeta", "alpha", "mid"] {
            std::fs::write(ws.join("target").join(name), name)?;
        }
        let tar = dir.path().join("c.tar");
        assert_eq!(archive(&paths, &tar)?, 3, "what doesn't exist is left out");
        // Each directory's entries in name order, whatever order the file system keeps.
        let names: Vec<String> = tar::Archive::new(std::fs::File::open(&tar)?).entries()?.filter_map(|e| e.ok()?.path().ok().map(|p| p.to_string_lossy().trim_end_matches('/').to_owned())).filter(|n| n.starts_with("0/")).collect();
        assert_eq!(names, ["0/alpha", "0/debug", "0/debug/app", "0/mid", "0/zeta"], "{names:?}");
        // Gone, then back where each was.
        std::fs::remove_dir_all(ws.join("target"))?;
        std::fs::remove_dir_all(home.join(".cargo"))?;
        std::fs::remove_file(ws.join("lock.txt"))?;
        unpack(&tar, &paths, &dir.path().join("staging"))?;
        assert_eq!(std::fs::read_to_string(ws.join("target/debug/app"))?, "binary");
        assert_eq!(std::fs::read_to_string(home.join(".cargo/registry/index"))?, "crates");
        assert_eq!(std::fs::read_to_string(ws.join("lock.txt"))?, "one file");
        // The version names the paths, and isn't actions/cache's.
        assert_ne!(paths.version(), Paths::new(&["target".into()], &ws, None).version());
        Ok(())
    }

    #[test]
    fn u07_operation_protection_is_temporary_and_idle_bytes_obey_budget() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ledger = crate::store::Store::in_memory()?;
        let local = Local::with_ledger(dir.path(), ledger, Some(8))?;
        let missing = tempfile::tempdir()?;
        assert!(Local::initialized(missing.path()).is_err(), "reads never invent a metadata owner");
        assert!(!missing.path().join("_cache").exists());
        // Capacity requests and handles made before initialization keep the SAME owner.
        let early = tempfile::tempdir()?;
        let plain = Local::new(early.path(), None);
        #[cfg(unix)] let aliases = tempfile::tempdir()?;
        #[cfg(unix)] let alias_path = aliases.path().join("early-work");
        #[cfg(unix)] std::os::unix::fs::symlink(early.path(), &alias_path)?;
        #[cfg(not(unix))] let alias_path = early.path().join(".");
        let alias = Local::new(&alias_path, Some(4));
        let owner = Local::with_ledger(&alias_path, crate::store::Store::in_memory()?, None)?;
        let too_large = hex::encode(Sha256::digest(b"12345678"));
        let error = alias.put_accounted(&too_large, b"12345678").err().ok_or_else(|| anyhow::anyhow!("early alias capacity was lost"))?;
        assert!(Local::declined(&error), "{error:#}");
        let small = hex::encode(Sha256::digest(b"1234"));
        alias.put_accounted(&small, b"1234")?;
        assert_eq!(plain.get(&small).as_deref(), Some(b"1234".as_slice()));
        assert_eq!(owner.get(&small).as_deref(), Some(b"1234".as_slice()));
        // A missing work directory can acquire its canonical spelling without orphaning an
        // already returned handle when no other live owner claims that spelling.
        let absent = missing.path().join("not-created-yet");
        let before = Local::new(&absent, Some(4));
        let owner = Local::with_ledger(&absent, crate::store::Store::in_memory()?, None)?;
        before.put_accounted(&small, b"1234")?;
        assert_eq!(owner.get(&small).as_deref(), Some(b"1234".as_slice()));
        let data = b"12345678";
        let digest = hex::encode(Sha256::digest(data));
        let mut publication = local.reserve(&[(digest.clone(), 8), (digest.clone(), 8)])?;
        publication.put(&digest, data)?;
        let protected = publication.finish()?;
        let alias = Local::initialized(&dir.path().join("."))?;
        assert_eq!(alias.get(&digest).as_deref(), Some(data.as_slice()));
        #[cfg(unix)]
        {
            let aliases = tempfile::tempdir()?;
            let link = aliases.path().join("work");
            std::os::unix::fs::symlink(dir.path(), &link)?;
            assert_eq!(Local::initialized(&link)?.get(&digest).as_deref(), Some(data.as_slice()));
            // A newly created symlink may reveal TWO already escaped owners. Initialization
            // must refuse rather than lose the older handle or either capacity request.
            let target = tempfile::tempdir()?;
            let late_link = aliases.path().join("late-work");
            let unresolved = Local::new(&late_link, Some(4));
            std::os::unix::fs::symlink(target.path(), &late_link)?;
            let canonical = Local::new(target.path(), None);
            let error = Local::with_ledger(&late_link, crate::store::Store::in_memory()?, None).err().ok_or_else(|| anyhow::anyhow!("conflicting escaped owners merged"))?;
            assert!(format!("{error:#}").contains("conflicting live owners"), "{error:#}");
            drop(unresolved); drop(canonical);
        }
        let other = b"abcdefgh";
        let other_digest = hex::encode(Sha256::digest(other));
        let error = local.put_accounted(&other_digest, other).err().ok_or_else(|| anyhow::anyhow!("admission evicted active data"))?;
        assert!(Local::declined(&error));
        assert_eq!(local.get_protected(&digest, &protected).as_deref(), Some(data.as_slice()));
        drop(protected);
        // A later legitimate operation, rather than a historical holding or permanent pin,
        // controls residency. Missing reads remain misses and capacity never overflows.
        local.put_accounted(&other_digest, other)?;
        assert!(local.get(&digest).is_none());
        assert_eq!(local.get(&other_digest).as_deref(), Some(other.as_slice()));
        assert!(local.put_accounted(&other_digest, b"wrong").is_err());
        // Native/chunk aliases are one charged inode. A host reader retains protection even
        // after its original NativeFile handle is dropped; corruption cannot retire it early.
        let plan = NativePlan::new(other_digest.clone(), 8)?;
        let native = local.native_file(&plan)?.ok_or_else(|| anyhow::anyhow!("native alias missed the chunk"))?;
        let mut reader = native.reader();
        drop(native);
        let mut back = Vec::new();
        reader.read_to_end(&mut back)?;
        assert_eq!(back, other);
        std::fs::write(local.path(&other_digest), b"corrupt!")?;
        assert!(local.native_file(&plan).is_err(), "equal-size corrupt content is never a native cache hit");
        assert!(!local.discard_corrupt(&other_digest)?, "a protected inode cannot release physical credit");
        assert!(local.reserve(&[(other_digest.clone(), 8)]).is_err(), "chunk repair must also honor the native reader");
        drop(reader);
        assert!(local.discard_corrupt(&other_digest)?);

        crate::rt::block_on(async {
            let base = crate::os::home_dir().join(".cache/kiln-tests/u07-native");
            std::fs::create_dir_all(&base)?;
            let dir = tempfile::tempdir_in(base)?;
            let bytes = vec![b'n'; cdc::MAX + 1];
            let size = u64::try_from(bytes.len())?;
            let digest = hex::encode(Sha256::digest(&bytes));
            let local = Local::with_ledger(dir.path(), crate::store::Store::in_memory()?, Some(size))?;
            let plan = NativePlan::new(digest.clone(), size)?;
            assert!(local.reserve(&[(digest.clone(), size)]).is_err(), "native admission never widens CDC chunks");
            let (tx, done) = local.reserve_native(plan.clone())?.sink()?;
            // Native staging releases the global fence before any long-running stream/hash.
            drop(guards::Mutation::acquire(&local.dir)?);
            local.trim_checked()?;
            for chunk in bytes.chunks(64 * 1024) { tx.send(bytes::Bytes::copy_from_slice(chunk)).await?; }
            drop(tx);
            let native = done.await?;
            assert_eq!(native.len(), size);
            assert!(local.get(&digest).is_none(), "oversized native bytes remain outside CDC reads");
            let mut back = Vec::new();
            native.reader().read_to_end(&mut back)?;
            assert_eq!(back, bytes);
            let reused = local.native_file(&plan)?.ok_or_else(|| anyhow::anyhow!("verified native reuse missed"))?;
            drop(guards::Mutation::acquire(&local.dir)?);
            let tiny = Local::new(dir.path(), Some(0));
            assert!(tiny.trim_checked().is_err(), "native readers retain their exact residency");
            drop(native); drop(reused);
            tiny.trim_checked()?;
            assert!(!local.path(&digest).exists());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn u07_optional_capacity_decline_preserves_archive_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let local = Local::with_ledger(dir.path(), crate::store::Store::in_memory()?, Some(0))?;
        let path = dir.path().join("archive");
        std::fs::write(&path, b"verified archive remains the upload source")?;
        let without = chunks(&path, None)?;
        let with = chunks(&path, Some(&local))?;
        assert_eq!(with, without);
        assert!(with.2.iter().all(|chunk| local.get(&chunk.digest).is_none()));
        Ok(())
    }

    #[test]
    fn w08_r09_chunks_and_the_local_tier() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let f = dir.path().join("blob");
        // 6 MiB of noise: several chunks, the same cuts as the store's.
        let data: Vec<u8> = (0..6u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        std::fs::write(&f, &data)?;
        let local = Local::with_ledger(dir.path(), crate::store::Store::in_memory()?, Some(1 << 30))?;
        let (digest, size, list) = chunks(&f, Some(&local))?;
        assert_eq!((digest, size), (hex::encode(Sha256::digest(&data)), data.len() as u64));
        let cuts: Vec<u64> = cdc::cuts(&data).into_iter().map(|n| n as u64).collect();
        assert_eq!(list.iter().map(|c| c.size).collect::<Vec<_>>(), cuts);
        assert!(list.windows(2).all(|w| w.get(1).map(|b| b.offset) == w.first().map(|a| a.offset + a.size)));
        assert!(list.iter().all(|c| local.get(&c.digest).is_some()), "kept in the local tier");
        // Past its size, the oldest go.
        let small = Local::new(dir.path(), Some(1));
        small.trim();
        assert!(list.iter().all(|c| local.get(&c.digest).is_none()));
        assert_eq!((parse_size("10G"), parse_size("512M"), parse_size("42")), (Some(10 << 30), Some(512 << 20), Some(42)));
        assert_eq!((parse_size("20GB"), parse_size("20GiB"), parse_size("1 k"), parse_size("lots")), (Some(20 << 30), Some(20 << 30), Some(1 << 10), None));
        Ok(())
    }
}
