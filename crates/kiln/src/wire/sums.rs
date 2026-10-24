//! What a copy of an agent session has of its events, said in digests, so a stream that starts
//! again (its runner started again, or the server lost the stream) sends only what the copy lacks
//! (docs/wire.md, "Restarts").
//!
//! The server says it of each session of a runner it has a copy of, before `Welcome`
//! ([`crate::proto::KeptSession`]): the copy's epoch, its oldest and newest events' numbers, a
//! digest of each run of [`RUN`] events from its oldest on, and one of each of its last [`LAST`]
//! events. The runner works out the same of its own timeline ([`delta`]) and sends the events after
//! the copy's newest, and those through it whose digest differs; the copy keeps the rest as they
//! are. Past the runner's own newest the copy lets go (it had what the runner lost: a store that
//! hadn't kept it yet when the runner stopped). A digest is of an event as the copy keeps it: its
//! JSON as it goes over the wire (cut to fit, as `agent::harness_frames` cuts it), read back.

use sha2::{Digest as _, Sha256};

use crate::proto::KeptSession;

/// How many events a run's digest covers.
pub const RUN: u64 = 64;
/// How many of its newest events a copy gives a digest of each.
pub const LAST: u64 = 64;

/// The first eight bytes of a SHA-256, as a number.
fn short(h: Sha256) -> u64 {
    h.finalize().iter().take(8).fold(0u64, |acc, b| (acc << 8) | u64::from(*b))
}

/// An event's digest: of its JSON as a copy keeps it.
pub fn event(v: &serde_json::Value) -> u64 {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(v).unwrap_or_default());
    short(h)
}

/// A run's digest: of its events' numbers and digests, in order.
fn run(events: &[(u64, u64)]) -> u64 {
    let mut h = Sha256::new();
    for (seq, sum) in events {
        h.update(seq.to_be_bytes());
        h.update(sum.to_be_bytes());
    }
    short(h)
}

/// Those of `events` (each its number and digest, by number) numbered from `a` up to `b`.
fn within(events: &[(u64, u64)], a: u64, b: u64) -> &[(u64, u64)] {
    let start = events.partition_point(|(s, _)| *s < a);
    let end = events.partition_point(|(s, _)| *s < b);
    events.get(start..end).unwrap_or_default()
}

/// What a copy has of `session`'s events, said in digests. `events`: each its number and digest,
/// by number.
pub fn kept(session: String, epoch: u64, events: &[(u64, u64)]) -> KeptSession {
    let (Some(&(from, _)), Some(&(seq, _))) = (events.first(), events.last()) else {
        return KeptSession { session, epoch, seq: 0, from: 0, run: RUN, runs: Vec::new(), last: Vec::new() };
    };
    let tail = seq.saturating_add(1).saturating_sub(LAST).max(from);
    let mut runs = Vec::new();
    let mut a = from;
    while a < tail {
        let b = a.saturating_add(RUN).min(tail);
        runs.push(run(within(events, a, b)));
        a = b;
    }
    let last = (tail..=seq).map(|s| within(events, s, s.saturating_add(1)).first().map_or(0, |(_, d)| *d)).collect();
    KeptSession { session, epoch, seq, from, run: RUN, runs, last }
}

/// What a stream that starts again sends of a session whose copy is as [`kept`] says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// The copy keeps its events through this number; every event after it is sent.
    pub since: u64,
    /// The events through `since` sent again (the copy's differ, or it lacks them), by number.
    pub again: Vec<u64>,
}

/// What this side sends of a session whose copy is `kept` (of the same epoch: the caller's to
/// see). `mine`: this side's events, each its number and digest, by number. None: all of it, the
/// copy having none of it, or none from this side's oldest on (what's between can't be sent).
pub fn delta(kept: &KeptSession, mine: &[(u64, u64)]) -> Option<Delta> {
    let (Some(&(first, _)), Some(&(newest, _))) = (mine.first(), mine.last()) else { return None };
    if kept.seq == 0 {
        return None;
    }
    let since = kept.seq.min(newest);
    if since.saturating_add(1) < first {
        return None;
    }
    let tail = kept.seq.saturating_add(1).saturating_sub(u64::try_from(kept.last.len()).unwrap_or(u64::MAX));
    let size = kept.run.max(1);
    let mut again = Vec::new();
    let mut a = kept.from;
    for want in &kept.runs {
        let b = a.saturating_add(size).min(tail);
        // A run this side has some of (from its oldest on), and not past what the copy keeps.
        if b > first && a <= since {
            let have = within(mine, a, b);
            if run(have) != *want {
                again.extend(have.iter().map(|(s, _)| *s).filter(|s| *s <= since));
            }
        }
        a = b;
    }
    for (s, want) in (tail..).zip(&kept.last) {
        if s > since {
            break;
        }
        if let Some(&(_, d)) = within(mine, s, s.saturating_add(1)).first()
            && d != *want
        {
            again.push(s);
        }
    }
    Some(Delta { since, again })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events numbered `from..=to`, each its digest made from `tag` and its number.
    fn events(from: u64, to: u64, tag: &str) -> Vec<(u64, u64)> {
        (from..=to).map(|s| (s, event(&serde_json::json!({"seq": s, "text": format!("{tag} {s}")})))).collect()
    }

    fn changed(mut evs: Vec<(u64, u64)>, at: &[u64]) -> Vec<(u64, u64)> {
        for (s, d) in &mut evs {
            if at.contains(s) {
                *d = event(&serde_json::json!({"seq": *s, "text": "changed"}));
            }
        }
        evs
    }

    #[test]
    fn the_same_timeline_sends_only_what_follows() {
        let copy = kept("s".into(), 2, &events(1, 300, "line"));
        assert_eq!((copy.from, copy.seq, copy.runs.len(), copy.last.len()), (1, 300, 4, 64), "runs of 64 up to the last 64, each of those its own");
        assert_eq!(delta(&copy, &events(1, 340, "line")), Some(Delta { since: 300, again: Vec::new() }));
    }

    #[test]
    fn an_event_that_changed_since_goes_again_with_its_run_or_alone() {
        let copy = kept("s".into(), 2, &events(1, 300, "line"));
        // In the last 64: that event alone.
        assert_eq!(delta(&copy, &changed(events(1, 310, "line"), &[290])), Some(Delta { since: 300, again: vec![290] }));
        // Further back: the run of 64 it's in (from 65 up to 128).
        let d = delta(&copy, &changed(events(1, 300, "line"), &[100])).unwrap_or(Delta { since: 0, again: Vec::new() });
        assert_eq!((d.since, d.again.first().copied(), d.again.last().copied(), d.again.len()), (300, Some(65), Some(128), 64));
    }

    #[test]
    fn a_copy_with_more_than_this_side_goes_on_from_this_sides_newest() {
        // Its runner's store hadn't kept the last few when it stopped: the copy lets go of them.
        let copy = kept("s".into(), 2, &events(1, 300, "line"));
        assert_eq!(delta(&copy, &events(1, 296, "line")), Some(Delta { since: 296, again: Vec::new() }));
    }

    #[test]
    fn a_copy_of_nothing_or_with_a_gap_gets_all_of_it() {
        assert_eq!(delta(&kept("s".into(), 2, &[]), &events(1, 10, "line")), None, "the copy has none of it");
        assert_eq!(delta(&kept("s".into(), 2, &events(1, 10, "line")), &events(50, 60, "line")), None, "nothing between its newest and this side's oldest");
        assert_eq!(delta(&kept("s".into(), 2, &events(1, 10, "line")), &[]), None, "this side has none of it");
    }

    #[test]
    fn what_this_side_no_longer_holds_stays_as_the_copy_has_it() {
        // This side holds only its newest 60 of 210: the copy's older ones aren't compared or sent.
        let copy = kept("s".into(), 2, &events(1, 200, "line"));
        assert_eq!(delta(&copy, &events(151, 210, "line")), Some(Delta { since: 200, again: Vec::new() }));
        // Its oldest inside a run the copy has whole: only its part of that run goes, when it differs.
        let copy = kept("s".into(), 2, &events(1, 300, "line"));
        let d = delta(&copy, &events(100, 300, "line")).unwrap_or(Delta { since: 0, again: Vec::new() });
        assert_eq!((d.since, d.again.first().copied(), d.again.last().copied()), (300, Some(100), Some(128)));
    }

    #[test]
    fn a_long_timeline_is_said_in_a_few_hundred_digests() {
        let copy = kept("s".into(), 2, &events(1, 5_000, "line"));
        assert_eq!(copy.runs.len() + copy.last.len(), 78 + 64);
        assert_eq!(delta(&copy, &events(1, 5_000, "line")), Some(Delta { since: 5_000, again: Vec::new() }));
    }
}
