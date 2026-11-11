//! Verified journal handover at the existing Hub/Store/Instances owners. There is no transfer
//! polling task: authenticated offer, readiness and adoption events advance durable phases.

use std::sync::Arc;

use super::mail::handoffs::{self, Phase, Principal, Record};
use super::mail::{TransferDown, TransferManifest, TransferRefusal as Refusal, TransferUp};
use crate::hub::Hub;
use crate::instances::{Owned, PeerId};

fn principal(hub: &Hub, peer: PeerId) -> Result<Principal, Refusal> {
    let instances = hub.instances.load();
    let instance = instances.get(peer).filter(|_| instances.current_connection(peer)).ok_or(Refusal::Unavailable)?;
    if !crate::proto::feature::speaks(&instance.features, crate::proto::feature::MAIL_TRANSFER) { return Err(Refusal::Unsupported); }
    Ok(Principal { peer: format!("{peer:016x}"), machine: instance.name.clone(), instance: instance.id.clone() })
}

fn peer(principal: &Principal) -> Result<PeerId, Refusal> {
    u64::from_str_radix(&principal.peer, 16).map_err(|_| Refusal::Unavailable)
}

fn destination(hub: &Hub, target: &str, instance: Option<&str>) -> Result<Principal, Refusal> {
    let selected = {
        let instances = hub.instances.load();
        match instance {
            Some(id) => instances.of(target).into_iter().find(|peer| instances.get(*peer).is_some_and(|i| i.id.as_deref() == Some(id))),
            None => instances.newest(target),
        }
    }.ok_or(Refusal::Unavailable)?;
    principal(hub, selected)
}

fn floor(hub: &Hub) -> Result<Option<u64>, Refusal> {
    hub.cfg.ci.scheduler.admission.as_ref().map(|p| p.admission().map(|p| p.disk_free).map_err(|_| Refusal::Unavailable)).transpose()
}

async fn room(hub: &Hub, manifest: &TransferManifest, rows: u64) -> Result<(), Refusal> {
    // Prior bounded originals may be larger than a metadata-only epoch update. Reserve their
    // existing protocol bound for each affected row, without loading communication bodies.
    let payload = u64::try_from(super::state_view::FRAME).map_err(|_| Refusal::Capacity)?.checked_mul(rows).ok_or(Refusal::Capacity)?;
    let tables = handoffs::TABLES.iter().map(|name| (*name).to_owned()).collect();
    let scratch = hub.store.storage_reserve_tables(payload, tables, rows).await.map_err(|_| Refusal::Unavailable)?;
    let path = hub.cfg.server.data_dir.clone();
    let free = crate::rt::blocking(move || crate::os::disk_available(&path)).await.map_err(|_| Refusal::Unavailable)?.ok_or(Refusal::Unavailable)?;
    let required = floor(hub)?.unwrap_or(0).checked_add(scratch).ok_or(Refusal::Capacity)?;
    if free < required { return Err(Refusal::Capacity); }
    if manifest.destination_epoch != manifest.source_epoch.checked_add(1).ok_or(Refusal::Capacity)? { return Err(Refusal::Unsupported); }
    Ok(())
}

async fn send(hub: &Hub, recipient: &Principal, phase: TransferDown) -> Result<(), Refusal> {
    if principal(hub, peer(recipient)?)? != *recipient { return Err(Refusal::Stale); }
    let bytes = super::state_view::encode(&phase, super::state_view::FRAME).map_err(|_| Refusal::Capacity)?;
    let json = String::from_utf8(bytes).map_err(|_| Refusal::Unsupported)?;
    // Dropping the answer waiter does not cancel the existing command owner's retransmission.
    // Ready is an application event, so this control handler never blocks receiving its ACK.
    drop(hub.command_peers(&recipient.machine, vec![peer(recipient)?], &crate::proto::ServerMsg::HarnessMailTransfer { json }).await);
    Ok(())
}

async fn offered(hub: &Hub, record: &Record) -> Result<(), Refusal> {
    send(hub, &record.destination, TransferDown::Offered { manifest: record.manifest.clone(), disk_floor: floor(hub)? }).await
}

async fn terminal(hub: &Hub, record: &Record) -> Result<(), Refusal> {
    let phase = match record.phase {
        Phase::Committed => TransferDown::Committed { manifest: record.manifest.clone() },
        Phase::Aborted => TransferDown::Aborted { manifest: record.manifest.clone() },
        Phase::Offered | Phase::Prepared => return Err(Refusal::Stale),
    };
    let latest = handoffs::latest(&hub.store, record.manifest.session.clone()).await?;
    if record.phase == Phase::Committed && latest.as_ref().is_some_and(|latest| latest == record) {
        let destination = peer(&record.destination)?;
        hub.instances.update(|instances| instances.own(Owned::Agent(record.manifest.session.clone()), destination));
        if let Some(label) = hub.instances.load().get(destination).map(crate::instances::Instance::label) {
            hub.harness.held_by(&record.manifest.session, &label);
        }
        hub.harness.mail_delivery.changed();
    }
    send(hub, &record.destination, phase.clone()).await?;
    send(hub, &record.source, phase).await
}

async fn apply(hub: &Arc<Hub>, actor: Principal, input: TransferUp) -> Result<(), Refusal> {
    match input {
        TransferUp::Offer { manifest, target, instance } => {
            let record = match handoffs::get(&hub.store, manifest.attempt.clone()).await? {
                Some(record) => {
                    if record.manifest != manifest || record.source != actor || record.destination.machine != target || instance.as_ref().is_some_and(|id| record.destination.instance.as_ref() != Some(id)) { return Err(Refusal::Reused); }
                    record
                }
                None => {
                    if hub.instances.load().owner(&Owned::Agent(manifest.session.clone())) != Some(peer(&actor)?) || hub.harness.epoch_of(&manifest.session) != manifest.source_epoch { return Err(Refusal::Stale); }
                    let destination = destination(hub, &target, instance.as_deref())?;
                    if destination == actor { return Err(Refusal::Unsupported); }
                    room(hub, &manifest, 1).await?;
                    handoffs::offer(&hub.store, manifest, actor, destination).await?
                }
            };
            match record.phase {
                // A fresh Ready event reinstalls destination policy after a reconnect, even if
                // Prepared was already durable. Never permission the source on a stale socket.
                Phase::Offered | Phase::Prepared => offered(hub, &record).await,
                Phase::Committed | Phase::Aborted => terminal(hub, &record).await,
            }
        }
        TransferUp::Ready { manifest } => {
            let previous = handoffs::get(&hub.store, manifest.attempt.clone()).await?.ok_or(Refusal::Stale)?;
            if previous.manifest != manifest || previous.destination != actor { return Err(Refusal::Reused); }
            if matches!(previous.phase, Phase::Committed | Phase::Aborted) { return terminal(hub, &previous).await; }
            room(hub, &manifest, 1).await?;
            let record = handoffs::prepare(&hub.store, manifest, actor).await?;
            match record.phase {
                Phase::Prepared => send(hub, &record.source, TransferDown::Offered { manifest: record.manifest, disk_floor: floor(hub)? }).await,
                Phase::Committed | Phase::Aborted => terminal(hub, &record).await,
                Phase::Offered => Err(Refusal::Unavailable),
            }
        }
        TransferUp::Adopt { manifest } => {
            let previous = handoffs::get(&hub.store, manifest.attempt.clone()).await?.ok_or(Refusal::Stale)?;
            if previous.manifest != manifest || previous.destination != actor { return Err(Refusal::Reused); }
            if matches!(previous.phase, Phase::Committed | Phase::Aborted) { return terminal(hub, &previous).await; }
            let affected = handoffs::affected(&hub.store, manifest.session.clone(), manifest.source_epoch).await?;
            let rows = affected.checked_mul(2).and_then(|n| n.checked_add(1)).ok_or(Refusal::Capacity)?;
            room(hub, &manifest, rows).await?;
            let record = handoffs::adopt(&hub.store, manifest, actor).await?;
            terminal(hub, &record).await
        }
        TransferUp::Abort { manifest } => {
            let previous = handoffs::get(&hub.store, manifest.attempt.clone()).await?.ok_or(Refusal::Stale)?;
            if previous.manifest != manifest || previous.source != actor { return Err(Refusal::Reused); }
            if matches!(previous.phase, Phase::Committed | Phase::Aborted) { return terminal(hub, &previous).await; }
            room(hub, &manifest, 1).await?;
            let record = handoffs::abort(&hub.store, manifest, actor).await?;
            terminal(hub, &record).await
        }
    }
}

/// Called by the authenticated runner control loop. Bodies are never logged or echoed on a
/// failed transition. Refused is uncertainty, not permission to resume an old owner.
pub async fn heard(hub: &Arc<Hub>, sender: PeerId, json: String) {
    if json.len() > super::state_view::FRAME { return; }
    let Ok(input) = serde_json::from_str::<TransferUp>(&json) else { return; };
    let attempt = match &input {
        TransferUp::Offer { manifest, .. } | TransferUp::Ready { manifest } | TransferUp::Adopt { manifest } | TransferUp::Abort { manifest } => manifest.attempt.clone(),
    };
    let actor = match principal(hub, sender) { Ok(actor) => actor, Err(_) => return };
    if let Err(code) = apply(hub, actor.clone(), input).await {
        let _ = send(hub, &actor, TransferDown::Refused { attempt, code }).await;
    }
}

/// A new owner cannot bypass the durable handover by publishing Open or an inventory first.
pub async fn allows_claim(hub: &Hub, sender: PeerId, session: &str, epoch: Option<u64>) -> bool {
    let record = match handoffs::latest(&hub.store, session.to_owned()).await { Ok(record) => record, Err(_) => return false };
    let Some(record) = record else { return true };
    let (owner, minimum) = match record.phase {
        Phase::Offered | Phase::Prepared | Phase::Aborted => (&record.source, record.manifest.source_epoch),
        Phase::Committed => (&record.destination, record.manifest.destination_epoch),
    };
    principal(hub, sender).as_ref().ok() == Some(owner) && epoch.is_none_or(|epoch| epoch >= minimum)
}

pub async fn allows_generic_adoption(hub: &Hub, sender: PeerId, session: &str, success: bool) -> bool {
    match handoffs::latest(&hub.store, session.to_owned()).await {
        Ok(None) => true,
        Ok(Some(record)) => match record.phase {
            Phase::Offered | Phase::Prepared => false,
            Phase::Committed => principal(hub, sender).ok().as_ref() == Some(&record.destination),
            Phase::Aborted => !success && principal(hub, sender).ok().as_ref() == Some(&record.destination),
        },
        Err(_) => false,
    }
}

pub async fn dispatch_ready(hub: &Hub, session: &str, capacity: &super::admission::Capacity) -> Result<bool, Refusal> {
    let Some(record) = handoffs::latest(&hub.store, session.to_owned()).await? else { return Ok(true) };
    match record.phase {
        Phase::Offered | Phase::Prepared => Ok(false),
        Phase::Aborted => Ok(true),
        Phase::Committed => {
            let owner = hub.instances.load().owner(&Owned::Agent(session.to_owned()));
            Ok(capacity.epoch >= record.manifest.destination_epoch && owner.and_then(|owner| principal(hub, owner).ok()).as_ref() == Some(&record.destination))
        }
    }
}

#[cfg(test)]
#[path = "mail_transfer_tests.rs"]
mod tests;
