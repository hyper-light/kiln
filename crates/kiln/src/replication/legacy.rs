//! U15: read-only pre-import fence for the retired Kiln cluster log. This is compatibility
//! inspection, never a live replica/controller. A committed write the SQL source lacks requires
//! offline legacy recovery; the importer must not acknowledge a source snapshot that omits it.

use std::path::Path;

use anyhow::Context as _;
use hyper_block::buf::Alignment;
use hyper_log::{Config, Facts};
use super::legacy_read::{LegacyLog, Source};

const LOG_ID: u128 = u128::from_be_bytes(*b"kiln-cluster-gp1");
const GROUP: u128 = 1;

fn settings(align: Alignment) -> anyhow::Result<Config> {
    // U15: retired fixed on-disk settings, retained only to inspect migration input.
    Config::derive(&Facts { align, sealed: false, largest_entry: (8 << 20) + (64 << 10), disk_bytes: 4 << 30, max_groups: 2, cadence_entries: 4096, cadence_bytes: 256 << 20, uncommitted_entries: 2048, uncommitted_bytes: 64 << 20, cache_bytes: 8 << 20 })
        .map_err(|e| anyhow::anyhow!("legacy log settings: {e:?}"))
}

/// Call while the importer's source SQL snapshot is pinned, before creating a Restore. Keep
/// the returned source until final publication and verify it again before completing Restore.
/// Only harmless log recovery writes enter the bounded overlay; the original remains read-only.
/// `applied` and `term` are where the SQL says it was last made durable, `seq` its last write.
pub(crate) fn check(source: &Path, applied: u64, term: u64, seq: u64) -> anyhow::Result<Option<Source>> {
    let parent = source.parent().context("legacy source has no parent")?;
    let path = parent.join("cluster").join("cluster.log");
    let align = Alignment::new(4096).map_err(|e| anyhow::anyhow!("legacy log alignment: {e:?}"))?;
    let Some(log) = LegacyLog::open(&path, settings(align)?, LOG_ID, GROUP)
        .with_context(||format!("U15: {} requires offline legacy recovery before import; source retained",path.display()))? else { return Ok(None); };
    if let Some(view) = log.view() {
        anyhow::ensure!(!view.proposals.iter().any(|p| p.index > applied), "U15: legacy log retains proposals beyond the SQL source; offline legacy recovery is required before import; source retained at {}", path.display());
        anyhow::ensure!(log.term(applied)? == term,"U15: legacy SQL snapshot term disagrees with its log; source retained at {}",path.display());
    }
    // Shared replay validates the snapshot range, committed suffix continuity and generation.
    // The old build wrote each change into kiln.db first, numbered there (`seq`), then into the
    // log, and made kiln.db durable at a log index (`applied`) only now and then: on a log
    // compaction, and on a stop that got that far. So the committed entries past `applied` are
    // writes kiln.db holds already, unless the disk lost one from kiln.db that the log kept (its
    // number past kiln.db's last). The importer can't replay the retired protocol: that one is a
    // refusal, never a replay into partially imported canonical state.
    let source = log.replay(applied, |index, term, bytes| held(index, term, bytes, seq, &path))?;
    Ok(Some(source))
}

/// A committed entry the SQL holds: a configuration change or an empty entry (a new leader's, a
/// barrier) changes no rows; a write is held once the SQL's last write is numbered at or past it.
fn held(index: u64, term: u64, bytes: &[u8], seq: u64, path: &Path) -> anyhow::Result<()> {
    let entry = hyper_durable::decode_entry(index, term, bytes).with_context(|| format!("U15: legacy log entry {index} is unreadable; source retained at {}", path.display()))?;
    if entry.kind != hyper_raft::proto::EntryType::EntryNormal || entry.data.is_empty() { return Ok(()); }
    // The old build's entry: its write's number, its part and its parts, then its rows.
    let ((write, _, _), _) = postcard::take_from_bytes::<(u64, u32, u32)>(entry.data).map_err(|e| anyhow::anyhow!("U15: legacy log entry {index} isn't a write this build reads ({e}); source retained at {}", path.display()))?;
    anyhow::ensure!(write <= seq, "U15: legacy SQL source lacks write {write}, which its cluster log committed (entry {index}); offline legacy recovery is required before import; source retained at {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_block::file::{CachingRequest, DeviceFile};
    use hyper_log::{Entries, Entry, HardState, Log, Update};
    use std::path::PathBuf;
    use std::io::Write as _;

    /// A retired log at `cluster/cluster.log` beside a `kiln.db`: `entries` from index 1, committed
    /// through `commit`.
    fn retired(entries: Vec<hyper_raft::proto::Entry>, commit: u64) -> anyhow::Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let cache = PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache");
        std::fs::create_dir_all(&cache)?;
        let dir = tempfile::tempdir_in(cache)?;
        let source = dir.path().join("kiln.db");
        let folder = dir.path().join("cluster");
        std::fs::create_dir(&folder)?;
        let path = folder.join("cluster.log");
        let align = Alignment::new(4096)?;
        let device = DeviceFile::open(&path, true, CachingRequest::Buffered, align)?;
        let log = Log::create(device, settings(align)?, LOG_ID).map_err(|e| anyhow::anyhow!("create legacy fixture: {e:?}"))?;
        let term = entries.iter().map(|e| e.term).max().unwrap_or(0);
        let entries = entries.iter().map(|e| Ok(Entry { term: e.term, bytes: hyper_durable::encode_entry(e).context("encode a legacy fixture entry")? })).collect::<anyhow::Result<Vec<_>>>()?;
        log.write(GROUP, Update { entries: Some(Entries { first: 1, entries }), hard_state: Some(HardState { term, vote: 1, commit, held: None }), ..Update::default() }).map_err(|e| anyhow::anyhow!("write legacy fixture: {e:?}"))?;
        log.close().map_err(|e| anyhow::anyhow!("close legacy fixture: {e:?}"))?;
        Ok((dir, source, path))
    }

    /// The old build's write entry: its number, its part and its parts, then its rows (none).
    fn write(term: u64, seq: u64, part: u32, parts: u32) -> anyhow::Result<hyper_raft::proto::Entry> {
        Ok(hyper_raft::proto::Entry { term, data: postcard::to_allocvec(&(seq, part, parts, Vec::<u8>::new()))?, ..Default::default() })
    }

    #[test]
    fn u15_legacy_durable_tail_refuses_import_without_mutating_source() -> anyhow::Result<()> {
        let (_dir, source, path) = retired(vec![write(1, 1, 0, 1)?], 1)?;
        let before = std::fs::read(&path)?;
        let error = check(&source, 0, 0, 0).err().context("a SQL source lacking a committed write was accepted")?;
        anyhow::ensure!(error.to_string().contains("lacks write 1"), "wrong refusal: {error:#}");
        assert!(check(&source,1,2,1).is_err(),"a mismatched SQL snapshot term was accepted");
        assert!(check(&source,2,1,1).is_err(),"a SQL snapshot ahead of durable commit was accepted");
        let inspection=check(&source,1,1,1)?.context("fixture log vanished")?;
        inspection.verify()?;
        assert_eq!(std::fs::read(&path)?,before,"harmless recovery must not change source bytes");
        std::fs::OpenOptions::new().append(true).open(&path)?.write_all(b"changed after inspection")?;
        assert!(inspection.verify().is_err(),"a changed log could publish an imported snapshot");
        Ok(())
    }

    /// U15: the old build made kiln.db durable at a log index only on a compaction or a stop that
    /// got that far, so a committed tail past that point is no loss: its writes are in kiln.db
    /// (adacachy's retired kiln.db: durable at 0, its log's writes 1 to 14, kiln.db's last 14;
    /// production's: 13 writes past its durable point, kiln.db's last past all of them). A new
    /// leader's entry, a barrier, a configuration change and a write in two parts among them.
    #[test]
    fn u15_a_committed_tail_kiln_db_holds_is_imported() -> anyhow::Result<()> {
        use hyper_raft::proto::{Entry as Raft, EntryType};
        let barrier = Raft { term: 1, context: 7u64.to_le_bytes().to_vec(), ..Default::default() };
        let change = Raft { entry_type: EntryType::EntryConfChangeV2, term: 1, data: vec![9; 12], ..Default::default() };
        let (_dir, source, path) = retired(vec![Raft { term: 1, ..Default::default() }, barrier, write(1, 1, 0, 1)?, change, write(1, 2, 0, 2)?, write(1, 2, 1, 2)?], 6)?;
        let before = std::fs::read(&path)?;
        check(&source, 0, 0, 2)?.context("fixture log vanished")?.verify()?;
        check(&source, 3, 1, 5)?.context("fixture log vanished")?.verify()?;
        let error = check(&source, 0, 0, 1).err().context("a SQL source lacking a write's last part was accepted")?;
        anyhow::ensure!(error.to_string().contains("lacks write 2"), "wrong refusal: {error:#}");
        assert_eq!(std::fs::read(&path)?, before, "inspection must not change source bytes");
        Ok(())
    }
}
