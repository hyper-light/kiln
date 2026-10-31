//! U06: the kiln-seams Records boundary over the product's Amphora Namespace.
//! Validation, CAS, history, feeds, waits and compaction belong to Amphora. This adapter only
//! owns async scheduling and the conversion between the public seam and product value types.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use amphora::database::{Database, Namespace};
use amphora_api::{self as api, Records as _};
use futures::future::BoxFuture;
use kiln_seams::records::{Change, Changes, Cond, Consistency, Page, Records, Refusal, Rev, Row, Scan, Tx, Write};

use crate::store::Store;

pub(crate) mod legacy;
mod import;
mod session;
pub use session::{SessionBatchCommitter, SessionBatches, SessionNamespace, SessionStorage};
pub use session::SimSessionBatches;

/// Legacy import input only. New consumers share their host Database.
pub const FILE: &str = "records.db";
const CALL_WAIT: Duration = Duration::from_secs(30);

fn deadline() -> Instant { Instant::now() + CALL_WAIT }

/// U06: open through the host database, never create a database per feature.
/// Compatibility callers pass the old records path; the host store is its sibling kiln.db.
pub fn open(path: &Path) -> anyhow::Result<Arc<dyn Records>> {
    let host = path.parent().ok_or_else(|| anyhow::anyhow!("records path has no host directory"))?.join("kiln.db");
    let store = Store::open(&host)?;
    Ok(Arc::new(from_legacy_database(&store.database(), "host", path)?))
}

/// U06: complete or resume the retired host Records import before injecting its namespace.
/// Missing/changed input refuses an incomplete import; source files remain read-only.
pub fn import_host(database: &Database, legacy_path: &Path) -> anyhow::Result<()> {
    import_host_namespace(database, legacy_path, "host")
}

/// U06/U17: independent legacy instance stores retain independent revision spaces. Use the
/// same stable namespace for this import and `from_database`; a different source into an
/// occupied namespace is refused rather than merged or renumbered.
pub fn import_host_namespace(database: &Database, legacy_path: &Path, namespace: &str) -> anyhow::Result<()> {
    import::host(database, legacy_path, namespace)
}

/// One adapter for server and offline host consumers. Clones share the product namespace.
#[derive(Clone)]
pub struct StoreRecords { namespace: Namespace, namespace_name: Arc<str> }

pub fn from_database(database: &Database, namespace: &str) -> anyhow::Result<StoreRecords> {
    Ok(StoreRecords { namespace: database.namespace(namespace)?, namespace_name: namespace.into() })
}

/// U06: canonical host injection imports historical receipts before returning its adapter.
/// Independent legacy instances must use distinct stable namespaces with their original revisions.
pub fn from_legacy_database(database: &Database, namespace: &str, legacy_path: &Path) -> anyhow::Result<StoreRecords> {
    import_host_namespace(database, legacy_path, namespace)?;
    from_database(database, namespace)
}

impl StoreRecords {
    pub fn open(store: Store) -> anyhow::Result<Self> {
        let database = store.database();
        import::server(&database)?;
        from_database(&database, "kiln")
    }

    pub async fn compact(&self, before: Rev) -> anyhow::Result<()> {
        let namespace = self.namespace.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || namespace.compact(before, deadline())).await??;
        Ok(())
    }

    async fn call<R: Send + 'static>(&self, f: impl FnOnce(Namespace) -> Result<R, api::Refusal> + Send + 'static) -> Result<R, Refusal> {
        let namespace = self.namespace.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || f(namespace)).await.map_err(|e| Refusal::Unavailable(format!("{e:#}")))?.map_err(refusal)
    }
}

/// U05/U17: an immutable transaction bound by SessionStorage to this host actor and the
/// existing Records namespace. The token owns no channel sender or database controller.
#[derive(Clone)]
pub struct BoundRecordsTx {
    namespace: Arc<str>,
    transaction: api::Tx,
    actor: Arc<()>,
}
impl std::fmt::Debug for BoundRecordsTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundRecordsTx").field("namespace", &self.namespace)
            .field("conditions", &self.transaction.conds.len()).field("writes", &self.transaction.writes.len())
            .field("retained_bytes", &self.bytes().ok()).finish()
    }
}
impl PartialEq for BoundRecordsTx {
    fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.actor, &other.actor) && self.namespace == other.namespace && self.transaction == other.transaction }
}
impl Eq for BoundRecordsTx {}
impl BoundRecordsTx {
    fn new(namespace: Arc<str>, transaction: Tx, actor: Arc<()>) -> Self { Self { namespace, transaction: api_transaction(transaction), actor } }
    pub(crate) fn namespace(&self) -> &str { &self.namespace }
    pub(crate) fn transaction(&self) -> &api::Tx { &self.transaction }
    pub(crate) fn api_transaction(&self) -> api::Tx { self.transaction.clone() }
    pub(crate) fn belongs_to(&self, actor: &Arc<()>) -> bool { Arc::ptr_eq(&self.actor, actor) }
    pub(crate) fn bytes(&self) -> Result<usize, crate::convo::batch::AdmissionRefusal> {
        use crate::convo::batch::AdmissionRefusal;
        let invalid = |e: api::Refusal| match e {
            api::Refusal::TooLarge { size, limit, .. } => AdmissionRefusal::TooLarge { size, limit },
            other => AdmissionRefusal::Invalid(other.to_string()),
        };
        api::family(&self.namespace).map_err(invalid)?;
        api::transaction(&self.transaction).map_err(invalid)?;
        let overflow = || AdmissionRefusal::TooLarge { size: usize::MAX, limit: api::REQUEST_MAX };
        let mut bytes = 512usize.checked_add(self.namespace.len()).ok_or_else(overflow)?;
        bytes = bytes.checked_add(self.transaction.conds.capacity().checked_mul(std::mem::size_of::<api::Cond>()).ok_or_else(overflow)?).ok_or_else(overflow)?;
        bytes = bytes.checked_add(self.transaction.writes.capacity().checked_mul(std::mem::size_of::<api::Write>()).ok_or_else(overflow)?).ok_or_else(overflow)?;
        for c in &self.transaction.conds {
            let (api::Cond::Absent { family, key } | api::Cond::At { family, key, .. }) = c;
            bytes = bytes.checked_add(128).and_then(|n| n.checked_add(family.capacity())).and_then(|n| n.checked_add(key.capacity())).ok_or_else(overflow)?;
        }
        for w in &self.transaction.writes {
            let (api::Write::Put { family, key, .. } | api::Write::Delete { family, key }) = w;
            bytes = bytes.checked_add(128).and_then(|n| n.checked_add(family.capacity())).and_then(|n| n.checked_add(key.capacity())).ok_or_else(overflow)?;
            if let api::Write::Put { value, .. } = w { bytes = bytes.checked_add(value.capacity()).ok_or_else(overflow)?; }
        }
        Ok(bytes)
    }
}

fn api_transaction(tx: Tx) -> api::Tx {
    api::Tx {
        conds: tx.conds.into_iter().map(|c| match c { Cond::Absent { family, key } => api::Cond::Absent { family, key }, Cond::At { family, key, rev } => api::Cond::At { family, key, rev } }).collect(),
        writes: tx.writes.into_iter().map(|w| match w { Write::Put { family, key, value } => api::Write::Put { family, key, value }, Write::Delete { family, key } => api::Write::Delete { family, key } }).collect(),
    }
}

fn consistency(at: Consistency) -> api::Consistency {
    match at { Consistency::Linearizable => api::Consistency::Linearizable, Consistency::Snapshot(r) => api::Consistency::Snapshot(r) }
}
fn row(r: api::Row) -> Row { Row { key: r.key, value: r.value, rev: r.rev } }
fn refusal(e: api::Refusal) -> Refusal {
    match e {
        api::Refusal::Conflict { family, key, at } => Refusal::Conflict { family, key, at },
        api::Refusal::TooLarge { what, size, limit } => Refusal::TooLarge { what: label(&what), size, limit },
        api::Refusal::Invalid { what, why } => Refusal::Invalid { what: label(&what), why },
        api::Refusal::Full { limit } => Refusal::Full { limit },
        api::Refusal::Compacted { asked, horizon } => Refusal::Compacted { asked, horizon },
        api::Refusal::Ahead { asked, head } => Refusal::Ahead { asked, head },
        api::Refusal::Timeout => Refusal::Unavailable("Amphora deadline elapsed; a submitted transaction may still commit".into()),
        api::Refusal::Unavailable(why) => Refusal::Unavailable(why),
        // U06: the seam has no ambiguity variant; preserve the reconcile-before-retry rule.
        api::Refusal::UnknownOutcome => Refusal::Unavailable("Amphora commit has an unknown outcome; reconcile durable state before retrying".into()),
        api::Refusal::NotLeader { leader } => Refusal::Unavailable(format!("Amphora replica is not leader; known leader: {leader:?}")),
        api::Refusal::Fenced(why) => Refusal::Unavailable(format!("Amphora replica is fenced: {why}")),
    }
}
// The seam's diagnostic labels are static; retain the product diagnostic without leaking strings.
fn label(what: &str) -> &'static str {
    match what { "key" => "key", "value" => "value", "prefix" => "prefix", "limit" => "limit", "family or namespace" => "family or namespace", "conditions" => "conditions", "writes" => "writes", _ => "Amphora request" }
}

impl Records for StoreRecords {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
        let (family, key) = (family.to_owned(), key.to_vec());
        Box::pin(async move { self.call(move |n| n.get(&family, &key, consistency(at), deadline()).map(|r| r.map(row))).await })
    }
    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
        let family = family.to_owned();
        Box::pin(async move {
            self.call(move |n| n.scan(&family, api::Scan { prefix: scan.prefix, after: scan.after, limit: scan.limit, at: consistency(scan.at) }, deadline()).map(|p| Page { rows: p.rows.into_iter().map(row).collect(), more: p.more })).await
        })
    }
    fn commit(&self, tx: Tx) -> BoxFuture<'_, Result<Rev, Refusal>> {
        let tx = api_transaction(tx);
        Box::pin(async move { self.call(move |n| n.commit(tx, deadline())).await })
    }
    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
        let family = family.to_owned();
        Box::pin(async move { self.call(move |n| n.changes(&family, after, limit, deadline()).map(|c| Changes { head: c.head, changes: c.changes.into_iter().map(|c| Change { rev: c.rev, key: c.key, value: c.value }).collect() })).await })
    }
    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
        // U06: Amphora owns durable publication and cancellation-safe registrations. Dormant
        // inboxes consume no blocking-pool workers; satisfied waits resolve on the first poll.
        Box::pin(async move { self.namespace.watch(family, after).map_err(refusal)?.await.map_err(refusal) })
    }

    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
        Box::pin(async move { self.call(move |n| n.head(deadline())).await })
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
    fn u06_amphora_passes_the_records_suite() -> anyhow::Result<()> {
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
    fn u06_records_reopen_preserves_cas_and_compaction() -> anyhow::Result<()> {
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

}
