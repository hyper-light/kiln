//! Amphora: the pages the log's recent frames hold, in frame order (Amphora's `docs/design/store.md`,
//! decision 18).
//!
//! A connection starting a read transaction after others committed forgets only the pages those
//! commits wrote (`Wal::begin_read_tx`, `Pager::begin_read_tx`) instead of its whole page cache,
//! which was SQLite's way and Turso's: a reader's cache then outlives a commit. The list vouches
//! only for the frames it saw appended, one after another: a frame appended past a gap, a rewind
//! below what it holds, a restart or a restore leaves it unable to say what changed before, and a
//! reader that last read before that clears its cache whole, as before.

use std::collections::VecDeque;

/// Entries kept at most (16 bytes each, 1 MiB): past it the oldest [`DROP`] go, and a reader whose
/// last read is older than what is left clears its cache whole.
const MOST: usize = 1 << 16;

/// Entries dropped at once when the list is full.
const DROP: usize = 1 << 15;

/// The pages frames `floor + 1 ..= next - 1` hold, contiguously: `(frame, page)` in frame order, one
/// entry for a frame of one page, one for each page of a packed record at its first slot.
#[derive(Debug)]
pub(crate) struct ChangeLog {
    entries: VecDeque<(u64, u64)>,
    /// Frames at or below it are not listed.
    floor: u64,
    /// The frame the next append is expected at.
    next: u64,
}

impl Default for ChangeLog {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            floor: 0,
            next: 1,
        }
    }
}

impl ChangeLog {
    /// Frames `first ..` (`slots` of them) were appended, holding `pages`: one for a frame, each
    /// page of a record for a packed one. Appending below the next expected frame rewrites what
    /// was there; appending past it leaves a gap, and only what follows is known.
    pub(crate) fn record(&mut self, first: u64, slots: u64, pages: impl Iterator<Item = u64>) {
        if first < self.next {
            self.drop_from(first);
        }
        if first > self.next {
            self.entries.clear();
            self.floor = first.saturating_sub(1);
        }
        if self.entries.len() >= MOST {
            self.drop_oldest();
        }
        for page in pages {
            self.entries.push_back((first, page));
        }
        self.next = first.saturating_add(slots.max(1));
    }

    /// Frames from `frame` up are gone (rolled back, or about to be written again).
    pub(crate) fn drop_from(&mut self, frame: u64) {
        while self.entries.back().is_some_and(|&(f, _)| f >= frame) {
            self.entries.pop_back();
        }
        self.floor = self.floor.min(frame.saturating_sub(1));
        self.next = self.next.min(frame);
    }

    /// The log started again from its first frame, or was replaced: nothing before is known.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// The distinct pages frames `after + 1 ..= upto` hold, or None when not all of those frames
    /// are listed or they hold more than `most` entries (a cache that holds fewer pages than that
    /// is cleared faster than it is forgotten page by page).
    pub(crate) fn pages(&self, after: u64, upto: u64, most: usize) -> Option<Vec<u64>> {
        if after < self.floor || upto >= self.next || after > upto {
            return None;
        }
        let start = self.entries.partition_point(|&(f, _)| f <= after);
        let mut pages = Vec::new();
        for &(frame, page) in self.entries.range(start..) {
            if frame > upto {
                break;
            }
            if pages.len() >= most {
                return None;
            }
            pages.push(page);
        }
        pages.sort_unstable();
        pages.dedup();
        Some(pages)
    }

    /// `(page, frame)` for each page each of frames `after + 1 ..= upto` holds, as listed; None
    /// when not all of those frames are listed. Copied out, for [`latest_by_page`] to sort once
    /// the list's lock is let go (Amphora's `docs/design/store.md`, decision 24).
    pub(crate) fn entries(&self, after: u64, upto: u64) -> Option<Vec<(u64, u64)>> {
        if after < self.floor || upto >= self.next || after > upto {
            return None;
        }
        let start = self.entries.partition_point(|&(f, _)| f <= after);
        Some(
            self.entries
                .range(start..)
                .take_while(|&&(frame, _)| frame <= upto)
                .map(|&(frame, page)| (page, frame))
                .collect(),
        )
    }

    /// Drops the oldest [`DROP`] entries, and with them every entry of the last frame they reach,
    /// so a frame is listed whole or not at all.
    fn drop_oldest(&mut self) {
        let mut last = None;
        for _ in 0..DROP {
            match self.entries.pop_front() {
                Some((frame, _)) => last = Some(frame),
                None => break,
            }
        }
        if let Some(last) = last {
            while self.entries.front().is_some_and(|&(f, _)| f == last) {
                self.entries.pop_front();
            }
            self.floor = self.floor.max(last);
        }
    }
}

/// Each page of `entries` (`(page, frame)`) once, with the latest frame it is listed at, by page:
/// what a checkpoint of their frames copies, found from the frames' own list rather than among
/// every page the log holds (decision 24).
pub(crate) fn latest_by_page(mut entries: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    // By page, its latest frame first: the one each page keeps.
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    entries.dedup_by_key(|&mut (page, _)| page);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest_frames(log: &ChangeLog, after: u64, upto: u64) -> Option<Vec<(u64, u64)>> {
        log.entries(after, upto).map(latest_by_page)
    }

    fn one(log: &mut ChangeLog, frame: u64, page: u64) {
        log.record(frame, 1, std::iter::once(page));
    }

    /// Do: append frames 1..=5 of pages 7, 8, 7, 9, 10. Expect: after frame 2 up to 4, pages 7 and
    /// 9; nothing changed from 5 to 5; and asking past the last listed frame is refused.
    #[test]
    fn lists_the_pages_of_a_range_of_frames() {
        let mut log = ChangeLog::default();
        for (frame, page) in [(1, 7), (2, 8), (3, 7), (4, 9), (5, 10)] {
            one(&mut log, frame, page);
        }
        assert_eq!(log.pages(2, 4, 100), Some(vec![7, 9]));
        assert_eq!(log.pages(0, 5, 100), Some(vec![7, 8, 9, 10]));
        assert_eq!(log.pages(5, 5, 100), Some(vec![]));
        assert_eq!(log.pages(4, 6, 100), None);
        assert_eq!(log.pages(0, 5, 3), None, "more entries than asked for");
    }

    /// Do: frames 1..=5 of pages 7, 8, 7, 9, 10, then a record of pages 7 and 11 on slots 6 and
    /// 7. Expect: each page of a range once, with the latest frame of the range that holds it, by
    /// page; a range past the last listed frame refused.
    #[test]
    fn lists_each_pages_latest_frame_in_a_range() {
        let mut log = ChangeLog::default();
        for (frame, page) in [(1, 7), (2, 8), (3, 7), (4, 9), (5, 10)] {
            one(&mut log, frame, page);
        }
        log.record(6, 2, [7, 11].into_iter());
        assert_eq!(
            latest_frames(&log, 0, 5),
            Some(vec![(7, 3), (8, 2), (9, 4), (10, 5)])
        );
        assert_eq!(
            latest_frames(&log, 3, 7),
            Some(vec![(7, 6), (9, 4), (10, 5), (11, 6)])
        );
        assert_eq!(latest_frames(&log, 5, 5), Some(vec![]));
        assert_eq!(latest_frames(&log, 0, 8), None);
    }

    /// Do: a packed record of three pages on four slots, then a frame. Expect: the record's pages
    /// at its first slot, the next frame expected after its last slot.
    #[test]
    fn a_record_lists_each_page_at_its_first_slot() {
        let mut log = ChangeLog::default();
        log.record(1, 4, [3, 4, 5].into_iter());
        one(&mut log, 5, 6);
        assert_eq!(log.pages(0, 4, 100), Some(vec![3, 4, 5]));
        assert_eq!(log.pages(1, 5, 100), Some(vec![6]));
    }

    /// Do: frames 1..=4, then frame 3 written again (a rewind), then a rollback to frame 2. Expect:
    /// the rewritten frame's new page replaces the old ones above it; after the rollback nothing
    /// past frame 2 is listed and frame 3 is expected next.
    #[test]
    fn a_rewind_and_a_rollback_forget_what_they_overwrite() {
        let mut log = ChangeLog::default();
        for (frame, page) in [(1, 1), (2, 2), (3, 3), (4, 4)] {
            one(&mut log, frame, page);
        }
        one(&mut log, 3, 30);
        assert_eq!(log.pages(0, 3, 100), Some(vec![1, 2, 30]));
        assert_eq!(log.pages(0, 4, 100), None, "frame 4 is gone");
        log.drop_from(3);
        assert_eq!(log.pages(0, 2, 100), Some(vec![1, 2]));
        one(&mut log, 3, 33);
        assert_eq!(log.pages(2, 3, 100), Some(vec![33]));
    }

    /// Do: frames appended from 10 with nothing listed before (a log recovered or loaded whole),
    /// then a reset. Expect: frames before 10 can't be vouched for; after the reset nothing can.
    #[test]
    fn a_gap_or_a_reset_refuses_what_came_before() {
        let mut log = ChangeLog::default();
        one(&mut log, 10, 1);
        one(&mut log, 11, 2);
        assert_eq!(log.pages(5, 11, 100), None);
        assert_eq!(log.pages(9, 11, 100), Some(vec![1, 2]));
        log.reset();
        assert_eq!(log.pages(9, 11, 100), None);
    }

    /// Do: append past the bound. Expect: the oldest frames dropped whole, readers from before
    /// them refused, the recent ones still listed.
    #[test]
    fn past_its_bound_it_drops_the_oldest_frames() {
        let mut log = ChangeLog::default();
        let frames = u64::try_from(MOST).unwrap() + 10;
        for frame in 1..=frames {
            one(&mut log, frame, frame % 1000);
        }
        assert!(log.entries.len() <= MOST);
        assert_eq!(log.pages(0, frames, usize::MAX), None);
        assert_eq!(
            log.pages(frames - 2, frames, 100),
            Some(vec![(frames - 1) % 1000, frames % 1000])
        );
    }
}
