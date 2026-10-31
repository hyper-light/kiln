//! U01/U03: ordinary physical ownership journal in the existing Dispatch/Amphora writer.
//! Logical completion and transport loss never prove physical retirement. Marker/registration
//! protect future readers, not already-open old writers; rollout still requires their drain.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use kiln_ci::sched::state::Allocation;
use kiln_ci::wire::{JobEventV2, OrdinaryFrontierAckV1, OrdinaryOwnerV1, OrdinaryPhaseV1, OrdinaryPublicationV1};
use crate::sql::{OptionalExtension as _, params};
use super::{Dispatch, Input, Outcome, now_ms};
use crate::ci::runs;

pub(super) const FORMAT: &str = "u01-ordinary-domain-v1";
pub(super) const MARKER_JOB: &str = "@kiln/u01-ordinary-format-v1";
pub(super) const MARKER_AGENT: &str = "@kiln-format";
const DOMAIN_PREFIX: &str = "u01/ordinary-domain/";
const ANCHOR_PREFIX: &str = "u01/ordinary-anchor/";
const LIMIT: usize = 4096;
const BYTES: usize = 16384;

#[derive(Debug,thiserror::Error)]
pub(super) enum Refusal {
    #[error("PublicationEpochMissing: ordinary state requires its original anchored domain")]
    EpochMissing,
    #[error("PublicationEpochMismatch: ordinary state cannot change its original domain")]
    EpochMismatch,
    #[error("OrdinaryFormatRequiresDrain: ordinary format evidence is missing or inconsistent")]
    FormatRequiresDrain,
    #[error("ordinary publication frontier is full; unresolved holes remain fenced")]
    FrontierFull,
    #[error("ordinary publication has no accepted original intent")]
    IntentMissing,
    #[error("ordinary publication changed its original owner")]
    OwnerChanged,
    #[error("ordinary publication contradicts its positive physical disposition")]
    PhaseConflict,
}

/// Only these owned dispositions permit the caller to abandon its SAME positively
/// invalidated, never-exposed Commands token. Unknown/query/owner failures stay outer.
#[derive(Debug,thiserror::Error,PartialEq,Eq)]
pub(super) enum ClaimRefusal {
    #[error("ordinary ownership journal is full; retain pending physical debt and retry after drain")]
    Full,
    #[error("ordinary ownership journal exceeds its byte budget before original claim")]
    TooLarge,
    #[error("this original attempt requires durable producer logs; the selected runner cannot downgrade that contract")]
    LogContractRequired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub format: String,
    pub version: u8,
    pub owner: OrdinaryOwnerV1,
    pub allocation: Allocation,
    pub recorded: String,
    pub physical: String,
    pub logical: String,
    /// U05: None preserves the legacy journal bytes. A required producer contract is
    /// fixed with the original grant, before the same assignment can be exposed.
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub log_contract: Option<kiln_ci::wire::JobLogContractV1>,
}
impl Journal {
    fn validate(&self) -> anyhow::Result<()> {
        self.owner.validate().map_err(anyhow::Error::msg)?;
        if let Some(contract)=&self.log_contract {
            crate::proto::job_log::validate_binding(&kiln_ci::wire::JobLogBindingV1 {contract:contract.clone(),owner:self.owner.clone()})?;
        }
        anyhow::ensure!(self.format == FORMAT && self.version == 1 && self.owner.job != MARKER_JOB
            && self.allocation.id == self.owner.allocation && self.allocation.runner == self.owner.host && self.allocation.parent_binding.is_none()
            && self.allocation.request.cpu.is_finite() && self.allocation.request.cpu >= 0.0
            && matches!(self.physical.as_str(), "pending" | "stopped" | "no_start") && matches!(self.logical.as_str(), "running" | "lost" | "terminal")
            && !self.recorded.is_empty() && self.recorded.len() <= 128 && !self.recorded.chars().any(char::is_control), "U01 invalid ordinary journal");
        Ok(())
    }
}

fn marker_json() -> String { serde_json::json!({"format":FORMAT,"version":1}).to_string() }

/// The payload alone is not the marker's identity. Call this before omitting a SQL row
/// whose allocation_json returned None, so real or malformed ownership is never hidden.
pub(super) fn validate_marker(job: &str, attempt: u32, agent: &str, execution: Option<&str>, until: i64, transferred: bool, released: bool) -> anyhow::Result<()> {
    anyhow::ensure!(job == MARKER_JOB && attempt == 1 && agent == MARKER_AGENT && execution.is_none() && until == 0 && !transferred && !released, Refusal::FormatRequiresDrain);
    Ok(())
}

pub(super) fn journal_json(json: &str) -> anyhow::Result<Option<Journal>> {
    anyhow::ensure!(json.len() <= BYTES, "U01 allocation exceeds its byte budget");
    let value: serde_json::Value = serde_json::from_str(json)?;
    if value.get("format").is_none() { return Ok(None); }
    let journal: Journal = serde_json::from_value(value)?;
    journal.validate()?;
    Ok(Some(journal))
}

pub(super) fn allocation_json(json: &str) -> anyhow::Result<Option<Allocation>> {
    anyhow::ensure!(json.len() <= BYTES, "U01 allocation exceeds its byte budget");
    let value: serde_json::Value = serde_json::from_str(json)?;
    if value == serde_json::json!({"format":FORMAT,"version":1}) { return Ok(None); }
    if value.get("format").is_some() {
        let journal: Journal = serde_json::from_value(value)?;
        journal.validate()?;
        return Ok(Some(journal.allocation));
    }
    Ok(Some(serde_json::from_value(value)?))
}

fn ensure_marker(c: &crate::sql::Connection<'_>, install: bool) -> anyhow::Result<()> {
    let row: Option<(u32,String,Option<String>,i64,String,bool,bool)> = c.query_row("SELECT attempt,agent,execution,until_at,allocation,transferred,released FROM ci_retained_ownership WHERE job=?1", [MARKER_JOB], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
    if let Some((attempt,agent,execution,until,json,transferred,released)) = row {
        let count: usize = c.query_row("SELECT count(*) FROM ci_retained_ownership WHERE job=?1",[MARKER_JOB],|r|r.get(0))?;
        anyhow::ensure!(count==1,Refusal::FormatRequiresDrain);
        validate_marker(MARKER_JOB,attempt,&agent,execution.as_deref(),until,transferred,released)?;
        anyhow::ensure!(serde_json::from_str::<serde_json::Value>(&json).map_err(|_|Refusal::FormatRequiresDrain)? == serde_json::json!({"format":FORMAT,"version":1}), Refusal::FormatRequiresDrain);
        return Ok(());
    }
    anyhow::ensure!(install, Refusal::FormatRequiresDrain);
    // A remaining domain or envelope is prior format evidence, not fresh authority.
    let prior: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM kv WHERE k LIKE 'u01/ordinary-domain/%' OR k LIKE 'u01/ordinary-anchor/%') OR EXISTS(SELECT 1 FROM ci_retained_ownership WHERE json_extract(allocation,'$.format')=?1)", [FORMAT], |r| r.get(0))?;
    anyhow::ensure!(!prior, Refusal::FormatRequiresDrain);
    let real_job: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id=?1)",[MARKER_JOB],|r|r.get(0))?;
    let count: usize = c.query_row("SELECT count(*) FROM ci_retained_ownership",[],|r|r.get(0))?;
    anyhow::ensure!(!real_job && count < LIMIT, "U01 ordinary marker requires one reserved ownership slot");
    c.execute("INSERT INTO ci_retained_ownership(job,attempt,agent,execution,until_at,allocation,transferred,released,received_at) VALUES(?1,1,?2,NULL,0,?3,0,0,0)",params![MARKER_JOB,MARKER_AGENT,marker_json()])?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Domain { version: u8, host: String, epoch: String, through: u64, accepted_above: BTreeSet<u64> }
impl Domain {
    fn validate(&self,host: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1 && self.host == host && valid_text(host,256)
            && !self.epoch.is_empty() && self.epoch.len() <= 128 && self.epoch.bytes().all(|b|b.is_ascii_alphanumeric() || b == b'-')
            && self.accepted_above.len() <= LIMIT && self.accepted_above.iter().all(|seq| *seq > self.through && seq.saturating_sub(self.through) <= LIMIT as u64), Refusal::FormatRequiresDrain);
        Ok(())
    }
    fn ack(&self) -> OrdinaryFrontierAckV1 { OrdinaryFrontierAckV1 { kind:"ordinary_frontier".into(),version:1,epoch:self.epoch.clone(),through:self.through } }
}
fn valid_text(s: &str, max: usize) -> bool { !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control) }
fn domain_key(host: &str) -> String { format!("{DOMAIN_PREFIX}{}",crate::util::sha256_hex(host.as_bytes())) }
fn anchor_key(host: &str) -> String { format!("{ANCHOR_PREFIX}{}",crate::util::sha256_hex(host.as_bytes())) }
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Anchor {version:u8,host:String,epoch:String}
fn domain_in(c: &crate::sql::Connection<'_>,host: &str) -> anyhow::Result<Option<Domain>> {
    let json: Option<String> = c.query_row("SELECT v FROM kv WHERE k=?1",[domain_key(host)],|r|r.get(0)).optional()?;
    let anchor: Option<String> = c.query_row("SELECT v FROM kv WHERE k=?1",[anchor_key(host)],|r|r.get(0)).optional()?;
    anyhow::ensure!(json.is_some()==anchor.is_some(),Refusal::EpochMissing);
    json.map(|json| {
        anyhow::ensure!(json.len() <= 131072,Refusal::FormatRequiresDrain);
        let domain: Domain = serde_json::from_str(&json).map_err(|_|Refusal::FormatRequiresDrain)?; domain.validate(host)?;
        let anchor=anchor.ok_or(Refusal::EpochMissing)?;
        anyhow::ensure!(anchor.len()<=1024,Refusal::FormatRequiresDrain);
        let anchor:Anchor=serde_json::from_str(&anchor).map_err(|_|Refusal::FormatRequiresDrain)?;
        anyhow::ensure!(anchor.version==1 && anchor.host==host && anchor.epoch==domain.epoch,Refusal::EpochMismatch);
        Ok(domain)
    }).transpose()
}
fn save_domain(c: &crate::sql::Connection<'_>,domain: &Domain) -> anyhow::Result<()> {
    domain.validate(&domain.host)?;
    let json=serde_json::to_string(domain)?;
    anyhow::ensure!(json.len() <= 131072,Refusal::FormatRequiresDrain);
    c.execute("INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",params![domain_key(&domain.host),json])?;
    Ok(())
}

struct Row { journal: Journal, transferred: bool, released: bool }
fn row_in(c: &crate::sql::Connection<'_>,job: &str,attempt: u32) -> anyhow::Result<Option<Row>> {
    let row: Option<(String,Option<String>,String,bool,bool)> = c.query_row("SELECT agent,execution,allocation,transferred,released FROM ci_retained_ownership WHERE job=?1 AND attempt=?2",params![job,attempt],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((agent,execution,json,transferred,released))=row else {return Ok(None)};
    let Some(journal)=journal_json(&json).map_err(|_|Refusal::FormatRequiresDrain)? else {return Ok(None)};
    anyhow::ensure!(journal.owner.job==job && journal.owner.attempt==attempt && journal.owner.host==agent && execution.as_deref()==Some(journal.owner.execution.as_str())
        && released==(journal.physical!="pending") && (journal.logical!="running" || !transferred)
        && (journal.logical=="running" || journal.physical!="pending" || transferred),Refusal::FormatRequiresDrain);
    Ok(Some(Row{journal,transferred,released}))
}
fn save_row(c: &crate::sql::Connection<'_>,row: &Row) -> anyhow::Result<()> {
    row.journal.validate()?;
    let json=serde_json::to_string(&row.journal)?;
    anyhow::ensure!(json.len()<=BYTES,"U01 ordinary allocation exceeds its byte budget");
    let changed=c.execute("UPDATE ci_retained_ownership SET allocation=?3,transferred=?4,released=?5,received_at=?6 WHERE job=?1 AND attempt=?2 AND agent=?7 AND execution=?8",
        params![row.journal.owner.job,row.journal.owner.attempt,json,row.transferred,row.released,crate::util::now_unix(),row.journal.owner.host,row.journal.owner.execution])?;
    anyhow::ensure!(changed==1,"U01 original ordinary owner disappeared");
    Ok(())
}

/// U05: read the SAME validated durable owner for log registration/publication.
/// This constructs no grant and does not promote a syntactic owner payload to authority.
pub(super) fn log_owner(c:&crate::sql::Connection<'_>,job:&str,attempt:u32)->anyhow::Result<Option<OrdinaryOwnerV1>> {
    Ok(log_registration(c,job,attempt)?.map(|(owner,_)|owner))
}

pub(super) fn log_registration(c:&crate::sql::Connection<'_>,job:&str,attempt:u32)->anyhow::Result<Option<(OrdinaryOwnerV1,Option<kiln_ci::wire::JobLogContractV1>)>> {
    ensure_marker(c,false)?;
    let Some(row)=row_in(c,job,attempt)? else{return Ok(None)};
    let domain=domain_in(c,&row.journal.owner.host)?.ok_or(Refusal::EpochMissing)?;
    anyhow::ensure!(domain.epoch==row.journal.owner.publication_epoch,Refusal::EpochMismatch);
    Ok(Some((row.journal.owner,row.journal.log_contract)))
}

pub(super) fn log_no_start(c:&crate::sql::Connection<'_>,owner:&OrdinaryOwnerV1)->anyhow::Result<bool> {
    if log_owner(c,&owner.job,owner.attempt)?.as_ref()!=Some(owner) {return Ok(false)}
    Ok(row_in(c,&owner.job,owner.attempt)?.is_some_and(|row|row.released && row.journal.physical=="no_start"))
}

/// Called only inside the writer callback BEFORE any write/effect is attempted.
/// Only this module's closed typed refusal is an owned, definite rejection; raw SQL,
/// session and commit errors are never classified by their text or outer error type.
fn before_effect<T>(result: anyhow::Result<T>) -> anyhow::Result<Result<T,Refusal>> {
    match result {
        Ok(value)=>Ok(Ok(value)),
        Err(error)=>match error.downcast::<Refusal>() {Ok(refusal)=>Ok(Err(refusal)),Err(error)=>Err(error)},
    }
}

impl Dispatch {
    pub(super) async fn ordinary_domain(&self,host: &str) -> anyhow::Result<Option<String>> {
        anyhow::ensure!(valid_text(host,256),"U01 invalid ordinary host");
        let host=host.to_owned();
        self.hub.store.call(move|c| {
            let domain=domain_in(c,&host)?;
            if domain.is_some() {ensure_marker(c,false)?;}
            Ok(domain.map(|d|d.epoch))
        }).await
    }

    pub(super) async fn ordinary_record(&self,job: &str,attempt: u32) -> anyhow::Result<Option<Journal>> {
        let job=job.to_owned();
        self.hub.store.call(move|c| {
            let row=row_in(c,&job,attempt)?;
            if row.is_some() {ensure_marker(c,false)?;}
            Ok(row.map(|r|r.journal))
        }).await
    }

    pub(super) async fn claim_ordinary(&self,owner: OrdinaryOwnerV1,allocation: Allocation,recorded: &str,log_contract:Option<kiln_ci::wire::JobLogContractV1>) -> anyhow::Result<Result<(),ClaimRefusal>> {
        anyhow::ensure!(self.admission.ready,"U01 ordinary ownership admission is fenced");
        let journal=Journal{format:FORMAT.into(),version:1,owner,allocation,recorded:recorded.into(),physical:"pending".into(),logical:"running".into(),log_contract};
        journal.validate()?;
        let encoded=serde_json::to_string(&journal)?;
        self.hub.store.call(move|c| {
            // SAME serialized writer callback: every eligibility/budget read precedes
            // even the explicit nested transaction. An inner refusal attempts no write.
            ensure_marker(c,false)?;
            let domain=domain_in(c,&journal.owner.host)?.ok_or(Refusal::EpochMissing)?;
            anyhow::ensure!(domain.epoch==journal.owner.publication_epoch,Refusal::EpochMismatch);
            let previous=row_in(c,&journal.owner.job,journal.owner.attempt)?;
            if let Some(previous)=&previous
                && previous.journal.owner==journal.owner {
                anyhow::ensure!(previous.journal.allocation==journal.allocation && previous.journal.recorded==journal.recorded && previous.journal.log_contract==journal.log_contract
                    && previous.journal.logical=="running" && previous.journal.physical=="pending","U01 ordinary claim cannot replace original ownership");
                let expected=journal.log_contract.clone().map(|contract|kiln_ci::wire::JobLogBindingV1 {contract,owner:journal.owner.clone()});
                anyhow::ensure!(crate::logs::accepted::producer::registered(c,&journal.owner.job,journal.owner.attempt,&journal.owner.host)?==expected,"original ordinary claim cannot add or drop its producer contract");
                return Ok(Ok(()));
            }
            if let Some(previous)=&previous {
                anyhow::ensure!(previous.released && previous.journal.physical=="no_start" && previous.journal.logical=="running" && !previous.transferred,
                    "U01 ordinary claim cannot replace unresolved or launched debt");
            }
            if journal.log_contract.is_none() && crate::logs::accepted::producer::requires_contract(c,&journal.owner.job,journal.owner.attempt)? {
                return Ok(Err(ClaimRefusal::LogContractRequired));
            }
            let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE job=?1 AND attempt=?2)",params![journal.owner.job,journal.owner.attempt],|r|r.get(0))?;
            anyhow::ensure!(previous.is_some() || !exists,"U01 ordinary claim cannot replace a different existing ownership format");
            let count:usize=c.query_row("SELECT count(*) FROM ci_retained_ownership",[],|r|r.get(0))?;
            anyhow::ensure!(count<=LIMIT,"U01 ordinary ownership exceeds its journal bound");
            let status: Option<(String,u32,bool)> = c.query_row("SELECT status,attempt,job_id IS NULL FROM jobs WHERE id=?1",[&journal.owner.job],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let (status,attempt,legacy)=status.ok_or_else(||anyhow::anyhow!("U01 ordinary claim has no original job"))?;
            anyhow::ensure!(!legacy && (if previous.is_some() {status=="queued"} else {matches!(status.as_str(),"queued"|"pending"|"waiting")}) && attempt.max(1)==journal.owner.attempt,"U01 ordinary claim lost its queued attempt");
            // Leave room for the longest positive physical/logical state. Existing or
            // unresolved ownership above never becomes a refundable size/budget refusal.
            if encoded.len()>BYTES-16 {return Ok(Err(ClaimRefusal::TooLarge));}
            if previous.is_none() && count==LIMIT {return Ok(Err(ClaimRefusal::Full));}
            let tx=crate::store::tx(c)?;
            if let Some(previous)=previous {
                // Its original publication frontier or never-exposed Commands disposition
                // already excludes this exact old grant. The queued row proves the old
                // scheduler rejection settled before reusing this attempt's journal key.
                let changed=tx.execute("DELETE FROM ci_retained_ownership WHERE job=?1 AND attempt=?2 AND allocation=?3 AND released=1 AND transferred=0",
                    params![previous.journal.owner.job,previous.journal.owner.attempt,serde_json::to_string(&previous.journal)?])?;
                anyhow::ensure!(changed==1,"U01 previous no-start owner changed before replacement");
            }
            runs::claim_in(&tx,&journal.owner.job,&journal.owner.host,&journal.recorded,journal.owner.attempt,crate::util::now_unix())?;
            tx.execute("INSERT INTO ci_retained_ownership(job,attempt,agent,execution,until_at,allocation,transferred,released,received_at) VALUES(?1,?2,?3,?4,0,?5,0,0,?6)",
                params![journal.owner.job,journal.owner.attempt,journal.owner.host,journal.owner.execution,encoded,crate::util::now_unix()])?;
            if let Some(contract)=&journal.log_contract {
                let binding=kiln_ci::wire::JobLogBindingV1 {contract:contract.clone(),owner:journal.owner.clone()};
                let order:i64=tx.query_row("SELECT rowid FROM jobs WHERE id=?1 AND attempt=?2 AND agent=?3",params![journal.owner.job,journal.owner.attempt,journal.owner.host],|r|r.get(0))?;
                crate::logs::accepted::producer::register(&tx,&binding,order)?;
            }
            tx.commit()?;
            Ok(Ok(()))
        }).await
    }

    /// Caller must FIRST invalidate its exact existing Commands never-exposed token.
    /// This is not a timeout, generic Reject or observation of a missing pending item.
    /// The borrowed writer rejects any already terminal/transferred owner.
    pub(super) async fn refuse_unexposed(&self,owner: &OrdinaryOwnerV1) -> anyhow::Result<()> {
        owner.validate().map_err(anyhow::Error::msg)?;
        let owner=owner.clone();
        self.hub.store.call(move|c| {
            let tx=crate::store::tx(c)?;ensure_marker(&tx,false)?;
            let mut row=row_in(&tx,&owner.job,owner.attempt)?.ok_or_else(||anyhow::anyhow!("U01 never-exposed refusal has no original intent"))?;
            anyhow::ensure!(row.journal.owner==owner && row.journal.logical=="running" && !row.transferred
                && matches!(row.journal.physical.as_str(),"pending"|"no_start"),"U01 never-exposed refusal cannot replace launched or terminal ownership");
            if row.journal.physical=="no_start" {crate::logs::accepted::producer::no_start(&tx,&row.journal.owner)?;tx.commit()?;return Ok(());}
            row.journal.physical="no_start".into();row.released=true;
            save_row(&tx,&row)?;
            crate::logs::accepted::producer::no_start(&tx,&row.journal.owner)?;
            tx.commit()?;Ok(())
        }).await
    }

    /// Returns a physical reservation only after SAME-writer attempt termination committed.
    /// A staged completion wins loss arbitration before either logical phase/debt can change.
    pub(super) async fn terminal_ordinary(&mut self,job: &str,attempt: u32,end: runs::AttemptEnd,losing: bool) -> anyhow::Result<Option<(bool,Option<Allocation>)>> {
        let job=job.to_owned(); let labels=(end.conclusion,end.class);
        let result=self.hub.store.call(move|c| {
            let tx=crate::store::tx(c)?;
            let Some(mut row)=row_in(&tx,&job,attempt)? else {return Ok(None)};
            ensure_marker(&tx,false)?;
            if row.journal.logical!="running" {
                let won=(row.journal.logical=="lost")==losing;
                return Ok(Some((won,if won && !row.released {Some(row.journal.allocation.clone())} else {None},false)));
            }
            let attempt_exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_attempts WHERE leg_id=?1 AND number=?2 AND runner=?3 AND status='running')",params![job,attempt,row.journal.owner.host],|r|r.get(0))?;
            anyhow::ensure!(attempt_exists,"U01 ordinary logical transition lost its original running attempt");
            if !runs::attempt_end_in(&tx,&job,attempt,&end,losing,crate::util::now_unix())? {return Ok(Some((false,None,false)));}
            row.journal.logical=if losing {"lost"} else {"terminal"}.into();
            row.transferred = !row.released;
            save_row(&tx,&row)?;
            tx.commit()?;
            Ok(Some((true,(!row.released).then(||row.journal.allocation.clone()),true)))
        }).await;
        if result.is_err() {self.admission.ready=false;}
        let result=result?;
        if let Some((won,reservation,fresh))=result {
            if fresh {runs::attempt_metric(labels.0,labels.1);}
            if let Some(allocation)=&reservation {self.admission.retained.insert(allocation.id.clone());}
            return Ok(Some((won,reservation)));
        }
        Ok(None)
    }

    pub(super) async fn ordinary_event(&mut self,host: &str,actual_nonce: &str,event: JobEventV2) -> anyhow::Result<Option<OrdinaryFrontierAckV1>> {
        anyhow::ensure!(valid_text(host,256) && valid_text(actual_nonce,128),"U01 ordinary event lacks authenticated producer");
        match event {
            JobEventV2::OrdinaryDomain{epoch,process_nonce} => {
                anyhow::ensure!(process_nonce==actual_nonce && valid_text(&epoch,128) && epoch.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'-'),"U01 ordinary domain declaration changed its authenticated process");
                let host=host.to_owned();
                self.hub.store.call(move|c| {
                    let tx=crate::store::tx(c)?; ensure_marker(&tx,true)?;
                    let domain=match domain_in(&tx,&host)? {
                        Some(domain)=>{anyhow::ensure!(domain.epoch==epoch,Refusal::EpochMismatch);domain},
                        None=>{
                            let prior:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE agent=?1 AND json_extract(allocation,'$.format')=?2)",params![host,FORMAT],|r|r.get(0))?;
                            anyhow::ensure!(!prior,Refusal::EpochMissing);
                            let count:usize=tx.query_row("SELECT count(*) FROM kv WHERE k LIKE 'u01/ordinary-domain/%'",[],|r|r.get(0))?;
                            anyhow::ensure!(count<LIMIT,"U01 ordinary producer domain budget exhausted");
                            let anchor=Anchor{version:1,host:host.clone(),epoch:epoch.clone()};
                            tx.execute("INSERT INTO kv(k,v) VALUES(?1,?2)",params![anchor_key(&host),serde_json::to_string(&anchor)?])?;
                            Domain{version:1,host,epoch,through:0,accepted_above:BTreeSet::new()}
                        }
                    };
                    save_domain(&tx,&domain)?;tx.commit()?;Ok(())
                }).await?;
                Ok(None)
            }
            JobEventV2::OrdinaryExecutionPublication{publication} => self.ordinary_publication(host,actual_nonce,publication).await.map(Some),
            _=>Ok(None),
        }
    }

    async fn ordinary_publication(&mut self,host: &str,actual_nonce: &str,publication: OrdinaryPublicationV1) -> anyhow::Result<OrdinaryFrontierAckV1> {
        publication.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(publication.owner.host==host && (publication.recovery || publication.owner.process_nonce==actual_nonce),"U01 ordinary proof changed its authenticated host/process");
        let host=host.to_owned();
        let result=self.hub.store.call(move|c| {
            // The existing Store callback is one atomic writer session. All classified
            // refusals below occur BEFORE even the nested write transaction starts.
            if let Err(refusal)=before_effect(ensure_marker(c,false))? {return Ok(Err(refusal));}
            let mut domain=match before_effect(domain_in(c,&host))? {
                Ok(Some(domain))=>domain,
                Ok(None)=>return Ok(Err(Refusal::EpochMissing)),
                Err(refusal)=>return Ok(Err(refusal)),
            };
            if domain.epoch!=publication.epoch {return Ok(Err(Refusal::EpochMismatch));}
            if publication.seq<=domain.through || domain.accepted_above.contains(&publication.seq) {
                let remembered=match before_effect(row_in(c,&publication.owner.job,publication.owner.attempt))? {
                    Ok(row)=>row.filter(|r|r.journal.owner==publication.owner && r.released).map(|r|(r.journal,r.transferred)),
                    Err(refusal)=>return Ok(Err(refusal)),
                };
                return Ok(Ok((domain.ack(),remembered)));
            }
            if publication.seq.saturating_sub(domain.through)>LIMIT as u64 || domain.accepted_above.len()>=LIMIT {return Ok(Err(Refusal::FrontierFull));}
            let mut row=match before_effect(row_in(c,&publication.owner.job,publication.owner.attempt))? {
                Ok(Some(row))=>row,
                Ok(None)=>return Ok(Err(Refusal::IntentMissing)),
                Err(refusal)=>return Ok(Err(refusal)),
            };
            if row.journal.owner!=publication.owner {return Ok(Err(Refusal::OwnerChanged));}
            let physical=match publication.phase {OrdinaryPhaseV1::Stopped=>"stopped",OrdinaryPhaseV1::NoStart=>"no_start"};
            if row.journal.physical!="pending" && row.journal.physical!=physical {return Ok(Err(Refusal::PhaseConflict));}
            let release_debt=row.transferred && !row.released;
            // No refusal classification is permitted after this point. Any SQL/commit
            // error is an outer error and fences admission, preserving original custody.
            let tx=crate::store::tx(c)?;
            row.journal.physical=physical.into();row.released=true;
            save_row(&tx,&row)?;
            if row.journal.physical=="no_start" {crate::logs::accepted::producer::no_start(&tx,&row.journal.owner)?;}
            tx.execute("UPDATE jobs SET kept_until=NULL WHERE id=?1 AND agent=?2 AND attempt=?3",params![row.journal.owner.job,row.journal.owner.host,row.journal.owner.attempt])?;
            domain.accepted_above.insert(publication.seq);
            while let Some(next)=domain.through.checked_add(1) {
                if !domain.accepted_above.remove(&next) {break;} domain.through=next;
            }
            save_domain(&tx,&domain)?;
            tx.commit()?;
            Ok(Ok((domain.ack(),Some((row.journal,release_debt)))))
        }).await;
        let (ack,effect)=match result {
            Ok(Ok(applied))=>applied,
            Ok(Err(refusal))=>return Err(refusal.into()),
            Err(error)=>{self.admission.ready=false;return Err(error);}
        };
        if let Some((journal,release_debt))=effect {
            self.next.extend(self.sched.handle(Input::OrdinaryExecutionReleased{leg:journal.owner.job.clone(),attempt:journal.owner.attempt,runner:journal.owner.host.clone()},now_ms()));
            if release_debt {
                self.admission.retained.remove(&journal.allocation.id);
                let commands=self.sched.handle(Input::AttemptFinished{leg:journal.allocation.id.clone(),attempt:1,outcome:Outcome::Success},now_ms());
                self.next.extend(commands);
            }
            if journal.physical=="no_start" && journal.logical=="running" {
                self.hub.ci.send(super::Msg::OrdinaryRejected{owner:journal.owner.clone()});
            }
            self.refresh_retained_owner(&journal.owner.job,&journal.owner.host).await?;
            self.prune_released().await?;
        }
        Ok(ack)
    }

    /// Legacy debug messages can extend UI retention, but cannot stand in for the strict
    /// positive ordinary physical publication or replace its envelope with raw Allocation.
    pub(super) async fn ordinary_kept(&mut self,agent: &str,job: &str,event: JobEventV2) -> anyhow::Result<bool> {
        let (execution,attempt,until,release)=match event {
            JobEventV2::KeptOwned{execution,attempt,until}=>(execution,attempt,until,false),
            JobEventV2::KeptReleased{execution,attempt}=>(execution,attempt,0,true),
            JobEventV2::Kept{..}=>{
                let (job,agent)=(job.to_owned(),agent.to_owned());
                let ordinary:bool=self.hub.store.call(move|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_retained_ownership WHERE job=?1 AND agent=?2 AND json_extract(allocation,'$.format')=?3)",params![job,agent,FORMAT],|r|r.get(0))?)).await?;
                anyhow::ensure!(!ordinary,"U01 ordinary debug retention requires its exact execution/attempt");
                return Ok(false);
            }
            _=>return Ok(false),
        };
        let Some(journal)=self.ordinary_record(job,attempt).await? else {return Ok(false)};
        anyhow::ensure!(journal.owner.host==agent && journal.owner.execution==execution,"U01 ordinary debug event changed its original owner");
        anyhow::ensure!(!release,"U01 ordinary physical retirement requires its sequenced Gate publication");
        if journal.physical!="pending" {return Ok(true);}
        let (job,agent)=(job.to_owned(),agent.to_owned());
        let ui_job=job.clone();let ui_agent=agent.clone();
        self.hub.store.call(move|c| {
            let tx=crate::store::tx(c)?;ensure_marker(&tx,false)?;
            let row=row_in(&tx,&job,attempt)?.ok_or_else(||anyhow::anyhow!("U01 ordinary debug owner disappeared"))?;
            anyhow::ensure!(row.journal.owner==journal.owner && !row.released,"U01 ordinary debug phase changed");
            tx.execute("UPDATE ci_retained_ownership SET until_at=?3 WHERE job=?1 AND attempt=?2",params![job,attempt,until])?;
            tx.execute("UPDATE jobs SET kept_until=?4 WHERE id=?1 AND agent=?2 AND attempt=?3",params![job,agent,attempt,until])?;
            tx.commit()?;Ok(())
        }).await?;
        self.next.extend(self.sched.handle(Input::RetainExecution{leg:ui_job.clone(),attempt},now_ms()));
        self.hub.ci.keep(&ui_job,Some((ui_agent,until)));
        Ok(true)
    }
}

/// Called by the existing retained transfer oracle; no extra test driver/controller.
#[cfg(test)]
pub(super) async fn exercise_oracle(d: &mut Dispatch) -> anyhow::Result<()> {
    use kiln_ci::sched::state::{LegIn,Resources,RunnerIn};
    use kiln_ci::sched::retry::{Failed,FailureClass};
    use kiln_ci::wire::OrdinaryCommandV1;
    if !d.admission.ready {d.restore_admission().await?;}
    d.hub.store.call(|c| {
        c.execute("INSERT INTO repos(id,name,clone_url,created_at) VALUES('ordinary-repo','test/ordinary','local',0)",[])?;
        c.execute("INSERT INTO runs(id,repo_id,number,pipeline,event,branch,sha,status,created_at,attempt) VALUES('ordinary-run','ordinary-repo',1,'ci','manual','main','sha','running',0,1)",[])?;
        for job in ["ordinary-proof-first","ordinary-loss-first","ordinary-completion","ordinary-refused"] {
            c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,job_id,attempt) VALUES(?1,'ordinary-run',?1,?1,'queued','[]','[]','{}','a',1)",[job])?;
        }
        Ok(())
    }).await?;
    let declaration=|nonce: &str|JobEventV2::OrdinaryDomain{epoch:"epoch-oracle".into(),process_nonce:nonce.into()};
    d.ordinary_event("ordinary-host","process-one",declaration("process-one")).await?;
    let owner=|job:&str,n|OrdinaryOwnerV1{version:1,job:job.into(),attempt:1,host:"ordinary-host".into(),process_nonce:"process-one".into(),
        publication_epoch:"epoch-oracle".into(),connection:"a".repeat(64),grant:format!("grant-oracle-{n}"),execution:format!("ci-oracle-{n}"),allocation:format!("retained-oracle-{n}"),
        command:OrdinaryCommandV1{stream:"l:7.1".into(),seq:n}};
    let publication=|seq,owner:OrdinaryOwnerV1,phase|OrdinaryPublicationV1{version:1,epoch:"epoch-oracle".into(),seq,owner,phase,recovery:true};
    let end=|losing|runs::AttemptEnd{conclusion:if losing {"failure"} else {"success"},class:losing.then_some("runner-lost"),exit_code:None,error:None,peak_rss:None,cpu_seconds:None};
    let lost=||Outcome::Failed(Failed{class:FailureClass::RunnerLost,exit_code:None,log_hits:vec![]});
    let owned:Vec<_>=["ordinary-proof-first","ordinary-loss-first","ordinary-completion","ordinary-refused"].into_iter().zip(1..).map(|(job,n)|owner(job,n)).collect();
    d.sched.handle(Input::RunnerUp(Box::new(RunnerIn{id:"ordinary-host".into(),slots:4,..RunnerIn::default()})),0);
    for owner in &owned {
        d.sched.handle(Input::LegRunning{leg:Box::new(LegIn{id:owner.job.clone(),run:"ordinary-run".into(),repo:"test/ordinary".into(),request:Resources{cpu:2.0,mem:20,..Resources::default()},..LegIn::default()}),runner:owner.host.clone(),attempt:1},1);
        let allocation=d.sched.allocation(&owner.job,1,owner.allocation.clone()).ok_or_else(||anyhow::anyhow!("ordinary oracle original allocation"))?;
        d.claim_ordinary(owner.clone(),allocation,"host",None).await??;
    }
    let first=owned.first().cloned().ok_or_else(||anyhow::anyhow!("ordinary oracle first owner"))?;
    let second=owned.get(1).cloned().ok_or_else(||anyhow::anyhow!("ordinary oracle second owner"))?;
    let third=owned.get(2).cloned().ok_or_else(||anyhow::anyhow!("ordinary oracle third owner"))?;
    let fourth=owned.get(3).cloned().ok_or_else(||anyhow::anyhow!("ordinary oracle fourth owner"))?;
    assert!(d.ordinary_kept("ordinary-host",&first.job,JobEventV2::KeptOwned{execution:first.execution.clone(),attempt:1,until:7}).await?);
    assert!(d.hub.ci.kept_by(&first.job,"ordinary-host"));
    assert_eq!(d.ordinary_record(&first.job,1).await?.map(|j|j.owner),Some(first.clone()),"UI debug retention preserves the original ordinary envelope");
    // Overlapping authenticated process registrations share a publication domain, never a
    // last-registered-nonce authority that substitutes one process for another.
    d.ordinary_event("ordinary-host","process-two",declaration("process-two")).await?;
    let first_proof=publication(2,first.clone(),OrdinaryPhaseV1::Stopped);
    let ack=d.ordinary_event("ordinary-host","process-one",JobEventV2::OrdinaryExecutionPublication{publication:first_proof.clone()}).await?.ok_or_else(||anyhow::anyhow!("ordinary oracle prefix ACK"))?;
    assert_eq!(ack.through,0,"out-of-order acceptance cannot skip the missing publication");
    let duplicate=d.ordinary_event("ordinary-host","process-one",JobEventV2::OrdinaryExecutionPublication{publication:first_proof}).await?.ok_or_else(||anyhow::anyhow!("ordinary duplicate ACK"))?;
    assert_eq!(duplicate.through,0,"accepted-above duplicate has no second effect");
    assert!(!d.hub.ci.kept_by(&first.job,"ordinary-host"));
    assert!(d.sched.standing(&first.job).is_some(),"physical proof does not by itself finish logical work");
    assert!(d.sched.allocation(&first.job,1,first.allocation.clone()).is_none(),"positive physical release cannot mint another allocation while logical work waits");
    let (won,reservation)=d.terminal_ordinary(&first.job,1,end(true),true).await?.ok_or_else(||anyhow::anyhow!("ordinary oracle first journal"))?;
    assert!(won && reservation.is_none(),"proof before loss must not invent later physical debt");
    d.sched.handle(Input::AttemptFinished{leg:first.job.clone(),attempt:1,outcome:lost()},2);
    assert!(!d.admission.retained.contains(&first.allocation));
    let (won,reservation)=d.terminal_ordinary(&second.job,1,end(true),true).await?.ok_or_else(||anyhow::anyhow!("ordinary oracle second journal"))?;
    assert!(won);
    let debt=reservation.ok_or_else(||anyhow::anyhow!("ordinary oracle lost physical debt"))?;
    d.sched.handle(Input::RetainedFinished{leg:second.job.clone(),attempt:1,reservation:Box::new(debt.clone()),outcome:lost()},3);
    assert_eq!(d.sched.allocation(&debt.id,1,debt.id.clone()),Some(debt.clone()),"loss transfers the exact footprint once");
    let repeated=d.terminal_ordinary(&second.job,1,end(true),true).await?.ok_or_else(||anyhow::anyhow!("ordinary loss replay"))?;
    assert_eq!(repeated.1,Some(debt.clone()));
    let ack=d.ordinary_event("ordinary-host","process-two",JobEventV2::OrdinaryExecutionPublication{publication:publication(1,second.clone(),OrdinaryPhaseV1::Stopped)}).await?.ok_or_else(||anyhow::anyhow!("ordinary closed prefix"))?;
    assert_eq!(ack.through,2);
    assert!(d.sched.allocation(&debt.id,1,debt.id.clone()).is_none(),"only committed physical proof releases adopted debt");
    assert!(!d.admission.retained.contains(&debt.id));
    let result=crate::proto::JobResult{job:third.job.clone(),status:crate::store::Status::Succeeded,exit_code:Some(0),error:None,peak_rss:None,cpu_seconds:None};
    assert!(runs::stage_completion(&d.hub,"ordinary-host",&result,Some(1),None).await?.is_some());
    let before=d.ordinary_record(&third.job,1).await?.ok_or_else(||anyhow::anyhow!("ordinary completion owner"))?;
    let losing=d.terminal_ordinary(&third.job,1,end(true),true).await?.ok_or_else(||anyhow::anyhow!("ordinary completion arbitration"))?;
    assert!(!losing.0 && losing.1.is_none());
    assert_eq!(d.ordinary_record(&third.job,1).await?.map(|j|j.logical),Some(before.logical),"a staged completion wins before either logical/debt field changes");
    let (_,reservation)=d.terminal_ordinary(&third.job,1,end(false),false).await?.ok_or_else(||anyhow::anyhow!("ordinary completion finish"))?;
    let debt=reservation.ok_or_else(||anyhow::anyhow!("ordinary completed physical debt"))?;
    d.sched.handle(Input::RetainedFinished{leg:third.job.clone(),attempt:1,reservation:Box::new(debt),outcome:Outcome::Success},4);
    d.ordinary_event("ordinary-host","process-two",JobEventV2::OrdinaryExecutionPublication{publication:publication(3,third,OrdinaryPhaseV1::Stopped)}).await?;
    let no_start=publication(4,fourth.clone(),OrdinaryPhaseV1::NoStart);
    d.ordinary_event("ordinary-host","process-one",JobEventV2::OrdinaryExecutionPublication{publication:no_start.clone()}).await?;
    d.sched.handle(Input::Rejected{leg:fourth.job.clone()},5);
    runs::requeue(&d.hub,&fourth.job,1,None).await?;
    let next=owner(&fourth.job,5);
    let allocation=Allocation{id:next.allocation.clone(),runner:next.host.clone(),request:Resources{cpu:2.0,mem:20,..Resources::default()},mode:kiln_ci::sched::state::ReservationMode::Ordinary,
        repo:"test/ordinary".into(),actor:String::new(),lane:String::new(),parent_binding:None};
    d.claim_ordinary(next.clone(),allocation,"host",None).await??;
    d.ordinary_event("ordinary-host","process-one",JobEventV2::OrdinaryExecutionPublication{publication:no_start}).await?;
    assert_eq!(d.ordinary_record(&next.job,1).await?.map(|j|j.owner),Some(next.clone()),"old NoStart cannot reject or overwrite a same-attempt successor");
    assert!(d.ordinary_kept("ordinary-host",&next.job,JobEventV2::KeptReleased{execution:next.execution.clone(),attempt:1}).await.is_err());
    assert!(d.admission.ready);
    let future=publication(LIMIT as u64+5+1,next.clone(),OrdinaryPhaseV1::Stopped);
    assert!(d.ordinary_event("ordinary-host","process-one",JobEventV2::OrdinaryExecutionPublication{publication:future}).await.is_err(),"future holes cause bounded refusal");
    assert!(d.admission.ready,"known pre-write frontier refusal must leave unrelated placement usable");
    let before=d.ordinary_record(&next.job,1).await?.ok_or_else(||anyhow::anyhow!("ordinary successor owner"))?;
    assert_eq!(before.physical,"pending");
    // Actual shared journal bound, including its permanent marker: a definite Full
    // neither starts the queued attempt nor changes existing launched/debt authority.
    let budget=owner("ordinary-budget",6);
    let mut budget_allocation=before.allocation.clone(); budget_allocation.id=budget.allocation.clone();
    d.hub.store.call(|c| {
        c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,job_id,attempt) VALUES('ordinary-budget','ordinary-run','ordinary-budget','ordinary-budget','queued','[]','[]','{}','a',1)",[])?;Ok(())
    }).await?;
    let present=d.hub.store.call(|c|Ok(c.query_row("SELECT count(*) FROM ci_retained_ownership",[],|r|r.get::<_,usize>(0))?)).await?;
    for page in (present..LIMIT).step_by(128) {
        let until=(page+128).min(LIMIT);
        let allocation=budget_allocation.clone();
        d.hub.store.call(move|c| {
            let tx=crate::store::tx(c)?;
            for n in page..until {
                let mut allocation=allocation.clone();allocation.id=format!("retained-budget-padding-{n}");
                tx.execute("INSERT INTO ci_retained_ownership(job,attempt,agent,execution,until_at,allocation,transferred,released,received_at) VALUES(?1,1,'ordinary-budget-padding',NULL,0,?2,0,0,0)",params![format!("ordinary-budget-padding-{n}"),serde_json::to_string(&allocation)?])?;
            }
            tx.commit()?;Ok(())
        }).await?;
    }
    let full_count=d.hub.store.call(|c|Ok(c.query_row("SELECT count(*) FROM ci_retained_ownership",[],|r|r.get::<_,usize>(0))?)).await?;
    assert_eq!(full_count,LIMIT);
    assert_eq!(d.claim_ordinary(budget.clone(),budget_allocation.clone(),"host",None).await?,Err(ClaimRefusal::Full));
    assert!(d.admission.ready,"known pre-write claim Full must leave unrelated placement usable");
    d.claim_ordinary(next.clone(),before.allocation.clone(),"host",None).await??;
    let mut changed=before.allocation.clone();changed.repo="x".repeat(BYTES);
    assert!(d.claim_ordinary(next.clone(),changed,"host",None).await.is_err(),"an existing original owner cannot become a refundable oversized new claim");
    d.hub.store.call(|c| {c.execute("DELETE FROM ci_retained_ownership WHERE agent='ordinary-budget-padding' AND job LIKE 'ordinary-budget-padding-%'",[])?;Ok(())}).await?;
    let mut too_large=budget_allocation;too_large.repo="x".repeat(BYTES);
    assert_eq!(d.claim_ordinary(budget.clone(),too_large,"host",None).await?,Err(ClaimRefusal::TooLarge));
    assert!(d.ordinary_record(&budget.job,1).await?.is_none());
    let untouched=d.hub.store.call(|c|Ok(c.query_row("SELECT status,(SELECT count(*) FROM ci_attempts WHERE leg_id='ordinary-budget') FROM jobs WHERE id='ordinary-budget'",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,usize>(1)?)))?)).await?;
    assert_eq!(untouched,("queued".into(),0),"definite Full/TooLarge attempt no original claim write");
    assert!(d.admission.ready);
    assert_eq!(d.ordinary_record(&next.job,1).await?.map(|j|j.owner),Some(next.clone()));
    d.ordinary_event("ordinary-host","process-one",JobEventV2::OrdinaryExecutionPublication{publication:publication(5,next.clone(),OrdinaryPhaseV1::Stopped)}).await?;
    d.terminal_ordinary(&next.job,1,end(false),false).await?;
    d.hub.store.call(|c| {c.execute("UPDATE jobs SET status='failed' WHERE run_id='ordinary-run'",[])?;Ok(())}).await?;
    d.prune_released().await?;
    let marker=d.hub.store.call(|c|Ok(c.query_row("SELECT allocation FROM ci_retained_ownership WHERE job=?1",[MARKER_JOB],|r|r.get::<_,String>(0))?)).await?;
    assert!(serde_json::from_str::<Allocation>(&marker).is_err(),"actual old Allocation decoder refuses even after every real new row compacts");
    assert!(allocation_json(&marker)?.is_none());
    assert!(validate_marker("a-real-job",1,MARKER_AGENT,None,0,false,false).is_err(),"marker payload alone cannot hide an actual allocation row");
    let count=d.hub.store.call(|c|Ok(c.query_row("SELECT count(*) FROM ci_retained_ownership WHERE job!=?1 AND json_extract(allocation,'$.format')=?2",params![MARKER_JOB,FORMAT],|r|r.get::<_,usize>(0))?)).await?;
    assert_eq!(count,0);
    let receipts=d.hub.store.call(|c|Ok(c.query_row("SELECT count(*) FROM ci_retained_receipts WHERE agent='ordinary-host'",[],|r|r.get::<_,usize>(0))?)).await?;
    assert_eq!(receipts,0,"ordinary proof does not grow lifetime legacy receipt rows");
    assert!(d.ordinary_event("ordinary-host","process-two",JobEventV2::OrdinaryDomain{epoch:"rotated-epoch".into(),process_nonce:"process-two".into()}).await.is_err());
    assert_eq!(d.ordinary_domain("ordinary-host").await?,Some("epoch-oracle".into()));
    let preserved=d.hub.store.call(|c| {
        let key=domain_key("ordinary-host");
        let value:String=c.query_row("SELECT v FROM kv WHERE k=?1",[&key],|r|r.get(0))?;
        c.execute("DELETE FROM kv WHERE k=?1",[key])?;Ok(value)
    }).await?;
    assert!(d.ordinary_event("ordinary-host","process-two",declaration("process-two")).await.is_err(),"a missing frontier with a retained anchor cannot reset an empty producer");
    d.hub.store.call(move|c| {c.execute("INSERT INTO kv(k,v) VALUES(?1,?2)",params![domain_key("ordinary-host"),preserved])?;Ok(())}).await?;
    assert_eq!(d.ordinary_domain("ordinary-host").await?,Some("epoch-oracle".into()));
    Ok(())
}
