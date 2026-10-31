//! U15/B2: offline compatibility inspection, never a second replica or controller.
//! hyper-log's public open repairs its tail before serving reads. Those repairs go only to
//! a bounded memory overlay; the source handle has no write permission. Domain replay must
//! stage its result, then verify both this Source and its snapshot source before publication.

use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use anyhow::Context as _;
use hyper_block::{DiskError, block::BlockFile, buf::Alignment};
use hyper_log::{Config, Fetched, Log, Page, View};
use sha2::{Digest as _, Sha256};

// Compatibility admission budgets, not performance tuning. Exhaustion refuses migration
// and retains the source. Overlay pages and I/O are shared across all recovery handles.
const SOURCE_MAX: u64 = 8 << 30;
const IO_MAX: u64 = 64 << 30;
const OVERLAY_MAX: usize = 64 << 20;
const BLOCK: usize = 4096;
const PAGE_BYTES: u64 = 8 << 20;
const ENTRIES_MAX: u64 = 1_000_000;

#[derive(Clone, PartialEq, Eq)]
struct Generation {
    bytes: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
}
impl Generation {
    fn of(metadata: Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            bytes: metadata.len(), modified: metadata.modified()?, created: metadata.created().ok(),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino(), metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

struct State {
    file: File,
    generation: Generation,
    len: u64,
    pages: BTreeMap<u64, Vec<u8>>,
    io: u64,
}
#[derive(Clone)]
struct Overlay { state: Arc<Mutex<State>>, path: PathBuf, align: Alignment }
impl Overlay {
    fn error(&self, op: &'static str, source: impl Into<std::io::Error>) -> DiskError {
        DiskError::Io { op, path: self.path.clone(), source: source.into() }
    }
    fn invalid(&self, why: &'static str) -> DiskError { self.error("inspect legacy log", std::io::Error::other(why)) }
    fn lock(&self) -> Result<MutexGuard<'_, State>, DiskError> { self.state.lock().map_err(|_| self.invalid("legacy inspection lock poisoned")) }
    fn unchanged(&self, state: &State) -> Result<(), DiskError> {
        let handle = Generation::of(state.file.metadata().map_err(|e| self.error("stat legacy handle", e))?).map_err(|e| self.error("legacy generation", e))?;
        let path = Generation::of(std::fs::metadata(&self.path).map_err(|e| self.error("stat legacy path", e))?).map_err(|e| self.error("legacy generation", e))?;
        if handle != state.generation || path != state.generation { return Err(self.invalid("legacy source changed during inspection")); }
        Ok(())
    }
    fn charge(&self, state: &mut State, bytes: usize) -> Result<(), DiskError> {
        let bytes = u64::try_from(bytes).map_err(|_| self.invalid("legacy I/O size overflow"))?;
        state.io = state.io.checked_add(bytes).filter(|n| *n <= IO_MAX).ok_or_else(|| self.invalid("legacy inspection exceeds cumulative I/O budget"))?;
        Ok(())
    }
    fn original(&self, state: &mut State, bytes: &mut [u8], offset: u64) -> Result<(), DiskError> {
        self.charge(state, bytes.len())?;
        state.file.seek(SeekFrom::Start(offset)).and_then(|_| state.file.read_exact(bytes)).map_err(|e| self.error("read legacy source", e))
    }
    fn digest(&self) -> anyhow::Result<String> {
        let mut state = self.lock()?;
        self.unchanged(&state)?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0u8; 64 << 10];
        let mut at = 0u64;
        while at < state.generation.bytes {
            let len = usize::try_from(state.generation.bytes.checked_sub(at).context("legacy digest offset")?.min(64 << 10))?;
            let bytes = buffer.get_mut(..len).context("legacy digest buffer")?;
            self.original(&mut state, bytes, at)?;
            hash.update(bytes);
            at = at.checked_add(u64::try_from(len)?).context("legacy digest overflow")?;
        }
        self.unchanged(&state)?;
        Ok(hex::encode(hash.finalize()))
    }
}
impl BlockFile for Overlay {
    fn alignment(&self) -> Alignment { self.align }
    fn len(&self) -> Result<u64, DiskError> {
        let state = self.lock()?;
        self.unchanged(&state)?;
        Ok(state.len)
    }
    fn read_exact_at(&self, bytes: &mut [u8], offset: u64) -> Result<(), DiskError> {
        let mut state = self.lock()?;
        self.unchanged(&state)?;
        let end = offset.checked_add(u64::try_from(bytes.len()).map_err(|_| self.invalid("legacy read size overflow"))?).ok_or_else(|| self.invalid("legacy read offset overflow"))?;
        if end > state.len { return Err(self.error("read legacy overlay", std::io::ErrorKind::UnexpectedEof)); }
        self.charge(&mut state, bytes.len())?;
        bytes.fill(0);
        if offset < state.generation.bytes {
            let len = usize::try_from(state.generation.bytes.min(end).checked_sub(offset).ok_or_else(|| self.invalid("legacy read range"))?).map_err(|_| self.invalid("legacy read size"))?;
            let target = bytes.get_mut(..len).ok_or_else(|| self.invalid("legacy read buffer"))?;
            // The complete transfer was charged above; avoid double charging the base read.
            state.file.seek(SeekFrom::Start(offset)).and_then(|_| state.file.read_exact(target)).map_err(|e| self.error("read legacy source", e))?;
        }
        let block = u64::try_from(BLOCK).map_err(|_| self.invalid("legacy block size"))?;
        let first_page = offset.checked_div(block).and_then(|n| n.checked_mul(block)).ok_or_else(|| self.invalid("legacy read page"))?;
        for (&page, data) in state.pages.range(first_page..end) {
            let page_end = page.checked_add(block).ok_or_else(|| self.invalid("legacy page overflow"))?;
            if page_end <= offset { continue; }
            let first = page.max(offset);
            let last = page_end.min(end);
            let src_start = usize::try_from(first.checked_sub(page).ok_or_else(|| self.invalid("legacy page range"))?).map_err(|_| self.invalid("legacy page range"))?;
            let src_end = usize::try_from(last.checked_sub(page).ok_or_else(|| self.invalid("legacy page range"))?).map_err(|_| self.invalid("legacy page range"))?;
            let dst_start = usize::try_from(first.checked_sub(offset).ok_or_else(|| self.invalid("legacy page range"))?).map_err(|_| self.invalid("legacy page range"))?;
            let dst_end = usize::try_from(last.checked_sub(offset).ok_or_else(|| self.invalid("legacy page range"))?).map_err(|_| self.invalid("legacy page range"))?;
            bytes.get_mut(dst_start..dst_end).ok_or_else(|| self.invalid("legacy read buffer"))?.copy_from_slice(data.get(src_start..src_end).ok_or_else(|| self.invalid("legacy overlay page"))?);
        }
        self.unchanged(&state)
    }
    fn write_all_at(&self, bytes: &[u8], offset: u64) -> Result<(), DiskError> {
        let mut state = self.lock()?;
        self.unchanged(&state)?;
        self.charge(&mut state, bytes.len())?;
        let end = offset.checked_add(u64::try_from(bytes.len()).map_err(|_| self.invalid("legacy repair size"))?).ok_or_else(|| self.invalid("legacy repair offset"))?;
        let max = state.generation.bytes.checked_add(u64::try_from(OVERLAY_MAX).map_err(|_| self.invalid("legacy overlay budget"))?).ok_or_else(|| self.invalid("legacy overlay size"))?;
        if end > max { return Err(self.invalid("legacy repair exceeds overlay extent budget")); }
        let block = u64::try_from(BLOCK).map_err(|_| self.invalid("legacy block size"))?;
        let mut at = offset;
        while at < end {
            let page = at.checked_div(block).and_then(|n| n.checked_mul(block)).ok_or_else(|| self.invalid("legacy repair page"))?;
            if !state.pages.contains_key(&page) {
                if state.pages.len() >= OVERLAY_MAX / BLOCK { return Err(self.invalid("legacy recovery exceeds memory overlay budget")); }
                let mut data = vec![0; BLOCK];
                if page < state.generation.bytes {
                    let len = usize::try_from(state.generation.bytes.checked_sub(page).ok_or_else(|| self.invalid("legacy page length"))?.min(block)).map_err(|_| self.invalid("legacy page length"))?;
                    self.original(&mut state, data.get_mut(..len).ok_or_else(|| self.invalid("legacy page buffer"))?, page)?;
                }
                state.pages.insert(page, data);
            }
            let next = page.checked_add(block).ok_or_else(|| self.invalid("legacy repair page overflow"))?.min(end);
            let src_start = usize::try_from(at.checked_sub(offset).ok_or_else(|| self.invalid("legacy repair range"))?).map_err(|_| self.invalid("legacy repair range"))?;
            let src_end = usize::try_from(next.checked_sub(offset).ok_or_else(|| self.invalid("legacy repair range"))?).map_err(|_| self.invalid("legacy repair range"))?;
            let dst_start = usize::try_from(at.checked_sub(page).ok_or_else(|| self.invalid("legacy repair range"))?).map_err(|_| self.invalid("legacy repair range"))?;
            let dst_end = usize::try_from(next.checked_sub(page).ok_or_else(|| self.invalid("legacy repair range"))?).map_err(|_| self.invalid("legacy repair range"))?;
            state.pages.get_mut(&page).and_then(|data| data.get_mut(dst_start..dst_end)).ok_or_else(|| self.invalid("legacy repair page missing"))?.copy_from_slice(bytes.get(src_start..src_end).ok_or_else(|| self.invalid("legacy repair buffer"))?);
            at = next;
        }
        state.len = state.len.max(end);
        self.unchanged(&state)
    }
    fn sync_data(&self) -> Result<(), DiskError> { let state = self.lock()?; self.unchanged(&state) }
    fn try_clone(&self) -> Result<Self, DiskError> { Ok(self.clone()) }
}

/// Retain until the domain's import is ready to publish. Verifies the original bytes,
/// not the harmless repair overlay. Verification exhaustion is a migration refusal.
pub(crate) struct Source { overlay: Overlay, digest: String }
impl Source {
    pub(crate) fn digest(&self) -> &str { &self.digest }
    pub(crate) fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.overlay.digest()? == self.digest, "legacy source content changed during inspection; source retained");
        Ok(())
    }
}

pub(crate) struct LegacyLog { log: Log<Overlay>, source: Source, view: Option<View>, group: u128 }
impl LegacyLog {
    pub(crate) fn open(path: &Path, config: Config, id: u128, group: u128) -> anyhow::Result<Option<Self>> {
        anyhow::ensure!(config.segment_bytes <= 64 << 20 && config.max_groups <= 16 && config.group_entries <= ENTRIES_MAX && config.group_bytes <= 1 << 30 && config.group_cache <= 64 << 20 && config.queue_submissions <= 32768, "legacy log configuration exceeds compatibility inspection budgets");
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("open legacy log {}", path.display())),
        };
        let generation = Generation::of(file.metadata()?)?;
        anyhow::ensure!(generation.bytes <= SOURCE_MAX, "legacy log exceeds compatibility source budget; source retained");
        let align = Alignment::new(BLOCK)?;
        let overlay = Overlay { state: Arc::new(Mutex::new(State { file, len: generation.bytes, generation, pages: BTreeMap::new(), io: 0 })), path: path.to_path_buf(), align };
        let source = Source { digest: overlay.digest()?, overlay: overlay.clone() };
        let (log, recovery) = Log::open(overlay, config, id).map_err(|e| anyhow::anyhow!("inspect legacy log without modifying source: {e:?}"))?;
        anyhow::ensure!(!recovery.damaged.contains(&group), "legacy group has damaged durable entries; source retained");
        let view = log.view(group).map_err(|e| anyhow::anyhow!("inspect legacy group: {e:?}"))?;
        if let Some(view) = &view {
            anyhow::ensure!(view.uncertain.is_none(), "legacy group has uncertain durable entries; source retained");
            anyhow::ensure!(Self::commit(view) <= view.last, "legacy committed suffix is missing; source retained");
        }
        source.verify()?;
        Ok(Some(Self { log, source, view, group }))
    }
    pub(crate) fn view(&self) -> Option<&View> { self.view.as_ref() }
    /// Snapshot owners compare their image term with this retained log position.
    pub(crate) fn term(&self, index: u64) -> anyhow::Result<u64> {
        { let state = self.source.overlay.lock()?; self.source.overlay.unchanged(&state)?; }
        self.log.term(self.group, index).map_err(|e| anyhow::anyhow!("inspect legacy snapshot term: {e:?}"))
    }
    fn commit(view: &View) -> u64 { view.hard_state.map_or(view.start.index, |h| h.commit.max(view.start.index)).max(view.released) }

    /// Visits every committed entry after the snapshot cursor, never the uncommitted tail.
    /// The caller stages domain state; only a verified Source permits its final publication.
    pub(crate) fn replay(self, after: u64, mut visit: impl FnMut(u64, u64, &[u8]) -> anyhow::Result<()>) -> anyhow::Result<Source> {
        if let Some(view) = &self.view {
            let commit = Self::commit(view);
            anyhow::ensure!(after >= view.start.index, "legacy snapshot precedes compacted log start; source retained");
            anyhow::ensure!(after <= commit, "legacy snapshot is ahead of durable commit; source retained");
            anyhow::ensure!(commit.checked_sub(after).context("legacy replay range")? <= ENTRIES_MAX, "legacy committed suffix exceeds replay entry budget; source retained");
            let high = commit.checked_add(1).context("legacy committed index exhausted")?;
            let mut next = after.checked_add(1).context("legacy snapshot index exhausted")?;
            let mut previous_term = self.log.term(self.group, after).map_err(|e| anyhow::anyhow!("legacy snapshot term is unavailable: {e:?}"))?;
            let mut into = Fetched::new();
            while next < high {
                into = self.log.fetch(self.group, next, high, Page { max_bytes: PAGE_BYTES, per_entry: 24 }, into).map_err(|e| anyhow::anyhow!("legacy committed suffix is unreadable: {e:?}"))?;
                anyhow::ensure!(!into.is_empty(), "legacy committed suffix has a gap; source retained");
                for (term, bytes) in into.iter() {
                    anyhow::ensure!(next < high && term >= previous_term && view.hard_state.is_none_or(|h| term <= h.term), "legacy committed suffix term/index mismatch; source retained");
                    visit(next, term, bytes)?;
                    previous_term = term;
                    next = next.checked_add(1).context("legacy replay index exhausted")?;
                }
            }
        } else { anyhow::ensure!(after == 0, "legacy snapshot has no matching log group; source retained"); }
        self.log.close().map_err(|e| anyhow::anyhow!("close legacy inspection: {e:?}"))?;
        self.source.verify()?;
        Ok(self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_block::file::{CachingRequest, DeviceFile};
    use hyper_log::{Entries, Entry, HardState, Update, Waits};
    use std::io::Write as _;

    const ID: u128 = 0x006b_696c_6e2d_6364;
    const GROUP: u128 = 1;
    fn config() -> Config {
        Config { segment_bytes: 1 << 20, max_segments: 8, max_groups: 8, group_entries: 1024, group_bytes: 8 << 20, group_cache: 1 << 20, queue_submissions: 64, waits: Waits::Never }
    }
    fn directory() -> anyhow::Result<tempfile::TempDir> {
        let cache = PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache");
        std::fs::create_dir_all(&cache)?;
        Ok(tempfile::tempdir_in(cache)?)
    }
    fn fixture(path: &Path) -> anyhow::Result<()> {
        let device = DeviceFile::open(path, true, CachingRequest::Buffered, Alignment::new(BLOCK)?)?;
        let log = Log::create(device, config(), ID).map_err(|e| anyhow::anyhow!("create fixture: {e:?}"))?;
        log.write(GROUP, Update { entries: Some(Entries { first: 1, entries: vec![Entry { term: 1, bytes: b"committed".to_vec() }, Entry { term: 1, bytes: b"uncommitted".to_vec() }] }), hard_state: Some(HardState { term: 1, vote: 1, commit: 1, held: None }), ..Update::default() }).map_err(|e| anyhow::anyhow!("write fixture: {e:?}"))?;
        log.close().map_err(|e| anyhow::anyhow!("close fixture: {e:?}"))?;
        Ok(())
    }

    // U15/B2: harmless recovery repairs are accepted without modifying the legacy source;
    // only its committed prefix reaches domain replay, never a durable uncommitted tail.
    #[test]
    fn u15_legacy_inspection_preserves_source_and_excludes_uncommitted_tail() -> anyhow::Result<()> {
        let dir = directory()?;
        let path = dir.path().join("cd.log");
        fixture(&path)?;
        let original = std::fs::read(&path)?;
        let log = LegacyLog::open(&path, config(), ID, GROUP)?.context("log missing")?;
        anyhow::ensure!(log.view().context("group missing")?.last == 2, "fixture lost its uncommitted tail");
        anyhow::ensure!(log.term(1)? == 1, "snapshot term unavailable");
        let mut replayed = Vec::new();
        let source = log.replay(0, |index, term, bytes| { replayed.push((index, term, bytes.to_vec())); Ok(()) })?;
        anyhow::ensure!(replayed == vec![(1, 1, b"committed".to_vec())], "wrong committed suffix");
        source.verify()?;
        anyhow::ensure!(std::fs::read(&path)? == original, "inspection mutated original log");
        anyhow::ensure!(source.digest() == hex::encode(Sha256::digest(&original)), "source identity included overlay repairs");
        Ok(())
    }

    // U15/B2: an image that cannot bridge the retained committed prefix refuses migration;
    // a source changed after replay can never pass the final publication fence.
    #[test]
    fn u15_legacy_generation_and_snapshot_mismatches_refuse_cutover() -> anyhow::Result<()> {
        let dir = directory()?;
        let path = dir.path().join("cd.log");
        fixture(&path)?;
        let log = LegacyLog::open(&path, config(), ID, GROUP)?.context("log missing")?;
        anyhow::ensure!(log.replay(2, |_, _, _| Ok(())).is_err(), "snapshot ahead of commit accepted");
        let log = LegacyLog::open(&path, config(), ID, GROUP)?.context("log missing")?;
        let source = log.replay(0, |_, _, _| Ok(()))?;
        let mut external = std::fs::OpenOptions::new().append(true).open(&path)?;
        external.write_all(b"changed generation")?;
        anyhow::ensure!(source.verify().is_err(), "changed source accepted at publication");
        Ok(())
    }
}
