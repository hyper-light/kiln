//! U03: the execution gate of the existing physical fence registry. Group records prove
//! individual kernel/daemon cleanup; only a closed gate with every group proved can become
//! stopped evidence. Application delivery and its acknowledgements belong to host Amphora.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::{Admission, ExecutorOwner, ParentBinding, remove, save};

const LIMIT: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoppedExecution {
    pub execution: String,
    pub job: String,
    pub attempt: Option<u32>,
    pub work: PathBuf,
    pub parent: Option<ParentBinding>,
    pub was_retained: Option<bool>,
    pub executor_owner: Option<ExecutorOwner>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    execution: String,
    job: String,
    attempt: Option<u32>,
    work: PathBuf,
    parent: Option<ParentBinding>,
    #[serde(default)]
    executor_owner: Option<ExecutorOwner>,
    #[serde(default)]
    ordinary_owner: Option<kiln_ci::wire::OrdinaryOwnerV1>,
    #[serde(default)]
    incoming_closed: bool,
    phase: Phase,
    #[serde(default)]
    resource_domain: Option<ResourceDomain>,
    #[serde(default)]
    retained: bool,
    #[serde(default)]
    was_retained: Option<bool>,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase { Open, Closed, Stopped, V2Open, V2Closed, V2Stopped, OrdinaryOpen, OrdinaryClosed, OrdinaryStopped, OrdinaryNoStart }
impl Phase {
    fn open(self) -> bool { matches!(self, Self::Open | Self::V2Open | Self::OrdinaryOpen) }
    fn closed(self) -> bool { matches!(self, Self::Closed | Self::V2Closed | Self::OrdinaryClosed) }
    fn stopped(self) -> bool { matches!(self, Self::Stopped | Self::V2Stopped | Self::OrdinaryStopped | Self::OrdinaryNoStart) }
    fn v2(self) -> bool { matches!(self, Self::V2Open | Self::V2Closed | Self::V2Stopped) }
    fn ordinary(self) -> bool { matches!(self, Self::OrdinaryOpen | Self::OrdinaryClosed | Self::OrdinaryStopped | Self::OrdinaryNoStart) }
    fn closing(self) -> Self { if self.ordinary() { Self::OrdinaryClosed } else if self.v2() { Self::V2Closed } else { Self::Closed } }
    fn stopping(self) -> Self { if self.ordinary() { Self::OrdinaryStopped } else if self.v2() { Self::V2Stopped } else { Self::Stopped } }
}

/// The existing strict phase enum is the reader-version fence: older binaries reject all
/// v2 phases instead of silently ignoring this new physical proof obligation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ResourceDomain { identity: String, limits: crate::os::cgroup::CpuMemoryLimit }


pub(super) struct Gate { pub(super) admission: Admission, pub(super) identity: String, _permit: std::fs::File, lifecycle: parking_lot::Mutex<()> }

fn root(admission: &Admission) -> PathBuf { admission.ci().join("executions") }
fn path(admission: &Admission, identity: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(identity.starts_with("ci-") && identity.len() <= 128 && identity.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid execution fence identity");
    Ok(root(admission).join(format!("{identity}.json")))
}
fn read(admission: &Admission, identity: &str) -> anyhow::Result<Record> {
    let record: Record = serde_json::from_slice(&std::fs::read(path(admission, identity)?)?)?;
    validate_record(identity, &record)?;
    Ok(record)
}

fn validate_record(identity: &str, record: &Record) -> anyhow::Result<()> {
    anyhow::ensure!(record.execution == identity && !record.job.is_empty() && record.job.len() <= 512 && record.attempt.is_none_or(|a| a > 0), "invalid execution fence metadata");
    anyhow::ensure!(!(record.retained && (record.phase.stopped() || record.was_retained == Some(false))), "inconsistent retained execution fence");
    anyhow::ensure!(record.phase.v2() == record.resource_domain.is_some(), "inconsistent execution Gate version/domain");
    anyhow::ensure!(record.phase.ordinary() == record.ordinary_owner.is_some(), "inconsistent ordinary Gate version/owner");
    if let Some(owner) = &record.ordinary_owner {
        owner.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(owner.execution == record.execution && owner.job == record.job && record.attempt == Some(owner.attempt)
            && record.parent.is_none() && record.executor_owner.is_none() && record.resource_domain.is_none(), "ordinary Gate changed its original native grant");
    }
    if let Some(domain) = &record.resource_domain {
        domain.limits.validate()?;
        let identity = Path::new(&domain.identity);
        anyhow::ensure!(record.executor_owner.is_some() && identity.starts_with("/sys/fs/cgroup")
            && identity.file_name().and_then(|n| n.to_str()) == Some(record.execution.as_str())
            && !identity.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)), "invalid aggregate execution domain identity");
    }
    if let Some(owner) = &record.executor_owner {
        for value in [&owner.executor, &owner.generation, &owner.allocation] { anyhow::ensure!(!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control), "invalid existing executor grant in physical evidence"); }
    }
    if let Some(parent) = &record.parent {
        anyhow::ensure!(parent.physical_execution == identity && parent.job == record.job && record.attempt == Some(parent.attempt), "the physical execution fence changed its adopted owner");
        if let Some(owner) = &record.executor_owner { anyhow::ensure!(parent.executor == owner.executor && parent.generation == owner.generation && parent.allocation == owner.allocation, "the physical execution fence changed its startup grant"); }
    }
    Ok(())
}

impl Gate {
    pub(super) fn create(admission: Admission, execution: &str, job: &str, attempt: Option<u32>, work: &Path, parent: Option<ParentBinding>, executor_owner: Option<ExecutorOwner>) -> anyhow::Result<Arc<Self>> {
        Self::create_owned(admission, execution, job, attempt, work, parent, executor_owner, None)
    }

    pub(super) fn create_owned(admission: Admission, execution: &str, job: &str, attempt: Option<u32>, work: &Path, parent: Option<ParentBinding>, executor_owner: Option<ExecutorOwner>, ordinary_owner: Option<kiln_ci::wire::OrdinaryOwnerV1>) -> anyhow::Result<Arc<Self>> {
        if let Some(owner) = &ordinary_owner {
            owner.validate().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(owner.execution == execution && owner.job == job && attempt == Some(owner.attempt) && parent.is_none() && executor_owner.is_none(), "ordinary Gate admission changed its grant");
        }
        std::fs::create_dir_all(root(&admission))?;
        anyhow::ensure!(entries(&admission)?.len() < LIMIT, "physical execution evidence is full; deliver its pending stopped proofs before admitting work");
        let at = path(&admission, execution)?;
        let permit = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(at.with_extension("lock"))?;
        permit.try_lock()?;
        let work = std::path::absolute(work)?;
        save(&at, &Record { execution: execution.into(), job: job.into(), attempt, work, parent, executor_owner, phase: if ordinary_owner.is_some() { Phase::OrdinaryOpen } else { Phase::Open }, ordinary_owner, incoming_closed: false, resource_domain: None, retained: false, was_retained: Some(false) })?;
        Ok(Arc::new(Self { admission, identity: execution.into(), _permit: permit, lifecycle: parking_lot::Mutex::new(()) }))
    }

    pub(super) fn close_incoming(&self, owner: &kiln_ci::wire::OrdinaryOwnerV1, no_start: bool) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        let mut record = read(&self.admission, &self.identity)?;
        anyhow::ensure!(record.ordinary_owner.as_ref() == Some(owner), "ordinary admission changed its exact owner");
        if no_start {
            anyhow::ensure!(record.phase == Phase::OrdinaryOpen && !super::records(&self.admission)?.iter().any(|(_, group)| group.execution.as_deref() == Some(self.identity.as_str())), "NoStart cannot erase an admitted physical effect");
            record.phase = Phase::OrdinaryNoStart;
        }
        record.incoming_closed = true;
        save(&path(&self.admission, &self.identity)?, &record)
    }

    pub(super) fn bind_domain(&self, kernel: &super::Containment, limits: crate::os::cgroup::CpuMemoryLimit) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        limits.validate()?;
        let mut record = read(&self.admission, &self.identity)?;
        anyhow::ensure!(record.phase.open() && record.executor_owner.is_some(), "aggregate limits require an open existing executor owner");
        let domain = ResourceDomain { identity: kernel.identity(), limits };
        anyhow::ensure!(record.resource_domain.as_ref().is_none_or(|old| old == &domain), "aggregate domain changed its original grant");
        record.resource_domain = Some(domain);
        record.phase = Phase::V2Open;
        save(&path(&self.admission, &self.identity)?, &record)
    }

    /// The same existing Gate owner serializes admission through durable leaf registration.
    /// Stop/retained release can then see that record before considering aggregate proof.
    pub(super) fn with_child<R>(&self, id: &str, register: impl FnOnce(super::Containment, Option<ParentBinding>) -> anyhow::Result<R>) -> anyhow::Result<R> {
        let _serial = self.lifecycle.lock();
        let record = read(&self.admission, &self.identity)?;
        let kernel = if let Some(domain) = record.resource_domain {
            anyhow::ensure!(record.phase.open() || (record.phase.closed() && record.retained), "aggregate execution no longer admits child work");
            super::Containment::recover(&self.admission, &domain.identity, &record.execution)?.child(id, domain.limits)?
        } else { super::Containment::create(&self.admission, id)? };
        register(kernel, record.parent)
    }

    pub(super) fn try_stopped(&self) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        try_stopped(&self.admission, &self.identity)
    }

    pub(super) fn close(&self) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        let mut record = read(&self.admission, &self.identity)?;
        if record.phase.open() { record.phase = record.phase.closing(); save(&path(&self.admission, &self.identity)?, &record)?; }
        try_stopped(&self.admission, &self.identity)
    }

    pub(super) fn bind_parent(&self, parent: &ParentBinding) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        let mut record = read(&self.admission, &self.identity)?;
        anyhow::ensure!(record.phase.open(), "a parent binding cannot change a closed execution");
        match &record.executor_owner {
            Some(owner) => anyhow::ensure!(owner.executor == parent.executor && owner.generation == parent.generation && owner.allocation == parent.allocation, "the assignment does not own this existing executor grant"),
            None => anyhow::ensure!(record.job == parent.job && record.attempt == Some(parent.attempt), "a parent binding cannot change a different job execution"),
        }
        anyhow::ensure!(record.parent.as_ref().is_none_or(|old| old == parent), "an executor cannot change its physical owner");
        record.job = parent.job.clone();
        record.attempt = Some(parent.attempt);
        record.parent = Some(parent.clone());
        save(&path(&self.admission, &self.identity)?, &record)?;
        Ok(())
    }

    pub(super) fn retained(&self) -> anyhow::Result<bool> { Ok(read(&self.admission, &self.identity)?.retained) }

    pub(super) fn retain(&self) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        let mut record = read(&self.admission, &self.identity)?;
        anyhow::ensure!(record.phase.open(), "debug ownership must be retained before normal execution closes");
        record.retained = true;
        record.was_retained = Some(true);
        save(&path(&self.admission, &self.identity)?, &record)
    }

    pub(super) fn release(&self) -> anyhow::Result<()> {
        let _serial = self.lifecycle.lock();
        let mut record = read(&self.admission, &self.identity)?;
        anyhow::ensure!(!record.phase.open(), "retained debug ownership cannot release an active ordinary execution");
        record.retained = false;
        save(&path(&self.admission, &self.identity)?, &record)?;
        try_stopped(&self.admission, &self.identity)
    }
}

/// U01/U03: inspect this exact durable Gate. Closed/retained is pending, not physical
/// retirement; a missing or unreadable Gate is unknown. Only the existing group retirement
/// and retained-release paths may advance it to Stopped after every canonical group is gone.
pub(super) fn stopped(admission: &Admission, identity: &str, owner: &ExecutorOwner) -> anyhow::Result<bool> {
    let record = read(admission, identity)?;
    anyhow::ensure!(record.executor_owner.as_ref() == Some(owner), "stopped evidence changed the existing executor grant");
    Ok(record.phase.stopped())
}

pub(super) fn registered(admission: &Admission, identity: &str) -> anyhow::Result<()> { read(admission, identity).map(|_| ()) }

fn try_stopped(admission: &Admission, identity: &str) -> anyhow::Result<()> {
    let mut record = read(admission, identity)?;
    if !record.phase.closed() || record.retained { return Ok(()) }
    if super::records(admission)?.iter().any(|(_, group)| group.execution.as_deref() == Some(identity)) { return Ok(()) }
    let domain = record.resource_domain.as_ref().map(|domain| super::Containment::recover(admission, &domain.identity, &record.execution)).transpose()?;
    if let Some(domain) = &domain {
        // Future admission is closed and every canonical child record has retired. Kill
        // any remaining descendants through the SAME aggregate kernel owner.
        domain.kill();
        if !domain.empty_aggregate_proven()? { return Ok(()); }
    }
    record.phase = record.phase.stopping();
    // A missing bound domain is never proof. Persist positive proof BEFORE removing it,
    // so a crash between these operations cannot turn absence into guessed emptiness.
    save(&path(admission, identity)?, &record)?;
    if let Some(domain) = domain { domain.remove(); }
    Ok(())
}

fn entries(admission: &Admission) -> anyhow::Result<Vec<(PathBuf, Record)>> {
    let files = match std::fs::read_dir(root(admission)) { Ok(files) => files, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()), Err(e) => return Err(e.into()) };
    let mut rows = Vec::new();
    for file in files {
        let file = file?;
        if file.path().extension().and_then(|s| s.to_str()) != Some("json") { continue }
        let identity = file.path().file_stem().and_then(|s| s.to_str()).context("execution fence has no identity")?.to_owned();
        match read(admission, &identity) {
            Ok(record) => rows.push((file.path(), record)),
            Err(e) if !file.path().try_exists()? => { tracing::debug!(error = %e, "an acknowledged execution fence disappeared during inspection"); },
            Err(e) => return Err(e),
        }
        anyhow::ensure!(rows.len() <= LIMIT, "physical execution evidence exceeds its budget");
    }
    Ok(rows)
}

/// After per-group recovery, an abandoned gate can accept no more spawns. Its held permit
/// distinguishes a live execution from a crash without guessing from a missing process list.
pub(super) fn recover(admission: &Admission) -> anyhow::Result<()> {
    for (at, inventoried) in entries(admission)? {
        let permit = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(at.with_extension("lock"))?;
        match permit.try_lock() {
            Ok(()) => {},
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        // Inventory can predate a live owner's final proof/ACK. Only the current record
        // under this exclusive permit may drive mutation; never roll Stopped back to Closed.
        let mut record = match read(admission, &inventoried.execution) {
            Ok(record) => record,
            Err(_) if !at.try_exists()? => continue, // Disappeared; mint no new proof.
            Err(error) => return Err(error),
        };
        // No capability holding this permit survived. Recovery, rather than a missing
        // process listing or keep deadline, closes all future debug admission.
        if record.phase.open() || record.retained { record.phase = record.phase.closing(); record.retained = false; save(&at, &record)?; }
        try_stopped(admission, &record.execution)?;
        anyhow::ensure!(!record.phase.v2() || read(admission, &record.execution)?.phase.stopped(), "aggregate execution recovery is still pending; retaining admission fence");
    }
    Ok(())
}

/// Import these exact tuples into the existing host Amphora delivery spool before ACKing
/// physical evidence. Unknown historical attempts deliberately supply no release proof.
pub(crate) fn recovery_evidence(admission: &Admission) -> anyhow::Result<Vec<StoppedExecution>> {
    Ok(entries(admission)?.into_iter().filter_map(|(_, r)| {
        (r.phase.stopped()).then_some(StoppedExecution { execution: r.execution, job: r.job, attempt: r.attempt, work: r.work, parent: r.parent, was_retained: r.was_retained, executor_owner: r.executor_owner })
    }).collect())
}

#[cfg(test)]
pub(crate) fn acknowledge_evidence(admission: &Admission, proof: &StoppedExecution) -> anyhow::Result<()> {
    anyhow::ensure!(try_acknowledge_evidence(admission, proof)?, "execution cleanup still holds its physical evidence");
    Ok(())
}

/// After canonical outbox commit, a live exact Gate permit can defer registry cleanup.
/// Only WouldBlock is pending; missing acknowledged evidence is idempotent and malformed
/// metadata or other lock failures remain errors. No other proof in an import page is starved.
pub(crate) fn try_acknowledge_evidence(admission: &Admission, proof: &StoppedExecution) -> anyhow::Result<bool> {
    let at = path(admission, &proof.execution)?;
    let record = match read(admission, &proof.execution) { Ok(r) => r, Err(_) if !at.try_exists()? => return Ok(true), Err(e) => return Err(e) };
    anyhow::ensure!((proof.attempt.is_some() || proof.executor_owner.is_some()) && proof.was_retained.is_some() && record.phase.stopped() && record.job == proof.job && record.attempt == proof.attempt && record.work == proof.work && record.parent == proof.parent && record.was_retained == proof.was_retained && record.executor_owner == proof.executor_owner, "the receipt does not own this stopped execution proof");
    if record.phase.ordinary() { return Ok(false); }
    // Do not unlink evidence while a live cleanup still holds its exact root permit. That
    // cleanup may still be returning from its durable close; the Amphora spool can retry.
    let permit = match std::fs::OpenOptions::new().read(true).write(true).open(at.with_extension("lock")) {
        Ok(permit) => permit,
        Err(_) if !at.try_exists()? => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    match permit.try_lock() {
        Ok(()) => {},
        Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    if let Some(domain) = &record.resource_domain {
        // v2_stopped is already the durable positive kernel proof. Absence here is only
        // idempotent metadata cleanup, never the basis for issuing that proof.
        super::Containment::recover(admission, &domain.identity, &record.execution)?.remove();
    }
    remove(&at)?;
    remove(&at.with_extension("lock"))?;
    Ok(true)
}

/// U01/U03: custody of the EXISTING Gate lock, never another lock/controller.
pub(crate) enum OrdinaryClaim { Legacy, Pending, Claimed(Box<OrdinaryEvidence>) }
pub(crate) struct OrdinaryEvidence { admission: Admission, proof: StoppedExecution, owner: kiln_ci::wire::OrdinaryOwnerV1, phase: kiln_ci::wire::OrdinaryPhaseV1, _permit: std::fs::File }
impl OrdinaryEvidence {
    pub(crate) fn owner(&self) -> &kiln_ci::wire::OrdinaryOwnerV1 { &self.owner }
    pub(crate) fn phase(&self) -> kiln_ci::wire::OrdinaryPhaseV1 { self.phase }
    pub(crate) fn confirm_incoming(&self) -> anyhow::Result<()> {
        let mut record = read(&self.admission, &self.proof.execution)?;
        anyhow::ensure!(record.ordinary_owner.as_ref() == Some(&self.owner) && record.phase.stopped(), "incoming closure changed exact stopped owner");
        record.incoming_closed = true;
        save(&path(&self.admission,&self.proof.execution)?,&record)
    }
    pub(crate) fn retire(self) -> anyhow::Result<bool> {
        let record = read(&self.admission, &self.proof.execution)?;
        anyhow::ensure!(record.ordinary_owner.as_ref() == Some(&self.owner) && record.phase.stopped(), "ordinary source changed during canonical transfer");
        if !record.incoming_closed { return Ok(false); }
        let at = path(&self.admission, &self.proof.execution)?;
        remove(&at)?;
        remove(&at.with_extension("lock"))?;
        Ok(true)
    }
}
#[derive(Debug,thiserror::Error)]
#[error("LegacyOrdinaryExecutionsRequireDrain: pre-feature native Gate evidence remains; physically drain it before ordinary publication readiness")]
struct LegacyOrdinaryExecutionsRequireDrain;
pub(crate) fn ordinary_inventory_present(admission: &Admission) -> anyhow::Result<bool> {
    let rows = entries(admission)?;
    anyhow::ensure!(!rows.iter().any(|(_,record)| !record.phase.ordinary() && record.attempt.is_some() && record.parent.is_none() && record.executor_owner.is_none()),LegacyOrdinaryExecutionsRequireDrain);
    Ok(rows.iter().any(|(_,record)| record.phase.ordinary()))
}
pub(crate) fn claim_ordinary_evidence(admission: &Admission, proof: &StoppedExecution) -> anyhow::Result<OrdinaryClaim> {
    let at = path(admission, &proof.execution)?;
    let first = match read(admission, &proof.execution) { Ok(record) => record, Err(_) if !at.try_exists()? => return Ok(OrdinaryClaim::Pending), Err(error) => return Err(error) };
    if !first.phase.ordinary() { return Ok(OrdinaryClaim::Legacy); }
    let permit = match std::fs::OpenOptions::new().read(true).write(true).open(at.with_extension("lock")) {
        Ok(permit) => permit, Err(_) if !at.try_exists()? => return Ok(OrdinaryClaim::Pending), Err(error) => return Err(error.into()),
    };
    match permit.try_lock() { Ok(()) => {}, Err(std::fs::TryLockError::WouldBlock) => return Ok(OrdinaryClaim::Pending), Err(std::fs::TryLockError::Error(error)) => return Err(error.into()) }
    // Re-read AFTER owning the same permit: a pre-lock snapshot may have been retired.
    let record = match read(admission, &proof.execution) { Ok(record) => record, Err(_) if !at.try_exists()? => return Ok(OrdinaryClaim::Pending), Err(error) => return Err(error) };
    anyhow::ensure!(record.phase.ordinary() && record.phase.stopped() && record.job == proof.job && record.attempt == proof.attempt && record.work == proof.work
        && record.parent == proof.parent && record.executor_owner == proof.executor_owner && record.was_retained == proof.was_retained, "ordinary proof changed its exact physical owner");
    let owner = record.ordinary_owner.context("ordinary proof lacks original owner")?;
    let phase = if record.phase == Phase::OrdinaryNoStart { kiln_ci::wire::OrdinaryPhaseV1::NoStart } else { kiln_ci::wire::OrdinaryPhaseV1::Stopped };
    Ok(OrdinaryClaim::Claimed(Box::new(OrdinaryEvidence { admission: admission.clone(), proof: proof.clone(), owner, phase, _permit: permit })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An older exact phase parser must reject every new proof state. A defaultable
    /// domain field alone would be silently ignored and is therefore insufficient.
    #[test]
    fn u01_aggregate_gate_format_fences_legacy_readers() -> anyhow::Result<()> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyPhase { Open, Closed, Stopped }
        let mut record = Record { execution: "ci-format-fixture".into(), job: "executor:fixture".into(), attempt: None, work: PathBuf::from("fixture"), parent: None,
            executor_owner: Some(ExecutorOwner { executor: "fixture".into(), generation: "generation".into(), allocation: "allocation".into() }),
            ordinary_owner: None, incoming_closed: false, phase: Phase::Open, resource_domain: None, retained: false, was_retained: Some(false) };
        for phase in [Phase::Open, Phase::Closed, Phase::Stopped] {
            record.phase = phase;
            validate_record(&record.execution, &record)?;
            assert!(serde_json::from_value::<LegacyPhase>(serde_json::to_value(phase)?).is_ok());
        }
        let domain = ResourceDomain { identity: "/sys/fs/cgroup/kiln-agents/ci-format-fixture".into(),
            limits: crate::os::cgroup::CpuMemoryLimit { cpu_quota_us: 150000, cpu_period_us: 100000, memory_bytes: 128 << 20 } };
        record.resource_domain = Some(domain.clone());
        assert!(validate_record(&record.execution, &record).is_err(), "a legacy phase cannot hide a new domain");
        for phase in [Phase::V2Open, Phase::V2Closed, Phase::V2Stopped] {
            record.phase = phase;
            validate_record(&record.execution, &record)?;
            assert!(serde_json::from_value::<LegacyPhase>(serde_json::to_value(phase)?).is_err());
            record.resource_domain = None;
            assert!(validate_record(&record.execution, &record).is_err(), "a v2 reader cannot lose its domain obligation");
            record.resource_domain = Some(domain.clone());
        }
        // Ordinary ownership must also fence the actual previous v2 decoder, rather
        // than relying on an optional field which old importers would ignore.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum BeforeOrdinary { Open, Closed, Stopped, V2Open, V2Closed, V2Stopped }
        record.resource_domain = None; record.executor_owner = None;
        record.job = "ordinary-job".into(); record.attempt = Some(1);
        record.ordinary_owner = Some(kiln_ci::wire::OrdinaryOwnerV1 { version:1,grant:"grant".into(),execution:record.execution.clone(),allocation:"retained-grant".into(),job:record.job.clone(),attempt:1,host:"host".into(),process_nonce:"a".repeat(43),publication_epoch:"epoch".into(),connection:"b".repeat(64),command:kiln_ci::wire::OrdinaryCommandV1 { stream:"l:7.1".into(),seq:1 } });
        for phase in [Phase::OrdinaryOpen,Phase::OrdinaryClosed,Phase::OrdinaryStopped,Phase::OrdinaryNoStart] {
            record.phase = phase; validate_record(&record.execution,&record)?;
            assert!(serde_json::from_value::<BeforeOrdinary>(serde_json::to_value(phase)?).is_err());
        }
        record.ordinary_owner = None;
        assert!(validate_record(&record.execution,&record).is_err(), "strict phase cannot lose original owner");
        Ok(())
    }
}
