//! `Records`: every record kiln keeps, by family and key, each at the revision of the commit that
//! last wrote it. Transactions are all or nothing, with conditions (a row absent, or at a revision:
//! compare-and-set); reads state their consistency; each family has a change feed, and a wait for
//! the next change, so a driver reads changes instead of polling. Amphora is the real one
//! (CP-T1.5, which grows the suite past 200 cases); [`SimRecords`] keeps it in memory, its history
//! bounded.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, refused, same};

/// A commit's number: every commit gets one higher than every commit before it. 0: none yet.
pub type Rev = u64;

/// A row as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// The commit that last wrote it.
    pub rev: Rev,
}

/// What a read sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consistency {
    /// Every commit acknowledged before the read began.
    Linearizable,
    /// The store as it was just after commit `Rev`.
    Snapshot(Rev),
}

/// A condition a transaction commits only under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    /// No row at `key`.
    Absent { family: String, key: Vec<u8> },
    /// The row at `key` was last written by commit `rev` (compare-and-set).
    At { family: String, key: Vec<u8>, rev: Rev },
}

/// A write a transaction makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    Put { family: String, key: Vec<u8>, value: Vec<u8> },
    Delete { family: String, key: Vec<u8> },
}

/// Writes made together, under conditions: every one or none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tx {
    pub conds: Vec<Cond>,
    pub writes: Vec<Write>,
}

/// Rows of one family in key order (bytewise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// Only keys that start with it.
    pub prefix: Vec<u8>,
    /// Only keys after it (the last key of the page before).
    pub after: Option<Vec<u8>>,
    /// Most rows (1 to [`SCAN_MAX`]).
    pub limit: usize,
    pub at: Consistency,
}

/// A page of a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub rows: Vec<Row>,
    /// More rows follow the last one.
    pub more: bool,
}

/// One committed change to a row: its new value, or `None` for a delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub rev: Rev,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// Changes to a family after a revision, in commit order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    pub changes: Vec<Change>,
    /// The newest commit, of any family, when they were read.
    pub head: Rev,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("a condition didn't hold for {family}/{key}: it's at {at:?}", key = hex::encode(key))]
    Conflict { family: String, key: Vec<u8>, at: Option<Rev> },
    #[error("{what} is {size} bytes or items, over its limit of {limit}")]
    TooLarge { what: &'static str, size: usize, limit: usize },
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("the store holds its most rows ({limit})")]
    Full { limit: usize },
    #[error("revision {asked} is older than what's kept (from {horizon})")]
    Compacted { asked: Rev, horizon: Rev },
    #[error("revision {asked} isn't committed yet (the head is {head})")]
    Ahead { asked: Rev, head: Rev },
    #[error("the store is unavailable: {0}")]
    Unavailable(String),
}

/// Every record kiln keeps.
pub trait Records: Send + Sync + 'static {
    /// The row at `key`, if any.
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>>;
    /// Rows of `family` in key order.
    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>>;
    /// Commits `tx`: every write at one new revision, or none (a condition failed). A transaction
    /// with no writes commits nothing and returns the head.
    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>>;
    /// Changes to `family` committed after `after`, oldest first, at most `limit`.
    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>>;
    /// Completes with the newest revision of a change to `family` once there's one after `after`
    /// (at once when there is already).
    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>>;
    /// The newest commit's revision.
    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>>;
}

/// Longest family name.
pub const FAMILY_MAX: usize = 64;
/// Longest key.
pub const KEY_MAX: usize = 1024;
/// Largest value.
pub const VALUE_MAX: usize = 1 << 20;
/// Most conditions, and most writes, in one transaction.
pub const TX_MAX: usize = 1000;
/// Most rows a scan page or a change read returns.
pub const SCAN_MAX: usize = 1000;

fn check_family(family: &str) -> Result<(), Refusal> {
    let ok = !family.is_empty() && family.len() <= FAMILY_MAX && family.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'));
    if ok { Ok(()) } else { Err(Refusal::Invalid { what: "a family name", why: format!("{family:?}: 1 to {FAMILY_MAX} of a-z, 0-9, _ - .") }) }
}

fn check_key(key: &[u8]) -> Result<(), Refusal> {
    if key.is_empty() {
        return Err(Refusal::Invalid { what: "a key", why: "it's empty".into() });
    }
    if key.len() > KEY_MAX {
        return Err(Refusal::TooLarge { what: "a key", size: key.len(), limit: KEY_MAX });
    }
    Ok(())
}

fn check_limit(limit: usize) -> Result<usize, Refusal> {
    if limit == 0 { Err(Refusal::Invalid { what: "a limit", why: "it's 0".into() }) } else { Ok(limit.min(SCAN_MAX)) }
}

// ---------------------------------------------------------------------------------- simulator

/// What a [`SimRecords`] keeps.
#[derive(Debug, Clone, Copy)]
pub struct SimLimits {
    /// Most live rows.
    pub rows: usize,
    /// Versions kept per key, for snapshot reads (the newest always).
    pub versions: usize,
    /// Changes kept for the change feed.
    pub changes: usize,
}

impl Default for SimLimits {
    fn default() -> Self {
        Self { rows: 1_000_000, versions: 32, changes: 100_000 }
    }
}

/// Records in memory: linearizable (one lock), snapshot reads from each key's bounded history.
/// Clones share one store.
#[derive(Clone, Default)]
pub struct SimRecords {
    inner: Arc<SimInner>,
}

#[derive(Default)]
struct SimInner {
    state: Mutex<SimState>,
    changed: event_listener::Event,
}

#[derive(Default)]
struct SimState {
    limits: SimLimits,
    head: Rev,
    /// family → key → versions, oldest first: (rev, value or deleted).
    families: BTreeMap<String, BTreeMap<Vec<u8>, VecDeque<(Rev, Option<Vec<u8>>)>>>,
    /// Revisions below this may have lost versions (a snapshot there is refused).
    horizon: Rev,
    log: VecDeque<(Rev, String, Vec<u8>, Option<Vec<u8>>)>,
    /// The feed starts after this revision.
    log_horizon: Rev,
    live: usize,
}

impl SimState {
    fn latest(&self, family: &str, key: &[u8]) -> Option<(Rev, Option<&Vec<u8>>)> {
        self.families.get(family)?.get(key)?.back().map(|(r, v)| (*r, v.as_ref()))
    }

    fn row_at(&self, family: &str, key: &[u8], at: Option<Rev>) -> Option<Row> {
        let versions = self.families.get(family)?.get(key)?;
        let (rev, value) = match at {
            None => versions.back()?,
            Some(at) => versions.iter().rev().find(|(r, _)| *r <= at)?,
        };
        value.as_ref().map(|v| Row { key: key.to_vec(), value: v.clone(), rev: *rev })
    }

    fn snapshot(&self, at: Consistency) -> Result<Option<Rev>, Refusal> {
        match at {
            Consistency::Linearizable => Ok(None),
            Consistency::Snapshot(r) if r > self.head => Err(Refusal::Ahead { asked: r, head: self.head }),
            Consistency::Snapshot(r) if r < self.horizon => Err(Refusal::Compacted { asked: r, horizon: self.horizon }),
            Consistency::Snapshot(r) => Ok(Some(r)),
        }
    }
}

impl SimRecords {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that keeps what `limits` say.
    pub fn with_limits(limits: SimLimits) -> Self {
        let s = Self::default();
        s.inner.state.lock().limits = limits;
        s
    }
}

impl Records for SimRecords {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
        let out = (|| {
            check_family(family)?;
            check_key(key)?;
            let st = self.inner.state.lock();
            let at = st.snapshot(at)?;
            Ok(st.row_at(family, key, at))
        })();
        Box::pin(async move { out })
    }

    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
        let out = (|| {
            check_family(family)?;
            let limit = check_limit(scan.limit)?;
            let st = self.inner.state.lock();
            let at = st.snapshot(scan.at)?;
            let Some(keys) = st.families.get(family) else { return Ok(Page { rows: Vec::new(), more: false }) };
            let start = match &scan.after {
                Some(a) if a.as_slice() >= scan.prefix.as_slice() => std::ops::Bound::Excluded(a.clone()),
                _ => std::ops::Bound::Included(scan.prefix.clone()),
            };
            let mut rows = Vec::new();
            let mut more = false;
            for (k, _) in keys.range((start, std::ops::Bound::Unbounded)) {
                if !k.starts_with(&scan.prefix) {
                    break;
                }
                let Some(row) = st.row_at(family, k, at) else { continue };
                if rows.len() >= limit {
                    more = true;
                    break;
                }
                rows.push(row);
            }
            Ok(Page { rows, more })
        })();
        Box::pin(async move { out })
    }

    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> {
        let out = (|| {
            if tx.conds.len() > TX_MAX {
                return Err(Refusal::TooLarge { what: "a transaction's conditions", size: tx.conds.len(), limit: TX_MAX });
            }
            if tx.writes.len() > TX_MAX {
                return Err(Refusal::TooLarge { what: "a transaction's writes", size: tx.writes.len(), limit: TX_MAX });
            }
            for c in &tx.conds {
                let (Cond::Absent { family, key } | Cond::At { family, key, .. }) = c;
                check_family(family)?;
                check_key(key)?;
            }
            for w in &tx.writes {
                let (Write::Put { family, key, .. } | Write::Delete { family, key }) = w;
                check_family(family)?;
                check_key(key)?;
                if let Write::Put { value, .. } = w
                    && value.len() > VALUE_MAX
                {
                    return Err(Refusal::TooLarge { what: "a value", size: value.len(), limit: VALUE_MAX });
                }
            }
            let mut st = self.inner.state.lock();
            for c in &tx.conds {
                match c {
                    Cond::Absent { family, key } => {
                        if let Some((at, Some(_))) = st.latest(family, key) {
                            return Err(Refusal::Conflict { family: family.clone(), key: key.clone(), at: Some(at) });
                        }
                    }
                    Cond::At { family, key, rev } => {
                        let now = st.latest(family, key).and_then(|(at, v)| v.map(|_| at));
                        if now != Some(*rev) {
                            return Err(Refusal::Conflict { family: family.clone(), key: key.clone(), at: now });
                        }
                    }
                }
            }
            if tx.writes.is_empty() {
                return Ok(st.head);
            }
            // The rows this commit would leave live, checked before anything is written.
            let mut live = st.live;
            let mut seen: BTreeMap<(&str, &[u8]), bool> = BTreeMap::new();
            for w in &tx.writes {
                let (family, key, puts) = match w {
                    Write::Put { family, key, .. } => (family.as_str(), key.as_slice(), true),
                    Write::Delete { family, key } => (family.as_str(), key.as_slice(), false),
                };
                let was = *seen.entry((family, key)).or_insert_with(|| st.latest(family, key).is_some_and(|(_, v)| v.is_some()));
                live = match (was, puts) {
                    (false, true) => live.saturating_add(1),
                    (true, false) => live.saturating_sub(1),
                    _ => live,
                };
                seen.insert((family, key), puts);
            }
            if live > st.limits.rows {
                return Err(Refusal::Full { limit: st.limits.rows });
            }
            let rev = st.head.saturating_add(1);
            let (versions_max, log_max) = (st.limits.versions.max(1), st.limits.changes.max(1));
            for w in tx.writes {
                let (family, key, value) = match w {
                    Write::Put { family, key, value } => (family, key, Some(value)),
                    Write::Delete { family, key } => (family, key, None),
                };
                let versions = st.families.entry(family.clone()).or_default().entry(key.clone()).or_default();
                // A key written twice in one commit keeps the last.
                if versions.back().is_some_and(|(r, _)| *r == rev) {
                    versions.pop_back();
                }
                versions.push_back((rev, value.clone()));
                let mut dropped = None;
                while versions.len() > versions_max {
                    dropped = versions.pop_front().map(|(r, _)| r);
                }
                if let Some(r) = dropped {
                    st.horizon = st.horizon.max(r.saturating_add(1));
                }
                st.log.push_back((rev, family, key, value));
                while st.log.len() > log_max {
                    if let Some((r, ..)) = st.log.pop_front() {
                        st.log_horizon = st.log_horizon.max(r);
                    }
                }
            }
            st.live = live;
            st.head = rev;
            Ok(rev)
        })();
        if out.is_ok() {
            self.inner.changed.notify(usize::MAX);
        }
        Box::pin(async move { out })
    }

    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
        let out = (|| {
            check_family(family)?;
            let limit = check_limit(limit)?;
            let st = self.inner.state.lock();
            if after < st.log_horizon {
                return Err(Refusal::Compacted { asked: after, horizon: st.log_horizon });
            }
            let changes = st.log.iter().filter(|(r, f, ..)| *r > after && f == family).take(limit).map(|(r, _, k, v)| Change { rev: *r, key: k.clone(), value: v.clone() }).collect();
            Ok(Changes { changes, head: st.head })
        })();
        Box::pin(async move { out })
    }

    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            check_family(family)?;
            loop {
                let listener = self.inner.changed.listen();
                {
                    let st = self.inner.state.lock();
                    if let Some(r) = st.log.iter().rev().find(|(_, f, ..)| f == family).map(|(r, ..)| *r).filter(|r| *r > after) {
                        return Ok(r);
                    }
                }
                listener.await;
            }
        })
    }

    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
        let h = self.inner.state.lock().head;
        Box::pin(async move { Ok(h) })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Records` conformance suite runs against: an empty store per case.
pub trait RecordsFixture: Send + Sync {
    fn records(&self) -> &dyn Records;
}

/// The fixture for a [`SimRecords`].
pub struct SimRecordsFixture(pub SimRecords);

impl RecordsFixture for SimRecordsFixture {
    fn records(&self) -> &dyn Records {
        &self.0
    }
}

const F: &str = "conformance";

fn put(key: &str, value: &str) -> Write {
    Write::Put { family: F.into(), key: key.as_bytes().to_vec(), value: value.as_bytes().to_vec() }
}

fn tx(writes: Vec<Write>) -> Tx {
    Tx { conds: Vec::new(), writes }
}

async fn value(r: &dyn Records, key: &str) -> Result<Option<String>, Failure> {
    Ok(fine("get", r.get(F, key.as_bytes(), Consistency::Linearizable).await)?.map(|row| String::from_utf8_lossy(&row.value).into_owned()))
}

fn scan(prefix: &str, after: Option<&str>, limit: usize) -> Scan {
    Scan { prefix: prefix.as_bytes().to_vec(), after: after.map(|a| a.as_bytes().to_vec()), limit, at: Consistency::Linearizable }
}

fn keys(p: &Page) -> Vec<String> {
    p.rows.iter().map(|r| String::from_utf8_lossy(&r.key).into_owned()).collect()
}

fn c1<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let rev = fine("commit", r.commit(tx(vec![put("a", "one")])).await)?;
        let row = fine("get", r.get(F, b"a", Consistency::Linearizable).await)?;
        same("the row read back", row, Some(Row { key: b"a".to_vec(), value: b"one".to_vec(), rev }))?;
        same("a key never written", fine("get", r.get(F, b"zz", Consistency::Linearizable).await)?, None)
    })
}

fn c2<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let start = fine("head", r.head().await)?;
        let mut last = start;
        for i in 0..5 {
            let rev = fine("commit", r.commit(tx(vec![put(&format!("k{i}"), "v")])).await)?;
            ensure(rev > last, || format!("commit {i} got revision {rev}, not above {last}"))?;
            last = rev;
        }
        same("head() after them", fine("head", r.head().await)?, last)
    })
}

fn c3<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let only_new = |v: &str| Tx { conds: vec![Cond::Absent { family: F.into(), key: b"a".to_vec() }], writes: vec![put("a", v)] };
        fine("the first create", r.commit(only_new("first")).await)?;
        let e = refused("a second create", r.commit(only_new("second")).await)?;
        ensure(matches!(e, Refusal::Conflict { .. }), || format!("refused as {e:?}, not Conflict"))?;
        same("the value after the refused create", value(r, "a").await?, Some("first".into()))
    })
}

fn c4<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let first = fine("commit", r.commit(tx(vec![put("a", "one")])).await)?;
        let cas = |rev: Rev, v: &str| Tx { conds: vec![Cond::At { family: F.into(), key: b"a".to_vec(), rev }], writes: vec![put("a", v)] };
        let second = fine("a compare-and-set at the current revision", r.commit(cas(first, "two")).await)?;
        let e = refused("a compare-and-set at a stale revision", r.commit(cas(first, "three")).await)?;
        same("the refusal", e, Refusal::Conflict { family: F.into(), key: b"a".to_vec(), at: Some(second) })?;
        same("the value", value(r, "a").await?, Some("two".into()))
    })
}

fn c5<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        fine("commit", r.commit(tx(vec![put("taken", "x")])).await)?;
        let head = fine("head", r.head().await)?;
        let bad = Tx { conds: vec![Cond::Absent { family: F.into(), key: b"taken".to_vec() }], writes: vec![put("a", "1"), put("b", "2")] };
        refused("a transaction whose condition fails", r.commit(bad).await)?;
        same("a after it", value(r, "a").await?, None)?;
        same("b after it", value(r, "b").await?, None)?;
        same("the head after it", fine("head", r.head().await)?, head)?;
        let rev = fine("both together", r.commit(tx(vec![put("a", "1"), put("b", "2")])).await)?;
        let a = fine("get a", r.get(F, b"a", Consistency::Linearizable).await)?.map(|x| x.rev);
        let b = fine("get b", r.get(F, b"b", Consistency::Linearizable).await)?.map(|x| x.rev);
        same("both rows' revision", (a, b), (Some(rev), Some(rev)))
    })
}

fn c6<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        fine("commit", r.commit(tx(vec![put("a", "one")])).await)?;
        fine("delete", r.commit(tx(vec![Write::Delete { family: F.into(), key: b"a".to_vec() }])).await)?;
        same("a deleted row", value(r, "a").await?, None)?;
        same("a scan after the delete", keys(&fine("scan", r.scan(F, scan("", None, 10)).await)?), Vec::<String>::new())?;
        // Absent holds once it's deleted.
        let again = Tx { conds: vec![Cond::Absent { family: F.into(), key: b"a".to_vec() }], writes: vec![put("a", "again")] };
        fine("a create after the delete", r.commit(again).await).map(|_| ())
    })
}

fn c7<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        fine("commit", r.commit(tx(["b/2", "a/1", "b/1", "c/1", "b/3", "b/10"].iter().map(|k| put(k, "v")).collect())).await)?;
        let first = fine("scan", r.scan(F, scan("b/", None, 2)).await)?;
        same("the first page", (keys(&first), first.more), (vec!["b/1".to_owned(), "b/10".to_owned()], true))?;
        let second = fine("scan", r.scan(F, scan("b/", Some("b/10"), 2)).await)?;
        same("the second page", (keys(&second), second.more), (vec!["b/2".to_owned(), "b/3".to_owned()], false))?;
        let e = refused("a scan of 0 rows", r.scan(F, scan("", None, 0)).await)?;
        ensure(matches!(e, Refusal::Invalid { .. }), || format!("refused as {e:?}"))
    })
}

fn c8<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let w = |family: &str, v: &str| Write::Put { family: family.into(), key: b"k".to_vec(), value: v.as_bytes().to_vec() };
        fine("commit", r.commit(tx(vec![w("conformance-one", "1"), w("conformance-two", "2")])).await)?;
        let one = fine("get", r.get("conformance-one", b"k", Consistency::Linearizable).await)?.map(|x| x.value);
        let two = fine("get", r.get("conformance-two", b"k", Consistency::Linearizable).await)?.map(|x| x.value);
        same("one key in two families", (one, two), (Some(b"1".to_vec()), Some(b"2".to_vec())))
    })
}

fn c9<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let from = fine("head", r.head().await)?;
        let r1 = fine("commit", r.commit(tx(vec![put("a", "1")])).await)?;
        fine("another family", r.commit(Tx { conds: vec![], writes: vec![Write::Put { family: "conformance-other".into(), key: b"x".to_vec(), value: b"y".to_vec() }] }).await)?;
        let r2 = fine("commit", r.commit(tx(vec![put("a", "2"), put("b", "3")])).await)?;
        let r3 = fine("delete", r.commit(tx(vec![Write::Delete { family: F.into(), key: b"a".to_vec() }])).await)?;
        let feed = fine("changes", r.changes(F, from, 100).await)?;
        let want = vec![
            Change { rev: r1, key: b"a".to_vec(), value: Some(b"1".to_vec()) },
            Change { rev: r2, key: b"a".to_vec(), value: Some(b"2".to_vec()) },
            Change { rev: r2, key: b"b".to_vec(), value: Some(b"3".to_vec()) },
            Change { rev: r3, key: b"a".to_vec(), value: None },
        ];
        same("the family's changes", feed.changes, want)?;
        let rest = fine("changes after the second", r.changes(F, r2, 100).await)?;
        same("changes after a revision", rest.changes.iter().map(|c| c.rev).collect::<Vec<_>>(), vec![r3])?;
        let page = fine("changes, one at a time", r.changes(F, from, 1).await)?;
        same("a limited read", page.changes.len(), 1)
    })
}

fn c10<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let r1 = fine("commit", r.commit(tx(vec![put("a", "1")])).await)?;
        let before = r1.saturating_sub(1);
        same("a wait for a change already made", futures::FutureExt::now_or_never(r.wait(F, before)).map(|x| x.ok()), Some(Some(r1)))?;
        let mut waiting = r.wait(F, r1);
        ensure(futures::FutureExt::now_or_never(&mut waiting).is_none(), || "a wait with no newer change completed".into())?;
        let r2 = fine("commit", r.commit(tx(vec![put("a", "2")])).await)?;
        same("the wait, once a newer change is committed", waiting.await, Ok(r2))
    })
}

fn c11<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let r1 = fine("commit", r.commit(tx(vec![put("a", "old"), put("b", "kept")])).await)?;
        fine("commit", r.commit(tx(vec![put("a", "new"), Write::Delete { family: F.into(), key: b"b".to_vec() }, put("c", "later")])).await)?;
        let at = Consistency::Snapshot(r1);
        let a = fine("get at the snapshot", r.get(F, b"a", at).await)?.map(|x| x.value);
        same("a at the snapshot", a, Some(b"old".to_vec()))?;
        let page = fine("scan at the snapshot", r.scan(F, Scan { prefix: Vec::new(), after: None, limit: 10, at }).await)?;
        same("the keys at the snapshot", keys(&page), vec!["a".to_owned(), "b".to_owned()])?;
        let head = fine("head", r.head().await)?;
        let e = refused("a snapshot ahead of the head", r.get(F, b"a", Consistency::Snapshot(head.saturating_add(1))).await)?;
        ensure(matches!(e, Refusal::Ahead { .. }), || format!("refused as {e:?}, not Ahead"))
    })
}

fn c12<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let bad_family = Write::Put { family: "Not A Family!".into(), key: b"k".to_vec(), value: Vec::new() };
        ensure(matches!(r.commit(tx(vec![bad_family])).await, Err(Refusal::Invalid { .. })), || "a bad family name wasn't refused Invalid".into())?;
        let empty_key = Write::Put { family: F.into(), key: Vec::new(), value: Vec::new() };
        ensure(matches!(r.commit(tx(vec![empty_key])).await, Err(Refusal::Invalid { .. })), || "an empty key wasn't refused Invalid".into())?;
        let long_key = Write::Put { family: F.into(), key: vec![b'k'; KEY_MAX.saturating_add(1)], value: Vec::new() };
        ensure(matches!(r.commit(tx(vec![long_key])).await, Err(Refusal::TooLarge { .. })), || "a key over the limit wasn't refused TooLarge".into())?;
        let big = Write::Put { family: F.into(), key: b"k".to_vec(), value: vec![0; VALUE_MAX.saturating_add(1)] };
        ensure(matches!(r.commit(tx(vec![big])).await, Err(Refusal::TooLarge { .. })), || "a value over the limit wasn't refused TooLarge".into())?;
        let many = tx((0..=TX_MAX).map(|i| put(&format!("k{i}"), "v")).collect());
        ensure(matches!(r.commit(many).await, Err(Refusal::TooLarge { .. })), || "a transaction over the limit wasn't refused TooLarge".into())?;
        same("what the refused writes left", keys(&fine("scan", r.scan(F, scan("", None, 10)).await)?), Vec::<String>::new())
    })
}

fn c13<X: RecordsFixture>(f: &X) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let r = f.records();
        let head = fine("head", r.head().await)?;
        same("a transaction with no writes", fine("commit", r.commit(Tx::default()).await)?, head)?;
        let check = Tx { conds: vec![Cond::Absent { family: F.into(), key: b"a".to_vec() }], writes: Vec::new() };
        same("a check with no writes", fine("commit", r.commit(check).await)?, head)?;
        let rev = fine("one key written twice in a commit", r.commit(tx(vec![put("a", "first"), put("a", "last")])).await)?;
        same("the key written twice", fine("get", r.get(F, b"a", Consistency::Linearizable).await)?, Some(Row { key: b"a".to_vec(), value: b"last".to_vec(), rev }))
    })
}

/// The `Records` contract, clause by clause (docs/seams.md, "Records").
pub fn cases<X: RecordsFixture>() -> Vec<Case<X>> {
    vec![
        Case { id: "RECORDS-1", says: "a row written is read back with its value and its commit's revision; a key never written reads as none", check: c1::<X> },
        Case { id: "RECORDS-2", says: "every commit's revision is higher than every one before it, and head() is the newest", check: c2::<X> },
        Case { id: "RECORDS-3", says: "a write conditioned on a row's absence succeeds once; the next is refused Conflict and writes nothing", check: c3::<X> },
        Case { id: "RECORDS-4", says: "a write conditioned on a row's revision (compare-and-set) succeeds at the current one and is refused Conflict at a stale one, naming the current", check: c4::<X> },
        Case { id: "RECORDS-5", says: "a transaction is all or nothing: one failed condition and no write applies, the head unmoved; together, its writes share one revision", check: c5::<X> },
        Case { id: "RECORDS-6", says: "a deleted row reads as none, leaves scans, and satisfies an absence condition", check: c6::<X> },
        Case { id: "RECORDS-7", says: "a scan returns a family's rows in key order, within its prefix, after its cursor, at most its limit, saying when more follow; a limit of 0 is refused", check: c7::<X> },
        Case { id: "RECORDS-8", says: "families are apart: one key in two families is two rows", check: c8::<X> },
        Case { id: "RECORDS-9", says: "a family's change feed gives every change committed after a revision, in commit order, a delete as none, at most its limit", check: c9::<X> },
        Case { id: "RECORDS-10", says: "a wait completes once a change to the family is committed after its revision, at once when one already is", check: c10::<X> },
        Case { id: "RECORDS-11", says: "a snapshot read sees the store as it was just after its revision; one ahead of the head is refused Ahead", check: c11::<X> },
        Case { id: "RECORDS-12", says: "bad family names, empty or long keys, large values and large transactions are refused, typed, and write nothing", check: c12::<X> },
        Case { id: "RECORDS-13", says: "a transaction with no writes commits nothing and returns the head; a key written twice in one commit keeps the last", check: c13::<X> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_store_passes_the_records_suite() -> Result<(), String> {
        futures::executor::block_on(run("Records", &cases::<SimRecordsFixture>(), || async { SimRecordsFixture(SimRecords::new()) })).into_result().map(|_| ())
    }

    #[test]
    fn its_history_and_rows_are_bounded_and_say_so() {
        let r = SimRecords::with_limits(SimLimits { rows: 2, versions: 2, changes: 3 });
        let commit = |t: Tx| futures::executor::block_on(r.commit(t));
        let r1 = commit(tx(vec![put("a", "1")])).unwrap_or_default();
        for v in ["2", "3", "4"] {
            assert!(commit(tx(vec![put("a", v)])).is_ok());
        }
        // a's first versions are gone: a snapshot there is refused, not answered wrong.
        assert!(matches!(futures::executor::block_on(r.get(F, b"a", Consistency::Snapshot(r1))), Err(Refusal::Compacted { .. })));
        assert!(matches!(futures::executor::block_on(r.changes(F, 0, 10)), Err(Refusal::Compacted { .. })));
        assert!(commit(tx(vec![put("b", "1")])).is_ok());
        assert_eq!(commit(tx(vec![put("c", "1")])), Err(Refusal::Full { limit: 2 }));
        // A delete and a put in one commit stay within it.
        assert!(commit(tx(vec![Write::Delete { family: F.into(), key: b"b".to_vec() }, put("c", "1")])).is_ok());
    }
}
