//! The observe group's settings and its log (`docs/observe.md` §5): what every member of the group
//! states alike, how its hyper-log file is laid out, and when its log is compacted.
//!
//! One log file a node, holding the one group; the store beside it in a folder of its own. A
//! replica's entries are write batches ([`crate::command`]), at most [`ENTRY_BYTES`] each, which
//! the node that takes a write splits to (`crate::batch::Builder::byte_len`).

use std::path::Path;
use std::task::Waker;
use std::time::Duration;

use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{GroupStore, LogStore, Output, Replica, ReplicaError, Settings, StateMachine};
use hyper_log::{Config as LogConfig, Facts, Log};
use hyper_raft::proto::{ConfChangeSingle, ConfChangeTransition, ConfChangeType, ConfChangeV2, ConfState, Message};
use hyper_raft::{Config, Elections, Limits, Stated};

/// The group's id in its log.
pub const GROUP: u128 = 1;
/// The log's id, the same in every kiln's file: "kiln-observe-v1." in ASCII.
pub const LOG_ID: u128 = u128::from_be_bytes(*b"kiln-observe-v1.");

/// Bytes one frame of the log holds, at the least: what one flush carries when writes queue
/// behind a flush. Measured (`docs/observe.md` §12.9: 64 senders of the corpus's 10 s batches on
/// NVMe, three runs each), the median ingest climbs from 3.8 M samples/s at 1 MiB to 7.4 M at
/// 8 MiB with commit p50 steady near 14 ms, and falls back at 16 MiB; below some 16 writes in
/// flight the frame makes no difference. Its cost is the log's frame buffers, 8 MiB each.
pub const FRAME_BYTES: usize = 8 << 20;

/// The largest entry: kiln's sync log's (1 MiB), which holds a batch of some 67,000 samples at the
/// 15.5 bytes a sample batches take (§12.3) or a few MiB of log lines at LZ4's ratio on them (§12.6).
pub const ENTRY_BYTES: usize = 1 << 20;
/// The largest message a member sends as hyper-raft's wire writes it, but for an append of one
/// entry larger, which goes alone: kiln's lanes carry a message in parts (`crate::sync::wire`'s
/// `PART`), so this bounds what an append gathers, not what a lane carries. Kiln's sync groups'.
pub const MESSAGE_BYTES: usize = 768 << 10;
/// The most members a configuration names: the server and every runner, as kiln's sync groups.
/// Every member states the same.
pub const MEMBERS: usize = 64;
/// Bytes a member's raft queues may hold.
const QUEUE_MEMORY: usize = 64 << 20;
/// An append's entries, at the most (an entry larger goes alone).
pub const APPEND_BYTES: u64 = 256 << 10;

/// How often an owner ticks its replica; a leader beats every [`BEAT`] ticks and a follower that
/// hears no leader for three beats stands: kiln's sync groups' timing, over the same links.
pub const TICK: Duration = Duration::from_millis(100);
pub const BEAT: usize = 10;
const ELECTION: usize = BEAT * 3;

/// Bytes of entries between the store's cuts the log is laid out for: it retains two of these and
/// what is uncommitted, and the owner cuts at half of one ([`due`]). At §12.3's 15.5 bytes a sample
/// in batches against 1.57 in the head, a cut finds about 3 MiB of samples in the head.
pub const CADENCE_BYTES: u64 = 64 << 20;
/// Entries between cuts, at the most: a cadence of the smallest writes (a sample each).
const CADENCE_ENTRIES: u64 = CADENCE_BYTES / 64;
/// What may be uncommitted past the cadence: every sender's write in flight (one each).
const UNCOMMITTED_ENTRIES: u64 = MEMBERS as u64 * 16;
const UNCOMMITTED_BYTES: u64 = 16 * ENTRY_BYTES as u64;
/// Recent entries a learner's log keeps in memory, and the leader's before it has learned what its learners lack:
/// one append's (goal 11, plan 01a1a87b2e56 step 5). The replica keeps past this budget whatever it has yet to apply
/// and whatever a learner in step has yet to acknowledge of what it was sent (hyper-durable's `Replica::keep`, through
/// `LogStore::keep_after`), so the budget serves only a learner out of step: one that dropped and probes, or one the
/// apply point has passed, sent nothing past it. What it lacks past the budget is read from the file, on the leader's
/// thread, which holds the leader's own writes (each acknowledged once applied) behind the read: so the leader's log
/// holds what its learners lacked back from away, as learned ([`cache_bytes`], `crate::catch_up`; kiln's goal 11
/// audit, row B21). It was 8 MiB, and the log held 3.19 MB of it in the lab (train 4's heap profile, `update_of`
/// under `pump`): a round's whole log, about 20 KB a second, so the 256 KiB are about 13 s of a learner's lag.
const CACHE_BYTES: u64 = APPEND_BYTES;

/// The leader's log's cache, its one group's whole share: the most bytes a learner back from away lacked, as the runs
/// before this one learned it (`crate::catch_up`), one append at the least (nothing learned yet), within the log's own
/// file ([`DISK_BYTES`]).
pub fn cache_bytes(learned: u64) -> u64 {
    learned.clamp(CACHE_BYTES, DISK_BYTES)
}
/// Bytes the log's file may take: its persist area and the segments two cadences and what is
/// uncommitted fill, with room for one more cadence of slack.
const DISK_BYTES: u64 = 3 * CADENCE_BYTES + UNCOMMITTED_BYTES + (16 << 20);
/// The block the file is written in.
const BLOCK: usize = 4096;

/// A member's settings: `id` in the group, on a store that holds `depth` writes out at once.
pub fn settings(id: u64, depth: usize) -> Result<Settings, hyper_raft::Error> {
    let limits = Limits::derive(Stated { message: MESSAGE_BYTES, members: MEMBERS, memory: QUEUE_MEMORY, depth })?;
    Ok(Settings {
        core: Config { election_tick: ELECTION, heartbeat_tick: BEAT, max_size_per_msg: APPEND_BYTES, check_quorum: true, pre_vote: true, seed: id, ..Config::new(id, limits) },
        elections: Elections::Ticks,
        // Woken by events: the commit is written alone at the first moment no write is out.
        quiet: Duration::ZERO,
        // Today's: a peer's heartbeats wait on its flush proof, and a leader applies only what a
        // majority's disks hold, its own among them (hyper-durable's `Disk`).
        disk: hyper_durable::Disk::Flushed,
    })
}

/// The log's layout, from the facts above. Fixed: a log reopens with the facts it was made with.
pub fn log_config(align: Alignment) -> Result<LogConfig, hyper_log::LogError> {
    log_config_framed(align, FRAME_BYTES)
}

/// The log's layout with frames of `frame` bytes at least: hyper-log lays every update that came
/// while it flushed into one frame and flushes once a frame, so a frame is what one flush carries.
pub fn log_config_framed(align: Alignment, frame: usize) -> Result<LogConfig, hyper_log::LogError> {
    log_config_with(align, frame, CACHE_BYTES)
}

/// [`log_config_framed`] with a cache of `cache` bytes ([`cache_bytes`]).
pub fn log_config_with(align: Alignment, frame: usize, cache: u64) -> Result<LogConfig, hyper_log::LogError> {
    LogConfig::derive(&facts(align, frame, cache))
}

/// The facts the log is laid out by, its cache `cache` bytes. Of them only the cache may differ from
/// the facts a log file was made with: the rest give its segments and slots, which a log reopens by.
fn facts(align: Alignment, frame: usize, cache: u64) -> Facts {
    Facts {
        align,
        sealed: false,
        // The frame holds the largest entry in a record of its own (hyper-log's rule), so stating
        // the frame as the largest entry sizes it; no entry is larger than ENTRY_BYTES.
        largest_entry: frame.max(ENTRY_BYTES + crate::command::WRITE_HEADER),
        disk_bytes: DISK_BYTES,
        max_groups: 1,
        cadence_entries: CADENCE_ENTRIES,
        cadence_bytes: CADENCE_BYTES,
        uncommitted_entries: UNCOMMITTED_ENTRIES,
        uncommitted_bytes: UNCOMMITTED_BYTES,
        cache_bytes: cache,
    }
}

/// The group's log at `path`, made there if new, and the group's part of it.
pub fn open_log(path: &Path) -> Result<(Log<DeviceFile>, GroupStore<DeviceFile>), String> {
    open_log_framed(path, FRAME_BYTES)
}

/// [`open_log`] with frames of `frame` bytes at least.
pub fn open_log_framed(path: &Path, frame: usize) -> Result<(Log<DeviceFile>, GroupStore<DeviceFile>), String> {
    open_log_with(path, frame, CACHE_BYTES)
}

/// [`open_log`] with frames of `frame` bytes at least and a cache of `cache` bytes ([`cache_bytes`]): the leader's.
pub fn open_log_with(path: &Path, frame: usize, cache: u64) -> Result<(Log<DeviceFile>, GroupStore<DeviceFile>), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let fresh = std::fs::metadata(path).map_or(true, |m| m.len() == 0);
    let align = Alignment::new(BLOCK).map_err(|e| format!("{e:?}"))?;
    let file = DeviceFile::open(path, true, CachingRequest::PreferDirect, align).map_err(|e| format!("{}: {e:?}", path.display()))?;
    let config = log_config_with(file.layout_block(), frame, cache).map_err(|e| format!("the observe log's layout: {e:?}"))?;
    let log = if fresh {
        Log::create(file, config, LOG_ID).map_err(|e| format!("making the observe log at {}: {e:?}", path.display()))?
    } else {
        Log::open(file, config, LOG_ID).map(|(log, _)| log).map_err(|e| format!("opening the observe log at {}: {e:?}", path.display()))?
    };
    let store = GroupStore::claim(&log, GROUP).map_err(|e| format!("the observe group of {}: {e:?}", path.display()))?;
    Ok((log, store))
}

/// The configuration a group founded with `voters` starts from, on every member: they alone, every
/// other member a change the log holds.
pub fn founding(voters: &[u64]) -> ConfState {
    ConfState { voters: voters.to_vec(), ..ConfState::default() }
}

/// One change of configuration: a member added as a learner, promoted to a voter, or removed.
pub fn change(member: u64, kind: ConfChangeType) -> ConfChangeV2 {
    ConfChangeV2 { transition: ConfChangeTransition::Auto, changes: vec![ConfChangeSingle { change_type: kind, node_id: member }], context: Vec::new() }
}

/// Whether the group's log is due to be compacted, which cuts the store: it holds half a cadence
/// of applied entries, or it is stalled for room.
pub fn due<L: LogStore, M: StateMachine>(r: &Replica<L, M>) -> bool {
    r.is_stalled() || r.compactable_bytes() >= CADENCE_BYTES / 2
}

/// How long applied entries wait for a cut at the most when they come slowly (a cadence's bytes take
/// hours): what a log archive's objects wait for (`docs/observe.md` §10.5), Datadog's own archives'
/// 15 minutes (its writer's batches').
pub const CUT_AFTER: Duration = Duration::from_secs(15 * 60);

/// Whether the group's log is due to be compacted (the store cut) for its age: entries are
/// applied past `since` (where the store was last cut), the first of them `waited` ago, and that is
/// `after` or more.
pub fn due_by_age<L: LogStore, M: StateMachine>(r: &Replica<L, M>, since: u64, waited: Duration, after: Duration) -> bool {
    r.applied().index > since && waited >= after
}

/// How many applied entries a compaction keeps for members a little behind: half a cadence's
/// worth at the entries' mean size, so that a member away for that long takes the log, not an
/// image and its files; none when the group is stalled for room. `since` is where the log was
/// last compacted to.
pub fn keep<L: LogStore, M: StateMachine>(r: &Replica<L, M>, since: u64) -> u64 {
    if r.is_stalled() {
        return 0;
    }
    let entries = r.applied().index.saturating_sub(since).max(1);
    let mean = (r.compactable_bytes() / entries).max(1);
    (CADENCE_BYTES / 2 / mean).min(entries)
}

/// Drives `r` until it has nothing more to do now, keeping the messages it sends and what it
/// answers; when to drive it again though nothing arrives (hyper-durable's `Driven::wake`), if
/// ever.
pub fn pump<L: LogStore, M: StateMachine>(r: &mut Replica<L, M>, now: u64, waker: &Waker, out: &mut Output<M::Answer>, messages: &mut Vec<Message>, answers: &mut Vec<M::Answer>) -> Result<Option<u64>, ReplicaError> {
    // Each drive gives what it can without waiting: a bound on rounds keeps a replica that always
    // has more from holding its owner's thread.
    let mut wake = None;
    for _ in 0..1024 {
        out.clear();
        let driven = r.drive(now, waker, out)?;
        messages.append(&mut out.messages);
        answers.append(&mut out.answers);
        wake = driven.wake;
        if !driven.more {
            break;
        }
    }
    Ok(wake)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The observe log keeps one append of recent entries in memory, its one group's whole share of the log's
    /// cache, and nothing else of its layout moves: a log file made with the 8 MiB cache before it reopens with
    /// this one, its segments and slots the same (goal 11, plan 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_the_observe_log_keeps_one_append_in_memory() -> Result<(), String> {
        let align = Alignment::new(BLOCK).map_err(|e| format!("{e:?}"))?;
        let config = log_config(align).map_err(|e| format!("{e:?}"))?;
        assert_eq!(config.max_groups, 1);
        assert_eq!(config.group_cache, APPEND_BYTES, "one append for the one group");
        let before = LogConfig::derive(&facts(align, FRAME_BYTES, 8 << 20)).map_err(|e| format!("{e:?}"))?;
        assert_eq!(LogConfig { group_cache: before.group_cache, ..config }, before, "the cache alone differs");
        assert!(config.segment_bytes >= FRAME_BYTES as u64, "a frame still holds what one flush carries");
        // The leader's, learned (kiln's goal 11 audit, row B21): what its learners lacked back from away, one append at
        // the least, within the file; the rest of the layout the same.
        assert_eq!(cache_bytes(0), APPEND_BYTES, "nothing learned: one append");
        let learned = 5 * APPEND_BYTES + 3;
        let leader = log_config_with(align, FRAME_BYTES, cache_bytes(learned)).map_err(|e| format!("{e:?}"))?;
        assert_eq!(leader.group_cache, learned);
        assert_eq!(LogConfig { group_cache: before.group_cache, ..leader }, before, "the cache alone differs");
        assert_eq!(cache_bytes(u64::MAX), DISK_BYTES, "within the log's file");
        Ok(())
    }
}
