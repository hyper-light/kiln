//! Backups and point-in-time recovery to Mantle (CP-T1.12, AC-T1.12.3, AC-T1.12.4;
//! docs/design/backup.md). A database's log is shipped to an object store ([`amphora_api::blobs`],
//! Mantle behind it) one second of commits to an object, as the second ends; a base is a copy of
//! its file, made consistent by the log shipped beside it, as PostgreSQL's base backups are; a
//! restore writes a base and replays the log after it, through the engine's own recovery, up to the
//! last commit of the second asked for.
//!
//! - [`Hooks`]: what a database's writer tells the backup: when each commit was made ([`Marks`])
//!   and whether its log may start again ([`Fence`]): a log only starts again once what it holds is
//!   shipped, until it reaches the most it keeps.
//! - [`plan`]: the pure core: the archive's keys, which objects restore a second, which expire.
//! - [`Shipper`] and [`BaseCopy`]: the shell that ships the log and copies bases.
//! - [`Shipping`]: the thread that ships a database's log and copies its bases as it commits
//!   ([`crate::database::Database::open_backed_up`]).
//! - [`restore()`]: the shell that restores a second.
//! - [`restore_and_check`]: restores the newest whole second into a scratch file and checks it.
//! - [`restore_cut`]: several groups restored to one second, cross-group transactions finished.
//! - [`FolderBlobs`]: the store Amphora owns: a folder, local or a peer's over a mount.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

mod check;
mod cut;
mod folder;
mod object;
pub mod plan;
mod restore;
mod ship;
mod shipping;

pub use check::{
    CHECK_METRIC, Check, CheckCounters, CheckFailure, RowDigest, check_due, digest, newest_second,
    restore_and_check, restore_and_check_at,
};
pub use cut::restore_cut;
pub use folder::FolderBlobs;
pub use restore::{Restored, expire, restore};
pub use ship::{BaseCopy, Shipped, Shipper};
pub(crate) use shipping::Prepared;
pub use shipping::{Archive, Shipping, Status, system_clock};

/// Nanoseconds since the Unix epoch, from the clock the caller gives (the system's in production, a
/// simulated one in tests).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The nanoseconds of a second: what "any second" is counted in (AC-T1.12.1).
pub const SECOND_NS: u64 = 1_000_000_000;

/// The second a stamp falls in, counted as restore targets are: a commit stamped in
/// `(s - 1, s]` seconds is in second `s`, so restoring to `s` keeps it.
#[must_use]
pub fn second_of(ns: u64) -> u64 {
    ns.div_ceil(SECOND_NS)
}

/// The last commit of a second the writer made, where its log held it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// The log's generation (its header's checkpoint sequence).
    pub seq: u32,
    /// The commit's last frame.
    pub frame: u64,
    /// When it was committed.
    pub ns: u64,
    /// Set when marks were merged to stay within their bound: the commits up to `frame` were made
    /// after this and at or before `ns`, but which of them by which second isn't known.
    pub unknown_from: Option<u64>,
}

#[derive(Default)]
struct MarkState {
    marks: VecDeque<Mark>,
    /// The latest stamp or drain: the next is later (a clock that steps back is held at it).
    last: u64,
    held_back: u64,
    merged: u64,
    /// The newest mark drained: a statement that commits nothing after it makes none.
    drained: Option<(u32, u64)>,
    /// The newest commit stamped: its generation, last frame and stamp.
    stamped: Option<(u32, u64, u64)>,
    /// The second a quiet snapshot holds ([`Marks::quiet_second`]), and whether a commit was
    /// stamped in it since.
    watch: Option<(u64, bool)>,
}

/// When each commit was made: the last commit of each second, in commit order, until the shipper
/// takes them. One mark a second, at most `capacity`; past it the two oldest merge, the second
/// between them then refused as unknown rather than restored wrong (CLAUDE.md §2, §3).
pub struct Marks {
    state: Mutex<MarkState>,
    capacity: usize,
    clock: Clock,
}

/// What the shipper takes from [`Marks`]: the marks, and the time every commit stamped at or
/// before is among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drained {
    pub marks: Vec<Mark>,
    pub through: u64,
}

impl Marks {
    fn state(&self) -> anyhow::Result<MutexGuard<'_, MarkState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("the backup's marks lock was poisoned"))
    }

    /// A stamp later than every one before: a clock that steps back is held at the last.
    fn stamp(&self, state: &mut MarkState) -> u64 {
        let now = (self.clock)();
        let next = state.last.saturating_add(1);
        let ns = if now < next {
            state.held_back = state.held_back.saturating_add(1);
            next
        } else {
            now
        };
        state.last = ns;
        ns
    }

    /// A commit ended at `frame` of generation `seq`: stamped now, under the lock the shipper
    /// drains by, so a drain at `t` has every commit stamped at or before `t`.
    pub fn committed(&self, seq: u32, frame: u64) -> anyhow::Result<()> {
        let mut state = self.state()?;
        // A statement that committed nothing (a read, a BEGIN) leaves the log where it was.
        if state
            .marks
            .back()
            .is_some_and(|last| last.seq == seq && last.frame >= frame)
            || state.drained.is_some_and(|(s, f)| s == seq && f >= frame)
        {
            return Ok(());
        }
        let ns = self.stamp(&mut state);
        if let Some((second, moved)) = state.watch.as_mut()
            && *second == second_of(ns)
        {
            *moved = true;
        }
        if let Some(last) = state.marks.back_mut()
            && last.seq == seq
            && second_of(last.ns) == second_of(ns)
        {
            last.frame = last.frame.max(frame);
            last.ns = ns;
            let stamped = (seq, last.frame, ns);
            state.stamped = Some(stamped);
            return Ok(());
        }
        state.stamped = Some((seq, frame, ns));
        if state.marks.len() >= self.capacity
            && let Some(oldest) = state.marks.pop_front()
            && let Some(next) = state.marks.front_mut()
        {
            // Merging marks of two generations would claim a frame of one for the other: the
            // older is dropped then, its second unknown as a merged one's is.
            let from = oldest.unknown_from.unwrap_or(oldest.ns);
            next.unknown_from = Some(next.unknown_from.map_or(from, |f| f.min(from)));
            state.merged = state.merged.saturating_add(1);
        }
        state.marks.push_back(Mark {
            seq,
            frame,
            ns,
            unknown_from: None,
        });
        Ok(())
    }

    /// Takes every mark made so far, and the time through which they are all of them.
    pub fn drain(&self) -> anyhow::Result<Drained> {
        let mut state = self.state()?;
        let through = self.stamp(&mut state);
        if let Some(last) = state.marks.back() {
            state.drained = Some((last.seq, last.frame));
        }
        Ok(Drained {
            marks: state.marks.drain(..).collect(),
            through,
        })
    }

    /// Takes in a stamp another group made before a step it sent here (T-T1.12.X2): every commit
    /// stamped here from now on is later, a hybrid logical clock's receive rule (Kulkarni et al.,
    /// "Logical Physical Clocks", OPODIS 2014): a stamp is the larger of the clock's reading and
    /// one past the latest stamp seen, so a commit made after another, by any path of steps, is
    /// stamped after it whatever the two clocks read.
    pub fn observe(&self, ns: u64) -> anyhow::Result<()> {
        let mut state = self.state()?;
        state.last = state.last.max(ns);
        Ok(())
    }

    /// The latest stamp made or seen here: every commit stamped so far is at or before it.
    pub fn latest(&self) -> anyhow::Result<u64> {
        Ok(self.state()?.last)
    }

    /// Lines a reader's snapshot up with the commits' stamps (T-T1.12.C3): under the lock commits
    /// are stamped by, `snapshot` begins a read and says where the log ends. When that is the
    /// newest commit stamped, no commit lies between the snapshot and its stamp, so the snapshot
    /// holds every commit stamped at or before it and none after: the rows of its second, once no
    /// commit is stamped later in that second ([`Marks::settled`]). Which second, if it does: none
    /// when the log moved past the newest stamp (a commit not stamped yet, a log started again).
    pub fn quiet_second(
        &self,
        snapshot: impl FnOnce() -> anyhow::Result<(u32, u64)>,
    ) -> anyhow::Result<Option<u64>> {
        let mut state = self.state()?;
        let Some((seq, frame, ns)) = state.stamped else {
            return Ok(None);
        };
        if snapshot()? != (seq, frame) {
            return Ok(None);
        }
        let second = second_of(ns);
        state.watch = Some((second, false));
        Ok(Some(second))
    }

    /// Whether the snapshot [`Marks::quiet_second`] last lined up holds the rows of `second`: yes
    /// once the clock is past the second's end with no commit stamped in it since (every later
    /// commit is stamped later still), no once one was (or a later snapshot took its place), not
    /// yet while the second runs.
    pub fn settled(&self, second: u64) -> anyhow::Result<Option<bool>> {
        let mut state = self.state()?;
        match state.watch {
            Some((s, false)) if s == second => {
                let now = self.stamp(&mut state);
                Ok(if now / SECOND_NS >= second {
                    Some(true)
                } else {
                    None
                })
            }
            _ => Ok(Some(false)),
        }
    }

    /// How often the clock stepped back and was held, and how many marks were merged.
    pub fn counts(&self) -> anyhow::Result<(u64, u64)> {
        let state = self.state()?;
        Ok((state.held_back, state.merged))
    }
}

/// The log's restarts since the last that ended a generation with frames in it (an empty one ends
/// no frames, so is never forced). The engine's restart raises the header's checkpoint sequence by
/// one (`restart_wal_header`, as SQLite's `walRestartHdr`), so the generations they ended run on
/// from `seq` one by one, and a log restarted twice with no write between keeps its link whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ended {
    /// The generation that ended with frames in it, its last frame, and whether it was started
    /// again before all of it was shipped.
    seq: u32,
    frame: u64,
    forced: bool,
    /// How many empty generations ended after it, one after another.
    empties: u32,
}

impl Ended {
    /// The generation the latest restart began.
    fn next(&self) -> u32 {
        self.seq.wrapping_add(self.empties).wrapping_add(1)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FenceState {
    /// The last frame shipped, of generation `.0`.
    shipped: Option<(u32, u64)>,
    ended: Option<Ended>,
}

/// What a log restart asks of the backup: whether all the log holds is shipped.
#[derive(Default)]
pub struct Fence {
    state: Mutex<FenceState>,
}

impl Fence {
    fn state(&self) -> anyhow::Result<MutexGuard<'_, FenceState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("the backup's fence lock was poisoned"))
    }

    pub(crate) fn shipped(&self, seq: u32, frame: u64) -> anyhow::Result<()> {
        self.state()?.shipped = Some((seq, frame));
        Ok(())
    }

    /// Whether the log, at `frame` of generation `seq`, is shipped whole.
    pub(crate) fn holds(&self, seq: u32, frame: u64) -> anyhow::Result<bool> {
        Ok(frame == 0
            || self
                .state()?
                .shipped
                .is_some_and(|(s, f)| s == seq && f >= frame))
    }

    pub(crate) fn ended(&self, seq: u32, frame: u64, forced: bool) -> anyhow::Result<()> {
        let mut state = self.state()?;
        state.ended = match state.ended {
            Some(mut e) if frame == 0 && e.next() == seq => {
                e.empties = e.empties.wrapping_add(1);
                Some(e)
            }
            _ => Some(Ended {
                seq,
                frame,
                forced,
                empties: 0,
            }),
        };
        Ok(())
    }

    /// Whether the log ran unbroken from frame `frame` of generation `old`, as it ended, to the start
    /// of generation `new`: every restart between them was through [`crate::db::Db::restart_log`]
    /// and none of them forced (T-T1.12.D1).
    pub(crate) fn ended_whole(&self, old: u32, frame: u64, new: u32) -> anyhow::Result<bool> {
        let Some(e) = self.state()?.ended else {
            return Ok(false);
        };
        if e.next() != new {
            return Ok(false);
        }
        if old == e.seq {
            return Ok(frame == e.frame && !e.forced);
        }
        // One of the empty generations after it: it ended at frame 0.
        let after = old.wrapping_sub(e.seq);
        Ok(frame == 0 && (1..=e.empties).contains(&after))
    }
}

/// What a database's writer tells its backup ([`crate::db::Db::back_up`]).
pub struct Hooks {
    pub marks: Marks,
    pub fence: Fence,
}

impl Hooks {
    /// Hooks stamping commits by `clock`, keeping at most `capacity` marks between drains: the
    /// frames a log holds at most between restarts (decision 31's [`crate::db::Db::keep_log`]),
    /// since a mark is a commit and a commit at least a frame.
    #[must_use]
    pub fn new(clock: Clock, capacity: u64) -> Arc<Self> {
        Arc::new(Self {
            marks: Marks {
                state: Mutex::new(MarkState::default()),
                capacity: usize::try_from(capacity).unwrap_or(usize::MAX).max(2),
                clock,
            },
            fence: Fence::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clock() -> (Arc<AtomicU64>, Clock) {
        // Just past a second's start: a stamp at exactly 10 s is in second 10 ((9, 10] s) and one
        // half a second later in second 11, so the tests begin inside a second, as most stamps are.
        let now = Arc::new(AtomicU64::new(10 * SECOND_NS + 1));
        let read = Arc::clone(&now);
        (now, Arc::new(move || read.load(Ordering::SeqCst)))
    }

    /// T-T1.12.P1: commits within a second keep one mark, its last; a new second a new mark.
    #[test]
    fn t_t1_12_p1_one_mark_a_second() {
        let (now, clock) = clock();
        let hooks = Hooks::new(clock, 100);
        hooks.marks.committed(1, 3).unwrap();
        now.fetch_add(SECOND_NS / 2, Ordering::SeqCst);
        hooks.marks.committed(1, 5).unwrap();
        now.fetch_add(SECOND_NS, Ordering::SeqCst);
        hooks.marks.committed(1, 9).unwrap();
        let drained = hooks.marks.drain().unwrap();
        assert_eq!(
            drained.marks.iter().map(|m| m.frame).collect::<Vec<_>>(),
            vec![5, 9]
        );
        assert!(drained.through > drained.marks[1].ns);
    }

    /// T-T1.12.P2: a clock that steps back holds stamps at the last, each later than the one
    /// before, and is counted.
    #[test]
    fn t_t1_12_p2_clock_held_back() {
        let (now, clock) = clock();
        let hooks = Hooks::new(clock, 100);
        hooks.marks.committed(1, 1).unwrap();
        now.fetch_sub(5 * SECOND_NS, Ordering::SeqCst);
        hooks.marks.committed(1, 2).unwrap();
        let drained = hooks.marks.drain().unwrap();
        assert_eq!(drained.marks.len(), 1);
        assert_eq!(drained.marks[0].frame, 2);
        assert_eq!(hooks.marks.counts().unwrap().0, 2);
    }

    /// T-T1.12.P3: past its bound the two oldest marks merge and their seconds are unknown.
    #[test]
    fn t_t1_12_p3_marks_merge_at_their_bound() {
        let (now, clock) = clock();
        let hooks = Hooks::new(clock, 2);
        for frame in 1..=3 {
            hooks.marks.committed(1, frame).unwrap();
            now.fetch_add(SECOND_NS, Ordering::SeqCst);
        }
        let drained = hooks.marks.drain().unwrap();
        assert_eq!(drained.marks.len(), 2);
        assert_eq!(drained.marks[0].frame, 2);
        assert_eq!(drained.marks[0].unknown_from, Some(10 * SECOND_NS + 1));
        assert_eq!(hooks.marks.counts().unwrap().1, 1);
    }

    /// T-T1.12.C3: a snapshot lines up with the stamps only when the log ends at the newest commit
    /// stamped; its second settles once the clock passes the second's end with no commit in it
    /// since, and never once one is stamped in it.
    #[test]
    fn t_t1_12_c3_quiet_second_lines_a_snapshot_up() {
        let (now, clock) = clock();
        let hooks = Hooks::new(clock, 100);
        assert_eq!(
            hooks.marks.quiet_second(|| Ok((1, 0))).unwrap(),
            None,
            "nothing stamped"
        );
        hooks.marks.committed(1, 4).unwrap();
        assert_eq!(
            hooks.marks.quiet_second(|| Ok((1, 5))).unwrap(),
            None,
            "a commit not stamped yet"
        );
        assert_eq!(
            hooks.marks.quiet_second(|| Ok((2, 0))).unwrap(),
            None,
            "the log started again"
        );
        assert_eq!(hooks.marks.quiet_second(|| Ok((1, 4))).unwrap(), Some(11));
        assert_eq!(
            hooks.marks.settled(11).unwrap(),
            None,
            "second 11 still runs"
        );
        assert_eq!(
            hooks.marks.settled(10).unwrap(),
            Some(false),
            "not the second lined up"
        );
        now.fetch_add(SECOND_NS, Ordering::SeqCst);
        assert_eq!(hooks.marks.settled(11).unwrap(), Some(true));
        hooks.marks.committed(1, 6).unwrap();
        assert_eq!(
            hooks.marks.settled(11).unwrap(),
            Some(true),
            "a commit in a later second"
        );

        // A commit stamped within the second lined up: the snapshot isn't that second's rows.
        assert_eq!(hooks.marks.quiet_second(|| Ok((1, 6))).unwrap(), Some(12));
        hooks.marks.committed(1, 8).unwrap();
        now.fetch_add(SECOND_NS, Ordering::SeqCst);
        assert_eq!(hooks.marks.settled(12).unwrap(), Some(false));
    }

    /// T-T1.12.X2: a stamp observed from another group makes every later commit here later than
    /// it, though this group's clock reads earlier.
    #[test]
    fn t_t1_12_x2_observed_stamps_order_later_commits() {
        let (_, clock) = clock();
        let hooks = Hooks::new(clock, 100);
        hooks.marks.committed(1, 1).unwrap();
        let ahead = 15 * SECOND_NS;
        hooks.marks.observe(ahead).unwrap();
        hooks.marks.committed(1, 2).unwrap();
        let drained = hooks.marks.drain().unwrap();
        assert!(drained.marks.last().unwrap().ns > ahead);
        assert!(hooks.marks.latest().unwrap() >= drained.through);
    }

    /// T-T1.12.P11: a log started again twice with no write between keeps its link whole; a
    /// restart forced before its frames were shipped, or one the fence wasn't told of, breaks it.
    #[test]
    fn t_t1_12_p11_fence_follows_restarts_one_by_one() {
        let fence = Fence::default();
        assert!(!fence.ended_whole(4, 9, 5).unwrap(), "no restart seen");
        fence.shipped(4, 9).unwrap();
        fence.ended(4, 9, false).unwrap();
        assert!(fence.ended_whole(4, 9, 5).unwrap());
        assert!(
            !fence.ended_whole(4, 8, 5).unwrap(),
            "ended past what was shipped"
        );
        // Generation 5 holds nothing and starts again: 4 still reaches 6 whole, and so does 5.
        fence.ended(5, 0, false).unwrap();
        assert!(fence.ended_whole(4, 9, 6).unwrap());
        assert!(fence.ended_whole(5, 0, 6).unwrap());
        assert!(!fence.ended_whole(4, 9, 5).unwrap(), "the log is past 5");
        assert!(!fence.ended_whole(4, 9, 7).unwrap(), "6 ended untold");
        // A forced restart breaks the link from it on.
        fence.ended(6, 3, true).unwrap();
        assert!(!fence.ended_whole(6, 3, 7).unwrap());
        assert!(!fence.ended_whole(4, 9, 7).unwrap());
        // An empty generation that doesn't follow on starts afresh.
        fence.ended(9, 0, false).unwrap();
        assert!(fence.ended_whole(9, 0, 10).unwrap());
        assert!(!fence.ended_whole(6, 3, 10).unwrap());
    }
}
