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

/// Whether `a` and `b` are one runner process: its machine and instance, whatever connection it
/// came over. (Until 2026-11-13 the connection was compared too, so a reconnect mid-handover left
/// it Prepared for good, and a destination that reconnected after its commit could never claim
/// the sessions it held: they read offline and their mail held.)
fn same(a: &Principal, b: &Principal) -> bool {
    a.machine == b.machine && a.instance == b.instance
}

/// Whether `actor` holds what a handover recorded as `recorded`'s: it is that process, or that
/// process is gone and `actor` is its machine's newest, which took up what it held (a runner
/// restarted keeps its sessions: 2026-11-12 21:09, adacachy's).
fn holds(hub: &Hub, actor: &Principal, recorded: &Principal) -> bool {
    if same(actor, recorded) {
        return true;
    }
    if actor.machine != recorded.machine {
        return false;
    }
    if live(hub, recorded) {
        return false;
    }
    let instances = hub.instances.load();
    instances.newest(&actor.machine).and_then(|peer| instances.get(peer)).is_some_and(|i| i.id == actor.instance)
}

/// Whether `principal`'s process is connected now, over any of its connections.
fn live(hub: &Hub, principal: &Principal) -> bool {
    let instances = hub.instances.load();
    instances.of(&principal.machine).into_iter().any(|peer| instances.current_connection(peer) && instances.get(peer).is_some_and(|i| i.id == principal.instance))
}

/// The connection `principal`'s process speaks over now: the one it was recorded with, else its
/// newest (a reconnect is another connection of the same process).
fn peer(hub: &Hub, principal: &Principal) -> Result<PeerId, Refusal> {
    let recorded = u64::from_str_radix(&principal.peer, 16).map_err(|_| Refusal::Unavailable)?;
    let instances = hub.instances.load();
    let current = |peer: PeerId| instances.current_connection(peer) && instances.get(peer).is_some_and(|i| i.name == principal.machine && i.id == principal.instance);
    if current(recorded) {
        return Ok(recorded);
    }
    instances.of(&principal.machine).into_iter().find(|peer| current(*peer)).ok_or(Refusal::Unavailable)
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
    let to = peer(hub, recipient)?;
    if !same(&principal(hub, to)?, recipient) { return Err(Refusal::Stale); }
    let bytes = super::state_view::encode(&phase, super::state_view::FRAME).map_err(|_| Refusal::Capacity)?;
    let json = String::from_utf8(bytes).map_err(|_| Refusal::Unsupported)?;
    // Dropping the answer waiter does not cancel the existing command owner's retransmission.
    // Ready is an application event, so this control handler never blocks receiving its ACK.
    drop(hub.command_peers(&recipient.machine, vec![to], &crate::proto::ServerMsg::HarnessMailTransfer { json }).await);
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
        let destination = peer(hub, &record.destination)?;
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
                    if record.manifest != manifest || !same(&record.source, &actor) || record.destination.machine != target || instance.as_ref().is_some_and(|id| record.destination.instance.as_ref() != Some(id)) { return Err(Refusal::Reused); }
                    record
                }
                None => {
                    if hub.instances.load().owner(&Owned::Agent(manifest.session.clone())) != Some(peer(hub, &actor)?) || hub.harness.epoch_of(&manifest.session) != manifest.source_epoch { return Err(Refusal::Stale); }
                    let destination = destination(hub, &target, instance.as_deref())?;
                    if same(&destination, &actor) { return Err(Refusal::Unsupported); }
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
            if previous.manifest != manifest || !same(&previous.destination, &actor) { return Err(Refusal::Reused); }
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
            if previous.manifest != manifest || !same(&previous.destination, &actor) { return Err(Refusal::Reused); }
            if matches!(previous.phase, Phase::Committed | Phase::Aborted) { return terminal(hub, &previous).await; }
            let affected = handoffs::affected(&hub.store, manifest.session.clone(), manifest.source_epoch).await?;
            let rows = affected.checked_mul(2).and_then(|n| n.checked_add(1)).ok_or(Refusal::Capacity)?;
            room(hub, &manifest, rows).await?;
            let record = handoffs::adopt(&hub.store, manifest, actor).await?;
            terminal(hub, &record).await
        }
        TransferUp::Abort { manifest } => {
            let previous = handoffs::get(&hub.store, manifest.attempt.clone()).await?.ok_or(Refusal::Stale)?;
            if previous.manifest != manifest || !same(&previous.source, &actor) { return Err(Refusal::Reused); }
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
    let Ok(actor) = principal(hub, sender) else { return false };
    let (owns, minimum) = match record.phase {
        Phase::Offered | Phase::Prepared => (unfinished_holder(hub, &actor, &record), record.manifest.source_epoch),
        Phase::Aborted => (holds(hub, &actor, &record.source), record.manifest.source_epoch),
        Phase::Committed => (holds(hub, &actor, &record.destination), record.manifest.destination_epoch),
    };
    owns && epoch.is_none_or(|epoch| epoch >= minimum)
}

/// Who holds a session whose handover didn't finish: its source while that process lives, which
/// can still finish or abort it; once the source is gone it never will, and the session is
/// whoever took up the source's (its machine's newest process) or the destination that was
/// ready for it.
fn unfinished_holder(hub: &Hub, actor: &Principal, record: &Record) -> bool {
    holds(hub, actor, &record.source) || (!live(hub, &record.source) && same(actor, &record.destination))
}

pub async fn allows_generic_adoption(hub: &Hub, sender: PeerId, session: &str, success: bool) -> bool {
    match handoffs::latest(&hub.store, session.to_owned()).await {
        Ok(None) => true,
        Ok(Some(record)) => {
            let Ok(actor) = principal(hub, sender) else { return false };
            match record.phase {
                Phase::Offered | Phase::Prepared => !live(hub, &record.source) && unfinished_holder(hub, &actor, &record),
                Phase::Committed => holds(hub, &actor, &record.destination),
                Phase::Aborted => !success && holds(hub, &actor, &record.destination),
            }
        }
        Err(_) => false,
    }
}

pub async fn dispatch_ready(hub: &Hub, session: &str, capacity: &super::admission::Capacity) -> Result<bool, Refusal> {
    let Some(record) = handoffs::latest(&hub.store, session.to_owned()).await? else { return Ok(true) };
    let owner = hub.instances.load().owner(&Owned::Agent(session.to_owned()));
    let owner = owner.and_then(|owner| principal(hub, owner).ok());
    match record.phase {
        // While its source lives the handover may still finish: mail waits for it. Gone, it
        // never will, and mail goes to whoever holds the session now (2026-11-12 21:09: three
        // sessions' mail waited behind handovers a runner restart left Prepared).
        Phase::Offered | Phase::Prepared => Ok(!live(hub, &record.source) && owner.as_ref().is_some_and(|owner| unfinished_holder(hub, owner, &record))),
        Phase::Aborted => Ok(true),
        Phase::Committed => Ok(capacity.epoch >= record.manifest.destination_epoch && owner.as_ref().is_some_and(|owner| holds(hub, owner, &record.destination))),
    }
}

#[cfg(test)]
#[path = "mail_transfer_tests.rs"]
mod tests;
