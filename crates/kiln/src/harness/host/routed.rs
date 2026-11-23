//! A message routed to its session here (docs/sends.md, "Routed to its session"; Ada, 2026-11-20): taken into custody in
//! the session's own inbox at once, its sender answered, and handed to the session as its admission allows, here, with no
//! round trip to the server. A person's goes to it at once (held to no depth); an agent's or kiln's goes in its sender's
//! turn under the session's learned depth, and while that holds it back it waits in the session's own journal
//! (`Stage::Waiting`), handed on at the next change of the session's room ([`Host::admit_waiting`]), never on a timer.
//! Each sender's messages reach the session in the order they came: one never overtakes its sender's still waiting.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use super::{Host, Lane, Said, inbox, message_of};
use crate::harness::admission::{self, Admission, CancelReason, Demand, Disposition, FlowEvent, Input, Lease, Stage};
use crate::harness::mail::{self, StatusKey};
use crate::harness::model::Command;
use crate::harness::route::Route;
use crate::harness::session::{self, Session};

/// A message waiting in its session's own journal here to be handed to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Waiter {
    pub message: String,
    pub sender: String,
    /// Where its take stands among the session's (`Answer::seq`): its sender's turn starts at its oldest.
    pub seq: u64,
    pub status: Option<StatusKey>,
}

/// What waits in one session's journal here, in the order its inbox took them.
#[derive(Default, Debug)]
pub(super) struct Waiting {
    pub queue: VecDeque<Waiter>,
    /// A wait for the session's room runs ([`Host::watch_room`]).
    pub watched: bool,
    /// An admission pass waits on its lane already.
    pub queued: bool,
}

/// What the server last said of its own custody for a session (`Command::MailPeers`: W20's ledger, for messages it held
/// before routing, or while the session's runner was away): its senders and their waiting heads, beside the session's
/// own waiting ones in its turns.
pub(super) type ServerMail = HashMap<String, (Vec<String>, Vec<Demand>)>;

pub(super) type Reply = Option<futures::channel::oneshot::Sender<Said>>;
type Routed = (Command, Option<u64>, Route, Reply);

/// Pull another command only after planning the previous one. Arrival of a control during a
/// store read or receipt hash stops the turn. New data cannot continually extend an existing turn.
pub(super) fn following<'a>(
    data: &'a crate::memory::queue::Receiver<Lane>, high: &'a crate::memory::queue::Receiver<Lane>, pending: &'a mut Option<Lane>,
) -> impl FnMut() -> Option<Routed> + 'a {
    let mut remaining = data.len();
    move || {
        if remaining == 0 || !high.is_empty() { return None; }
        remaining -= 1;
        match data.try_recv().ok()? {
            Lane::Routed { c, epoch, at, route, reply } if route.status.is_none() => {
                crate::route_latency::observe_in(crate::route_latency::InLeg::ToSession, crate::route_latency::InKind::Session, at.elapsed());
                Some((*c, epoch, *route, reply))
            }
            other => { *pending = Some(other); None }
        }
    }
}

enum Plan {
    /// Refused before its inbox took it: what its sender is told.
    Said(Said),
    /// Its inbox takes it; then it's handed to the session, or it waits in the session's journal.
    Take(Box<Take>),
    /// No inboxes here: on as it comes, unchecked.
    Unchecked { c: Box<Command>, epoch: Option<u64> },
    /// Not a message: on to the session as any command.
    Command(Box<Command>),
}

/// A routed message its session's inbox takes: handed to the session once it has it (`hand`; `reserved`: its lease is
/// new), else waiting in the session's journal.
struct Take {
    lease: Lease,
    hand: bool,
    reserved: bool,
    c: Command,
    message: String,
    digest: String,
    charge: Option<inbox::RoutedCharge>,
    sent_for: u64,
    at: i64,
    route: Route,
}

impl Host {
    /// Hands message `c`, routed to session `id` here for `epoch` with `route` (its sender, and whether it's a status a
    /// newer one replaces), to the session's lane, after whatever it was told before: its inbox takes it and says so
    /// ([`super::Update::Inbox`]). False when there's no session `id` here, or its lane holds its most (said `full`).
    pub fn command_routed(&self, id: &str, c: Command, epoch: Option<u64>, route: Route) -> bool {
        self.enqueue_routed(id, c, epoch, route, None)
    }

    pub(crate) fn command_routed_reply(&self, id: &str, c: Command, epoch: u64, route: Route) -> Option<futures::channel::oneshot::Receiver<Said>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.enqueue_routed(id, c, Some(epoch), route, Some(tx)).then_some(rx)
    }

    fn enqueue_routed(&self, id: &str, c: Command, epoch: Option<u64>, route: Route, reply: Reply) -> bool {
        let operation = self.archive_lock(id);
        let Some(_guard) = operation.try_lock() else { return false };
        let Some(lane) = self.lanes.lock().get(id).map(|l| l.data.clone()) else { return false };
        match lane.try_send(Lane::Routed { c: Box::new(c), epoch, at: std::time::Instant::now(), route: Box::new(route), reply }) {
            Ok(()) => true,
            Err(async_channel::TrySendError::Full(Lane::Routed { c, reply, .. })) => {
                if let Some(message) = crate::harness::registry::message_id(&c) {
                    let said = Said { message, answer: Err(inbox::Refusal::Full { session: id.to_owned(), limit: super::LANE }) };
                    if let Some(reply) = reply { let _ = reply.send(said); return true; }
                    let (sink, id) = (self.sink.clone(), id.to_owned());
                    crate::rt::spawn(async move {
                        let _ = sink.send((id, super::Update::Inbox { said: Box::new(said) })).await;
                    });
                }
                false
            }
            Err(_) => false,
        }
    }

    /// What still waited in session `id`'s journal where it was (a resume from the server's store:
    /// `moving::Package::waiting`), down its lane behind its opening, each from its sender in the order it came: taken by
    /// its inbox here as any routed message. How many its lane took.
    pub(super) fn take_carried(&self, id: &str, waiting: Vec<crate::harness::moving::Waiting>) -> usize {
        let Some(here) = self.get(id).map(|s| s.epoch()) else { return 0 };
        let mut taken = 0usize;
        for w in waiting {
            let route = Route { status: w.status, ..Route::new("here", w.sender) };
            if self.command_routed(id, w.command, Some(here), route) {
                taken = taken.saturating_add(1);
            } else {
                tracing::warn!(target: "kiln.sends", event = "resume_waiting_lost", session = %id, "a message that waited where it was couldn't be taken here: its lane is full");
            }
        }
        taken
    }

    /// Plan only work already queued, within the shard’s measured step and the session’s actual
    /// transaction headroom. A single oversize request gets its normal admission verdict.
    pub(super) async fn take_routed_burst(&self, id: &str, first: Routed, mut more: impl FnMut() -> Option<Routed> + Send) {
        let start = std::time::Instant::now();
        let quantum = hyper_rt::futures::step_budget_ns().map(std::time::Duration::from_nanos);
        let s = self.get(id);
        let input = match (&s, self.inbox.clone()) {
            (Some(s), Some(inbox)) => Some(self.input_of(s, inbox)),
            _ => None,
        };
        let mut behind = HashSet::new();
        let mut plans = Vec::new();
        let mut planned_bytes = 0usize;
        let mut next = Some(first);
        while let Some((command, epoch, route, reply)) = next.take() {
            let mut plan = self.plan_routed(id, s.as_ref(), input.as_ref(), command, epoch, route, &mut behind).await;
            if let (Some(input), Plan::Take(take)) = (&input, &mut plan) {
                take.charge = input.inbox.routed_charge(id, &take.message, &take.digest, take.at, &take.lease, &take.c, take.route.status.as_ref()).ok();
                planned_bytes = planned_bytes.saturating_add(take.charge.as_ref().map_or(usize::MAX, inbox::RoutedCharge::bytes));
            }
            plans.push((plan, reply));
            if quantum.is_some_and(|step| start.elapsed() >= step) || input.as_ref().is_some_and(|i| planned_bytes >= i.physical_headroom()) {
                break;
            }
            next = more();
        }
        let takes = match &input {
            Some(input) => {
                let take_count = plans.iter().filter(|(p, _)| matches!(p, Plan::Take(_))).count();
                // Materialize the session head once before a multi-message concurrent burst so
                // parallel takes never collide on `Cond::Absent { HEADS }`.
                if take_count > 1 && !input.inbox.has_head(id) && let Some(s) = &s {
                    let _ = input.inbox.open(id, s.epoch()).await;
                }
                // Re-read actual headroom before submitting. Another session may have taken
                // room since planning, or this turn’s last request may need an entire commit.
                let mut out: Vec<Option<Result<inbox::Answer, inbox::Refusal>>> = Vec::with_capacity(plans.len());
                let mut idx = 0usize;
                while idx < plans.len() {
                    let avail = input.physical_headroom();
                    let mut end = idx;
                    let mut bytes = 0usize;
                    let mut batch_takes = 0usize;
                    while end < plans.len() {
                        if let Some((Plan::Take(take), _)) = plans.get(end) {
                            let charge = take.charge.as_ref().map_or(usize::MAX, inbox::RoutedCharge::bytes);
                            if batch_takes > 0 && bytes.saturating_add(charge) > avail {
                                break;
                            }
                            bytes = bytes.saturating_add(charge);
                            batch_takes = batch_takes.saturating_add(1);
                        }
                        end = end.saturating_add(1);
                    }
                    let slice = plans.get(idx..end).unwrap_or_default();
                    let batch = futures::future::join_all(slice.iter().map(|(plan, _)| async move {
                        match plan {
                            Plan::Take(take) => Some(input.inbox.accept_priced_routed(id, &take.message, &take.digest, take.sent_for, take.at, &take.lease, &take.c, take.lease.bytes, take.route.status.as_ref(), input, take.charge.as_ref()).await),
                            _ => None,
                        }
                    }))
                    .await;
                    out.extend(batch);
                    idx = end;
                }
                out
            }
            None => plans.iter().map(|_| None).collect(),
        };
        for ((plan, reply), taken) in plans.into_iter().zip(takes) {
            let said = match (plan, taken, &s, &input) {
                (Plan::Take(take), Some(answer), Some(s), Some(input)) => {
                    let Take { lease, hand, reserved, c, message, route, .. } = *take;
                    match (&answer, hand) {
                        (Ok(taken), true) if !taken.repeat => {
                            let _ = input.controller.lock().stage(&message, Stage::InboxAccepted);
                            // Its original stays in custody even if the session ends or leaves before it takes it.
                            if s.command(c) {
                                let _ = input.controller.lock().stage(&message, Stage::InCtl);
                            }
                        }
                        (Ok(taken), false) if !taken.repeat => {
                            self.add_waiting(id, Waiter { message: message.clone(), sender: lease.peer.clone(), seq: taken.seq, status: route.status.clone() });
                            self.set_demand(id, input);
                            self.watch_room(id, input);
                        }
                        _ => {
                            if reserved {
                                let _ = input.controller.lock().finish(&message, Disposition::Cancelled { reason: CancelReason::BeforeAcceptance }, input.now_ns(), false);
                            }
                        }
                    }
                    Some(Said { message, answer })
                }
                (Plan::Said(said), _, _, _) => Some(said),
                (Plan::Unchecked { c, epoch }, _, Some(s), _) => {
                    super::take_message(None, id, Some(&**s), *c, epoch, self.clock.unix_ms()).await
                }
                (Plan::Command(c), _, Some(s), _) => {
                    s.command(*c);
                    None
                }
                _ => None,
            };
            if let Some(said) = said {
                if let Some(reply) = reply {
                    let _ = reply.send(said);
                } else {
                    self.say(id, said).await;
                }
            }
        }
    }

    /// How routed message `c` goes, decided in its turn: refused before its inbox takes it, handed to the session once
    /// its inbox has it (its admission let it in), or kept waiting in the session's journal (held back, or behind one of
    /// its sender's: `behind`, which a sender held back joins).
    #[allow(clippy::too_many_arguments, reason = "one routed message's decision: the session, its admission, the message, the epoch it was sent for, its route, and the senders held back in its burst")]
    async fn plan_routed(&self, id: &str, s: Option<&Arc<Session>>, input: Option<&Input>, c: Command, epoch: Option<u64>, route: Route, behind: &mut HashSet<String>) -> Plan {
        let Some((message, digest)) = message_of(&c) else {
            // Routed, but no message: on to the session as any command.
            return Plan::Command(Box::new(c));
        };
        let refused = |answer: inbox::Refusal| Plan::Said(Said { message: message.clone(), answer: Err(answer) });
        let Some(s) = s else {
            // Gone from here since it was routed: the server sends it where the session is now.
            return refused(inbox::Refusal::NoSession { session: id.to_owned() });
        };
        // On its way elsewhere, or a new build not promoted yet: refused before its inbox takes it, and sent where it goes.
        if self.is_held() || s.is_moving() {
            return refused(inbox::Refusal::Moved { session: id.to_owned(), epoch: s.epoch() });
        }
        // No inboxes here (`KILN_INBOX` off): on as it comes, unchecked, as any message.
        let Some(input) = input else { return Plan::Unchecked { c: Box::new(c), epoch } };
        let at = self.clock.unix_ms();
        let c = match session::prepare(&self.files_for(id), c).await {
            Ok(c) => c,
            Err(_) => return refused(inbox::Refusal::Unavailable { session: id.to_owned(), why: "its attachments couldn't be made ready here; nothing of it was taken".into() }),
        };
        let here = s.epoch();
        let sent_for = epoch.unwrap_or(here);
        let Ok(bytes) = admission::command_bytes(&c) else {
            return refused(inbox::Refusal::Unavailable { session: id.to_owned(), why: "its size couldn't be counted".into() });
        };
        // A newer status from its sender under the same key replaces one of theirs still waiting (W20 coalescing).
        if let Some(status) = &route.status {
            self.supersede_waiting(id, input, &route.sender, status, here).await;
        }
        // A sender's messages reach the session in the order they came: behind one of theirs still waiting, it waits.
        let held = behind.contains(&route.sender) || self.waits_from(id, &route.sender);
        let offered = if held { None } else { Some(input.controller.lock().offer(&message, &route.sender, here, bytes, None, input.now_ns())) };
        let (lease, hand, reserved) = match offered {
            Some(Ok(Admission::Reserved(lease))) => (lease, true, true),
            Some(Ok(Admission::Existing(lease))) => (lease, true, false),
            // Held back (its depth, another sender's turn, its room, a restore under way), or behind its sender's.
            _ => {
                if !behind.contains(&route.sender) {
                    behind.insert(route.sender.clone());
                }
                (Lease { message: message.clone(), peer: route.sender.clone(), epoch: here, bytes, stage: Stage::Waiting, token: None, handoff: None }, false, false)
            }
        };
        let lease = if hand { Lease { stage: Stage::InboxAccepted, ..lease } } else { lease };
        Plan::Take(Box::new(Take { lease, hand, reserved, c, message, digest, charge: None, sent_for, at, route }))
    }

    /// Session `s`'s admission, made as it's first needed (its depth starting where its own was).
    fn input_of(&self, s: &Session, inbox: Arc<inbox::Inbox>) -> Input {
        let mut core = s.core.lock();
        let epoch = core.state.epoch;
        let depth = admission::learned_depth(&core.state);
        core.input.get_or_insert_with(|| self.learned_input(epoch, inbox, depth)).clone()
    }

    /// Hands session `id` what waits in its own journal, in turn, while its admission has room: a person's first, then
    /// the sender whose turn it is. Each one's journal row moves on as it goes. In the session's lane, so no message
    /// routed to it meanwhile comes between.
    pub(super) async fn admit_waiting(&self, id: &str) {
        if let Some(w) = self.waiting.lock().get_mut(id) {
            w.queued = false;
        }
        let Some(s) = self.get(id) else { return };
        let Some(input) = s.core.lock().input.clone() else { return };
        if self.is_held() || s.is_moving() {
            return;
        }
        let here = s.epoch();
        let mut handed = false;
        loop {
            let capacity = input.controller.lock().capacity();
            let next = {
                let waiting = self.waiting.lock();
                let Some(w) = waiting.get(id) else { break };
                w.queue.iter().find(|waiter| mail::person(&waiter.sender)).or_else(|| capacity.eligible_peer.as_ref().and_then(|peer| w.queue.iter().find(|waiter| &waiter.sender == peer))).cloned()
            };
            let Some(waiter) = next else { break };
            let pending = match input.inbox.pending(id, &waiter.message).await {
                Ok(Some(pending)) if pending.waiting_here() => pending,
                // Gone from its journal (replaced, or it left with its session): it no longer waits.
                Ok(_) => {
                    self.drop_waiting(id, &waiter.message);
                    continue;
                }
                Err(_) => break,
            };
            let Some(c) = pending.current_command().cloned() else {
                self.drop_waiting(id, &waiter.message);
                continue;
            };
            if pending.lease.epoch != here {
                // Taken at another epoch than it's at now: its journal's transfer moves it, and it waits for that.
                break;
            }
            let offered = {
                let mut controller = input.controller.lock();
                controller.advertised_ready(input.now_ns());
                controller.offer_publication(&waiter.message, &waiter.sender, here, pending.lease.bytes, capacity.feedback_generation, waiter.seq, input.now_ns())
            };
            match offered {
                Ok(Admission::Reserved(_)) => match input.inbox.stage_pending(id, &waiter.message, here, Stage::InboxAccepted).await {
                    Ok(true) => {
                        self.drop_waiting(id, &waiter.message);
                        let _ = input.controller.lock().stage(&waiter.message, Stage::InboxAccepted);
                        if s.command(c) {
                            let _ = input.controller.lock().stage(&waiter.message, Stage::InCtl);
                        }
                        handed = true;
                    }
                    _ => {
                        let _ = input.controller.lock().finish(&waiter.message, Disposition::Cancelled { reason: CancelReason::BeforeAcceptance }, input.now_ns(), false);
                        break;
                    }
                },
                // Let in already (a replay restored it): it no longer waits.
                Ok(Admission::Existing(_)) => self.drop_waiting(id, &waiter.message),
                // No room for it now: the next change of the session's room looks again.
                _ => break,
            }
        }
        self.set_demand(id, &input);
        if handed {
            let capacity = input.controller.lock().capacity();
            s.mail_flow(FlowEvent::Capacity { capacity });
        }
    }

    /// Message `waiter` waits in session `id`'s journal here, behind those taken before it.
    pub(super) fn add_waiting(&self, id: &str, waiter: Waiter) {
        let mut waiting = self.waiting.lock();
        let w = waiting.entry(id.to_owned()).or_default();
        if w.queue.iter().any(|w| w.message == waiter.message) {
            return;
        }
        // In the order taken: a restore brings them back in any order.
        let at = w.queue.iter().position(|w| w.seq > waiter.seq).unwrap_or(w.queue.len());
        w.queue.insert(at, waiter);
    }

    fn drop_waiting(&self, id: &str, message: &str) {
        let mut waiting = self.waiting.lock();
        if let Some(w) = waiting.get_mut(id) {
            w.queue.retain(|waiter| waiter.message != message);
        }
    }

    /// Whether one of `sender`'s messages waits in session `id`'s journal here.
    fn waits_from(&self, id: &str, sender: &str) -> bool {
        self.waiting.lock().get(id).is_some_and(|w| w.queue.iter().any(|waiter| waiter.sender == sender))
    }

    /// Session `id` left here or ended: what waited for it went with it (its journal's transfer) or ended with it.
    pub(super) fn forget_waiting(&self, id: &str) {
        self.waiting.lock().remove(id);
        self.server_mail.lock().remove(id);
    }

    /// A status from `sender` under `status` replaces their still-waiting ones under the same key (W20 coalescing): each
    /// is taken out of the journal and never handed to the session. One handed on already stays as it went.
    async fn supersede_waiting(&self, id: &str, input: &Input, sender: &str, status: &StatusKey, epoch: u64) {
        let replaced: Vec<String> = {
            let waiting = self.waiting.lock();
            waiting.get(id).map(|w| w.queue.iter().filter(|waiter| waiter.sender == sender && waiter.status.as_ref() == Some(status)).map(|waiter| waiter.message.clone()).collect()).unwrap_or_default()
        };
        for message in replaced {
            match input.inbox.supersede_waiting(id, &message, epoch).await {
                Ok(_) => self.drop_waiting(id, &message),
                Err(e) => tracing::warn!(target: "kiln.sends", event = "supersede_failed", session = %id, %message, error = %e, "a status a newer one replaces still waits: it couldn't be taken out of its journal"),
            }
        }
    }

    /// Session `id`'s admission told who waits and since when: the senders waiting in its own journal (each from its
    /// oldest), beside those the server last said it holds for it (W20 custody). Whether that changed what it knew.
    pub(super) fn set_demand(&self, id: &str, input: &Input) -> bool {
        let mut demand: Vec<Demand> = Vec::new();
        if let Some(w) = self.waiting.lock().get(id) {
            for waiter in &w.queue {
                match demand.iter_mut().find(|d| d.peer == waiter.sender) {
                    Some(d) => d.waiting = Some(d.waiting.unwrap_or(1).saturating_add(1)),
                    None => demand.push(Demand { peer: waiter.sender.clone(), revision: waiter.seq, waiting: Some(1) }),
                }
            }
        }
        let mut peers: Vec<String> = demand.iter().map(|d| d.peer.clone()).collect();
        if let Some((server_peers, server_demand)) = self.server_mail.lock().get(id) {
            for peer in server_peers {
                if !peers.contains(peer) {
                    peers.push(peer.clone());
                }
            }
            for d in server_demand {
                if !demand.iter().any(|mine| mine.peer == d.peer) {
                    demand.push(d.clone());
                }
            }
        }
        let mut controller = input.controller.lock();
        let peers_changed = controller.peers(&peers).unwrap_or(false);
        let demand_changed = controller.demand(&demand).unwrap_or(false);
        peers_changed || demand_changed
    }

    /// While anything waits in session `id`'s journal here, each change of its room queues an admission pass on its
    /// lane ([`Host::admit_waiting`]): one task for the session, ending once nothing waits.
    pub(super) fn watch_room(&self, id: &str, input: &Input) {
        {
            let mut waiting = self.waiting.lock();
            let Some(w) = waiting.get_mut(id) else { return };
            if w.watched {
                return;
            }
            w.watched = true;
        }
        let room = input.controller.lock().room();
        let (me, id) = (self.me.clone(), id.to_owned());
        drop(crate::rt::spawn(async move {
            loop {
                let listen = room.listen();
                let Some(h) = me.upgrade() else { return };
                if !h.wants_admission(&id) {
                    return;
                }
                h.queue_admission(&id);
                drop(h);
                listen.await;
            }
        }));
    }

    /// Whether anything waits in session `id`'s journal here; when nothing does, its watch ends.
    fn wants_admission(&self, id: &str) -> bool {
        let mut waiting = self.waiting.lock();
        match waiting.get_mut(id) {
            Some(w) if !w.queue.is_empty() => true,
            Some(w) => {
                w.watched = false;
                false
            }
            None => false,
        }
    }

    /// An admission pass for session `id`, queued on its lane unless one waits there already.
    pub(super) fn queue_admission(&self, id: &str) {
        {
            let mut waiting = self.waiting.lock();
            let Some(w) = waiting.get_mut(id) else { return };
            if w.queued {
                return;
            }
            w.queued = true;
        }
        let lane = self.lanes.lock().get(id).map(|l| l.data.clone());
        let sent = lane.is_some_and(|lane| lane.try_send(Lane::Admit).is_ok());
        if !sent && let Some(w) = self.waiting.lock().get_mut(id) {
            // Its lane is full or gone: the next change of its room tries again.
            w.queued = false;
        }
    }
}

/// What's routed to a session here, against a session no task runs, so what it's handed stays handed.
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::{Host, Limits, inbox};
    use crate::harness::admission::{Consumption, Disposition, Stage};
    use crate::harness::core::Update;
    use crate::harness::mail::StatusKey;
    use crate::harness::model::{Command, SendMode};
    use crate::harness::route::Route;
    use crate::harness::session::Session;

    struct Close {
        host: Arc<Host>,
        id: String,
    }
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = self.host.end(&self.id);
        }
    }

    struct Fixture {
        host: Arc<Host>,
        rx: async_channel::Receiver<(String, Update)>,
        s: Arc<Session>,
        id: String,
        _close: Close,
        _held: parking_lot::Mutex<Box<dyn std::any::Any + Send>>,
    }

    fn send(id: &str, text: &str) -> Command {
        Command::Send { id: id.into(), text: text.into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }
    }

    /// A host on simulated records, room for its sessions' input, and one session no task runs (what it's handed stays
    /// handed), its lane open: ready once its admission takes messages.
    async fn fixture() -> anyhow::Result<Fixture> {
        let id = crate::util::new_id();
        let (tx, rx) = async_channel::unbounded();
        let shares = crate::memory::room::Shares::new();
        let host = Host::with_records_following(tx, None, Limits::default(), Arc::new(kiln_seams::clock::SimClock::new()), Some(Arc::new(kiln_seams::records::SimRecords::new())), &shares);
        host.set_input_budget(1 << 20);
        let (s, held) = Session::idle_for_tests(&id, 1);
        let s = Arc::new(s);
        host.sessions.lock().insert(id.clone(), Arc::clone(&s));
        {
            let mut lanes = host.lanes.lock();
            host.open_lane(&mut lanes, &id, 1, false);
        }
        let close = Close { host: Arc::clone(&host), id: id.clone() };
        for _ in 0..2000 {
            let ready = s.core.lock().input.as_ref().is_some_and(|input| input.controller.lock().capacity().target > 0);
            if ready {
                return Ok(Fixture { host, rx, s, id, _close: close, _held: parking_lot::Mutex::new(held) });
            }
            crate::rt::sleep(Duration::from_millis(5)).await;
        }
        anyhow::bail!("the session's admission never took messages")
    }

    impl Fixture {
        /// Routes `c` to the session from `sender`, and waits for its inbox's answer.
        async fn route(&self, c: Command, route: Route) -> anyhow::Result<Result<inbox::Answer, inbox::Refusal>> {
            let message = crate::harness::registry::message_id(&c).ok_or_else(|| anyhow::anyhow!("no message id"))?;
            anyhow::ensure!(self.host.command_routed(&self.id, c, Some(self.s.epoch()), route), "its lane didn't take it");
            crate::rt::timeout(crate::harness::registry::ANSWER_WAIT, async {
                loop {
                    let (source, update) = self.rx.recv().await?;
                    if source != self.id {
                        continue;
                    }
                    if let Update::Inbox { said } = update
                        && said.message == message
                    {
                        return Ok::<_, anyhow::Error>(said.answer);
                    }
                }
            })
            .await
            .map_err(|_| anyhow::anyhow!("no answer came for {message}"))?
        }

        fn input(&self) -> anyhow::Result<crate::harness::admission::Input> {
            self.s.core.lock().input.clone().ok_or_else(|| anyhow::anyhow!("no admission"))
        }

        /// Handed to the session: its admission's lease is past its inbox (or the message ended), and its journal row
        /// doesn't wait.
        async fn handed(&self, message: &str) -> anyhow::Result<bool> {
            let row = self.pending(message).await?;
            let stage = self.input()?.controller.lock().lease(message).map(|lease| lease.stage);
            Ok(row.as_ref().is_none_or(|row| !row.waiting_here()) && stage.is_none_or(|stage| !matches!(stage, Stage::Reserved | Stage::InboxAccepted | Stage::Waiting)))
        }

        /// Waiting in the session's journal: its row waits there, and it holds no lease.
        async fn waiting(&self, message: &str) -> anyhow::Result<bool> {
            let row = self.pending(message).await?;
            Ok(row.is_some_and(|row| row.waiting_here()) && self.input()?.controller.lock().lease(message).is_none())
        }

        async fn until_handed(&self, message: &str) -> anyhow::Result<()> {
            for _ in 0..2000 {
                if self.handed(message).await? {
                    return Ok(());
                }
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            anyhow::bail!("{message} never reached the session: its row {:?}", self.pending(message).await?)
        }

        async fn pending(&self, message: &str) -> anyhow::Result<Option<inbox::Pending>> {
            let inbox = self.host.inbox().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            Ok(inbox.pending(&self.id, message).await?)
        }
    }

    #[test]
    fn cp_t3_2_each_routed_call_owns_its_receipt_even_when_a_retry_is_cancelled() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            let route = Route::new("local", "user:ada");
            let accepted = f.route(send("one", "body"), route.clone()).await??;
            let ask = |text| f.host.command_routed_reply(&f.id, send("one", text), f.s.epoch(), route.clone())
                .ok_or_else(|| anyhow::anyhow!("session lane unavailable"));
            drop(ask("body")?);
            let wrong = ask("different")?;
            let same = ask("body")?;
            let (wrong, same) = futures::join!(wrong, same);
            anyhow::ensure!(matches!(wrong?.answer, Err(inbox::Refusal::Reused { .. })));
            let repeat = same?.answer?;
            anyhow::ensure!(repeat.repeat && repeat.seq == accepted.seq);
            Ok(())
        })
    }

    #[test]
    fn a_persons_routed_message_is_taken_by_its_sessions_inbox_and_handed_on_at_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            let taken = f.route(send("m1", "hello"), Route::new("local", "user:ada")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(!taken.repeat);
            f.until_handed("m1").await?;
            if let Some(row) = f.pending("m1").await? {
                anyhow::ensure!(row.origin == inbox::Origin::Routed && row.lease.stage != Stage::Waiting && row.lease.peer == "user:ada", "{row:?}");
            }
            // Sent again (a retry, a forward that raced it): its repeat, and nothing more is handed on.
            let again = f.route(send("m1", "hello"), Route::new("local", "user:ada")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(again.repeat && again.seq == taken.seq, "{again:?} after {taken:?}");
            anyhow::ensure!(f.input()?.controller.lock().capacity().admitted <= 1, "a repeat took a second place");
            Ok(())
        })
    }

    #[test]
    fn an_agents_routed_messages_wait_in_its_sessions_journal_past_its_depth_and_go_as_room_frees() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            let agent = || Route::new("local", "agent:a1");
            f.route(send("m1", "first"), agent()).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            f.until_handed("m1").await?;
            // One sender's round is one place: the second is taken into custody here, and waits in the journal.
            let second = f.route(send("m2", "second"), agent()).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(!second.repeat);
            anyhow::ensure!(f.waiting("m2").await?, "the agent's second didn't wait: {:?}", f.pending("m2").await?);
            // A person's still goes at once, past the agent's waiting one.
            f.route(send("p1", "a person's"), Route::new("local", "admin")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            f.until_handed("p1").await?;
            anyhow::ensure!(f.waiting("m2").await?, "the agent's second went past its depth");
            // A replay (a reconnect) doesn't hand on what waits.
            f.host.replay_mail_of(&std::collections::BTreeSet::from([f.id.clone()])).await;
            anyhow::ensure!(f.waiting("m2").await?, "a replay handed on a waiting message");
            // The first consumed: room frees, and the second goes on with no word from the server.
            let input = f.input()?;
            let now = input.now_ns();
            input.controller.lock().finish("m1", Disposition::Consumed { boundary: Consumption::ProviderAccepted }, now, false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            f.until_handed("m2").await?;
            Ok(())
        })
    }

    #[test]
    fn a_newer_status_replaces_one_still_waiting_and_never_one_handed_on() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            let news = |key: &str| Route { status: Some(StatusKey { task: None, key: key.into() }), ..Route::new("local", "server") };
            f.route(send("n1", "news one"), news("internal-news")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            f.until_handed("n1").await?;
            f.route(send("n2", "news two"), news("internal-news")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(f.waiting("n2").await?, "the second news didn't wait behind the first");
            f.route(send("n3", "news three"), news("internal-news")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(f.pending("n2").await?.is_none(), "the waiting status a newer one replaced is still in the journal");
            anyhow::ensure!(f.pending("n1").await?.is_some_and(|row| !row.waiting_here()), "one handed on already stays as it went");
            // Its id stays taken: sent again, it's its repeat, and still never handed on.
            let again = f.route(send("n2", "news two"), news("internal-news")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(again.repeat);
            let input = f.input()?;
            let now = input.now_ns();
            input.controller.lock().finish("n1", Disposition::Consumed { boundary: Consumption::ProviderAccepted }, now, false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            f.until_handed("n3").await?;
            anyhow::ensure!(f.pending("n2").await?.is_none() && f.input()?.controller.lock().lease("n2").is_none(), "the replaced news was handed on after all");
            Ok(())
        })
    }

    /// A burst routed to one session (what waits in its lane is taken together, its store syncing the takes at once)
    /// reaches it whole and in the order it was sent, each message taken once.
    #[test]
    fn cp_t3_2_urgent_control_preempts_a_live_routed_backlog_and_new_arrivals_wait_one_turn() -> anyhow::Result<()> {
        use super::Lane;
        let (data, rx) = crate::memory::queue::places(&crate::memory::owners::CHANNELS, super::super::LANE);
        let (controls, high) = crate::memory::queue::places(&crate::memory::owners::CHANNELS, super::super::CONTROL_LANE);
        let request = |id: &str| Lane::Routed {
            c: Box::new(send(id, id)), epoch: Some(1), at: std::time::Instant::now(),
            route: Box::new(Route::new("here", "user:test")), reply: None,
        };
        data.try_send(request("a")).map_err(|_| anyhow::anyhow!("closed data queue"))?;
        data.try_send(request("b")).map_err(|_| anyhow::anyhow!("closed data queue"))?;
        let mut pending = None;
        {
            let mut more = super::following(&rx, &high, &mut pending);
            assert!(matches!(more(),Some((Command::Send{id,..},..)) if id=="a"));
            controls.try_send(Lane::Admit).map_err(|_| anyhow::anyhow!("closed control queue"))?;
            assert!(more().is_none(), "a control that arrived during planning must end this burst");
            assert_eq!(rx.len(), 1);
        }
        assert!(matches!(high.try_recv(), Ok(Lane::Admit)));
        {
            let mut more = super::following(&rx, &high, &mut pending);
            data.try_send(request("c")).map_err(|_| anyhow::anyhow!("closed data queue"))?;
            assert!(matches!(more(),Some((Command::Send{id,..},..)) if id=="b"));
            assert!(more().is_none(), "new input cannot extend an in-flight turn indefinitely");
        }
        assert_eq!(rx.len(), 1);
        assert!(pending.is_none());
        Ok(())
    }

    #[test]
    fn a_burst_routed_to_a_session_reaches_it_once_each_in_the_order_sent() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            let ids: Vec<String> = (0..20).map(|n| format!("b{n:02}")).collect();
            for id in &ids {
                anyhow::ensure!(f.host.command_routed(&f.id, send(id, "burst"), Some(f.s.epoch()), Route::new("local", "user:ada")), "its lane didn't take {id}");
            }
            let mut answered = std::collections::BTreeMap::new();
            crate::rt::timeout(crate::harness::registry::ANSWER_WAIT, async {
                while answered.len() < ids.len() {
                    let (source, update) = f.rx.recv().await?;
                    if source == f.id && let Update::Inbox { said } = update {
                        answered.insert(said.message.clone(), said.answer);
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .map_err(|_| anyhow::anyhow!("not every message of the burst was answered: {}", answered.len()))??;
            anyhow::ensure!(answered.values().all(|a| a.as_ref().is_ok_and(|a| !a.repeat)), "{answered:?}");
            // Taken in the order sent: each one's take committed after the one before it.
            let seqs: Vec<u64> = ids.iter().filter_map(|id| answered.get(id).and_then(|a| a.as_ref().ok()).map(|a| a.seq)).collect();
            anyhow::ensure!(seqs.len() == ids.len() && seqs.windows(2).all(|w| w.first() < w.get(1)), "in the order sent: {seqs:?}");
            for id in &ids {
                f.until_handed(id).await?;
            }
            Ok(())
        })
    }

    /// A session resumed here from the server's store (its machine gone) brings what still waited in its journal there:
    /// each is taken by its inbox here, from its sender, and handed on as its admission allows.
    #[test]
    fn what_waited_where_a_resumed_session_was_is_taken_here() -> anyhow::Result<()> {
        use crate::harness::moving::Waiting;
        crate::rt::block_on(async {
            let f = fixture().await?;
            let taken = f.host.take_carried(&f.id, vec![Waiting { command: send("w1", "the agent's"), sender: "agent:a1".into(), status: None }, Waiting { command: send("w2", "a person's"), sender: "admin".into(), status: None }]);
            anyhow::ensure!(taken == 2, "{taken}");
            let mut answered = std::collections::BTreeMap::new();
            crate::rt::timeout(crate::harness::registry::ANSWER_WAIT, async {
                while answered.len() < 2 {
                    let (source, update) = f.rx.recv().await?;
                    if source == f.id
                        && let Update::Inbox { said } = update
                    {
                        answered.insert(said.message.clone(), said.answer);
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .map_err(|_| anyhow::anyhow!("what waited wasn't answered here"))??;
            anyhow::ensure!(answered.values().all(|a| a.as_ref().is_ok_and(|a| !a.repeat)), "{answered:?}");
            f.until_handed("w1").await?;
            f.until_handed("w2").await?;
            Ok(())
        })
    }

    #[test]
    fn a_session_not_here_takes_nothing_routed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            anyhow::ensure!(!f.host.command_routed("not-here", send("m1", "x"), Some(1), Route::new("local", "admin")));
            Ok(())
        })
    }

    #[test]
    fn burst_under_tight_physical_budget_succeeds_without_byte_authority_rejection() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            // Tight budget: enough for 1-2 simultaneous transaction encodings, far less than 12x.
            let payload = "x".repeat(4096);
            f.host.set_input_budget(20 * 1024);
            let ids: Vec<String> = (0..12).map(|n| format!("tight-{n:02}")).collect();
            for id in &ids {
                anyhow::ensure!(f.host.command_routed(&f.id, send(id, &payload), Some(f.s.epoch()), Route::new("local", "agent:a1")), "lane rejected {id}");
            }
            let mut answered = std::collections::BTreeMap::new();
            crate::rt::timeout(crate::harness::registry::ANSWER_WAIT, async {
                while answered.len() < ids.len() {
                    let (source, update) = f.rx.recv().await?;
                    if source == f.id && let Update::Inbox { said } = update {
                        answered.insert(said.message.clone(), said.answer);
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .map_err(|_| anyhow::anyhow!("only {} of {} answered", answered.len(), ids.len()))??;
            anyhow::ensure!(answered.values().all(|a| a.is_ok()), "tight budget rejected burst messages: {answered:?}");
            Ok(())
        })
    }

    #[test]
    fn transient_unavailable_self_heals_when_storage_recovers() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let f = fixture().await?;
            let input = f.input()?;
            // Simulate a transient storage hiccup latching Readiness::Unavailable.
            input.controller.lock().unavailable().map_err(|e| anyhow::anyhow!("{e:?}"))?;
            anyhow::ensure!(input.controller.lock().capacity().target == 0, "latched unavailable has target 0");
            // Once storage works again and accepts a routed message, readiness recovers and the
            // waiting message is handed to the session instead of staying bricked forever.
            let taken = f.route(send("heal-1", "after hiccup"), Route::new("local", "user:ada")).await?.map_err(|r| anyhow::anyhow!("refused: {r}"))?;
            anyhow::ensure!(!taken.repeat);
            anyhow::ensure!(input.controller.lock().capacity().target > 0, "readiness did not recover after durable commit");
            f.until_handed("heal-1").await?;
            Ok(())
        })
    }
}
