//! U03: retained evidence outbox in the runner's existing shared Amphora Records.
//! Completion receipts and process lifetimes never retire this independent evidence.

pub(crate) mod logs;

use std::sync::Arc;
use kiln_seams::records::{Cond, Consistency, Records, Refusal, Scan, Tx, Write};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use kiln_ci::wire::{OrdinaryFrontierAckV1, OrdinaryOwnerV1, OrdinaryPhaseV1, OrdinaryPublicationV1};

const FAMILY: &str = "retained-events";
const PAGE: usize = 128;
const META: &str = "retained-meta";
const COUNT: &[u8] = b"count";
const LIMIT: usize = 4096;
const ORDINARY: &str = "retained-ordinary";
const DOMAIN: &[u8] = b"ordinary-domain-v1";
const ANCHOR: &[u8] = b"ordinary-anchor-v1";
const ISSUED: &[u8] = b"ordinary-issued-v1/";

/// U01/U03: refusal preserves the original Gate, publication and capacity charge.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OrdinaryRefusal {
    #[error("PublicationEpochMissing: ordinary publication metadata requires its original domain")]
    EpochMissing,
    #[error("PublicationEpochMismatch: ordinary publication changed its original domain")]
    EpochMismatch,
    #[error("PublicationCounterRegression: ordinary publication metadata is inconsistent")]
    CounterRegression,
    #[error("PublicationCounterExhausted: ordinary publication requires an explicit drain")]
    CounterExhausted,
    #[error("PublicationFrontierOutOfRange: acknowledgement exceeds this producer's issued sequence")]
    FrontierOutOfRange,
    #[error("ordinary publication spool is full; original evidence remains fenced")]
    Full,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrdinaryDomain { version: u8, epoch: String, issued: u64, acknowledged: u64 }
impl OrdinaryDomain {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1 && valid_epoch(&self.epoch), OrdinaryRefusal::EpochMismatch);
        anyhow::ensure!(self.acknowledged <= self.issued, OrdinaryRefusal::CounterRegression);
        Ok(())
    }
}
fn valid_epoch(epoch: &str) -> bool { !epoch.is_empty() && epoch.len() <= 128 && epoch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') }
fn ordinary_key(seq: u64) -> Vec<u8> { seq.to_be_bytes().to_vec() }
fn issue_key(owner: &OrdinaryOwnerV1) -> Vec<u8> {
    // One grant can have one immutable positive disposition. A changed owner or a
    // Stopped/NoStart contradiction must conflict instead of allocating another sequence.
    let mut key = ISSUED.to_vec();
    key.extend_from_slice(hex::encode(sha2::Sha256::digest(owner.grant.as_bytes())).as_bytes());
    key
}

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

pub(crate) struct Spool { admission: crate::runner::processes::Admission, records: Arc<dyn Records>, after: Option<Vec<u8>>, import_after: Option<String>, ordinary_after: Option<Vec<u8>> }
impl Spool {
    pub(crate) fn new_in(records: Arc<dyn Records>, admission: crate::runner::processes::Admission) -> Self { Self { admission, records, after: None, import_after: None, ordinary_after: None } }

    /// Called by the existing runner owner BEFORE readiness, registration or ordinary
    /// admission. `pristine` must come from its authoritative Gate/incoming-domain check;
    /// an empty delivery queue alone never authorizes a new publication epoch.
    pub(crate) async fn initialize_ordinary(&self, epoch: &str, pristine: bool) -> anyhow::Result<String> {
        anyhow::ensure!(valid_epoch(epoch), OrdinaryRefusal::EpochMismatch);
        for _ in 0..3 {
            if self.records.get(META, DOMAIN, Consistency::Linearizable).await?.is_some() { return self.ordinary_domain().await; }
            anyhow::ensure!(pristine, OrdinaryRefusal::EpochMissing);
            anyhow::ensure!(self.records.get(META, ANCHOR, Consistency::Linearizable).await?.is_none(), OrdinaryRefusal::EpochMissing);
            for (family, prefix) in [(ORDINARY, Vec::new()), (META, ISSUED.to_vec())] {
                anyhow::ensure!(self.records.scan(family, Scan { prefix, after: None, limit: 1, at: Consistency::Linearizable }).await?.rows.is_empty(), OrdinaryRefusal::EpochMissing);
            }
            let counter = self.records.get(META, COUNT, Consistency::Linearizable).await?;
            if counter.is_none() {
                anyhow::ensure!(self.records.scan(FAMILY, Scan { prefix: vec![], after: None, limit: 1, at: Consistency::Linearizable }).await?.rows.is_empty(), OrdinaryRefusal::CounterRegression);
            }
            let count: usize = counter.as_ref().map(|row| serde_json::from_slice(&row.value)).transpose()?.unwrap_or(0);
            anyhow::ensure!(count <= LIMIT, OrdinaryRefusal::CounterRegression);
            let domain = OrdinaryDomain { version: 1, epoch: epoch.to_owned(), issued: 0, acknowledged: 0 };
            let condition = counter.as_ref().map_or(Cond::Absent { family: META.into(), key: COUNT.to_vec() }, |row| Cond::At { family: META.into(), key: COUNT.to_vec(), rev: row.rev });
            match self.records.commit(Tx { conds: vec![Cond::Absent { family: META.into(), key: DOMAIN.to_vec() }, Cond::Absent { family: META.into(), key: ANCHOR.to_vec() }, condition], writes: vec![
                Write::Put { family: META.into(), key: DOMAIN.to_vec(), value: serde_json::to_vec(&domain)? },
                Write::Put { family: META.into(), key: ANCHOR.to_vec(), value: serde_json::to_vec(epoch)? },
                Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&count)? },
            ] }).await {
                Ok(_) => return Ok(epoch.to_owned()),
                Err(Refusal::Conflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("ordinary publication initialization is contended; admission remains fenced")
    }

    async fn domain_row(&self) -> anyhow::Result<(kiln_seams::records::Row, OrdinaryDomain, Consistency)> {
        let at = Consistency::Snapshot(self.records.head().await?);
        let row = self.records.get(META, DOMAIN, at).await?.ok_or(OrdinaryRefusal::EpochMissing)?;
        let domain: OrdinaryDomain = serde_json::from_slice(&row.value).map_err(|_| OrdinaryRefusal::EpochMissing)?;
        domain.validate()?;
        let anchor = self.records.get(META, ANCHOR, at).await?.ok_or(OrdinaryRefusal::EpochMissing)?;
        let epoch: String = serde_json::from_slice(&anchor.value).map_err(|_| OrdinaryRefusal::EpochMismatch)?;
        anyhow::ensure!(epoch == domain.epoch, OrdinaryRefusal::EpochMismatch);
        Ok((row, domain, at))
    }

    pub(crate) async fn ordinary_domain(&self) -> anyhow::Result<String> { Ok(self.domain_row().await?.1.epoch) }

    /// The caller must hold the existing exact ordinary Gate's exclusive evidence permit.
    /// Unknown commits are retried by that same owner; a later lookup returns the immutable
    /// publication even if its delivery ACK already removed the pending row.
    pub(crate) async fn publish_ordinary(&self, owner: &OrdinaryOwnerV1, phase: OrdinaryPhaseV1) -> anyhow::Result<OrdinaryPublicationV1> {
        owner.validate().map_err(anyhow::Error::msg)?;
        let index = issue_key(owner);
        for _ in 0..3 {
            let (row, mut domain, at) = self.domain_row().await?;
            anyhow::ensure!(owner.publication_epoch == domain.epoch, OrdinaryRefusal::EpochMismatch);
            if let Some(issued) = self.records.get(META, &index, at).await? {
                let publication: OrdinaryPublicationV1 = serde_json::from_slice(&issued.value)?;
                publication.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(publication.owner == *owner && publication.phase == phase && publication.epoch == domain.epoch && publication.seq <= domain.issued, OrdinaryRefusal::CounterRegression);
                return Ok(publication);
            }
            let counter = self.records.get(META, COUNT, at).await?.ok_or(OrdinaryRefusal::CounterRegression)?;
            let count: usize = serde_json::from_slice(&counter.value)?;
            anyhow::ensure!(count <= LIMIT, OrdinaryRefusal::CounterRegression);
            anyhow::ensure!(count < LIMIT && domain.issued - domain.acknowledged < LIMIT as u64, OrdinaryRefusal::Full);
            let seq = domain.issued.checked_add(1).ok_or(OrdinaryRefusal::CounterExhausted)?;
            let publication = OrdinaryPublicationV1 { version: 1, epoch: domain.epoch.clone(), seq, owner: owner.clone(), phase, recovery: true };
            publication.validate().map_err(anyhow::Error::msg)?;
            let value = serde_json::to_vec(&publication)?;
            anyhow::ensure!(value.len() <= 65536, "ordinary publication exceeds its budget");
            domain.issued = seq;
            match self.records.commit(Tx { conds: vec![
                Cond::At { family: META.into(), key: DOMAIN.to_vec(), rev: row.rev }, Cond::At { family: META.into(), key: COUNT.to_vec(), rev: counter.rev },
                Cond::Absent { family: META.into(), key: index.clone() }, Cond::Absent { family: ORDINARY.into(), key: ordinary_key(seq) },
            ], writes: vec![
                Write::Put { family: META.into(), key: DOMAIN.to_vec(), value: serde_json::to_vec(&domain)? },
                Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&(count + 1))? },
                Write::Put { family: META.into(), key: index.clone(), value: value.clone() },
                Write::Put { family: ORDINARY.into(), key: ordinary_key(seq), value },
            ] }).await {
                Ok(_) => return Ok(publication),
                Err(Refusal::Conflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("ordinary publication is contended; retain the original Gate")
    }

    /// Only AFTER the same exclusive source owner retired its Gate with incoming replay
    /// closure may its issued index be forgotten. A crash before this call holds a bounded
    /// slot for explicit source-closure reconciliation; absence alone is never stop proof.
    pub(crate) async fn forget_ordinary_issue(&self, owner: &OrdinaryOwnerV1) -> anyhow::Result<()> {
        owner.validate().map_err(anyhow::Error::msg)?;
        let index = issue_key(owner);
        for _ in 0..3 {
            let (row, domain, at) = self.domain_row().await?;
            anyhow::ensure!(owner.publication_epoch == domain.epoch, OrdinaryRefusal::EpochMismatch);
            let Some(issued) = self.records.get(META, &index, at).await? else { return Ok(()) };
            let publication: OrdinaryPublicationV1 = serde_json::from_slice(&issued.value)?;
            publication.validate().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(publication.owner == *owner && publication.epoch == domain.epoch && publication.seq <= domain.issued, OrdinaryRefusal::CounterRegression);
            let pending = self.records.get(ORDINARY, &ordinary_key(publication.seq), at).await?;
            let counter = self.records.get(META, COUNT, at).await?.ok_or(OrdinaryRefusal::CounterRegression)?;
            let count: usize = serde_json::from_slice(&counter.value)?;
            anyhow::ensure!(count > 0 && count <= LIMIT, OrdinaryRefusal::CounterRegression);
            let mut conds = vec![Cond::At { family: META.into(), key: DOMAIN.to_vec(), rev: row.rev }, Cond::At { family: META.into(), key: index.clone(), rev: issued.rev }, Cond::At { family: META.into(), key: COUNT.to_vec(), rev: counter.rev }];
            let mut writes = vec![Write::Delete { family: META.into(), key: index.clone() }];
            if let Some(pending) = pending {
                anyhow::ensure!(pending.value == issued.value, OrdinaryRefusal::CounterRegression);
                conds.push(Cond::At { family: ORDINARY.into(), key: pending.key, rev: pending.rev });
            } else {
                anyhow::ensure!(publication.seq <= domain.acknowledged, OrdinaryRefusal::CounterRegression);
                conds.push(Cond::Absent { family: ORDINARY.into(), key: ordinary_key(publication.seq) });
                writes.push(Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&(count - 1))? });
            }
            match self.records.commit(Tx { conds, writes }).await {
                Ok(_) => return Ok(()), Err(Refusal::Conflict { .. }) => continue, Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("ordinary source transfer is contended; retain its issued slot")
    }

    /// An authenticated frontier never acknowledges another epoch or a not-yet-issued
    /// sequence. Old ACKs are idempotent; prefix holes stay charged until the server closes
    /// them positively. Cleanup is bounded and resumes on the existing delivery sample.
    pub(crate) async fn received_ordinary(&self, ack: &OrdinaryFrontierAckV1) -> anyhow::Result<()> {
        ack.validate().map_err(anyhow::Error::msg)?;
        for _ in 0..3 {
            let (row, mut domain, _) = self.domain_row().await?;
            anyhow::ensure!(ack.epoch == domain.epoch, OrdinaryRefusal::EpochMismatch);
            anyhow::ensure!(ack.through <= domain.issued, OrdinaryRefusal::FrontierOutOfRange);
            if ack.through <= domain.acknowledged { return self.cleanup_ordinary().await; }
            domain.acknowledged = ack.through;
            match self.records.commit(Tx { conds: vec![Cond::At { family: META.into(), key: DOMAIN.to_vec(), rev: row.rev }], writes: vec![Write::Put { family: META.into(), key: DOMAIN.to_vec(), value: serde_json::to_vec(&domain)? }] }).await {
                Ok(_) => return self.cleanup_ordinary().await,
                Err(Refusal::Conflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("ordinary acknowledgement is contended; pending evidence is retained")
    }

    async fn cleanup_ordinary(&self) -> anyhow::Result<()> {
        for _ in 0..3 {
            let (domain_row, domain, at) = self.domain_row().await?;
            let page = self.records.scan(ORDINARY, Scan { prefix: vec![], after: None, limit: PAGE, at }).await?;
            let counter = self.records.get(META, COUNT, at).await?.ok_or(OrdinaryRefusal::CounterRegression)?;
            let mut count: usize = serde_json::from_slice(&counter.value)?;
            anyhow::ensure!(count <= LIMIT, OrdinaryRefusal::CounterRegression);
            let mut conds = vec![Cond::At { family: META.into(), key: DOMAIN.to_vec(), rev: domain_row.rev }, Cond::At { family: META.into(), key: COUNT.to_vec(), rev: counter.rev }];
            let mut writes = Vec::new();
            for row in page.rows {
                let publication: OrdinaryPublicationV1 = serde_json::from_slice(&row.value)?;
                publication.validate().map_err(anyhow::Error::msg)?;
                anyhow::ensure!(row.key == ordinary_key(publication.seq) && publication.epoch == domain.epoch && publication.seq <= domain.issued, OrdinaryRefusal::CounterRegression);
                if publication.seq > domain.acknowledged { break; }
                let index = issue_key(&publication.owner);
                match self.records.get(META, &index, at).await? {
                    Some(issued) => {
                        anyhow::ensure!(issued.value == row.value, OrdinaryRefusal::CounterRegression);
                        conds.push(Cond::At { family: META.into(), key: index, rev: issued.rev });
                    }
                    None => {
                        conds.push(Cond::Absent { family: META.into(), key: index });
                        count = count.checked_sub(1).ok_or(OrdinaryRefusal::CounterRegression)?;
                    }
                }
                conds.push(Cond::At { family: ORDINARY.into(), key: row.key.clone(), rev: row.rev });
                writes.push(Write::Delete { family: ORDINARY.into(), key: row.key });
            }
            if writes.is_empty() { return Ok(()); }
            writes.push(Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&count)? });
            match self.records.commit(Tx { conds, writes }).await {
                Ok(_) => return Ok(()), Err(Refusal::Conflict { .. }) => continue, Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("ordinary acknowledgement cleanup is contended; pending rows remain")
    }

    pub(crate) async fn pending_ordinary(&mut self) -> anyhow::Result<Vec<OrdinaryPublicationV1>> {
        self.cleanup_ordinary().await?;
        let (_, domain, at) = self.domain_row().await?;
        let page = self.records.scan(ORDINARY, Scan { prefix: vec![], after: self.ordinary_after.clone().or_else(|| Some(ordinary_key(domain.acknowledged))), limit: PAGE, at }).await?;
        self.ordinary_after = if page.more { page.rows.last().map(|row| row.key.clone()) } else { None };
        page.rows.into_iter().filter(|row| row.key > ordinary_key(domain.acknowledged)).map(|row| {
            let publication: OrdinaryPublicationV1 = serde_json::from_slice(&row.value)?;
            publication.validate().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(row.key == ordinary_key(publication.seq) && publication.epoch == domain.epoch && publication.seq <= domain.issued, OrdinaryRefusal::CounterRegression);
            Ok(publication)
        }).collect()
    }

    pub(crate) async fn publish(&self, receipt: &Receipt) -> anyhow::Result<()> {
        let (key, value) = (receipt.key()?, receipt.bytes()?);
        for _ in 0..3 {
            if let Some(row) = self.records.get(FAMILY, &key, Consistency::Linearizable).await? {
                anyhow::ensure!(row.value == value, "retained evidence identity conflicts");
                return Ok(());
            }
            let counter = self.records.get(META, COUNT, Consistency::Linearizable).await?;
            if counter.is_none() {
                anyhow::ensure!(self.records.get(META, DOMAIN, Consistency::Linearizable).await?.is_none()
                    && self.records.get(META, ANCHOR, Consistency::Linearizable).await?.is_none(), OrdinaryRefusal::CounterRegression);
            }
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
            // New ordinary evidence has a strict Gate phase. Claim and revalidate the
            // SAME physical lock before awaiting publication, so another importer cannot
            // replay a stale inventory after its issued sequence has been compacted.
            let ordinary = if proof.executor_owner.is_some() {
                // The new slice is unpooled native work only. Preserve the existing
                // executor/ParentBinding evidence path and its independent ownership.
                crate::runner::processes::OrdinaryClaim::Legacy
            } else {
                let admission = self.admission.clone();
                let inventoried = proof.clone();
                crate::rt::blocking(move || crate::runner::processes::claim_ordinary_evidence(&admission, &inventoried)).await??
            };
            if matches!(&ordinary, crate::runner::processes::OrdinaryClaim::Pending) { continue; }
            if let crate::runner::processes::OrdinaryClaim::Claimed(ordinary) = ordinary {
                let owner = ordinary.owner().clone();
                // Recovery can find a positive Gate before the original receiver's
                // acceptance commit. Close its exact incoming stamp through that SAME
                // Records owner while holding the Gate permit, then persist the latch.
                // Prefix closure never supplies physical stop proof; the claimed Gate did.
                if let Err(error) = crate::wire::keep::close_ordinary_incoming(&*self.records, &owner.process_nonce, &owner.connection, &owner.command.stream, owner.command.seq).await {
                    // Gate inventory is ordered by execution identity, not incoming seq.
                    // A later proof must not hide an earlier closure in this same page.
                    if matches!(error.downcast_ref::<crate::wire::keep::OrdinaryIncomingRefusal>(), Some(crate::wire::keep::OrdinaryIncomingRefusal::MissingPrefix)) { continue; }
                    return Err(error); // Store Unknown/corruption/other refusals keep custody.
                }
                let ordinary = crate::rt::blocking(move || {
                    ordinary.confirm_incoming()?;
                    Ok::<_, anyhow::Error>(ordinary)
                }).await??;
                self.publish_ordinary(&owner, ordinary.phase()).await?;
                // retire also requires durable incoming closure; publication/ACK alone
                // cannot authorize unlink. A pending exact source keeps its issued slot.
                if crate::rt::blocking(move || ordinary.retire()).await?? { self.forget_ordinary_issue(&owner).await?; }
                continue;
            }
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
            // Actual stopped Gates sorted opposite to their incoming seq reproduce the
            // recovery ordering edge without a fake proof, extra controller or long loop.
            let ordinary_records: Arc<dyn Records> = Arc::new(kiln_seams::records::SimRecords::new());
            let mut ordinary = Spool::new_in(Arc::clone(&ordinary_records), admission.clone());
            ordinary.initialize_ordinary("epoch-import", true).await?;
            let work = std::path::absolute("ordinary-import-page-fixture")?;
            let paths = admission.clone(); let at = work.clone();
            let (first,second) = crate::rt::blocking(move || {
                let owner = |seq,execution:&str| OrdinaryOwnerV1 { version:1,grant:format!("grant-import-{seq}"),execution:execution.into(),allocation:format!("retained-import-{seq}"),
                    job:format!("import-{seq}"),attempt:1,host:"host".into(),process_nonce:"n".repeat(43),publication_epoch:"epoch-import".into(),connection:"a".repeat(64),
                    command:kiln_ci::wire::OrdinaryCommandV1{stream:"l:9.1".into(),seq} };
                let first = crate::runner::processes::Execution::ordinary_in(paths.clone(),at.clone(),owner(1,"ci-import-z"))?;
                let second = crate::runner::processes::Execution::ordinary_in(paths,at,owner(2,"ci-import-a"))?;
                Ok::<_,anyhow::Error>((first,second))
            }).await??;
            first.stop().await; second.stop().await; drop(first); drop(second);
            let paths = admission.clone();
            let proofs = crate::rt::blocking(move || crate::runner::processes::recovery_evidence(&paths)).await??;
            assert_eq!(proofs.len(),2);
            ordinary.import_page(work.clone(),proofs.clone()).await?;
            let pending = ordinary.pending_ordinary().await?;
            assert_eq!(pending.len(),1);
            assert_eq!(pending.first().map(|p|p.owner.command.seq),Some(1),"later UUID-sorted proof cannot prevent earlier prefix closure");
            ordinary.import_page(work,proofs).await?;
            let pending = ordinary.pending_ordinary().await?;
            assert_eq!(pending.iter().map(|p|p.owner.command.seq).collect::<std::collections::BTreeSet<_>>(),[1,2].into_iter().collect::<std::collections::BTreeSet<_>>());
            let paths = admission.clone();
            assert!(!crate::rt::blocking(move || crate::runner::processes::ordinary_inventory_present(&paths)).await??,"both source Gates retire only after their own durable incoming closure and publication");
            ordinary.received_ordinary(&OrdinaryFrontierAckV1{kind:"ordinary_frontier".into(),version:1,epoch:"epoch-import".into(),through:2}).await?;
            assert!(ordinary.pending_ordinary().await?.is_empty());
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
            let mut restarted = Spool::new_in(Arc::clone(&records), admission.clone());
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

            // U01/U03: the SAME oracle now covers ordinary sequence custody; these inputs
            // stand for positive strict Gate proofs, not generic was_retained=false rows.
            assert_eq!(restarted.initialize_ordinary("epoch-fixture", true).await?, "epoch-fixture");
            let ordinary_owner = |n| OrdinaryOwnerV1 { version: 1, grant: format!("grant-{n}"), execution: format!("ci-ordinary-{n}"), allocation: format!("retained-{n}"),
                job: format!("ordinary-{n}"), attempt: 1, host: "host-fixture".into(), process_nonce: "process-fixture".into(), publication_epoch: "epoch-fixture".into(), connection: "a".repeat(64),
                command: kiln_ci::wire::OrdinaryCommandV1 { stream: "l:7.1".into(), seq: n } };
            let ack = |through| OrdinaryFrontierAckV1 { kind: "ordinary_frontier".into(), version: 1, epoch: "epoch-fixture".into(), through };
            // U05: actual original native Gate + SAME Records. A lost local reply
            // reconciles the exact committed slot; no generic delivery ACK deletes it.
            let owner=ordinary_owner(9000);
            let execution=crate::runner::processes::Execution::ordinary_in(admission.clone(),std::path::absolute("producer-log-fixture")?,owner.clone())?;
            let binding=crate::proto::JobLogBindingV1{contract:crate::proto::JobLogContractV1{version:1,capture_limit:5},owner};
            let producer=restarted.register_job_log(binding.clone(),&execution).await?;
            let writer=producer.foreground();
            let (tx,rx)=async_channel::bounded(1);let up=crate::exec::Uplink(tx);
            let mut write=Box::pin(writer.log(&up,4,b"abc".to_vec()));
            assert!(futures::poll!(write.as_mut()).is_pending());
            let crate::exec::Up::JobLog(id)=rx.recv().await? else{anyhow::bail!("fixture lost original producer input")};
            assert_eq!(id,crate::proto::job_log::binding_id(&binding)?);
            let input=producer.pending().ok_or_else(||anyhow::anyhow!("fixture lost original producer input"))?;
            assert!(Arc::ptr_eq(&producer.pending().ok_or_else(||anyhow::anyhow!("producer lost its pending input"))?,&input));
            restarted.publish_job_log(&input).await?;
            let original=input.published().flatten().ok_or_else(||anyhow::anyhow!("producer item was not published"))?;
            restarted.publish_job_log(&input).await?; // original commit completed, its reply was lost
            assert_eq!(input.published().flatten(),Some(original.clone()));
            assert!(restarted.job_log_debt().await?);
            producer.complete(&input)?;write.await?;
            let mut write=Box::pin(writer.log(&up,7,b"defgh".to_vec()));
            assert!(futures::poll!(write.as_mut()).is_pending());
            let crate::exec::Up::JobLog(id)=rx.recv().await? else{anyhow::bail!("fixture lost second producer input")};
            assert_eq!(id,crate::proto::job_log::binding_id(&binding)?);
            let input=producer.pending().ok_or_else(||anyhow::anyhow!("fixture lost second producer input"))?;
            restarted.publish_job_log(&input).await?;producer.complete(&input)?;write.await?;
            let result=crate::proto::JobResult{job:binding.owner.job.clone(),status:crate::store::Status::Succeeded,exit_code:Some(0),error:None,peak_rss:None,cpu_seconds:Some(0.0)};
            let mut end_wait=Box::pin(producer.end(&up,&result));
            assert!(futures::poll!(end_wait.as_mut()).is_pending());
            let crate::exec::Up::JobLog(id)=rx.recv().await? else{anyhow::bail!("fixture lost original producer End")};
            assert_eq!(id,crate::proto::job_log::binding_id(&binding)?);
            let input=producer.pending().ok_or_else(||anyhow::anyhow!("fixture lost original producer End"))?;
            restarted.publish_job_log(&input).await?;
            let Some(Some(crate::wire::channel::Frame::Item{seq,body,..}))=input.published() else{anyhow::bail!("fixture End not retained")};
            let crate::proto::AgentMsg::JobLogEndV1(end)=crate::proto::read_agent(&body).map_err(|e|anyhow::anyhow!(e.why("producer fixture End")))? else{anyhow::bail!("fixture retained wrong item")};
            assert_eq!((seq,end.final_seq,end.captured_bytes),(3,2,5));
            assert_eq!(end.capture,crate::proto::JobLogCaptureV1::Truncated{limit:5,discarded_bytes:3});
            producer.complete(&input)?;end_wait.await?;
            let id=crate::proto::job_log::binding_id(&binding)?;
            let pending=crate::wire::keep::load_out(&*records,"server").await?;
            assert_eq!(pending.iter().find(|row|row.stream==crate::proto::job_log::stream(&binding).unwrap_or_default()).map(|row|row.pending.len()),Some(3));
            let unknown=crate::proto::JobLogReplyV1::Unknown{binding_id:id,item_seq:seq,operation:Some(crate::proto::job_log::stable_operation(&id,seq)?),why:"lost canonical reply".into()};
            assert!(restarted.receive_job_log(&binding,&unknown).await.is_err());
            assert!(matches!(restarted.clean_job_log_page(&binding).await?,logs::Clean::Waiting));
            let receipt=crate::proto::JobLogReceiptV1{binding_id:id,item_seq:seq,chunk_seq:2,captured_bytes:5,root:end.root,operation:crate::proto::job_log::stable_operation(&id,seq)?,view:crate::proto::JobLogViewV1::Captured(crate::proto::JobLogPrefixV1{view:"original-view".into(),revision:1,root:"a".repeat(64)}),result_hash:Some(end.result_hash)};
            let mut wrong=receipt.clone();wrong.root=[9;32];
            let before=records.head().await?;
            assert!(restarted.receive_job_log(&binding,&crate::proto::JobLogReplyV1::Committed(wrong)).await.is_err());
            assert_eq!(records.head().await?,before);
            let ack=restarted.receive_job_log(&binding,&crate::proto::JobLogReplyV1::Committed(receipt)).await?;
            assert_eq!(ack.upto,3);
            assert!(matches!(restarted.clean_job_log_page(&binding).await?,logs::Clean::Finished));
            assert!(!restarted.job_log_debt().await?);
            // No OS child was launched by this metadata fixture; close that SAME Gate
            // NoStart rather than fabricate a stopped kernel owner.
            execution.close_ordinary_admission(true)?;
            assert!(restarted.register_job_log(binding,&execution).await.is_err());

            // The real row window reaches Full without deleting the blocked original.
            let owner=ordinary_owner(9001);
            let execution=crate::runner::processes::Execution::ordinary_in(admission.clone(),std::path::absolute("producer-full-fixture")?,owner.clone())?;
            let binding=crate::proto::JobLogBindingV1{contract:crate::proto::JobLogContractV1{version:1,capture_limit:512},owner};
            let producer=restarted.register_job_log(binding.clone(),&execution).await?;
            let writer=producer.foreground();
            let mut flows=Vec::new();
            for _ in 1..64{flows.push(writer.fork().await?);}
            let mut waiting=Box::pin(writer.fork());
            assert!(futures::poll!(waiting.as_mut()).is_pending(),"RAM Full waits before a new flow can read");
            assert!(writer.log(&up,0,Vec::new()).await.is_err(),"empty input cannot bypass RAM quota");
            assert!(producer.pending().is_none()&&rx.try_recv().is_err(),"empty input creates no pending metadata or wake");
            let flow=flows.pop().ok_or_else(||anyhow::anyhow!("fixture capture flow missing"))?;
            let mut lost=Box::pin(flow.log(&up,0,vec![7]));
            assert!(futures::poll!(lost.as_mut()).is_pending());
            let crate::exec::Up::JobLog(id)=rx.recv().await? else{anyhow::bail!("fixture lost preowned RAM input")};
            assert_eq!(id,crate::proto::job_log::binding_id(&binding)?);
            let original=producer.pending().ok_or_else(||anyhow::anyhow!("fixture lost preowned RAM input"))?;
            drop(lost);drop(flow);
            assert!(Arc::ptr_eq(&producer.pending().ok_or_else(||anyhow::anyhow!("cancelled waiter lost original bytes"))?,&original));
            assert!(futures::poll!(waiting.as_mut()).is_pending(),"cancelling a writer cannot reuse its occupied input slot");
            restarted.publish_job_log(&original).await?;
            let mut first=original.published().flatten();
            producer.complete(&original)?;
            let reused=waiting.await?;drop(reused);drop(flows);
            for _ in 0..255u32{
                let mut write=Box::pin(writer.log(&up,0,vec![7]));
                assert!(futures::poll!(write.as_mut()).is_pending());
                let crate::exec::Up::JobLog(id)=rx.recv().await? else{anyhow::bail!("fixture lost bounded input")};
            assert_eq!(id,crate::proto::job_log::binding_id(&binding)?);
            let input=producer.pending().ok_or_else(||anyhow::anyhow!("fixture lost bounded input"))?;
                restarted.publish_job_log(&input).await?;
                if first.is_none(){first=input.published().flatten();}
                producer.complete(&input)?;write.await?;
            }
            let mut blocked=Box::pin(writer.log(&up,0,vec![8]));
            assert!(futures::poll!(blocked.as_mut()).is_pending());
            let crate::exec::Up::JobLog(id)=rx.recv().await? else{anyhow::bail!("fixture lost Full input")};
            assert_eq!(id,crate::proto::job_log::binding_id(&binding)?);
            let input=producer.pending().ok_or_else(||anyhow::anyhow!("fixture lost Full input"))?;
            let before=records.head().await?;
            let error=restarted.publish_job_log(&input).await.err().ok_or_else(||anyhow::anyhow!("outbox did not impose its row bound"))?;
            assert!(error.downcast_ref::<logs::Full>().is_some());
            assert_eq!(records.head().await?,before);
            assert!(Arc::ptr_eq(&producer.pending().ok_or_else(||anyhow::anyhow!("Full lost original bytes"))?,&input));
            let Some(crate::wire::channel::Frame::Item{body,..})=first else{anyhow::bail!("fixture lost first committed body")};
            let crate::proto::AgentMsg::JobLogChunkV1(chunk)=crate::proto::read_agent(&body).map_err(|e|anyhow::anyhow!(e.why("producer fixture chunk")))? else{anyhow::bail!("fixture first body is not a chunk")};
            let id=crate::proto::job_log::binding_id(&binding)?;
            let receipt=crate::proto::JobLogReceiptV1{binding_id:id,item_seq:1,chunk_seq:1,captured_bytes:1,root:crate::proto::job_log::chunk_root(&chunk)?,operation:crate::proto::job_log::stable_operation(&id,1)?,view:crate::proto::JobLogViewV1::Captured(crate::proto::JobLogPrefixV1{view:"full-view".into(),revision:1,root:"b".repeat(64)}),result_hash:None};
            restarted.receive_job_log(&binding,&crate::proto::JobLogReplyV1::Committed(receipt)).await?;
            restarted.clean_job_log_page(&binding).await?;
            restarted.publish_job_log(&input).await?;producer.complete(&input)?;blocked.await?;
            drop(producer);
            // This deliberate pending producer remains durable after handle loss; neither
            // a missing sender nor successful physical NoStart is log-completion proof.
            execution.close_ordinary_admission(true)?;
            assert!(restarted.job_log_debt().await?);
            let cold=Spool::new_in(Arc::clone(&records),admission.clone());
            assert!(cold.job_log_debt().await?);

            let first_owner = ordinary_owner(1);
            let first = restarted.publish_ordinary(&first_owner, OrdinaryPhaseV1::Stopped).await?;
            assert_eq!(first.seq, 1);
            drop(restarted); // publication committed but reply/send was lost
            let mut restarted = Spool::new_in(Arc::clone(&records), admission.clone());
            assert_eq!(restarted.initialize_ordinary("unused-candidate-epoch", false).await?, "epoch-fixture", "restart retains its permanent domain");
            assert_eq!(restarted.publish_ordinary(&first_owner, OrdinaryPhaseV1::Stopped).await?, first);
            assert_eq!(restarted.pending_ordinary().await?, vec![first.clone()]);
            let mut conflicting = first_owner.clone();
            conflicting.allocation = "retained-elsewhere".into();
            assert!(restarted.publish_ordinary(&conflicting, OrdinaryPhaseV1::Stopped).await.is_err());
            assert!(restarted.publish_ordinary(&first_owner, OrdinaryPhaseV1::NoStart).await.is_err(), "one grant cannot prove both dispositions");
            let before = records.head().await?;
            let mut foreign = ack(1); foreign.epoch = "other-epoch".into();
            assert!(restarted.received_ordinary(&foreign).await.is_err());
            assert!(restarted.received_ordinary(&ack(2)).await.is_err(), "future ACK cannot clear unissued evidence");
            let mut unknown = ack(1); unknown.kind = "unknown-tag".into();
            assert!(restarted.received_ordinary(&unknown).await.is_err());
            assert_eq!(records.head().await?, before, "invalid ACKs mutate neither rows, count nor frontier");
            restarted.received_ordinary(&ack(1)).await?;
            assert!(restarted.pending_ordinary().await?.is_empty());
            assert_eq!(restarted.publish_ordinary(&first_owner, OrdinaryPhaseV1::Stopped).await?, first, "ACK before source retirement must not allocate a new sequence");
            let counter = records.get(META, COUNT, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("missing fixture count"))?;
            assert_eq!(serde_json::from_slice::<usize>(&counter.value)?, 1, "source custody remains bounded and charged after delivery cleanup");
            restarted.forget_ordinary_issue(&first_owner).await?;
            restarted.forget_ordinary_issue(&first_owner).await?;

            // The reverse order retains pending evidence through a lost server reply.
            let second_owner = ordinary_owner(2);
            let second = restarted.publish_ordinary(&second_owner, OrdinaryPhaseV1::NoStart).await?;
            restarted.forget_ordinary_issue(&second_owner).await?;
            drop(restarted);
            let mut restarted = Spool::new_in(Arc::clone(&records), admission.clone());
            assert_eq!(restarted.pending_ordinary().await?, vec![second]);
            restarted.received_ordinary(&ack(2)).await?;
            restarted.received_ordinary(&ack(1)).await?; // stale prefix cannot regress
            assert!(restarted.pending_ordinary().await?.is_empty());
            let counter = records.get(META, COUNT, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("missing fixture count"))?;
            assert_eq!(serde_json::from_slice::<usize>(&counter.value)?, 0);

            // Two small windows cross the cleanup PAGE and repeat compacted prefixes.
            // No lifetime per-job row survives; sequence state is two permanent metadata
            // rows, and pending/issued work never needs a 16k-iteration timing benchmark.
            let mut last = 2;
            for _ in 0..2 {
                for _ in 0..=PAGE {
                    last += 1;
                    let owner = ordinary_owner(last);
                    let publication = restarted.publish_ordinary(&owner, OrdinaryPhaseV1::Stopped).await?;
                    assert_eq!(publication.seq, last);
                    restarted.forget_ordinary_issue(&owner).await?;
                }
                assert_eq!(restarted.pending_ordinary().await?.len(), PAGE);
                assert_eq!(restarted.pending_ordinary().await?.len(), 1, "a missing prefix ACK cannot starve later pending evidence");
                restarted.received_ordinary(&ack(last)).await?; // one bounded cleanup page
                assert!(restarted.pending_ordinary().await?.is_empty()); // resumes last row cleanup
                assert!(records.scan(META, Scan { prefix: ISSUED.to_vec(), after: None, limit: PAGE, at: Consistency::Linearizable }).await?.rows.is_empty());
                assert!(records.scan(ORDINARY, Scan { prefix: vec![], after: None, limit: PAGE, at: Consistency::Linearizable }).await?.rows.is_empty());
                let counter = records.get(META, COUNT, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("missing fixture count"))?;
                assert_eq!(serde_json::from_slice::<usize>(&counter.value)?, 0);
            }

            let (domain_row, mut domain, _) = restarted.domain_row().await?;
            let preserved = domain_row.value.clone();
            records.commit(Tx { conds: vec![], writes: vec![Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&LIMIT)? }] }).await?;
            let before = records.head().await?;
            assert!(restarted.publish_ordinary(&ordinary_owner(last + 1), OrdinaryPhaseV1::Stopped).await.is_err(), "unsettled source custody imposes backpressure even after delivery ACK");
            assert_eq!(records.head().await?, before);
            records.commit(Tx { conds: vec![], writes: vec![Write::Put { family: META.into(), key: COUNT.to_vec(), value: serde_json::to_vec(&0usize)? }] }).await?;
            domain.issued = last + LIMIT as u64;
            records.commit(Tx { conds: vec![], writes: vec![Write::Put { family: META.into(), key: DOMAIN.to_vec(), value: serde_json::to_vec(&domain)? }] }).await?;
            let before = records.head().await?;
            assert!(restarted.publish_ordinary(&ordinary_owner(last + 1), OrdinaryPhaseV1::Stopped).await.is_err(), "a full unresolved prefix cannot skip holes or reuse sequence numbers");
            assert_eq!(records.head().await?, before);
            records.commit(Tx { conds: vec![], writes: vec![Write::Delete { family: META.into(), key: DOMAIN.to_vec() }] }).await?;
            let before = records.head().await?;
            assert!(restarted.initialize_ordinary("replacement", true).await.is_err(), "empty queue cannot reset a previously installed publication domain");
            assert_eq!(records.head().await?, before);
            records.commit(Tx { conds: vec![], writes: vec![Write::Put { family: META.into(), key: DOMAIN.to_vec(), value: preserved }] }).await?;
            domain.issued = u64::MAX;
            domain.acknowledged = u64::MAX;
            records.commit(Tx { conds: vec![], writes: vec![Write::Put { family: META.into(), key: DOMAIN.to_vec(), value: serde_json::to_vec(&domain)? }] }).await?;
            let before = records.head().await?;
            assert!(restarted.publish_ordinary(&ordinary_owner(last + 1), OrdinaryPhaseV1::Stopped).await.is_err(), "exhaustion never wraps or rotates the original epoch");
            assert_eq!(records.head().await?, before);
            Ok(())
        })
    }
}
