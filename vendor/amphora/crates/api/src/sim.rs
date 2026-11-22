//! [`SimRecords`] (CP-T1.5): one namespace's Records in memory, kept in Amphora's own layout so it
//! answers as Amphora does (docs/design/records.md, "The simulator"):
//! - every version a commit writes, by (family, key, revision), with its place in the commit's
//!   writes (`__amphora_versions`);
//! - the feed's index over those same versions, by (family, revision, place) (`__amphora_feed`):
//!   the change feed is read from the versions, never kept beside them;
//! - each family's newest change (`__amphora_families`), which `wait` and `changed` answer from;
//! - the snapshot horizon and the feed's, and one compaction's progress, moved by the bounded
//!   compaction steps Amphora takes (docs/design/records-compaction.md).
//!
//! Linearizable: one lock orders every call. Bounded: past [`SimLimits`] a commit is refused
//! `Full` and writes nothing; compaction frees versions. Clones share one store.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use crate::{
    Change, Changes, CompactionProgress, Cond, Consistency, Page, Records, Refusal, Rev, Row, Scan,
    TX_MAX, Tx, Write, bound, family, invalid, key, limit, transaction,
};

/// What a [`SimRecords`] holds before it refuses a commit `Full`. The caller states both: a test
/// sizes them to what it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimLimits {
    /// Live rows: keys whose newest version is not a delete (Amphora's `LIVE_MAX`).
    pub rows: usize,
    /// Versions kept, every key's history together. A compaction frees those it drops.
    pub versions: usize,
}

/// Records in memory, answering as Amphora's `Namespace` does.
#[derive(Clone)]
pub struct SimRecords {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    /// Told at every commit and every compaction step: a waiter checks its family again.
    changed: Condvar,
}

/// A version's identity: (family, key, revision).
type At = (String, Vec<u8>, Rev);

struct Version {
    /// Its place among its commit's writes.
    seq: usize,
    /// `None`: a delete.
    value: Option<Vec<u8>>,
}

#[derive(Default)]
struct Compaction {
    target: Option<Rev>,
    completed: Rev,
    cursor: Option<At>,
}

struct State {
    limits: SimLimits,
    head: Rev,
    /// Snapshots from before it are refused `Compacted`.
    horizon: Rev,
    /// Reads of changes, and waits, from before it are refused `Compacted`.
    feed: Rev,
    live: usize,
    versions: BTreeMap<At, Version>,
    /// The feed's index over `versions`: (family, revision, place) to key.
    by_rev: BTreeMap<(String, Rev, usize), Vec<u8>>,
    newest: BTreeMap<String, Rev>,
    compaction: Compaction,
}

fn poisoned<T>(_: T) -> Refusal {
    Refusal::Unavailable("the simulator's lock was poisoned by a panic".into())
}

fn overflow(what: &str) -> Refusal {
    invalid(what, "overflow")
}

/// `Timeout` once `until` has passed: a loop over the store's data ends at its caller's deadline.
fn in_time(until: Instant) -> Result<(), Refusal> {
    if Instant::now() >= until {
        Err(Refusal::Timeout)
    } else {
        Ok(())
    }
}

impl State {
    /// The newest version of `key` at or before `at`: its revision and value (`None`: deleted).
    fn version(&self, family: &str, key: &[u8], at: Rev) -> Option<(Rev, Option<&Vec<u8>>)> {
        let low = (family.to_owned(), key.to_vec(), 0);
        let high = (family.to_owned(), key.to_vec(), at);
        self.versions
            .range(low..=high)
            .next_back()
            .map(|((_, _, rev), v)| (*rev, v.value.as_ref()))
    }

    /// The row `key` holds at `at`.
    fn row(&self, family: &str, key: &[u8], at: Rev) -> Option<Row> {
        let (rev, value) = self.version(family, key, at)?;
        value.map(|value| Row {
            key: key.to_vec(),
            value: value.clone(),
            rev,
        })
    }

    fn snapshot(&self, at: Consistency) -> Result<Rev, Refusal> {
        match at {
            Consistency::Linearizable => Ok(self.head),
            Consistency::Snapshot(r) if r > self.head => Err(Refusal::Ahead {
                asked: r,
                head: self.head,
            }),
            Consistency::Snapshot(r) if r < self.horizon => Err(Refusal::Compacted {
                asked: r,
                horizon: self.horizon,
            }),
            Consistency::Snapshot(r) => Ok(r),
        }
    }

    fn changed(&self, family: &str, after: Rev) -> Result<Option<Rev>, Refusal> {
        if after < self.feed {
            return Err(Refusal::Compacted {
                asked: after,
                horizon: self.feed,
            });
        }
        Ok(self.newest.get(family).copied().filter(|r| *r > after))
    }

    fn scan(&self, family: &str, scan: &Scan, until: Instant) -> Result<Page, Refusal> {
        let at = self.snapshot(scan.at)?;
        // A cursor below the prefix is the prefix's first key, as Amphora reads it.
        let (lower, continued) = match &scan.after {
            Some(after) if *after >= scan.prefix => (after.clone(), true),
            _ => (scan.prefix.clone(), false),
        };
        let mut rows = Vec::new();
        let mut last: Option<&Vec<u8>> = None;
        let start = (family.to_owned(), lower.clone(), 0);
        for ((f, k, _), _) in self
            .versions
            .range((Bound::Included(start), Bound::Unbounded))
        {
            if f != family || !k.starts_with(&scan.prefix) {
                break;
            }
            if last == Some(k) || (continued && *k == lower) {
                continue;
            }
            in_time(until)?;
            last = Some(k);
            if let Some(row) = self.row(family, k, at) {
                rows.push(row);
                if rows.len() > scan.limit {
                    break;
                }
            }
        }
        let more = rows.len() > scan.limit;
        rows.truncate(scan.limit);
        Ok(Page { rows, more })
    }

    fn changes(&self, family: &str, after: Rev, limit: usize) -> Result<Changes, Refusal> {
        if after < self.feed {
            return Err(Refusal::Compacted {
                asked: after,
                horizon: self.feed,
            });
        }
        let head = self.head;
        if after >= head {
            return Ok(Changes {
                changes: Vec::new(),
                head,
            });
        }
        let from = (
            family.to_owned(),
            after.checked_add(1).ok_or_else(|| overflow("revision"))?,
            0,
        );
        let mut changes = Vec::new();
        for ((f, rev, _), k) in self.by_rev.range(from..) {
            if f != family || changes.len() > limit {
                break;
            }
            let value = self
                .versions
                .get(&(f.clone(), k.clone(), *rev))
                .ok_or_else(|| {
                    Refusal::Unavailable("the feed names a version that is gone".into())
                })?
                .value
                .clone();
            changes.push(Change {
                rev: *rev,
                key: k.clone(),
                value,
            });
        }
        if changes.len() > limit {
            // A page never ends inside a commit: its last revision is a cursor that loses nothing.
            let next = changes.get(limit).map(|c| c.rev);
            changes.truncate(limit);
            if changes.last().map(|c| c.rev) == next {
                changes.retain(|c| Some(c.rev) != next);
                if changes.is_empty() {
                    return Err(invalid(
                        "change limit",
                        "a transaction does not fit; request up to TX_MAX changes",
                    ));
                }
            }
        }
        Ok(Changes { changes, head })
    }

    fn commit(&mut self, tx: &Tx) -> Result<Rev, Refusal> {
        let head = self.head;
        for c in &tx.conds {
            let (Cond::Absent { family, key } | Cond::At { family, key, .. }) = c;
            let now = self.row(family, key, head).map(|r| r.rev);
            let holds = match c {
                Cond::Absent { .. } => now.is_none(),
                Cond::At { rev, .. } => now == Some(*rev),
            };
            if !holds {
                return Err(Refusal::Conflict {
                    family: family.clone(),
                    key: key.clone(),
                    at: now,
                });
            }
        }
        if tx.writes.is_empty() {
            return Ok(head);
        }
        // A key written twice in one commit keeps its last write, at that write's place.
        let mut last = BTreeMap::new();
        for (seq, w) in tx.writes.iter().enumerate() {
            let (Write::Put { family, key, .. } | Write::Delete { family, key }) = w;
            last.insert((family, key), (seq, w));
        }
        let mut live = self.live;
        for ((family, key), (_, w)) in &last {
            let was = self.row(family, key, head).is_some();
            live = match (was, matches!(w, Write::Put { .. })) {
                (false, true) => live.checked_add(1).ok_or_else(|| overflow("live rows"))?,
                (true, false) => live.checked_sub(1).ok_or_else(|| overflow("live rows"))?,
                _ => live,
            };
        }
        if live > self.limits.rows {
            return Err(Refusal::Full {
                limit: self.limits.rows,
            });
        }
        let versions = self
            .versions
            .len()
            .checked_add(last.len())
            .ok_or_else(|| overflow("versions"))?;
        if versions > self.limits.versions {
            return Err(Refusal::Full {
                limit: self.limits.versions,
            });
        }
        let at = head.checked_add(1).ok_or_else(|| overflow("revision"))?;
        for ((family, key), (seq, w)) in last {
            let value = match w {
                Write::Put { value, .. } => Some(value.clone()),
                Write::Delete { .. } => None,
            };
            self.versions
                .insert((family.clone(), key.clone(), at), Version { seq, value });
            self.by_rev.insert((family.clone(), at, seq), key.clone());
            self.newest.insert(family.clone(), at);
        }
        self.live = live;
        self.head = at;
        Ok(at)
    }

    /// The first version after `cursor` at or before `cutoff`, in (family, key, revision) order,
    /// and whether it is a delete.
    fn next(
        &self,
        cutoff: Rev,
        cursor: Option<&At>,
        until: Instant,
    ) -> Result<Option<(At, bool)>, Refusal> {
        let from = match cursor {
            Some(c) => Bound::Excluded(c.clone()),
            None => Bound::Unbounded,
        };
        for (at, v) in self.versions.range((from, Bound::Unbounded)) {
            if at.2 <= cutoff {
                return Ok(Some((at.clone(), v.value.is_none())));
            }
            in_time(until)?;
        }
        Ok(None)
    }

    /// One bounded compaction step, as Amphora's (crates/amphora/src/database/compaction.rs):
    /// raise the horizons, then visit at most TX_MAX versions at or before the cycle's target,
    /// dropping each one a newer version at or before the target hides, and each delete before it
    /// with nothing older left.
    fn compact_step(&mut self, before: Rev, until: Instant) -> Result<CompactionProgress, Refusal> {
        let head = self.head;
        let horizon = before.min(head).max(self.horizon);
        let feed = self.feed.max(horizon.saturating_sub(1));
        self.horizon = horizon;
        self.feed = feed;
        let mut target = self.compaction.target;
        let mut completed = self.compaction.completed;
        let mut cursor = self.compaction.cursor.clone();
        if target.is_none() && completed < horizon {
            target = Some(horizon);
            cursor = None;
        }
        let mut visited = 0usize;
        let mut deleted = 0usize;
        if let Some(cutoff) = target {
            while visited < TX_MAX {
                let Some((at, tombstone)) = self.next(cutoff, cursor.as_ref(), until)? else {
                    break;
                };
                let (family, key, revision) = &at;
                let latest = self.version(family, key, cutoff).map_or(0, |(r, _)| r);
                let mut obsolete = *revision < latest;
                if !obsolete && tombstone && *revision < cutoff {
                    // Revisions start at 1, so a version before this one is at or before r - 1.
                    obsolete = self
                        .version(family, key, revision.saturating_sub(1))
                        .is_none();
                }
                if obsolete && let Some(v) = self.versions.remove(&at) {
                    self.by_rev.remove(&(family.clone(), *revision, v.seq));
                    deleted = deleted
                        .checked_add(1)
                        .ok_or_else(|| overflow("deleted count"))?;
                }
                cursor = Some(at);
                visited = visited
                    .checked_add(1)
                    .ok_or_else(|| overflow("visited count"))?;
            }
            if self.next(cutoff, cursor.as_ref(), until)?.is_none() {
                completed = cutoff;
                target = if completed < horizon {
                    Some(horizon)
                } else {
                    None
                };
                cursor = None;
            }
        }
        self.compaction = Compaction {
            target,
            completed,
            cursor,
        };
        Ok(CompactionProgress {
            head,
            horizon,
            feed,
            completed,
            target,
            visited,
            deleted,
            more: completed < horizon,
        })
    }
}

impl SimRecords {
    /// An empty store that holds what `limits` say.
    pub fn new(limits: SimLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    limits,
                    head: 0,
                    horizon: 0,
                    feed: 0,
                    live: 0,
                    versions: BTreeMap::new(),
                    by_rev: BTreeMap::new(),
                    newest: BTreeMap::new(),
                    compaction: Compaction::default(),
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Refusal> {
        self.inner.state.lock().map_err(poisoned)
    }
}

impl Records for SimRecords {
    fn get(
        &self,
        f: &str,
        k: &[u8],
        at: Consistency,
        _until: Instant,
    ) -> Result<Option<Row>, Refusal> {
        family(f)?;
        key(k)?;
        let st = self.state()?;
        let at = st.snapshot(at)?;
        Ok(st.row(f, k, at))
    }

    fn scan(&self, f: &str, mut scan: Scan, until: Instant) -> Result<Page, Refusal> {
        family(f)?;
        scan.limit = limit(scan.limit)?;
        bound("prefix", scan.prefix.len(), crate::KEY_MAX)?;
        if let Some(after) = &scan.after {
            key(after)?;
        }
        self.state()?.scan(f, &scan, until)
    }

    fn commit(&self, tx: Tx, _until: Instant) -> Result<Rev, Refusal> {
        transaction(&tx)?;
        let rev = self.state()?.commit(&tx)?;
        self.inner.changed.notify_all();
        Ok(rev)
    }

    fn changes(&self, f: &str, after: Rev, n: usize, _until: Instant) -> Result<Changes, Refusal> {
        family(f)?;
        let n = limit(n)?;
        self.state()?.changes(f, after, n)
    }

    fn wait(&self, f: &str, after: Rev, until: Instant) -> Result<Rev, Refusal> {
        family(f)?;
        let mut st = self.state()?;
        loop {
            if let Some(rev) = st.changed(f, after)? {
                return Ok(rev);
            }
            let left = until
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or(Refusal::Timeout)?;
            st = self
                .inner
                .changed
                .wait_timeout(st, left)
                .map_err(poisoned)?
                .0;
        }
    }

    fn changed(&self, f: &str, after: Rev, _until: Instant) -> Result<Option<Rev>, Refusal> {
        family(f)?;
        self.state()?.changed(f, after)
    }

    fn head(&self, _until: Instant) -> Result<Rev, Refusal> {
        Ok(self.state()?.head)
    }

    fn compact(&self, before: Rev, until: Instant) -> Result<(), Refusal> {
        crate::compact(self, before, until)
    }

    fn compact_step(&self, before: Rev, until: Instant) -> Result<CompactionProgress, Refusal> {
        in_time(until)?;
        let progress = self.state()?.compact_step(before, until)?;
        // Waits from before the feed's new horizon are refused now, not left waiting.
        self.inner.changed.notify_all();
        Ok(progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{cases, run};
    use std::time::Duration;

    /// The tests' bound on a call that never answers (docs/design/local-owner-transport.md, "The
    /// tests' budgets": HANG, measured there).
    const HANG: Duration = Duration::from_secs(120);

    fn until() -> Instant {
        Instant::now() + HANG
    }

    fn put(key: &str, value: &str) -> Write {
        Write::Put {
            family: "f".into(),
            key: key.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        }
    }

    fn tx(writes: Vec<Write>) -> Tx {
        Tx {
            conds: Vec::new(),
            writes,
        }
    }

    fn roomy() -> SimLimits {
        SimLimits {
            rows: TX_MAX,
            versions: TX_MAX,
        }
    }

    /// CP-T1.5 (AC-T1.5.3): the simulator passes the Records suite, every case.
    #[test]
    fn the_simulator_passes_the_records_suite() {
        let report = run(&cases(), || Ok(SimRecords::new(roomy())), HANG);
        assert_eq!(report.passed.len(), cases().len(), "{:?}", report.failed);
    }

    /// CP-T1.5: past its live rows, or its versions, a commit is refused Full and writes nothing;
    /// a compaction frees the versions it drops, and the commit then goes in.
    #[test]
    fn its_rows_and_versions_are_bounded_and_say_so() {
        let r = SimRecords::new(SimLimits {
            rows: 2,
            versions: 4,
        });
        let first = r
            .commit(tx(vec![put("a", "1"), put("b", "1")]), until())
            .unwrap();
        assert_eq!(
            r.commit(tx(vec![put("c", "1")]), until()),
            Err(Refusal::Full { limit: 2 })
        );
        // A delete and a put in one commit stay within the rows.
        r.commit(
            tx(vec![
                Write::Delete {
                    family: "f".into(),
                    key: b"b".to_vec(),
                },
                put("c", "1"),
            ]),
            until(),
        )
        .unwrap();
        // Four versions held: a fifth is refused, and the head stays.
        let head = r.head(until()).unwrap();
        assert_eq!(
            r.commit(tx(vec![put("a", "2")]), until()),
            Err(Refusal::Full { limit: 4 })
        );
        assert_eq!(r.head(until()).unwrap(), head);
        assert_eq!(
            r.get("f", b"a", Consistency::Snapshot(first), until())
                .unwrap()
                .map(|x| x.value),
            Some(b"1".to_vec())
        );
        // Compacted to the head: b's put, hidden by its delete, goes (the delete, at the target,
        // stays).
        r.compact(head, until()).unwrap();
        r.commit(tx(vec![put("a", "2")]), until()).unwrap();
        assert_eq!(
            r.get("f", b"a", Consistency::Linearizable, until())
                .unwrap()
                .map(|x| x.value),
            Some(b"2".to_vec())
        );
    }

    /// CP-T1.5: a compaction steps at most TX_MAX versions at a time, keeps each key's version at
    /// its target and the deletes at it, and its horizons refuse what's before them.
    #[test]
    fn compaction_steps_are_bounded_and_keep_the_target_s_reads() {
        let r = SimRecords::new(SimLimits {
            rows: TX_MAX,
            versions: 3 * TX_MAX,
        });
        let keys: Vec<String> = (0..TX_MAX).map(|i| format!("k{i:04}")).collect();
        r.commit(tx(keys.iter().map(|k| put(k, "1")).collect()), until())
            .unwrap();
        let second = r
            .commit(tx(keys.iter().map(|k| put(k, "2")).collect()), until())
            .unwrap();
        let first = r.compact_step(second, until()).unwrap();
        assert_eq!(
            (first.visited, first.deleted, first.more),
            (TX_MAX, TX_MAX / 2, true)
        );
        assert_eq!(first.horizon, second);
        assert_eq!(first.feed, second - 1);
        let rest = r.compact_step(second, until()).unwrap();
        assert_eq!(
            (rest.visited, rest.deleted, rest.more, rest.completed),
            (TX_MAX, TX_MAX / 2, false, second)
        );
        assert_eq!(
            r.get("f", b"k0000", Consistency::Snapshot(second), until())
                .unwrap()
                .map(|x| x.value),
            Some(b"2".to_vec())
        );
        assert!(matches!(
            r.get("f", b"k0000", Consistency::Snapshot(second - 1), until()),
            Err(Refusal::Compacted { .. })
        ));
        assert_eq!(
            r.changes("f", second - 1, TX_MAX, until())
                .unwrap()
                .changes
                .len(),
            TX_MAX
        );
        assert!(matches!(
            r.changes("f", 0, 10, until()),
            Err(Refusal::Compacted { asked: 0, .. })
        ));
    }

    /// CP-T1.5: a wait left waiting when a compaction moves the feed past it is refused then.
    #[test]
    fn a_wait_overtaken_by_a_compaction_is_refused() {
        let r = SimRecords::new(roomy());
        let other = |v: &str| {
            tx(vec![Write::Put {
                family: "other".into(),
                key: b"x".to_vec(),
                value: v.as_bytes().to_vec(),
            }])
        };
        r.commit(other("1"), until()).unwrap();
        r.commit(other("2"), until()).unwrap();
        let head = r.commit(other("3"), until()).unwrap();
        let waited = std::thread::scope(|s| {
            let waiter = s.spawn(|| r.wait("f", 0, until()));
            r.compact(head, until()).unwrap();
            waiter.join().unwrap()
        });
        assert_eq!(
            waited,
            Err(Refusal::Compacted {
                asked: 0,
                horizon: head - 1
            })
        );
    }
}
