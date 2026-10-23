//! Where a machine's sync groups are kept: one hyper-log file, one group of it for each synced repo
//! (the server) or copy (a runner), so that one flush makes every group's writes durable.

use std::path::Path;

use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_log::{Config, Facts, Log};

pub use hyper_durable::GroupStore;

/// A machine's sync log.
pub type SyncLog = Log<DeviceFile>;
/// One group's part of it.
pub type Store = GroupStore<DeviceFile>;

/// The log's id, the same in every kiln's file.
const LOG_ID: u128 = u128::from_be_bytes(*b"kiln-repo-sync-1");
/// The block the file is written in.
const BLOCK: usize = 4096;
/// Groups one log holds at most: repos on the server, copies on a runner.
pub const GROUPS: usize = 32;
/// Bytes the file may take (it grows as it is used).
const DISK: u64 = 2 << 30;

/// What the log is laid out by. Every value is fixed: a log reopens with the facts it was made with.
fn config(align: Alignment) -> anyhow::Result<Config> {
    Config::derive(&Facts {
        align,
        sealed: false,
        // A step is at most `STEP_CHANGES` changes; one with long paths stays under this.
        largest_entry: 1 << 20,
        disk_bytes: DISK,
        max_groups: GROUPS,
        // An owner compacts a group well within this ([`due`]).
        cadence_entries: 2048,
        cadence_bytes: CADENCE_BYTES,
        uncommitted_entries: 512,
        uncommitted_bytes: 4 << 20,
        cache_bytes: 8 << 20,
    })
    .map_err(|e| anyhow::anyhow!("the sync log's settings: {e:?}"))
}

/// Opens the log at `path`, or makes it there.
pub fn open(path: &Path) -> anyhow::Result<SyncLog> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let fresh = std::fs::metadata(path).map_or(true, |m| m.len() == 0);
    let align = Alignment::new(BLOCK).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let file = DeviceFile::open(path, true, CachingRequest::PreferDirect, align).map_err(|e| anyhow::anyhow!("{}: {e:?}", path.display()))?;
    let config = config(file.layout_block())?;
    if fresh {
        Log::create(file, config, LOG_ID).map_err(|e| anyhow::anyhow!("making the sync log at {}: {e:?}", path.display()))
    } else {
        Log::open(file, config, LOG_ID).map(|(log, _)| log).map_err(|e| anyhow::anyhow!("opening the sync log at {}: {e:?}", path.display()))
    }
}

/// Group `id` of `log`, its records as the log holds them.
pub fn claim(log: &SyncLog, id: u128) -> anyhow::Result<Store> {
    GroupStore::claim(log, id).map_err(|e| anyhow::anyhow!("group {id} of the sync log: {e:?}"))
}

/// Removes group `id`'s records from `log`.
pub fn remove(log: &SyncLog, id: u128) -> anyhow::Result<()> {
    GroupStore::remove(log, id).map_err(|e| anyhow::anyhow!("removing group {id} of the sync log: {e:?}"))
}

/// Bytes of entries between a group's checkpoints the log is laid out for: it retains two of these
/// and what is uncommitted, and refuses a group's writes past that until the group compacts.
const CADENCE_BYTES: u64 = 16 << 20;
/// Entries and bytes a compaction leaves for copies a little behind, at the most: whatever is
/// kept stays within the log's room, so a compaction always frees some.
const KEEP_ENTRIES: u64 = 256;
const KEEP_BYTES: u64 = 2 << 20;

/// Whether a group's log is due to be compacted: by the thesis's rule weighed against its last
/// image, at the latest once it holds half of what the log retains for a group, and at once when
/// it is stalled for room.
pub fn due<M: hyper_durable::StateMachine>(r: &hyper_durable::Replica<Store, M>, since: u64) -> bool {
    r.is_stalled()
        || r.compaction_due(hyper_durable::Compaction { expansion: 4 })
        || r.applied().index.saturating_sub(since) >= 1024
        || r.compactable_bytes() >= CADENCE_BYTES / 2
}

/// How many applied entries a compaction keeps for copies a little behind (`since`: where the log
/// was last compacted to): as many as fit [`KEEP_BYTES`] at the entries' mean size, none when the
/// group is stalled for room, then they take an image.
pub fn keep<M: hyper_durable::StateMachine>(r: &hyper_durable::Replica<Store, M>, since: u64) -> u64 {
    if r.is_stalled() {
        return 0;
    }
    let entries = r.applied().index.saturating_sub(since).max(1);
    let mean = (r.compactable_bytes() / entries).max(1);
    (KEEP_BYTES / mean).min(KEEP_ENTRIES)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Wake, Waker};
    use std::time::{Duration, Instant};

    use hyper_durable::{Output, Replica, Unbounded};

    use super::*;
    use crate::sync::group::{self, Index, SERVER};
    use crate::sync::{Change, Step, Version};

    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Drives `r` until `done` holds, the log's answers waking it.
    fn until(r: &mut Replica<Store, Index>, done: impl Fn(&Replica<Store, Index>) -> bool) -> anyhow::Result<()> {
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            flag.0.store(false, Ordering::SeqCst);
            let now = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            group::pump(r, now, &waker, &mut Output::default(), &mut Vec::new()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            if done(r) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        anyhow::bail!("the replica never got there")
    }

    #[test]
    fn a_group_on_the_disk_log_keeps_what_it_committed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sync.log");
        let step =
            Step { by: "m".into(), member: 2, seq: 1, at: 1, changes: vec![Change { path: "a.txt".into(), to: Some(Version::File { hash: "ab".repeat(32), exec: false }), base: None, bundle: None }] };
        {
            let log = open(&path)?;
            let store = claim(&log, 7)?;
            let depth = hyper_durable::LogStore::depth(&store);
            let mut r = Replica::open(&group::settings(SERVER, depth)?, store, Index::open(None)?, Unbounded).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            r.campaign().map_err(|e| anyhow::anyhow!("{e:?}"))?;
            until(&mut r, Replica::is_leader)?;
            r.propose(Vec::new(), group::entry(&step)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            until(&mut r, |r| r.machine().tree.files.contains_key("a.txt"))?;
            until(&mut r, |r| r.in_flight() == 0)?;
        }
        // Reopened from the file alone (the tree kept in memory only): the log gives it back.
        let log = open(&path)?;
        let store = claim(&log, 7)?;
        let depth = hyper_durable::LogStore::depth(&store);
        let mut r = Replica::open(&group::settings(SERVER, depth)?, store, Index::open(None)?, Unbounded).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        r.campaign().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        until(&mut r, |r| r.is_leader() && r.machine().tree.files.contains_key("a.txt"))?;
        assert_eq!(r.machine().tree.seqs.get(&2), Some(&1));
        Ok(())
    }
}
