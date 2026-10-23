//! Records (docs/seams.md, "Records") kept in kiln's store: the seam's durable implementation until
//! Amphora's (CP-T1.5) takes its place, as `SimRecords` is the one in memory. Every version of a
//! row is kept by family, key and the commit that wrote it, so a snapshot read and the change feed
//! come from one table; [`StoreRecords::compact`] forgets the history before a revision, as a
//! compaction does. Only what the seam offers is here (families, keys, values and conditions), so
//! CP-T1.5's copy, family by family through the seam, carries every row as it is.
//!
//! kiln's store runs one call at a time, so commits are linearizable as they are. Beside the
//! tables, what a wait needs is kept in memory too (the head, the feed's horizon, each family's
//! newest change), so a wait for a change already made answers at once.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::future::BoxFuture;
use kiln_seams::records::{Change, Changes, Cond, Consistency, FAMILY_MAX, KEY_MAX, Page, Records, Refusal, Rev, Row, SCAN_MAX, Scan, TX_MAX, Tx, VALUE_MAX, Write};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};

use crate::store::Store;

/// What's kept in memory beside the tables.
#[derive(Default)]
struct Mem {
    head: Rev,
    /// Snapshots from before it are refused.
    horizon: Rev,
    /// The feed is whole only after it: a read of changes, or a wait, from before is refused.
    log_horizon: Rev,
    /// Each family's newest change.
    newest: HashMap<String, Rev>,
}

struct Inner {
    mem: Mutex<Mem>,
    changed: event_listener::Event,
}

/// Records in kiln's store. Clones share one store.
#[derive(Clone)]
pub struct StoreRecords {
    store: Store,
    inner: Arc<Inner>,
}

fn family_ok(family: &str) -> Result<(), Refusal> {
    let ok = !family.is_empty() && family.len() <= FAMILY_MAX && family.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'));
    if ok { Ok(()) } else { Err(Refusal::Invalid { what: "a family name", why: format!("{family:?}: 1 to {FAMILY_MAX} of a-z, 0-9, _ - .") }) }
}

fn key_ok(key: &[u8]) -> Result<(), Refusal> {
    if key.is_empty() {
        return Err(Refusal::Invalid { what: "a key", why: "it's empty".into() });
    }
    if key.len() > KEY_MAX {
        return Err(Refusal::TooLarge { what: "a key", size: key.len(), limit: KEY_MAX });
    }
    Ok(())
}

fn limit_ok(limit: usize) -> Result<usize, Refusal> {
    if limit == 0 { Err(Refusal::Invalid { what: "a limit", why: "it's 0".into() }) } else { Ok(limit.min(SCAN_MAX)) }
}

fn unavailable(e: anyhow::Error) -> Refusal {
    Refusal::Unavailable(format!("{e:#}"))
}

/// A revision as SQLite keeps it, and back.
fn sql(r: Rev) -> i64 {
    i64::try_from(r).unwrap_or(i64::MAX)
}

fn rev(r: i64) -> Rev {
    u64::try_from(r).unwrap_or(0)
}

/// The smallest key after every key that starts with `prefix`; none when each of its bytes is 0xff.
fn past(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.pop() {
        if last < u8::MAX {
            p.push(last.saturating_add(1));
            return Some(p);
        }
    }
    None
}

/// The newest revision a read at `at` sees, or why it's refused.
fn reads_at(m: &Mem, at: Consistency) -> Result<i64, Refusal> {
    match at {
        Consistency::Linearizable => Ok(i64::MAX),
        Consistency::Snapshot(r) if r > m.head => Err(Refusal::Ahead { asked: r, head: m.head }),
        Consistency::Snapshot(r) if r < m.horizon => Err(Refusal::Compacted { asked: r, horizon: m.horizon }),
        Consistency::Snapshot(r) => Ok(sql(r)),
    }
}

impl StoreRecords {
    /// Records in `store` (its migrations make their tables), with what's in memory read from them:
    /// at once, as a server starts.
    pub fn open(store: Store) -> anyhow::Result<Self> {
        let mem = store
            .now(|c| {
                let (head, horizon, log_horizon): (i64, i64, i64) = c.query_row("SELECT head, horizon, log_horizon FROM records_meta WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                let mut s = c.prepare("SELECT family, newest FROM records_families")?;
                let newest = s.query_map([], |r| Ok((r.get::<_, String>(0)?, rev(r.get::<_, i64>(1)?))))?.collect::<Result<HashMap<_, _>, _>>()?;
                Ok(Mem { head: rev(head), horizon: rev(horizon), log_horizon: rev(log_horizon), newest })
            })?;
        Ok(Self { store, inner: Arc::new(Inner { mem: Mutex::new(mem), changed: event_listener::Event::new() }) })
    }

    /// Forgets the history from before `before` (at most the head), as a compaction does: each key
    /// keeps the version a snapshot at `before` reads and those after it, a key deleted by then
    /// goes, and the feed starts there. Snapshots and the feed from `before` on read as they did;
    /// from before it they're refused `Compacted`.
    pub async fn compact(&self, before: Rev) -> anyhow::Result<()> {
        let inner = Arc::clone(&self.inner);
        self.store
            .call(move |c| {
                let t = crate::store::tx(c)?;
                let mut m = inner.mem.lock();
                let before = sql(before.min(m.head));
                t.execute(
                    "DELETE FROM records_versions WHERE rev < (SELECT MAX(w.rev) FROM records_versions w WHERE w.family = records_versions.family AND w.key = records_versions.key AND w.rev <= ?1)",
                    [before],
                )?;
                t.execute(
                    "DELETE FROM records_versions WHERE value IS NULL AND rev <= ?1 AND NOT EXISTS (SELECT 1 FROM records_versions w WHERE w.family = records_versions.family AND w.key = records_versions.key AND w.rev > records_versions.rev)",
                    [before],
                )?;
                let horizon = m.horizon.max(rev(before));
                let log_horizon = m.log_horizon.max(rev(before).saturating_sub(1));
                t.execute("UPDATE records_meta SET horizon = ?1, log_horizon = ?2 WHERE id = 1", params![sql(horizon), sql(log_horizon)])?;
                t.commit()?;
                (m.horizon, m.log_horizon) = (horizon, log_horizon);
                Ok(())
            })
            .await?;
        // Waits from before it are refused now, not left waiting.
        self.inner.changed.notify(usize::MAX);
        Ok(())
    }
}

impl Records for StoreRecords {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
        Box::pin(async move {
            family_ok(family)?;
            key_ok(key)?;
            let (inner, family, key) = (Arc::clone(&self.inner), family.to_owned(), key.to_vec());
            self.store
                .call(move |c| {
                    let at = match reads_at(&inner.mem.lock(), at) {
                        Ok(at) => at,
                        Err(refused) => return Ok(Err(refused)),
                    };
                    let found: Option<(Option<Vec<u8>>, i64)> = c
                        .query_row("SELECT value, rev FROM records_versions WHERE family = ?1 AND key = ?2 AND rev <= ?3 ORDER BY rev DESC LIMIT 1", params![family, key, at], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    Ok(Ok(found.and_then(|(value, r)| value.map(|value| Row { key, value, rev: rev(r) }))))
                })
                .await
                .map_err(unavailable)?
        })
    }

    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
        Box::pin(async move {
            family_ok(family)?;
            let limit = limit_ok(scan.limit)?;
            let (inner, family) = (Arc::clone(&self.inner), family.to_owned());
            self.store
                .call(move |c| {
                    let at = match reads_at(&inner.mem.lock(), scan.at) {
                        Ok(at) => at,
                        Err(refused) => return Ok(Err(refused)),
                    };
                    let mut s = c.prepare_cached(
                        "SELECT v.key, v.value, v.rev FROM records_versions v
                         WHERE v.family = ?1 AND v.key >= ?2 AND (?3 IS NULL OR v.key < ?3) AND (?4 IS NULL OR v.key > ?4)
                           AND v.rev = (SELECT MAX(w.rev) FROM records_versions w WHERE w.family = v.family AND w.key = v.key AND w.rev <= ?5)
                           AND v.value IS NOT NULL
                         ORDER BY v.key LIMIT ?6",
                    )?;
                    let mut rows = s
                        .query_map(params![family, scan.prefix, past(&scan.prefix), scan.after, at, i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)], |r| {
                            Ok(Row { key: r.get(0)?, value: r.get(1)?, rev: rev(r.get(2)?) })
                        })?
                        .collect::<Result<Vec<Row>, _>>()?;
                    let more = rows.len() > limit;
                    rows.truncate(limit);
                    Ok(Ok(Page { rows, more }))
                })
                .await
                .map_err(unavailable)?
        })
    }

    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            // What doesn't fit is refused before anything is read or written.
            if tx.conds.len() > TX_MAX {
                return Err(Refusal::TooLarge { what: "a transaction's conditions", size: tx.conds.len(), limit: TX_MAX });
            }
            if tx.writes.len() > TX_MAX {
                return Err(Refusal::TooLarge { what: "a transaction's writes", size: tx.writes.len(), limit: TX_MAX });
            }
            for c in &tx.conds {
                let (Cond::Absent { family, key } | Cond::At { family, key, .. }) = c;
                family_ok(family)?;
                key_ok(key)?;
            }
            for w in &tx.writes {
                let (Write::Put { family, key, .. } | Write::Delete { family, key }) = w;
                family_ok(family)?;
                key_ok(key)?;
                if let Write::Put { value, .. } = w
                    && value.len() > VALUE_MAX
                {
                    return Err(Refusal::TooLarge { what: "a value", size: value.len(), limit: VALUE_MAX });
                }
            }
            let inner = Arc::clone(&self.inner);
            let out = self
                .store
                .call(move |c| {
                    let t = crate::store::tx(c)?;
                    for cond in &tx.conds {
                        let (Cond::Absent { family, key } | Cond::At { family, key, .. }) = cond;
                        let newest: Option<(i64, bool)> = t
                            .query_row("SELECT rev, value IS NOT NULL FROM records_versions WHERE family = ?1 AND key = ?2 ORDER BY rev DESC LIMIT 1", params![family, key], |r| Ok((r.get(0)?, r.get(1)?)))
                            .optional()?;
                        let live = newest.filter(|(_, live)| *live).map(|(r, _)| rev(r));
                        let holds = match cond {
                            Cond::Absent { .. } => live.is_none(),
                            Cond::At { rev: at, .. } => live == Some(*at),
                        };
                        if !holds {
                            return Ok(Err(Refusal::Conflict { family: family.clone(), key: key.clone(), at: live }));
                        }
                    }
                    let mut m = inner.mem.lock();
                    if tx.writes.is_empty() {
                        return Ok(Ok(m.head));
                    }
                    let at = m.head.saturating_add(1);
                    let mut families = BTreeSet::new();
                    for (seq, w) in tx.writes.iter().enumerate() {
                        let (family, key, value) = match w {
                            Write::Put { family, key, value } => (family, key, Some(value)),
                            Write::Delete { family, key } => (family, key, None),
                        };
                        // A key written twice in one commit keeps the last.
                        t.execute(
                            "INSERT OR REPLACE INTO records_versions (family, key, rev, seq, value) VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![family, key, sql(at), i64::try_from(seq).unwrap_or(i64::MAX), value],
                        )?;
                        families.insert(family.clone());
                    }
                    for f in &families {
                        t.execute("INSERT INTO records_families (family, newest) VALUES (?1, ?2) ON CONFLICT (family) DO UPDATE SET newest = excluded.newest", params![f, sql(at)])?;
                    }
                    t.execute("UPDATE records_meta SET head = ?1 WHERE id = 1", [sql(at)])?;
                    t.commit()?;
                    m.head = at;
                    for f in families {
                        m.newest.insert(f, at);
                    }
                    Ok(Ok(at))
                })
                .await
                .map_err(unavailable)?;
            if out.is_ok() {
                self.inner.changed.notify(usize::MAX);
            }
            out
        })
    }

    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
        Box::pin(async move {
            family_ok(family)?;
            let limit = limit_ok(limit)?;
            let (inner, family) = (Arc::clone(&self.inner), family.to_owned());
            self.store
                .call(move |c| {
                    let head = {
                        let m = inner.mem.lock();
                        if after < m.log_horizon {
                            return Ok(Err(Refusal::Compacted { asked: after, horizon: m.log_horizon }));
                        }
                        m.head
                    };
                    let mut s = c.prepare_cached("SELECT rev, key, value FROM records_versions WHERE family = ?1 AND rev > ?2 ORDER BY rev, seq LIMIT ?3")?;
                    let changes = s
                        .query_map(params![family, sql(after), i64::try_from(limit).unwrap_or(i64::MAX)], |r| Ok(Change { rev: rev(r.get(0)?), key: r.get(1)?, value: r.get(2)? }))?
                        .collect::<Result<Vec<Change>, _>>()?;
                    Ok(Ok(Changes { changes, head }))
                })
                .await
                .map_err(unavailable)?
        })
    }

    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            family_ok(family)?;
            loop {
                let listener = self.inner.changed.listen();
                {
                    let m = self.inner.mem.lock();
                    // The feed is whole only after its horizon: a change before it may have gone.
                    if after < m.log_horizon {
                        return Err(Refusal::Compacted { asked: after, horizon: m.log_horizon });
                    }
                    if let Some(r) = m.newest.get(family).copied().filter(|r| *r > after) {
                        return Ok(r);
                    }
                }
                listener.await;
            }
        })
    }

    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
        let head = self.inner.mem.lock().head;
        Box::pin(async move { Ok(head) })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use kiln_seams::records::{RecordsFixture, cases};

    use super::*;

    struct Fixture(StoreRecords);

    impl RecordsFixture for Fixture {
        fn records(&self) -> &dyn Records {
            &self.0
        }

        fn compact(&self, before: Rev) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                if let Err(e) = self.0.compact(before).await {
                    tracing::warn!(error = %format!("{e:#}"), "records: a compaction failed");
                }
            })
        }
    }

    /// kiln's own Records passes the seam's suite, every clause (docs/seams.md, "Records").
    #[test]
    fn kiln_s_store_passes_the_records_suite() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let cases = cases::<Fixture>();
            // A store of its own for each case, opened first.
            let mut stores = VecDeque::new();
            for _ in 0..cases.len() {
                stores.push_back(StoreRecords::open(Store::in_memory()?)?);
            }
            let last = stores.back().cloned().ok_or_else(|| anyhow::anyhow!("no cases"))?;
            let make = || {
                let r = stores.pop_front().unwrap_or_else(|| last.clone());
                async move { Fixture(r) }
            };
            let clock = crate::rt::clock();
            let n = kiln_seams::conformance::run_within("Records (kiln's store)", &cases, make, || clock.sleep(Duration::from_secs(30))).await.into_result().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(n == cases.len(), "{n} of {} clauses held", cases.len());
            Ok(())
        })
    }

    /// What's written outlives the store's process: opened again, its rows, head, horizons and
    /// each family's newest change are as they were.
    #[test]
    fn records_outlive_the_process_that_wrote_them() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("kiln.db");
            let put = |k: &str, v: &str| Write::Put { family: "deploy.app".into(), key: k.as_bytes().to_vec(), value: v.as_bytes().to_vec() };
            let (r1, r2) = {
                let r = StoreRecords::open(Store::open(&path)?)?;
                let r1 = r.commit(Tx { conds: vec![], writes: vec![put("web", "1")] }).await?;
                let r2 = r.commit(Tx { conds: vec![], writes: vec![put("web", "2"), put("api", "1")] }).await?;
                r.compact(r2).await?;
                (r1, r2)
            };
            let r = StoreRecords::open(Store::open(&path)?)?;
            anyhow::ensure!(r.head().await? == r2, "the head as it was");
            let web = r.get("deploy.app", b"web", Consistency::Linearizable).await?;
            anyhow::ensure!(web == Some(Row { key: b"web".to_vec(), value: b"2".to_vec(), rev: r2 }), "web as it was: {web:?}");
            // The compaction's horizons are kept: a snapshot from before it is still refused.
            anyhow::ensure!(matches!(r.get("deploy.app", b"web", Consistency::Snapshot(r1)).await, Err(Refusal::Compacted { .. })));
            // A wait for a change it already has answers at once, read from what's kept.
            let at_once = futures::FutureExt::now_or_never(r.wait("deploy.app", r2.saturating_sub(1)));
            anyhow::ensure!(at_once == Some(Ok(r2)), "the family's newest change: {at_once:?}");
            Ok(())
        })
    }

    #[test]
    fn a_prefix_scan_ends_where_the_prefix_does() {
        assert_eq!(past(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(past(&[b'a', 0xff]), Some(b"b".to_vec()));
        assert_eq!(past(&[0xff, 0xff]), None);
        assert_eq!(past(b""), None);
    }
}
