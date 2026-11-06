//! A runner's loops for what its server sends (runner-fan; goal 10, plan 01a1a87b2dae step 4), the runner's counterpart of
//! the hub's (`crate::wire::fan`, `crate::hub::Hub::net_loops`). The net thread hands each of the server's frames to the
//! loop that takes it, read off its head by the server's heads (`crate::wire::fan::peek_server`): what's for one agent
//! session (a message for it, a question about it, a command on its own command streams) to the session loop on the
//! shard that owns the session ([`session_loop`], `crate::harness::copies::owner_of` of the runner's shards), where its
//! session's tasks run too (`crate::harness::host::Host::owner_of`); the rest to the runner's loop as it was
//! (`super::run`, its control loop).
//!
//! One loop took every frame the server sent, in the order they came, behind every event of the runner's own (its
//! sessions' updates on their way up, its jobs, its clocks, an identity's file written and synced, a heartbeat's read of
//! its config): a message for one session waited for all of them, and its session's tasks shared that loop's shard with
//! every other session's, the runner's other shard idle (runner-fan's measurement, 2026-11-06: the loop and every session's
//! drive task on one of a runner's two shards). Now a session's frames go to its own shard's loop, beside its tasks, and
//! wait for nothing of another session's or of the control loop's but what they're ordered against.
//!
//! - **Order.** The fan numbers what it hands over, and each loop marks how far it took (`crate::wire::fan::Marks`): what
//!   a session's frames are judged by (the handshake, the server's caps, a session's identity, custody, the agent limit,
//!   what the server has of the runner's streams), and a session's command on the process's own command streams (a server
//!   from before a session's own), are barriers on the control loop, ordered both ways against the session loops; the rest
//!   is plain, ordered against nothing of theirs.
//! - **Blue/green.** While a hand-over to the machine's new build is under way, or sessions were handed to it, the control
//!   loop decides what goes on to the new build (`super::goes_on`): the session loops hand it their frames ([`Deferral`]),
//!   in their order, until it says nothing's handed over and it took all they handed it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::exec::{Up, Uplink};
use crate::net::NetEvent;
use crate::proto::{self, AgentMsg, ServerMsg};
use crate::route_latency::{InKind, InLeg, observe_in};
use crate::wire::fan::{Marks, Routed};
use crate::wire::payload::{Carried, FromServer};

/// What a runner's loops share for its agent sessions' frames.
pub(super) struct Shared {
    /// Its agent sessions (none: it runs none).
    pub harness: Option<Arc<crate::harness::host::Host>>,
    /// What the runner says of its own, in order (`super::run`'s `own`): a session refused, said gone.
    pub own: Uplink,
    /// Its uplink: a question's answer, made off the loop.
    pub uplink: Uplink,
    /// kiln's root, this machine's name and this process's instance: what another agent's relayed words are checked by
    /// (docs/ci.md W27-R08).
    pub root: Vec<u8>,
    pub name: String,
    pub instance_id: String,
    /// Its state folder: where a session's live pages keep what they serve.
    pub state_dir: std::path::PathBuf,
    /// The server takes word of a frame this runner couldn't read (its caps, said on its `Caps`: a barrier, so what a
    /// session loop takes after it sees it set).
    pub server_unread: AtomicBool,
    /// The server checks custody records (its caps' `CAP_CUSTODY`): a hand-over's word is checked against it.
    pub server_custody: AtomicBool,
    /// The runner's net thread: a session loop's answers to its sessions' commands go straight to it.
    pub net: crate::net::NetHandle,
    /// What each session loop's receivers have of their sessions' own command streams, as each last changed them: what the
    /// runner says it has (its `Have`) is the control loop's with all of these (`have_of`).
    pub have: Box<[crate::rcu::Rcu<Vec<crate::wire::channel::Has>>]>,
    /// The link's comings and goings, counted by the control loop: a session loop's receivers go down with it (their gaps
    /// asked for again on the next link only once an item past them comes), as the control loop's do.
    pub link: AtomicU64,
    /// What the control loop answered of a session's own command streams while blue/green had it take their items, by
    /// stream: the session loop takes it up before that stream's next item, so it takes none of them again (a resend of one
    /// whose answer was lost is answered again, never applied twice), and what the runner says it has never goes back.
    pub adopt: crate::rcu::Rcu<std::collections::HashMap<String, crate::wire::channel::Has>>,
    /// Frames handed to the control loop while blue/green decides what goes on (Unix's alone).
    pub deferral: Deferral,
}

/// The session loops' frames while the control loop decides what of them goes on to the machine's new build (blue/green,
/// `super::goes_on`): from when a hand-over begins until nothing's handed over any more, each session loop hands the control
/// loop every frame it takes, in its order, and goes on taking them itself only once the control loop took all it handed.
/// No lock: a loop applying a frame says so before it looks whether to hand it over (`busy`), and the control loop, having
/// said a hand-over begins (`on`), waits until none is (Dekker's: both sequentially consistent), so nothing a session loop
/// applies overlaps what the control loop hands over.
pub(super) struct Deferral {
    on: AtomicBool,
    /// Each session loop applying a frame now.
    busy: Box<[AtomicBool]>,
    /// Told as a session loop is done applying one.
    idle: crate::rt::Changed,
    /// Each session loop's frames handed to the control loop and not yet taken there: while any, it hands its next too.
    handed: Box<[AtomicU64]>,
    /// The control loop's end of them: which session loop's, its number from the fan, and the frame. The control loop
    /// marks it taken for that loop once it applied it (`crate::wire::fan::Marks::took`), so what's ordered after it
    /// still waits for it.
    tx: crate::memory::queue::Sender<(usize, u64, Vec<u8>)>,
}

impl Deferral {
    /// One for `loops` session loops, and the control loop's end of what they hand it.
    pub fn new(loops: usize) -> (Self, crate::memory::queue::Receiver<(usize, u64, Vec<u8>)>) {
        // Unbounded: what's handed is what the fan handed the session loops, bounded where it was (`crate::net::EVENTS_BYTES`).
        let (tx, rx) = crate::memory::queue::unbounded(&crate::memory::owners::NET_EVENTS);
        let d = Self { on: AtomicBool::new(false), busy: (0..loops).map(|_| AtomicBool::new(false)).collect(), idle: crate::rt::Changed::new(), handed: (0..loops).map(|_| AtomicU64::new(0)).collect(), tx };
        (d, rx)
    }

    /// The control loop: from now on the session loops hand it their frames, none of them applying one when this returns.
    // Blue/green is Unix's alone: elsewhere none begins.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub async fn begin(&self) {
        self.on.store(true, Ordering::SeqCst);
        loop {
            // Listened for first, then looked at: a loop done after the look is a notify after the listen.
            let done = self.idle.listen();
            if self.busy.iter().all(|b| !b.load(Ordering::SeqCst)) {
                return;
            }
            done.await;
        }
    }

    /// The control loop: nothing's handed over any more; each session loop goes on taking its frames itself once the
    /// control loop took what it handed.
    // Blue/green is Unix's alone: elsewhere none begins.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn end(&self) {
        self.on.store(false, Ordering::SeqCst);
    }

    /// Whether a hand-over is under way.
    // Blue/green is Unix's alone: elsewhere none begins.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn under_way(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// The control loop took a frame session loop `k` handed it.
    pub fn taken(&self, k: usize) {
        if let Some(h) = self.handed.get(k) {
            let _ = h.try_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_sub(1)));
        }
    }

    /// Session loop `me`: frame `n`, `data`, handed to the control loop (true), or to apply here, the loop marked applying
    /// until [`Deferral::applied`] (false).
    fn hand(&self, me: usize, n: u64, data: &[u8]) -> bool {
        let (Some(busy), Some(handed)) = (self.busy.get(me), self.handed.get(me)) else { return false };
        busy.store(true, Ordering::SeqCst);
        if !self.on.load(Ordering::SeqCst) && handed.load(Ordering::Acquire) == 0 {
            return false;
        }
        busy.store(false, Ordering::SeqCst);
        self.idle.notify();
        handed.fetch_add(1, Ordering::AcqRel);
        if self.tx.try_send((me, n, data.to_vec())).is_err() {
            // The control loop is gone: nothing takes it now.
            handed.fetch_sub(1, Ordering::AcqRel);
        }
        true
    }

    /// Session loop `me` is done applying a frame.
    fn applied(&self, me: usize) {
        if let Some(busy) = self.busy.get(me) {
            busy.store(false, Ordering::SeqCst);
            self.idle.notify();
        }
    }
}

/// Session loop `me` of a runner's (the session loop of shard `me - 1`, `crate::wire::fan::session_loop`): takes the frames
/// the fan hands it, each once the server's earlier frames it may not overtake are taken (`Routed::after`), on its owner's
/// shard beside the sessions' tasks; times their legs (`crate::route_latency::InLeg`); and marks each taken. Its sessions'
/// own command streams are its receivers' alone ([`Loop`]): their gaps it asks for again on its own clock. Returns once its
/// queue ends (the net thread gone).
pub(super) async fn session_loop(shared: Arc<Shared>, me: usize, rx: crate::memory::queue::Receiver<Routed>, marks: Arc<Marks>) {
    // Said ended as it goes, however it goes: nothing waits for it then.
    let _ending = marks.ending(me);
    let mut turns = crate::rt::Turns::new();
    let mut lp = Loop::new(me.saturating_sub(1), shared.link.load(Ordering::Acquire));
    loop {
        let next = futures::select_biased! {
            r = futures::FutureExt::fuse(turns.next(&rx)) => r,
            () = futures::FutureExt::fuse(super::sleep_until_opt(lp.receiver.want_due())) => {
                lp.wants(&shared).await;
                continue;
            }
        };
        let Some(Routed { ev, n, after, handed }) = next else { break };
        let order = if after.is_empty() {
            std::time::Duration::ZERO
        } else {
            let dequeued = Instant::now();
            if marks.wait(&after).await {
                turns = crate::rt::Turns::new();
            }
            dequeued.elapsed()
        };
        let began = Instant::now();
        if let NetEvent::Frame { data, at, .. } = &ev {
            observe_in(InLeg::Net, InKind::Session, handed.saturating_duration_since(*at));
            observe_in(InLeg::Wait, InKind::Session, began.saturating_duration_since(*at).saturating_sub(order));
            if !after.is_empty() {
                observe_in(InLeg::Order, InKind::Session, order);
            }
            // Handed to the control loop, it's that loop's to mark taken once it applied it.
            if shared.deferral.hand(lp.k, n, data) {
                observe_in(InLeg::Held, InKind::Session, began.elapsed());
                turns.end().await;
                continue;
            }
            lp.take(&shared, data).await;
            shared.deferral.applied(lp.k);
        }
        observe_in(InLeg::Held, InKind::Session, began.elapsed());
        marks.took(me, n);
        turns.end().await;
    }
}

/// What a session loop keeps of its own: its place among the session loops, the server's command streams of its sessions
/// (their receivers, and which of them are current), the link it last saw, and how it tells the server what it couldn't
/// read.
struct Loop {
    k: usize,
    receiver: crate::wire::channel::Receiver,
    current: crate::wire::commands::Current,
    link: u64,
    told: crate::wire::unread::Told,
}

impl Loop {
    fn new(k: usize, link: u64) -> Self {
        Self { k, receiver: crate::wire::channel::Receiver::timed(), current: crate::wire::commands::Current::new(), link, told: crate::wire::unread::Told::default() }
    }

    /// What its receivers have now, for the runner's `Have` (`have_of`).
    fn publish(&self, shared: &Shared) {
        if let (Some(h), crate::wire::channel::Frame::Have(has)) = (shared.have.get(self.k), self.receiver.have()) {
            h.set(has);
        }
    }

    /// The gaps in its sessions' streams to ask for again now, asked for.
    async fn wants(&mut self, shared: &Shared) {
        for f in self.receiver.wants(Instant::now()) {
            say(shared, AgentMsg::Wire(f)).await;
        }
    }

    /// A frame for one agent session, as its session loop takes it: read, what it carries read, applied; what couldn't be
    /// read refused as the control loop refuses it (its inbox answering a message whose id could be read). A command on its
    /// own stream is taken once however often it comes, applied, and answered at once, straight to the net thread.
    async fn take(&mut self, shared: &Shared, data: &[u8]) {
        use crate::wire::unread::Wire;
        let msg: ServerMsg = match proto::read_server(data) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(what = %e.what(), why = %e.why("ServerMsg"), "a frame from the server couldn't be read");
                if shared.server_unread.load(Ordering::Acquire)
                    && let Some(folded) = self.told.tell(Instant::now(), Wire::Server, e.code(), &e.what(), &e.why("ServerMsg"))
                {
                    shared.own.msg(AgentMsg::Unread { wire: Wire::Server, what: e.what(), code: e.code(), why: e.why("ServerMsg"), folded, session: None }).await;
                }
                return;
            }
        };
        if let ServerMsg::Wire(crate::wire::channel::Frame::Item { stream, seq, low, body }) = msg {
            self.item(shared, stream, seq, low, body).await;
            return;
        }
        // What's signed is checked over the bytes the server sent: another agent's words kiln relayed (docs/ci.md W27-R08).
        let raw = match &msg {
            ServerMsg::HarnessSend { json, .. } => Some(json.clone()),
            _ => None,
        };
        let msg = match self.payload(shared, msg).await {
            Some(m) => m,
            None => return,
        };
        match msg {
            Carried::Read(FromServer::HarnessSend { session, epoch, command }) => send(shared, session, epoch, *command, raw.as_deref()).await,
            Carried::Read(FromServer::Preview { req, session, ask }) => preview(shared, req, session, ask),
            Carried::Read(FromServer::Workspace { req, session, ask }) => workspace(shared, req, session, ask),
            Carried::Nothing(ServerMsg::HarnessTaskOutput { req, session, task, since }) => task_output(shared, req, session, task, since),
            // The fan hands a session loop no other (`crate::wire::fan::peek_server`).
            _ => tracing::warn!("a frame the server sent came to an agent session's loop that isn't one of a session's"),
        }
    }

    /// What `msg` carries, read (`crate::wire::payload_of`); none when it can't be, refused as the control loop refuses it.
    async fn payload(&mut self, shared: &Shared, msg: ServerMsg) -> Option<Carried<ServerMsg, FromServer>> {
        match crate::wire::payload_of(msg) {
            Ok(read) => Some(read),
            Err(e) => {
                tracing::warn!(what = %e.what, code = e.code.name(), why = %e.why, "what a message from the server carries couldn't be read");
                let tell = if shared.server_unread.load(Ordering::Acquire) { self.told.tell(Instant::now(), crate::wire::unread::Wire::ServerPayload, e.code, &e.what, &e.why) } else { None };
                for up in super::unread_payload(&e, tell).into_iter().chain(super::unread_send(shared.harness.as_deref(), &e)) {
                    shared.own.0.send(up).await.ok();
                }
                None
            }
        }
    }

    /// Item `seq` of one of its sessions' own command streams (docs/wire.md, "Commands"): taken once however often it comes,
    /// applied as if it had come alone, and answered (an ACK, or a NACK saying why not) straight to the net thread. A newer
    /// generation of its stream lets go of those it supersedes; an item of one let go of is dropped.
    async fn item(&mut self, shared: &Shared, stream: String, seq: u64, low: u64, body: Vec<u8>) {
        use crate::wire::channel::{Arrival, Refusal};
        use crate::wire::commands::Stand;
        // The link went since it last looked: its gaps asked for again on this one only once an item past them comes.
        let link = shared.link.load(Ordering::Acquire);
        if link != self.link {
            self.link = link;
            self.receiver.down();
        }
        match self.current.of(&stream) {
            Stand::Gone => return,
            Stand::Newer => self.receiver.forget_where(|s| s != stream && crate::wire::commands::superseded(&stream, s)),
            Stand::Current => {}
        }
        // What the control loop answered of this stream while blue/green had it take them, taken up first.
        if let Some(has) = shared.adopt.load().get(&stream).filter(|h| h.upto > self.receiver.state(&stream).upto) {
            self.receiver.restore(has.clone());
        }
        let answer = match self.receiver.on_item(&stream, seq, low, body, Instant::now()) {
            Arrival::Take(body) => {
                let (refused, closed) = match proto::read_server(&body) {
                    Ok(ServerMsg::Wire(_)) => (Some((Refusal::Malformed, "a channel frame inside a command".to_owned())), None),
                    Ok(inner) => self.apply(shared, inner).await,
                    Err(e) => {
                        if shared.server_unread.load(Ordering::Acquire)
                            && let Some(folded) = self.told.tell(Instant::now(), crate::wire::unread::Wire::ServerItem, e.code(), &e.what(), &e.why("ServerMsg"))
                        {
                            shared.own.msg(AgentMsg::Unread { wire: crate::wire::unread::Wire::ServerItem, what: e.what(), code: e.code(), why: e.why("ServerMsg"), folded, session: None }).await;
                        }
                        (Some((e.code().refusal(), e.why("ServerMsg"))), None)
                    }
                };
                let answer = match refused {
                    Some((code, why)) => self.receiver.refused(&stream, seq, code, why),
                    None => self.receiver.took(&stream, seq),
                };
                // A session closed here: what's known of its streams goes once its close is answered (a resend of it is
                // taken from the item's `low`, as a receiver that lost its stream takes it).
                if let Some(session) = closed {
                    let of = |s: &str| crate::wire::commands::session_of(s) == Some(session.as_str());
                    self.receiver.forget_where(of);
                    self.current.forget_session(&session);
                    shared.adopt.update(|m| m.retain(|s, _| !of(s)));
                }
                answer
            }
            Arrival::Answered(Some(a)) | Arrival::Gap(a) => a,
            Arrival::Answered(None) => return,
        };
        self.publish(shared);
        let began = Instant::now();
        say(shared, AgentMsg::Wire(answer)).await;
        observe_in(InLeg::Answer, InKind::Session, began.elapsed());
    }

    /// A command one of its sessions' own streams carried, applied: what became of it, and the session it closed.
    async fn apply(&mut self, shared: &Shared, inner: ServerMsg) -> (Refused, Option<String>) {
        use crate::wire::channel::Refusal;
        let raw = match &inner {
            ServerMsg::HarnessCmd { json, .. } => json.clone(),
            _ => String::new(),
        };
        let Some(msg) = self.payload(shared, inner).await else { return (Some((Refusal::Malformed, "what it carries couldn't be read".to_owned())), None) };
        match msg {
            // Applied here only while no blue/green hand-over is under way (`Deferral`): a session this runner doesn't hold
            // is gone here.
            Carried::Read(FromServer::HarnessCmd { session, command: c }) => (command(shared, session, *c, &raw, true).await, None),
            Carried::Nothing(ServerMsg::HarnessClose { session }) => {
                let refused = close(shared, session.clone());
                (refused, Some(session))
            }
            Carried::Nothing(ServerMsg::HarnessFile { session, file: f, offset, data, done }) => {
                file(shared, &session, &f, offset, &data, done);
                (None, None)
            }
            Carried::Nothing(ServerMsg::HarnessRelease { session }) => {
                release(shared, &session);
                (None, None)
            }
            Carried::Read(FromServer::StreamOpen { stream, session, spec }) => {
                stream_open(shared, stream, session, *spec);
                (None, None)
            }
            Carried::Read(FromServer::StreamControl { stream, control, .. }) => {
                crate::browser::stream::serve_control(&stream, control);
                (None, None)
            }
            Carried::Read(FromServer::StreamInput { stream, input, .. }) => {
                crate::browser::stream::serve_input(&stream, input);
                (None, None)
            }
            Carried::Nothing(ServerMsg::StreamClose { stream, .. }) => {
                drop(crate::rt::spawn(async move { crate::browser::stream::streams().close(&stream).await }));
                (None, None)
            }
            // A session's own stream carries a session's commands alone.
            _ => (Some((Refusal::Unsupported, "not a command for one agent session".to_owned())), None),
        }
    }
}

/// `msg`, said to the server straight from a session loop, on the lane the runner's own answers ride (`Up::Term`'s).
async fn say(shared: &Shared, msg: AgentMsg) {
    if let Ok(data) = proto::encode(&msg) {
        shared.net.send(crate::net::SERVER_PEER, crate::proto::LANE_TERM, crate::net::Kind::Term, data).await;
    }
}

/// What became of a command for one agent session: taken (none), or refused how and why, its answer on its stream.
pub(super) type Refused = Option<(crate::wire::channel::Refusal, String)>;

/// A command for agent session `session` (`HarnessCmd`; `raw`, its JSON as the server sent it). A hand-over's word (docs/ci.md
/// W27-R08) is applied only by the process it's for, at the epoch it holds the session at here, refused otherwise, kiln told;
/// and one about a session this runner handed to its machine's new build (not `gone_here`: blue/green) is that build's.
/// Another agent's words kiln passed on are checked end to end before the session takes them. One for a session not here is
/// refused `unknown`, and the session said gone (where it's `gone_here`). What became of it.
pub(super) async fn command(shared: &Shared, session: String, command: crate::harness::model::Command, raw: &str, gone_here: bool) -> Refused {
    use crate::wire::channel::Refusal;
    let harness = shared.harness.as_ref();
    let known = harness.is_some_and(|h| h.get(&session).is_some());
    let epoch_here = harness.and_then(|h| h.get(&session)).map(|s| s.epoch());
    let for_me = crate::runner::custody::word_for_me(&session, raw, shared.server_custody.load(Ordering::Acquire), &shared.root, &shared.name, &shared.instance_id, epoch_here).and_then(|()| {
        if !crate::runner::custody::hand_overs_word(raw) || gone_here {
            return Ok(());
        }
        let why = "it's about a session this runner handed to its machine's new build, which holds it now".to_owned();
        let word = crate::runner::custody::word_in(raw);
        crate::runner::custody::refused_word(&session, word.as_deref(), &why);
        Err(why)
    });
    let relayed = if known { crate::runner::custody::relayed_words(&session, raw, &shared.root, &shared.name, &shared.instance_id) } else { Ok(()) };
    match command {
        _ if for_me.is_err() => Some((Refusal::Refused, format!("a hand-over's word not for this process: {}", for_me.err().unwrap_or_default()))),
        _ if relayed.is_err() => Some((Refusal::Refused, relayed.err().unwrap_or_default())),
        // The server's word on a hand-over to this machine's newer runner process: never "gone here", even once this process
        // has let go. With no newer process to take it, a word that it wasn't taken settles as a refusal does.
        crate::harness::model::Command::HandedOver { error, unsure, by_newest, none_newer } => {
            if let Some(h) = harness {
                h.handed_over(&session, error, unsure, by_newest || none_newer);
            }
            None
        }
        c if known => harness.filter(|h| !h.command(&session, c)).map(|_| (Refusal::Refused, format!("session {session} didn't take it: its lane is full, or it just ended"))),
        _ => {
            // Gone here (e.g. stopped while the server was away): say so.
            if gone_here && let Ok(json) = serde_json::to_string(&crate::harness::core::Update::Closed) {
                shared.own.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
            }
            Some((Refusal::Unknown, format!("no session {session} here")))
        }
    }
}

/// Agent session `session` closed: one this runner doesn't have is gone here already (`unknown`); what it had going live
/// (its app, its streams) stops.
pub(super) fn close(shared: &Shared, session: String) -> Refused {
    let refused = (!shared.harness.as_ref().is_some_and(|h| h.close(&session))).then(|| (crate::wire::channel::Refusal::Unknown, format!("no session {session} here")));
    crate::browser::apps::stop(&session);
    let dir = shared.state_dir.clone();
    drop(crate::rt::spawn(async move { crate::browser::stream::streams().close_session(&dir, &session).await }));
    refused
}

/// A piece of a file a message to agent session `session` carries, kept.
pub(super) fn file(shared: &Shared, session: &str, file: &str, offset: u64, data: &[u8], done: bool) {
    if let Some(h) = &shared.harness
        && let Err(e) = h.receive(session, file, offset, data, done)
    {
        tracing::warn!(%session, error = %format!("{e:#}"), "could not store an attachment");
    }
}

/// Agent session `session` went on elsewhere: this machine lets go of it.
pub(super) fn release(shared: &Shared, session: &str) {
    if let Some(h) = &shared.harness {
        h.let_go(session);
    }
}

/// A page streamed for whoever views one of a session's (`crate::browser::stream`), opened.
pub(super) fn stream_open(shared: &Shared, stream: String, session: String, spec: crate::browser::stream::Spec) {
    drop(crate::rt::spawn(crate::browser::stream::serve_open(shared.state_dir.clone(), stream, session, spec, shared.uplink.0.clone())));
}

/// What the runner says it has of its server's command streams (its `Have`): its control loop's receivers' (`own`), with
/// each session loop's of its sessions' own (`Shared::have`), every stream in the order of its name. A session's stream both
/// have (one the control loop took while blue/green decided) is said as far as the further has it, with every refusal
/// either knows: it never goes back, which its server would take for a runner that lost what it answered.
pub(super) fn have_of(own: crate::wire::channel::Frame, shared: &Shared) -> crate::wire::channel::Frame {
    use std::collections::BTreeMap;
    let mut all: BTreeMap<String, crate::wire::channel::Has> = BTreeMap::new();
    let mut add = |h: crate::wire::channel::Has| match all.get_mut(&h.stream) {
        Some(had) => {
            had.upto = had.upto.max(h.upto);
            for n in h.nacks {
                if !had.nacks.iter().any(|x| x.seq == n.seq) {
                    had.nacks.push(n);
                }
            }
            had.nacks.sort_by_key(|n| n.seq);
        }
        None => {
            all.insert(h.stream.clone(), h);
        }
    };
    if let crate::wire::channel::Frame::Have(has) = own {
        has.into_iter().for_each(&mut add);
    }
    for h in shared.have.iter() {
        h.load().iter().cloned().for_each(&mut add);
    }
    crate::wire::channel::Frame::Have(all.into_values().collect())
}

/// A message for agent session `session`, sent for its epoch `epoch` (CP-T3.2): down its lane, through its inbox, which
/// answers it. Another agent's words kiln relayed (`raw`, as the server sent it) are checked end to end before the session
/// takes them: refused otherwise, its inbox's answer saying why (docs/ci.md W27-R08's relays). One for a session not here
/// is refused, and the session said gone.
pub(super) async fn send(shared: &Shared, session: String, epoch: u64, command: crate::harness::model::Command, raw: Option<&str>) {
    let known = shared.harness.as_ref().is_some_and(|h| h.get(&session).is_some());
    let relayed = if known { crate::runner::custody::relayed_words(&session, raw.unwrap_or_default(), &shared.root, &shared.name, &shared.instance_id) } else { Ok(()) };
    match command {
        c if relayed.is_err() => {
            let why = relayed.err().unwrap_or_default();
            if let Some(message) = crate::harness::registry::message_id(&c) {
                let what = format!("message {message}");
                let refused = crate::harness::inbox::Said { message, answer: Err(crate::harness::inbox::Refusal::Invalid { what, why }) };
                if let Ok(json) = serde_json::to_string(&crate::harness::core::Update::Inbox { said: Box::new(refused) }) {
                    shared.own.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                }
            }
        }
        c if known => {
            if let Some(h) = &shared.harness {
                h.command_at(&session, c, Some(epoch));
            }
        }
        c => {
            // Gone here: the message is refused, and the session said gone.
            let mut said = Vec::new();
            if let Some(message) = crate::harness::registry::message_id(&c) {
                let refused = crate::harness::inbox::Said { message, answer: Err(crate::harness::inbox::Refusal::NoSession { session: session.clone() }) };
                said.push(crate::harness::core::Update::Inbox { said: Box::new(refused) });
            }
            said.push(crate::harness::core::Update::Closed);
            for u in said {
                if let Ok(json) = serde_json::to_string(&u) {
                    shared.own.0.send(Up::Term(AgentMsg::Harness { session: session.clone(), json })).await.ok();
                }
            }
        }
    }
}

/// What a person opens live from agent session `session` (`crate::browser::apps`), answered off the loop.
pub(super) fn preview(shared: &Shared, req: u64, session: String, ask: serde_json::Value) {
    let (up, dir) = (shared.uplink.0.clone(), shared.state_dir.clone());
    let found = shared.harness.as_ref().and_then(|h| h.get(&session)).map(|s| (std::path::PathBuf::from(s.info().cwd), *s.pid.lock()));
    drop(crate::rt::spawn(async move {
        let answer = match found {
            None => serde_json::json!({"error": "no such session here"}),
            Some((cwd, pid)) => crate::browser::apps::serve(&dir, &session, &cwd, pid, &ask).await,
        };
        let _ = up.send(Up::Term(AgentMsg::Answer { req, json: Some(answer.to_string()) })).await;
    }));
}

/// A question about the folder agent session `session` works in, answered off the loop: the session says which, so a
/// question can only be about a folder an agent here works in. What it commits of the agent's work is signed as the agent,
/// with the key this process holds for its session at its epoch (W27-R03).
pub(super) fn workspace(shared: &Shared, req: u64, session: String, ask: Box<crate::workspace::Ask>) {
    let up = shared.uplink.0.clone();
    let found = shared.harness.as_ref().and_then(|h| h.get(&session)).map(|s| {
        let ws = s.spec.lock().workspace.clone();
        let info = s.info();
        let sign = crate::runner::identity::enabled().then(|| crate::runner::identity::slot("agent", Some(&session), Some(info.epoch)));
        // A compare of an agent's point: how it left its files, as its session keeps them.
        let mut versions = crate::workspace::git::Versions::new();
        if let crate::workspace::Ask::Compare { from, to, .. } = ask.as_ref() {
            let core = s.core.lock();
            for p in [from, to] {
                if let crate::workspace::git::Point::Agent { by } = p {
                    versions.insert(by.clone(), core.version_of(by));
                }
            }
        }
        (std::path::PathBuf::from(&info.cwd), std::path::PathBuf::from(info.work_dir()), ws, versions, sign)
    });
    drop(crate::rt::spawn(async move {
        let answer = match found {
            None => serde_json::json!({"error": "no such agent on this machine"}),
            Some((cwd, work, ws, versions, sign)) => crate::workspace::serve(&cwd, &work, ws.as_ref(), *ask, &versions, sign.as_deref()).await,
        };
        let _ = up.send(Up::Term(AgentMsg::Answer { req, json: Some(answer.to_string()) })).await;
    }));
}

/// What a task of agent session `session` wrote since `since`, read off the loop.
pub(super) fn task_output(shared: &Shared, req: u64, session: String, task: String, since: Option<u64>) {
    let s = shared.harness.as_ref().and_then(|h| h.get(&session));
    let up = shared.uplink.0.clone();
    drop(crate::rt::spawn(async move {
        let json = match s {
            Some(s) => crate::rt::blocking(move || s.task_output(&task, since).to_string()).await.ok(),
            None => None,
        };
        let _ = up.send(Up::Term(AgentMsg::HarnessTaskOutput { req, json })).await;
    }));
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    use super::*;

    /// runner-fan, blue/green: while a hand-over is under way a session loop hands the control loop its frames, numbered,
    /// in its order, and none applies one itself; once it ends, a loop the control loop still owes frames hands it its next
    /// too, until the control loop took all it handed, then applies its own again.
    #[test]
    fn runner_fan_a_hand_over_takes_the_session_loops_frames_in_their_order() -> anyhow::Result<()> {
        let (d, rx) = Deferral::new(2);
        let d = Arc::new(d);
        anyhow::ensure!(!d.hand(0, 1, b"a"), "no hand-over: the loop applies it");
        d.applied(0);
        let begun = Arc::clone(&d);
        crate::rt::block_on(async move {
            begun.begin().await;
            anyhow::Ok(())
        })?;
        anyhow::ensure!(d.under_way());
        anyhow::ensure!(d.hand(0, 2, b"b") && d.hand(0, 3, b"c") && d.hand(1, 4, b"d"), "under way: handed to the control loop");
        let got: Vec<(usize, u64, Vec<u8>)> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        anyhow::ensure!(got == vec![(0, 2, b"b".to_vec()), (0, 3, b"c".to_vec()), (1, 4, b"d".to_vec())], "{got:?}");
        d.end();
        anyhow::ensure!(d.hand(0, 5, b"e"), "the control loop still owes loop 0 two: its next goes there too");
        for _ in 0..3 {
            d.taken(0);
        }
        anyhow::ensure!(!d.hand(0, 6, b"f"), "all it handed taken: it applies its own again");
        d.applied(0);
        anyhow::ensure!(d.hand(1, 7, b"g"), "loop 1 is still owed one");
        Ok(())
    }

    /// runner-fan, blue/green: a hand-over begins only once no session loop is applying a frame (Dekker's: a loop says it's
    /// applying before it looks whether one is under way), so nothing a session loop applies overlaps what the control loop
    /// hands over; a loop that looks after the hand-over began hands its frame over.
    #[test]
    fn runner_fan_a_hand_over_begins_only_once_no_session_loop_is_applying() {
        let (d, _rx) = Deferral::new(1);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(!d.hand(0, 1, b"x"), "applying it");
        let mut begin = std::pin::pin!(d.begin());
        assert!(begin.as_mut().poll(&mut cx).is_pending(), "waits for the loop applying a frame");
        d.applied(0);
        assert_eq!(begin.as_mut().poll(&mut cx), Poll::Ready(()), "it's done: the hand-over begins");
        assert!(d.hand(0, 2, b"y"), "after: handed over");
    }

    /// What a session loop shares, with no sessions of its own, its own messages on `own`.
    fn shared(own: Uplink) -> (Arc<Shared>, crate::memory::queue::Receiver<(usize, u64, Vec<u8>)>) {
        let (s, deferred, _) = shared_and_net(own);
        (s, deferred)
    }

    /// [`shared`], and what its session loops send the server (`crate::net::NetHandle::detached`).
    fn shared_and_net(own: Uplink) -> (Arc<Shared>, crate::memory::queue::Receiver<(usize, u64, Vec<u8>)>, crate::memory::queue::Receiver<crate::net::Out>) {
        let (deferral, deferred) = Deferral::new(1);
        let (uplink, _) = Uplink::new();
        let (net, sent) = crate::net::NetHandle::detached();
        let s = Shared {
            harness: None,
            own,
            uplink,
            root: Vec::new(),
            name: "box".into(),
            instance_id: "main".into(),
            state_dir: std::env::temp_dir(),
            server_unread: AtomicBool::new(false),
            server_custody: AtomicBool::new(false),
            net,
            have: vec![crate::rcu::Rcu::new(Vec::new())].into_boxed_slice(),
            link: AtomicU64::new(0),
            adopt: crate::rcu::Rcu::new(std::collections::HashMap::new()),
            deferral,
        };
        (Arc::new(s), deferred, sent)
    }

    /// The channel's frames a session loop sent the server.
    fn said(sent: &crate::memory::queue::Receiver<crate::net::Out>) -> Vec<crate::wire::channel::Frame> {
        std::iter::from_fn(|| sent.try_recv().ok())
            .filter_map(|o| match o {
                crate::net::Out::Frame { data, .. } => match crate::proto::read_agent(&data) {
                    Ok(AgentMsg::Wire(f)) => Some(f),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The fan's frame `n` for a session loop: item `seq` of session `session`'s own live command stream, carrying `inner`.
    fn item(session: &str, seq: u64, inner: &ServerMsg, n: u64) -> anyhow::Result<Routed> {
        let msg = ServerMsg::Wire(crate::wire::channel::Frame::Item { stream: format!("l:7.1/{session}"), seq, low: seq.saturating_sub(1), body: proto::encode(inner)? });
        Ok(Routed { ev: NetEvent::Frame { peer: crate::net::SERVER_PEER, lane: crate::proto::LANE_CONTROL, kind: crate::net::Kind::Control, data: proto::encode(&msg)?, at: Instant::now() }, n, after: Vec::new(), handed: Instant::now() })
    }

    /// runner-fan (P, per-session command streams): a session loop takes the items of its session's own command stream
    /// with receivers of its own: each applied once however often it comes, answered straight to the net thread (a NACK
    /// saying why: no such session here, the session said gone), what it has published for the runner's `Have`; and once
    /// the session's close is answered, what it knew of the session's streams goes.
    #[test]
    fn runner_fan_a_session_loop_takes_its_sessions_own_command_stream() -> anyhow::Result<()> {
        use crate::wire::channel::{Frame, Refusal};
        crate::rt::block_on(async {
            let (own_tx, own_rx) = crate::memory::queue::unbounded::<Up>(&crate::memory::owners::CHANNELS);
            let (s, _deferred, sent) = shared_and_net(Uplink(own_tx));
            let marks = Arc::new(Marks::new(2));
            let interrupt = ServerMsg::HarnessCmd { session: "gone-3".into(), json: "{\"type\":\"interrupt\"}".into() };
            let (tx, rx) = crate::memory::queue::unbounded::<Routed>(&crate::memory::owners::CHANNELS);
            for r in [item("gone-3", 1, &interrupt, 1)?, item("gone-3", 1, &interrupt, 2)?] {
                tx.try_send(r).map_err(|_| anyhow::anyhow!("queue"))?;
            }
            drop(tx);
            session_loop(Arc::clone(&s), 1, rx, Arc::clone(&marks)).await;
            let answers = said(&sent);
            let refused = |f: &Frame| matches!(f, Frame::Ack(h) if h.stream == "l:7.1/gone-3" && h.upto == 1 && h.nacks.iter().any(|n| n.seq == 1 && n.code == Refusal::Unknown));
            anyhow::ensure!(answers.len() == 2 && answers.iter().all(refused), "its NACK, then again for the copy: {answers:?}");
            let closed = std::iter::from_fn(|| own_rx.try_recv().ok()).filter(|u| matches!(u, Up::Term(AgentMsg::Harness { session, .. }) if session == "gone-3")).count();
            anyhow::ensure!(closed == 1, "applied once: the session said gone once, not for its copy ({closed})");
            let have = s.have.first().map(|h| h.get()).unwrap_or_default();
            anyhow::ensure!(have.iter().any(|h| h.stream == "l:7.1/gone-3" && h.upto == 1), "published for the runner's Have: {have:?}");
            // Its close, answered: the stream's state goes.
            let close = ServerMsg::HarnessClose { session: "gone-3".into() };
            let (tx, rx) = crate::memory::queue::unbounded::<Routed>(&crate::memory::owners::CHANNELS);
            tx.try_send(item("gone-3", 2, &close, 3)?).map_err(|_| anyhow::anyhow!("queue"))?;
            drop(tx);
            session_loop(Arc::clone(&s), 1, rx, Arc::new(Marks::new(2))).await;
            let answers = said(&sent);
            anyhow::ensure!(answers.iter().any(|f| matches!(f, Frame::Ack(h) if h.upto == 2)), "its close answered: {answers:?}");
            let have = s.have.first().map(|h| h.get()).unwrap_or_default();
            anyhow::ensure!(have.iter().all(|h| !h.stream.ends_with("/gone-3")), "let go of once closed: {have:?}");
            Ok(())
        })
    }

    /// The fan's frame `n` for a session loop: a message for `session`, from the server.
    fn routed(session: &str, n: u64) -> anyhow::Result<Routed> {
        let msg = ServerMsg::HarnessSend { session: session.into(), epoch: 1, json: "{\"type\":\"send\",\"id\":\"m1\",\"text\":\"x\"}".into() };
        Ok(Routed { ev: NetEvent::Frame { peer: crate::net::SERVER_PEER, lane: crate::proto::LANE_TERM, kind: crate::net::Kind::Term, data: proto::encode(&msg)?, at: Instant::now() }, n, after: Vec::new(), handed: Instant::now() })
    }

    /// runner-fan: a session loop takes the server's message for a session on its own: for one not on this runner, its
    /// inbox's refusal and the session said gone go up, the frame is marked taken (what waits for it goes), and its legs
    /// are timed; while a hand-over is under way it hands the control loop the frame instead, unmarked (the control loop
    /// marks it once it applied it).
    #[test]
    fn runner_fan_a_session_loop_takes_a_message_and_hands_it_over_while_blue_green_decides() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (own_tx, own_rx) = crate::memory::queue::unbounded::<Up>(&crate::memory::owners::CHANNELS);
            let (s, deferred) = shared(Uplink(own_tx));
            let marks = Arc::new(Marks::new(2));
            let held = crate::route_latency::in_legs().of(InLeg::Held, InKind::Session).map_or(0, crate::metrics::PerShard::count);
            let (tx, rx) = crate::memory::queue::unbounded::<Routed>(&crate::memory::owners::CHANNELS);
            tx.try_send(routed("gone-1", 1)?).map_err(|_| anyhow::anyhow!("queue"))?;
            drop(tx);
            session_loop(Arc::clone(&s), 1, rx, Arc::clone(&marks)).await;
            let said: Vec<String> = std::iter::from_fn(|| own_rx.try_recv().ok())
                .filter_map(|u| match u {
                    Up::Term(AgentMsg::Harness { session, json }) if session == "gone-1" => Some(json),
                    _ => None,
                })
                .collect();
            anyhow::ensure!(said.len() == 2 && said.first().is_some_and(|j| j.contains("no_session")) && said.get(1).is_some_and(|j| j.contains("closed")), "{said:?}");
            anyhow::ensure!(marks.taken(1) == u64::MAX, "taken, and the loop ended as its queue did");
            let after = crate::route_latency::in_legs().of(InLeg::Held, InKind::Session).map_or(0, crate::metrics::PerShard::count);
            anyhow::ensure!(after > held, "its hold timed");
            // Under way: handed over, not applied, not marked.
            s.deferral.begin().await;
            let marks = Arc::new(Marks::new(2));
            let (tx, rx) = crate::memory::queue::unbounded::<Routed>(&crate::memory::owners::CHANNELS);
            tx.try_send(routed("gone-2", 7)?).map_err(|_| anyhow::anyhow!("queue"))?;
            let watched = Arc::clone(&marks);
            let lp = crate::rt::spawn(session_loop(Arc::clone(&s), 1, rx, Arc::clone(&marks)));
            // The bound only ends a hang: a loaded machine hands it over in what time it takes.
            let handed = crate::rt::timeout(Duration::from_secs(300), deferred.recv()).await.map_err(|_| anyhow::anyhow!("nothing handed over"))?.map_err(|_| anyhow::anyhow!("the control loop's end of the hand-over closed"))?;
            anyhow::ensure!(handed.0 == 0 && handed.1 == 7, "its place and number: {:?}", (handed.0, handed.1));
            anyhow::ensure!(watched.taken(1) == 0, "the control loop marks it, once it applied it");
            anyhow::ensure!(!std::iter::from_fn(|| own_rx.try_recv().ok()).any(|u| matches!(u, Up::Term(AgentMsg::Harness { ref session, .. }) if session == "gone-2")), "not applied here");
            drop(tx);
            let _ = lp.await;
            Ok(())
        })
    }
}
