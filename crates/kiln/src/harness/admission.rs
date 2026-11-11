//! Receiver-owned application credit (queue-prevention/ADMISSION.md). Only a correlated provider
//! acceptance returns consumption credit; inbox ACKs and movement between buffers do not.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use crate::memory::owners::HARNESS_INPUT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Learning { Bootstrap, Observed }

/// The durable sender's exact waiting head. Fairness population also includes admitted peers;
/// a waiting revision is the only authority for starting that peer's natural refill witness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Demand { pub peer: String, pub revision: u64 }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Capacity {
    pub epoch: u64,
    pub generation: u64,
    /// Permission for another attempt after preaccept refusal; presentation changes are not
    /// new credit. Absent only on a legacy source that predates this distinction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub retry_generation: Option<u64>,
    pub learning: Learning,
    pub target: usize,
    pub admitted: usize,
    pub bytes: usize,
    pub peers: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub eligible_peer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub feedback_generation: Option<u64>,
}

impl Capacity { pub fn retry_permission(&self) -> u64 { self.retry_generation.unwrap_or(self.generation) } }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum WaitCause { Bootstrap, LogicalWindow, PeerTurn, PhysicalBytes, Fenced, Restoring, ConsumptionUnknown, SourceUnavailable }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Consumption { ProviderAccepted, ProviderCompleted, WaitHandoff }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AdmissionFault"))]
#[serde(rename_all = "snake_case")]
pub enum Fault { OwnerOnly }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum FlowEvent {
    Accepted { token: super::mail::Token, epoch: u64, capacity: Capacity },
    Consumed { token: super::mail::Token, epoch: u64, boundary: Consumption, capacity: Capacity },
    Waiting { token: super::mail::Token, epoch: u64, cause: WaitCause, capacity: Capacity },
    Capacity { capacity: Capacity },
    Disposed { token: super::mail::Token, epoch: u64, disposition: Disposition, capacity: Capacity },
    Fault { epoch: u64, code: Fault, capacity: Capacity },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage { Reserved, InboxAccepted, InCtl, Carried, Queued, ProviderFlight }

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum Replay { #[default] Waiting, ProviderReady }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff { pub id: String, pub turn: Option<i64>, pub digest: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffProof { pub handoff: Handoff, pub messages: Vec<String>, pub native_id: String }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FlightProtocol { Claude, Codex, Acp, Agy }

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LegacyFlight { pub id: String, pub protocol: FlightProtocol, pub request: Option<u64> }

/// The four supported adapters' prompt envelopes. Recovery/control requests and native tool
/// replies stay live while admission is restoring; only new model input is held at that barrier.
pub fn native_input(wire: &str) -> bool {
    use std::borrow::Cow;
    #[derive(Deserialize)]
    struct Envelope<'a> {
        #[serde(default, borrow, rename = "type")] kind: Option<Cow<'a, str>>,
        #[serde(default, borrow)] method: Option<Cow<'a, str>>,
        #[serde(default, borrow)] event: Option<Cow<'a, str>>,
    }
    let Ok(envelope) = serde_json::from_str::<Envelope<'_>>(wire) else { return true };
    envelope.kind.as_deref() == Some("user") || envelope.event.as_deref() == Some("user") || matches!(envelope.method.as_deref(), Some("turn/start" | "turn/steer" | "session/prompt"))
}

impl LegacyFlight {
    /// Match only this provider's exact emitted envelope fields. No generic identity search,
    /// textual tags, or correlation by nearby timestamps is used for an opaque retained frame.
    pub fn matches(&self, wire: &str) -> bool {
        use std::borrow::Cow;
        #[derive(Deserialize)]
        struct Params<'a> { #[serde(default, borrow, rename = "clientUserMessageId")] client: Option<Cow<'a, str>> }
        #[derive(Deserialize)]
        struct Envelope<'a> {
            #[serde(default)] id: Option<u64>,
            #[serde(default, borrow)] uuid: Option<Cow<'a, str>>,
            #[serde(default, borrow)] event: Option<Cow<'a, str>>,
            #[serde(default, borrow)] method: Option<Cow<'a, str>>,
            #[serde(default, borrow)] params: Option<Params<'a>>,
            #[serde(default, borrow, rename = "type")] kind: Option<Cow<'a, str>>,
        }
        let Ok(envelope) = serde_json::from_str::<Envelope<'_>>(wire) else { return false };
        match self.protocol {
            FlightProtocol::Claude => envelope.kind.as_deref() == Some("user") && envelope.uuid.as_deref() == Some(self.id.as_str()),
            FlightProtocol::Codex => matches!(envelope.method.as_deref(), Some("turn/start" | "turn/steer")) && envelope.params.as_ref().and_then(|params| params.client.as_deref()) == Some(self.id.as_str()),
            FlightProtocol::Acp => envelope.method.as_deref() == Some("session/prompt") && self.request.is_some() && envelope.id == self.request,
            FlightProtocol::Agy => envelope.event.as_deref() == Some("user"), // exactly one active turn, as this adapter owns it
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub message: String,
    pub peer: String,
    pub epoch: u64,
    pub bytes: usize,
    pub stage: Stage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<super::mail::Token>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum CancelReason { User, ProviderRefused, SessionEnded, Superseded, BeforeAcceptance }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum Disposition {
    Consumed { boundary: Consumption },
    Cancelled { reason: CancelReason },
    Transferred { epoch: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Reserved(Lease),
    Existing(Lease),
    Waiting { epoch: u64, generation: u64, cause: WaitCause },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error { Overflow, Allocation, MissingLease, StaleEpoch, InvalidTransition }

/// Owned communication storage, including vector element places and string capacities. JSON
/// length alone undercounts a collection of short attachments and is not resident authority.
pub fn command_bytes(command: &super::model::Command) -> Result<usize, Error> {
    command_storage(command, false)
}

fn command_storage(command: &super::model::Command, copy: bool) -> Result<usize, Error> {
    use super::model::Command;
    let mut bytes = std::mem::size_of::<Command>();
    let mut add = |n: usize| { bytes = bytes.checked_add(n).ok_or(Error::Overflow)?; Ok::<(), Error>(()) };
    let string = |value: &String| if copy { value.len() } else { value.capacity() };
    match command {
        Command::Send { id, text, attachments, overrides, reply_to, task, .. } => {
            add(string(id))?; add(string(text))?;
            add((if copy { attachments.len() } else { attachments.capacity() }).checked_mul(std::mem::size_of::<super::model::Attachment>()).ok_or(Error::Overflow)?)?;
            for attachment in attachments {
                for value in [&attachment.name, &attachment.mime, &attachment.data_base64] { add(string(value))?; }
                for value in [&attachment.file_id, &attachment.path].into_iter().flatten() { add(string(value))?; }
            }
            if let Some(overrides) = overrides { for value in [&overrides.model, &overrides.effort].into_iter().flatten() { add(string(value))?; } }
            for value in [reply_to, task].into_iter().flatten() { add(string(value))?; }
        }
        Command::SkillRun { name, args, id, .. } => {
            add(string(name))?;
            for value in [args, id].into_iter().flatten() { add(string(value))?; }
        }
        _ => return Err(Error::InvalidTransition),
    }
    Ok(bytes)
}

/// Before cloning a retained queue body for legacy reconciliation, reserve its actual owned
/// string/vector capacities. The source item remains in its queue until its journal is durable.
pub fn queued_command_bytes(queued: &super::model::QueueItem, task: Option<&str>) -> Result<usize, Error> {
    let mut bytes = std::mem::size_of::<super::model::Command>();
    let mut add = |n: usize| { bytes = bytes.checked_add(n).ok_or(Error::Overflow)?; Ok::<(), Error>(()) };
    add(queued.id.capacity())?; add(queued.text.capacity())?;
    add(queued.attachments.capacity().checked_mul(std::mem::size_of::<super::model::Attachment>()).ok_or(Error::Overflow)?)?;
    for attachment in &queued.attachments {
        for value in [&attachment.name, &attachment.mime, &attachment.data_base64] { add(value.capacity())?; }
        for value in [&attachment.file_id, &attachment.path].into_iter().flatten() { add(value.capacity())?; }
    }
    if let Some(overrides) = &queued.overrides { for value in [&overrides.model, &overrides.effort].into_iter().flatten() { add(value.capacity())?; } }
    if let Some(reply) = &queued.reply_to { add(reply.capacity())?; }
    if let Some(task) = task { add(task.len())?; }
    Ok(bytes)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    #[serde(alias = "message", deserialize_with = "receipt_messages")]
    pub messages: Vec<String>,
    pub disposition: Disposition,
    pub at_ns: u64,
    pub continuously_available: bool,
}

fn receipt_messages<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Messages { One(String), Batch(Vec<String>) }
    Ok(match Messages::deserialize(deserializer)? { Messages::One(message) => vec![message], Messages::Batch(messages) => messages })
}

/// Attached to the existing session owner. Journal effects run in its normal flush, outside the
/// Core lock; adapters only record a correlated receipt at the boundary they actually observed.
#[derive(Clone)]
pub struct Input {
    pub controller: Arc<Mutex<Controller>>,
    pub inbox: Arc<super::inbox::Inbox>,
    born: std::time::Instant,
    physical: Arc<PhysicalBudget>,
    account: Arc<AtomicUsize>,
    observed_physical: Arc<Mutex<PhysicalSnapshot>>,
}

impl Input {
    pub fn new(epoch: u64, physical: Arc<PhysicalBudget>, inbox: Arc<super::inbox::Inbox>) -> Self {
        let controller = Controller::new(epoch, Arc::clone(&physical));
        let account = Arc::clone(&controller.account);
        let counts = physical.snapshot(&account);
        Self { controller: Arc::new(Mutex::new(controller)), inbox, born: std::time::Instant::now(), physical, account, observed_physical: Arc::new(Mutex::new(counts)) }
    }
    pub fn now_ns(&self) -> u64 { u64::try_from(self.born.elapsed().as_nanos()).unwrap_or(u64::MAX) }
    pub fn reserve_buffer(&self, bytes: usize) -> Result<Option<BufferCharge>, Error> { self.physical.reserve_buffer_for(bytes, Some(Arc::clone(&self.account))) }
    /// Only attachment preparation requires a second original command representation. Reserve
    /// its owned storage before making that fallible copy; ordinary text input stays borrowed.
    pub fn clone_command(&self, command: &super::model::Command) -> Result<Option<(super::model::Command, BufferCharge)>, Error> {
        use super::model::{Attachment, Command, Overrides};
        fn words(value: &str) -> Result<String, Error> { let mut out = String::new(); out.try_reserve_exact(value.len()).map_err(|_| Error::Allocation)?; out.push_str(value); Ok(out) }
        fn optional(value: &Option<String>) -> Result<Option<String>, Error> { value.as_deref().map(words).transpose() }
        let Some(charge) = self.reserve_buffer(command_storage(command, true)?)? else { return Ok(None) };
        let copied = match command {
            Command::Send { id, text, mode, attachments, overrides, reply_to, task } => {
                let mut copied = Vec::new();
                copied.try_reserve_exact(attachments.len()).map_err(|_| Error::Allocation)?;
                for attachment in attachments { copied.push(Attachment { name: words(&attachment.name)?, mime: words(&attachment.mime)?, data_base64: words(&attachment.data_base64)?, file_id: optional(&attachment.file_id)?, path: optional(&attachment.path)? }); }
                Command::Send { id: words(id)?, text: words(text)?, mode: *mode, attachments: copied, overrides: overrides.as_ref().map(|value| Ok::<_, Error>(Overrides { model: optional(&value.model)?, effort: optional(&value.effort)? })).transpose()?, reply_to: optional(reply_to)?, task: optional(task)? }
            }
            Command::SkillRun { name, args, id, mode } => Command::SkillRun { name: words(name)?, args: optional(args)?, id: optional(id)?, mode: *mode },
            _ => return Err(Error::InvalidTransition),
        };
        Ok(Some((copied, charge)))
    }
    pub fn observe_physical(&self) -> bool {
        let now = self.physical.snapshot(&self.account);
        let mut observed = self.observed_physical.lock();
        let changed = *observed != now;
        // Mark the bytes this operation already projected. A concurrent foreign release or
        // bound change remains pending for the owner's event path; it cannot be swallowed here.
        observed.held = now.held;
        changed
    }
    pub fn take_budget_change(&self) -> Option<bool> {
        let now = self.physical.snapshot(&self.account);
        let mut observed = self.observed_physical.lock();
        if *observed == now { return None; }
        let improved = now.bound > observed.bound || now.foreign < observed.foreign;
        *observed = now;
        Some(improved)
    }
    pub async fn physical_changed(&self) {
        loop {
            let listener = self.physical.changed.listen();
            if *self.observed_physical.lock() != self.physical.snapshot(&self.account) { return; }
            listener.await;
        }
    }
}

/// One host-wide strict byte authority, shared by every session. The coordinator supplies its
/// bound from resource facts; no default allowance or oversized-item exception exists here.
pub struct PhysicalBudget { state: Mutex<Physical>, changed: crate::rt::Changed }

/// Authority for one real temporary representation. Move this guard with its allocation; do
/// not drop it when an encoded buffer is still in a bridge, transfer or provider queue.
pub struct BufferCharge { physical: Arc<PhysicalBudget>, account: Option<Arc<AtomicUsize>>, bytes: usize }

impl BufferCharge {
    pub fn bytes(&self) -> usize { self.bytes }
    pub fn try_resize(&mut self, bytes: usize) -> Result<bool, Error> {
        if bytes > self.bytes && !self.physical.reserve_for(bytes.checked_sub(self.bytes).ok_or(Error::Overflow)?, self.account.as_deref())? { return Ok(false); }
        if self.bytes > bytes { self.physical.release_for(self.bytes.checked_sub(bytes).ok_or(Error::Overflow)?, self.account.as_deref()); }
        self.bytes = bytes;
        Ok(true)
    }
}

impl Drop for BufferCharge { fn drop(&mut self) { self.physical.release_for(self.bytes, self.account.as_deref()); } }

#[derive(Default)]
struct Physical { bound: usize, held: usize }

#[derive(Clone, Copy, PartialEq, Eq)]
struct PhysicalSnapshot { held: usize, bound: usize, foreign: usize }

impl PhysicalBudget {
    pub fn new(bound: usize) -> Arc<Self> {
        HARNESS_INPUT.bound_add(bound);
        Arc::new(Self { state: Mutex::new(Physical { bound, held: 0 }), changed: crate::rt::Changed::new() })
    }
    pub fn resize(&self, bound: usize) {
        let mut p = self.state.lock();
        if p.bound == bound { return; }
        if bound > p.bound { HARNESS_INPUT.bound_add(bound.saturating_sub(p.bound)); }
        else { HARNESS_INPUT.bound_sub(p.bound.saturating_sub(bound)); }
        p.bound = bound;
        drop(p);
        self.changed.notify();
    }
    pub fn counts(&self) -> (usize, usize) { let p = self.state.lock(); (p.held, p.bound) }
    /// A coalesced transfer recovery owner observes actual pool changes. Subscribe before
    /// reading so a release racing registration cannot leave a held header asleep.
    pub(crate) async fn changed_since(&self, counts: (usize, usize)) {
        loop {
            let changed = self.changed.listen();
            if self.counts() != counts { return; }
            changed.await;
        }
    }
    fn snapshot(&self, account: &AtomicUsize) -> PhysicalSnapshot {
        let p = self.state.lock();
        PhysicalSnapshot { held: p.held, bound: p.bound, foreign: p.held.saturating_sub(account.load(Ordering::Relaxed)) }
    }
    pub fn reserve_buffer(self: &Arc<Self>, bytes: usize) -> Result<Option<BufferCharge>, Error> {
        self.reserve_buffer_for(bytes, None)
    }
    fn reserve_buffer_for(self: &Arc<Self>, bytes: usize, account: Option<Arc<AtomicUsize>>) -> Result<Option<BufferCharge>, Error> {
        if !self.reserve_for(bytes, account.as_deref())? { return Ok(None); }
        Ok(Some(BufferCharge { physical: Arc::clone(self), account, bytes }))
    }
    fn reserve_for(&self, bytes: usize, account: Option<&AtomicUsize>) -> Result<bool, Error> {
        let mut p = self.state.lock();
        let after = p.held.checked_add(bytes).ok_or(Error::Overflow)?;
        if after > p.bound { return Ok(false); }
        if let Some(account) = account { account.store(account.load(Ordering::Relaxed).checked_add(bytes).ok_or(Error::Overflow)?, Ordering::Relaxed); }
        p.held = after;
        HARNESS_INPUT.add(bytes);
        drop(p);
        self.changed.notify();
        Ok(true)
    }

    /// Restored accepted work is debt: account it even above a newly smaller authority.
    fn restore(&self, bytes: usize, account: &AtomicUsize) -> Result<(), Error> {
        let mut p = self.state.lock();
        let total = p.held.checked_add(bytes).ok_or(Error::Overflow)?;
        let owned = account.load(Ordering::Relaxed).checked_add(bytes).ok_or(Error::Overflow)?;
        p.held = total;
        account.store(owned, Ordering::Relaxed);
        HARNESS_INPUT.add(bytes);
        drop(p);
        self.changed.notify();
        Ok(())
    }

    fn release_for(&self, bytes: usize, account: Option<&AtomicUsize>) {
        let mut p = self.state.lock();
        let released = bytes.min(p.held).min(account.map_or(bytes, |account| account.load(Ordering::Relaxed)));
        if let Some(account) = account { account.store(account.load(Ordering::Relaxed).saturating_sub(released), Ordering::Relaxed); }
        p.held = p.held.saturating_sub(released);
        HARNESS_INPUT.sub(released);
        drop(p);
        if released > 0 { self.changed.notify(); }
    }
}

impl Drop for PhysicalBudget {
    fn drop(&mut self) {
        let p = self.state.get_mut();
        HARNESS_INPUT.sub(p.held);
        HARNESS_INPUT.bound_sub(p.bound);
    }
}

#[derive(Clone, Copy)]
struct Service { count: usize, elapsed_ns: u64, quantum: usize }

#[derive(Clone, Copy)]
struct Feedback { generation: u64, revision: u64, issued_ns: u64 }

/// Observations cover one natural busy consumption/refill cycle. A lifecycle change invalidates
/// the cycle; time passing and arrivals cannot enlarge the target.
#[derive(Default)]
struct Observations {
    previous_consumption: Option<u64>,
    service: Option<Service>,
    feedback_ns: Option<u64>,
    outstanding: BTreeMap<String, Feedback>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Readiness { Ready, Restoring, Unavailable }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ownership { Here, Fenced }

pub struct Controller {
    epoch: u64,
    generation: u64,
    retry_generation: u64,
    leases: BTreeMap<String, Lease>,
    peers: VecDeque<String>,
    demand: BTreeMap<String, u64>,
    physical: Arc<PhysicalBudget>,
    account: Arc<AtomicUsize>,
    observations: Observations,
    target: usize,
    learning: Learning,
    readiness: Readiness,
    ownership: Ownership,
    available: bool,
}

impl Controller {
    pub fn new(epoch: u64, physical: Arc<PhysicalBudget>) -> Self {
        // These are two necessary pipeline roles, not a selected backlog allowance: a real item
        // in service and one real refill item permit natural busy service to be observed.
        let roles = [Stage::ProviderFlight, Stage::Queued];
        Self { epoch, generation: 0, retry_generation: 0, leases: BTreeMap::new(), peers: VecDeque::new(), demand: BTreeMap::new(), physical, account: Arc::new(AtomicUsize::new(0)), observations: Observations::default(), target: roles.len(), learning: Learning::Bootstrap, readiness: Readiness::Ready, ownership: Ownership::Here, available: true }
    }

    pub fn capacity(&self) -> Capacity {
        let eligible_peer = (self.accepting() && self.leases.len() < self.target).then(|| self.next_peer().cloned()).flatten();
        let feedback_generation = eligible_peer.as_ref().and_then(|peer| self.observations.outstanding.get(peer)).map(|feedback| feedback.generation);
        Capacity { epoch: self.epoch, generation: self.generation, retry_generation: Some(self.retry_generation), learning: self.learning, target: if self.accepting() { self.target } else { 0 }, admitted: self.leases.len(), bytes: self.leases.values().map(|l| l.bytes).fold(0, usize::saturating_add), peers: self.peers.len(), eligible_peer, feedback_generation }
    }

    pub fn lease(&self, message: &str) -> Option<&Lease> { self.leases.get(message) }
    pub fn register_handoff(&mut self, messages: &[String], handoff: &Handoff) -> Result<bool, Error> {
        let mut growth = 0usize;
        for message in messages {
            let lease = self.leases.get(message).ok_or(Error::MissingLease)?;
            if lease.handoff.as_ref().is_some_and(|current| current != handoff) { return Err(Error::InvalidTransition); }
            if lease.handoff.is_none() { growth = growth.checked_add(handoff.id.len()).and_then(|bytes| bytes.checked_add(handoff.digest.len())).ok_or(Error::Overflow)?; }
        }
        if !self.physical.reserve_for(growth, Some(&self.account))? { return Ok(false); }
        for message in messages {
            if let Some(lease) = self.leases.get_mut(message) && lease.handoff.is_none() {
                lease.bytes = lease.bytes.checked_add(handoff.id.len()).and_then(|bytes| bytes.checked_add(handoff.digest.len())).ok_or(Error::Overflow)?;
                lease.handoff = Some(handoff.clone());
            }
        }
        if growth != 0 { self.changed()?; }
        Ok(true)
    }

    pub fn withdraw_handoff(&mut self, messages: &[String], handoff: &Handoff) {
        let mut released = 0usize;
        for message in messages {
            if let Some(lease) = self.leases.get_mut(message) && lease.handoff.as_ref() == Some(handoff)
                && let Some(previous) = lease.handoff.take()
            {
                let bytes = previous.id.capacity().saturating_add(previous.digest.capacity());
                lease.bytes = lease.bytes.saturating_sub(bytes);
                released = released.saturating_add(bytes);
            }
        }
        self.physical.release_for(released, Some(&self.account));
    }
    /// Resize the same resident lease. Growth reserves first; failure changes nothing.
    pub fn resize(&mut self, message: &str, bytes: usize) -> Result<bool, Error> {
        let old = self.leases.get(message).ok_or(Error::MissingLease)?.bytes;
        if bytes > old && !self.physical.reserve_for(bytes.checked_sub(old).ok_or(Error::Overflow)?, Some(&self.account))? { return Ok(false); }
        if let Some(lease) = self.leases.get_mut(message) { lease.bytes = bytes; }
        if old > bytes { self.physical.release_for(old.checked_sub(bytes).ok_or(Error::Overflow)?, Some(&self.account)); }
        if bytes != old { self.changed()?; }
        Ok(true)
    }
    pub fn budget_changed(&mut self, improved: bool) -> Result<(), Error> { if improved { self.credit()?; } self.changed() }
    pub fn provider_ready(&mut self) -> Result<(), Error> { self.credit()?; self.changed() }
    /// Called only after the journal CAS succeeded and this owner synchronously applied that
    /// edit. Reservation/rollback and a persisted-but-unapplied edit are not released capacity.
    pub fn applied_edit(&mut self, message: &str, before: usize, after: usize) -> Result<bool, Error> {
        if after >= before || !self.leases.get(message).is_some_and(|lease| lease.bytes == before && lease.stage != Stage::Reserved) { return Ok(false); }
        if !self.resize(message, after)? { return Err(Error::InvalidTransition); }
        self.credit()?;
        self.changed()?;
        Ok(true)
    }
    pub fn writes_ready(&self) -> bool { self.readiness == Readiness::Ready && self.ownership == Ownership::Here }
    fn accepting(&self) -> bool { self.readiness == Readiness::Ready && self.ownership == Ownership::Here && self.available }
    fn next_peer(&self) -> Option<&String> { self.peers.iter().find(|peer| self.demand.contains_key(*peer)) }
    pub fn initial_epoch(&mut self, epoch: u64) -> Result<(), Error> {
        if !self.leases.is_empty() { return Err(Error::StaleEpoch); }
        self.epoch = epoch;
        Ok(())
    }

    fn changed(&mut self) -> Result<(), Error> { self.generation = self.generation.checked_add(1).ok_or(Error::Overflow)?; Ok(()) }
    fn credit(&mut self) -> Result<(), Error> { self.retry_generation = self.retry_generation.checked_add(1).ok_or(Error::Overflow)?; Ok(()) }

    /// Authenticated peers with unresolved custody, supplied by lifecycle events. Their arrival
    /// order is retained; newly active peers divide the same target and reset obsolete samples.
    pub fn peers(&mut self, active: &[String]) -> Result<bool, Error> {
        let same = self.peers.len() == active.len() && active.iter().all(|p| self.peers.contains(p));
        if same { return Ok(false); }
        self.peers.retain(|p| active.contains(p));
        for peer in active { if !self.peers.contains(peer) { self.peers.push_back(peer.clone()); } }
        self.demand.retain(|peer, _| self.peers.contains(peer));
        self.observations = Observations::default();
        self.learning = Learning::Bootstrap;
        self.target = [Stage::ProviderFlight, Stage::Queued].len();
        self.changed()?;
        Ok(true)
    }

    /// Waiting demand is supplied by the durable sender ledger, separately from the fairness
    /// population (which also includes already admitted communications). No arrival/ACK sample.
    pub fn demand(&mut self, waiting: &[Demand]) -> Result<bool, Error> {
        let next: BTreeMap<_, _> = waiting.iter().filter(|head| self.peers.contains(&head.peer)).map(|head| (head.peer.clone(), head.revision)).collect();
        if self.demand == next { return Ok(false); }
        // Waiting→Dispatched withdraws a head before its Admit can arrive. Keep that one exact
        // witness until its matching publication answers, peer lifecycle ends, or another head
        // replaces it. A later idle producer publication cannot match the old revision.
        self.observations.outstanding.retain(|peer, feedback| next.get(peer).is_none_or(|revision| *revision == feedback.revision));
        self.demand = next;
        if self.leases.is_empty() && self.demand.is_empty() && self.observations.outstanding.is_empty() {
            self.observations.previous_consumption = None;
            self.observations.feedback_ns = None;
        }
        self.changed()?;
        Ok(true)
    }

    /// A true hold/pause transition invalidates the natural cycle without evicting accepted
    /// debt. Unrelated status changes and repeated resource measurements are no event here.
    pub fn available(&mut self, available: bool) -> Result<bool, Error> {
        if self.available == available { return Ok(false); }
        self.available = available;
        self.observations = Observations::default();
        if available { self.credit()?; }
        self.changed()?;
        Ok(true)
    }

    pub fn fence(&mut self, on: bool) -> Result<(), Error> {
        let ownership = if on { Ownership::Fenced } else { Ownership::Here };
        if self.ownership != ownership {
            self.ownership = ownership;
            self.observations = Observations::default();
            if ownership == Ownership::Here { self.credit()?; }
            self.changed()?;
        }
        Ok(())
    }

    pub fn restoring(&mut self) -> Result<(), Error> {
        if self.readiness == Readiness::Restoring { return Ok(()); }
        self.readiness = Readiness::Restoring;
        self.observations = Observations::default();
        self.changed()
    }
    pub fn restore_complete(&mut self) -> Result<(), Error> {
        if self.readiness != Readiness::Restoring { return Err(Error::InvalidTransition); }
        self.readiness = Readiness::Ready;
        self.credit()?;
        self.changed()
    }
    pub fn unavailable(&mut self) -> Result<(), Error> {
        if self.readiness == Readiness::Unavailable { return Ok(()); }
        self.readiness = Readiness::Unavailable;
        self.observations = Observations::default();
        self.changed()
    }

    fn waiting(&self, cause: WaitCause) -> Admission { Admission::Waiting { epoch: self.epoch, generation: self.generation, cause } }

    /// Reserve before inbox acceptance and before any lane/ctl enqueue. An offer carries the
    /// capacity generation it actually answered, on this receiver's monotonic clock.
    pub fn offer(&mut self, message: &str, peer: &str, epoch: u64, bytes: usize, feedback_generation: Option<u64>, now_ns: u64) -> Result<Admission, Error> {
        self.offer_inner(message, peer, epoch, bytes, feedback_generation, None, now_ns)
    }

    #[allow(clippy::too_many_arguments)] // Independent authenticated identity, authority and source-monotonic observation fields.
    pub fn offer_publication(&mut self, message: &str, peer: &str, epoch: u64, bytes: usize, feedback_generation: Option<u64>, revision: u64, now_ns: u64) -> Result<Admission, Error> {
        self.offer_inner(message, peer, epoch, bytes, feedback_generation, Some(revision), now_ns)
    }

    #[allow(clippy::too_many_arguments)] // Same protocol fields; legacy offers have no publication revision.
    fn offer_inner(&mut self, message: &str, peer: &str, epoch: u64, bytes: usize, feedback_generation: Option<u64>, revision: Option<u64>, now_ns: u64) -> Result<Admission, Error> {
        if epoch != self.epoch { return Err(Error::StaleEpoch); }
        if let Some(lease) = self.leases.get(message) { return Ok(Admission::Existing(lease.clone())); }
        if self.ownership == Ownership::Fenced { return Ok(self.waiting(WaitCause::Fenced)); }
        if !self.available { return Ok(self.waiting(WaitCause::ConsumptionUnknown)); }
        match self.readiness {
            Readiness::Restoring => return Ok(self.waiting(WaitCause::Restoring)),
            Readiness::Unavailable => return Ok(self.waiting(WaitCause::SourceUnavailable)),
            Readiness::Ready => {}
        }
        if self.leases.len() >= self.target { return Ok(self.waiting(if self.learning == Learning::Bootstrap { WaitCause::Bootstrap } else { WaitCause::LogicalWindow })); }
        let answered = self.observations.outstanding.get(peer).is_some_and(|feedback| Some(feedback.revision) == revision && Some(feedback.generation) == feedback_generation);
        if !self.peers.is_empty() && self.next_peer().is_none_or(|first| first != peer) && !answered { return Ok(self.waiting(WaitCause::PeerTurn)); }
        if !self.physical.reserve_for(bytes, Some(&self.account))? { return Ok(self.waiting(WaitCause::PhysicalBytes)); }
        let lease = Lease { message: message.to_owned(), peer: peer.to_owned(), epoch, bytes, stage: Stage::Reserved, token: None, handoff: None };
        self.leases.insert(message.to_owned(), lease.clone());
        if let Some(position) = self.peers.iter().position(|active| active == peer) && let Some(active) = self.peers.remove(position) { self.peers.push_back(active); }
        if let Some(feedback) = self.observations.outstanding.remove(peer)
            && feedback_generation == Some(feedback.generation) && revision == Some(feedback.revision)
        {
            self.observations.feedback_ns = now_ns.checked_sub(feedback.issued_ns).filter(|n| *n > 0);
            self.learn()?;
        }
        self.changed()?;
        Ok(Admission::Reserved(lease))
    }

    pub fn stage(&mut self, message: &str, stage: Stage) -> Result<(), Error> {
        let lease = self.leases.get_mut(message).ok_or(Error::MissingLease)?;
        lease.stage = stage;
        Ok(())
    }

    pub fn bind(&mut self, message: &str, token: super::mail::Token) -> Result<Lease, Error> {
        let lease = self.leases.get_mut(message).ok_or(Error::MissingLease)?;
        lease.token = Some(token);
        Ok(lease.clone())
    }

    pub fn restore(&mut self, lease: Lease) -> Result<(), Error> {
        if lease.epoch != self.epoch { return Err(Error::StaleEpoch); }
        if self.leases.contains_key(&lease.message) { return Ok(()); }
        self.physical.restore(lease.bytes, &self.account)?;
        self.leases.insert(lease.message.clone(), lease);
        self.observations = Observations::default();
        self.changed()
    }

    /// An explicit cancellation returns room but is not a service-rate observation. A transfer
    /// returns local bytes only once the destination has durably taken custody.
    pub fn finish(&mut self, message: &str, disposition: Disposition, now_ns: u64, continuously_available: bool) -> Result<bool, Error> {
        self.finish_batch(std::slice::from_ref(&message), disposition, now_ns, continuously_available).map(|count| count != 0)
    }

    /// One actual provider/tool response can hand over several communications. Its observed
    /// service quantum is the number of distinct live leases it consumed, never a preset one.
    pub fn finish_batch(&mut self, messages: &[&str], disposition: Disposition, now_ns: u64, continuously_available: bool) -> Result<usize, Error> {
        let mut live = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        for message in messages {
            if let Some(lease) = self.leases.get(*message) && live.insert(*message) {
                bytes = bytes.checked_add(lease.bytes).ok_or(Error::Overflow)?;
            }
        }
        let count = live.len();
        if count == 0 { return Ok(0); }
        for message in live { self.leases.remove(message); }
        self.physical.release_for(bytes, Some(&self.account));
        if !matches!(disposition, Disposition::Cancelled { reason: CancelReason::BeforeAcceptance }) { self.credit()?; }
        if matches!(disposition, Disposition::Consumed { .. }) && continuously_available && self.accepting() {
            if let Some(previous) = self.observations.previous_consumption
                && let Some(elapsed_ns) = now_ns.checked_sub(previous).filter(|n| *n > 0)
            {
                self.observations.service = Some(Service { count, elapsed_ns, quantum: count });
            }
            self.observations.previous_consumption = (!self.leases.is_empty()).then_some(now_ns);
            self.learn()?;
        } else { self.observations = Observations::default(); }
        self.changed()?;
        Ok(count)
    }

    /// Called only when a real capacity update is sent to a peer known to have pending work.
    /// No periodic probe or sender's generation timestamp is used to measure feedback delay.
    pub fn advertised(&mut self, peer: &str, generation: u64, now_ns: u64) {
        if self.accepting() && generation == self.generation && let Some(revision) = self.demand.get(peer) {
            self.observations.outstanding.entry(peer.to_owned()).or_insert(Feedback { generation, revision: *revision, issued_ns: now_ns });
        }
    }

    pub fn advertised_ready(&mut self, now_ns: u64) {
        if self.leases.len() < self.target && self.accepting()
            && let Some(peer) = self.next_peer().cloned() { self.advertised(&peer, self.generation, now_ns); }
    }

    /// Pause, idle/starvation, connection/owner change and unsupported evidence end the busy
    /// cycle. They never create a sample or increase authority.
    pub fn interrupted(&mut self) { self.observations = Observations::default(); }

    fn learn(&mut self) -> Result<(), Error> {
        let (Some(service), Some(feedback)) = (self.observations.service, self.observations.feedback_ns) else { return Ok(()) };
        let work = u128::try_from(service.count).map_err(|_| Error::Overflow)?.checked_mul(u128::from(feedback)).ok_or(Error::Overflow)?;
        let during_feedback = work.checked_div(u128::from(service.elapsed_ns)).ok_or(Error::Overflow)?;
        let remainder = work.checked_rem(u128::from(service.elapsed_ns)).ok_or(Error::Overflow)?;
        let whole = during_feedback.checked_add(u128::from(remainder != 0)).ok_or(Error::Overflow)?;
        let target = usize::try_from(whole).map_err(|_| Error::Overflow)?.checked_add(service.quantum).ok_or(Error::Overflow)?;
        self.target = target;
        self.learning = Learning::Observed;
        Ok(())
    }
}

impl Drop for Controller {
    fn drop(&mut self) { for lease in self.leases.values() { self.physical.release_for(lease.bytes, Some(&self.account)); } }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(peers: &[&str], revision: u64) -> Vec<Demand> { peers.iter().map(|peer| Demand { peer: (*peer).into(), revision }).collect() }

    #[test]
    fn retry_permission_excludes_own_rollback_metadata_and_buffers_but_observes_foreign_release() -> Result<(), Error> {
        use std::task::Poll;
        futures::executor::block_on(async {
            let pool = PhysicalBudget::new(100);
            let inbox = Arc::new(super::super::inbox::Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), super::super::inbox::Limits::default()));
            let a = Input::new(1, Arc::clone(&pool), Arc::clone(&inbox));
            let b = Input::new(1, Arc::clone(&pool), inbox);
            let first = a.controller.lock().capacity();
            {
                let mut controller = a.controller.lock();
                controller.peers(&["peer".into()])?;
                controller.demand(&waiting(&["peer"], 10))?;
                assert!(matches!(controller.offer_publication("original", "peer", 1, 10, None, 10, 1)?, Admission::Reserved(_)));
                controller.stage("original", Stage::InCtl)?;
                controller.finish("original", Disposition::Cancelled { reason: CancelReason::BeforeAcceptance }, 2, false)?;
                controller.demand(&[])?;
                controller.demand(&waiting(&["peer"], 10))?;
                assert!(controller.capacity().generation > first.generation);
                assert_eq!(controller.capacity().retry_permission(), first.retry_permission());
            }
            let own = a.reserve_buffer(20)?.ok_or(Error::MissingLease)?;
            a.observe_physical();
            drop(own);
            let change = a.take_budget_change().ok_or(Error::MissingLease)?;
            assert!(!change, "the failed operation's own buffer release is not outside capacity");
            a.controller.lock().budget_changed(change)?;
            assert_eq!(a.controller.lock().capacity().retry_permission(), first.retry_permission());
            pool.resize(100);
            assert!(a.take_budget_change().is_none(), "same-bound remeasurement mints no permission");
            let foreign = b.reserve_buffer(30)?.ok_or(Error::MissingLease)?;
            assert_eq!(a.take_budget_change(), Some(false));
            let wait = a.physical_changed();
            futures::pin_mut!(wait);
            assert!(matches!(futures::poll!(&mut wait), Poll::Pending));
            drop(foreign);
            // Interleaved own activity cannot erase the other input's pending release witness.
            let own = a.reserve_buffer(5)?.ok_or(Error::MissingLease)?;
            a.observe_physical();
            assert!(matches!(futures::poll!(&mut wait), Poll::Ready(())));
            assert_eq!(a.take_budget_change(), Some(true));
            a.controller.lock().budget_changed(true)?;
            let external = a.controller.lock().capacity().retry_permission();
            assert_eq!(external, first.retry_permission().checked_add(1).ok_or(Error::Overflow)?);
            drop(own);
            assert_eq!(a.take_budget_change(), Some(false));
            a.controller.lock().budget_changed(false)?;
            assert_eq!(a.controller.lock().capacity().retry_permission(), external);
            pool.resize(101);
            assert_eq!(a.take_budget_change(), Some(true));
            assert_eq!(pool.counts().0, 0);
            Ok(())
        })
    }

    #[test]
    fn native_ready_does_not_clear_storage_unknown_and_only_successful_restore_reopens() -> Result<(), Error> {
        let mut controller = Controller::new(1, PhysicalBudget::new(100));
        controller.unavailable()?;
        controller.provider_ready()?;
        assert_eq!(controller.capacity().target, 0);
        assert!(!controller.writes_ready());
        controller.restoring()?;
        controller.restore_complete()?;
        assert!(controller.writes_ready());
        assert!(controller.capacity().retry_generation.is_some());
        Ok(())
    }

    #[test]
    fn fallible_original_copy_reserves_only_its_actual_representation_and_preserves_fields() -> Result<(), Error> {
        use super::super::model::{Attachment, Command, SendMode};
        let mut text = String::with_capacity(100);
        text.push_str("é\n\"literal");
        let command = Command::Send { id: "original".into(), text, mode: SendMode::Steer, attachments: vec![Attachment { name: "file".into(), mime: "text/plain".into(), data_base64: "eA==".into(), file_id: Some("fid".into()), path: Some("path".into()) }], overrides: None, reply_to: Some("parent".into()), task: None };
        let bytes = command_storage(&command, true)?;
        assert!(bytes < command_bytes(&command)?, "unused source string capacity is not another cloned body");
        let pool = PhysicalBudget::new(bytes);
        let inbox = Arc::new(super::super::inbox::Inbox::new(Arc::new(kiln_seams::records::SimRecords::new()), super::super::inbox::Limits::default()));
        let input = Input::new(0, Arc::clone(&pool), inbox);
        let (copied, charge) = input.clone_command(&command)?.ok_or(Error::MissingLease)?;
        assert_eq!(copied, command);
        assert_eq!(pool.counts().0, bytes);
        assert!(input.clone_command(&command)?.is_none());
        drop(copied);
        drop(charge);
        assert_eq!(pool.counts().0, 0);
        Ok(())
    }

    #[test]
    fn native_prompt_barrier_keeps_control_and_recovery_live() -> Result<(), Error> {
        for prompt in [r#"{"type":"user","uuid":"m"}"#, r#"{"method":"turn/start"}"#, r#"{"method":"turn/steer"}"#, r#"{"method":"session/prompt"}"#, r#"{"event":"user"}"#] { assert!(native_input(prompt)); }
        for control in [r#"{"id":1,"result":{"approved":true}}"#, r#"{"method":"thread/resume"}"#, r#"{"method":"turn/interrupt"}"#, r#"{"type":"control_response"}"#, r#"{"event":"abort"}"#] { assert!(!native_input(control)); }
        assert!(native_input("malformed"), "unclassified input must not bypass the barrier");
        let mut controller = Controller::new(1, PhysicalBudget::new(100));
        assert!(controller.writes_ready());
        controller.fence(true)?;
        assert!(!controller.writes_ready(), "a fenced old owner cannot send another prompt");
        controller.unavailable()?;
        controller.fence(false)?;
        assert!(!controller.writes_ready(), "unfencing never clears a storage error");
        Ok(())
    }

    #[test]
    fn legacy_flight_requires_its_actual_native_identity() {
        let flight = LegacyFlight { id: "original".into(), protocol: FlightProtocol::Codex, request: None };
        assert!(flight.matches(r#"{"method":"turn/steer","params":{"clientUserMessageId":"original"}}"#));
        assert!(!flight.matches(r#"{"method":"turn/steer","params":{"clientUserMessageId":"other","text":"original"}}"#));
        assert!(!flight.matches(r#"{"method":"thread/read","params":{"clientUserMessageId":"original"}}"#));
    }

    #[test]
    fn actual_buffer_ownership_uses_the_same_strict_aggregate_and_releases_on_drop() -> Result<(), Error> {
        let pool = PhysicalBudget::new(100);
        let mut controller = Controller::new(1, Arc::clone(&pool));
        assert!(matches!(controller.offer("body", "peer", 1, 60, None, 1)?, Admission::Reserved(_)));
        assert!(pool.reserve_buffer(41)?.is_none());
        let mut buffer = pool.reserve_buffer(40)?.ok_or(Error::MissingLease)?;
        assert_eq!(pool.counts(), (100, 100));
        assert!(!buffer.try_resize(41)?);
        assert_eq!(buffer.bytes(), 40);
        assert!(buffer.try_resize(20)?);
        assert_eq!(pool.counts(), (80, 100));
        drop(buffer);
        assert_eq!(pool.counts(), (60, 100));
        assert!(controller.lease("body").is_some(), "temporary-buffer release cannot consume its original communication");
        Ok(())
    }

    #[test]
    fn physical_authority_is_aggregate_and_has_no_oversize_exception() {
        let pool = PhysicalBudget::new(100);
        let (mut a, mut b) = (Controller::new(1, Arc::clone(&pool)), Controller::new(1, Arc::clone(&pool)));
        assert!(matches!(a.offer("a", "p", 1, 60, None, 1), Ok(Admission::Reserved(_))));
        assert!(matches!(b.offer("b", "p", 1, 60, None, 1), Ok(Admission::Waiting { cause: WaitCause::PhysicalBytes, .. })));
        pool.resize(20);
        assert_eq!(pool.counts(), (60, 20), "shrink retains accepted debt");
        assert!(a.finish("a", Disposition::Cancelled { reason: CancelReason::User }, 2, false).is_ok());
        assert!(matches!(b.offer("b", "p", 1, 21, None, 3), Ok(Admission::Waiting { cause: WaitCause::PhysicalBytes, .. })));
    }

    #[test]
    fn a_different_session_wakes_on_pool_release_but_not_unchanged_resize() {
        futures::executor::block_on(async {
            let pool = PhysicalBudget::new(100);
            let records: Arc<dyn kiln_seams::Records> = Arc::new(kiln_seams::records::SimRecords::new());
            let inbox = Arc::new(super::super::inbox::Inbox::new(records, super::super::inbox::Limits::default()));
            let (a, b) = (Input::new(1, Arc::clone(&pool), Arc::clone(&inbox)), Input::new(1, Arc::clone(&pool), inbox));
            assert!(a.controller.lock().offer("a", "peer", 1, 80, None, 1).is_ok());
            assert_eq!(b.take_budget_change(), Some(false));
            let wait = b.physical_changed();
            futures::pin_mut!(wait);
            assert!(futures::poll!(&mut wait).is_pending());
            assert!(a.controller.lock().finish("a", Disposition::Cancelled { reason: CancelReason::User }, 2, false).is_ok());
            assert!(futures::poll!(&mut wait).is_ready(), "release wakes the other session's existing owner future");
            assert_eq!(b.take_budget_change(), Some(true));
            let generation = b.controller.lock().capacity().generation;
            pool.resize(100);
            assert!(!b.observe_physical(), "an unchanged resource reading is not new credit");
            assert_eq!(b.controller.lock().capacity().generation, generation);
            let wait = b.physical_changed();
            futures::pin_mut!(wait);
            assert!(futures::poll!(&mut wait).is_pending());
        });
    }

    #[test]
    fn restoration_and_unavailable_input_never_advertise_a_bootstrap_grant() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert!(c.peers(&["peer".into()]).is_ok());
        assert!(c.demand(&waiting(&["peer"], 1)).is_ok());
        assert!(c.restoring().is_ok());
        assert_eq!(c.capacity().target, 0);
        assert_eq!(c.capacity().eligible_peer, None);
        assert!(matches!(c.offer("a", "peer", 1, 1, None, 1), Ok(Admission::Waiting { cause: WaitCause::Restoring, .. })));
        assert!(c.unavailable().is_ok());
        assert_eq!(c.capacity().target, 0);
        assert!(matches!(c.offer("a", "peer", 1, 1, None, 1), Ok(Admission::Waiting { cause: WaitCause::SourceUnavailable, .. })));
        assert!(c.fence(false).is_ok());
        assert_eq!(c.capacity().target, 0, "moving unfence cannot clear unavailable storage");
        assert_eq!(c.restore_complete(), Err(Error::InvalidTransition));
        assert!(c.restoring().is_ok());
        assert!(c.restore_complete().is_ok());
        assert_eq!(c.capacity().target, 2);
        assert_eq!(c.capacity().eligible_peer.as_deref(), Some("peer"));
    }

    #[test]
    fn a_restore_finished_while_moving_still_has_no_admission_authority() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert!(c.peers(&["peer".into()]).is_ok());
        assert!(c.demand(&waiting(&["peer"], 1)).is_ok());
        assert!(c.fence(true).is_ok());
        assert!(c.restoring().is_ok());
        assert!(c.restore_complete().is_ok());
        assert_eq!(c.capacity().target, 0, "storage readiness cannot clear ownership fencing");
        assert!(c.fence(false).is_ok());
        assert_eq!(c.capacity().target, 2);
    }

    #[test]
    fn an_outstanding_feedback_witness_survives_other_capacity_updates() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert!(c.peers(&["peer".into()]).is_ok());
        assert!(c.demand(&waiting(&["peer"], 1)).is_ok());
        c.advertised_ready(10);
        let first = c.capacity().feedback_generation;
        assert!(first.is_some());
        assert!(c.budget_changed(true).is_ok());
        c.advertised_ready(20);
        assert_ne!(first, Some(c.capacity().generation));
        assert_eq!(c.capacity().feedback_generation, first, "resource generation is not a new natural grant cycle");
        assert!(c.offer_publication("m", "peer", 1, 1, first, 1, 30).is_ok());
        assert_eq!(c.observations.feedback_ns, Some(20), "measure from the real initial grant");
        c.interrupted();
        assert!(c.observations.outstanding.is_empty());
    }

    #[test]
    fn only_waiting_demand_can_start_feedback_and_holds_end_the_cycle_without_eviction() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert!(c.peers(&["admitted-only".into(), "waiting".into()]).is_ok());
        c.advertised_ready(1);
        assert!(c.capacity().feedback_generation.is_none(), "an unresolved admitted receipt is not pending sender demand");
        assert!(c.demand(&waiting(&["waiting"], 1)).is_ok());
        c.advertised_ready(2);
        assert_eq!(c.capacity().eligible_peer.as_deref(), Some("waiting"));
        assert!(c.capacity().feedback_generation.is_some());
        assert!(matches!(c.offer("m", "waiting", 1, 10, c.capacity().feedback_generation, 3), Ok(Admission::Reserved(_))));
        assert_eq!(c.available(false), Ok(true));
        let held_generation = c.capacity().generation;
        assert_eq!(c.capacity().target, 0);
        assert_eq!(c.capacity().admitted, 1, "hold preserves accepted debt");
        c.advertised_ready(100);
        assert!(c.observations.outstanding.is_empty());
        assert_eq!(c.available(false), Ok(false));
        assert_eq!(c.capacity().generation, held_generation, "unrelated state while held cannot generate credit");
        assert_eq!(c.available(true), Ok(true));
        c.advertised_ready(101);
        assert!(c.observations.outstanding.values().all(|feedback| feedback.issued_ns == 101));
        assert!(c.demand(&[]).is_ok());
        c.advertised_ready(1000);
        assert!(c.observations.feedback_ns.is_none(), "withdrawal itself creates no feedback sample");
    }

    #[test]
    fn draining_the_last_accepted_item_cannot_bridge_a_producer_idle_service_interval() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        let done = Disposition::Consumed { boundary: Consumption::ProviderAccepted };
        assert!(c.offer("before-idle", "peer", 1, 1, None, 1).is_ok());
        assert!(c.finish("before-idle", done.clone(), 2, true).is_ok());
        assert!(c.observations.previous_consumption.is_none());
        assert!(c.offer("after-idle", "peer", 1, 1, None, 1000).is_ok());
        assert!(c.finish("after-idle", done, 1001, true).is_ok());
        assert!(c.observations.service.is_none(), "time with no accepted work is not service capacity");
    }

    #[test]
    fn real_waiting_claim_withdrawal_admit_and_native_handoffs_can_leave_bootstrap() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert!(c.peers(&["peer".into()]).is_ok());
        for (revision, issued, arrived, id) in [(11, 10, 20, "first"), (12, 21, 31, "second")] {
            assert!(c.demand(&waiting(&["peer"], revision)).is_ok());
            c.advertised_ready(issued);
            let witness = c.capacity().feedback_generation;
            assert!(witness.is_some());
            assert!(c.demand(&[]).is_ok(), "the server claimed the exact waiting publication before Admit arrives");
            assert!(matches!(c.offer_publication(id, "peer", 1, 1, witness, revision, arrived), Ok(Admission::Reserved(_))));
        }
        let consumed = Disposition::Consumed { boundary: Consumption::ProviderAccepted };
        assert!(c.finish("first", consumed.clone(), 40, true).is_ok());
        assert!(c.finish("second", consumed, 50, true).is_ok());
        assert_eq!(c.capacity().learning, Learning::Observed);
        assert_eq!(c.capacity().target, 2);
        assert!(c.observations.previous_consumption.is_none(), "true idle terminates the active busy interval");
        let idle_capacity = c.capacity();
        c.advertised_ready(999);
        assert_eq!(c.capacity(), idle_capacity, "idle time is not growth or synthetic demand");
        assert!(c.demand(&waiting(&["peer"], 13)).is_ok());
        c.advertised_ready(1000);
        let witness = c.capacity().feedback_generation;
        assert!(c.demand(&[]).is_ok());
        assert!(matches!(c.offer_publication("after-idle", "peer", 1, 1, witness, 13, 1030), Ok(Admission::Reserved(_))));
        assert_eq!(c.capacity().target, 4, "latest actual busy rate times this exact grant's 30ns refill plus service quantum; producer idle is excluded");
    }

    #[test]
    fn an_actual_batch_supplies_its_service_quantum_and_seventeen_peers_divide_the_window() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        for message in ["one", "two", "three", "four"] {
            assert!(c.restore(Lease { message: message.into(), peer: "peer".into(), epoch: 1, bytes: 1, stage: Stage::ProviderFlight, token: None, handoff: None }).is_ok());
        }
        let disposition = Disposition::Consumed { boundary: Consumption::ProviderCompleted };
        assert_eq!(c.finish_batch(&["one", "two"], disposition.clone(), 10, true), Ok(2));
        assert_eq!(c.finish_batch(&["three", "four"], disposition, 20, true), Ok(2));
        let observed = c.observations.service;
        assert!(matches!(observed, Some(Service { count: 2, quantum: 2, elapsed_ns: 10 })));
        let peers: Vec<_> = (0..17).map(|n| format!("peer-{n}")).collect();
        assert!(c.peers(&peers).is_ok());
        assert!(c.demand(&peers.iter().map(|peer| Demand { peer: peer.clone(), revision: 1 }).collect::<Vec<_>>()).is_ok());
        assert_eq!(c.capacity().target, 2, "seventeen peers divide the two bootstrap roles rather than minting seventeen windows");
        assert!(matches!(c.offer("new-one", "peer-0", 1, 1, None, 30), Ok(Admission::Reserved(_))));
        assert!(matches!(c.offer("new-two", "peer-1", 1, 1, None, 31), Ok(Admission::Reserved(_))));
        assert!(matches!(c.offer("new-three", "peer-2", 1, 1, None, 32), Ok(Admission::Waiting { .. })));
    }

    #[test]
    fn every_stage_retains_the_same_credit_and_repeats_acquire_none() {
        let pool = PhysicalBudget::new(100);
        let mut c = Controller::new(1, Arc::clone(&pool));
        assert!(matches!(c.offer("m", "p", 1, 10, None, 1), Ok(Admission::Reserved(_))));
        for stage in [Stage::InboxAccepted, Stage::InCtl, Stage::Carried, Stage::Queued, Stage::ProviderFlight] {
            assert!(c.stage("m", stage).is_ok());
            assert_eq!(c.capacity().admitted, 1);
            assert_eq!(pool.counts().0, 10);
        }
        assert!(matches!(c.offer("m", "p", 1, 10, None, 1), Ok(Admission::Existing(_))));
        assert_eq!(pool.counts().0, 10);
        assert_eq!(c.finish("m", Disposition::Consumed { boundary: Consumption::ProviderAccepted }, 2, false), Ok(true));
        assert_eq!(c.finish("m", Disposition::Consumed { boundary: Consumption::ProviderAccepted }, 2, false), Ok(false));
        assert_eq!(pool.counts().0, 0);
    }

    #[test]
    fn peers_divide_bootstrap_roles_and_lifecycle_invalidates_samples() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert_eq!(c.peers(&["a".into(), "b".into(), "c".into()]), Ok(true));
        assert!(c.demand(&waiting(&["a", "b", "c"], 1)).is_ok());
        assert_eq!(c.capacity().target, 2);
        assert!(matches!(c.offer("b", "b", 1, 1, None, 1), Ok(Admission::Waiting { cause: WaitCause::PeerTurn, .. })));
        assert!(matches!(c.offer("a", "a", 1, 1, None, 1), Ok(Admission::Reserved(_))));
        assert!(matches!(c.offer("b", "b", 1, 1, None, 2), Ok(Admission::Reserved(_))));
        assert!(matches!(c.offer("c", "c", 1, 1, None, 3), Ok(Admission::Waiting { cause: WaitCause::Bootstrap, .. })));
        assert!(c.finish("a", Disposition::Cancelled { reason: CancelReason::User }, 4, false).is_ok());
        assert!(matches!(c.offer("c", "c", 1, 1, None, 5), Ok(Admission::Reserved(_))));
        assert_eq!(c.capacity().learning, Learning::Bootstrap);
    }

    #[test]
    fn only_a_real_busy_cycle_and_correlated_refill_learn_depth() {
        let mut c = Controller::new(1, PhysicalBudget::new(100));
        assert!(c.peers(&["a".into()]).is_ok());
        assert!(c.demand(&waiting(&["a"], 1)).is_ok());
        assert!(c.offer("a1", "a", 1, 1, None, 1).is_ok());
        assert!(c.offer("a2", "a", 1, 1, None, 1).is_ok());
        assert!(c.finish("a1", Disposition::Consumed { boundary: Consumption::ProviderAccepted }, 10, true).is_ok());
        let generation = c.capacity().generation;
        c.advertised("a", generation, 10);
        assert!(c.finish("a2", Disposition::Consumed { boundary: Consumption::ProviderAccepted }, 20, true).is_ok());
        assert!(c.offer_publication("a3", "a", 1, 1, Some(generation), 1, 30).is_ok());
        assert_eq!(c.capacity().learning, Learning::Observed);
        assert_eq!(c.capacity().target, 3, "one quantum plus two consumed during feedback");
        assert!(c.peers(&["a".into(), "b".into()]).is_ok());
        assert_eq!(c.capacity().learning, Learning::Bootstrap);
        assert_eq!(c.capacity().target, 2, "new peers do not multiply capacity");
    }
}
