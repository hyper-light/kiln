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
const APPEND_BYTES: u64 = 256 << 10;

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
/// Recent entries the log keeps in memory for members a little behind.
const CACHE_BYTES: u64 = 8 << 20;
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
    })
}

/// The log's layout, from the facts above. Fixed: a log reopens with the facts it was made with.
pub fn log_config(align: Alignment) -> Result<LogConfig, hyper_log::LogError> {
    LogConfig::derive(&Facts {
        align,
        sealed: false,
        largest_entry: ENTRY_BYTES + crate::command::WRITE_HEADER,
        disk_bytes: DISK_BYTES,
        max_groups: 1,
        cadence_entries: CADENCE_ENTRIES,
        cadence_bytes: CADENCE_BYTES,
        uncommitted_entries: UNCOMMITTED_ENTRIES,
        uncommitted_bytes: UNCOMMITTED_BYTES,
        cache_bytes: CACHE_BYTES,
    })
}

/// The group's log at `path`, made there if new, and the group's part of it.
pub fn open_log(path: &Path) -> Result<(Log<DeviceFile>, GroupStore<DeviceFile>), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let fresh = std::fs::metadata(path).map_or(true, |m| m.len() == 0);
    let align = Alignment::new(BLOCK).map_err(|e| format!("{e:?}"))?;
    let file = DeviceFile::open(path, true, CachingRequest::PreferDirect, align).map_err(|e| format!("{}: {e:?}", path.display()))?;
    let config = log_config(file.layout_block()).map_err(|e| format!("the observe log's layout: {e:?}"))?;
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
/// answers.
pub fn pump<L: LogStore, M: StateMachine>(r: &mut Replica<L, M>, now: u64, waker: &Waker, out: &mut Output<M::Answer>, messages: &mut Vec<Message>, answers: &mut Vec<M::Answer>) -> Result<(), ReplicaError> {
    // Each drive gives what it can without waiting: a bound on rounds keeps a replica that always
    // has more from holding its owner's thread.
    for _ in 0..1024 {
        out.clear();
        let driven = r.drive(now, waker, out)?;
        messages.append(&mut out.messages);
        answers.append(&mut out.answers);
        if !driven.more {
            break;
        }
    }
    Ok(())
}
