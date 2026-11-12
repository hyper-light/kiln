//! Durable verified owner handover. Originals and attempt generations stay in agent_mail;
//! these records bind authenticated peers to one exact exported/imported manifest.

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use super::{TransferManifest, TransferRefusal as Refusal};
use crate::store::Store;

pub const TABLES: &[&str] = &["agent_mail_handoffs", "agent_mail", "agent_mail_transfers"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub peer: String,
    pub machine: String,
    #[serde(default)]
    pub instance: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Offered, Prepared, Committed, Aborted }

impl Phase {
    fn name(self) -> &'static str { match self { Self::Offered => "offered", Self::Prepared => "prepared", Self::Committed => "committed", Self::Aborted => "aborted" } }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub manifest: TransferManifest,
    pub source: Principal,
    pub destination: Principal,
    pub phase: Phase,
}

fn sql(error: rusqlite::Error) -> Refusal {
    match error { rusqlite::Error::SqliteFailure(error, _) if error.code == rusqlite::ErrorCode::DiskFull => Refusal::Capacity, _ => Refusal::Unavailable }
}

fn store(error: anyhow::Error) -> Refusal {
    error.downcast::<rusqlite::Error>().map_or(Refusal::Unavailable, sql)
}

fn integer(value: u64) -> Result<i64, Refusal> { i64::try_from(value).map_err(|_| Refusal::Capacity) }

fn principal(value: &Principal) -> bool {
    super::valid_identity(&value.machine) && value.instance.as_deref().is_none_or(super::valid_identity)
        && !value.peer.is_empty() && value.peer.len().is_multiple_of(2) && value.peer.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate(value: &Record) -> Result<(), Refusal> {
    let manifest=&value.manifest;
    if !super::valid_identity(&manifest.attempt) || !super::valid_identity(&manifest.session) || manifest.destination_epoch<=manifest.source_epoch
        || !principal(&value.source) || !principal(&value.destination)
        // SHA-256 is 256 bits, represented by two hexadecimal characters per byte.
        || manifest.sha256.len()!=256/8*2 || !manifest.sha256.bytes().all(|byte|byte.is_ascii_hexdigit()) { return Err(Refusal::Unsupported); }
    integer(manifest.source_epoch)?;integer(manifest.destination_epoch)?;
    Ok(())
}

fn json(record: &Record, phase: Phase) -> Result<String, Refusal> {
    #[derive(Serialize)]
    struct Borrowed<'a> { manifest:&'a TransferManifest,source:&'a Principal,destination:&'a Principal,phase:Phase }
    let bytes=crate::harness::state_view::encode(&Borrowed { manifest:&record.manifest,source:&record.source,destination:&record.destination,phase },crate::harness::state_view::FRAME).map_err(|error|match error { crate::harness::state_view::Error::Budget=>Refusal::Capacity,_=>Refusal::Unavailable })?;
    String::from_utf8(bytes).map_err(|_|Refusal::Unavailable)
}

fn read_on(c: &Connection, key: &str, latest: bool) -> Result<Option<Record>, Refusal> {
    let query=if latest {
        "SELECT CASE WHEN length(CAST(record AS BLOB))<=?2 THEN record END FROM agent_mail_handoffs WHERE session=?1 ORDER BY ordinal DESC LIMIT 1"
    } else {
        "SELECT CASE WHEN length(CAST(record AS BLOB))<=?2 THEN record END FROM agent_mail_handoffs WHERE attempt=?1"
    };
    let row:Option<Option<String>>=c.query_row(query,params![key,integer(u64::try_from(crate::harness::state_view::FRAME).map_err(|_|Refusal::Capacity)?)?],|row|row.get(0)).optional().map_err(sql)?;
    let record=match row { None=>return Ok(None),Some(Some(json))=>serde_json::from_str::<Record>(&json).map_err(|_|Refusal::Unavailable)?,Some(None)=>return Err(Refusal::Capacity) };
    validate(&record).map_err(|_|Refusal::Unavailable)?;
    if (if latest { record.manifest.session.as_str() } else { record.manifest.attempt.as_str() })!=key { return Err(Refusal::Unavailable); }
    Ok(Some(record))
}

pub async fn get(store: &Store, attempt: String) -> Result<Option<Record>, Refusal> {
    if !super::valid_identity(&attempt) { return Err(Refusal::Unsupported); }
    store.read(move|c|Ok(read_on(c,&attempt,false))).await.map_err(self::store)?
}

pub async fn latest(store: &Store, session: String) -> Result<Option<Record>, Refusal> {
    if !super::valid_identity(&session) { return Err(Refusal::Unsupported); }
    store.read(move|c|Ok(read_on(c,&session,true))).await.map_err(self::store)?
}

/// Actual mutation cardinality for the handler's SQLite preflight, independent of exported
/// package count. The caller holds the Offered/Prepared dispatch fence until adoption commits.
pub async fn affected(store: &Store, session: String, source_epoch: u64) -> Result<u64, Refusal> {
    if !super::valid_identity(&session) { return Err(Refusal::Unsupported); }
    let epoch=integer(source_epoch)?;
    store.read(move|c|Ok(affected_on(c,&session,epoch))).await.map_err(self::store)?
}

fn affected_on(c: &Connection, session: &str, epoch: i64) -> Result<u64, Refusal> {
    let count:i64=c.query_row("SELECT COUNT(*) FROM agent_mail WHERE recipient=?1 AND epoch=?2 AND generation>0 AND disposition IN ('waiting','dispatched','admitted')",params![session,epoch],|row|row.get(0)).map_err(sql)?;
    u64::try_from(count).map_err(|_|Refusal::Unavailable)
}

pub async fn offer(store: &Store, manifest: TransferManifest, source: Principal, destination: Principal) -> Result<Record, Refusal> {
    let record=Record { manifest,source,destination,phase:Phase::Offered };
    store.call(move|c|Ok(offer_on(c,&record))).await.map_err(self::store)?
}

fn offer_on(c: &mut Connection, candidate: &Record) -> Result<Record, Refusal> {
    validate(candidate)?;
    if candidate.phase!=Phase::Offered { return Err(Refusal::Unsupported); }
    let tx=c.savepoint().map_err(sql)?;
    if let Some(previous)=read_on(&tx,&candidate.manifest.attempt,false)? {
        if previous.manifest!=candidate.manifest || previous.source!=candidate.source || previous.destination!=candidate.destination { return Err(Refusal::Reused); }
        tx.commit().map_err(sql)?;return Ok(previous);
    }
    let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_mail_handoffs WHERE session=?1 AND phase IN ('offered','prepared'))",[&candidate.manifest.session],|row|row.get(0)).map_err(sql)?;
    if active { return Err(Refusal::Stale); }
    let ordinal:i64=tx.query_row("SELECT COALESCE(MAX(ordinal),0) FROM agent_mail_handoffs",[],|row|row.get(0)).map_err(sql)?;
    let ordinal=ordinal.checked_add(1).ok_or(Refusal::Capacity)?;
    // An accepted offer must also fit its longest future phase, without a guessed byte reserve.
    drop(json(candidate,Phase::Committed)?);
    let payload=json(candidate,Phase::Offered)?;
    tx.execute("INSERT INTO agent_mail_handoffs(attempt,ordinal,session,source_epoch,destination_epoch,phase,record) VALUES(?1,?2,?3,?4,?5,'offered',?6)",params![candidate.manifest.attempt,ordinal,candidate.manifest.session,integer(candidate.manifest.source_epoch)?,integer(candidate.manifest.destination_epoch)?,payload]).map_err(sql)?;
    tx.commit().map_err(sql)?;
    Ok(candidate.clone())
}

pub async fn adopt(store: &Store, manifest: TransferManifest, actor: Principal) -> Result<Record, Refusal> {
    store.call(move|c|Ok(finish_on(c,&manifest,&actor,Phase::Committed))).await.map_err(self::store)?
}

pub async fn prepare(store: &Store, manifest: TransferManifest, actor: Principal) -> Result<Record, Refusal> {
    store.call(move|c|Ok(finish_on(c,&manifest,&actor,Phase::Prepared))).await.map_err(self::store)?
}

pub async fn abort(store: &Store, manifest: TransferManifest, actor: Principal) -> Result<Record, Refusal> {
    store.call(move|c|Ok(finish_on(c,&manifest,&actor,Phase::Aborted))).await.map_err(self::store)?
}

fn finish_on(c: &mut Connection, manifest: &TransferManifest, actor: &Principal, phase: Phase) -> Result<Record, Refusal> {
    let tx=c.savepoint().map_err(sql)?;
    let mut record=read_on(&tx,&manifest.attempt,false)?.ok_or(Refusal::Stale)?;
    if record.manifest!=*manifest { return Err(Refusal::Reused); }
    let expected=match phase { Phase::Prepared|Phase::Committed=>&record.destination,Phase::Aborted=>&record.source,Phase::Offered=>return Err(Refusal::Unsupported) };
    if actor!=expected { return Err(Refusal::Stale); }
    if record.phase==phase || (record.phase==Phase::Committed&&phase==Phase::Prepared) { tx.commit().map_err(sql)?;return Ok(record); }
    let allowed=match phase { Phase::Prepared=>record.phase==Phase::Offered,Phase::Committed=>record.phase==Phase::Prepared,Phase::Aborted=>matches!(record.phase,Phase::Offered|Phase::Prepared),Phase::Offered=>false };
    if !allowed { return Err(Refusal::Stale); }
    if phase==Phase::Committed {
        let terminal=serde_json::to_string(&super::super::admission::Disposition::Transferred { epoch:manifest.destination_epoch }).map_err(|_|Refusal::Unavailable)?;
        let args=params![manifest.session,integer(manifest.source_epoch)?,integer(manifest.destination_epoch)?,terminal];
        let conflict:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_mail m JOIN agent_mail_transfers t ON t.revision=m.revision AND t.generation=m.generation AND t.from_epoch=?2 WHERE m.recipient=?1 AND m.epoch=?2 AND m.generation>0 AND m.disposition IN ('waiting','dispatched','admitted') AND (t.to_epoch<>?3 OR t.terminal<>?4))",args,|row|row.get(0)).map_err(sql)?;
        if conflict { return Err(Refusal::Reused); }
        tx.execute("INSERT INTO agent_mail_transfers(revision,generation,from_epoch,to_epoch,terminal) SELECT revision,generation,?2,?3,?4 FROM agent_mail WHERE recipient=?1 AND epoch=?2 AND generation>0 AND disposition IN ('waiting','dispatched','admitted') ON CONFLICT(revision,generation,from_epoch) DO NOTHING",params![manifest.session,integer(manifest.source_epoch)?,integer(manifest.destination_epoch)?,terminal]).map_err(sql)?;
        // Terminal history stays untouched; the current owner reconciles its exact committed
        // terminal token with mail::terminal_projection instead of migrating all old rows.
        tx.execute("UPDATE agent_mail SET epoch=?3,source_connection=NULL WHERE recipient=?1 AND epoch=?2 AND generation>0 AND disposition IN ('waiting','dispatched','admitted')",params![manifest.session,integer(manifest.source_epoch)?,integer(manifest.destination_epoch)?]).map_err(sql)?;
    }
    let previous=record.phase;
    record.phase=phase;
    let changed=tx.execute("UPDATE agent_mail_handoffs SET phase=?2,record=?3 WHERE attempt=?1 AND phase=?4",params![manifest.attempt,phase.name(),json(&record,phase)?,previous.name()]).map_err(sql)?;
    if changed!=1 { return Err(Refusal::Stale); }
    tx.commit().map_err(sql)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super as mail;
    use serde_json::{Value,json};

    fn why(error: Refusal) -> anyhow::Error { anyhow::anyhow!("{error:?}") }

    fn db() -> anyhow::Result<Connection> {
        let c=Connection::open_in_memory()?;
        c.execute_batch(include_str!("../../store/migrations/20261111-0100_agent_mail.sql"))?;
        Ok(c)
    }

    fn peer(name: &str, certificate: &str) -> Principal { Principal { peer:certificate.repeat(256/8),machine:name.into(),instance:Some(format!("{name}-instance")) } }

    fn manifest(attempt: &str, source: u64, destination: u64) -> TransferManifest {
        TransferManifest { attempt:attempt.into(),session:"session".into(),source_epoch:source,destination_epoch:destination,count:7,sha256:"ab".repeat(256/8) }
    }

    fn offered(manifest: TransferManifest, source: Principal, destination: Principal) -> Record { Record { manifest,source,destination,phase:Phase::Offered } }

    fn publication(id: &str) -> mail::Publish {
        mail::Publish { owner:"owner".into(),sender:"sender".into(),recipient:"session".into(),id:id.into(),command:json!({"type":"send","id":id,"text":"private peer original 日本語"}),provenance:Some(json!({"proof":"original provenance"})),status:None }
    }

    fn seeded(c: &mut Connection, id: &str, epoch: u64, state: mail::Disposition) -> anyhow::Result<mail::Delivery> {
        let receipt=mail::publish_on(c,&publication(id),u64::MAX,u64::MAX)?;
        let delivery=mail::claim_at_on(c,receipt.revision,epoch,1,Some("source-connection"),crate::harness::state_view::FRAME)?.ok_or_else(||anyhow::anyhow!("missing fixture claim"))?;
        match state {
            mail::Disposition::Dispatched=>{},
            mail::Disposition::Cancelled=>{ mail::transition_on(c,delivery.token,Some(epoch),state,Some("{\"disposition\":\"cancelled\",\"reason\":\"user\"}"))?; },
            _=>{ mail::transition_on(c,delivery.token,Some(epoch),state,None)?; },
        }
        Ok(delivery)
    }

    fn snapshot(c: &Connection, revision: u64) -> anyhow::Result<Value> {
        let raw:String=c.query_row("SELECT json_object('id',id,'body',body,'bytes',bytes,'status_task',status_task,'status_key',status_key,'disposition',disposition,'generation',generation,'epoch',epoch,'frozen',frozen,'terminal',terminal,'source_generation',source_generation,'source_connection',source_connection) FROM agent_mail WHERE revision=?1",[i64::try_from(revision)?],|row|row.get(0))?;
        Ok(serde_json::from_str(&raw)?)
    }

    #[test]
    fn same_uncertain_token_survives_two_verified_owner_moves_with_terminal_receipts() -> anyhow::Result<()> {
        let mut c=db()?;let a=peer("A","aa");let b=peer("B","bb");let destination=peer("C","cc");
        let mut deliveries=Vec::new();
        for (id,state) in [("flight",mail::Disposition::Dispatched),("admitted",mail::Disposition::Admitted),("consumed-awaiting-ack",mail::Disposition::Consumed),("cancelled-awaiting-ack",mail::Disposition::Cancelled)] { deliveries.push(seeded(&mut c,id,1,state)?); }
        let outsider=seeded(&mut c,"other-epoch",99,mail::Disposition::Admitted)?;
        let waiting=mail::publish_on(&mut c,&publication("server-held"),u64::MAX,u64::MAX)?;
        let before:Vec<Value>=deliveries.iter().map(|d|snapshot(&c,d.token.revision)).collect::<anyhow::Result<_>>()?;
        assert_eq!(affected_on(&c,"session",1).map_err(why)?,2);
        let unrelated=snapshot(&c,outsider.token.revision)?;let server_held=snapshot(&c,waiting.revision)?;
        let ab=offered(manifest("A-B",1,2),a.clone(),b.clone());
        assert_eq!(offer_on(&mut c,&ab).map_err(why)?.phase,Phase::Offered);
        assert_eq!(finish_on(&mut c,&ab.manifest,&b,Phase::Committed),Err(Refusal::Stale),"destination must first commit Prepared metadata");
        assert_eq!(finish_on(&mut c,&ab.manifest,&b,Phase::Prepared).map_err(why)?.phase,Phase::Prepared);
        assert_eq!(finish_on(&mut c,&ab.manifest,&b,Phase::Prepared).map_err(why)?.phase,Phase::Prepared);
        assert_eq!(offer_on(&mut c,&ab).map_err(why)?.phase,Phase::Prepared);
        assert_eq!(finish_on(&mut c,&ab.manifest,&b,Phase::Committed).map_err(why)?.phase,Phase::Committed);
        let bc=offered(manifest("B-C",2,3),b.clone(),destination.clone());
        assert_eq!(affected_on(&c,"session",1).map_err(why)?,0);assert_eq!(affected_on(&c,"session",2).map_err(why)?,2);
        offer_on(&mut c,&bc).map_err(why)?;finish_on(&mut c,&bc.manifest,&destination,Phase::Prepared).map_err(why)?;finish_on(&mut c,&bc.manifest,&destination,Phase::Committed).map_err(why)?;
        for (delivery,mut expected) in deliveries.iter().zip(before) {
            let unresolved=matches!(expected.get("disposition").and_then(Value::as_str),Some("dispatched"|"admitted"|"waiting"));
            let object=expected.as_object_mut().ok_or_else(||anyhow::anyhow!("fixture snapshot is not an object"))?;
            if unresolved { object.insert("epoch".into(),json!(3));object.insert("source_connection".into(),Value::Null); }
            assert_eq!(snapshot(&c,delivery.token.revision)?,expected,"only unresolved owner epoch and replay connection change; terminal history stays untouched");
            assert_eq!(c.query_row("SELECT COUNT(*) FROM agent_mail_transfers WHERE revision=?1 AND generation=?2",params![i64::try_from(delivery.token.revision)?,i64::try_from(delivery.token.generation)?],|row|row.get::<_,i64>(0))?,if unresolved { 2 } else { 0 });
            if unresolved {
                for epoch in [1,2] { assert_eq!(mail::attempt_on(&c,"session",delivery.token,epoch,false)?.map(|(r,_)|r.disposition),Some(mail::Disposition::Transferred)); }
                assert!(mail::terminal_projection_on(&c,"session",delivery.token)?.is_none());
            } else {
                let terminal=mail::terminal_projection_on(&c,"session",delivery.token)?.ok_or_else(||anyhow::anyhow!("exact committed terminal proof was lost after move"))?;
                assert!(terminal.publication.repeat);assert_eq!(terminal.publication.id,delivery.publication.id);
                assert_eq!(terminal.mail_scope,mail::scope(&delivery.publication.sender));
                assert!(mail::projection_on(&c,"session",delivery.token,3)?.is_none(),"historical terminal epoch was not rewritten");
                assert!(mail::terminal_projection_on(&c,"wrong-recipient",delivery.token)?.is_none());
                assert!(mail::terminal_projection_on(&c,"session",mail::Token { generation:delivery.token.generation+1,..delivery.token })?.is_none());
            }
        }
        assert_eq!(snapshot(&c,outsider.token.revision)?,unrelated);assert_eq!(snapshot(&c,waiting.revision)?,server_held);
        let flight=deliveries.first().ok_or_else(||anyhow::anyhow!("no flight"))?;
        let replay=mail::replay_at_on(&mut c,flight.token.revision,3,"C-connection",crate::harness::state_view::FRAME)?.ok_or_else(||anyhow::anyhow!("destination cannot reconcile"))?;
        assert_eq!(replay.token,flight.token);assert_eq!(replay.publication,flight.publication);
        assert!(mail::replay_at_on(&mut c,flight.token.revision,3,"C-connection",crate::harness::state_view::FRAME)?.is_none());
        assert_eq!(offer_on(&mut c,&ab).map_err(why)?.phase,Phase::Committed,"duplicate Offer must not regress its phase");
        finish_on(&mut c,&ab.manifest,&b,Phase::Committed).map_err(why)?;
        assert_eq!(finish_on(&mut c,&ab.manifest,&b,Phase::Prepared).map_err(why)?.phase,Phase::Committed,"late Ready cannot regress a committed handoff");
        assert_eq!(snapshot(&c,flight.token.revision)?.get("epoch"),Some(&json!(3)),"old Adopt must not rebind the next owner");
        assert_eq!(read_on(&c,"session",true).map_err(why)?.map(|r|r.manifest),Some(bc.manifest));
        assert!(!json(&ab,Phase::Offered).map_err(why)?.contains("private peer original"));
        Ok(())
    }

    #[test]
    fn manifest_principal_and_phase_mismatches_cannot_move_custody() -> anyhow::Result<()> {
        let mut c=db()?;let a=peer("A","aa");let b=peer("B","bb");let stranger=peer("other","dd");
        let delivery=seeded(&mut c,"one",1,mail::Disposition::Admitted)?;let original=snapshot(&c,delivery.token.revision)?;
        let offer=offered(manifest("attempt",1,2),a.clone(),b.clone());offer_on(&mut c,&offer).map_err(why)?;
        assert_eq!(finish_on(&mut c,&offer.manifest,&a,Phase::Committed),Err(Refusal::Stale));
        assert_eq!(finish_on(&mut c,&offer.manifest,&b,Phase::Aborted),Err(Refusal::Stale));
        let mut restarted=b.clone();restarted.instance=Some("different-instance".into());
        assert_eq!(finish_on(&mut c,&offer.manifest,&restarted,Phase::Prepared),Err(Refusal::Stale));
        let mut wrong=offer.clone();wrong.manifest.sha256="cd".repeat(256/8);
        assert_eq!(finish_on(&mut c,&wrong.manifest,&b,Phase::Committed),Err(Refusal::Reused));
        assert_eq!(offer_on(&mut c,&wrong),Err(Refusal::Reused));
        let mut wrong=offer.clone();wrong.source=stranger.clone();assert_eq!(offer_on(&mut c,&wrong),Err(Refusal::Reused));
        wrong=offer.clone();wrong.destination=stranger.clone();assert_eq!(offer_on(&mut c,&wrong),Err(Refusal::Reused));
        let competing=offered(manifest("competing",1,3),a.clone(),stranger.clone());assert_eq!(offer_on(&mut c,&competing),Err(Refusal::Stale));
        assert_eq!(snapshot(&c,delivery.token.revision)?,original);assert_eq!(c.query_row("SELECT COUNT(*) FROM agent_mail_transfers",[],|row|row.get::<_,i64>(0))?,0);
        assert_eq!(finish_on(&mut c,&offer.manifest,&a,Phase::Aborted).map_err(why)?.phase,Phase::Aborted);
        assert_eq!(finish_on(&mut c,&offer.manifest,&a,Phase::Aborted).map_err(why)?.phase,Phase::Aborted);
        assert_eq!(finish_on(&mut c,&offer.manifest,&b,Phase::Committed),Err(Refusal::Stale));
        assert_eq!(offer_on(&mut c,&offer).map_err(why)?.phase,Phase::Aborted);
        offer_on(&mut c,&competing).map_err(why)?;finish_on(&mut c,&competing.manifest,&stranger,Phase::Prepared).map_err(why)?;finish_on(&mut c,&competing.manifest,&stranger,Phase::Committed).map_err(why)?;
        assert_eq!(finish_on(&mut c,&competing.manifest,&a,Phase::Aborted),Err(Refusal::Stale));
        assert_eq!(finish_on(&mut c,&competing.manifest,&b,Phase::Committed),Err(Refusal::Stale));
        let next=offered(manifest("abort-prepared",3,4),stranger.clone(),b.clone());offer_on(&mut c,&next).map_err(why)?;
        finish_on(&mut c,&next.manifest,&b,Phase::Prepared).map_err(why)?;
        assert_eq!(offer_on(&mut c,&offered(manifest("blocked-prepared",3,5),stranger.clone(),a.clone())),Err(Refusal::Stale));
        assert_eq!(finish_on(&mut c,&next.manifest,&stranger,Phase::Aborted).map_err(why)?.phase,Phase::Aborted);
        assert_eq!(snapshot(&c,delivery.token.revision)?.get("epoch"),Some(&json!(3)),"Prepared and abort do not move originals");
        Ok(())
    }

    #[test]
    fn failed_phase_commit_rolls_back_transfer_audit_and_owner_epoch() -> anyhow::Result<()> {
        let mut c=db()?;let a=peer("A","aa");let b=peer("B","bb");let delivery=seeded(&mut c,"one",1,mail::Disposition::Admitted)?;
        let before=snapshot(&c,delivery.token.revision)?;let offer=offered(manifest("attempt",1,2),a,b.clone());offer_on(&mut c,&offer).map_err(why)?;
        finish_on(&mut c,&offer.manifest,&b,Phase::Prepared).map_err(why)?;
        c.execute_batch("CREATE TRIGGER refuse_handoff BEFORE UPDATE ON agent_mail_handoffs WHEN NEW.phase='committed' BEGIN SELECT RAISE(ABORT,'fixture refuses commit'); END;")?;
        assert_eq!(finish_on(&mut c,&offer.manifest,&b,Phase::Committed),Err(Refusal::Unavailable));
        assert_eq!(snapshot(&c,delivery.token.revision)?,before);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM agent_mail_transfers",[],|row|row.get::<_,i64>(0))?,0);
        assert_eq!(read_on(&c,"attempt",false).map_err(why)?.map(|r|r.phase),Some(Phase::Prepared));
        assert_eq!(sql(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),Some("private SQL detail".into()))),Refusal::Capacity);
        assert_eq!(serde_json::to_value(Refusal::Unavailable)?,json!("unavailable"));
        Ok(())
    }

    #[test]
    fn offered_metadata_reserves_its_exact_longest_phase_with_no_clipping() -> anyhow::Result<()> {
        let mut c=db()?;let mut offer=offered(manifest("bounded",1,2),peer("A","aa"),peer("B","bb"));
        let overhead=json(&offer,Phase::Committed).map_err(why)?.len();
        let remaining=crate::harness::state_view::FRAME.checked_sub(overhead).ok_or_else(||anyhow::anyhow!("fixture overhead exceeds frame"))?;
        offer.destination.machine.push_str(&"x".repeat(remaining));
        assert_eq!(json(&offer,Phase::Committed).map_err(why)?.len(),crate::harness::state_view::FRAME);
        let mut too_large=offer.clone();too_large.destination.machine.push('x');
        assert_eq!(offer_on(&mut c,&too_large),Err(Refusal::Capacity));
        assert!(read_on(&c,"bounded",false).map_err(why)?.is_none());
        let saved=offer_on(&mut c,&offer).map_err(why)?;assert_eq!(saved.destination,offer.destination);
        finish_on(&mut c,&offer.manifest,&offer.destination,Phase::Prepared).map_err(why)?;
        let committed=finish_on(&mut c,&offer.manifest,&offer.destination,Phase::Committed).map_err(why)?;
        assert_eq!(committed.destination,offer.destination);assert_eq!(json(&committed,committed.phase).map_err(why)?.len(),crate::harness::state_view::FRAME);
        Ok(())
    }

    #[test]
    fn transfer_body_cursor_pages_repeated_generations_without_changing_entry_fields() -> anyhow::Result<()> {
        for first_cursor in [1i64,9,99] {
        let mut c=db()?;let a=peer("A","aa");let b=peer("B","bb");let destination=peer("C","cc");let delivery=seeded(&mut c,"one",1,mail::Disposition::Admitted)?;
        c.execute("INSERT INTO sqlite_sequence(name,seq) VALUES('agent_mail_transfers',?1)",[first_cursor-1])?;
        let first=offered(manifest("A-B",1,2),a,b.clone());offer_on(&mut c,&first).map_err(why)?;finish_on(&mut c,&first.manifest,&b,Phase::Prepared).map_err(why)?;finish_on(&mut c,&first.manifest,&b,Phase::Committed).map_err(why)?;
        let second=offered(manifest("B-C",2,3),b,destination.clone());offer_on(&mut c,&second).map_err(why)?;finish_on(&mut c,&second.manifest,&destination,Phase::Prepared).map_err(why)?;finish_on(&mut c,&second.manifest,&destination,Phase::Committed).map_err(why)?;
        let read=mail::BodyRead { revision:delivery.token.revision,path:"/$transfers".into(),offset:0 };
        let all=mail::body_on(&c,"owner","sender","session",&read,crate::harness::state_view::FRAME)?;
        let full_budget=serde_json::to_vec(&all)?.len();
        assert_eq!(mail::body_on(&c,"owner","sender","session",&read,full_budget)?,all,"final null cursor must fit its exact envelope");
        let mail::BodyNode::Transfers { entries }=all.node else { anyhow::bail!("not transfer history") };
        assert_eq!(entries.len(),2);assert!(entries.iter().all(|entry|entry.generation==delivery.token.generation));
        let cursor:i64=c.query_row("SELECT MIN(cursor) FROM agent_mail_transfers WHERE revision=?1",[i64::try_from(delivery.token.revision)?],|row|row.get(0))?;
        assert_eq!(cursor,first_cursor);
        let one=entries.first().ok_or_else(||anyhow::anyhow!("no first transfer"))?.clone();
        let expected=mail::BodyReply { revision:read.revision,path:read.path.clone(),offset:0,node:mail::BodyNode::Transfers { entries:vec![one] },next:Some(usize::try_from(cursor)?) };
        let budget=serde_json::to_vec(&expected)?.len();
        let page=mail::body_on(&c,"owner","sender","session",&read,budget)?;assert_eq!(serde_json::to_vec(&page)?.len(),budget);assert_eq!(page,expected);
        assert!(matches!(mail::body_on(&c,"owner","sender","session",&read,budget-1),Err(mail::Error::TooLarge { .. })),"no silent empty or clipped entry below its exact envelope");
        let next_read=mail::BodyRead { offset:page.next.ok_or_else(||anyhow::anyhow!("no cursor"))?,..read };
        let next=mail::body_on(&c,"owner","sender","session",&next_read,crate::harness::state_view::FRAME)?;
        let next_budget=serde_json::to_vec(&next)?.len();
        assert_eq!(mail::body_on(&c,"owner","sender","session",&next_read,next_budget)?,next,"continuation offset digit changes are part of the envelope");
        assert!(matches!(next.node,mail::BodyNode::Transfers { entries } if entries.len()==1&&entries.first().is_some_and(|entry|entry.from_epoch==2)));
        assert_eq!(next.next,None);
        let legacy=serde_json::to_string(&crate::harness::admission::Disposition::Transferred { epoch:2 })?;
        assert!(mail::transfer_on(&mut c,delivery.token,1,2,&legacy)?.repeat,"old terminal retry remains exact after the second move");
        }
        Ok(())
    }
}
