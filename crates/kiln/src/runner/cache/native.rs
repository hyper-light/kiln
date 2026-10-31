//! U07: exact native bytes in Local's existing physical tier. These handles authorize only
//! in-process reads/writes. External Group/Execution residency and generated outputs need a
//! separate physical contract; no path, raw file or descriptor is exposed to a VMM here.

use std::fs::File;
use std::io;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{CacheGuard, Local, accounting, guards};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind { NativeFile }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Producer { InProcess }

/// A finite exact content plan. Chunk callers retain their existing CDC bound and format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePlan {
    kind: Kind,
    producer: Producer,
    digest: String,
    bytes: u64,
}
impl NativePlan {
    pub fn new(digest: impl Into<String>, bytes: u64) -> anyhow::Result<Self> {
        let plan = Self { kind: Kind::NativeFile, producer: Producer::InProcess, digest: digest.into(), bytes };
        plan.validate()?;
        Ok(plan)
    }
    pub fn digest(&self) -> &str { &self.digest }
    pub fn len(&self) -> u64 { self.bytes }
    pub fn is_empty(&self) -> bool { self.bytes == 0 }
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(Local::valid(&self.digest) && i64::try_from(self.bytes).is_ok(), "U07 native content requires a SHA-256 and representable finite size");
        Ok(())
    }
}

/// Host reads retain both the verified inode and its exact cache protection. This cannot be
/// converted to a path or raw file for an external process whose lifetime could outlive it.
pub struct NativeFile {
    plan: NativePlan,
    file: Arc<File>,
    guard: CacheGuard,
}
// Keep the inode and lease together even on verification errors: field drop order closes
// the file reference before releasing its exact operation protection.
struct GuardedFile { file: Arc<File>, guard: CacheGuard }
impl NativeFile {
    pub fn digest(&self) -> &str { self.plan.digest() }
    pub fn len(&self) -> u64 { self.plan.len() }
    pub fn is_empty(&self) -> bool { self.plan.is_empty() }
    pub fn reader(&self) -> NativeReader {
        NativeReader { file: self.file.clone(), _guard: self.guard.clone(), offset: 0, bytes: self.plan.len() }
    }
}

/// Independent offsets do not share a File seek position; every reader retains the lease.
pub struct NativeReader { file: Arc<File>, _guard: CacheGuard, offset: u64, bytes: u64 }
impl io::Read for NativeReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = usize::try_from(self.bytes.saturating_sub(self.offset).min(u64::try_from(bytes.len()).map_err(io::Error::other)?)).map_err(io::Error::other)?;
        if count == 0 { return Ok(0) }
        let bytes = bytes.get_mut(..count).ok_or_else(|| io::Error::other("native read bound changed"))?;
        let n = crate::os::read_at(&self.file, bytes, self.offset)?;
        if n == 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "U07 native content changed after verification")) }
        self.offset = self.offset.checked_add(u64::try_from(n).map_err(io::Error::other)?).ok_or_else(|| io::Error::other("native read offset overflow"))?;
        Ok(n)
    }
}

/// Native streaming is a facade over the same Accounting reservation and guard slots as
/// Publication. Unlike bounded CDC writes, its long I/O never retains global Mutation.
pub struct NativePublication {
    local: Local,
    owner: accounting::Accounting,
    reservation: accounting::Reservation,
    plan: NativePlan,
    guard: CacheGuard,
}

impl Local {
    /// Verify outside Mutation while protection makes all owned cache mutations refuse a
    /// replacement. The returned handle reads this actual inode, not a later path lookup.
    pub fn native_file(&self, plan: &NativePlan) -> anyhow::Result<Option<NativeFile>> {
        plan.validate()?;
        let Some(held) = open_guarded(self, plan.digest())? else { return Ok(None) };
        anyhow::ensure!(verify(&held.file, plan)?, "U07 native content differs from its digest/size; repair only after its readers release protection");
        let _ = held.file.set_modified(SystemTime::now());
        Ok(Some(NativeFile { plan: plan.clone(), file: held.file, guard: held.guard }))
    }

    /// Check native_file first to reuse a verified hit without staging. A new publication
    /// reserves its full extent even if an old inode exists: old+replacement peak remains
    /// charged until physical unlink and ancestor sync establish credit.
    pub fn reserve_native(&self, plan: NativePlan) -> anyhow::Result<NativePublication> {
        plan.validate()?;
        let fence = Arc::new(guards::Mutation::acquire(&self.dir)?);
        let owner = self.owner(fence.clone())?;
        let id = crate::util::new_id();
        let guard = guards::protect_intent(&self.dir, &fence, plan.digest(), &id)?;
        let protected = guards::protected(&self.dir, &fence)?;
        let reservation = owner.prepare_native(&self.dir, &plan, id, &protected, fence, guard.clone())?;
        Ok(NativePublication { local: self.clone(), owner, reservation, plan, guard })
    }
}

impl NativePublication {
    /// Each batch is bounded BEFORE writing. Caller cancellation leaves the same durable
    /// intent charged and its OS slot owned until detached writer/blocking work has ended.
    pub fn sink(self) -> anyhow::Result<(async_channel::Sender<bytes::Bytes>, impl std::future::Future<Output = anyhow::Result<NativeFile>> + Send + 'static)> {
        let path = accounting::native_staging_path(&self.local.dir, &self.reservation.id)?;
        std::fs::create_dir_all(path.parent().context("native staging parent")?)?;
        let file = File::options().create_new(true).write(true).open(&path)?;
        let (tx, done) = crate::rt::file_sink_owned(file, self.plan.len(), self.guard.clone());
        Ok((tx, async move {
            let bytes = done.await?;
            anyhow::ensure!(bytes == self.plan.len(), "U07 native stream is shorter than its admitted size");
            // This closure owns self/guard even if the awaiting task is cancelled.
            crate::rt::blocking(move || self.publish()).await?
        }))
    }

    fn publish(self) -> anyhow::Result<NativeFile> {
        let temporary = accounting::native_staging_path(&self.local.dir, &self.reservation.id)?;
        {
            let file = File::options().read(true).write(true).open(&temporary)?;
            anyhow::ensure!(verify(&file, &self.plan)?, "U07 native stream differs from its admitted digest/size");
            file.sync_all()?;
        }
        // Exact protection was installed before admission. Existing immutable content cannot
        // be replaced by any owned chunk/native writer while we stream-verify this inode.
        // Unowned writes to the private physical tier are outside that contract.
        let path = self.local.path(self.plan.digest());
        let previous = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(metadata.is_file(), "U07 native content is not a regular file");
                Some((metadata.len(), verify(&File::open(&path)?, &self.plan)?))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let fence = Arc::new(guards::Mutation::acquire(&self.local.dir)?);
        anyhow::ensure!(self.guard.owns_intent(&self.local.dir, &self.reservation.id), "U07 native publication lost its exact intent lease");
        let protected = guards::protected(&self.local.dir, &fence)?;
        self.owner.recover(&self.local.dir, &protected, fence.clone())?;
        let current = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => { anyhow::ensure!(metadata.is_file(), "U07 native content is not a regular file"); Some(metadata.len()) }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        // Missing -> present is possible when an earlier chunk intent publishes the same
        // digest. Never hash gigabytes under Mutation or trust that newly appeared path.
        anyhow::ensure!(current == previous.map(|(bytes, _)| bytes), "U07 canonical content changed during verification; retry its verified read");
        if previous.is_some_and(|(_, valid)| valid) {
            accounting::remove_native_staging(&self.local.dir, &self.reservation.id)?;
        } else {
            let other = guards::activity(&self.local.dir, &fence, Some(&self.guard))?;
            anyhow::ensure!(!other.digests.contains(self.plan.digest()), "U07 corrupt native content is still protected; release its readers before replacement");
            let parent = path.parent().context("native content parent")?;
            std::fs::create_dir_all(parent)?;
            std::fs::rename(&temporary, &path)?;
            crate::blobs::chunks::sync_dir(parent)?;
            // Credit must also prove the source name absent, even after a failed prior sync.
            accounting::remove_native_staging(&self.local.dir, &self.reservation.id)?;
        }
        self.owner.settle_live(&self.local.dir, &self.reservation, fence, self.guard.clone())?;
        let file = Arc::new(File::open(&path)?);
        Ok(NativeFile { plan: self.plan, file, guard: self.guard })
    }
}

fn open_guarded(local: &Local, digest: &str) -> anyhow::Result<Option<GuardedFile>> {
    let fence = Arc::new(guards::Mutation::acquire(&local.dir)?);
    let _owner = local.owner(fence.clone())?;
    let guard = guards::protect(&local.dir, &fence, &[digest.to_owned()])?;
    let path = local.path(digest);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => anyhow::ensure!(metadata.is_file(), "U07 content is not a regular file"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    Ok(Some(GuardedFile { file: Arc::new(File::open(path)?), guard }))
}

fn verify(file: &File, plan: &NativePlan) -> anyhow::Result<bool> {
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "U07 native inode is not a regular file");
    if metadata.len() != plan.len() { return Ok(false) }
    Ok(hash(file, plan.len())? == plan.digest())
}

fn hash(file: &File, bytes: u64) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    while offset < bytes {
        let count = usize::try_from((bytes - offset).min(u64::try_from(buffer.len())?))?;
        let chunk = buffer.get_mut(..count).context("native hash bound")?;
        let n = crate::os::read_at(file, chunk, offset)?;
        anyhow::ensure!(n > 0, "U07 content became shorter during verification");
        hash.update(chunk.get(..n).context("native hash read bound")?);
        offset = offset.checked_add(u64::try_from(n)?).context("native hash offset overflow")?;
    }
    anyhow::ensure!(crate::os::read_at(file, &mut [0u8; 1], bytes)? == 0 && file.metadata()?.len() == bytes, "U07 content changed during verification");
    Ok(hex::encode(hash.finalize()))
}

/// Common corruption retirement also verifies outside Mutation and honors every current
/// operation's protection, including native readers. It never unlinks an open reader's inode.
pub(super) fn discard_corrupt(local: &Local, digest: &str) -> anyhow::Result<bool> {
    let Some(held) = open_guarded(local, digest)? else {
        let fence = Arc::new(guards::Mutation::acquire(&local.dir)?);
        local.owner(fence.clone())?.forget(&local.dir, digest, fence)?;
        return Ok(false);
    };
    let bytes = held.file.metadata()?.len();
    if hash(&held.file, bytes)? == digest { return Ok(false) }
    let GuardedFile { file, guard } = held;
    drop(file);
    let fence = Arc::new(guards::Mutation::acquire(&local.dir)?);
    let protected = guards::activity(&local.dir, &fence, Some(&guard))?;
    if protected.digests.contains(digest) { return Ok(false) }
    let owner = local.owner(fence.clone())?;
    owner.discard(&local.dir, digest, fence)?;
    tracing::warn!(target: "kiln.work", object = digest, bytes, "U07 verified corrupt cache bytes discarded");
    Ok(true)
}
