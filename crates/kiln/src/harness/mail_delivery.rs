//! One event-driven pump over the durable ledger. Bodies are read one bounded page at a time;
//! blocked publications create neither tasks nor a second outbox. Source receipts are committed
//! before their journal is acknowledged (queue-prevention/ADMISSION.md and COALESCING.md).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::FutureExt as _;
use serde_json::Value;

use super::admission::{Capacity, FlowEvent};
use super::mail::{self, Delivery, Error, Publish, Receipt, Token};
use super::model::Command;
use super::registry::Location;
use crate::hub::Hub;

#[derive(Default)]
pub struct Relay {
    changed: crate::rt::Changed,
    generation: AtomicU64,
}

impl Relay {
    /// Owner/capacity/connection events only. Ledger commits have their own existing notifier.
    pub fn changed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.notify();
    }
}

fn unavailable(reason: &str) -> Error { Error::Unavailable { reason: reason.to_owned() } }

/// Scheduler.admission explicitly documents unset as off. That reserves no operator floor;
/// it does not waive measured free space, transaction scratch, or commit failure handling.
pub(super) fn storage_floor(hub: &Hub) -> Result<u64, Error> {
    hub.cfg.ci.scheduler.admission.as_ref().map(|policy| {
        policy.admission().map(|policy| policy.disk_free).map_err(|_| unavailable("operator disk reserve policy is invalid"))
    }).transpose().map(|floor| floor.unwrap_or(0))
}

/// A connected machine alone is insufficient: the session's current owner must speak mail-flow
/// and have pushed capacity for this epoch. Missing/legacy evidence remains explicitly held.
pub fn capacity(hub: &Hub, id: &str) -> Option<Capacity> {
    let info = hub.harness.info_shared(id)?;
    if info.status == super::model::Status::Disconnected { return None; }
    let capacity = info.admission.clone().filter(|c| c.epoch == hub.harness.epoch_of(id))?;
    match hub.harness.location(id)? {
        Location::Local => Some(capacity),
        Location::Machine(machine) => {
            let instances = hub.instances.load();
            let peer = instances.owner(&crate::instances::Owned::Agent(id.to_owned()))?;
            let owner = instances.get(peer)?;
            (owner.name == machine && crate::proto::feature::speaks(&owner.features, crate::proto::feature::MAIL_FLOW)).then_some(capacity)
        }
    }
}

/// The session's current owner is a runner that predates mail-flow: it never pushes capacity, so
/// [`capacity`] never admits mail to it until that runner is upgraded. A person's message, and
/// kiln's own, go to its inbox as before W20 (`Registry::send_asked`, `Registry::dispatch`); an
/// agent's stays in custody, bounded. An owner not known (its runner away) is not one.
pub fn legacy_owner(hub: &Hub, id: &str) -> bool {
    let Some(Location::Machine(machine)) = hub.harness.location(id) else { return false };
    let instances = hub.instances.load();
    let Some(owner) = instances.owner(&crate::instances::Owned::Agent(id.to_owned())).and_then(|peer| instances.get(peer)) else { return false };
    owner.name == machine && !crate::proto::feature::speaks(&owner.features, crate::proto::feature::MAIL_FLOW)
}

async fn peers(hub: &Hub, id: &str) -> Result<Vec<String>, Error> {
    let page = mail::peer_heads(&hub.store, id.to_owned(), 0, super::state_view::FRAME).await?;
    if page.next.is_some() { return Err(Error::TooLarge { revision: page.next.unwrap_or(0), budget: super::state_view::FRAME }); }
    Ok(page.items.into_iter().map(|head| head.sender).collect())
}

fn encode_wire(command: Command, proof: Option<&Value>) -> Result<String, Error> {
    let mut wire = serde_json::to_value(command).map_err(|_| unavailable("communication command could not be encoded"))?;
    if let Some(proof) = proof {
        let object = wire.as_object_mut().ok_or_else(|| unavailable("communication command is not an object"))?;
        object.insert("relayed".into(), proof.clone());
    }
    let bytes = super::state_view::encode(&wire, super::state_view::FRAME).map_err(|_| Error::TooLarge { revision: 0, budget: super::state_view::FRAME })?;
    String::from_utf8(bytes).map_err(|_| unavailable("communication command was not UTF-8"))
}

/// Before custody, check both physical storage and the receiver-wide logical working set,
/// including server-held work. An exact immutable retry is still answered when authority shrinks.
pub async fn publish(hub: &Hub, publication: Publish) -> Result<Receipt, Error> {
    if mail::receipt(&hub.store, publication.owner.clone(), publication.sender.clone(), publication.recipient.clone(), publication.id.clone()).await?.is_some() {
        // The ledger checks the complete immutable command, including attachments. Neither a
        // disconnected owner nor shrinking authority can revoke an already committed receipt.
        return mail::publish(&hub.store, publication, 0, 0).await;
    }
    let command: Command = serde_json::from_value(publication.command.clone()).map_err(|_| Error::Invalid { reason: "publication is not a supported communication command".into() })?;
    let command = hub.harness.attachment_command(hub, &publication.recipient, command).await?;
    // A committed retry keeps its receipt even after closure; a new publication to an unknown
    // recipient has no owner to take custody and must preserve the port's typed absence.
    if hub.harness.location(&publication.recipient).is_none() { return Err(Error::NoSession); }
    let current = match capacity(hub, &publication.recipient) {
        Some(capacity) if super::mail_transfer::dispatch_ready(hub, &publication.recipient, &capacity).await.map_err(|_| unavailable("owner handover state is unavailable"))? => Some(capacity),
        _ => None,
    };
    let authority = match &current {
        Some(current) => {
            let admitted = mail::admitted_count(&hub.store, publication.recipient.clone(), current.epoch).await?;
            let outside = u64::try_from(current.admitted).map_err(|_| unavailable("source admission count does not fit"))?.saturating_sub(admitted);
            u64::try_from(current.target).map_err(|_| unavailable("source admission target does not fit"))?.saturating_sub(outside)
        }
        None => 0,
    };
    let physical = if current.is_some() {
        let floor = storage_floor(hub)?;
        let data = hub.cfg.server.data_dir.clone();
        let free = crate::rt::blocking(move || crate::os::disk_available(&data)).await.map_err(|_| unavailable("data-volume free space could not be read"))?.ok_or_else(|| unavailable("data-volume free space is unknown"))?;
        let reserve = hub.store.storage_reserve(super::state_view::FRAME as u64).await.map_err(|_| unavailable("SQLite/WAL storage reserve is unknown"))?;
        // The room on disk for this one original: what's retained already is on disk, so
        // nothing is counted across conversations.
        free.saturating_sub(floor).saturating_sub(reserve)
    } else { 0 };
    if let Some(current) = &current {
        let mut population = peers(hub, &publication.recipient).await?;
        if !population.contains(&publication.sender) { population.push(publication.sender.clone()); }
        // The longest token is reserved before custody, so a newly accepted body can be sent
        // under the existing frame cap without clipping or giant JSON reassembly.
        let envelope = Command::Admit { peer: publication.sender.clone(), token: Token { revision: u64::MAX, generation: u64::MAX }, epoch: current.epoch, generation: current.feedback_generation.unwrap_or(current.generation), peers: population, command: Box::new(command) };
        let proof = publication.provenance.as_ref().and_then(|p| p.get("relayed"));
        let _ = encode_wire(envelope, proof)?;
    }
    let result = mail::publish(&hub.store, publication, physical, authority).await;
    match result {
        Err(Error::Capacity { .. } | Error::Backpressure { .. }) if current.is_none() => Err(unavailable("current owner capacity is pending or unsupported; custody was not accepted")),
        other => other,
    }
}

/// One bounded protocol write, with no request waiter/body task. Only source FlowEvent proves
/// admission/consumption. A transport failure leaves the exact ledger attempt unresolved.
pub(super) async fn send(hub: &Hub, id: &str, command: Command, proof: Option<&Value>) -> Result<bool, Error> {
    let Some(location) = hub.harness.location(id) else { return Ok(false) };
    match location {
        Location::Local => Ok(hub.harness.local.command_at(id, command, Some(hub.harness.epoch_of(id)))),
        Location::Machine(machine) => {
            let json = encode_wire(command, proof)?;
            Ok(hub.send_to_agent(&machine, crate::proto::LANE_CONTROL, &crate::proto::ServerMsg::HarnessCmd { session: id.to_owned(), json }).await)
        }
    }
}

async fn deliver(hub: &Hub, delivery: Delivery, capacity: &Capacity, population: &[String]) -> Result<(), Error> {
    let recipient = delivery.publication.recipient.as_str();
    if capacity.epoch != delivery.epoch || hub.harness.epoch_of(recipient) != delivery.epoch { return Ok(()); }
    let mut command: Command = serde_json::from_value(delivery.publication.command.clone()).map_err(|_| unavailable("saved communication command is unreadable"))?;
    let mut proof = delivery.publication.provenance.as_ref().and_then(|p| p.get("relayed")).cloned();
    if let Some(seq) = delivery.publication.provenance.as_ref().and_then(|p| p.get("seq")).and_then(Value::as_u64) {
        use crate::runner::chaos::RelayFault;
        match crate::runner::chaos::get().and_then(|fault| fault.relay_words).filter(|_| proof.is_some() && RelayFault::strikes(seq)) {
            Some(RelayFault::Drop) => return Ok(()),
            Some(RelayFault::Alter) => { if let Command::Send { text, .. } = &mut command { text.push('!'); } }
            Some(RelayFault::Swap) => {
                if let Some((other, words)) = proof.as_ref().and_then(|p| hub.relays.swapped(p)) {
                    if let Command::Send { text, .. } = &mut command { *text = words; }
                    proof = Some(other);
                }
            }
            None => {}
        }
    }
    let location = hub.harness.location(recipient).ok_or_else(|| unavailable("recipient owner is unavailable"))?;
    command = hub.harness.route_attachments(hub, recipient, &location, command).await.map_err(|_| unavailable("communication attachments are waiting for their owner"))?;
    let command = Command::Admit { peer: delivery.publication.sender.clone(), token: delivery.token, epoch: delivery.epoch, generation: capacity.feedback_generation.unwrap_or(capacity.generation), peers: population.to_vec(), command: Box::new(command) };
    let _ = send(hub, recipient, command, proof.as_ref()).await?;
    Ok(())
}

async fn drain_recipient(hub: &Arc<Hub>, recipient: &str) -> Result<(), Error> {
    let Some(current) = capacity(hub, recipient) else { return Ok(()) };
    if !super::mail_transfer::dispatch_ready(hub, recipient, &current).await.map_err(|_| unavailable("owner handover state is unavailable"))? { return Ok(()); }
    let Some(connection) = hub.harness.mail_connection(hub, recipient) else { return Ok(()) };
    let population = peers(hub, recipient).await?;
    let demand = mail::waiting_peers(&hub.store, recipient.to_owned(), 0, super::state_view::FRAME).await?;
    if demand.next.is_some() { return Err(Error::TooLarge { revision: demand.next.unwrap_or(0), budget: super::state_view::FRAME }); }
    let _ = send(hub, recipient, Command::MailPeers { epoch: current.epoch, peers: population.clone(), demand: demand.items }, None).await?;
    {
        let mut after = 0;
        loop {
            let page = mail::attempts(&hub.store, recipient.to_owned(), after, super::state_view::FRAME).await?;
            for delivery in page.items {
                if delivery.epoch != current.epoch { continue; }
                let revision = delivery.token.revision;
                drop(delivery);
                // Reconnect/startup replay keeps the exact token. Capacity and ledger writes
                // cannot manufacture a new connection identity or repeat the same transmission.
                if let Some(replay) = mail::replay_at(&hub.store, revision, current.epoch, connection.clone(), super::state_view::FRAME).await? {
                    deliver(hub, replay, &current, &population).await?;
                }
            }
            match page.next { Some(next) if next > after => after = next, _ => break }
        }
    }
    let Some(peer) = current.eligible_peer.clone() else { return Ok(()) };
    if current.admitted >= current.target { return Ok(()) }
    let mut after = 0;
    loop {
        let page = mail::pending_for(&hub.store, recipient.to_owned(), peer.clone(), after, super::state_view::FRAME).await?;
        for receipt in page.items {
            // The persisted observed epoch/capacity/connection gate refuses bookkeeping wakes.
            // Only a proven preaccept Waiting attempt can gain a new delivery generation.
            if let Some(delivery) = mail::claim_at(&hub.store, receipt.revision, current.epoch, current.retry_permission(), connection.clone(), super::state_view::FRAME).await? {
                deliver(hub, delivery, &current, &population).await?;
                return Ok(());
            }
        }
        match page.next { Some(next) if next > after => after = next, _ => return Ok(()) }
    }
}

async fn drain(hub: &Arc<Hub>) -> Result<(), Error> {
    let mut after = 0;
    loop {
        let page = mail::recipients(&hub.store, after, super::state_view::FRAME).await?;
        for recipient in page.items {
            if let Err(error) = drain_recipient(hub, &recipient).await {
                tracing::warn!(session = %recipient, error = %error, "durable communication remains held");
            }
        }
        match page.next { Some(next) if next > after => after = next, _ => return Ok(()) }
    }
}

/// The server supervisor owns this single task. Register every event source before reading;
/// compare committed revision and source generation again before waiting, closing lost wakes.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let committed = hub.store.changes().watch(mail::TABLES);
    loop {
        let source = hub.harness.mail_delivery.changed.listen();
        let revision = hub.store.changes().of(mail::TABLES);
        let generation = hub.harness.mail_delivery.generation.load(Ordering::Acquire);
        if let Err(error) = crate::programs::resume_notices(&hub).await {
            tracing::warn!(error = %error, "program communication remains waiting for a source or store event");
        }
        if let Err(error) = drain(&hub).await {
            tracing::warn!(error = %error, "durable communication pump waits for a source or store event");
        }
        if revision != hub.store.changes().of(mail::TABLES) || generation != hub.harness.mail_delivery.generation.load(Ordering::Acquire) { continue; }
        futures::pin_mut!(source);
        futures::select_biased! { () = source.fuse() => {}, () = committed.notified().fuse() => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_recipient_refuses_new_custody_but_keeps_an_existing_exact_receipt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let command = Command::Send { id: "one".into(), text: "original words".into(), mode: super::super::model::SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            let publication = Publish { owner: "fixture".into(), sender: "fixture".into(), recipient: "unknown".into(), id: "one".into(), command: serde_json::to_value(command)?, provenance: None, status: None };
            assert_eq!(publish(&hub, publication.clone()).await, Err(Error::NoSession));
            assert!(mail::receipt(&hub.store, "fixture".into(), "fixture".into(), "unknown".into(), "one".into()).await?.is_none());
            // A committed original predating closure remains authoritative; absence cannot
            // turn its exact retry into new custody or a different publication.
            let original = mail::publish(&hub.store, publication.clone(), u64::MAX, 1).await?;
            let repeated = publish(&hub, publication).await?;
            assert!(repeated.repeat);
            assert_eq!(repeated.revision, original.revision);
            assert_eq!(repeated.disposition, original.disposition);
            Ok(())
        })
    }
}

pub fn event_capacity(event: &FlowEvent) -> &Capacity {
    event.capacity()
}

/// Called only after current authenticated source ownership and epoch have been established.
/// A failed store transition returns without MailAck, leaving the source replay journal intact.
pub async fn flow(hub: &Arc<Hub>, recipient: &str, source: Location, event: FlowEvent) -> Result<(), Error> {
    let terminal = match &event {
        FlowEvent::Consumed { boundary, .. } => Some(super::admission::Disposition::Consumed { boundary: *boundary }),
        FlowEvent::Disposed { disposition, .. } => Some(disposition.clone()),
        _ => None,
    };
    let current = event_capacity(&event).clone();
    if current.epoch != hub.harness.epoch_of(recipient) || hub.harness.location(recipient).as_ref() != Some(&source) { return Err(unavailable("stale source mail owner or capacity")); }
    let mut terminal_replay = None;
    let receipt = match &event {
        FlowEvent::Capacity { .. } | FlowEvent::Fault { .. } => None,
        FlowEvent::Accepted { token, epoch, .. } | FlowEvent::Consumed { token, epoch, .. } | FlowEvent::Waiting { token, epoch, .. } | FlowEvent::Disposed { token, epoch, .. } => {
            let (token, epoch) = (*token, *epoch);
            let attempt = mail::attempt(&hub.store, recipient.to_owned(), token, epoch).await?;
            if attempt.is_none() && epoch == current.epoch {
                let projection = mail::terminal_projection(&hub.store, recipient.to_owned(), token).await?;
                let matching = projection.as_ref().is_some_and(|p| matches!((&event, p.publication.disposition),
                    (FlowEvent::Consumed { .. }, mail::Disposition::Consumed)
                    | (FlowEvent::Disposed { disposition: super::admission::Disposition::Cancelled { .. }, .. }, mail::Disposition::Cancelled)));
                if matching {
                    terminal_replay = projection;
                }
            }
            if let Some(projection) = &terminal_replay {
                Some((projection.publication.clone(), token, epoch))
            } else {
            let Some(attempt) = attempt else { return Err(Error::Stale { revision: token.revision, generation: token.generation }); };
            let transferred_here = matches!(&event, FlowEvent::Disposed { disposition: super::admission::Disposition::Transferred { epoch: destination }, .. } if *destination == current.epoch);
            if (epoch != current.epoch && !transferred_here) || hub.harness.location(recipient).as_ref() != Some(&source) { return Err(Error::Stale { revision: token.revision, generation: token.generation }); }
            let committed = match &event {
                FlowEvent::Accepted { .. } => mail::mark_admitted(&hub.store, token, epoch).await?,
                FlowEvent::Consumed { .. } => mail::mark_consumed(&hub.store, token, epoch).await?,
                // Provider uncertainty after admission cannot revoke custody or mint another
                // claim. Preserve the attempt while exposing its source waiting reason.
                FlowEvent::Waiting { .. } if attempt.disposition == mail::Disposition::Admitted => attempt.clone(),
                FlowEvent::Waiting { .. } => mail::mark_waiting(&hub.store, token).await?,
                FlowEvent::Disposed { disposition, .. } => mail::mark_disposed(&hub.store, token, epoch, disposition.clone()).await?,
                FlowEvent::Capacity { .. } | FlowEvent::Fault { .. } => return Err(unavailable("mail receipt has no attempt")),
            };
            if committed.id != attempt.id { return Err(unavailable("mail receipt identity changed")); }
            Some((committed, token, epoch))
            }
        }
    };
    let projection = if let Some((committed, token, epoch)) = &receipt {
        let projection = match terminal_replay {
            Some(projection) => Some(projection),
            None => mail::projection(&hub.store, recipient.to_owned(), *token, *epoch).await?,
        };
        let Some(mut projection) = projection else { return Err(unavailable("committed mail receipt could not be projected")); };
        let latest = &projection.publication;
        if latest.id != committed.id || latest.revision != committed.revision || latest.generation != committed.generation {
            return Err(unavailable("committed mail receipt projection changed identity"));
        }
        if latest.disposition == committed.disposition { projection.publication = committed.clone(); }
        else { projection.publication.repeat = true; }
        crate::programs::mail_progress(hub, recipient, &projection.publication, &projection.mail_scope).await.map_err(|_| unavailable("program communication progress could not be retained"))?;
        Some(projection)
    } else { None };
    // Replayed terminal receipts reconcile custody but cannot pose as another model handoff.
    let projected_event = if projection.as_ref().is_some_and(|p| p.publication.repeat) { FlowEvent::Capacity { capacity: current.clone() } } else { event };
    let internal_progress = projection.clone();
    if !hub.harness.mail_event(hub, recipient, source.clone(), projected_event, projection).await { return Err(unavailable("source owner changed before receipt acknowledgement")); }
    if let Some(projection) = &internal_progress {
        super::told::mail_progress(hub, recipient, &projection.publication, &projection.mail_scope);
    }
    if let Some((receipt, token, epoch)) = receipt {
        if hub.harness.location(recipient).as_ref() != Some(&source) || hub.harness.epoch_of(recipient) != current.epoch { return Err(unavailable("source owner changed before receipt acknowledgement")); }
        if terminal.is_some() {
            let _ = send(hub, recipient, Command::MailAck { message: receipt.id.clone(), token, epoch, disposition: terminal }, None).await?;
        }
        hub.harness.mail_receipt(recipient, receipt);
    }
    Ok(())
}
