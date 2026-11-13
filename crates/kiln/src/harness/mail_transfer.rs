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

/// The connection of the process that superseded `owner`'s: its machine's newest runner process,
/// when that's another process and `owner`'s has no connection of its own any more (it ended, and
/// its machine's runner started again). None while `owner`'s process is connected, or nothing
/// newer is.
fn successor(hub: &Hub, owner: &Principal) -> Option<PeerId> {
    let instances = hub.instances.load();
    let of = |peer: PeerId| instances.get(peer).and_then(|i| i.id.clone());
    let lives = owner.instance.is_some() && instances.of(&owner.machine).into_iter().any(|peer| instances.current_connection(peer) && of(peer) == owner.instance);
    let newest = instances.newest(&owner.machine)?;
    (!lives && instances.current_connection(newest) && (owner.instance.is_none() || of(newest) != owner.instance)).then_some(newest)
}

/// Whether runner connection `sender` speaks for the process `owner` names, at the session's epoch
/// `epoch` against the binding's `minimum`: that process itself (its stored instance), or
/// (2026-11-12: adacachy's runner started again, and the sessions a hand-over had left with its
/// old process could never be claimed, their mail refused as a stale owner's without end) the
/// process of its machine that superseded it ([`successor`]), with the session past the binding's
/// epoch: the old process can't act again, and only a holder that went on with the session has
/// moved its epoch past it. Without an epoch (an inventory, a dispatch), the stored process only.
fn speaks_for(hub: &Hub, sender: PeerId, owner: &Principal, epoch: Option<u64>, minimum: u64) -> bool {
    let Ok(me) = principal(hub, sender) else { return false };
    if me.machine != owner.machine {
        return false;
    }
    (me.instance.is_some() && me.instance == owner.instance) || (epoch.is_some_and(|epoch| epoch > minimum) && successor(hub, owner) == Some(sender))
}

/// [`speaks_for`], and at the binding's own epoch its successor too when the bound process signed a hand-on of
/// `session` (`custody::handed_on_by`): that process's word that it let the session go, which the epoch alone can't
/// say. 2026-11-12, adacachy: its old process handed each session on to its next epoch and the new one held them on
/// at the one they had (each's epoch the binding's: 1, 7 and 5), and none could be claimed, their mail refused.
async fn speaks_for_session(hub: &Hub, sender: PeerId, owner: &Principal, epoch: Option<u64>, minimum: u64, session: &str) -> bool {
    if speaks_for(hub, sender, owner, epoch, minimum) {
        return true;
    }
    let Some(instance) = owner.instance.as_deref() else { return false };
    if epoch != Some(minimum) || successor(hub, owner) != Some(sender) {
        return false;
    }
    crate::custody::handed_on_by(&hub.store, session, minimum, &owner.machine, instance).await.unwrap_or(false)
}

/// Whether the process `owner` names is connected now.
fn lives(hub: &Hub, owner: &Principal) -> bool {
    let instances = hub.instances.load();
    owner.instance.is_some() && instances.of(&owner.machine).into_iter().any(|peer| instances.current_connection(peer) && instances.get(peer).is_some_and(|i| i.id == owner.instance))
}

/// The latest hand-over of `session`, one its source's process left unfinished (offered or
/// prepared) and was superseded before finishing settled first, since that process will never
/// say its word: committed for the destination that prepared it and is connected (what an adopt
/// does: the mail goes on at the destination's epoch), else aborted (the mail stays with the
/// source's epoch). Its runners are told as that phase tells them; one that's gone can't hear it.
async fn settled(hub: &Hub, session: &str) -> Result<Option<Record>, Refusal> {
    let Some(record) = handoffs::latest(&hub.store, session.to_owned()).await? else { return Ok(None) };
    if !matches!(record.phase, Phase::Offered | Phase::Prepared) || successor(hub, &record.source).is_none() {
        return Ok(Some(record));
    }
    let done = if record.phase == Phase::Prepared && lives(hub, &record.destination) {
        handoffs::adopt(&hub.store, record.manifest.clone(), record.destination.clone()).await?
    } else {
        handoffs::abort(&hub.store, record.manifest.clone(), record.source.clone()).await?
    };
    tracing::info!(%session, attempt = %done.manifest.attempt, phase = ?done.phase, "a hand-over its source's process left unfinished is settled: that process was superseded");
    let _ = terminal(hub, &done).await;
    Ok(Some(done))
}

/// A new owner cannot bypass the durable handover by publishing Open or an inventory first.
pub async fn allows_claim(hub: &Hub, sender: PeerId, session: &str, epoch: Option<u64>) -> bool {
    let record = match settled(hub, session).await { Ok(record) => record, Err(_) => return false };
    let Some(record) = record else { return true };
    let (owner, minimum) = match record.phase {
        Phase::Offered | Phase::Prepared | Phase::Aborted => (&record.source, record.manifest.source_epoch),
        Phase::Committed => (&record.destination, record.manifest.destination_epoch),
    };
    epoch.is_none_or(|epoch| epoch >= minimum) && speaks_for_session(hub, sender, owner, epoch, minimum, session).await
}

pub async fn allows_generic_adoption(hub: &Hub, sender: PeerId, session: &str, success: bool) -> bool {
    match settled(hub, session).await {
        Ok(None) => true,
        Ok(Some(record)) => match record.phase {
            Phase::Offered | Phase::Prepared => false,
            Phase::Committed => speaks_for(hub, sender, &record.destination, None, record.manifest.destination_epoch),
            Phase::Aborted => !success && speaks_for(hub, sender, &record.destination, None, record.manifest.destination_epoch),
        },
        Err(_) => false,
    }
}

pub async fn dispatch_ready(hub: &Hub, session: &str, capacity: &super::admission::Capacity) -> Result<bool, Refusal> {
    let Some(record) = settled(hub, session).await? else { return Ok(true) };
    match record.phase {
        Phase::Offered | Phase::Prepared => Ok(false),
        Phase::Aborted => Ok(true),
        Phase::Committed => {
            let owner = hub.instances.load().owner(&Owned::Agent(session.to_owned()));
            let minimum = record.manifest.destination_epoch;
            let Some(owner) = owner.filter(|_| capacity.epoch >= minimum) else { return Ok(false) };
            Ok(speaks_for_session(hub, owner, &record.destination, Some(capacity.epoch), minimum, session).await)
        }
    }
}

#[cfg(test)]
#[path = "mail_transfer_tests.rs"]
mod tests;
