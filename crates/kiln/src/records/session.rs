//! U05/U17: application session batches share one host actor and its existing Database.
//! This is injected beside Records; it adds no SQL or conversation types to that seam.

use std::{path::{Path, PathBuf}, sync::Arc, time::Instant};
use futures::future::BoxFuture;

use amphora::database::Database;

use crate::convo::{batch::{AdmissionRefusal, Batch, BatchInput, PendingSubmission, Rejected, Ticket}, rows::Row, store::{Store, Upkeep}};
use super::{BoundRecordsTx, StoreRecords, Tx};

/// Construct once per host, then bind its independent historical Records namespaces below.
/// The constructor cannot pair an arbitrary Records handle with an unrelated conversation actor.
pub struct SessionStorage {
    database: Database,
    conversations: Store,
}
impl SessionStorage {
    pub fn from_host_database(database: Database, host: &str, legacy_conversations: &[PathBuf], upkeep: Upkeep) -> anyhow::Result<Self> {
        anyhow::ensure!(crate::convo::enabled(), "U05 session batch capability requires conversation storage");
        let conversations = crate::convo::from_host_sources(database.clone(), host, legacy_conversations, upkeep)?;
        Ok(Self { database, conversations })
    }

    pub fn conversations(&self) -> Store { self.conversations.clone() }

    /// Keep the exact configured/imported namespace. A session ID never selects a namespace.
    pub fn namespace(&self, namespace: &str, legacy_records: Option<&Path>) -> anyhow::Result<(StoreRecords, SessionBatchCommitter)> {
        let records = match legacy_records {
            Some(path) => super::from_legacy_database(&self.database, namespace, path)?,
            None => super::from_database(&self.database, namespace)?,
        };
        let committer = SessionBatchCommitter { namespace: Arc::clone(&records.namespace_name), store: self.conversations.clone() };
        Ok((records, committer))
    }

    /// U05: a sealed pair for a production host. It creates namespace handles only; the
    /// actor was created once by this factory and is retained by the bound capability.
    pub fn bind_namespace(&self, namespace: &str, legacy_records: Option<&Path>) -> anyhow::Result<SessionNamespace> {
        let (records, batches) = self.namespace(namespace, legacy_records)?;
        Ok(SessionNamespace { records: Arc::new(records), batches: Arc::new(batches) })
    }
}

/// U05: original Records namespace and its same-actor batch capability cannot be paired
/// independently by a consumer. Cloned handles retain that actor, not another factory.
pub struct SessionNamespace {
    records: Arc<StoreRecords>,
    batches: Arc<SessionBatchCommitter>,
}
impl SessionNamespace {
    pub fn records(&self) -> Arc<dyn kiln_seams::Records> { self.records.clone() }
    pub fn batches(&self) -> Arc<dyn SessionBatches> { self.batches.clone() }
}

/// A caller/lab receives this application capability alongside `Arc<dyn Records>`.
/// Sealing creates an owned envelope; admission alone never reports a durable commit.
pub trait SessionBatches: Send + Sync {
    fn batch(&self, session: String, rows: Vec<Row>, transaction: Tx) -> Result<Batch, Rejected<BatchInput>>;
    fn try_admit(&self, batch: Batch) -> Result<Ticket, Rejected<Batch>>;
    /// Nonblocking, bounded admission of the existing actor's FIFO settlement barrier.
    /// On dequeue it flushes preceding work without the projection batching delay; it cannot
    /// preempt a busy database call or fence other producers. Admission is not completion,
    /// a durability receipt or a latency bound. Await the ticket, and still check each effect's
    /// own ticket: a settled definite failure never becomes successful through this frontier.
    /// Full can persist because a definite failure still owns its input permit. Inspect effect
    /// tickets and take_failed before retrying; do not wait for this barrier to discover those
    /// failures, or a one-slot producer can wait forever for its own retained permit.
    fn barrier(&self) -> Result<Ticket, AdmissionRefusal>;
    /// Cancellation or expiry before admission leaves the original input in pending.
    /// On success its ticket is installed before the future returns, with no later await.
    fn admit_until<'a>(&'a self, pending: &'a mut PendingSubmission, until: Instant) -> BoxFuture<'a, Result<(), AdmissionRefusal>>;
}

/// Namespace-bound producer handle. Admitted transactions retain only its actor token, not
/// this sender, so failed-batch custody cannot keep a dropped actor channel open forever.
#[derive(Clone)]
pub struct SessionBatchCommitter {
    namespace: Arc<str>,
    store: Store,
}
impl SessionBatches for SessionBatchCommitter {
    fn batch(&self, session: String, rows: Vec<Row>, transaction: Tx) -> Result<Batch, Rejected<BatchInput>> {
        Batch::new(BatchInput { session, rows, records: Some(Box::new(BoundRecordsTx::new(Arc::clone(&self.namespace), transaction, self.store.identity()))) })
    }

    fn try_admit(&self, batch: Batch) -> Result<Ticket, Rejected<Batch>> {
        if !batch.records().is_some_and(|bound| bound.namespace() == self.namespace.as_ref() && bound.belongs_to(&self.store.identity())) {
            return Err(Rejected { input: batch, reason: AdmissionRefusal::Invalid("session batch belongs to a different namespace or host actor".into()) });
        }
        self.store.try_admit(batch)
    }
    fn barrier(&self) -> Result<Ticket, AdmissionRefusal> { self.store.barrier() }
    fn admit_until<'a>(&'a self, pending: &'a mut PendingSubmission, until: Instant) -> BoxFuture<'a, Result<(), AdmissionRefusal>> {
        Box::pin(async move {
            if let Some(batch) = pending.input()
                && !batch.records().is_some_and(|bound| bound.namespace() == self.namespace.as_ref() && bound.belongs_to(&self.store.identity()))
            {
                return Err(AdmissionRefusal::Invalid("session batch belongs to a different namespace or host actor".into()));
            }
            self.store.admit_until(pending, until).await
        })
    }
}

/// Explicit opt-in lab strategy using the same bounded Store actor, admission and tickets.
/// Its projection oracle is Records data, not a SQL engine or physical durability model.
pub struct SimSessionBatches {
    committer: SessionBatchCommitter,
    gate: Arc<SimGate>,
}
impl SimSessionBatches {
    /// Per-row retained capacity accepted by the JSON oracle, smaller than native staging.
    pub const ROW_MAX: usize = kiln_seams::records::VALUE_MAX / 8;
    pub fn new(records: kiln_seams::records::SimRecords) -> anyhow::Result<Self> {
        Self::with_limits(records, crate::convo::store::AdmissionLimits::default())
    }
    pub fn with_limits(records: kiln_seams::records::SimRecords, limits: crate::convo::store::AdmissionLimits) -> anyhow::Result<Self> {
        let gate: Arc<SimGate> = Arc::default();
        let engine = SimSessionEngine { records, gate: Arc::clone(&gate), revision: 0 };
        let store = Store::start_with_limits(Box::new(engine), Upkeep::default(), limits)?;
        Ok(Self { committer: SessionBatchCommitter { namespace: "simulated".into(), store }, gate })
    }
    /// Pause application on the existing actor. Its admitted payload/permit/ticket stay with
    /// that actor; bounded capacity and cancelled waits use exactly the production mechanism.
    pub fn hold_completion(&self, held: bool) { self.gate.hold(held); }
}
impl Drop for SimSessionBatches {
    fn drop(&mut self) { self.gate.hold(false); }
}
impl SessionBatches for SimSessionBatches {
    fn batch(&self, session: String, rows: Vec<Row>, transaction: Tx) -> Result<Batch, Rejected<BatchInput>> {
        self.committer.batch(session, rows, transaction)
    }
    fn try_admit(&self, batch: Batch) -> Result<Ticket, Rejected<Batch>> { self.committer.try_admit(batch) }
    fn barrier(&self) -> Result<Ticket, AdmissionRefusal> { self.committer.barrier() }
    fn admit_until<'a>(&'a self, pending: &'a mut PendingSubmission, until: Instant) -> BoxFuture<'a, Result<(), AdmissionRefusal>> {
        self.committer.admit_until(pending, until)
    }
}

#[derive(Default)]
struct SimGate { held: parking_lot::Mutex<bool>, changed: parking_lot::Condvar }
impl SimGate {
    fn hold(&self, held: bool) {
        *self.held.lock() = held;
        if !held { self.changed.notify_all(); }
    }
    fn wait(&self) {
        let mut held = self.held.lock();
        while *held { self.changed.wait(&mut held); }
    }
}

struct SimSessionEngine {
    records: kiln_seams::records::SimRecords,
    gate: Arc<SimGate>,
    revision: u64,
}
fn simulated_refusal(error: kiln_seams::records::Refusal) -> amphora_api::Refusal {
    use kiln_seams::records::Refusal;
    match error {
        Refusal::Conflict { family, key, at } => amphora_api::Refusal::Conflict { family, key, at },
        Refusal::TooLarge { what, size, limit } => amphora_api::Refusal::TooLarge { what: what.into(), size, limit },
        Refusal::Invalid { what, why } => amphora_api::Refusal::Invalid { what: what.into(), why },
        Refusal::Full { limit } => amphora_api::Refusal::Full { limit },
        Refusal::Compacted { asked, horizon } => amphora_api::Refusal::Compacted { asked, horizon },
        Refusal::Ahead { asked, head } => amphora_api::Refusal::Ahead { asked, head },
        Refusal::Unavailable(why) => amphora_api::Refusal::Unavailable(why),
    }
}
fn oracle_query<T>() -> anyhow::Result<T> { anyhow::bail!("session simulator models atomic input presence; SQL projection queries require the real adapter") }
impl crate::convo::store::Engine for SimSessionEngine {
    fn id(&self) -> String { "sim-session-oracle".into() }
    fn apply(&mut self, _: &[Row]) -> anyhow::Result<()> { oracle_query() }
    fn apply_batch(&mut self, batch: &Batch) -> Result<crate::convo::batch::Committed, crate::convo::batch::ApplyFailure> {
        use crate::convo::batch::{ApplyFailure, Committed};
        use kiln_seams::records::Records as _;
        self.gate.wait();
        let tx = simulated_transaction(batch).map_err(|e| ApplyFailure::Definite(amphora_api::invalid("simulated projection", &e.to_string())))?;
        // Concrete SimRecords refuses before changing any row; it has no I/O/rollback phase.
        // All projection oracle writes and journal/inbox conditions enter this one transaction.
        let rev = futures::executor::block_on(self.records.commit(tx)).map_err(|e| ApplyFailure::Definite(simulated_refusal(e)))?;
        self.revision = rev;
        Ok(Committed { conversation_rev: rev, records_rev: Some(rev) })
    }
    fn apply_group(&mut self, _: &[&Batch]) -> Result<Vec<crate::convo::batch::Committed>, crate::convo::batch::ApplyFailure> {
        Err(crate::convo::batch::ApplyFailure::Definite(amphora_api::invalid("simulated session group", "the fixture accepts only separate Records-bearing session envelopes")))
    }
    fn kept(&mut self, _: &str, _: usize) -> anyhow::Result<Option<crate::convo::store::Kept>> { oracle_query() }
    fn events(&mut self, _: &str, _: Option<u64>, _: usize) -> anyhow::Result<Vec<crate::convo::rows::EventRow>> { oracle_query() }
    fn search_some(&mut self, _: &crate::convo::store::Query, _: Option<crate::convo::store::SearchFrom>, _: usize, _: usize) -> anyhow::Result<(Vec<crate::convo::rows::EventRow>, Option<crate::convo::store::SearchFrom>)> { oracle_query() }
    fn changes(&mut self, _: u64, _: usize, _: bool) -> anyhow::Result<crate::convo::store::Changes> { oracle_query() }
    fn take(&mut self, _: &str, _: Option<&str>, _: u64, _: &crate::convo::store::Changes) -> anyhow::Result<crate::convo::store::Took> { oracle_query() }
    fn peer(&mut self, _: &str) -> anyhow::Result<u64> { oracle_query() }
    fn peers(&mut self) -> anyhow::Result<Vec<(String, u64, i64)>> { oracle_query() }
    fn marks(&mut self, _: &str) -> anyhow::Result<Vec<crate::convo::store::Mark>> { oracle_query() }
    fn record(&mut self, _: &str, _: &str, _: u64, _: usize) -> anyhow::Result<Vec<(u64, String)>> { oracle_query() }
    fn unended(&mut self, _: i64) -> anyhow::Result<Vec<crate::convo::store::Unended>> { oracle_query() }
    fn ended(&mut self, _: (i64, &str), _: usize) -> anyhow::Result<Vec<(String, i64)>> { oracle_query() }
    fn squeeze_some(&mut self, _: usize) -> anyhow::Result<bool> { Ok(false) }
    fn take_copy(&mut self, _: &str, _: u64, _: &crate::convo::store::Changes) -> anyhow::Result<crate::convo::store::Took> { oracle_query() }
    fn end_copy(&mut self) -> anyhow::Result<usize> { oracle_query() }
    fn is_empty(&mut self) -> anyhow::Result<bool> { Ok(self.revision == 0) }
    fn top(&mut self) -> u64 { self.revision }
    fn prune_some(&mut self, _: i64, _: usize) -> anyhow::Result<usize> { Ok(0) }
    fn compact_some(self: Box<Self>, _: u64, _: usize) -> anyhow::Result<(Box<dyn crate::convo::store::Engine>, crate::convo::store::Compacting)> { Ok((self, crate::convo::store::Compacting::Not)) }
    fn look(&mut self, _: bool) -> anyhow::Result<crate::convo::store::Look> { Ok(crate::convo::store::Look { top: self.revision, ..Default::default() }) }
}

fn simulated_transaction(batch: &Batch) -> Result<Tx, AdmissionRefusal> {
    use kiln_seams::records::{Cond, Write, VALUE_MAX};
    let Some(bound) = batch.records() else { return Err(AdmissionRefusal::Invalid("missing simulated Records transaction".into())); };
    let tx = bound.transaction();
    let mut transaction = Tx {
        conds: tx.conds.iter().map(|c| match c { amphora_api::Cond::Absent { family, key } => Cond::Absent { family: family.clone(), key: key.clone() }, amphora_api::Cond::At { family, key, rev } => Cond::At { family: family.clone(), key: key.clone(), rev: *rev } }).collect(),
        writes: tx.writes.iter().map(|w| match w { amphora_api::Write::Put { family, key, value } => Write::Put { family: family.clone(), key: key.clone(), value: value.clone() }, amphora_api::Write::Delete { family, key } => Write::Delete { family: family.clone(), key: key.clone() } }).collect(),
    };
    for row in batch.rows() {
        // This small oracle deliberately bounds encoding before allocation. Large transcript
        // staging/native SQL limits are tested in the real backend, not simulated by JSON.
        let size = row.retained_bytes().ok_or(AdmissionRefusal::TooLarge { size: usize::MAX, limit: VALUE_MAX })?;
        let bound = SimSessionBatches::ROW_MAX;
        if size > bound { return Err(AdmissionRefusal::TooLarge { size, limit: bound }); }
        let value = serde_json::to_vec(row).map_err(|e| AdmissionRefusal::Invalid(e.to_string()))?;
        if value.len() > VALUE_MAX { return Err(AdmissionRefusal::TooLarge { size: value.len(), limit: VALUE_MAX }); }
        let digest = crate::util::sha256_hex(&value);
        let key = format!("{}\0{digest}", batch.session()).into_bytes();
        transaction.writes.push(Write::Put { family: "sim-session-projection".into(), key, value });
    }
    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use amphora_api::Records as _;
    use crate::convo::{amphora::Amphora, batch::{ApplyFailure, Committed}, rows::RecordRow, store::Engine};
    use kiln_seams::records::{Cond, Consistency, Records as _, SimRecords, Write};

    use super::*;

    fn until() -> Instant { Instant::now() + Duration::from_secs(30) }
    fn state(session: &str, value: &str) -> Row {
        Row::Session { session: session.into(), info: None, meta: None, state: Some(value.into()), changes: None, epoch: Some(1), open: true, at: 1 }
    }
    fn transaction(expected: Option<u64>, value: &str) -> Tx {
        let condition = match expected {
            Some(rev) => Cond::At { family: "session-control".into(), key: b"scope".to_vec(), rev },
            None => Cond::Absent { family: "session-control".into(), key: b"scope".to_vec() },
        };
        Tx { conds: vec![condition], writes: vec![
            Write::Put { family: "session-control".into(), key: b"scope".to_vec(), value: value.as_bytes().to_vec() },
            Write::Put { family: "inbox".into(), key: format!("s/{value}").into_bytes(), value: value.as_bytes().to_vec() },
        ] }
    }
    fn envelope(namespace: &str, rows: Vec<Row>, tx: Tx) -> anyhow::Result<Batch> {
        Batch::new(BatchInput { session: "s".into(), rows, records: Some(Box::new(BoundRecordsTx::new(namespace.into(), tx, Arc::new(())))) })
            .map_err(|e| anyhow::anyhow!("batch refused: {:?}", e.reason))
    }

    #[test]
    fn u05_u17_session_cas_and_conversation_have_one_durable_result_and_reopen() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("host.db");
        let committed: Committed;
        {
            let database = Database::open(&path, until())?;
            let storage = SessionStorage::from_host_database(database.clone(), "authenticated-host", &[], Upkeep::default())?;
            let namespace = storage.bind_namespace("runner.original", None)?;
            let committer = namespace.batches();
            let batch = committer.batch("s".into(), vec![state("s", "first")], transaction(None, "first")).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
            let ticket = committer.try_admit(batch).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
            committed = futures::executor::block_on(ticket.wait()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            anyhow::ensure!(committed.records_rev == Some(1) && committed.conversation_rev > 0, "one real result");
            let namespace = database.namespace("runner.original")?;
            anyhow::ensure!(namespace.get("session-control", b"scope", amphora_api::Consistency::Linearizable, until())?.is_some_and(|r| Some(r.rev) == committed.records_rev), "snapshot revision");
            anyhow::ensure!(namespace.get("inbox", b"s/first", amphora_api::Consistency::Linearizable, until())?.is_some_and(|r| Some(r.rev) == committed.records_rev), "receipt revision");
            anyhow::ensure!(database.namespace("runner.other")?.get("session-control", b"scope", amphora_api::Consistency::Linearizable, until())?.is_none(), "namespace identity remains distinct");
            // Production Host accessor retains the same actor/namespace pair. A second actor
            // would reject this batch at Store::try_admit's original actor-token check.
            crate::rt::block_on(async move {
                let bound = storage.bind_namespace("runner.host-fixture", None)?;
                let records = bound.records();
                let (sink, _updates) = async_channel::bounded(1);
                let host = crate::harness::host::Host::try_with_session_namespace(sink, None, crate::harness::host::Limits::default(), crate::rt::clock(), bound, records.clone(), "factory-fixture")?;
                let batches = host.session_batches().ok_or_else(|| anyhow::anyhow!("host batch capability missing"))?;
                let batch = batches.batch("s".into(), Vec::new(), transaction(None, "host")).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
                let ticket = storage.conversations().try_admit(batch).map_err(|e| anyhow::anyhow!("same actor refused: {:?}", e.reason))?;
                let answer = ticket.wait().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
                let row = records.get("inbox", b"s/host", Consistency::Linearizable).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
                anyhow::ensure!(row.is_some_and(|row| Some(row.rev) == answer.records_rev), "Host accessor changed Records namespace");
                Ok(())
            })?;
        }
        // Product ownership can outlive the last handle while its existing worker drains.
        // Reopen through Database's existing ownership protocol; no application lease workaround.
        let database = Database::open(&path, until())?;
        let mut engine = Amphora::from_database(database.clone())?;
        anyhow::ensure!(engine.kept("s", 0)?.and_then(|s| s.state).as_deref() == Some("first"), "projection reopened");
        anyhow::ensure!(engine.top() == committed.conversation_rev, "exact conversation revision reopened");
        anyhow::ensure!(database.namespace("runner.original")?.head(until())? == 1, "Records head reopened");
        Ok(())
    }

    #[test]
    fn u05_u17_stale_cas_and_post_sql_failure_publish_neither_side() -> anyhow::Result<()> {
        let database = Database::in_memory(until())?;
        let mut engine = Amphora::from_database(database.clone())?;
        let first = engine.apply_batch(&envelope("runner.original", vec![state("s", "first")], transaction(None, "first"))?).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let stale = envelope("runner.original", vec![state("s", "stale")], transaction(None, "stale"))?;
        anyhow::ensure!(matches!(engine.apply_batch(&stale), Err(ApplyFailure::Definite(amphora_api::Refusal::Conflict { .. }))), "known CAS refusal stays typed");
        anyhow::ensure!(engine.kept("s", 0)?.and_then(|s| s.state).as_deref() == Some("first"), "stale CAS applied no SQL");
        let mut marker = vec![3];
        marker.extend_from_slice(&1u64.to_le_bytes());
        marker.extend_from_slice(&[9; 32]);
        let missing_content = Row::Record(Box::new(RecordRow { session: "s".into(), source: "transcript".into(), n: 0, end: 1, line: String::new(), kept: Some(marker) }));
        let later = envelope("runner.original", vec![state("s", "must roll back"), missing_content], transaction(first.records_rev, "later"))?;
        // Records writes and the first SQL state update execute, then the missing ready
        // manifest fails a later SQL query. The error must escape the outer call.
        anyhow::ensure!(matches!(engine.apply_batch(&later), Err(ApplyFailure::Unknown(_))), "no false success or unjustified definite rollback proof");
        anyhow::ensure!(engine.kept("s", 0)?.and_then(|s| s.state).as_deref() == Some("first"), "earlier SQL rolled back");
        anyhow::ensure!(engine.top() == first.conversation_rev, "failed revision not cached");
        let namespace = database.namespace("runner.original")?;
        anyhow::ensure!(namespace.head(until())? == 1, "failed Records transaction rolled back");
        anyhow::ensure!(namespace.get("inbox", b"s/later", amphora_api::Consistency::Linearizable, until())?.is_none(), "failed receipt absent");
        Ok(())
    }

    #[test]
    fn u05_projection_groups_report_each_exact_writer_revision() -> anyhow::Result<()> {
        let database = Database::in_memory(until())?;
        let mut engine = Amphora::from_database(database)?;
        let a = Batch::projection("a".into(), vec![state("a", "a")]).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        let b = Batch::projection("b".into(), vec![state("b", "b")]).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        let answers = engine.apply_group(&[&a, &b]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let [a, b] = answers.as_slice() else { anyhow::bail!("one answer per envelope") };
        anyhow::ensure!(a.records_rev.is_none() && b.records_rev.is_none() && a.conversation_rev < b.conversation_rev && b.conversation_rev == engine.top(), "results came from each writer position");
        Ok(())
    }

    #[test]
    fn u05_simulated_session_projection_uses_the_same_records_cas_transaction() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let records = SimRecords::default();
            let committer = SimSessionBatches::with_limits(records.clone(), crate::convo::store::AdmissionLimits { batches: 1, bytes: 4 << 20 })?;
            committer.hold_completion(true);
            let first = committer.batch("s".into(), vec![state("s", "first")], transaction(None, "first")).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
            let first = committer.try_admit(first).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
            anyhow::ensure!(first.try_result().is_none(), "admitted does not mean completed");
            anyhow::ensure!(matches!(committer.barrier(), Err(AdmissionRefusal::Full { .. })), "barrier shares the retained actor capacity while the effect is held");
            let stale = committer.batch("s".into(), vec![state("s", "stale")], transaction(None, "stale")).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
            let mut pending = PendingSubmission::new(stale);
            {
                let mut wait = committer.admit_until(&mut pending, until());
                let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
                anyhow::ensure!(std::future::Future::poll(wait.as_mut(), &mut context).is_pending(), "in-flight capacity waits without taking input");
            }
            anyhow::ensure!(pending.input().is_some_and(|b| b.rows() == [state("s", "stale")]) && pending.ticket().is_none(), "cancelled wait retained exact input");
            committer.hold_completion(false);
            let first = first.wait().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let revision = first.records_rev;
            committer.admit_until(&mut pending, until()).await?;
            let stale = pending.ticket().ok_or_else(|| anyhow::anyhow!("admitted ticket missing"))?;
            anyhow::ensure!(matches!(stale.wait().await, Err(error) if matches!(error.as_ref(), ApplyFailure::Definite(amphora_api::Refusal::Conflict { .. }))), "one failed CAS");
            anyhow::ensure!(matches!(committer.barrier(), Err(AdmissionRefusal::Full { .. })), "a definite failure retains quota until its input is taken; a barrier-first consumer cannot make room");
            let rejected = stale.take_failed().ok_or_else(|| anyhow::anyhow!("failed input was not retained"))?;
            anyhow::ensure!(rejected.rows() == [state("s", "stale")], "failed input returned intact");
            let barrier = committer.barrier().map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let frontier = barrier.wait().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            anyhow::ensure!(frontier.records_rev.is_none() && frontier.conversation_rev == first.conversation_rev, "barrier settles the existing writer frontier without a new effect");
            anyhow::ensure!(matches!(stale.try_result(), Some(Err(error)) if matches!(error.as_ref(), ApplyFailure::Definite(amphora_api::Refusal::Conflict { .. }))), "settled frontier does not turn the failed effect into success");
            let value = serde_json::to_vec(&state("s", "first"))?;
            let key = format!("s\0{}", crate::util::sha256_hex(&value));
            let kept = records.get("sim-session-projection", key.as_bytes(), Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("projection missing"))?;
            anyhow::ensure!(Some(kept.rev) == revision && kept.value == value, "oracle and command had one outcome");
            anyhow::ensure!(records.get("inbox", b"s/stale", Consistency::Linearizable).await?.is_none(), "no refused receipt");
            Ok(())
        })
    }
}
