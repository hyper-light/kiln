//! The single owner of a replicated Database. Hosts carry authenticated wire records only.
//!
//! One command at a time crosses ReadIndex before evaluation. A captured answer stays here until
//! hyper-durable releases its matching durable application; no caller sees speculative state.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use api::replication::{Outbound, ReplicaConfig, ReplicaStatus};
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{Bytes, GroupStore, Output, Replica, StateMachine};
use hyper_raft::proto::{ConfState, Message};
use hyper_raft::wire::Record;

// Admission limits, not throughput tuning: overflow refuses before taking ownership. The queue
// charges a full API request for an opaque FnOnce, whose captures the API cannot inspect.
const COMMANDS: usize = 64;
const QUEUED_BYTES: usize = 512 << 20;
const OUTGOING_BYTES: usize = 512 << 20;
const OUTGOING_MESSAGES: usize = 4096;
const MEMBERS: usize = 64;
const WIRE_BYTES: usize = crate::replication::SNAPSHOT_BYTES.saturating_add(2 << 20);
const MESSAGE_BYTES: usize = crate::replication::COMMAND_BYTES.saturating_add(2 << 20);
// Ticks are the pinned core's documented tick election mode (raft.rs, Elections::Ticks). Its
// twenty-tick randomized election and two-tick heartbeat use this owner clock, never host votes.
const TICK: Duration = Duration::from_millis(10);
const POLL: Duration = Duration::from_millis(1);
const DRIVE_QUANTUM: usize = 32;

type Durable = Replica<GroupStore<DeviceFile>, crate::replication::Machine, Bytes>;
type Response<T> = mpsc::SyncSender<Result<T, Refusal>>;

/// A bounded ingress and egress handle. Clones share one product-owned Raft actor.
#[derive(Clone)]
pub struct ReplicaHandle {
    sender: mpsc::SyncSender<Envelope>,
    shared: Arc<Control>,
}

/// Completion of a bounded queued wire record. Admission transfers byte ownership only;
/// processing, application and durability are not acknowledged until their own protocol steps.
#[must_use = "poll the receipt to observe later processing errors"]
pub struct ReceiveReceipt {
    // Mutex makes receipts Sync for host carrier shells; exclusive polling uses get_mut and
    // never locks or waits for the actor. The actor holds only the channel's sending end.
    receiver: Mutex<mpsc::Receiver<Result<(), Refusal>>>,
    until: Instant,
    completed: bool,
}
impl ReceiveReceipt {
    /// None is pending; Some(()) means the actor accepted the record through step. Typed errors
    /// report rejection, deadline expiry or owner loss. A completed receipt cannot be polled again.
    /// Expiry stops observation; it does not undo processing that already began.
    pub fn try_result(&mut self) -> Result<Option<()>, Refusal> {
        if self.completed {
            return Err(api::invalid("replica receive receipt", "already completed"));
        }
        let result = match self.receiver.get_mut() {
            Ok(receiver) => match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) if Instant::now() < self.until => None,
                Err(mpsc::TryRecvError::Empty) => Some(Err(Refusal::Timeout)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(fenced("replica owner ended"))),
            },
            Err(_) => Some(Err(unavailable(
                "replica receive receipt lock was poisoned",
            ))),
        };
        match result {
            None => Ok(None),
            Some(result) => {
                self.completed = true;
                result.map(|()| Some(()))
            }
        }
    }
}
#[derive(Clone)]
pub(super) struct Client {
    handle: ReplicaHandle,
}
impl Client {
    pub(super) fn submit(
        &self,
        namespace: String,
        operation: Operation,
        until: Instant,
    ) -> Result<Answer, Refusal> {
        self.handle.submit(namespace, operation, until)
    }
    pub(super) fn handle(&self) -> ReplicaHandle {
        self.handle.clone()
    }
}
struct Control {
    admitted: AtomicUsize,
    outgoing: Mutex<Outgoing>,
    status: Mutex<Result<ReplicaStatus, Refusal>>,
}
#[derive(Default)]
struct Outgoing {
    bytes: usize,
    messages: VecDeque<Outbound>,
}
struct Reservation {
    control: Arc<Control>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.control
            .admitted
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Envelope {
    command: Input,
    _reservation: Reservation,
}
enum Input {
    Submit {
        namespace: String,
        operation: Operation,
        until: Instant,
        response: Response<Answer>,
    },
    Receive {
        from: u64,
        bytes: Vec<u8>,
        until: Instant,
        response: Response<()>,
    },
}
struct Pending {
    context: Vec<u8>,
    term: u64,
    leader: u64,
    until: Instant,
    namespace: String,
    response: Option<Response<Answer>>,
    phase: Phase,
    // Keep admission charged while its command is in flight, not just while queued.
    _reservation: Reservation,
}
enum Phase {
    Barrier(Option<Operation>),
    Proposed(Answer),
    Bootstrap(Option<Operation>),
}
struct Actor {
    replica: Durable,
    // Keep the log owner and its exclusive lease until the replica has drained/dropped.
    _log: hyper_log::Log<DeviceFile>,
    _log_owner: File,
    // Drop after the log writer has drained; reopen cannot race its final disk completion.
    _lease: Arc<File>,
    changed: Arc<Changed>,
    control: Arc<Control>,
    pending: Option<Pending>,
    // At most COMMANDS admitted operations wait behind the sole prepared transaction.
    waiting: VecDeque<Envelope>,
    serial: u64,
    started: Instant,
    ticked: Instant,
    published: u64,
    transferred: Option<(u64, Instant)>,
}

fn fenced(reason: impl std::fmt::Display) -> Refusal {
    Refusal::Fenced(reason.to_string())
}
fn unknown() -> Refusal {
    Refusal::UnknownOutcome
}
fn raft_error(error: hyper_durable::ReplicaError) -> Refusal {
    match error {
        hyper_durable::ReplicaError::Fenced(cause) => fenced(cause),
        other => unavailable(other),
    }
}
fn answer<T>(response: Response<T>, result: Result<T, Refusal>) {
    // Failure means the deadline-expired caller relinquished its reply. Committed work remains
    // applied and published; dropping this private response never drops a database update.
    let _ = response.send(result);
}

impl ReplicaHandle {
    fn enqueue(&self, command: Input, bytes: usize) -> Result<(), Refusal> {
        // Untouched application calls can wait for leadership. Keep room for one maximum wire
        // record against application pressure so that waiting calls cannot block that discovery.
        let application_limit = QUEUED_BYTES
            .checked_sub(WIRE_BYTES)
            .ok_or_else(|| fenced("replica wire reservation exceeds command queue budget"))?;
        let mut charged = self.shared.admitted.load(Ordering::Acquire);
        let mut reserved = false;
        for _ in 0..COMMANDS {
            let next = charged.checked_add(bytes).ok_or(Refusal::Full {
                limit: QUEUED_BYTES,
            })?;
            if matches!(&command, Input::Submit { .. }) && next > application_limit {
                return Err(Refusal::Full {
                    limit: application_limit,
                });
            }
            api::bound("replica command queue bytes", next, QUEUED_BYTES)?;
            match self.shared.admitted.compare_exchange_weak(
                charged,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    reserved = true;
                    break;
                }
                Err(now) => charged = now,
            }
        }
        if !reserved {
            return Err(Refusal::Full { limit: COMMANDS });
        }
        let envelope = Envelope {
            command,
            _reservation: Reservation {
                control: Arc::clone(&self.shared),
                bytes,
            },
        };
        self.sender.try_send(envelope).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Refusal::Full { limit: COMMANDS },
            mpsc::TrySendError::Disconnected(_) => {
                fenced("replica owner ended; reopen from durable state")
            }
        })
    }

    pub(super) fn submit(
        &self,
        namespace: String,
        operation: Operation,
        until: Instant,
    ) -> Result<Answer, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Submit {
                namespace,
                operation,
                until,
                response,
            },
            api::REQUEST_MAX,
        )?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => unknown(),
                mpsc::RecvTimeoutError::Disconnected => unknown(),
            })?
    }

    fn enqueue_receive(
        &self,
        from: u64,
        bytes: Vec<u8>,
        until: Instant,
    ) -> Result<mpsc::Receiver<Result<(), Refusal>>, Refusal> {
        if Instant::now() >= until {
            return Err(Refusal::Timeout);
        }
        api::bound("replica wire record", bytes.len(), WIRE_BYTES)?;
        let charge = bytes.len();
        let (response, receiver) = mpsc::sync_channel(1);
        self.enqueue(
            Input::Receive {
                from,
                bytes,
                until,
                response,
            },
            charge,
        )?;
        Ok(receiver)
    }

    /// `from` must be the identity authenticated by the host's transport, not a wire claim.
    /// Success waits until the actor processes the record; it does not acknowledge a commit.
    pub fn receive(&self, from: u64, bytes: Vec<u8>, until: Instant) -> Result<(), Refusal> {
        let receiver = self.enqueue_receive(from, bytes, until)?;
        receiver
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => Refusal::Timeout,
                mpsc::RecvTimeoutError::Disconnected => fenced("replica owner ended"),
            })?
    }

    /// Transfers a wire record to the existing bounded actor queue without waiting for step.
    /// `from` is the host-authenticated identity. The host must reap the returned receipt to
    /// observe later validation, expiry and fencing; successful admission is not a durability ACK.
    /// Dropping the receipt abandons observation; it does not cancel or replay the queued record.
    /// Hosts must bound their retained receipts as well as handling the queue's admission limit.
    pub fn admit_receive(
        &self,
        from: u64,
        bytes: Vec<u8>,
        until: Instant,
    ) -> Result<ReceiveReceipt, Refusal> {
        Ok(ReceiveReceipt {
            receiver: Mutex::new(self.enqueue_receive(from, bytes, until)?),
            until,
            completed: false,
        })
    }

    /// Takes at most `limit` messages. An undrained host eventually fences explicitly at the
    /// egress bound; the actor never acknowledges a send the host has not taken.
    pub fn drain(&self, limit: usize) -> Result<Vec<Outbound>, Refusal> {
        api::bound("replica drain count", limit, OUTGOING_MESSAGES)?;
        let mut outgoing = locked(&self.shared.outgoing)?;
        if outgoing.messages.is_empty()
            && let Err(error) = &*locked(&self.shared.status)?
        {
            return Err(error.clone());
        }
        let mut messages = Vec::with_capacity(limit.min(outgoing.messages.len()));
        for _ in 0..limit {
            let Some(message) = outgoing.messages.pop_front() else {
                break;
            };
            outgoing.bytes = outgoing
                .bytes
                .checked_sub(message.bytes.len())
                .ok_or_else(|| fenced("outgoing accounting underflow"))?;
            messages.push(message);
        }
        Ok(messages)
    }

    pub fn status(&self) -> Result<ReplicaStatus, Refusal> {
        locked(&self.shared.status)?.clone()
    }
}

pub(super) fn start(
    path: &Path,
    log_path: &Path,
    config: ReplicaConfig,
    changed: Arc<Changed>,
    lease: Arc<File>,
    until: Instant,
) -> Result<Client, Refusal> {
    if Instant::now() >= until {
        return Err(Refusal::Timeout);
    }
    validate(&config)?;
    let log_owner = exclusive_file(log_path)?;
    let machine = crate::replication::Machine::open_with_owner(
        path,
        ConfState {
            voters: config.voters.clone(),
            learners: config.learners.clone(),
            ..ConfState::default()
        },
        Arc::clone(&lease),
        config.node_id,
        config.bootstrap.clone(),
    )?;
    let file = DeviceFile::open(
        log_path,
        true,
        CachingRequest::Buffered,
        hyper_block::buf::Alignment::new(4096).map_err(unavailable)?,
    )
    .map_err(unavailable)?;
    let settings = settings(config.node_id)?;
    let log_config = hyper_log::Config {
        segment_bytes: 64 << 20,
        max_segments: 16,
        max_groups: 1,
        group_entries: 65_536,
        // A 256 MiB seed needs its chunk envelopes and election/configuration entries too.
        // 512 MiB payload admission stays below the 16 x 64 MiB segment ring (plus fixed
        // persist metadata). Even one 32 MiB chunk per segment uses at most nine segments;
        // seven remain, including hyper-log's final segment reserved for making room.
        group_bytes: 512 << 20,
        group_cache: 64 << 20,
        queue_submissions: 16,
        waits: hyper_log::Waits::Never,
    };
    let log = if file.len().map_err(unavailable)? == 0 {
        if machine.durable().index != 0 {
            return Err(fenced("state exists but its replication log is missing"));
        }
        hyper_log::Log::create(file, log_config, u128::from(config.node_id)).map_err(unavailable)?
    } else {
        let (log, recovery) = hyper_log::Log::open(file, log_config, u128::from(config.node_id))
            .map_err(unavailable)?;
        if !recovery.damaged.is_empty() {
            return Err(fenced("replication log recovery found acknowledged damage"));
        }
        log
    };
    let group = GroupStore::claim(&log, 1).map_err(unavailable)?;
    let replica =
        Replica::open(&settings, group, machine, Bytes::new(512 << 20)).map_err(unavailable)?;
    let control = Arc::new(Control {
        admitted: AtomicUsize::new(0),
        outgoing: Mutex::default(),
        status: Mutex::new(Ok(status(&replica))),
    });
    let (sender, receiver) = mpsc::sync_channel(COMMANDS);
    let started = Instant::now();
    let mut actor = Actor {
        published: replica.applied().index,
        replica,
        _log: log,
        _log_owner: log_owner,
        _lease: lease,
        changed,
        control: Arc::clone(&control),
        pending: None,
        waiting: VecDeque::new(),
        serial: 0,
        transferred: None,
        started,
        ticked: started,
    };
    actor.refresh()?;
    if Instant::now() >= until {
        return Err(Refusal::Timeout);
    }
    std::thread::Builder::new()
        .name("amphora.replica".into())
        .spawn(move || actor.run(receiver))
        .map_err(unavailable)?;
    Ok(Client {
        handle: ReplicaHandle {
            sender,
            shared: control,
        },
    })
}

fn validate(config: &ReplicaConfig) -> Result<(), Refusal> {
    let count = config
        .voters
        .len()
        .checked_add(config.learners.len())
        .ok_or_else(|| api::invalid("replica configuration", "member count overflow"))?;
    api::bound("replica members", count, MEMBERS)?;
    if config.node_id == 0 || config.voters.is_empty() {
        return Err(api::invalid(
            "replica configuration",
            "nonzero identity and voters required",
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    for id in config.voters.iter().chain(&config.learners) {
        if *id == 0 || !unique.insert(*id) {
            return Err(api::invalid(
                "replica configuration",
                "members must be nonzero, unique and disjoint",
            ));
        }
    }
    if let Some(bootstrap) = &config.bootstrap
        && !config.voters.contains(&bootstrap.source_node)
    {
        return Err(api::invalid(
            "bootstrap source",
            "source must be an initial voter",
        ));
    }
    if !unique.contains(&config.node_id) {
        return Err(api::invalid(
            "replica configuration",
            "local identity is not a member",
        ));
    }
    Ok(())
}
fn settings(id: u64) -> Result<hyper_durable::Settings, Refusal> {
    let mut limits = hyper_raft::Limits::derive(hyper_raft::Stated {
        message: MESSAGE_BYTES,
        members: MEMBERS,
        memory: 64 << 20,
        depth: 1,
    })
    .map_err(unavailable)?;
    limits.pending_messages = OUTGOING_MESSAGES;
    limits.pending_reads = COMMANDS;
    limits.unstable_entries = 1024;
    limits.entries_per_message = 1024;
    limits.proposals = 1;
    Ok(hyper_durable::Settings {
        core: hyper_raft::Config {
            check_quorum: true,
            pre_vote: true,
            max_size_per_msg: u64::try_from(MESSAGE_BYTES).map_err(unavailable)?,
            max_uncommitted_size: 64 << 20,
            max_committed_size_per_ready: 64 << 20,
            max_inflight_msgs: 4,
            max_inflight_bytes: 64 << 20,
            seed: id,
            ..hyper_raft::Config::new(id, limits)
        },
        // CP-T2.2: preserve flush-proved group durability during API reconciliation.
        disk: hyper_durable::Disk::Flushed,
        elections: hyper_raft::Elections::Ticks,
        quiet: Duration::from_millis(50),
    })
}
fn status(replica: &Durable) -> ReplicaStatus {
    ReplicaStatus {
        node_id: replica.id(),
        leader: replica.leader(),
        term: replica.term(),
        applied: replica.applied().index,
        durable_commit: replica.durable_commit(),
        bootstrapping: replica.machine().bootstrap_pending(),
        voters: replica.configuration().voters.clone(),
        learners: replica.configuration().learners.clone(),
        fenced: replica.fenced().map(ToString::to_string),
    }
}

impl Actor {
    fn now(&self) -> Result<u64, Refusal> {
        u64::try_from(self.started.elapsed().as_nanos()).map_err(unavailable)
    }
    fn refresh(&mut self) -> Result<(), Refusal> {
        if self.replica.machine().bootstrap_pending() {
            self.published = self.replica.applied().index;
            return Ok(());
        }
        self.replica
            .machine_mut()
            .read_db(|db| reload_changed(db, &self.changed).map_err(Into::into))?;
        self.published = self.replica.applied().index;
        Ok(())
    }
    fn run(&mut self, receiver: mpsc::Receiver<Envelope>) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_loop(&receiver)))
                .unwrap_or_else(|_| {
                    Err(fenced("replica owner unwound; reopen from durable state"))
                });
        let reason = match result {
            Ok(()) => fenced("replica owner stopped"),
            Err(error) => fenced(error),
        };
        if let Some(mut pending) = self.pending.take()
            && let Some(response) = pending.response.take()
        {
            answer(response, Err(unknown()));
        }
        if let Ok(mut status) = self.control.status.lock() {
            *status = Err(reason.clone());
        }
        for _ in 0..COMMANDS {
            let Some(envelope) = self.waiting.pop_front() else {
                break;
            };
            if let Input::Submit { response, .. } = envelope.command {
                answer(response, Err(reason.clone()));
            }
        }
        // The ingress channel has at most COMMANDS items. Notify callers before closing it.
        for _ in 0..COMMANDS {
            let Ok(envelope) = receiver.try_recv() else {
                break;
            };
            match envelope.command {
                Input::Submit { response, .. } => answer(response, Err(reason.clone())),
                Input::Receive { response, .. } => answer(response, Err(reason.clone())),
            }
        }
    }
    fn run_loop(&mut self, receiver: &mpsc::Receiver<Envelope>) -> Result<(), Refusal> {
        loop {
            let now = Instant::now();
            if now.saturating_duration_since(self.ticked) >= TICK {
                // A delayed owner does not invent missed heartbeats or run an unbounded catchup.
                self.ticked = now;
                self.replica.tick().map_err(raft_error)?;
            }
            for _ in 0..DRIVE_QUANTUM {
                let mut output = Output::default();
                let nanos = self.now()?;
                let driven = self
                    .replica
                    .drive(nanos, std::task::Waker::noop(), &mut output)
                    .map_err(raft_error)?;
                self.release(output)?;
                if !driven.more {
                    break;
                }
            }
            if self.replica.applied().index != self.published {
                self.refresh()?;
            }
            self.expire();
            self.bootstrap()?;
            if !self.replica.machine().bootstrap_pending()
                && self.pending.is_none()
                && self.replica.leader() != 0
                && let Some(envelope) = self.waiting.pop_front()
            {
                self.receive(envelope)?;
            }
            if self.replica.machine().bootstrap_pending() && self.replica.is_stalled() {
                return Err(fenced(
                    "bootstrap exhausted replication log admission before publication",
                ));
            }
            if !self.replica.machine().bootstrap_pending()
                && (self.replica.compactable_bytes() >= 128 << 20 || self.replica.is_stalled())
            {
                let nanos = self.now()?;
                self.replica
                    .compact(0, nanos, std::task::Waker::noop())
                    .map_err(raft_error)?;
            }
            *locked(&self.control.status)? = Ok(status(&self.replica));
            match receiver.recv_timeout(POLL) {
                Ok(envelope) => self.receive(envelope)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
    fn expire(&mut self) {
        // Expired callers relinquish queued operations even while a seed or proposal is held.
        // Rotate the bounded queue once; surviving requests retain their FIFO order.
        let queued = self.waiting.len().min(COMMANDS);
        for _ in 0..queued {
            let Some(envelope) = self.waiting.pop_front() else {
                break;
            };
            let expired = matches!(&envelope.command, Input::Submit { until, .. } if Instant::now() >= *until);
            if expired {
                if let Input::Submit { response, .. } = envelope.command {
                    answer(response, Err(Refusal::Timeout));
                }
            } else {
                self.waiting.push_back(envelope);
            }
        }
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let expired = Instant::now() >= pending.until;
        let changed_term = pending.term != self.replica.term()
            || pending.leader != self.replica.leader()
            || self.replica.leader() == 0;
        if !expired && !changed_term {
            return;
        }
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        match &pending.phase {
            Phase::Barrier(_) => {
                if expired {
                    if let Some(response) = pending.response.take() {
                        answer(response, Err(Refusal::Timeout));
                    }
                } else {
                    self.requeue(pending);
                }
            }
            Phase::Proposed(_) | Phase::Bootstrap(_) => {
                if let Some(response) = pending.response.take() {
                    answer(response, Err(unknown()));
                }
                // The captured base cannot be used by another write until this one applies. A
                // caller timing out relinquishes its response, never an unresolved proposal.
                if !changed_term {
                    self.pending = Some(pending);
                }
            }
        }
    }
    fn requeue(&mut self, mut pending: Pending) {
        // Only an untouched application operation may move back. A fresh dispatch assigns a new
        // context and ReadIndex; its old read completion cannot release the renewed barrier.
        let Phase::Barrier(Some(operation)) = pending.phase else {
            return;
        };
        let Some(response) = pending.response.take() else {
            return;
        };
        if Instant::now() >= pending.until {
            answer(response, Err(Refusal::Timeout));
        } else if self.waiting.len() >= COMMANDS {
            answer(response, Err(Refusal::Full { limit: COMMANDS }));
        } else {
            self.waiting.push_front(Envelope {
                command: Input::Submit {
                    namespace: pending.namespace,
                    operation,
                    until: pending.until,
                    response,
                },
                _reservation: pending._reservation,
            });
        }
    }
    fn receive(&mut self, envelope: Envelope) -> Result<(), Refusal> {
        let Envelope {
            command,
            _reservation,
        } = envelope;
        match command {
            Input::Receive {
                from,
                bytes,
                until,
                response,
            } => {
                let result = if Instant::now() >= until {
                    Err(Refusal::Timeout)
                } else {
                    self.step(from, &bytes)
                };
                let fatal = matches!(&result, Err(Refusal::Fenced(_)));
                let error = result.as_ref().err().cloned();
                answer(response, result);
                if fatal && let Some(error) = error {
                    return Err(error);
                }
            }
            Input::Submit {
                namespace,
                operation,
                until,
                response,
            } => {
                if Instant::now() >= until {
                    answer(response, Err(Refusal::Timeout));
                } else if self.replica.leader() == 0
                    || self.pending.is_some()
                    || self.replica.machine().bootstrap_pending()
                {
                    if self.waiting.len() >= COMMANDS {
                        answer(response, Err(Refusal::Full { limit: COMMANDS }));
                    } else {
                        self.waiting.push_back(Envelope {
                            command: Input::Submit {
                                namespace,
                                operation,
                                until,
                                response,
                            },
                            _reservation,
                        });
                    }
                } else {
                    let context = self.context()?;
                    match self.replica.read(context.clone()) {
                        Ok(()) => {
                            self.pending = Some(Pending {
                                context,
                                term: self.replica.term(),
                                leader: self.replica.leader(),
                                until,
                                namespace,
                                response: Some(response),
                                phase: Phase::Barrier(Some(operation)),
                                _reservation,
                            })
                        }
                        Err(error) => answer(response, Err(raft_error(error))),
                    }
                }
            }
        }
        Ok(())
    }
    fn step(&mut self, from: u64, bytes: &[u8]) -> Result<(), Refusal> {
        let message = Message::decode(bytes)
            .map_err(|error| api::invalid("replica wire record", &error.to_string()))?;
        if message.from != from || message.to != self.replica.id() || from == self.replica.id() {
            return Err(api::invalid(
                "replica sender",
                "authenticated sender or destination differs from record",
            ));
        }
        let conf = self.replica.configuration();
        if from == 0
            || !conf
                .voters
                .iter()
                .chain(&conf.learners)
                .chain(&conf.voters_outgoing)
                .chain(&conf.learners_next)
                .any(|id| *id == from)
        {
            return Err(api::invalid(
                "replica sender",
                "sender is not a current member",
            ));
        }
        self.replica.step(message).map_err(raft_error)
    }
    fn release(&mut self, output: Output<crate::replication::Applied>) -> Result<(), Refusal> {
        // Publish durable follower and leader state before releasing any result or read.
        if self.replica.applied().index != self.published {
            self.refresh()?;
        }
        for applied in output.answers {
            let matches = self
                .pending
                .as_ref()
                .is_some_and(|p| p.context == applied.context);
            if matches && let Some(mut pending) = self.pending.take() {
                let result = match pending.phase {
                    Phase::Proposed(value) => applied.result.map(|()| value),
                    Phase::Barrier(_) => {
                        Err(fenced("a read barrier matched an unapplied proposal"))
                    }
                    Phase::Bootstrap(operation) => match applied.result {
                        Ok(()) => {
                            pending.phase = Phase::Barrier(operation);
                            match self.replica.read(pending.context.clone()) {
                                Ok(()) => {
                                    self.pending = Some(pending);
                                    continue;
                                }
                                Err(error) => Err(raft_error(error)),
                            }
                        }
                        Err(error) => Err(error),
                    },
                };
                if let Some(response) = pending.response.take() {
                    answer(response, result);
                }
            }
        }
        for (context, _) in output.reads {
            if self.pending.as_ref().is_some_and(|p| p.context == context) {
                self.prepare()?;
            }
        }
        for entry in output.displaced {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| p.context == entry.context)
                && let Some(mut pending) = self.pending.take()
                && let Some(response) = pending.response.take()
            {
                answer(response, Err(unknown()));
            }
        }
        let mut outgoing = locked(&self.control.outgoing)?;
        for message in output.messages {
            let mut bytes = Vec::new();
            message.encode(&mut bytes);
            api::bound("replica outgoing record", bytes.len(), WIRE_BYTES)?;
            let total = outgoing
                .bytes
                .checked_add(bytes.len())
                .ok_or_else(|| fenced("outgoing size overflow"))?;
            if total > OUTGOING_BYTES || outgoing.messages.len() >= OUTGOING_MESSAGES {
                return Err(fenced(
                    "replica outgoing queue is full; drain and reopen from durable state",
                ));
            }
            outgoing.bytes = total;
            outgoing.messages.push_back(Outbound {
                to: message.to,
                bytes,
            });
        }
        Ok(())
    }
    fn context(&mut self) -> Result<Vec<u8>, Refusal> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| fenced("request identity exhausted"))?;
        // Followers can reopen within a term; the durable shell's fresh incarnation disambiguates
        // an old log entry from a new request at the same origin.
        let mut context = self.replica.id().to_le_bytes().to_vec();
        context.extend_from_slice(&self.replica.term().to_le_bytes());
        context.extend_from_slice(&self.replica.core().raft.config().incarnation.to_le_bytes());
        context.extend_from_slice(&self.serial.to_le_bytes());
        Ok(context)
    }
    fn bootstrap(&mut self) -> Result<(), Refusal> {
        if !self.replica.machine().bootstrap_pending() || self.pending.is_some() {
            return Ok(());
        }
        let source = self
            .replica
            .machine()
            .bootstrap_source()
            .ok_or_else(|| fenced("pending bootstrap has no designated source"))?;
        if source != self.replica.id() || !self.replica.is_leader() {
            // A failed transfer can be requested again after the core's twenty-tick election
            // period. At most one such request is made per period, never a host election.
            let period = TICK
                .checked_mul(20)
                .ok_or_else(|| fenced("tick period overflow"))?;
            let due = self
                .transferred
                .is_none_or(|(term, at)| term != self.replica.term() || at.elapsed() >= period);
            if due
                && self.replica.leader() != 0
                && (self.replica.is_leader() || source == self.replica.id())
            {
                self.replica.transfer(source).map_err(raft_error)?;
                self.transferred = Some((self.replica.term(), Instant::now()));
            }
            return Ok(());
        }
        let context = self.context()?;
        self.replica.read(context.clone()).map_err(raft_error)?;
        // One internal attempt has a finite lease; committed chunk progress survives its expiry.
        // User requests retain their own deadlines in the bounded waiting queue throughout.
        let until = Instant::now()
            .checked_add(Duration::from_secs(60))
            .ok_or_else(|| fenced("bootstrap deadline overflow"))?;
        self.pending = Some(Pending {
            context,
            term: self.replica.term(),
            leader: self.replica.leader(),
            until,
            namespace: String::new(),
            response: None,
            phase: Phase::Barrier(None),
            _reservation: Reservation {
                control: Arc::clone(&self.control),
                bytes: 0,
            },
        });
        Ok(())
    }
    fn entry_room(&self, context: usize) -> Result<usize, Refusal> {
        self._log
            .entry_room()
            .map_err(unavailable)?
            .checked_sub(hyper_durable::ENTRY_OVERHEAD)
            .and_then(|room| room.checked_sub(context))
            .ok_or_else(|| fenced("replication log cannot hold the proposal envelope"))
    }
    fn propose(&mut self, context: &[u8], bytes: Vec<u8>) -> Result<(), Refusal> {
        api::bound(
            "replication log entry",
            bytes.len(),
            self.entry_room(context.len())?,
        )?;
        self.replica
            .propose(context.to_vec(), bytes)
            .map_err(raft_error)
    }
    fn prepare(&mut self) -> Result<(), Refusal> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        // A delayed read completion must not refuse or replay an already proposed operation.
        if !matches!(pending.phase, Phase::Barrier(_)) {
            self.pending = Some(pending);
            return Ok(());
        }
        if Instant::now() >= pending.until {
            if let Some(response) = pending.response.take() {
                answer(response, Err(Refusal::Timeout));
            }
            return Ok(());
        }
        if pending.term != self.replica.term()
            || pending.leader != self.replica.leader()
            || self.replica.leader() == 0
        {
            self.requeue(pending);
            return Ok(());
        }
        let Phase::Barrier(operation) = pending.phase else {
            self.pending = Some(pending);
            return Ok(());
        };
        if self.replica.machine().bootstrap_pending() {
            let source = self
                .replica
                .machine()
                .bootstrap_source()
                .ok_or_else(|| fenced("pending bootstrap has no designated source"))?;
            if source != self.replica.id() || !self.replica.is_leader() {
                // The core's transfer request names its target in `from`. Only the source can
                // forward that request authenticated as itself; a leader executes it locally.
                if self.replica.is_leader() || source == self.replica.id() {
                    self.replica.transfer(source).map_err(raft_error)?;
                }
                if let Some(response) = pending.response.take() {
                    answer(
                        response,
                        Err(Refusal::NotLeader {
                            leader: Some(source),
                        }),
                    );
                }
                return Ok(());
            }
            let room = self
                .entry_room(pending.context.len())?
                .min(crate::replication::COMMAND_BYTES);
            let seed = self.replica.machine_mut().prepare_bootstrap(room);
            let result = match seed {
                Ok(Some(bytes)) => self.propose(&pending.context, bytes),
                Ok(None) => Err(fenced("pending bootstrap produced no seed chunk")),
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => {
                    pending.phase = Phase::Bootstrap(operation);
                    self.pending = Some(pending);
                }
                Err(error) => {
                    if let Some(response) = pending.response.take() {
                        answer(response, Err(error));
                    }
                }
            }
            return Ok(());
        }
        let Some(operation) = operation else {
            return Ok(());
        };
        let bounded = matches!(
            &operation,
            Operation::Mutation(Mutation::CompactStep { .. })
        );
        let prepared = if let Operation::Mutation(Mutation::CompactStep { before, .. }) = &operation
        {
            let room = self.entry_room(pending.context.len())?;
            self.replica
                .machine_mut()
                .prepare_compaction(&pending.namespace, *before, pending.until, room)
                .map(|(prepared, progress)| (prepared, Answer::Compacted(progress)))
        } else {
            self.replica.machine_mut().prepare_db(|db| {
                // The guard ends with application evaluation, before prepare_pages rolls back its
                // speculative transaction. Cleanup must run even after the SQL deadline elapsed.
                let _deadline = db.deadline(pending.until);
                if restore::incomplete(db)? {
                    return Err(api::invalid("database", "restore is incomplete").into());
                }
                match &operation {
                    Operation::Call(call) => {
                        let call = locked(call)?
                            .take()
                            .ok_or_else(|| unavailable("SQL session already executed"))?;
                        call(db).map(Answer::Called)
                    }
                    other => apply(db, &pending.namespace, other)?.map_err(Into::into),
                }
            })
        };
        // A failed whole rollback poisons this machine. Stop its existing owner instead of
        // accepting another request against an unverified speculative base.
        if bounded && let Err(Refusal::Fenced(reason)) = &prepared {
            return Err(fenced(reason));
        }
        let result = match prepared {
            Ok((prepared, value)) => {
                if Instant::now() >= pending.until {
                    Err(Refusal::Timeout)
                } else if prepared.is_empty() {
                    Ok(value)
                } else {
                    match prepared
                        .encode()
                        .and_then(|bytes| self.propose(&pending.context, bytes))
                    {
                        Ok(()) => {
                            pending.phase = Phase::Proposed(value);
                            self.pending = Some(pending);
                            return Ok(());
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            Err(error) => Err(if Instant::now() >= pending.until {
                Refusal::Timeout
            } else {
                error
            }),
        };
        if let Some(response) = pending.response.take() {
            answer(response, result);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    fn open(dir: &Path, voters: Vec<u64>) -> Client {
        let path = dir.join("state.db");
        let until = deadline();
        // Wait for actual lease release, including the actor's log/machine destruction.
        let lease = loop {
            match exclusive_file(&path) {
                Ok(lease) => break Arc::new(lease),
                Err(error) => {
                    assert!(Instant::now() < until, "owner did not drain: {error}");
                    std::thread::park_timeout(POLL);
                }
            }
        };
        start(
            &path,
            &dir.join("raft.log"),
            ReplicaConfig {
                node_id: 1,
                voters,
                learners: Vec::new(),
                bootstrap: None,
            },
            Arc::new(Changed::default()),
            lease,
            until,
        )
        .unwrap()
    }
    fn leader(client: &Client) {
        let until = deadline();
        loop {
            let status = client.handle().status().unwrap();
            if status.leader == status.node_id && status.applied > 0 {
                return;
            }
            assert!(
                Instant::now() < until,
                "single voter did not elect: {status:?}"
            );
            std::thread::park_timeout(POLL);
        }
    }
    fn call(
        client: &Client,
        f: impl FnOnce(&mut Db) -> anyhow::Result<Vec<u8>> + Send + 'static,
    ) -> Result<Vec<u8>, Refusal> {
        let operation = Operation::Call(Mutex::new(Some(Box::new(move |db| {
            Ok(Box::new(f(db)?) as Box<dyn Any + Send>)
        }))));
        match client.submit(String::new(), operation, deadline())? {
            Answer::Called(value) => Ok(*value.downcast::<Vec<u8>>().unwrap()),
            _ => panic!("wrong actor reply"),
        }
    }
    fn value(db: &mut Db) -> anyhow::Result<Vec<u8>> {
        let mut value = Vec::new();
        db.query("SELECT value FROM actor_items", [], |row| {
            value = row
                .get_value(0)
                .to_blob()
                .ok_or_else(|| anyhow::anyhow!("expected blob"))?
                .to_vec();
            Ok(false)
        })?;
        Ok(value)
    }

    /// U15/CP-T1.8: the product owner must not run a closure before leadership and ReadIndex.
    #[test]
    fn unknown_leader_waits_to_deadline_without_evaluating_and_authentication_binds_wire_sender() {
        let dir = tempfile::tempdir().unwrap();
        let client = open(dir.path(), vec![1, 2, 3]);
        let evaluated = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&evaluated);
        let started = Instant::now();
        let result = call(&client, move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        });
        // The owner and synchronous receiver share the original five-second deadline. Either
        // expiry observer may win; neither permits this isolated member to evaluate the closure.
        assert!(matches!(
            result,
            Err(Refusal::Timeout | Refusal::UnknownOutcome)
        ));
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
        let mut bytes = Vec::new();
        Message {
            from: 2,
            to: 1,
            ..Message::default()
        }
        .encode(&mut bytes);
        assert!(matches!(
            client.handle().receive(3, bytes, deadline()),
            Err(Refusal::Invalid { .. })
        ));
        let mut foreign = Vec::new();
        Message {
            from: 4,
            to: 1,
            ..Message::default()
        }
        .encode(&mut foreign);
        assert!(matches!(
            client.handle().receive(4, foreign, deadline()),
            Err(Refusal::Invalid { .. })
        ));
        assert!(matches!(
            client.handle().drain(OUTGOING_MESSAGES + 1),
            Err(Refusal::TooLarge { .. })
        ));
    }

    /// CP-T1.8: admitted application calls cannot consume replication's reserved byte capacity.
    #[test]
    fn unknown_leader_application_pressure_keeps_wire_admission_available() {
        let dir = tempfile::tempdir().unwrap();
        let client = open(dir.path(), vec![1, 2, 3]);
        let handle = client.handle();
        let until = deadline();
        let evaluated = Arc::new(AtomicUsize::new(0));
        let mut replies = Vec::new();
        let mut refusal = None;
        for _ in 0..COMMANDS {
            let counted = Arc::clone(&evaluated);
            let operation = Operation::Call(Mutex::new(Some(Box::new(move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(Vec::<u8>::new()) as Box<dyn Any + Send>)
            }))));
            let (response, receiver) = mpsc::sync_channel(1);
            match handle.enqueue(
                Input::Submit {
                    namespace: String::new(),
                    operation,
                    until,
                    response,
                },
                api::REQUEST_MAX,
            ) {
                Ok(()) => replies.push(receiver),
                Err(error) => {
                    refusal = Some(error);
                    break;
                }
            }
        }
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
        let mut bytes = Vec::new();
        Message {
            msg_type: hyper_raft::proto::MessageType::MsgHeartbeat,
            from: 2,
            to: 1,
            term: 1,
            ..Message::default()
        }
        .encode(&mut bytes);
        handle.receive(2, bytes, until).unwrap();
        assert!(matches!(
            refusal,
            Some(Refusal::Full { limit }) if limit == QUEUED_BYTES - WIRE_BYTES
        ));
        assert_eq!(
            replies.len(),
            (QUEUED_BYTES - WIRE_BYTES) / api::REQUEST_MAX
        );
        assert!(!replies.is_empty());
        assert_eq!(evaluated.load(Ordering::Relaxed), 0);
    }

    /// U15/CP-T1.8: real DeviceFile log, product-driven election, once-only capture and reopen.
    #[test]
    fn single_voter_actor_captures_once_and_reopens_durable_random_value() {
        let dir = tempfile::tempdir().unwrap();
        let client = open(dir.path(), vec![1]);
        leader(&client);
        let evaluated = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&evaluated);
        let chosen = call(&client, move |db| {
            count.fetch_add(1, Ordering::Relaxed);
            db.exec("CREATE TABLE actor_items(value BLOB DEFAULT(randomblob(32))); INSERT INTO actor_items DEFAULT VALUES")?;
            value(db)
        }).unwrap();
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);
        assert_eq!(call(&client, value).unwrap(), chosen);
        // Bounded GC is captured by this same owner and survives its durable restart.
        for writes in [
            vec![Write::Put {
                family: "items".into(),
                key: b"retired".to_vec(),
                value: b"old".to_vec(),
            }],
            vec![Write::Delete {
                family: "items".into(),
                key: b"retired".to_vec(),
            }],
        ] {
            client
                .submit(
                    "gc".into(),
                    Operation::Mutation(Mutation::Records {
                        namespace: "gc".into(),
                        tx: Tx {
                            conds: vec![],
                            writes,
                        },
                    }),
                    deadline(),
                )
                .unwrap();
        }
        let progress = client
            .submit(
                "gc".into(),
                Operation::Mutation(Mutation::CompactStep {
                    namespace: "gc".into(),
                    before: 2,
                }),
                deadline(),
            )
            .unwrap();
        let Answer::Compacted(progress) = progress else {
            panic!("wrong compaction answer")
        };
        assert_eq!(progress.completed, 2);
        assert_eq!(progress.feed, 1);
        let applied = client.handle().status().unwrap().applied;
        drop(client);
        let reopened = open(dir.path(), vec![1]);
        leader(&reopened);
        assert!(reopened.handle().status().unwrap().applied >= applied);
        assert_eq!(call(&reopened, value).unwrap(), chosen);
        let changes = reopened
            .submit(
                "gc".into(),
                Operation::Changes("items".into(), 1, 10),
                deadline(),
            )
            .unwrap();
        let Answer::Changes(changes) = changes else {
            panic!("wrong changes answer")
        };
        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].value, None);
        let progress = reopened
            .submit(
                "gc".into(),
                Operation::Mutation(Mutation::CompactStep {
                    namespace: "gc".into(),
                    before: 2,
                }),
                deadline(),
            )
            .unwrap();
        let Answer::Compacted(progress) = progress else {
            panic!("wrong recovered compaction answer")
        };
        assert_eq!(progress.completed, 2);
        assert_eq!(progress.visited, 0);
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);
    }
    /// U15/CP-T1.8: a SQL deadline interrupts execution, rolls back capture and frees the driver.
    #[test]
    fn expired_sql_rolls_back_before_actor_continues() {
        let dir = tempfile::tempdir().unwrap();
        let client = open(dir.path(), vec![1]);
        leader(&client);
        let ran = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&ran);
        let operation = Operation::Call(Mutex::new(Some(Box::new(move |db| {
            counted.fetch_add(1, Ordering::Relaxed);
            db.exec("CREATE TABLE cancelled_actor_write(value); WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n")?;
            Ok(Box::new(Vec::<u8>::new()) as Box<dyn Any + Send>)
        }))));
        let result = client.submit(
            String::new(),
            operation,
            Instant::now() + Duration::from_millis(200),
        );
        assert!(matches!(
            result,
            Err(Refusal::Timeout | Refusal::UnknownOutcome)
        ));
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        let retained = call(&client, |db| {
            let mut count = 0;
            db.query(
                "SELECT count(*) FROM sqlite_schema WHERE name='cancelled_actor_write'",
                [],
                |row| {
                    count = row.get::<i64>(0).map_err(unavailable)?;
                    Ok(false)
                },
            )?;
            Ok(vec![u8::try_from(count)?])
        })
        .unwrap();
        assert_eq!(retained, vec![0]);
    }
    struct PumpPacket {
        from: u64,
        message: Outbound,
        until: Instant,
    }
    struct PumpReceipt {
        receipt: ReceiveReceipt,
        bytes: usize,
    }
    enum PumpPhase {
        Relay,
        Captures {
            leader: usize,
            held: [Option<PumpPacket>; 2],
        },
    }
    /// CP-T1.8: mirror the carrier's bounded admission and observe every processed receipt.
    /// Admission never waits for another owner. All records, including retained proposals,
    /// share COMMANDS/QUEUED_BYTES bounds; each keeps its first absolute receive deadline.
    struct Pump<'a> {
        clients: &'a [Client],
        receipts: [Option<PumpReceipt>; COMMANDS],
        phase: PumpPhase,
        next_source: usize,
        records: usize,
        bytes: usize,
    }
    impl<'a> Pump<'a> {
        fn new(clients: &'a [Client]) -> Self {
            assert!(!clients.is_empty() && clients.len() <= MEMBERS);
            Self {
                clients,
                receipts: std::array::from_fn(|_| None),
                phase: PumpPhase::Relay,
                next_source: 0,
                records: 0,
                bytes: 0,
            }
        }
        fn reap(&mut self) {
            for slot in &mut self.receipts {
                let Some(pending) = slot.as_mut() else {
                    continue;
                };
                let result = pending.receipt.try_result();
                if matches!(result, Ok(None)) {
                    continue;
                }
                let pending = slot.take().unwrap();
                self.records -= 1;
                self.bytes -= pending.bytes;
                // Admission was not an ACK: expiry, validation and fencing must fail the test.
                assert_eq!(result.unwrap(), Some(()));
            }
        }
        fn admit(&mut self, packet: PumpPacket) {
            let peer = self
                .clients
                .get(usize::try_from(packet.message.to - 1).unwrap())
                .unwrap();
            let bytes = packet.message.bytes.len();
            let receipt = peer
                .handle()
                .admit_receive(packet.from, packet.message.bytes, packet.until)
                .unwrap();
            let slot = self
                .receipts
                .iter_mut()
                .find(|slot| slot.is_none())
                .unwrap();
            *slot = Some(PumpReceipt { receipt, bytes });
        }
        fn drive(&mut self) -> usize {
            self.reap();
            let first = self.next_source;
            self.next_source = (first + 1) % self.clients.len();
            let mut admitted = 0;
            // U15/CP-T1.8: match the host carrier's bounded 16-record quantum. One
            // record per origin each round preserves fairness and the held-origin barrier;
            // control traffic must not consume the entire relay service opportunity.
            for _ in 0..16 {
                let mut drained = false;
                for offset in 0..self.clients.len() {
                    // Reserve room for any legal wire record before draining ownership from a peer.
                    // The conservative byte check needs no unbounded overflow/backpressure buffer.
                    if self.records == COMMANDS || self.bytes + WIRE_BYTES > QUEUED_BYTES {
                        break;
                    }
                    let index = (first + offset) % self.clients.len();
                    let from = u64::try_from(index + 1).unwrap();
                    // No later record from this origin may overtake its retained proposal.
                    if let PumpPhase::Captures { held, .. } = &self.phase
                        && held.iter().flatten().any(|packet| packet.from == from)
                    {
                        continue;
                    }
                    for message in self.clients.get(index).unwrap().handle().drain(1).unwrap() {
                        drained = true;
                        api::bound("fixture wire record", message.bytes.len(), WIRE_BYTES).unwrap();
                        let packet = PumpPacket {
                            from,
                            message,
                            until: deadline(),
                        };
                        self.records += 1;
                        self.bytes += packet.message.bytes.len();
                        if let PumpPhase::Captures { leader, held } = &mut self.phase {
                            let record = Message::decode(&packet.message.bytes).unwrap();
                            if record.msg_type == hyper_raft::proto::MessageType::MsgPropose {
                                assert_ne!(index, *leader);
                                assert_eq!(packet.message.to, u64::try_from(*leader + 1).unwrap());
                                assert!(held.iter().flatten().all(|other| other.from != from));
                                *held.iter_mut().find(|slot| slot.is_none()).unwrap() =
                                    Some(packet);
                                continue;
                            }
                        }
                        self.admit(packet);
                        admitted += 1;
                    }
                }
                if !drained {
                    break;
                }
            }
            self.reap();
            admitted
        }
        fn begin_captures(&mut self, leader: usize) {
            assert!(matches!(self.phase, PumpPhase::Relay));
            self.phase = PumpPhase::Captures {
                leader,
                held: [None, None],
            };
        }
        fn captures_ready(&self) -> bool {
            matches!(&self.phase, PumpPhase::Captures { held, .. } if held.iter().all(Option::is_some))
        }
        fn release_captures(&mut self) {
            assert!(self.captures_ready());
            let PumpPhase::Captures { held, .. } =
                std::mem::replace(&mut self.phase, PumpPhase::Relay)
            else {
                panic!("capture phase disappeared")
            };
            for packet in held {
                self.admit(packet.unwrap());
            }
            self.reap();
        }
        fn call(
            &mut self,
            origin: usize,
            f: impl FnOnce(&mut Db) -> anyhow::Result<Vec<u8>> + Send + 'static,
        ) -> Result<Vec<u8>, Refusal> {
            let client = self.clients.get(origin).unwrap().clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            let worker = std::thread::spawn(move || sender.send(call(&client, f)).unwrap());
            let until = deadline();
            let result = loop {
                match receiver.try_recv() {
                    Ok(result) => break result,
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(error) => panic!("actor caller ended: {error}"),
                }
                assert!(
                    Instant::now() < until,
                    "follower-origin command did not settle"
                );
                self.drive();
                std::thread::park_timeout(POLL);
            };
            worker.join().unwrap();
            self.reap();
            result
        }
        fn finish(mut self) {
            assert!(
                matches!(self.phase, PumpPhase::Relay),
                "retained proposals were not released"
            );
            // No new records or grace deadline: try_result enforces each admitted until.
            while self.records != 0 {
                self.reap();
                if self.records != 0 {
                    std::thread::park_timeout(POLL);
                }
            }
            assert_eq!(self.bytes, 0);
            assert!(self.receipts.iter().all(Option::is_none));
        }
    }
    impl Drop for Pump<'_> {
        fn drop(&mut self) {
            // Never wait in Drop, including failure unwinding. Success requires explicit finish.
            if !std::thread::panicking() {
                assert_eq!(
                    self.records, 0,
                    "fixture pump dropped with unsettled records"
                );
                assert_eq!(self.bytes, 0);
                assert!(self.receipts.iter().all(Option::is_none));
                assert!(matches!(self.phase, PumpPhase::Relay));
            }
        }
    }

    /// U15/CP-T1.8: follower calls use the real leader's ReadIndex and proposal forwarding.
    #[test]
    fn follower_origin_captures_once_and_reads_after_local_durable_apply() {
        let dir = tempfile::tempdir().unwrap();
        let mut clients = Vec::new();
        for node_id in 1..=3 {
            let path = dir.path().join(format!("state-{node_id}.db"));
            let lease = Arc::new(exclusive_file(&path).unwrap());
            clients.push(
                start(
                    &path,
                    &dir.path().join(format!("raft-{node_id}.log")),
                    ReplicaConfig {
                        node_id,
                        voters: vec![1, 2, 3],
                        learners: Vec::new(),
                        bootstrap: None,
                    },
                    Arc::new(Changed::default()),
                    lease,
                    deadline(),
                )
                .unwrap(),
            );
        }
        let mut pump = Pump::new(&clients);
        let until = deadline();
        let mut delivered = 0;
        let leader = loop {
            delivered += pump.drive();
            let states: Vec<_> = clients
                .iter()
                .map(|c| c.handle().status().unwrap())
                .collect();
            let candidate = states.first().unwrap().leader;
            if candidate != 0
                && states
                    .iter()
                    .all(|s| s.leader == candidate && s.applied > 0)
            {
                break usize::try_from(candidate - 1).unwrap();
            }
            assert!(
                Instant::now() < until,
                "three product actors did not elect: {states:?}"
            );
            std::thread::park_timeout(POLL);
        };
        assert!(delivered > 0);
        let origin = (leader + 1) % 3;
        let evaluated = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&evaluated);
        let chosen = pump.call(origin, move |db| {
            count.fetch_add(1, Ordering::Relaxed);
            db.exec("CREATE TABLE actor_items(value BLOB DEFAULT(randomblob(32))); INSERT INTO actor_items DEFAULT VALUES")?;
            value(db)
        }).unwrap();
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);
        assert_eq!(pump.call((origin + 1) % 3, value).unwrap(), chosen);
        assert_eq!(evaluated.load(Ordering::Relaxed), 1);

        // Hold both forwarded proposals in the pump so both captures use the same committed
        // base. No owner waits for the other capture, and later records cannot overtake a hold.
        // Exactly one page set may apply; the stale one's answer is refused.
        let captures = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let (sender, receiver) = mpsc::sync_channel(2);
        let mut workers = Vec::new();
        for offset in 1..=2 {
            let client = clients.get((leader + offset) % 3).unwrap().clone();
            let captures = Arc::clone(&captures);
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                let result = call(&client, move |db| {
                    captures[offset - 1].fetch_add(1, Ordering::Relaxed);
                    db.exec("INSERT INTO actor_items(value) VALUES(x'01')")?;
                    Ok(vec![1])
                });
                sender.send(result).unwrap();
            }));
        }
        drop(sender);
        let until = deadline();
        let mut outcomes = Vec::new();
        pump.begin_captures(leader);
        while outcomes.len() < 2 {
            if let Ok(outcome) = receiver.try_recv() {
                outcomes.push(outcome);
            }
            assert!(
                Instant::now() < until,
                "concurrent proposals did not settle"
            );
            pump.drive();
            if pump.captures_ready() {
                let first = clients
                    .get((leader + 1) % 3)
                    .unwrap()
                    .handle()
                    .status()
                    .unwrap();
                let second = clients
                    .get((leader + 2) % 3)
                    .unwrap()
                    .handle()
                    .status()
                    .unwrap();
                assert_eq!(first.applied, second.applied);
                assert_eq!(first.term, second.term);
                assert_eq!(first.leader, u64::try_from(leader + 1).unwrap());
                assert_eq!(second.leader, first.leader);
                assert!(
                    captures
                        .iter()
                        .all(|count| count.load(Ordering::Relaxed) == 1)
                );
                pump.release_captures();
            }
            std::thread::park_timeout(POLL);
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(matches!(pump.phase, PumpPhase::Relay));
        assert!(
            captures
                .iter()
                .all(|count| count.load(Ordering::Relaxed) == 1)
        );
        assert_eq!(
            outcomes.iter().filter(|r| r.is_ok()).count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(outcomes.iter().filter(|r| matches!(r, Err(Refusal::Invalid { what, .. }) if what == "prepared transaction")).count(), 1, "{outcomes:?}");
        assert!(clients.iter().all(|c| c.handle().status().is_ok()));
        let count = pump
            .call(origin, |db| {
                db.exec("INSERT INTO actor_items(value) VALUES(x'03')")?;
                let mut count = 0;
                db.query("SELECT count(*) FROM actor_items", [], |row| {
                    count = row.get::<i64>(0).map_err(unavailable)?;
                    Ok(false)
                })?;
                Ok(vec![u8::try_from(count)?])
            })
            .unwrap();
        assert_eq!(count, vec![3]);
        pump.finish();
    }
    /// U15/CP-T1.8: malformed membership and a source that cannot lead are refused at admission.
    #[test]
    fn malformed_initial_membership_is_refused() {
        let valid = ReplicaConfig {
            node_id: 1,
            voters: vec![1, 2, 3],
            learners: Vec::new(),
            bootstrap: None,
        };
        assert!(validate(&valid).is_ok());
        for config in [
            ReplicaConfig {
                node_id: 0,
                ..valid.clone()
            },
            ReplicaConfig {
                voters: Vec::new(),
                ..valid.clone()
            },
            ReplicaConfig {
                voters: vec![1, 1],
                ..valid.clone()
            },
            ReplicaConfig {
                learners: vec![2],
                ..valid.clone()
            },
            ReplicaConfig {
                node_id: 9,
                ..valid.clone()
            },
            ReplicaConfig {
                learners: vec![4],
                bootstrap: Some(api::replication::Bootstrap {
                    source_node: 4,
                    source_id: [0; 32],
                }),
                ..valid.clone()
            },
        ] {
            assert!(matches!(validate(&config), Err(Refusal::Invalid { .. })));
        }
    }
    fn seeded_nodes(dir: &Path, bootstrap: &api::replication::Bootstrap) -> Vec<Client> {
        let mut clients = Vec::new();
        for node_id in 1..=3 {
            let path = dir.join(format!("state-{node_id}.db"));
            let until = deadline();
            let lease = loop {
                match exclusive_file(&path) {
                    Ok(lease) => break Arc::new(lease),
                    Err(error) => {
                        assert!(Instant::now() < until, "seed owner did not drain: {error}");
                        std::thread::park_timeout(POLL);
                    }
                }
            };
            clients.push(
                start(
                    &path,
                    &dir.join(format!("raft-{node_id}.log")),
                    ReplicaConfig {
                        node_id,
                        voters: vec![1, 2, 3],
                        learners: Vec::new(),
                        bootstrap: Some(bootstrap.clone()),
                    },
                    Arc::new(Changed::default()),
                    lease,
                    until,
                )
                .unwrap(),
            );
        }
        clients
    }
    fn known_leader(pump: &mut Pump<'_>) {
        let until = deadline();
        loop {
            pump.drive();
            if pump
                .clients
                .iter()
                .all(|c| c.handle().status().unwrap().leader != 0)
            {
                return;
            }
            assert!(
                Instant::now() < until,
                "seed actors did not discover a leader"
            );
            std::thread::park_timeout(POLL);
        }
    }
    fn unchanged(db: &mut Db) -> anyhow::Result<Vec<u8>> {
        let mut value = Vec::new();
        db.query("SELECT value FROM baseline_items WHERE id = 1", [], |row| {
            value = row
                .get_value(0)
                .to_blob()
                .ok_or_else(|| anyhow::anyhow!("expected blob"))?
                .to_vec();
            Ok(false)
        })?;
        Ok(value)
    }
    /// U15/CP-T1.8: a complete original image precedes every delta, including untouched pages.
    #[test]
    fn autonomous_quorum_seed_preserves_unchanged_pages_then_delta_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("state-3.db");
        let original = {
            let mut db = Db::open(&source).unwrap();
            db.exec("CREATE TABLE baseline_items(id INTEGER PRIMARY KEY,value BLOB); INSERT INTO baseline_items VALUES(1,randomblob(9000)); INSERT INTO baseline_items VALUES(2,x'02')").unwrap();
            db.flusher()().unwrap();
            unchanged(&mut db).unwrap()
        };
        let bootstrap = crate::replication::bootstrap::inspect_source(&source, 3).unwrap();
        let clients = seeded_nodes(dir.path(), &bootstrap);
        let mut pump = Pump::new(&clients);
        known_leader(&mut pump);
        // This can be admitted before the last seed chunk commits. The closure must wait in the
        // product queue, while autonomous source transfer and seed application keep driving.
        assert_eq!(pump.call(0, unchanged).unwrap(), original);
        pump.call(1, |db| {
            db.exec("UPDATE baseline_items SET value=x'03' WHERE id=2")?;
            Ok(Vec::new())
        })
        .unwrap();
        for origin in 0..3 {
            assert_eq!(pump.call(origin, unchanged).unwrap(), original);
        }
        pump.finish();
        drop(clients);
        let reopened = seeded_nodes(dir.path(), &bootstrap);
        let mut pump = Pump::new(&reopened);
        known_leader(&mut pump);
        for client in &reopened {
            let status = client.handle().status().unwrap();
            assert_eq!(status.voters, vec![1, 2, 3]);
            assert!(status.learners.is_empty());
            assert!(status.fenced.is_none());
            assert!(!status.bootstrapping);
        }
        for origin in 0..3 {
            assert_eq!(
                pump.call(origin, |db| {
                    let mut value = Vec::new();
                    db.query("SELECT value FROM baseline_items WHERE id=2", [], |row| {
                        value = row.get_value(0).to_blob().unwrap().to_vec();
                        Ok(false)
                    })?;
                    Ok(value)
                })
                .unwrap(),
                vec![3]
            );
            assert_eq!(pump.call(origin, unchanged).unwrap(), original);
        }
        pump.finish();
    }

    mod receive_receipts {
        use super::*;

        fn finish(receipt: &mut ReceiveReceipt) -> Result<(), Refusal> {
            let until = deadline();
            loop {
                match receipt.try_result() {
                    Ok(None) => {}
                    Ok(Some(())) => return Ok(()),
                    Err(error) => return Err(error),
                }
                assert!(Instant::now() < until, "receive receipt did not complete");
                std::thread::park_timeout(POLL);
            }
        }

        /// CP-T1.8: admission must not wait behind a SQL capture or acknowledge validation.
        #[test]
        fn admission_is_bounded_while_processed_receive_waits_and_errors_remain_observable() {
            let dir = tempfile::tempdir().unwrap();
            let client = open(dir.path(), vec![1]);
            leader(&client);
            let (entered, entering) = mpsc::sync_channel(1);
            let (release, released) = mpsc::sync_channel(1);
            let caller = client.clone();
            let capture = std::thread::spawn(move || {
                call(&caller, move |_| {
                    entered.send(())?;
                    released.recv_timeout(Duration::from_secs(5))?;
                    Ok(Vec::new())
                })
            });
            entering.recv_timeout(Duration::from_secs(5)).unwrap();

            let handle = client.handle();
            let mut receipts = Vec::new();
            for _ in 0..COMMANDS - 1 {
                receipts.push(handle.admit_receive(2, vec![0xff], deadline()).unwrap());
            }
            assert!(
                receipts
                    .iter_mut()
                    .all(|r| matches!(r.try_result(), Ok(None)))
            );
            let legacy = handle.clone();
            let (finished, finishing) = mpsc::sync_channel(1);
            let processed = std::thread::spawn(move || {
                finished
                    .send(legacy.receive(2, vec![0xff], deadline()))
                    .unwrap();
            });
            let until = deadline();
            // Wait for the legacy record's actual queue reservation, not thread scheduling.
            while handle.shared.admitted.load(Ordering::Acquire) != api::REQUEST_MAX + COMMANDS {
                assert!(Instant::now() < until, "processed receive was not admitted");
                std::thread::park_timeout(POLL);
            }
            assert!(matches!(
                finishing.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            assert!(matches!(
                handle.admit_receive(2, vec![0xff], deadline()),
                Err(Refusal::Full { limit: COMMANDS })
            ));

            release.send(()).unwrap();
            assert!(capture.join().unwrap().is_ok());
            for receipt in &mut receipts {
                assert!(matches!(finish(receipt), Err(Refusal::Invalid { .. })));
                assert!(matches!(receipt.try_result(), Err(Refusal::Invalid { .. })));
            }
            assert!(matches!(
                finishing.recv_timeout(Duration::from_secs(5)).unwrap(),
                Err(Refusal::Invalid { .. })
            ));
            processed.join().unwrap();
        }

        /// CP-T1.8: only processing a valid authenticated record completes a success receipt.
        #[test]
        fn authenticated_heartbeat_completes_through_the_existing_actor_step() {
            fn send_sync<T: Send + Sync>() {}
            send_sync::<ReceiveReceipt>();
            let dir = tempfile::tempdir().unwrap();
            let client = open(dir.path(), vec![1, 2, 3]);
            let mut bytes = Vec::new();
            Message {
                msg_type: hyper_raft::proto::MessageType::MsgHeartbeat,
                from: 2,
                to: 1,
                term: 1,
                ..Message::default()
            }
            .encode(&mut bytes);
            let mut receipt = client.handle().admit_receive(2, bytes, deadline()).unwrap();
            assert!(finish(&mut receipt).is_ok());
            assert!(matches!(receipt.try_result(), Err(Refusal::Invalid { .. })));
            assert!(matches!(
                client.handle().admit_receive(2, Vec::new(), Instant::now()),
                Err(Refusal::Timeout)
            ));
        }

        /// CP-T1.8: queued completion wins over expiry; missing replies expire or fence explicitly.
        #[test]
        fn receipt_poll_preserves_ready_result_and_typed_terminal_errors() {
            let (sender, receiver) = mpsc::sync_channel(1);
            sender.send(Ok(())).unwrap();
            let mut receipt = ReceiveReceipt {
                receiver: Mutex::new(receiver),
                until: Instant::now(),
                completed: false,
            };
            assert!(matches!(receipt.try_result(), Ok(Some(()))));

            let (sender, receiver) = mpsc::sync_channel(1);
            let mut receipt = ReceiveReceipt {
                receiver: Mutex::new(receiver),
                until: Instant::now(),
                completed: false,
            };
            assert!(matches!(receipt.try_result(), Err(Refusal::Timeout)));
            drop(sender);

            let (sender, receiver) = mpsc::sync_channel(1);
            let mut receipt = ReceiveReceipt {
                receiver: Mutex::new(receiver),
                until: deadline(),
                completed: false,
            };
            drop(sender);
            assert!(matches!(receipt.try_result(), Err(Refusal::Fenced(_))));
        }
    }
}
