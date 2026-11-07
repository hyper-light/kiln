//! What a group's learners lacked when they came back from away, learned for its log's next open (kiln's goal 11 audit,
//! row B21): kiln.db's cluster group (`crates/kiln/src/store/group.rs`) and observe's (`crates/kiln/src/observe`) both
//! have a leader whose runners follow as learners.
//!
//! A learner back from away (a runner reconnecting) lacks what was written meanwhile. The leader sends it from its log's
//! cache where the cache holds it, and from the log's file where it doesn't: a read that holds the log's device (one job
//! at a time) and the leader's thread (its fetch waits on it) while it's out, so the log's next frame waits, and with it
//! the leader's own writes and every other learner's appends. hyper-log fixes a log's cache as it opens, so what a run
//! sees of its learners sizes the next run's: [`Spans`] watches each learner out of step until it's sent entries again,
//! and what it lacked then, each entry as the core counts it ([`Sizes`]), is the most yet or not; the most is kept
//! beside the log ([`keep`]) and read as it opens again ([`kept`]). A learner that took an image instead lacked what the
//! log no longer held, which no cache serves; one that never comes back counts for nothing.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use hyper_durable::{LogStore, Replica, StateMachine};
use hyper_raft::progress::ProgressState;

/// The bytes of each entry a leader's member applied since it opened, as the core counts an entry's
/// (`EntryRef::encoded_bytes`: the log's own count of it and twenty bytes), from the entry at `from` on, as far back as
/// the log holds them: what a learner back from away lacked, to the byte. Four bytes an entry, as hyper-durable counts
/// its own (an entry is no larger than a message the transport carries).
#[derive(Default, Debug)]
pub struct Sizes {
    from: u64,
    each: VecDeque<u32>,
}

impl Sizes {
    /// Entry `index`, of `bytes`, applied: after the last, or the first of a new run (an image installed).
    pub fn push(&mut self, index: u64, bytes: u64) {
        let next = self.from.saturating_add(u64::try_from(self.each.len()).unwrap_or(u64::MAX));
        if self.each.is_empty() || index != next {
            self.each.clear();
            self.from = index;
        }
        self.each.push_back(u32::try_from(bytes).unwrap_or(u32::MAX));
    }

    /// The log starts past `start`: what it let go of goes here too.
    pub fn let_go_through(&mut self, start: u64) {
        while self.from <= start && self.each.pop_front().is_some() {
            self.from = self.from.saturating_add(1);
        }
        if self.each.is_empty() {
            self.from = self.from.max(start.saturating_add(1));
        }
        if self.each.len() <= self.each.capacity() / 4 {
            self.each.shrink_to(self.each.len().saturating_mul(2));
        }
    }

    /// The bytes of the entries past `after` through `through`: those counted here to the byte, those before them
    /// (applied before the member opened) at `mean` bytes each.
    pub fn between(&self, after: u64, through: u64, mean: u64) -> u64 {
        let first = after.saturating_add(1);
        let known_from = self.from.max(first);
        let unknown = known_from.min(through.saturating_add(1)).saturating_sub(first);
        let skip = usize::try_from(known_from.saturating_sub(self.from)).unwrap_or(usize::MAX);
        let take = usize::try_from(through.saturating_add(1).saturating_sub(known_from)).unwrap_or(usize::MAX);
        let known = self.each.iter().skip(skip).take(take).fold(0u64, |sum, b| sum.saturating_add(u64::from(*b)));
        known.saturating_add(unknown.saturating_mul(mean))
    }
}

/// A leader's learners away (out of step: dropped, and probed until they answer), each with where it stood when the
/// leader last knew, and the most bytes one back from away lacked: this run's and those before it (as kept).
#[derive(Default, Debug)]
pub struct Spans {
    /// The most bytes a learner back from away lacked.
    pub most: u64,
    /// Each learner away, with where it stood (0 while the leader doesn't know: one that started since knows nothing
    /// of it until it answers).
    away: BTreeMap<u64, u64>,
}

impl Spans {
    /// Spans whose most so far is `most` (what a run before kept).
    pub fn new(most: u64) -> Self {
        Self { most, away: BTreeMap::new() }
    }

    /// The learners away now.
    pub fn away(&self) -> usize {
        self.away.len()
    }

    /// `member` as the leader sees it now: how it's sent to (`state`), and where it stands (`matched`). A learner back
    /// from away (sent entries again after it was probed) lacked the entries past where it stood when it went, or, if
    /// the leader didn't know that, past where its first answer put it: gives that entry's index, once it's back.
    pub fn saw(&mut self, member: u64, state: ProgressState, matched: u64) -> Option<u64> {
        match state {
            ProgressState::Probe => {
                let stood = self.away.entry(member).or_insert(matched);
                if *stood == 0 {
                    *stood = matched;
                }
                None
            }
            // An image on its way: the log no longer holds what it lacks.
            ProgressState::Snapshot => {
                self.away.remove(&member);
                None
            }
            ProgressState::Replicate => {
                let stood = self.away.remove(&member)?;
                Some(if stood == 0 { matched } else { stood.min(matched) })
            }
        }
    }

    /// The leader `r`'s learners as it sees them now, its log starting past `start` and its entries' sizes `sizes`: what
    /// each back from away lacked, through the last applied, those applied before it opened at the log's mean. Gives the
    /// most, when it grew (the caller keeps it: [`keep`]).
    pub fn look<L: LogStore, M: StateMachine>(&mut self, r: &Replica<L, M>, sizes: &Sizes, start: u64) -> Option<u64> {
        let applied = r.applied().index;
        // What compaction weighs: the applied entries the log holds past its start, as the core counts each.
        let mean = r.compactable_bytes() / applied.saturating_sub(start).max(1);
        let before = self.most;
        let learners = &r.configuration().learners;
        let tracker = r.core().raft.tracker();
        for &member in learners {
            let Some(p) = tracker.get(member) else { continue };
            if let Some(from) = self.saw(member, p.state, p.matched) {
                self.most = self.most.max(sizes.between(from, applied, mean));
            }
        }
        self.away.retain(|member, _| learners.contains(member));
        (self.most > before).then_some(self.most)
    }
}

/// The most bytes a learner back from away lacked, as a run before kept it in `file`: none if no run did (or its file
/// doesn't read: it's learned again).
pub fn kept(file: &Path) -> u64 {
    std::fs::read_to_string(file).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Keeps `bytes` in `file` for the log's next open, whole or not at all.
pub fn keep(file: &Path, bytes: u64) -> std::io::Result<()> {
    let part = file.with_extension("part");
    std::fs::write(&part, format!("{bytes}\n")).and_then(|()| std::fs::rename(&part, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A learner back from away lacked the entries past where it stood when it went, to the byte, though its first
    /// append took it further; one that took an image, or never came back, counts for nothing; a leader that started
    /// since it went counts from the learner's first answer, and entries applied before it opened at the log's mean.
    #[test]
    fn g11_b21_a_learner_back_from_away_lacked_what_came_past_where_it_stood() {
        use ProgressState::{Probe, Replicate, Snapshot};
        let mut sizes = Sizes::default();
        for index in 101..=400 {
            sizes.push(index, if index % 10 == 0 { 1000 } else { 10 });
        }
        let mut spans = Spans::default();
        assert_eq!(spans.saw(7, Replicate, 400), None, "in step");
        assert_eq!(spans.saw(7, Probe, 200), None);
        assert_eq!(spans.saw(7, Probe, 200), None);
        assert_eq!(spans.saw(7, Replicate, 264), Some(200), "where it stood when it went, not where its first append took it");
        // Entries 201 through 400: 20 of 1000 bytes, 180 of 10.
        assert_eq!(sizes.between(200, 400, 5), 21_800);
        assert_eq!(spans.saw(8, Probe, 50), None);
        assert_eq!(spans.saw(8, Snapshot, 50), None);
        assert_eq!(spans.saw(8, Replicate, 400), None, "an image: no cache serves it");
        assert_eq!(spans.saw(9, Probe, 10), None);
        assert_eq!(spans.away(), 1, "one that never comes back stays away");
        assert_eq!(spans.saw(10, Probe, 0), None);
        assert_eq!(spans.saw(10, Replicate, 380), Some(380), "a leader started since: from its first answer");
        // Entries before the member opened (91 through 100) at the mean, the rest to the byte.
        assert_eq!(sizes.between(90, 110, 7), 10 * 7 + 1000 + 9 * 10);
        sizes.let_go_through(300);
        assert_eq!(sizes.between(300, 400, 0), 10 * 1000 + 90 * 10, "what the log let go goes");
        sizes.push(500, 3);
        assert_eq!(sizes.between(499, 500, 0), 3, "a new run after a gap (an image)");
    }

    /// A member of an observe group driven by hand: its replica, then its log (dropped in that order, as a replica must
    /// be).
    struct Member {
        r: Replica<hyper_durable::GroupStore<hyper_block::file::DeviceFile>, crate::machine::Machine, hyper_durable::Unbounded>,
        _log: hyper_log::Log<hyper_block::file::DeviceFile>,
        out: hyper_durable::Output<(u64, crate::store::Answer)>,
    }

    impl Member {
        /// Member `id` in `dir`, its log's cache `cache` bytes, counting its entries' sizes if `sizes`.
        fn open(dir: &Path, id: u64, cache: u64, sizes: bool) -> Result<Self, String> {
            let (log, store) = crate::group::open_log_with(&dir.join("obs.log"), crate::group::FRAME_BYTES, cache)?;
            let mut machine = crate::machine::Machine::open(&dir.join("store"), crate::hash::Keyed::new(), &crate::group::founding(&[1])).map_err(|e| format!("{e}"))?;
            if sizes {
                machine.count_sizes();
            }
            let settings = crate::group::settings(id, store.depth()).map_err(|e| format!("{e:?}"))?;
            let mut r = Replica::open(&settings, store, machine, hyper_durable::Unbounded).map_err(|e| format!("{e:?}"))?;
            if id == 1 {
                r.campaign().map_err(|e| format!("{e:?}"))?;
            }
            Ok(Self { r, _log: log, out: hyper_durable::Output::default() })
        }
    }

    /// The leader (1, the group's one voter) and a learner (2), the learner's link `up` or not, as a runner's is; what
    /// the leader's learners lacked back from away (the server's owner does as much, `crates/kiln/src/observe`).
    struct Pair {
        leader: Option<Member>,
        learner: Member,
        up: bool,
        spans: Spans,
        file: std::path::PathBuf,
        waker: std::task::Waker,
        start: std::time::Instant,
        ticked: std::time::Instant,
    }

    impl Pair {
        fn leader(&mut self) -> Result<&mut Member, String> {
            self.leader.as_mut().ok_or_else(|| "no leader".to_owned())
        }

        /// One round: both driven, their messages carried while the link is up (the leader told the learner is
        /// unreachable while it's down, as the server's owner tells it), the clock ticked, the leader's learners looked
        /// at.
        fn round(&mut self) -> Result<(), String> {
            let now = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let mut messages = Vec::new();
            let mut answers = Vec::new();
            let waker = self.waker.clone();
            for m in [self.leader.as_mut(), Some(&mut self.learner)].into_iter().flatten() {
                crate::group::pump(&mut m.r, now, &waker, &mut m.out, &mut messages, &mut answers).map_err(|e| format!("{e:?}"))?;
            }
            for m in messages {
                if !self.up && (m.to == 2 || m.from == 2) {
                    if m.to == 2 {
                        let _ = self.leader()?.r.report_unreachable(2);
                    }
                    continue;
                }
                let to = if m.to == 2 { Some(&mut self.learner) } else { self.leader.as_mut() };
                if let Some(to) = to {
                    let _ = to.r.step(m);
                }
            }
            if self.ticked.elapsed() >= crate::group::TICK {
                self.ticked = std::time::Instant::now();
                for m in [self.leader.as_mut(), Some(&mut self.learner)].into_iter().flatten() {
                    let _ = m.r.tick();
                }
            }
            let (spans, file) = (&mut self.spans, &self.file);
            if let Some(l) = self.leader.as_mut()
                && let Some(sizes) = l.r.machine().sizes()
            {
                let start = l.r.log().handle().bounds().map_or(0, |(start, _)| start.index);
                if let Some(most) = spans.look(&l.r, sizes, start) {
                    keep(file, most).map_err(|e| format!("{e}"))?;
                }
                l.r.machine_mut().sizes_let_go_through(start);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
            Ok(())
        }

        /// Rounds until `done`, up to a hang's bound.
        fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) -> Result<(), String> {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(300);
            while !done(self) {
                if std::time::Instant::now() >= end {
                    return Err(format!("timed out waiting for {what}"));
                }
                self.round()?;
            }
            Ok(())
        }

        /// The learner holds everything the leader applied, sent to in step.
        fn in_step(&mut self) -> bool {
            let Some(l) = self.leader.as_ref() else { return false };
            let applied = l.r.applied().index;
            applied > 0 && self.learner.r.applied().index >= applied && self.spans.away() == 0 && l.r.core().raft.tracker().get(2).is_some_and(|p| p.state == ProgressState::Replicate)
        }

        /// The learner away (its link down) while the leader writes `writes` entries of about a quarter append each, then
        /// back: the reads of the leader's log file its catch-up took.
        fn away(&mut self, round: u64, writes: u64) -> Result<u64, String> {
            self.until("the learner in step", Self::in_step)?;
            self.up = false;
            let _ = self.leader()?.r.report_unreachable(2);
            self.until("the learner out of step", |p| p.spans.away() == 1)?;
            let mut x = 0x9e37_79b9_7f4a_7c15_u64 ^ round;
            for seq in 1..=writes {
                // Lines no compressor shortens: each write about a quarter of an append.
                let t0 = i64::try_from(round * 1_000_000 + seq * 1_000).unwrap_or(0);
                let records: Vec<crate::logs::record::Record> = (0..16)
                    .map(|k| {
                        let message: String = (0..4096)
                            .map(|_| {
                                x ^= x << 13;
                                x ^= x >> 7;
                                x ^= x << 17;
                                char::from(b"0123456789abcdef".get(usize::try_from(x % 16).unwrap_or(0)).copied().unwrap_or(b'0'))
                            })
                            .collect();
                        crate::logs::record::Record { ts: t0 * 1_000_000 + k, severity: 9, service: "svc".into(), host: "h".into(), source: "test".into(), message, ..Default::default() }
                    })
                    .collect();
                let mut b = crate::batch::Builder::new();
                b.logs(&records);
                let entry = b.finish_write(0x5e00 + round, seq, t0);
                self.leader()?.r.propose(Vec::new(), entry).map_err(|e| format!("{e:?}"))?;
            }
            let last = self.leader()?.r.core().raft.log().last_index().map_err(|e| format!("{e:?}"))?;
            self.until("the writes applied", |p| p.leader.as_ref().is_some_and(|l| l.r.applied().index >= last))?;
            let reads = self.leader()?.r.log().handle().asked();
            self.up = true;
            self.until("the learner caught up", |p| p.learner.r.applied().index >= last && p.spans.away() == 0)?;
            self.round()?;
            Ok(self.leader()?.r.log().handle().asked().saturating_sub(reads))
        }
    }

    /// Kiln's goal 11 audit, row B21, for observe's group (de07b0fc4d0 cut its log's cache to one append, 256 KiB): a
    /// learner back from away (a runner reconnecting) is sent what it lacks from the leader's log cache, not its file,
    /// once the leader has seen what its learners lack. Its first run opens with one append (nothing seen yet): the
    /// learner, away while the leader writes sixteen quarter appends, is caught up from the log's file, on the leader's
    /// thread, which holds the leader's own writes behind the read. What it lacked is kept, and opened again (a deploy),
    /// the leader's cache holds that: away again for twelve, no read of the file. With the one append the cache was, the
    /// second run reads the file as the first does.
    #[test]
    fn g11_b21_observes_learner_back_from_away_is_caught_up_from_memory_once_the_leader_saw_what_it_lacks() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| format!("{e}"))?;
        let (one, two) = (dir.path().join("one"), dir.path().join("two"));
        let file = one.join("catch-up");
        let opened = crate::group::cache_bytes(kept(&file));
        assert_eq!(opened, crate::group::APPEND_BYTES, "nothing learned: one append");
        let mut p = Pair {
            leader: Some(Member::open(&one, 1, opened, true)?),
            learner: Member::open(&two, 2, crate::group::cache_bytes(0), false)?,
            up: true,
            spans: Spans::new(kept(&file)),
            file: file.clone(),
            waker: std::task::Waker::noop().clone(),
            start: std::time::Instant::now(),
            ticked: std::time::Instant::now(),
        };
        p.until("the leader leading", |p| p.leader.as_ref().is_some_and(|l| l.r.is_leader()))?;
        p.leader()?.r.change(Vec::new(), &crate::group::change(2, hyper_raft::proto::ConfChangeType::AddLearnerNode)).map_err(|e| format!("{e:?}"))?;
        let first = p.away(1, 16)?;
        assert!(first > 0, "with one append in memory, the catch-up of sixteen quarter appends read the log's file");
        let learned = kept(&file);
        assert!(learned > crate::group::APPEND_BYTES, "what the learner lacked is kept, past the one append the cache was: {learned}");
        // A deploy: the leader's process stops and opens again on its files; the learner goes on.
        p.leader = None;
        let cache = crate::group::cache_bytes(kept(&file));
        assert_eq!(cache, learned, "opened again, its cache holds what the learner lacked");
        p.leader = Some(Member::open(&one, 1, cache, true)?);
        p.spans = Spans::new(kept(&file));
        p.until("the leader leading again", |p| p.leader.as_ref().is_some_and(|l| l.r.is_leader()))?;
        let second = p.away(2, 12)?;
        eprintln!("B21, observe: reads of the leader's log file a catch-up took: {first} with one append, {second} with {cache} bytes");
        assert_eq!(second, 0, "the catch-up of twelve quarter appends read the log's file with {cache} bytes of cache");
        Ok(())
    }

    /// What's kept beside the log reads back; a file that doesn't read is nothing learned.
    #[test]
    fn g11_b21_what_a_run_learned_is_kept_for_the_next() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("catch-up");
        assert_eq!(kept(&file), 0, "none kept");
        keep(&file, 1_049_319)?;
        assert_eq!(kept(&file), 1_049_319);
        std::fs::write(&file, "not a number")?;
        assert_eq!(kept(&file), 0);
        Ok(())
    }
}
