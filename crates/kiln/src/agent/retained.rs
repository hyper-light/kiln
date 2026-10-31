//! U03: retained evidence outbox in the runner's existing shared Amphora Records.
//! Completion receipts and process lifetimes never retire this independent evidence.

use std::sync::Arc;
use kiln_seams::records::{Cond, Consistency, Records, Refusal, Scan, Tx, Write};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

const FAMILY: &str = "retained-events";
const PAGE: usize = 128;
const META: &str = "retained-meta";
const COUNT: &[u8] = b"count";
const LIMIT: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Receipt { pub job: String, pub event: serde_json::Value }
impl Receipt {
    pub(crate) fn of(job: &str, json: &str) -> anyhow::Result<Option<Self>> {
        anyhow::ensure!(job.len() <= 256 && json.len() <= 65536, "retained evidence exceeds its budget");
        let mut event: serde_json::Value = serde_json::from_str(json)?;
        let Some(object) = event.as_object_mut() else { return Ok(None) };
        if !matches!(object.get("t").and_then(serde_json::Value::as_str), Some("kept_owned" | "kept_released")) { return Ok(None) }
        anyhow::ensure!(object.get("attempt").and_then(serde_json::Value::as_u64).is_some_and(|v| v > 0 && u32::try_from(v).is_ok()), "retained evidence requires its exact attempt");
        anyhow::ensure!(object.get("execution").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty() && s.len() <= 256), "retained evidence requires its exact execution");
        // Preserve parent binding, event phase and all optional fields; only the transient
        // normal job journal sequence is excluded from durable identity.
        object.remove("seq");
        Ok(Some(Self { job: job.to_owned(), event }))
    }
    pub(crate) fn executor(json: &str) -> anyhow::Result<Option<Self>> {
        anyhow::ensure!(json.len() <= 65536, "executor evidence exceeds its budget");
        let said: crate::runner::pool::Said = serde_json::from_str(json)?;
        if said.state != "stopped" { return Ok(None); }
        anyhow::ensure!(!said.executor.is_empty() && said.executor.len() <= 256
            && [&said.generation, &said.allocation, &said.physical_execution].into_iter().all(|v| v.as_ref().is_some_and(|s| !s.is_empty() && s.len() <= 256)), "stopped executor evidence lacks its exact startup owner");
        Ok(Some(Self { job: format!("executor:{}", said.executor), event: serde_json::to_value(said)? }))
    }

    fn bytes(&self) -> anyhow::Result<Vec<u8>> { Ok(serde_json::to_vec(self)?) }
    fn key(&self) -> anyhow::Result<Vec<u8>> { Ok(hex::encode(sha2::Sha256::digest(self.bytes()?)).into_bytes()) }
    pub(crate) fn message(&self) -> anyhow::Result<crate::proto::AgentMsg> {
        let json = serde_json::to_string(&self.event)?;
        if self.event.get("state").and_then(serde_json::Value::as_str) == Some("stopped") {
            let checked = Self::executor(&json)?.ok_or_else(|| anyhow::anyhow!("invalid executor outbox event"))?;
            anyhow::ensure!(checked.job == self.job, "executor outbox identity mismatch");
            Ok(crate::proto::AgentMsg::Executor { json })
        } else {
            Ok(crate::proto::AgentMsg::JobEvent { job: self.job.clone(), json })
        }
    }
}

fn stopped_receipt(proof: &crate::runner::processes::StoppedExecution) -> anyhow::Result<Option<Receipt>> {
    let attempt = proof.attempt.ok_or_else(|| anyhow::anyhow!("stopped execution attempt is unknown"))?;
    match proof.was_retained {
        Some(false) => Ok(None),
        Some(true) => {
            let json = serde_json::json!({ "t": "kept_released", "attempt": attempt, "execution": proof.execution, "parent": proof.parent }).to_string();
            Ok(Some(Receipt::of(&proof.job, &json)?.ok_or_else(|| anyhow::anyhow!("stopped execution lacks retained identity"))?))
        }
        None => anyhow::bail!("stopped execution retention history is unknown"),
    }
}

fn stopped_executor_receipt(proof: &crate::runner::processes::StoppedExecution) -> anyhow::Result<Option<Receipt>> {
    let Some(owner) = &proof.executor_owner else { return Ok(None) };
    anyhow::ensure!(proof.was_retained.is_some(), "stopped executor retention history is unknown");
    let said = crate::runner::pool::Said {
        executor: owner.executor.clone(), state: "stopped".into(), error: None,
        generation: Some(owner.generation.clone()), allocation: Some(owner.allocation.clone()),
        physical_execution: Some(proof.execution.clone()), shape: None, process_nonce: None,
        // This input came only from the existing durable closed/stopped Gate after every
        // physical group proved retirement. It says nothing about producer process death.
        stop_proof: Some(crate::runner::pool::StopProof::PhysicalRecovery),
    };
    Receipt::executor(&serde_json::to_string(&said)?)
}

pub(crate) struct Spool { admission: crate::runner::processes::Admission, records: Arc<dyn Records>, after: Option<Vec<u8>>, import_after: Option<String> }
impl Spool {
    pub(crate) fn new_in(records: Arc<dyn Records>, admission: crate::runner::processes::Admission) -> Self { Self { admission, records, after: None, import_after: None } }

    pub(crate) async fn publish(&self, receipt: &Receipt) -> anyhow::Result<()> {
        let (key, value) = (receipt.key()?, receipt.bytes()?);
        for _ in 0..3 {
            if let Some(row) = self.records.get(FAMILY, &key, Consistency::Linearizable).await? {
                anyhow::ensure!(row.value == value, "retained evidence identity conflicts");
                return Ok(());
            }
            let counter = self.records.get(META, COUNT, Consistency::Linearizable).await?;
            let count: usize = counter.as_ref().map(|row| serde_json::from_slice(&row.value)).transpose()?.unwrap_or(0);
            anyhow::ensure!(count < LIMIT, "retained evidence spool is full; execution evidence remains fenced");
            let condition = counter.map_or(Cond::Absent { family: META.into(), key: COUNT.to_vec() }, |row| Cond::At { family: META.into(), key: COUNT.to_vec(), rev: row.rev });
            match self.records.commit(Tx { conds: vec![Cond::Absent { family: FAMILY.into(), key: key.clone() }, condition], writes: vec![Write::Put { family: FAMILY.into(), key: key.clone(), value: value.clone() }, Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&count.saturating_add(1))? }] }).await {
                Ok(_) => return Ok(()),
                Err(Refusal::Conflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("retained evidence publication is contended; retry without acknowledging")
    }

    /// Transfer only this runner workdir's proven physical evidence into canonical Records.
    /// A crash at either boundary replays the same key; physical proof is retired only after
    /// the outbox commit has succeeded, and missing historical attempts remain quarantined.
    pub(crate) async fn import_stopped(&mut self, work: &std::path::Path) -> anyhow::Result<()> {
        let work = std::path::absolute(work)?;
        let admission = self.admission.clone();
        let proofs = crate::rt::blocking(move || crate::runner::processes::recovery_evidence(&admission)).await??;
        self.import_page(work, proofs).await
    }

    async fn import_page(&mut self, work: std::path::PathBuf, mut proofs: Vec<crate::runner::processes::StoppedExecution>) -> anyhow::Result<()> {
        proofs.retain(|proof| proof.work == work && (proof.attempt.is_some() || proof.executor_owner.is_some()) && proof.was_retained.is_some());
        proofs.sort_by(|a, b| a.execution.cmp(&b.execution));
        let mut page: Vec<_> = proofs.into_iter().filter(|proof| self.import_after.as_ref().is_none_or(|after| &proof.execution > after)).take(PAGE + 1).collect();
        let more = page.len() > PAGE;
        page.truncate(PAGE);
        let next = if more { page.last().map(|proof| proof.execution.clone()) } else { None };
        for proof in page {
            if let Some(receipt) = stopped_executor_receipt(&proof)? {
                self.publish(&receipt).await?;
            }
            if proof.attempt.is_some() {
                if let Some(receipt) = stopped_receipt(&proof)? { self.publish(&receipt).await?; }
            } else {
                anyhow::ensure!(proof.was_retained == Some(false), "retained execution cannot lose its job attempt");
            }
            // Exact startup roots need no job attempt, but their stopped executor evidence
            // must be durable first. An assigned retained root can require both receipts.
            // A live executor retains the same Gate permit until its ordered barrier is
            // durably published. Physical proof is already in this canonical outbox; a
            // pending cleanup must not starve other imported proofs in the current page.
            let admission = self.admission.clone();
            let _retired = crate::rt::blocking(move || crate::runner::processes::try_acknowledge_evidence(&admission, &proof)).await??;
        }
        // Advance only after this page committed; a held live Gate stays in the inventory
        // but cannot hide later proofs on every sample. Restart begins a safe deduplicated
        // scan again. This transient cursor belongs to the same existing outbox owner.
        self.import_after = next;
        Ok(())
    }

    /// A bounded page per sample; advance even when an old peer cannot acknowledge a row.
    pub(crate) async fn pending(&mut self) -> anyhow::Result<Vec<Receipt>> {
        let page = self.records.scan(FAMILY, Scan { prefix: vec![], after: self.after.clone(), limit: PAGE, at: Consistency::Linearizable }).await?;
        self.after = if page.more { page.rows.last().map(|row| row.key.clone()) } else { None };
        page.rows.into_iter().map(|row| serde_json::from_slice(&row.value).map_err(anyhow::Error::from)).collect()
    }

    /// Only the authenticated server's exact successful durable-stage receipt calls this.
    pub(crate) async fn received(&self, receipt: &Receipt) -> anyhow::Result<()> {
        let key = receipt.key()?;
        let Some(row) = self.records.get(FAMILY, &key, Consistency::Linearizable).await? else { return Ok(()) };
        anyhow::ensure!(row.value == receipt.bytes()?, "retained receipt identity conflicts");
        let counter = self.records.get(META, COUNT, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("retained spool count is missing"))?;
        let count: usize = serde_json::from_slice(&counter.value)?;
        anyhow::ensure!(count > 0, "retained spool count is inconsistent");
        self.records.commit(Tx { conds: vec![Cond::At { family: FAMILY.into(), key: key.clone(), rev: row.rev }, Cond::At { family: META.into(), key: COUNT.to_vec(), rev: counter.rev }], writes: vec![Write::Delete { family: FAMILY.into(), key }, Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&count.saturating_sub(1))? }] }).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u03_unassigned_executor_proof_uses_exact_startup_owner_without_job_attempt() -> anyhow::Result<()> {
        let mut proof = crate::runner::processes::StoppedExecution {
            execution: "physical".into(), job: "executor:warm".into(), attempt: None,
            work: std::path::PathBuf::from("work"), parent: None, was_retained: Some(false),
            executor_owner: Some(crate::runner::processes::ExecutorOwner { executor: "warm".into(), generation: "gen".into(), allocation: "grant".into() }),
        };
        let receipt = stopped_executor_receipt(&proof)?.ok_or_else(|| anyhow::anyhow!("startup proof lost executor owner"))?;
        assert!(matches!(receipt.message()?, crate::proto::AgentMsg::Executor { .. }));
        assert!(stopped_receipt(&proof).is_err(), "startup proof must never become a job release");
        proof.executor_owner = None;
        assert!(stopped_executor_receipt(&proof)?.is_none(), "old unknown startup roots cannot invent a grant");
        Ok(())
    }

    #[test]
    fn u03_executor_stopped_spool_survives_restart_and_exact_receipt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let admission = crate::runner::processes::Admission::private(&tempfile::tempdir()?.keep())?;
            let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::new());
            let receipt = Receipt::executor(r#"{"executor":"warm","state":"stopped","generation":"gen","allocation":"grant","physical_execution":"physical"}"#)?.ok_or_else(|| anyhow::anyhow!("missing stopped executor fixture"))?;
            Spool::new_in(Arc::clone(&records), admission.clone()).publish(&receipt).await?;
            let mut restarted = Spool::new_in(records, admission.clone());
            assert_eq!(restarted.pending().await?, vec![receipt.clone()]);
            assert!(matches!(receipt.message()?, crate::proto::AgentMsg::Executor { .. }));
            let mut stale = receipt.clone();
            let generation = stale.event.get_mut("generation").ok_or_else(|| anyhow::anyhow!("missing generation"))?;
            *generation = "previous".into();
            restarted.received(&stale).await?;
            assert_eq!(restarted.pending().await?, vec![receipt.clone()]);
            restarted.publish(&receipt).await?;
            restarted.received(&receipt).await?;
            assert!(restarted.pending().await?.is_empty());
            let work = std::path::absolute("retained-page-fixture")?;
            // Repeating the SAME canonical inventory models Gate proofs whose live owner
            // retains their permit after publication. Physical authenticity is exercised by
            // the adopted-retained Gate case; this case observes canonical outbox delivery.
            let proofs: Vec<_> = (0..=PAGE).map(|index| crate::runner::processes::StoppedExecution {
                execution: format!("ci-page-fixture-{index:04}"), job: format!("executor:page-fixture-{index:04}"), attempt: None, work: work.clone(), parent: None, was_retained: Some(false),
                executor_owner: Some(crate::runner::processes::ExecutorOwner { executor: format!("page-fixture-{index:04}"), generation: "page-generation".into(), allocation: "page-grant".into() }),
            }).collect();
            restarted.import_page(work.clone(), proofs.clone()).await?;
            restarted.import_page(work, proofs).await?;
            let first_page = restarted.pending().await?;
            let second_page = restarted.pending().await?;
            assert_eq!(first_page.len() + second_page.len(), PAGE + 1, "held first-page proofs cannot starve the next canonical outbox page");
            let mut keys = std::collections::BTreeSet::new();
            for receipt in first_page.into_iter().chain(second_page) { keys.insert(receipt.key()?); }
            assert_eq!(keys.len(), PAGE + 1, "replayed inventory keeps one canonical acceptance per proof");
            Ok(())
        })
    }

    #[test]
    fn u03_only_affirmative_retained_proof_emits_release() -> anyhow::Result<()> {
        let mut proof = crate::runner::processes::StoppedExecution { execution: "exec".into(), job: "leg".into(), attempt: Some(2), work: std::path::PathBuf::from("work"), parent: None, was_retained: Some(false), executor_owner: None };
        assert!(stopped_receipt(&proof)?.is_none(), "ordinary cleanup never emits KeptReleased");
        proof.was_retained = Some(true);
        assert!(stopped_receipt(&proof)?.is_some());
        proof.was_retained = None;
        assert!(stopped_receipt(&proof).is_err(), "missing history is not ordinary cleanup proof");
        proof.was_retained = Some(true);
        proof.attempt = None;
        assert!(stopped_receipt(&proof).is_err(), "unknown attempt stays quarantined");
        Ok(())
    }

    #[test]
    fn u03_retained_crash_retry_receipt_oracle_keeps_exact_execution_and_phase() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let admission = crate::runner::processes::Admission::private(&tempfile::tempdir()?.keep())?;
            let records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::new());
            let owned = Receipt::of("leg", r#"{"t":"kept_owned","execution":"exec","attempt":2,"until":10,"parent":{"generation":"p1"}}"#)?.ok_or_else(|| anyhow::anyhow!("missing owned fixture"))?;
            let released = Receipt::of("leg", r#"{"t":"kept_released","execution":"exec","attempt":2,"parent":{"generation":"p1"}}"#)?.ok_or_else(|| anyhow::anyhow!("missing release fixture"))?;
            let spool = Spool::new_in(Arc::clone(&records), admission.clone());
            spool.publish(&owned).await?;
            spool.publish(&released).await?;
            drop(spool); // crash after commit, before send or ACK
            let mut restarted = Spool::new_in(records, admission.clone());
            assert_eq!(restarted.pending().await?.len(), 2);
            restarted.publish(&released).await?; // lost ACK, retransmission
            let mut wrong_parent = released.clone();
            let generation = wrong_parent.event.get_mut("parent").and_then(|parent| parent.get_mut("generation")).ok_or_else(|| anyhow::anyhow!("missing parent generation"))?;
            *generation = "p2".into();
            restarted.received(&wrong_parent).await?;
            assert_eq!(restarted.pending().await?.len(), 2);
            restarted.received(&owned).await?;
            assert_eq!(restarted.pending().await?, vec![released.clone()], "Owned ACK never retires Released");
            restarted.received(&released).await?;
            assert!(restarted.pending().await?.is_empty());
            restarted.received(&released).await?; // duplicate authenticated ACK
            Ok(())
        })
    }
}
