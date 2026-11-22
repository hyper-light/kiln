//! The archive's pure core (CLAUDE.md §9: no clock, disk or network but what is passed in): the
//! keys its objects go under, which objects restore a second ([`locate`], [`cut_in`], [`chain`]),
//! which objects a retention lets go ([`expired`]), and when a new base is due ([`base_due`]).
//!
//! The archive under a prefix `p`:
//! - `p/log/{generation}/{first}/{last}/{first_ns}/{last_ns}/{through}/{unknown}/{link}`: a segment, the
//!   frames `first..=last` of log generation `generation` (the archive's count of the log's generations),
//!   holding the marks of the commits it ends ([`super::Mark`]) from `first_ns` to `last_ns`
//!   (`part` for a piece of a segment split by the store's object size, which ends at no mark);
//!   every commit stamped at or before `through` is in it or before it; `unknown`, the first
//!   mark's `unknown_from` (`-`: none); `link`, for a generation's first segment, whether the
//!   generation before it was shipped whole (`w` and its last frame) or not (`g`).
//! - `p/base/{end_gen}/{end_frame}/{start_gen}/{id}`: a base's manifest, written after its pages:
//!   a copy of the file begun in generation `start_gen` and ended at `end_frame` of `end_gen`.
//! - `p/pages/{id}/{chunk}`: a base's pages.
//! - `p/through/{generation}/{last}/{through}`: a through mark: the log, shipped through frame
//!   `last` of `generation`, held every commit stamped at or before `through`, a time later than
//!   the segment ending there says; written when a round finds nothing new to ship
//!   ([`super::Shipper::ship`]), so a database that stops committing (or closes) is whole through
//!   the second it stopped in (T-T1.12.D2).
//!
//! Numbers are zero-padded to 20 digits so that the store's key order is their order.

use super::{SECOND_NS, second_of};

/// What a second can't be restored for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unrestorable {
    #[error("second {0} is before the archive's first commit")]
    BeforeArchive(u64),
    #[error("second {asked} isn't archived yet: the archive is whole through {through} ns")]
    NotYetArchived { asked: u64, through: u64 },
    #[error(
        "which commits were made by second {asked} isn't known: commits after {from} ns weren't stamped one by one"
    )]
    Inexact { asked: u64, from: u64 },
    #[error("generation {0} of the log isn't archived whole")]
    Gap(u64),
    #[error("no base precedes second {0} on an unbroken log")]
    NoBase(u64),
    #[error("archive object {key}: {why}")]
    Malformed { key: String, why: String },
}

/// Whether the generation before a segment's was shipped whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Link {
    /// Whole, through this last frame.
    Whole(u64),
    Gap,
}

/// A segment as its key names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentKey {
    pub generation: u64,
    pub first: u64,
    pub last: u64,
    /// The stamps of its first and last marks; None for a part, which ends at no mark.
    pub marks: Option<(u64, u64)>,
    pub through: u64,
    pub unknown: Option<u64>,
    pub link: Option<Link>,
}

/// A base as its manifest's key names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseKey {
    pub end: (u64, u64),
    pub start_gen: u64,
    pub id: u64,
}

fn pad(n: u64) -> String {
    format!("{n:020}")
}

fn opt(n: Option<u64>) -> String {
    n.map_or_else(|| "-".into(), pad)
}

impl SegmentKey {
    #[must_use]
    pub fn key(&self, prefix: &str) -> String {
        let (first_ns, last_ns) = match self.marks {
            Some((a, b)) => (pad(a), pad(b)),
            None => ("part".into(), "part".into()),
        };
        let link = match self.link {
            Some(Link::Whole(last)) => format!("w{}", pad(last)),
            Some(Link::Gap) => "g".into(),
            None => "-".into(),
        };
        format!(
            "{prefix}/log/{}/{}/{}/{first_ns}/{last_ns}/{}/{}/{link}",
            pad(self.generation),
            pad(self.first),
            pad(self.last),
            pad(self.through),
            opt(self.unknown),
        )
    }

    pub fn parse(prefix: &str, key: &str) -> Result<Self, Unrestorable> {
        let bad = |why: &str| Unrestorable::Malformed {
            key: key.into(),
            why: why.into(),
        };
        let rest = key
            .strip_prefix(prefix)
            .and_then(|k| k.strip_prefix("/log/"))
            .ok_or_else(|| bad("not a segment's key"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        let [
            generation,
            first,
            last,
            first_ns,
            last_ns,
            through,
            unknown,
            link,
        ] = parts.as_slice()
        else {
            return Err(bad("not eight fields"));
        };
        let num = |s: &str| s.parse::<u64>().map_err(|_| bad("a field isn't a number"));
        let marks = match (*first_ns, *last_ns) {
            ("part", "part") => None,
            (a, b) => Some((num(a)?, num(b)?)),
        };
        Ok(Self {
            generation: num(generation)?,
            first: num(first)?,
            last: num(last)?,
            marks,
            through: num(through)?,
            unknown: match *unknown {
                "-" => None,
                n => Some(num(n)?),
            },
            link: match *link {
                "g" => Some(Link::Gap),
                "-" => None,
                w => match w.strip_prefix('w') {
                    Some(last) => Some(Link::Whole(num(last)?)),
                    None => return Err(bad("an unknown link")),
                },
            },
        })
    }
}

impl BaseKey {
    #[must_use]
    pub fn key(&self, prefix: &str) -> String {
        format!(
            "{prefix}/base/{}/{}/{}/{}",
            pad(self.end.0),
            pad(self.end.1),
            pad(self.start_gen),
            pad(self.id)
        )
    }

    pub fn parse(prefix: &str, key: &str) -> Result<Self, Unrestorable> {
        let bad = |why: &str| Unrestorable::Malformed {
            key: key.into(),
            why: why.into(),
        };
        let rest = key
            .strip_prefix(prefix)
            .and_then(|k| k.strip_prefix("/base/"))
            .ok_or_else(|| bad("not a base's key"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        let [end_gen, end_frame, start_gen, id] = parts.as_slice() else {
            return Err(bad("not four fields"));
        };
        let num = |s: &str| s.parse::<u64>().map_err(|_| bad("a field isn't a number"));
        Ok(Self {
            end: (num(end_gen)?, num(end_frame)?),
            start_gen: num(start_gen)?,
            id: num(id)?,
        })
    }
}

/// A through mark as its key names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThroughKey {
    pub generation: u64,
    pub last: u64,
    pub through: u64,
}

impl ThroughKey {
    #[must_use]
    pub fn key(&self, prefix: &str) -> String {
        format!(
            "{prefix}/through/{}/{}/{}",
            pad(self.generation),
            pad(self.last),
            pad(self.through)
        )
    }

    pub fn parse(prefix: &str, key: &str) -> Result<Self, Unrestorable> {
        let bad = |why: &str| Unrestorable::Malformed {
            key: key.into(),
            why: why.into(),
        };
        let rest = key
            .strip_prefix(prefix)
            .and_then(|k| k.strip_prefix("/through/"))
            .ok_or_else(|| bad("not a through mark's key"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        let [generation, last, through] = parts.as_slice() else {
            return Err(bad("not three fields"));
        };
        let num = |s: &str| s.parse::<u64>().map_err(|_| bad("a field isn't a number"));
        Ok(Self {
            generation: num(generation)?,
            last: num(last)?,
            through: num(through)?,
        })
    }
}

/// Raises the through of each segment a through mark follows (the one ending at the mark's frame
/// of its generation) to the mark's. `segments` are in key order, so in order of where they end.
pub fn raise_through(segments: &mut [SegmentKey], marks: &[ThroughKey]) {
    for mark in marks {
        if let Ok(at) =
            segments.binary_search_by_key(&(mark.generation, mark.last), |s| (s.generation, s.last))
            && let Some(segment) = segments.get_mut(at)
            && segment.marks.is_some()
        {
            segment.through = segment.through.max(mark.through);
        }
    }
}

/// The key of chunk `chunk` of base `id`'s pages.
#[must_use]
pub fn chunk_key(prefix: &str, id: u64, chunk: u64) -> String {
    format!("{prefix}/pages/{}/{}", pad(id), pad(chunk))
}

/// The base id a chunk's key names.
pub fn chunk_id(prefix: &str, key: &str) -> Result<u64, Unrestorable> {
    key.strip_prefix(prefix)
        .and_then(|k| k.strip_prefix("/pages/"))
        .and_then(|k| k.split('/').next())
        .and_then(|id| id.parse::<u64>().ok())
        .ok_or_else(|| Unrestorable::Malformed {
            key: key.into(),
            why: "not a chunk's key".into(),
        })
}

fn ns_of(second: u64) -> Result<u64, Unrestorable> {
    second
        .checked_mul(SECOND_NS)
        .ok_or(Unrestorable::BeforeArchive(second))
}

/// Which segment holds the last commit of `second` (an index into `segments`, in key order): the
/// last that ends at marks and whose first mark is at or before it. Refused where the archive
/// can't say which commits the second holds: before its first mark, past what it holds whole, or
/// where the next commit's mark says commits before it weren't stamped one by one.
pub fn locate(segments: &[SegmentKey], second: u64) -> Result<usize, Unrestorable> {
    let t = ns_of(second)?;
    let found = segments
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.marks.is_some_and(|(first, _)| first <= t));
    let Some((at, segment)) = found else {
        return Err(Unrestorable::BeforeArchive(second));
    };
    let last = segment.marks.map_or(0, |(_, last)| last);
    if t >= last {
        // The cut is this segment's last mark: the next mark is the next segment's first.
        let next = segments
            .iter()
            .skip(at.saturating_add(1))
            .find(|s| s.marks.is_some());
        // A segment knows the archive whole only through when it was shipped; the next one's first
        // mark says no commit came between then and the start of that mark's second, as the
        // shipper says of a mark still pending ([`super::Shipper::ship`]); where that mark's
        // commits weren't stamped one by one, the check below refuses. A second in that quiet
        // stretch restores at this segment's last mark (T-T1.12.D1, T-T1.12.D2).
        let through = next
            .and_then(|n| n.marks)
            .map_or(segment.through, |(first, _)| {
                segment
                    .through
                    .max(second_of(first).saturating_sub(1).saturating_mul(SECOND_NS))
            });
        if through < t {
            return Err(Unrestorable::NotYetArchived {
                asked: second,
                through,
            });
        }
        if let Some(from) = next.and_then(|s| s.unknown)
            && from < t
        {
            return Err(Unrestorable::Inexact {
                asked: second,
                from,
            });
        }
    }
    Ok(at)
}

/// The frame a restore to `second` ends at, from the marks of the segment [`locate`] found: its
/// last mark at or before the second. Refused where the mark after it says the commits before it
/// weren't stamped one by one.
pub fn cut_in(marks: &[super::Mark], second: u64) -> Result<u64, Unrestorable> {
    let t = ns_of(second)?;
    let at = marks.iter().rposition(|m| m.ns <= t);
    let Some(at) = at else {
        return Err(Unrestorable::BeforeArchive(second));
    };
    if let Some(next) = marks.get(at.saturating_add(1))
        && let Some(from) = next.unknown_from
        && from < t
    {
        return Err(Unrestorable::Inexact {
            asked: second,
            from,
        });
    }
    marks
        .get(at)
        .map(|m| m.frame)
        .ok_or(Unrestorable::BeforeArchive(second))
}

/// What a restore reads: a base, then the segments from its first generation's first frame through
/// the cut, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub base: BaseKey,
    /// Indexes into the segments, in order.
    pub segments: Vec<usize>,
    /// Where the restore ends: the generation and frame of the last commit it keeps.
    pub cut: (u64, u64),
}

/// The plan that restores the cut at `frame` of segment `at`'s generation: the latest base that
/// ended at or before it on a log unbroken from the base's first generation to the cut.
pub fn chain(
    segments: &[SegmentKey],
    bases: &[BaseKey],
    at: usize,
    frame: u64,
    second: u64,
) -> Result<Plan, Unrestorable> {
    let segment = segments.get(at).ok_or(Unrestorable::NoBase(second))?;
    let cut = (segment.generation, frame);
    // Walk back from the cut: each generation's segments must run from frame 1 without a hole,
    // and a generation reached from the one after it only where that one's link says whole.
    let mut generation = segment.generation;
    let mut expect_last: Option<u64> = None;
    let mut earliest = generation;
    let mut index = at;
    let mut taken = Vec::new();
    while let Some(s) = segments.get(index) {
        if s.generation != generation {
            // `s` ends the generation before, which `generation`'s link said was whole through its last
            // frame: reachable only then.
            if s.generation.checked_add(1) != Some(generation) {
                break;
            }
            generation = s.generation;
        }
        if let Some(expected) = expect_last
            && s.last != expected
        {
            break;
        }
        taken.push(index);
        if s.first == 1 {
            earliest = generation;
            let Some(Link::Whole(last)) = s.link else {
                break;
            };
            if index == 0 {
                break;
            }
            expect_last = Some(last);
            // Only the generation before, from its last segment back.
            match segments.get(index.saturating_sub(1)) {
                Some(prev) if prev.generation.checked_add(1) == Some(generation) => {}
                _ => break,
            }
        } else {
            expect_last = Some(s.first.saturating_sub(1));
        }
        let Some(prev) = index.checked_sub(1) else {
            break;
        };
        index = prev;
    }
    taken.reverse();
    // The cut's generation must itself run from frame 1.
    if !taken
        .iter()
        .filter_map(|i| segments.get(*i))
        .any(|s| s.generation == earliest && s.first == 1)
    {
        return Err(Unrestorable::Gap(earliest));
    }
    let base = bases
        .iter()
        .filter(|b| b.end <= cut && b.start_gen >= earliest && b.start_gen <= cut.0)
        .max_by_key(|b| (b.end, b.id))
        .cloned()
        .ok_or(Unrestorable::NoBase(second))?;
    let segments = taken
        .into_iter()
        .filter(|i| {
            segments
                .get(*i)
                .is_some_and(|s| s.generation >= base.start_gen)
        })
        .collect();
    Ok(Plan {
        base,
        segments,
        cut,
    })
}

/// The objects a retention of `retention_ns` lets go at `now_ns`: what no second since then needs.
/// The oldest second kept is restored from the latest base before it on an unbroken log; that base
/// and everything after it stay, and so does every base begun after the newest kept one (a copy
/// under way). Keys of segments, manifests and chunks.
#[must_use]
pub fn expired(
    prefix: &str,
    segments: &[SegmentKey],
    bases: &[BaseKey],
    chunks: &[(u64, String)],
    now_ns: u64,
    retention_ns: u64,
) -> Vec<String> {
    let oldest = now_ns.saturating_sub(retention_ns).div_ceil(SECOND_NS);
    let Ok(at) = locate(segments, oldest) else {
        return Vec::new();
    };
    // The base the oldest second would use, at the latest cut that segment allows.
    let frame = segments.get(at).map_or(0, |s| s.first);
    let Ok(plan) = chain(segments, bases, at, frame, oldest) else {
        return Vec::new();
    };
    let keep_from = plan.base.start_gen;
    let mut gone: Vec<String> = segments
        .iter()
        .filter(|s| s.generation < keep_from)
        .map(|s| s.key(prefix))
        .collect();
    let old: Vec<&BaseKey> = bases
        .iter()
        .filter(|b| (b.end, b.id) < (plan.base.end, plan.base.id))
        .collect();
    gone.extend(old.iter().map(|b| b.key(prefix)));
    let kept: Vec<u64> = bases
        .iter()
        .filter(|b| (b.end, b.id) >= (plan.base.end, plan.base.id))
        .map(|b| b.id)
        .collect();
    let newest = kept.iter().copied().max().unwrap_or(0);
    gone.extend(
        chunks
            .iter()
            .filter(|(id, _)| *id < newest && !kept.contains(id))
            .map(|(_, key)| key.clone()),
    );
    gone
}

/// The bytes a restore moves each second, as measured: fetching from the store and replaying the
/// log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub fetch: f64,
    pub replay: f64,
}

/// Whether a new base is due, with `log` bytes shipped since the latest base of `base` bytes, a new
/// one copying `copy` bytes (an incremental base: the chunks whose pages the log changed since the
/// latest began; none when it would copy the whole file):
/// - there is none, or the log broke since (`broken`);
/// - the log since it outweighs what a new base copies: the new base then costs less to make than
///   the log it spares every later restore;
/// - with rates measured, a restore of the newest second would take longer than `objective_s`
///   (AC-T1.12.3: 100 GB in at most 30 minutes) at them.
#[must_use]
pub fn base_due(
    base: Option<u64>,
    broken: bool,
    log: u64,
    copy: Option<u64>,
    rates: Option<Rates>,
    objective_s: f64,
) -> bool {
    let Some(base) = base else { return true };
    if broken || log >= copy.unwrap_or(base) {
        return true;
    }
    let Some(rates) = rates else { return false };
    if rates.fetch <= 0.0 || rates.replay <= 0.0 {
        return false;
    }
    let bytes = base.saturating_add(log);
    #[allow(clippy::cast_precision_loss)] // byte counts far below 2^53 in any archive
    let took = bytes as f64 / rates.fetch + log as f64 / rates.replay;
    took > objective_s
}

/// Whether a due base begins now (T-T1.12.I4): a base's own generation counts as changed for the
/// next base, frames before it began included, so one begun just after the log started again
/// leaves the next the least to copy. A due base waits for that restart, unless no base restores
/// the log yet (`needed`: none, or the log broke since the latest), the log has just started again
/// (`restarted`), or its generation already holds the frames a restart keeps (`frames`, `kept`;
/// decision 31), past which the log starts again whether shipped or not: the wait is at most one
/// generation.
#[must_use]
pub fn base_begins_now(needed: bool, restarted: bool, frames: u64, kept: u64) -> bool {
    needed || restarted || frames >= kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::Mark;

    fn seg(
        generation: u64,
        first: u64,
        last: u64,
        marks: Option<(u64, u64)>,
        through: u64,
    ) -> SegmentKey {
        SegmentKey {
            generation,
            first,
            last,
            marks,
            through,
            unknown: None,
            link: if first != 1 {
                None
            } else if generation == 1 {
                Some(Link::Gap)
            } else {
                Some(Link::Whole(20))
            },
        }
    }

    const S: u64 = SECOND_NS;

    /// T-T1.12.P4: keys round-trip through their text, in the store's order.
    #[test]
    fn t_t1_12_p4_keys_round_trip() {
        let s = SegmentKey {
            unknown: Some(7),
            ..seg(3, 1, 40, Some((5, 9)), 11)
        };
        assert_eq!(SegmentKey::parse("p", &s.key("p")).unwrap(), s);
        let part = seg(3, 41, 50, None, 11);
        assert_eq!(SegmentKey::parse("p", &part.key("p")).unwrap(), part);
        let b = BaseKey {
            end: (3, 17),
            start_gen: 2,
            id: 99,
        };
        assert_eq!(BaseKey::parse("p", &b.key("p")).unwrap(), b);
        assert!(seg(3, 9, 9, None, 0).key("p") < seg(3, 10, 10, None, 0).key("p"));
        assert!(seg(3, 99, 99, None, 0).key("p") < seg(4, 1, 1, None, 0).key("p"));
        assert!(SegmentKey::parse("p", "p/log/1/2").is_err());
        let t = ThroughKey {
            generation: 3,
            last: 40,
            through: 12,
        };
        assert_eq!(ThroughKey::parse("p", &t.key("p")).unwrap(), t);
        assert!(ThroughKey::parse("p", "p/through/1/2").is_err());
    }

    /// T-T1.12.P12: a through mark makes the seconds after the last segment's whole through its
    /// time; one that doesn't end at a segment says nothing.
    #[test]
    fn t_t1_12_p12_through_marks() {
        let mut segments = vec![
            seg(1, 1, 10, Some((2 * S, 3 * S)), 3 * S + 5),
            seg(1, 11, 20, Some((4 * S, 6 * S)), 6 * S + 5),
        ];
        assert!(matches!(
            locate(&segments, 7),
            Err(Unrestorable::NotYetArchived { .. })
        ));
        let mark = |last, through| ThroughKey {
            generation: 1,
            last,
            through,
        };
        raise_through(&mut segments, &[mark(19, 9 * S), mark(20, 7 * S + 1)]);
        assert_eq!(segments[0].through, 3 * S + 5);
        assert_eq!(segments[1].through, 7 * S + 1);
        assert_eq!(locate(&segments, 7), Ok(1));
        assert!(matches!(
            locate(&segments, 8),
            Err(Unrestorable::NotYetArchived { .. })
        ));
    }

    /// T-T1.12.P5: a second is found in the segment whose marks reach it, or in the quiet stretch
    /// before the next segment's first mark; before the first mark, past what is whole, and in an
    /// unstamped stretch it is refused.
    #[test]
    fn t_t1_12_p5_locate() {
        let segments = vec![
            seg(1, 1, 10, Some((2 * S, 3 * S)), 3 * S + 5),
            seg(1, 11, 20, Some((4 * S, 6 * S)), 6 * S + 5),
        ];
        assert_eq!(locate(&segments, 1), Err(Unrestorable::BeforeArchive(1)));
        assert_eq!(locate(&segments, 2), Ok(0));
        assert_eq!(locate(&segments, 3), Ok(0));
        assert_eq!(locate(&segments, 5), Ok(1));
        assert_eq!(locate(&segments, 6), Ok(1));
        assert!(matches!(
            locate(&segments, 7),
            Err(Unrestorable::NotYetArchived { .. })
        ));
        // The next segment's first commits weren't stamped one by one after 2.5 s: second 3, past
        // segment 0's last mark, can't say which of them it holds.
        let mut unknown = segments.clone();
        unknown[1].unknown = Some(2 * S + S / 2);
        assert!(matches!(
            locate(&unknown, 3),
            Err(Unrestorable::Inexact { asked: 3, .. })
        ));
        // Within segment 1's marks the cut is the marks' to say (cut_in).
        assert_eq!(locate(&unknown, 5), Ok(1));
        // Segment 0 was shipped at 3 s; the next commit came at 7.2 s: seconds 4 to 7 held no
        // commit, so they restore at segment 0's last mark; second 8 is segment 1's.
        let quiet = vec![
            seg(1, 1, 10, Some((2 * S, 3 * S)), 3 * S + 5),
            seg(1, 11, 20, Some((7 * S + 2, 8 * S)), 8 * S + 5),
        ];
        assert_eq!(locate(&quiet, 4), Ok(0));
        assert_eq!(locate(&quiet, 7), Ok(0));
        assert_eq!(locate(&quiet, 8), Ok(1));
        assert!(matches!(
            locate(&quiet, 9),
            Err(Unrestorable::NotYetArchived { .. })
        ));
        // Unless segment 1's first commits weren't stamped one by one after 4.5 s.
        let mut quiet_unknown = quiet.clone();
        quiet_unknown[1].unknown = Some(4 * S + S / 2);
        assert_eq!(locate(&quiet_unknown, 4), Ok(0));
        assert!(matches!(
            locate(&quiet_unknown, 5),
            Err(Unrestorable::Inexact { asked: 5, .. })
        ));
    }

    /// T-T1.12.P6: the cut is the last mark at or before the second; an unstamped stretch after
    /// it is refused.
    #[test]
    fn t_t1_12_p6_cut_in() {
        let m = |frame, ns, unknown_from| Mark {
            seq: 0,
            frame,
            ns,
            unknown_from,
        };
        let marks = vec![
            m(3, 2 * S, None),
            m(7, 4 * S, None),
            m(9, 6 * S, Some(4 * S + 1)),
        ];
        assert_eq!(cut_in(&marks, 3), Ok(3));
        assert_eq!(cut_in(&marks, 4), Ok(7));
        assert!(matches!(
            cut_in(&marks, 5),
            Err(Unrestorable::Inexact { .. })
        ));
        assert_eq!(cut_in(&marks, 6), Ok(9));
        assert_eq!(cut_in(&marks, 1), Err(Unrestorable::BeforeArchive(1)));
    }

    /// T-T1.12.P7: the chain takes the latest base on an unbroken log and the segments from its
    /// generation's first frame; a gap ends the walk, and no base past it is refused.
    #[test]
    fn t_t1_12_p7_chain() {
        let segments = vec![
            seg(1, 1, 10, Some((S, S)), S),
            seg(1, 11, 20, Some((2 * S, 2 * S)), 2 * S),
            seg(2, 1, 5, Some((3 * S, 3 * S)), 3 * S),
            seg(2, 6, 8, Some((4 * S, 4 * S)), 4 * S),
        ];
        let base = |end, start_gen, id| BaseKey { end, start_gen, id };
        let bases = vec![base((1, 12), 1, 1), base((2, 3), 2, 2), base((2, 7), 2, 3)];
        let plan = chain(&segments, &bases, 3, 7, 4).unwrap();
        assert_eq!(plan.base.id, 3);
        assert_eq!(plan.segments, vec![2, 3]);
        let plan = chain(&segments, &bases, 2, 4, 3).unwrap();
        assert_eq!(plan.base.id, 2);
        assert_eq!(plan.segments, vec![2]);
        let plan = chain(&segments, &bases, 2, 2, 3).unwrap();
        assert_eq!(plan.base.id, 1);
        assert_eq!(plan.segments, vec![0, 1, 2]);
        let mut gapped = segments.clone();
        gapped[2].link = Some(Link::Gap);
        let plan = chain(&gapped, &bases[..1], 1, 20, 2).unwrap();
        assert_eq!(plan.segments, vec![0, 1]);
        assert_eq!(
            chain(&gapped, &bases[..1], 2, 4, 3),
            Err(Unrestorable::NoBase(3))
        );
        let mut holed = segments;
        holed.remove(1);
        assert_eq!(
            chain(&holed, &bases, 1, 4, 3).map(|p| p.base.id),
            Ok(2),
            "a missing tail of generation 1 leaves only generation 2's bases"
        );
    }

    /// T-T1.12.P8: retention lets go of what precedes the base the oldest kept second needs.
    #[test]
    fn t_t1_12_p8_expired() {
        let mut segments = vec![
            seg(1, 1, 10, Some((S, S)), S),
            seg(2, 1, 5, Some((3 * S, 3 * S)), 3 * S),
            seg(2, 6, 8, Some((4 * S, 4 * S)), 4 * S),
        ];
        segments[1].link = Some(Link::Whole(10));
        // The oldest kept second (3) is cut, at the earliest, at segment 1's first frame.
        let bases = vec![
            BaseKey {
                end: (1, 5),
                start_gen: 1,
                id: 1,
            },
            BaseKey {
                end: (2, 1),
                start_gen: 2,
                id: 2,
            },
        ];
        let chunks = vec![
            (1, chunk_key("p", 1, 0)),
            (2, chunk_key("p", 2, 0)),
            (3, chunk_key("p", 3, 0)),
        ];
        let gone = expired("p", &segments, &bases, &chunks, 4 * S, S);
        assert_eq!(
            gone,
            vec![
                segments[0].key("p"),
                bases[0].key("p"),
                chunk_key("p", 1, 0)
            ]
        );
        assert!(expired("p", &segments, &bases, &chunks, 4 * S, 10 * S).is_empty());
    }

    /// T-T1.12.P9: a base is due with none, after a break, once the log outweighs it, and once a
    /// restore at the measured rates would pass the objective.
    #[test]
    fn t_t1_12_p9_base_due() {
        assert!(base_due(None, false, 0, None, None, 1800.0));
        assert!(base_due(Some(100), true, 0, None, None, 1800.0));
        assert!(base_due(Some(100), false, 100, None, None, 1800.0));
        assert!(!base_due(Some(100), false, 99, None, None, 1800.0));
        let rates = Rates {
            fetch: 1.0,
            replay: 1.0,
        };
        assert!(base_due(Some(1000), false, 900, None, Some(rates), 1800.0));
        assert!(!base_due(Some(1000), false, 10, None, Some(rates), 1800.0));
    }

    /// T-T1.12.I3: an incremental base is due once the log since the latest base outweighs what it
    /// would copy, well before the log outweighs the whole file.
    #[test]
    fn t_t1_12_i3_incremental_base_due() {
        assert!(base_due(Some(1000), false, 100, Some(100), None, 1800.0));
        assert!(!base_due(Some(1000), false, 99, Some(100), None, 1800.0));
        assert!(base_due(Some(1000), true, 0, Some(100), None, 1800.0));
    }

    /// T-T1.12.I4: a due base waits for the log to start again, unless no base restores the log
    /// yet or the generation holds what a restart keeps.
    #[test]
    fn t_t1_12_i4_base_begins_after_a_restart() {
        assert!(
            base_begins_now(true, false, 5, 100),
            "no base restores the log"
        );
        assert!(
            base_begins_now(false, true, 5, 100),
            "the log just started again"
        );
        assert!(
            !base_begins_now(false, false, 5, 100),
            "waits for the restart"
        );
        assert!(
            base_begins_now(false, false, 100, 100),
            "the generation is full"
        );
    }
}
