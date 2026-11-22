//! When a replica's owner starts its database's log again, and how far the log may grow
//! (docs/design/replication.md, "A replica's database log bounded by what it measures"). A pure
//! core: the owner tells it each step's entries and time, each tick and each restart, and the
//! flushing thread gives it the filesystem's free bytes. Nothing in it is picked:
//!
//! - a restart holds the owner, and with it every entry the owner applies until the backlog it
//!   made is gone: until a step begins with no more input queued than it found (held). One is
//!   due only while the entries held by restarts, the entries the owner's own long steps already
//!   delayed past AC-T1.3.4's p99.9 (over), and the most one of the last thousand restarts held
//!   (next, for the restart about to come) stay strictly below the p99.9's tail of the entries
//!   applied in the current generation. A generation ends at the restart made once it alone
//!   covers its own spend at the quantile, so what it forgets never made a restart due;
//! - the log may hold what its own file and an equal share, among the replica logs on its
//!   filesystem, of what is free there less one snapshot's image, hold: each frame its own bytes
//!   and the page its copy may add to the database's file ([`log_room`]).

use amphora_api::Refusal;
use std::collections::VecDeque;
use std::time::Duration;

/// The quantile the registry holds a commit's latency to, as a fraction: AC-T1.3.4's p99.9, the
/// highest of docs/requirements.toml (AC-T1.4.3 holds the same quantile).
const QUANTILE: (u64, u64) = (999, 1_000);
/// AC-T1.3.4's bound on a commit's p99.9: what an owner's step applies is delayed past it once the
/// step takes longer, a proxy for that entry's commit, whose quorum and network the owner can't
/// see.
const COMMIT_TAIL: Duration = Duration::from_millis(15);
/// A frame of the database's log: a 4 KiB page (the page every replicated delta and image holds,
/// `replication::validate_pages`) and its 24-byte header (sqlite.org/fileformat.html, "WAL File
/// Format").
pub(super) const FRAME_BYTES: u64 = 4096 + 24;

/// What one generation counted, in entries applied: from the restart that began it to the
/// restart made once it covered its own spend at the quantile (replication.md, "Decision (lane
/// constants): a generation of restarts ends once it covers its spend").
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Window {
    applied: u64,
    /// Applied while a restart held the owner, or before the backlog it made was gone.
    held: u64,
    /// Applied, not held, by a step longer than [`COMMIT_TAIL`].
    over: u64,
}

/// The most one restart held among the last `of / (of - quantile)` ended holds (a thousand at the
/// p99.9): the worst of n holds estimates their 1 - 1/n quantile, so the next restart is guessed at
/// the quantile the rule holds the tail to. A monotonic queue of (restart, held): each entry holds
/// more than every later one, so the front is the most, and it holds at most that many entries.
#[derive(Debug, Default)]
struct Worst {
    /// Holds ended so far.
    ended: u64,
    holds: VecDeque<(u64, u64)>,
}

impl Worst {
    fn most(&self) -> u64 {
        self.holds.front().map_or(0, |&(_, held)| held)
    }

    fn ended(&mut self, held: u64) -> Result<(), Refusal> {
        let (quantile, of) = QUANTILE;
        let last = of
            .checked_sub(quantile)
            .and_then(|tail| of.checked_div(tail))
            .ok_or_else(|| overflowed("the holds kept"))?;
        self.ended = self
            .ended
            .checked_add(1)
            .ok_or_else(|| overflowed("restarts"))?;
        while self.holds.back().is_some_and(|&(_, most)| most <= held) {
            self.holds.pop_back();
        }
        self.holds.push_back((self.ended, held));
        while self
            .holds
            .front()
            .is_some_and(|&(restart, _)| restart.saturating_add(last) <= self.ended)
        {
            self.holds.pop_front();
        }
        Ok(())
    }
}

/// A restart's hold on what the owner applies.
#[derive(Debug, Clone, Copy)]
struct Holding {
    /// The entries it has held so far.
    held: u64,
    /// The input queued when it began, in the owner's admitted bytes: once a step begins with no
    /// more, the backlog the restart made is gone.
    found: u64,
    /// No step has begun since it did: the step it ran in is held whatever its queue was.
    fresh: bool,
}

/// An owner's count of what its restarts of the database's log held.
#[derive(Debug, Default)]
pub(super) struct Spacing {
    current: Window,
    worst: Worst,
    holding: Option<Holding>,
}

fn overflowed(what: &str) -> Refusal {
    Refusal::Unavailable(format!("the log restarts' count of {what} overflowed"))
}

impl Spacing {
    /// The owner started the log again with `queued` bytes of input admitted and not yet taken:
    /// what it applies from now until a step begins with no more queued is held. A restart within
    /// another's hold goes on counting into it, its backlog measured from the first. A restart made
    /// once the generation alone covered its own spend at the quantile begins the next, and the
    /// one that ended is forgotten: it paid for itself, so forgetting it never makes a restart due.
    /// One made before (past the log's room, disk before latency) counts into the same generation,
    /// so its spend is paid before the next is due.
    pub(super) fn restarted(&mut self, queued: u64) -> Result<(), Refusal> {
        if self.holding.is_none() && self.due()? {
            self.current = Window::default();
        }
        let holding = self.holding.get_or_insert(Holding {
            held: 0,
            found: queued,
            fresh: true,
        });
        holding.fresh = true;
        Ok(())
    }

    /// A step of the owner that began with `queued` bytes of input admitted and not yet taken,
    /// applied `entries` and took `took` of its clock. A step that begins with no more queued than
    /// a restart found ends its hold: the backlog it made is gone (an idle owner's step, none
    /// queued, among them), and that step's entries waited for no restart.
    pub(super) fn stepped(
        &mut self,
        entries: u64,
        took: Duration,
        queued: u64,
    ) -> Result<(), Refusal> {
        if let Some(holding) = self.holding
            && !holding.fresh
            && queued <= holding.found
        {
            self.holding = None;
            self.worst.ended(holding.held)?;
        }
        let window = &mut self.current;
        window.applied = window
            .applied
            .checked_add(entries)
            .ok_or_else(|| overflowed("entries applied"))?;
        if let Some(holding) = self.holding.as_mut() {
            holding.fresh = false;
            holding.held = holding
                .held
                .checked_add(entries)
                .ok_or_else(|| overflowed("entries held"))?;
            window.held = window
                .held
                .checked_add(entries)
                .ok_or_else(|| overflowed("entries held"))?;
        } else if took > COMMIT_TAIL {
            window.over = window
                .over
                .checked_add(entries)
                .ok_or_else(|| overflowed("entries over the tail"))?;
        }
        Ok(())
    }

    /// Whether a restart's hold is under way.
    #[cfg(test)]
    pub(super) fn holding(&self) -> bool {
        self.holding.is_some()
    }

    /// The most one restart held, the next one's guess: the worst of the last holds that ended,
    /// or the hold under way if it already held more.
    fn next(&self) -> u64 {
        self.worst
            .most()
            .max(self.holding.map_or(0, |holding| holding.held))
    }

    /// Whether a restart is due by its share: the generation's held and over and the next
    /// restart's worst guess, against the entries the generation applied, strictly below the
    /// quantile's tail: the generation alone covers its own spend.
    pub(super) fn due(&self) -> Result<bool, Refusal> {
        let (quantile, of) = QUANTILE;
        let tail = of
            .checked_sub(quantile)
            .ok_or_else(|| overflowed("the tail"))?;
        let current = &self.current;
        let sum = |a: u64, b: u64, what| a.checked_add(b).ok_or_else(|| overflowed(what));
        let applied = current.applied;
        let spent = sum(current.held, current.over, "entries spent")?;
        let spent = sum(spent, self.next(), "entries spent")?;
        let spent = spent
            .checked_mul(of)
            .ok_or_else(|| overflowed("entries spent"))?;
        let room = applied
            .checked_mul(tail)
            .ok_or_else(|| overflowed("entries applied"))?;
        Ok(spent < room)
    }
}

/// The frames a replica's database log may hold: the bytes its file already takes (`kept` frames,
/// reused, store.md decision 13) and an equal share among the `logs` replica logs on its
/// filesystem of the `available` bytes there, less one image's `image` bytes (the most an image
/// may take now, its memory budget's room for one: SB.1; so an install has room), each frame taking its own
/// bytes and the page its copy may add to the database's file. Counted in bytes, a frame the log
/// grows by moves bytes from `available` to its file, so the room holds still for one log and
/// grows by less than half a frame a frame for more, the others' growth seen at the next pass. The
/// process's disk budget, once it exists, passes its room for replica logs as `available`.
pub(super) fn log_room(available: u64, image: u64, logs: u64, kept: u64) -> u64 {
    // Saturating, each: nothing free past the image, and a length past u64's bytes the most it can.
    let free = available.saturating_sub(image);
    let share = free.checked_div(logs.max(1)).unwrap_or(0);
    let held = kept.saturating_mul(FRAME_BYTES).saturating_add(share);
    FRAME_BYTES
        .checked_add(crate::db::PAGE_BYTES)
        .and_then(|each| held.checked_div(each))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: Duration = Duration::from_millis(1);

    /// AC-T1.8.D1, T-T1.8.D3: do a step that applied one entry on a new owner; expect a restart
    /// due: the first restart is free, nothing held yet.
    #[test]
    fn the_first_restart_is_free() {
        let mut spacing = Spacing::default();
        assert!(!spacing.due().unwrap(), "due with nothing applied");
        spacing.stepped(1, QUICK, 0).unwrap();
        assert!(spacing.due().unwrap());
    }

    /// AC-T1.8.D1, T-T1.8.D3: do a restart that held 5 entries until an idle step; expect the next
    /// due only once a thousand times its held and its worst guess (5 + 5) are below the entries
    /// applied: at 10,001 and not at 10,000.
    #[test]
    fn a_restart_spends_what_it_held_and_spaces_the_next() {
        let mut spacing = Spacing::default();
        spacing.restarted(0).unwrap();
        spacing.stepped(5, QUICK, 1).unwrap();
        spacing.stepped(0, QUICK, 0).unwrap();
        spacing.stepped(9_995, QUICK, 0).unwrap();
        assert!(!spacing.due().unwrap(), "due at 10,000 applied");
        spacing.stepped(1, QUICK, 0).unwrap();
        assert!(spacing.due().unwrap(), "not due at 10,001 applied");
    }

    /// AC-T1.8.D1, T-T1.8.D3: do a step longer than AC-T1.3.4's 15 ms applying 3 entries; expect
    /// them spent as the base tail (a restart due at 3,001 applied, not 3,000), and a restart's
    /// hold not counted twice: entries a long step applies while held count as held only, in the
    /// generation the restart began (the one before covered its spend, and is forgotten).
    #[test]
    fn long_steps_spend_the_tail_before_any_restart() {
        let mut spacing = Spacing::default();
        spacing.stepped(3, Duration::from_millis(16), 0).unwrap();
        spacing.stepped(2_997, QUICK, 0).unwrap();
        assert!(!spacing.due().unwrap(), "due at 3,000 applied with 3 over");
        spacing.stepped(1, QUICK, 0).unwrap();
        assert!(spacing.due().unwrap());
        assert_eq!(spacing.current.over, 3);
        spacing.restarted(0).unwrap();
        spacing.stepped(4, Duration::from_millis(16), 0).unwrap();
        assert_eq!((spacing.current.over, spacing.current.held), (0, 4));
    }

    /// AC-T1.8.D1, T-T1.8.D3 (amphora-owner-t1-log-bound-4a274ec-20261111, point 1): an owner under
    /// steady load, never idle after a restart, still ends the restart's hold once its backlog is
    /// gone. Do: a restart that found 3 bytes queued and held 5 entries in its step, a step that
    /// began with 9 queued applying 2, then a million steps of 1 entry each, every one beginning
    /// with 3 queued. Expect: 7 held, the step with 9 queued among them, and a restart due again
    /// by its share.
    #[test]
    fn a_restart_due_again_by_its_share_on_an_owner_never_idle() {
        let mut spacing = Spacing::default();
        spacing.restarted(3).unwrap();
        spacing.stepped(5, QUICK, 3).unwrap();
        spacing.stepped(2, QUICK, 9).unwrap();
        for _ in 0..1_000_000 {
            spacing.stepped(1, QUICK, 3).unwrap();
        }
        assert_eq!((spacing.current.held, spacing.worst.most()), (7, 7));
        assert!(
            spacing.due().unwrap(),
            "held for good: {:?}",
            spacing.current
        );
    }

    /// AC-T1.8.D1, T-T1.8.D3 (GN.2): do a restart that held 50 entries, 99,950 more applied
    /// (not yet covering its 50 and the next's guess of 50 at the p99.9), one more, then a
    /// restart; expect it due only at 100,001 applied, and the generation it ended forgotten: the
    /// next due once the new generation alone covers its own hold of 1 and the guess, 50 (at
    /// 51,001 applied, not 51,000), however many the one before applied.
    #[test]
    fn a_generation_ends_once_it_covers_its_spend_and_is_forgotten() {
        let mut spacing = Spacing::default();
        spacing.restarted(0).unwrap();
        spacing.stepped(50, QUICK, 1).unwrap();
        spacing.stepped(0, QUICK, 0).unwrap();
        spacing.stepped(99_950, QUICK, 0).unwrap();
        assert!(!spacing.due().unwrap(), "due at 100,000 applied");
        spacing.stepped(1, QUICK, 0).unwrap();
        assert!(spacing.due().unwrap(), "not due at 100,001 applied");
        spacing.restarted(0).unwrap();
        assert_eq!(
            spacing.current,
            Window::default(),
            "the generation not forgotten"
        );
        spacing.stepped(1, QUICK, 1).unwrap();
        spacing.stepped(0, QUICK, 0).unwrap();
        spacing.stepped(50_999, QUICK, 0).unwrap();
        assert!(!spacing.due().unwrap(), "due at 51,000 applied");
        spacing.stepped(1, QUICK, 0).unwrap();
        assert!(spacing.due().unwrap(), "not due at 51,001 applied");
    }

    /// AC-T1.8.D1, T-T1.8.D3 (GN.2): do 1,000 restarts each holding 1 after one that held 9;
    /// expect the guess the 9 through the 999th of them, and 1 once the 9 is past the last
    /// thousand holds; a restart made before its generation covered its spend (past the log's
    /// room) counted into the same generation.
    #[test]
    fn the_guess_is_the_worst_of_the_last_thousand_holds() {
        let mut spacing = Spacing::default();
        let hold = |spacing: &mut Spacing, held| {
            spacing.restarted(0).unwrap();
            spacing.stepped(held, QUICK, 1).unwrap();
            spacing.stepped(0, QUICK, 0).unwrap();
        };
        hold(&mut spacing, 9);
        for _ in 0..999 {
            hold(&mut spacing, 1);
        }
        assert_eq!(spacing.worst.most(), 9);
        hold(&mut spacing, 1);
        assert_eq!(spacing.worst.most(), 1);
        assert!(spacing.worst.holds.len() <= 1_000);
        assert_eq!(
            spacing.current.held, 1_009,
            "a restart not due began a generation"
        );
    }

    /// AC-T1.8.D1, T-T1.8.D3 (the share): do an owner applying entries in steps of 1 to 7, a step in
    /// 20,011 a long one (a base tail well within the p99.9's), 240,000 steps, each restart made as
    /// soon as due and holding 1 to 9 entries; expect, after the hold of every restart that held
    /// no more than the worst before it, the held and the base tail strictly below a thousandth of
    /// the entries applied over the window (the generation the restart ended and the one it
    /// began); and such restarts made.
    #[test]
    fn restarts_made_when_due_keep_their_share_below_the_tail() {
        let mut spacing = Spacing::default();
        let (mut restarts, mut checked, mut worst, mut seed) = (0u64, 0u64, 0u64, 7u64);
        let mut next = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            seed >> 33
        };
        for _ in 0..240_000 {
            let took = if next() % 20_011 == 0 {
                Duration::from_millis(20)
            } else {
                QUICK
            };
            spacing.stepped(1 + next() % 7, took, 0).unwrap();
            if spacing.due().unwrap() {
                let ended = spacing.current;
                spacing.restarted(0).unwrap();
                let held = 1 + next() % 9;
                spacing.stepped(held, QUICK, 1).unwrap();
                spacing.stepped(0, QUICK, 0).unwrap();
                restarts += 1;
                if held > worst {
                    worst = held;
                    continue;
                }
                checked += 1;
                let c = &spacing.current;
                let spent = (c.held + ended.held + c.over + ended.over) * 1_000;
                assert!(
                    spent < c.applied + ended.applied,
                    "restart {restarts}: {spent} against {} applied",
                    c.applied + ended.applied
                );
            }
        }
        assert!(checked >= 4, "{checked} of {restarts} restarts checked");
    }

    /// AC-T1.8.D1, T-T1.8.D3: do the disk's share at its edges; expect the log's own file and an
    /// equal share of what is free past one image, each frame its own bytes and a page: a file of
    /// 40 frames (164,800 bytes) alone holds 20; with 100 frames' worth free, 120; with a second
    /// log on the filesystem, 70; nothing free past the image the same as none. A frame grown
    /// from free bytes into the file leaves one log's room where it was.
    #[test]
    fn a_log_takes_its_file_and_an_equal_share_of_what_is_free_past_one_image() {
        let image = 256u64 << 20;
        let each = FRAME_BYTES + crate::db::PAGE_BYTES;
        assert_eq!(log_room(image, image, 1, 40), 20);
        assert_eq!(log_room(image / 2, image, 1, 40), 20);
        assert_eq!(log_room(image + 100 * each, image, 1, 40), 120);
        assert_eq!(log_room(image + 100 * each, image, 2, 40), 70);
        assert_eq!(
            log_room(image + 100 * each - FRAME_BYTES, image, 1, 41),
            log_room(image + 100 * each, image, 1, 40)
        );
        assert_eq!(log_room(u64::MAX, image, 0, u64::MAX), u64::MAX / each);
    }
}
