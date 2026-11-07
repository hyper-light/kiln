//! What the net thread hands the hub, fanned out to the hub's loops, one a shard (goal 10, plan
//! 01a1a87b2dae step 4): an agent session's update to the loop on the shard that owns the session's copy
//! (`crate::harness::copies`), the rest to the hub's control loop, which takes it as the one loop did.
//!
//! One hub loop took every runner's frames, and an agent session's update crossed two threads on its way
//! to the shard that owns its copy: the net thread's to the loop's shard, then that shard's to the owner's.
//! On its way it waited behind every runner's frames before it and the control plane's handlers (a runner's
//! Hello, Build and Caps, custody, CI), on a shard of their own choosing: rt9's runner wait was 10.5 ms at
//! p99 calm, all but 0.2 ms of it that loop's (`to_loop`). Now it crosses one thread: the net thread hands
//! it to the session loop on its owner's shard, which hands it to the owner there.
//!
//! - **Where a frame goes** is read off its head where the net thread hands it over ([`peek`]), none of it
//!   copied: a `Harness` message, or a channel item on a session's stream (`s:<id>#…`), goes to the session
//!   loop of the shard that owns session `<id>` ([`crate::harness::copies::owner_of`] of [`shards`]); all
//!   else to the control loop ([`CONTROL`]).
//! - **Each loop's channel receivers are its own.** A session's streams always go to one loop, its owner's,
//!   so each session loop takes its streams' items, and answers them, with a `crate::wire::link::Links` of
//!   its own, whose lock it alone takes; the control loop takes the rest (terminals') with the hub's own
//!   (`Hub::links_of`). Per stream nothing changes: one loop takes it, in order, each item applied before
//!   it's answered. What the server says again of a runner's streams is what the loops' together have, in
//!   the order one's would say it (`Hub::wire_have`). `wire/link.rs` and `wire/channel.rs` are as they were.
//! - **A runner's order across the loops** is kept by a watermark. The net thread numbers every event it
//!   hands over, in order, and each loop marks how far it has taken ([`Marks`]); an event waits for the
//!   numbers it names ([`Routed::after`]) before its loop takes it:
//!   - a barrier ([`Head::Barrier`]: its link coming or going, its Hello, Machine, Instance, Build and Caps,
//!     which sessions it holds, a hand-over: what a session's update is judged by) waits until its runner's
//!     earlier session updates are taken (handed to their owners, an Open or a Closed applied there), and its
//!     runner's later updates wait for it;
//!   - a reader ([`Head::Reader`]: word of what it couldn't read, an identity or custody it asks or says, an
//!     agent's ask) reads what its runner's sessions said, so it waits for their updates before it, and none
//!     waits for it;
//!   - the rest ([`Head::Plain`]: telemetry, jobs and their logs, terminals, reach, ports, answers, the
//!     conversation store's feed) neither reads nor changes what an update is judged by: it's taken in order
//!     on the control loop, and no session's update waits for it, nor it for one.
//!
//! An event waits only for a runner's own earlier ones: different runners' links are unordered, as they are
//! on the network. The numbers only fall along a wait (an event waits for ones handed before it), so the
//! oldest event not yet taken can always be taken. The waits take no lock: a loop's mark is an atomic only
//! it writes, and a wait for one is a [`crate::rt::Changed`]'s.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use hyper_transport::PeerId;

use crate::net::{Handing, NetEvent};
use crate::proto::AgentMsg;

/// The control loop's place among the hub's loops: the shards' session loops follow it, shard `k`'s at
/// `k + 1`.
pub const CONTROL: usize = 0;

/// The loop that takes shard `shard`'s sessions' updates.
pub fn session_loop(shard: usize) -> usize {
    shard.saturating_add(1)
}

/// The shards whose session loops take sessions' updates: as many as own sessions' copies
/// (`crate::harness::copies::Copies::start`), so a session's loop is on its owner's shard.
pub fn shards() -> usize {
    crate::rt::shard_count().max(1)
}

/// What a runner's frame is, as its head says ([`peek`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head<'a> {
    /// An update of agent session `id`'s: its session loop is its owner's.
    Session(&'a str),
    /// What its runner's session updates are judged by: it waits for its runner's earlier updates, and its
    /// later ones wait for it.
    Barrier,
    /// What reads what its runner's sessions said: it waits for its runner's earlier updates.
    Reader,
    /// Neither: taken in order on the control loop, waiting for no session's update, nor one for it.
    Plain,
}

/// `AgentMsg`'s variants that a session's update from the same runner is judged by: who the runner is and
/// what it can do, which sessions it holds, and a session going from one of its processes to another.
/// A runner's admission (docs/resilience.md R5) is one too: what of it is taken hangs on its `Admit`, and its `Settled`
/// ends it once its earlier updates are taken.
const BARRIERS: &[&str] = &["Hello", "Machine", "Instance", "Build", "Caps", "HarnessInventory", "HarnessHandOver", "HarnessAdopted", "Admit", "Settled"];

/// Its variants that read what its runner's sessions said (where one runs, its epoch, its holder): word of a
/// frame it couldn't read, about a session it holds; an identity it asks for one (answered at once once the
/// session's Open is taken); custody it records; an agent's ask of kiln's operations.
const READERS: &[&str] = &["Unread", "Identity", "Custody", "ReachAsk"];

/// Its variants that neither change what a session's update is judged by nor read what its updates said:
/// telemetry, jobs and their output, terminals, upgrades, answers to the server's questions, sync, reach and
/// its relay, ports, the conversation store's feed, CI's.
const PLAIN: &[&str] = &[
    "Heartbeat",
    "Sample",
    "JobStarted",
    "StepStarted",
    "StepFinished",
    "JobFinished",
    "JobSample",
    "Reject",
    "TermOutput",
    "TermExit",
    "TermList",
    "TermReplay",
    "AiSessions",
    "AiTranscript",
    "AiView",
    "UpgradeLog",
    "UpgradeDone",
    "HarnessTaskOutput",
    "DirList",
    "HarnessHistory",
    "Sync",
    "FrontDoorStatus",
    "HeadroomStatus",
    "JobEvent",
    "Facts",
    "Reach",
    "ReachHello",
    "Convo",
    "UpdatePrefs",
    "Power",
    "Answer",
    "HarnessFileData",
    "StreamFrame",
    "StreamEvent",
    "PortOpened",
    "PortData",
    "PortAck",
    "PortClose",
    "Executor",
    "Undeployed",
    "Cluster",
    "NetDrops",
    "WorkState",
    "AgentLimit",
    "Observe",
    "LinkUnread",
    "LegResults",
    "LegSignature",
];

/// `AgentMsg`'s variants that are a session's update, or may carry one: a `Harness` message, and the
/// channel's frames (`Wire`), whose items on a session's stream are.
const SESSION: &[&str] = &["Harness", "Wire"];

/// The names of enum `T`'s variants in their order on the wire (postcard's tag is a variant's index), as its
/// derived `Deserialize` says them; none for a type that isn't an enum.
fn variants_of<T: serde::de::DeserializeOwned>() -> &'static [&'static str] {
    /// What [`Probe`] says: the variants it was asked to read one of.
    #[derive(Debug)]
    struct Variants(&'static [&'static str]);

    impl std::fmt::Display for Variants {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} variants", self.0.len())
        }
    }

    impl std::error::Error for Variants {}

    impl serde::de::Error for Variants {
        fn custom<M: std::fmt::Display>(_: M) -> Self {
            Variants(&[])
        }
    }

    /// A deserializer that reads nothing: asked for an enum, it says the enum's variants.
    struct Probe;

    impl<'de> serde::Deserializer<'de> for Probe {
        type Error = Variants;

        fn deserialize_any<V: serde::de::Visitor<'de>>(self, _: V) -> Result<V::Value, Variants> {
            Err(Variants(&[]))
        }

        fn deserialize_enum<V: serde::de::Visitor<'de>>(self, _: &'static str, variants: &'static [&'static str], _: V) -> Result<V::Value, Variants> {
            Err(Variants(variants))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier ignored_any
        }
    }

    match T::deserialize(Probe) {
        Err(Variants(v)) => v,
        Ok(_) => &[],
    }
}

/// What [`peek`] reads tags by, made once from the types themselves.
struct Tags {
    /// `AgentMsg::Harness`'s tag, `AgentMsg::Wire`'s, and the channel's `Frame::Item`'s within a `Wire`.
    harness: u64,
    wire: u64,
    item: u64,
    /// `AgentMsg::Admit`'s ([`admits`]).
    admit: u64,
    /// Each `AgentMsg` tag's [`Head`], by tag; a tag past them (a newer runner's) is a barrier.
    heads: Vec<Head<'static>>,
}

impl Tags {
    fn head(&self, tag: u64) -> Head<'static> {
        usize::try_from(tag).ok().and_then(|t| self.heads.get(t)).copied().unwrap_or(Head::Barrier)
    }
}

/// [`Tags`], made at first use.
fn tags() -> &'static Tags {
    static TAGS: OnceLock<Tags> = OnceLock::new();
    TAGS.get_or_init(|| {
        let agent = variants_of::<AgentMsg>();
        let tag = |names: &[&str], name: &str| names.iter().position(|n| *n == name).and_then(|i| u64::try_from(i).ok()).unwrap_or(u64::MAX);
        // A session's update whose head couldn't be read as one ([`peek`]) is a barrier: refused where it's read, in
        // its runner's order. So is a variant named in no list (one added since): ordered against its runner's sessions'
        // updates, both ways, as every message was on the one loop.
        let barrier = |name: &&str| SESSION.contains(name) || BARRIERS.contains(name) || !(READERS.contains(name) || PLAIN.contains(name));
        let heads = agent
            .iter()
            .map(|name| {
                if barrier(name) {
                    Head::Barrier
                } else if READERS.contains(name) {
                    Head::Reader
                } else {
                    Head::Plain
                }
            })
            .collect();
        Tags { harness: tag(agent, "Harness"), wire: tag(agent, "Wire"), item: tag(variants_of::<super::channel::Frame>(), "Item"), admit: tag(agent, "Admit"), heads }
    })
}

/// Whether runner frame `data` is an `Admit` (docs/resilience.md R5), read off its head and nothing more: of a runner
/// being let go (`crate::hub::admission`), it's the one frame heard, as it's how it comes back.
pub fn admits(data: &[u8]) -> bool {
    varint(data).is_some_and(|(tag, _)| tag == tags().admit)
}

/// The postcard varint at the head of `b` (as a `u64`, from at most ten bytes), and what follows it.
fn varint(b: &[u8]) -> Option<(u64, &[u8])> {
    let mut v: u64 = 0;
    for (i, byte) in b.iter().take(10).enumerate() {
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((v, b.get(i + 1..)?));
        }
    }
    None
}

/// What the server has of `runner`'s channel streams, as the receivers of the hub's loops have them together, `links`
/// (each stream one loop's, [`peek`]'s split): what one loop's receivers that took them all would say
/// (`crate::wire::channel::Receiver::have`), every stream in the order of its name. Sent as a runner's link comes up
/// and every few seconds (docs/wire.md), read off the copies each loop publishes as it changes them, with no lock.
pub fn have_of<'a>(links: impl IntoIterator<Item = &'a crate::wire::link::Links>, runner: &str) -> super::channel::Frame {
    let mut all: Vec<super::channel::Has> = Vec::new();
    for l in links {
        if let super::channel::Frame::Have(has) = l.have(runner) {
            all.extend(has);
        }
    }
    all.sort_by(|a, b| a.stream.cmp(&b.stream));
    super::channel::Frame::Have(all)
}

/// The postcard string at the head of `b` (its length, then its bytes), and what follows it.
fn text(b: &[u8]) -> Option<(&str, &[u8])> {
    let (len, rest) = varint(b)?;
    let len = usize::try_from(len).ok()?;
    let s = std::str::from_utf8(rest.get(..len)?).ok()?;
    Some((s, rest.get(len..)?))
}

/// What runner frame `data`, of `kind` on `lane`, is, read off its head: its message's variant, and for a session's
/// update the session's id (a `Harness` message's, or the stream's of the channel item it is). Reads no more of it than
/// that, and copies none of it. A channel item goes by the stream its head names, which is the stream any read of the
/// whole frame finds, so its items are taken with that stream's loop's receivers (`Hub::links_of`) however the rest of
/// it reads: refused there, should it not. One whose head can't be read so is a barrier: it's refused where it's read,
/// in its runner's order.
pub fn peek(kind: crate::net::Kind, lane: u32, data: &[u8]) -> Head<'_> {
    // A job's output in bulk is no message: its job's writer takes it.
    if crate::hub::Hub::job_log(kind, lane) {
        return Head::Plain;
    }
    let tags = tags();
    let Some((tag, rest)) = varint(data) else { return Head::Barrier };
    if tag == tags.harness {
        return text(rest).map_or(Head::Barrier, |(id, _)| Head::Session(id));
    }
    if tag == tags.wire {
        let Some((frame, rest)) = varint(rest) else { return Head::Barrier };
        // The runner's answers to the server's commands, its wants and what it has of them.
        if frame != tags.item {
            return Head::Plain;
        }
        return match text(rest) {
            // A session's stream's item is its update; a terminal's is its output.
            Some((stream, _)) => crate::wire::uplink::session_of(stream).map_or(Head::Plain, Head::Session),
            None => Head::Barrier,
        };
    }
    tags.head(tag)
}

/// Which end's frames a fan reads, to hand each to the loop that takes it: the server's of its runners' (`AgentMsg`,
/// [`Runners`]), a runner's of its server's (`ServerMsg`, [`Server`]; runner-fan).
pub trait Heads: Send + 'static {
    /// What frame `data`, of `kind` on `lane`, is, read off its head, none of it copied.
    fn head<'a>(&self, kind: crate::net::Kind, lane: u32, data: &'a [u8]) -> Head<'a>;

    /// Whether it hands any frame to a session's loop ([`Head::Session`]): a fan whose heads never do has its control loop
    /// alone, no queue of a session loop's left for nothing to take (whose end would close the queues' room for all).
    fn sessions(&self) -> bool {
        true
    }
}

/// The server's fan: its runners' frames ([`peek`]).
pub struct Runners;

impl Heads for Runners {
    fn head<'a>(&self, kind: crate::net::Kind, lane: u32, data: &'a [u8]) -> Head<'a> {
        peek(kind, lane, data)
    }
}

/// A runner's fan: its server's frames ([`peek_server`]).
pub struct Server;

/// A fan that hands everything to its control loop, in the order it came: a peer that reads what came as it came (the net
/// thread's own tests, tests/hostile.rs's runner, a bench's server that takes nothing), where no frame is any session
/// loop's.
pub struct InOrder;

impl Heads for InOrder {
    fn head<'a>(&self, _: crate::net::Kind, _: u32, _: &'a [u8]) -> Head<'a> {
        Head::Plain
    }

    fn sessions(&self) -> bool {
        false
    }
}

impl Heads for Server {
    fn head<'a>(&self, kind: crate::net::Kind, lane: u32, data: &'a [u8]) -> Head<'a> {
        peek_server(kind, lane, data).0
    }
}

/// `ServerMsg`'s variants for one agent session that its session loop on a runner takes, the session's id their first
/// field: a message for it, which its inbox takes once (not a command: the server sends it as it is, `HarnessSend`).
const SERVER_SESSION: &[&str] = &["HarnessSend"];

/// Its variants for one agent session whose id follows a question's `req` (a varint): what it ran, a page it serves, a
/// look at its folder. Asked of the session, answered by its runner on its own.
const SERVER_SESSION_ASKS: &[&str] = &["HarnessTaskOutput", "Preview", "Workspace"];

/// Its variants that are commands for one agent session, as a channel item's body carries them: on the session's own
/// command streams (`crate::wire::commands::session_of`), its session loop takes them; on the runner process's own (a
/// server from before those), the control loop does, and they're barriers there, ordered both ways against what the
/// session loops take.
const SERVER_SESSION_COMMANDS: &[&str] = &["HarnessOpen", "HarnessCmd", "HarnessClose", "HarnessFile", "HarnessPackage", "HarnessRelease", "StreamOpen", "StreamControl", "StreamInput", "StreamClose"];

/// Its variants that what a session loop takes is judged by, ordered both ways against it: the handshake and what it
/// says the server takes (a session's custody, its relays, its unread words), what the server kept of its sessions, a
/// session's identity at an epoch and the work kiln handed it, how many sessions the machine runs at once; and the
/// server's word of when to dial it again (its admission's turn, its drain's notice: docs/resilience.md R5).
const SERVER_BARRIERS: &[&str] = &["Welcome", "Refused", "Caps", "Kept", "Identity", "Custody", "AgentLimit", "Later", "Back"];

/// Its variants of a protocol of their own the link carries.
const SERVER_CARRIED: &[&str] = &["Sync", "Observe", "Cluster", "Convo", "Reach", "ReachRefused", "PortData", "PortAck", "PortClose"];

/// What a runner's fan reads its server's frames by ([`peek_server`]), made once from the types themselves.
struct ServerTags {
    /// `ServerMsg::Wire`'s tag, and the channel's frames' within it.
    wire: u64,
    item: u64,
    have: u64,
    ack: u64,
    want: u64,
    /// Each `ServerMsg` tag's place, by tag; a tag past them (a newer server's) is a barrier.
    of: Vec<ServerTag>,
}

/// Where a `ServerMsg` of one variant goes on a runner, and what it is for the runner's legs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerTag {
    /// For one agent session, its id the first field.
    Session,
    /// For one agent session, its id after a question's `req`.
    SessionAsk,
    /// A command for one agent session (a channel item's body).
    SessionCommand,
    /// What a session loop's frames are judged by.
    Barrier,
    /// The rest, by kind.
    Plain(crate::route_latency::InKind),
}

impl ServerTags {
    fn of(&self, tag: u64) -> ServerTag {
        usize::try_from(tag).ok().and_then(|t| self.of.get(t)).copied().unwrap_or(ServerTag::Barrier)
    }
}

/// [`ServerTags`], made at first use.
fn server_tags() -> &'static ServerTags {
    static TAGS: OnceLock<ServerTags> = OnceLock::new();
    TAGS.get_or_init(|| {
        use crate::route_latency::InKind;
        let server = variants_of::<crate::proto::ServerMsg>();
        let frames = variants_of::<super::channel::Frame>();
        let tag = |names: &[&str], name: &str| names.iter().position(|n| *n == name).and_then(|i| u64::try_from(i).ok()).unwrap_or(u64::MAX);
        let of = server
            .iter()
            .map(|name| {
                if SERVER_SESSION.contains(name) {
                    ServerTag::Session
                } else if SERVER_SESSION_ASKS.contains(name) {
                    ServerTag::SessionAsk
                } else if SERVER_SESSION_COMMANDS.contains(name) {
                    ServerTag::SessionCommand
                } else if SERVER_BARRIERS.contains(name) {
                    ServerTag::Barrier
                } else if SERVER_CARRIED.contains(name) {
                    ServerTag::Plain(InKind::Carried)
                } else {
                    ServerTag::Plain(InKind::Other)
                }
            })
            .collect();
        ServerTags { wire: tag(server, "Wire"), item: tag(frames, "Item"), have: tag(frames, "Have"), ack: tag(frames, "Ack"), want: tag(frames, "Want"), of }
    })
}

/// What a server's frame `data`, of `kind` on `lane`, is on its runner, read off its head, none of it copied: where it goes
/// ([`Head`]), and what it is for the runner's legs (`crate::route_latency::InKind`). A message for one agent session, or
/// its question about one, goes to that session's loop ([`Head::Session`]), as does a command on the session's own command
/// stream (`crate::wire::commands::session_of`); a session's command on the runner process's own streams, and what a
/// session loop's frames are judged by (the handshake, the server's caps, a session's identity, custody, the agent limit,
/// what the server has of the runner's streams), are barriers; the rest is plain. One whose head can't be read so is a
/// barrier: refused where it's read, in its order.
pub fn peek_server(kind: crate::net::Kind, lane: u32, data: &[u8]) -> (Head<'_>, crate::route_latency::InKind) {
    use crate::route_latency::InKind;
    // Bulk frames are no messages to route by: in the order they came, on the control loop.
    if crate::hub::Hub::job_log(kind, lane) {
        return (Head::Plain, InKind::Carried);
    }
    let tags = server_tags();
    let Some((tag, rest)) = varint(data) else { return (Head::Barrier, InKind::Other) };
    if tag == tags.wire {
        let Some((frame, rest)) = varint(rest) else { return (Head::Barrier, InKind::Other) };
        if frame == tags.ack || frame == tags.want {
            return (Head::Plain, InKind::Answers);
        }
        if frame == tags.have {
            return (Head::Barrier, InKind::Answers);
        }
        if frame != tags.item {
            return (Head::Barrier, InKind::Other);
        }
        let Some((stream, rest)) = text(rest) else { return (Head::Barrier, InKind::Command) };
        if let Some(session) = super::commands::session_of(stream) {
            return (Head::Session(session), InKind::Session);
        }
        // The item's body, after its `seq` and `low`: the command it carries, its tag first.
        let inner = varint(rest).and_then(|(_, r)| varint(r)).and_then(|(_, r)| varint(r)).and_then(|(_, body)| varint(body)).map(|(t, _)| tags.of(t));
        return match inner {
            Some(ServerTag::SessionCommand | ServerTag::Session | ServerTag::SessionAsk) => (Head::Barrier, InKind::Session),
            Some(ServerTag::Plain(_)) => (Head::Plain, InKind::Command),
            Some(ServerTag::Barrier) | None => (Head::Barrier, InKind::Command),
        };
    }
    match tags.of(tag) {
        ServerTag::Session => text(rest).map_or((Head::Barrier, InKind::Session), |(id, _)| (Head::Session(id), InKind::Session)),
        ServerTag::SessionAsk => varint(rest).and_then(|(_, r)| text(r)).map_or((Head::Barrier, InKind::Session), |(id, _)| (Head::Session(id), InKind::Session)),
        // Sent as it is (a server's to a runner from before the channel): in its order, both ways.
        ServerTag::SessionCommand => (Head::Barrier, InKind::Session),
        ServerTag::Barrier => (Head::Barrier, InKind::Other),
        ServerTag::Plain(kind) => (Head::Plain, kind),
    }
}

/// What an event of a runner's waits for, and what of its runner's later ones wait for it ([`Order`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waits {
    Update,
    Barrier,
    Reader,
    Plain,
}

/// An event handed to one of the hub's loops: its number in the order the net thread handed them over, and
/// what its loop waits for before it takes it.
pub struct Routed {
    pub ev: NetEvent,
    /// From 1, in the order the net thread handed its events over, whichever loop each went to.
    pub n: u64,
    /// Its runner's earlier events, elsewhere, that it may not overtake: each loop's, by the number its mark
    /// must reach first ([`Marks::wait`]). Mostly none.
    pub after: Vec<(usize, u64)>,
    /// When the net thread handed it to its loop's queue: set against a frame's arrival, the net thread's own hold
    /// of it (`crate::route_latency::RunnerLeg::Net`).
    pub handed: std::time::Instant,
}

impl crate::memory::queue::Weigh for Routed {
    /// Its event's weight, its number's and its waits' places beside it, and the waits it keeps.
    fn weight(&self) -> usize {
        crate::memory::queue::Weigh::weight(&self.ev)
            .saturating_add(size_of::<u64>())
            .saturating_add(size_of::<std::time::Instant>())
            .saturating_add(size_of::<Vec<(usize, u64)>>())
            .saturating_add(self.after.capacity().saturating_mul(size_of::<(usize, u64)>()))
    }
}

/// One runner link's events, as the net thread has numbered them so far.
struct Seen {
    /// The number of its last barrier its loop hadn't taken yet when it was last looked at (0: none): what its
    /// next session updates wait for.
    barrier: u64,
    /// The number of its last session update on each loop since its last barrier or reader (0: none): what its
    /// next one waits for. Indexed by loop, the control loop's unused (what's there is in order).
    updates: Vec<u64>,
}

/// The net thread's numbering of what it hands over, and each runner link's last events: one thread's, so no
/// lock.
struct Order {
    /// The last number given (0: none yet).
    last: u64,
    loops: usize,
    peers: HashMap<PeerId, Seen>,
}

impl Order {
    fn new(loops: usize) -> Self {
        Self { last: 0, loops, peers: HashMap::new() }
    }

    /// The next event of `peer`'s, of `waits`, going to loop `to`: its number, and what it waits for of its
    /// runner's earlier ones that `marks` doesn't say are taken already.
    fn number(&mut self, peer: PeerId, to: usize, waits: Waits, marks: &Marks) -> (u64, Vec<(usize, u64)>) {
        self.last = self.last.saturating_add(1);
        let n = self.last;
        if waits == Waits::Plain {
            return (n, Vec::new());
        }
        let loops = self.loops;
        let seen = self.peers.entry(peer).or_insert_with(|| Seen { barrier: 0, updates: vec![0; loops] });
        let mut after = Vec::new();
        match waits {
            Waits::Update => {
                // Its runner's last barrier, unless its loop is this one (in order there) or took it.
                if seen.barrier > 0 && to != CONTROL {
                    if marks.taken(CONTROL) >= seen.barrier {
                        seen.barrier = 0;
                    } else {
                        after.push((CONTROL, seen.barrier));
                    }
                }
                if let Some(u) = seen.updates.get_mut(to) {
                    *u = n;
                }
            }
            Waits::Barrier | Waits::Reader => {
                // Its runner's last update on each other loop that hasn't taken it yet. Once this has waited
                // for them, the next barrier or reader needn't: it's taken after this one.
                for (k, u) in seen.updates.iter_mut().enumerate() {
                    if *u > 0 && k != CONTROL && marks.taken(k) < *u {
                        after.push((k, *u));
                    }
                    *u = 0;
                }
                if waits == Waits::Barrier {
                    seen.barrier = n;
                }
            }
            Waits::Plain => {}
        }
        (n, after)
    }

    /// `peer`'s link is gone: nothing of it comes after this.
    fn gone(&mut self, peer: PeerId) {
        self.peers.remove(&peer);
    }
}

/// How far each of the hub's loops has taken what it was handed: the number of the last event it took (0:
/// none), each loop's written by that loop alone and read by any. Its loop's events come to it in rising
/// numbers, so a loop whose mark has reached `n` has taken every event of its own up to `n`. A wait for one
/// takes no lock: each loop's moves are told through a [`crate::rt::Changed`] (goal 5's rule: no lock two
/// shards share on a runner's path).
pub struct Marks {
    taken: Box<[AtomicU64]>,
    /// Told as each loop's mark moves.
    moved: Box<[crate::rt::Changed]>,
}

impl Marks {
    pub fn new(loops: usize) -> Self {
        Self { taken: (0..loops).map(|_| AtomicU64::new(0)).collect(), moved: (0..loops).map(|_| crate::rt::Changed::new()).collect() }
    }

    /// How far loop `k` has taken (`u64::MAX` once it ended, or for a loop there isn't).
    pub fn taken(&self, k: usize) -> u64 {
        self.taken.get(k).map_or(u64::MAX, |t| t.load(Ordering::Acquire))
    }

    /// Loop `me` took event `n`, and so every event handed it before.
    pub fn took(&self, me: usize, n: u64) {
        if let (Some(t), Some(moved)) = (self.taken.get(me), self.moved.get(me)) {
            t.fetch_max(n, Ordering::Release);
            moved.notify();
        }
    }

    /// Loop `me` ended: nothing waits for it any more.
    pub fn ended(&self, me: usize) {
        self.took(me, u64::MAX);
    }

    /// Loop `me`'s end, said as it goes, however it goes (returned, or unwound by a panic): held by the loop.
    pub fn ending(self: &Arc<Self>, me: usize) -> Ending {
        Ending { marks: Arc::clone(self), me }
    }

    /// Back once each loop `after` names has taken through its number (at once when they all have): whether it
    /// waited for one, which gives the waiting loop's shard to its other work (its loop's turn starts afresh
    /// then, as after an empty queue's wait: `crate::rt::Turns`, goal 10, plan 01a1a87b2dae step 5).
    pub async fn wait(&self, after: &[(usize, u64)]) -> bool {
        let mut waited = false;
        for &(k, n) in after {
            let (Some(taken), Some(moved)) = (self.taken.get(k), self.moved.get(k)) else { continue };
            while taken.load(Ordering::Acquire) < n {
                // Listened for first, then looked at: a move after the look is a notify after the listen.
                let moves = moved.listen();
                if taken.load(Ordering::Acquire) >= n {
                    break;
                }
                moves.await;
                waited = true;
            }
        }
        waited
    }
}

/// A loop's end ([`Marks::ending`]): when it goes, nothing waits for that loop any more.
pub struct Ending {
    marks: Arc<Marks>,
    me: usize,
}

impl Drop for Ending {
    fn drop(&mut self) {
        self.marks.ended(self.me);
    }
}

/// The hub's ends of a [`Fan`]: each loop's queue, the control loop's first ([`CONTROL`]), then each shard's
/// session loop's in shard order; and the marks they keep.
pub struct Loops {
    pub queues: Vec<crate::memory::queue::Receiver<Routed>>,
    pub marks: Arc<Marks>,
}

/// The net thread's end: what it hands over goes to the loop that takes it, numbered ([`Order`]).
pub struct Fan<H: Heads = Runners> {
    loops: Vec<crate::memory::queue::Sender<Routed>>,
    /// The room its loops' queues share ([`Fan::room`]).
    room: Arc<crate::memory::queue::Room>,
    /// Shards whose session loops take sessions' frames: a session's go to its owner's session loop.
    shards: usize,
    marks: Arc<Marks>,
    order: Order,
    /// What the frames it hands over are read by: the server's runners' frames, or a runner's server's.
    heads: H,
}

impl Fan<Runners> {
    /// The server's fan: its runners' frames ([`Fan::with`]).
    pub fn new(shards: usize, bound: usize) -> (Fan, Loops) {
        Fan::with(Runners, shards, bound)
    }
}

impl<H: Heads> Fan<H> {
    /// A fan of frames read by `heads` to the control loop and a session loop for each of `shards` shards (as many as own
    /// sessions: [`shards`]), their queues together holding `bound` bytes at most, counted as the net thread's events'
    /// (`crate::memory::owners::NET_EVENTS`): what one loop's queue held before, so the loops' queues and the net thread
    /// hold no more of what the link sent than they did (the server's: a share of its memory, `crate::memory::room`; a
    /// runner's, `crate::net::EVENTS_BYTES`). With the loops' ends.
    pub fn with(heads: H, shards: usize, bound: usize) -> (Fan<H>, Loops) {
        let shards = shards.max(1);
        let loops = if heads.sessions() { session_loop(shards) } else { session_loop(CONTROL) };
        let room = Arc::new(crate::memory::queue::Room::new(&crate::memory::owners::NET_EVENTS, bound));
        let (senders, queues): (Vec<_>, Vec<_>) = (0..loops).map(|_| crate::memory::queue::bounded_in::<Routed>(&room)).unzip();
        let marks = Arc::new(Marks::new(loops));
        (Fan { loops: senders, room, shards, marks: Arc::clone(&marks), order: Order::new(loops), heads }, Loops { queues, marks })
    }

    /// The room its loops' queues share: the server's follows its share of its memory (`crate::memory::room::follow`;
    /// join-burst, item 1).
    pub fn room(&self) -> &Arc<crate::memory::queue::Room> {
        &self.room
    }

    /// Where `ev` goes, and what it waits for.
    fn route(&self, ev: &NetEvent) -> (PeerId, usize, Waits) {
        match ev {
            NetEvent::Connected { peer, .. } | NetEvent::Closed { peer, .. } | NetEvent::Unreachable { peer, .. } => (*peer, CONTROL, Waits::Barrier),
            NetEvent::Frame { peer, kind, lane, data, .. } => match self.heads.head(*kind, *lane, data) {
                Head::Session(id) => (*peer, session_loop(crate::harness::copies::owner_of(id, self.shards)), Waits::Update),
                Head::Barrier => (*peer, CONTROL, Waits::Barrier),
                Head::Reader => (*peer, CONTROL, Waits::Reader),
                Head::Plain => (*peer, CONTROL, Waits::Plain),
            },
        }
    }
}

impl<H: Heads> crate::net::Hand for Fan<H> {
    fn hand(&mut self, ev: NetEvent) -> Result<(), Option<Handing>> {
        let (peer, to, waits) = self.route(&ev);
        let (n, after) = self.order.number(peer, to, waits, &self.marks);
        if matches!(ev, NetEvent::Closed { .. } | NetEvent::Unreachable { .. }) {
            self.order.gone(peer);
        }
        let Some(tx) = self.loops.get(to) else { return Err(None) };
        match tx.try_send(Routed { ev, n, after, handed: std::time::Instant::now() }) {
            Ok(()) => Ok(()),
            Err(async_channel::TrySendError::Full(routed)) => {
                let tx = tx.clone();
                Err(Some(Box::pin(async move { tx.send(routed).await.is_ok() })))
            }
            Err(async_channel::TrySendError::Closed(_)) => Err(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::net::{Hand as _, Kind};
    use crate::wire::channel::{Arrival, Frame, Receiver, Refusal, Seq};
    use crate::wire::link::Links;
    use crate::wire::uplink::key_of;

    fn frame(peer: PeerId, msg: &AgentMsg) -> anyhow::Result<NetEvent> {
        Ok(NetEvent::Frame { peer, lane: crate::proto::LANE_CONTROL, kind: Kind::Control, data: crate::proto::encode(msg)?, at: Instant::now() })
    }

    fn harness(id: &str) -> AgentMsg {
        AgentMsg::Harness { session: id.into(), json: "{\"type\":\"meta\"}".into() }
    }

    /// Item `seq` of session `id`'s stream, carrying its update.
    fn item(id: &str, seq: u64) -> anyhow::Result<AgentMsg> {
        Ok(AgentMsg::Wire(Frame::Item { stream: format!("s:{id}#b1.0"), seq, low: seq.saturating_sub(1), body: crate::proto::encode(&harness(id))? }))
    }

    /// Goal 10, plan 01a1a87b2dae step 4: the tags the fan reads frames' heads by are the ones postcard writes,
    /// read off the types: every `AgentMsg` variant named in one list, a session's update, a barrier, a reader
    /// or plain; and none in two.
    #[test]
    fn g10_s4_every_runner_message_is_named_once_and_its_tag_is_the_wires() -> anyhow::Result<()> {
        let agent = variants_of::<AgentMsg>();
        anyhow::ensure!(agent.len() == crate::wire::agent_classes().len(), "{} variants read off AgentMsg, {} in its classes", agent.len(), crate::wire::agent_classes().len());
        for name in agent {
            let lists = [SESSION, BARRIERS, READERS, PLAIN].iter().filter(|l| l.contains(name)).count();
            assert_eq!(lists, 1, "AgentMsg::{name} is in {lists} of the fan's lists: name it in one (crate::wire::fan)");
        }
        for list in [SESSION, BARRIERS, READERS, PLAIN] {
            for name in list {
                assert!(agent.contains(name), "the fan names AgentMsg::{name}, which isn't one");
            }
        }
        // The tags as postcard writes them: each variant's index among the names read off the type.
        let head = |m: &AgentMsg| -> anyhow::Result<u64> { varint(&crate::proto::encode(m)?).map(|(t, _)| t).ok_or_else(|| anyhow::anyhow!("no tag")) };
        let tags = tags();
        assert_eq!(head(&harness("x"))?, tags.harness);
        assert_eq!(head(&AgentMsg::Wire(Frame::Have(Vec::new())))?, tags.wire);
        let samples = [
            harness("x"),
            AgentMsg::Heartbeat { free_slots: 0, running: Vec::new() },
            AgentMsg::HarnessInventory { json: String::new() },
            AgentMsg::Caps { caps: 0 },
            AgentMsg::HarnessAdopted { session: String::new(), error: None },
            AgentMsg::Identity { json: String::new() },
            AgentMsg::Machine { id: String::new() },
        ];
        for m in &samples {
            let name = crate::wire::agent_name(m);
            let index = agent.iter().position(|n| *n == name).ok_or_else(|| anyhow::anyhow!("{name} isn't among the names read off AgentMsg"))?;
            assert_eq!(head(m)?, u64::try_from(index)?, "{name}'s tag on the wire is its index among the names");
        }
        let item = crate::proto::encode(&AgentMsg::Wire(Frame::Item { stream: String::new(), seq: 0, low: 0, body: Vec::new() }))?;
        let (_, rest) = varint(&item).ok_or_else(|| anyhow::anyhow!("no tag"))?;
        assert_eq!(varint(rest).map(|(t, _)| t), Some(tags.item));
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a frame's head says where it goes and what it waits for, read off its
    /// bytes as they came.
    #[test]
    fn g10_s4_a_frames_head_says_where_it_goes() -> anyhow::Result<()> {
        let peek_of = |m: &AgentMsg| -> anyhow::Result<String> {
            let data = crate::proto::encode(m)?;
            Ok(format!("{:?}", peek(Kind::Control, crate::proto::LANE_CONTROL, &data)))
        };
        assert_eq!(peek_of(&harness("sess-1"))?, "Session(\"sess-1\")", "a plain Harness message");
        assert_eq!(peek_of(&item("sess-2", 7)?)?, "Session(\"sess-2\")", "an item of a session's stream");
        // A session's id may carry '#': its stream's name ends at its last.
        let odd = AgentMsg::Wire(Frame::Item { stream: "s:a#b#boot.3".into(), seq: 1, low: 0, body: Vec::new() });
        assert_eq!(peek_of(&odd)?, "Session(\"a#b\")");
        let term = AgentMsg::Wire(Frame::Item { stream: "t:term1#b1.0".into(), seq: 1, low: 0, body: Vec::new() });
        assert_eq!(peek_of(&term)?, "Plain", "a terminal's output");
        assert_eq!(peek_of(&AgentMsg::Wire(Frame::Want { stream: "c:x".into(), from: 2 }))?, "Plain", "a want of the server's commands");
        assert_eq!(peek_of(&AgentMsg::Caps { caps: 1 })?, "Barrier");
        assert_eq!(peek_of(&AgentMsg::HarnessInventory { json: "{}".into() })?, "Barrier");
        assert_eq!(peek_of(&AgentMsg::HarnessHandOver { session: "s".into(), offset: 0, data: vec![1], done: true })?, "Barrier");
        assert_eq!(peek_of(&AgentMsg::Identity { json: "{}".into() })?, "Reader");
        assert_eq!(peek_of(&AgentMsg::Heartbeat { free_slots: 1, running: Vec::new() })?, "Plain");
        assert_eq!(peek_of(&AgentMsg::TermOutput { session: "t".into(), data: vec![0; 64] })?, "Plain");
        // A job's output in bulk, cut short, empty, or a newer runner's message: never a session's.
        assert_eq!(peek(Kind::Log, crate::proto::LANE_LOGS, &[0xff; 8]), Head::Plain);
        let data = crate::proto::encode(&harness("sess-1"))?;
        assert_eq!(peek(Kind::Control, 0, data.get(..3).unwrap_or_default()), Head::Barrier, "a session's id cut short");
        // An item goes by the stream its head names, however the rest of it reads: refused on that stream's loop.
        let mut data = crate::proto::encode(&item("sess-2", 7)?)?;
        assert_eq!(peek(Kind::Control, 0, data.get(..data.len().saturating_sub(1)).unwrap_or_default()), Head::Session("sess-2"), "an item cut short in its body");
        data.push(0);
        assert_eq!(peek(Kind::Control, 0, &data), Head::Session("sess-2"), "an item with bytes past its end");
        assert_eq!(peek(Kind::Control, 0, &[]), Head::Barrier);
        assert_eq!(peek(Kind::Control, 0, &[0x7f]), Head::Barrier, "a tag past this build's");
        Ok(())
    }

    /// runner-fan: a fan that hands everything to its control loop (`InOrder`: a peer that reads what came as it came) has
    /// that loop alone, so no queue made for nothing is dropped (the last receiver of any queue in the room closes it for
    /// all), and whatever comes goes there, in order.
    #[test]
    fn runner_fan_an_in_order_fan_has_its_control_loop_alone() -> anyhow::Result<()> {
        let (mut fan, loops) = Fan::with(InOrder, 4, 1 << 20);
        anyhow::ensure!(loops.queues.len() == 1, "{} queues", loops.queues.len());
        for n in 0..3u64 {
            anyhow::ensure!(fan.hand(to_runner(&session_message("s", n))?).is_ok(), "handed");
        }
        let rx = loops.queues.first().ok_or_else(|| anyhow::anyhow!("no control loop"))?;
        let got: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok()).map(|r| r.n).collect();
        anyhow::ensure!(got == vec![1, 2, 3], "{got:?}");
        Ok(())
    }

    /// runner-fan: the lists a runner's fan reads its server's frames by name `ServerMsg`'s variants as they are, each in one
    /// list at most, and the tags they're read by are the ones postcard writes.
    #[test]
    fn runner_fan_the_servers_messages_are_named_once_and_read_by_their_wire_tags() -> anyhow::Result<()> {
        use crate::proto::ServerMsg;
        let server = variants_of::<ServerMsg>();
        anyhow::ensure!(server.len() == crate::wire::server_classes().len(), "{} variants read off ServerMsg, {} in its classes", server.len(), crate::wire::server_classes().len());
        let lists = [SERVER_SESSION, SERVER_SESSION_ASKS, SERVER_SESSION_COMMANDS, SERVER_BARRIERS, SERVER_CARRIED];
        for name in server {
            let n = lists.iter().filter(|l| l.contains(name)).count();
            anyhow::ensure!(n <= 1, "ServerMsg::{name} is in {n} of a runner fan's lists");
        }
        for list in lists {
            for name in list {
                anyhow::ensure!(server.contains(name), "a runner's fan names ServerMsg::{name}, which isn't one");
            }
        }
        let head = |m: &ServerMsg| -> anyhow::Result<u64> { varint(&crate::proto::encode(m)?).map(|(t, _)| t).ok_or_else(|| anyhow::anyhow!("no tag")) };
        let tags = server_tags();
        anyhow::ensure!(head(&ServerMsg::Wire(Frame::Have(Vec::new())))? == tags.wire);
        for (m, want) in [(ServerMsg::HarnessSend { session: "s".into(), epoch: 1, json: "{}".into() }, ServerTag::Session), (ServerMsg::Caps { caps: 0 }, ServerTag::Barrier), (ServerMsg::Drain, ServerTag::Plain(crate::route_latency::InKind::Other))] {
            anyhow::ensure!(tags.of(head(&m)?) == want, "{}: {:?}", crate::wire::server_name(&m), tags.of(head(&m)?));
        }
        Ok(())
    }

    /// runner-fan: a server's frame's head says on its runner where it goes and what it is: a message to an agent session,
    /// or a question about one, to its session's loop; a command on a session's own command stream too; a session's command
    /// on the runner process's own streams, and what a session loop's frames are judged by, are barriers; the rest is plain.
    #[test]
    fn runner_fan_a_servers_frames_head_says_where_it_goes_on_its_runner() -> anyhow::Result<()> {
        use crate::proto::ServerMsg;
        use crate::route_latency::InKind;
        let of = |m: &ServerMsg| -> anyhow::Result<String> {
            let data = crate::proto::encode(m)?;
            let (head, kind) = peek_server(Kind::Control, crate::proto::LANE_TERM, &data);
            Ok(format!("{head:?} {}", kind.name()))
        };
        let item = |stream: &str, inner: &ServerMsg| -> anyhow::Result<ServerMsg> { Ok(ServerMsg::Wire(Frame::Item { stream: stream.into(), seq: 3, low: 2, body: crate::proto::encode(inner)? })) };
        let cmd = ServerMsg::HarnessCmd { session: "sess-1".into(), json: "{\"type\":\"interrupt\"}".into() };
        anyhow::ensure!(of(&ServerMsg::HarnessSend { session: "sess-1".into(), epoch: 2, json: "{}".into() })? == "Session(\"sess-1\") session", "a message to a session");
        anyhow::ensure!(of(&ServerMsg::Workspace { req: 300, session: "sess-2".into(), json: "{}".into() })? == "Session(\"sess-2\") session", "a question about one");
        anyhow::ensure!(of(&item("l:7.1/sess-3", &cmd)?)? == "Session(\"sess-3\") session", "a command on a session's own stream goes by the stream");
        anyhow::ensure!(of(&item("l:7.1", &cmd)?)? == "Barrier session", "a session's command on the process's own stream");
        anyhow::ensure!(of(&item("p:7.1", &ServerMsg::Cancel { job: "j".into(), reason: "r".into() })?)? == "Plain command", "another command");
        anyhow::ensure!(of(&ServerMsg::Wire(Frame::Ack(crate::wire::channel::Has { stream: "s:x#b.0".into(), upto: 4, nacks: Vec::new() })))? == "Plain answers");
        anyhow::ensure!(of(&ServerMsg::Wire(Frame::Have(Vec::new())))? == "Barrier answers", "what the server has, as a link comes up");
        anyhow::ensure!(of(&ServerMsg::Caps { caps: 1 })? == "Barrier other");
        anyhow::ensure!(of(&ServerMsg::Cluster { json: "{}".into(), raft: vec![1, 2] })? == "Plain carried");
        anyhow::ensure!(of(&ServerMsg::Drain)? == "Plain other");
        // Cut short, empty, or a newer server's message: a barrier.
        let data = crate::proto::encode(&ServerMsg::HarnessSend { session: "sess-1".into(), epoch: 2, json: "{}".into() })?;
        anyhow::ensure!(peek_server(Kind::Control, 0, data.get(..3).unwrap_or_default()).0 == Head::Barrier);
        anyhow::ensure!(peek_server(Kind::Control, 0, &[]).0 == Head::Barrier);
        anyhow::ensure!(peek_server(Kind::Control, 0, &[0x7f]).0 == Head::Barrier);
        let kept: InKind = peek_server(Kind::Control, 0, &[0x7f]).1;
        anyhow::ensure!(kept == InKind::Other);
        Ok(())
    }

    /// A session whose copy shard `shard` of `shards` owns, by its id.
    fn owned_by(shard: usize, shards: usize, n: usize) -> String {
        (0u64..).map(|i| format!("sess-{shard}-{n}-{i}")).find(|id| crate::harness::copies::owner_of(id, shards) == shard).unwrap_or_default()
    }

    /// What each loop took, in the order taken: (loop, peer, what, began, ended).
    type Taken = Arc<std::sync::Mutex<Vec<(usize, PeerId, String, Instant, Instant)>>>;

    /// A runner's event as [`take_all`] names it: its runner, and its head and kind (`Session("s1")/Wire`).
    type Named = (PeerId, String);

    /// `events` handed through a fan of `shards` session loops and the control loop, as the net thread hands
    /// them; each loop taking its events as the hub's do (what each waits for first, then its own number marked
    /// taken), a session loop taking `slow` for each (an owner that's busy). A session loop holds each event
    /// named `hold.0` until one named `hold.1` is taken (10 s at most): what's to be taken beside it, not after
    /// it, then is, whatever the machine's load. Back once all are taken: what was taken, and by which loop.
    fn take_all(shards: usize, events: Vec<NetEvent>, slow: Duration, hold: Option<(Named, Named)>) -> anyhow::Result<Vec<(usize, PeerId, String, Instant, Instant)>> {
        take_all_with(Runners, runners_frame, shards, events, slow, hold)
    }

    /// A runner's frame as [`take_all`] names it: its head, then its message's name.
    fn runners_frame(kind: Kind, lane: u32, data: &[u8]) -> String {
        format!("{:?}/{}", peek(kind, lane, data), crate::proto::read_agent(data).map(|m| crate::wire::agent_name(&m)).unwrap_or("?"))
    }

    /// A server's frame on its runner as [`take_all_with`] names it (runner-fan): its head there, then its message's name.
    fn servers_frame(kind: Kind, lane: u32, data: &[u8]) -> String {
        format!("{:?}/{}", peek_server(kind, lane, data).0, crate::proto::read_server(data).map(|m| crate::wire::server_name(&m)).unwrap_or("?"))
    }

    /// [`take_all`] through a fan of `heads` (either end's), each frame named by `name`.
    fn take_all_with<H: Heads>(heads: H, name: fn(Kind, u32, &[u8]) -> String, shards: usize, events: Vec<NetEvent>, slow: Duration, hold: Option<(Named, Named)>) -> anyhow::Result<Vec<(usize, PeerId, String, Instant, Instant)>> {
        let count = events.len();
        crate::rt::block_on(async move {
            let (mut fan, loops) = Fan::with(heads, shards, 1 << 20);
            let taken: Taken = Arc::default();
            let Loops { queues, marks } = loops;
            for (me, rx) in queues.into_iter().enumerate() {
                let (marks, taken, hold) = (Arc::clone(&marks), Arc::clone(&taken), hold.clone());
                crate::rt::spawn(async move {
                    while let Ok(Routed { ev, n, after, .. }) = rx.recv().await {
                        marks.wait(&after).await;
                        let began = Instant::now();
                        let (peer, what) = match &ev {
                            NetEvent::Frame { peer, data, kind, lane, .. } => (*peer, name(*kind, *lane, data)),
                            NetEvent::Connected { peer, .. } => (*peer, "Connected".to_owned()),
                            NetEvent::Closed { peer, .. } | NetEvent::Unreachable { peer, .. } => (*peer, "Closed".to_owned()),
                        };
                        if me != CONTROL {
                            if let Some(((hp, hw), (up, uw))) = &hold
                                && *hp == peer
                                && *hw == what
                            {
                                // The bound only ends a hang (a frame the held one waits behind never comes).
                                let until = Instant::now() + Duration::from_secs(300);
                                loop {
                                    let seen = taken.lock().map(|t| t.iter().any(|x| x.1 == *up && x.2 == *uw)).unwrap_or(false);
                                    if seen || Instant::now() >= until {
                                        break;
                                    }
                                    crate::rt::sleep(Duration::from_millis(2)).await;
                                }
                            }
                            crate::rt::sleep(slow).await;
                        }
                        if let Ok(mut t) = taken.lock() {
                            t.push((me, peer, what, began, Instant::now()));
                        }
                        marks.took(me, n);
                    }
                });
            }
            for ev in events {
                // Nothing here is full: a queue of a megabyte.
                anyhow::ensure!(fan.hand(ev).is_ok(), "the fan refused an event");
            }
            // However long a loaded machine's loops take over them (30 s for all ran out under load, runner-fan's run of
            // 2026-11-06), the wait gives up only once they take none for a hang's bound.
            let stall = Duration::from_secs(300);
            let (mut moved, mut still_since) = (0, Instant::now());
            loop {
                let done = taken.lock().map(|t| t.len()).unwrap_or(0);
                if done >= count {
                    break;
                }
                if done > moved {
                    (moved, still_since) = (done, Instant::now());
                }
                anyhow::ensure!(still_since.elapsed() < stall, "{done} of {count} events taken, then nothing for {stall:?}");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            let out = taken.lock().map(|t| t.clone()).unwrap_or_default();
            Ok(out)
        })
    }

    /// Where `what` of `peer` was taken, the first one so.
    fn at<'a>(taken: &'a [(usize, PeerId, String, Instant, Instant)], peer: PeerId, what: &str) -> anyhow::Result<&'a (usize, PeerId, String, Instant, Instant)> {
        taken.iter().find(|t| t.1 == peer && t.2 == what).ok_or_else(|| anyhow::anyhow!("{what} of {peer} wasn't taken: {:?}", taken.iter().map(|t| (t.0, t.1, t.2.clone())).collect::<Vec<_>>()))
    }

    /// Goal 10, plan 01a1a87b2dae step 4: an Open, then the runner's inventory. The inventory is a barrier on the
    /// control loop; it's taken only once the session's loop has taken the Open (its owner applied it), however
    /// long that loop is busy, so the inventory reads where the session runs after it.
    #[test]
    fn g10_s4_an_open_then_the_runners_inventory_is_read_after_the_open() -> anyhow::Result<()> {
        let shards = 2;
        let s = owned_by(1, shards, 0);
        let open = AgentMsg::Wire(Frame::Item { stream: format!("s:{s}#b1.0"), seq: 1, low: 0, body: crate::proto::encode(&harness(&s))? });
        let events = vec![
            NetEvent::Connected { peer: 7, name: Some("box".into()), binding: None },
            frame(7, &AgentMsg::Caps { caps: crate::proto::CAP_WIRE })?,
            frame(7, &open)?,
            frame(7, &AgentMsg::HarnessInventory { json: "{\"sessions\":[]}".into() })?,
        ];
        let taken = take_all(shards, events, Duration::from_millis(200), None)?;
        let opened = at(&taken, 7, &format!("Session({s:?})/Wire"))?;
        let inventory = at(&taken, 7, "Barrier/HarnessInventory")?;
        assert_eq!(opened.0, session_loop(1), "the Open on its owner's session loop");
        assert_eq!(inventory.0, CONTROL);
        assert!(inventory.3 >= opened.4, "the inventory was taken before its runner's Open was");
        // And the Open waited for the Caps before it.
        assert!(opened.3 >= at(&taken, 7, "Barrier/Caps")?.4, "the Open overtook its runner's Caps");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a hand-over mid-stream. The older process's updates before its package
    /// are taken before the package goes on, and those after it wait for it; the newer process's updates wait for
    /// its word that it adopted the session. Another runner's wait for none of them: one is taken while the older
    /// process's first update is still being taken (held until it is).
    #[test]
    fn g10_s4_a_hand_over_mid_stream_keeps_each_process_in_order() -> anyhow::Result<()> {
        let shards = 3;
        let (s, other) = (owned_by(1, shards, 0), owned_by(2, shards, 1));
        let piece = AgentMsg::HarnessHandOver { session: s.clone(), offset: 0, data: vec![1; 16], done: true };
        let events = vec![
            frame(1, &item(&s, 1)?)?,
            frame(1, &item(&other, 1)?)?,
            frame(1, &piece)?,
            frame(1, &item(&s, 2)?)?,
            frame(2, &AgentMsg::HarnessAdopted { session: s.clone(), error: None })?,
            frame(2, &item(&s, 1)?)?,
            frame(3, &item(&other, 1)?)?,
        ];
        let hold = ((1, format!("Session({s:?})/Wire")), (3, format!("Session({other:?})/Wire")));
        let taken = take_all(shards, events, Duration::from_millis(50), Some(hold))?;
        let ups = |peer: PeerId, id: &str| taken.iter().filter(|t| t.1 == peer && t.2 == format!("Session({id:?})/Wire")).collect::<Vec<_>>();
        let handed = at(&taken, 1, "Barrier/HarnessHandOver")?;
        let (older_s, older_other) = (ups(1, &s), ups(1, &other));
        anyhow::ensure!(older_s.len() == 2 && older_other.len() == 1, "the older process's updates: {taken:?}", taken = taken.iter().map(|t| (t.0, t.1, t.2.clone())).collect::<Vec<_>>());
        let (first, after) = (older_s.first().copied(), older_s.get(1).copied());
        let (Some(first), Some(after), Some(o)) = (first, after, older_other.first().copied()) else { anyhow::bail!("updates missing") };
        assert!(handed.3 >= first.4 && handed.3 >= o.4, "the package went on before the older process's updates before it were taken");
        assert!(after.3 >= handed.4, "an update after the package overtook it");
        let adopted = at(&taken, 2, "Barrier/HarnessAdopted")?;
        let newer = ups(2, &s);
        assert!(newer.iter().all(|u| u.3 >= adopted.4), "the newer process's update overtook its word that it adopted the session");
        // Another runner's update waited for none of them: taken beside the first runner's, not after.
        let third = ups(3, &other);
        let Some(third) = third.first() else { anyhow::bail!("the third runner's update wasn't taken") };
        assert!(third.4 <= first.4 && third.3 < handed.3, "another runner's update waited for this runner's");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a runner's link closing as its update is being taken. The close waits
    /// for its update to be taken, so the sessions it says are disconnected are as their last update left them,
    /// never brought back by an update taken after. What's plain (a heartbeat) is taken while the update still is
    /// (held until it is), waiting for none.
    #[test]
    fn g10_s4_a_close_racing_an_update_waits_for_it_and_plain_frames_do_not() -> anyhow::Result<()> {
        let shards = 2;
        let s = owned_by(0, shards, 0);
        let sample = AgentMsg::Heartbeat { free_slots: 1, running: Vec::new() };
        let events = vec![frame(4, &item(&s, 1)?)?, frame(4, &sample)?, NetEvent::Closed { peer: 4, reason: "gone".into() }];
        let hold = ((4, format!("Session({s:?})/Wire")), (4, "Plain/Heartbeat".to_owned()));
        let taken = take_all(shards, events, Duration::from_millis(50), Some(hold))?;
        let update = at(&taken, 4, &format!("Session({s:?})/Wire"))?;
        let heartbeat = at(&taken, 4, "Plain/Heartbeat")?;
        let closed = at(&taken, 4, "Closed")?;
        assert!(closed.3 >= update.4, "the close was taken before its runner's update");
        assert!(heartbeat.4 <= update.4, "a heartbeat waited for an update");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 4: a session's items go to one loop, in order, each once; a runner's
    /// sessions on different shards are taken side by side, not one after another: one's first item is taken
    /// while the other's first still is (held until it is).
    #[test]
    fn g10_s4_a_streams_items_go_to_one_loop_in_order_and_sessions_side_by_side() -> anyhow::Result<()> {
        let shards = 2;
        let (a, b) = (owned_by(0, shards, 0), owned_by(1, shards, 1));
        let mut events = Vec::new();
        for seq in 1..=4 {
            events.push(frame(9, &item(&a, seq)?)?);
            events.push(frame(9, &item(&b, seq)?)?);
        }
        let hold = ((9, format!("Session({a:?})/Wire")), (9, format!("Session({b:?})/Wire")));
        let taken = take_all(shards, events, Duration::from_millis(20), Some(hold))?;
        for (id, shard) in [(&a, 0), (&b, 1)] {
            let mine: Vec<_> = taken.iter().filter(|t| t.2 == format!("Session({id:?})/Wire")).collect();
            assert_eq!(mine.len(), 4, "{id}'s items, each taken once");
            assert!(mine.iter().all(|t| t.0 == session_loop(shard)), "{id}'s items on its owner's loop");
            assert!(mine.windows(2).all(|w| w.first().zip(w.get(1)).is_some_and(|(x, y)| y.3 >= x.4)), "{id}'s items in order");
        }
        let first_of = |id: &str| taken.iter().find(|t| t.2 == format!("Session({id:?})/Wire")).map(|t| (t.3, t.4));
        let (Some((_, a_ended)), Some((_, b_ended))) = (first_of(&a), first_of(&b)) else { anyhow::bail!("nothing taken") };
        assert!(b_ended <= a_ended, "{b}'s first item waited for {a}'s");
        Ok(())
    }

    /// SplitMix64, as wire::channel's simulation draws: every schedule replayed from its seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn one_in(&mut self, n: u64) -> bool {
            self.next().is_multiple_of(n.max(1))
        }
    }

    /// Goal 10, plan 01a1a87b2dae step 4: random runners' events through a fan, each loop taking its own as long
    /// as a random draw says. Every event is taken once; no barrier or reader is taken before its runner's session
    /// updates handed over before it; no update before its runner's barriers before it; each session's updates in
    /// their order; and none waits for good (the oldest can always go).
    #[test]
    fn g10_s4_random_schedules_keep_every_runners_order() -> anyhow::Result<()> {
        for seed in 0..12u64 {
            let mut rng = Rng(seed);
            let shards = 1 + usize::try_from(rng.below(3)).unwrap_or(0);
            let sessions: Vec<String> = (0..4).map(|i| format!("sess-{seed}-{i}")).collect();
            let mut events = Vec::new();
            let mut seqs: HashMap<(PeerId, usize), u64> = HashMap::new();
            for _ in 0..40 {
                let peer = 1 + rng.below(3);
                let ev = match rng.below(6) {
                    0 => frame(peer, &AgentMsg::Caps { caps: 0 })?,
                    1 => frame(peer, &AgentMsg::Identity { json: "{}".into() })?,
                    2 => frame(peer, &AgentMsg::Sample(Default::default()))?,
                    _ => {
                        let i = usize::try_from(rng.below(4)).unwrap_or(0);
                        let seq = seqs.entry((peer, i)).or_insert(0);
                        *seq += 1;
                        frame(peer, &item(sessions.get(i).map_or("x", String::as_str), *seq)?)?
                    }
                };
                events.push(ev);
            }
            // What each event was, in the order handed over.
            let handed: Vec<(PeerId, String)> = events
                .iter()
                .map(|ev| match ev {
                    NetEvent::Frame { peer, data, kind, lane, .. } => (*peer, format!("{:?}", peek(*kind, *lane, data))),
                    _ => (0, String::new()),
                })
                .collect();
            let taken = take_all(shards, events, Duration::from_millis(1 + rng.below(4)), None)?;
            anyhow::ensure!(taken.len() == handed.len(), "seed {seed}: {} of {} taken", taken.len(), handed.len());
            // Each taken event matched to its place in what was handed, in each (peer, what)'s order.
            let mut place: HashMap<(PeerId, String), Vec<usize>> = HashMap::new();
            for (i, h) in handed.iter().enumerate() {
                place.entry(h.clone()).or_default().push(i);
            }
            let mut when: Vec<Option<(Instant, Instant)>> = vec![None; handed.len()];
            let mut next: HashMap<(PeerId, String), usize> = HashMap::new();
            let mut by_start: Vec<_> = taken.iter().collect();
            by_start.sort_by_key(|t| t.3);
            for t in by_start {
                let key = (t.1, t.2.split('/').next().unwrap_or("").to_owned());
                let k = next.entry(key.clone()).or_insert(0);
                let i = place.get(&key).and_then(|p| p.get(*k)).copied().ok_or_else(|| anyhow::anyhow!("seed {seed}: {key:?} taken more often than handed"))?;
                *k += 1;
                if let Some(w) = when.get_mut(i) {
                    *w = Some((t.3, t.4));
                }
            }
            for (i, (peer, what)) in handed.iter().enumerate() {
                let Some((began, _)) = when.get(i).copied().flatten() else { anyhow::bail!("seed {seed}: event {i} never taken") };
                for (j, (p, w)) in handed.iter().enumerate().take(i) {
                    if p != peer {
                        continue;
                    }
                    let Some((_, ended)) = when.get(j).copied().flatten() else { continue };
                    let update = |w: &str| w.starts_with("Session");
                    let ordered = (update(w) && (what == "Barrier" || what == "Reader")) || (w == "Barrier" && update(what)) || (update(w) && w == what);
                    assert!(!ordered || began >= ended, "seed {seed}: {peer}'s {what} (event {i}) was taken before its {w} (event {j}) was");
                }
            }
        }
        Ok(())
    }

    /// A server's frame for its runner, on the agent session lane, as the server sends it.
    fn to_runner(msg: &crate::proto::ServerMsg) -> anyhow::Result<NetEvent> {
        Ok(NetEvent::Frame { peer: crate::net::SERVER_PEER, lane: crate::proto::LANE_TERM, kind: Kind::Term, data: crate::proto::encode(msg)?, at: Instant::now() })
    }

    /// A command for session `id`, item `seq` of its own live command stream.
    fn session_command(id: &str, seq: u64) -> anyhow::Result<crate::proto::ServerMsg> {
        let cmd = crate::proto::ServerMsg::HarnessCmd { session: id.into(), json: "{\"type\":\"interrupt\"}".into() };
        Ok(crate::proto::ServerMsg::Wire(Frame::Item { stream: format!("l:7.1/{id}"), seq, low: seq.saturating_sub(1), body: crate::proto::encode(&cmd)? }))
    }

    /// A message for session `id`, as the server sends it (its inbox takes it once).
    fn session_message(id: &str, n: u64) -> crate::proto::ServerMsg {
        crate::proto::ServerMsg::HarnessSend { session: id.into(), epoch: 1, json: format!("{{\"type\":\"send\",\"id\":\"m{n}\",\"text\":\"x\"}}") }
    }

    /// runner-fan: on a runner, a session's message and a command on its own command stream reach the loop on the shard
    /// that owns it without waiting behind another session's: session `a`'s loop held on its message until session `b`'s
    /// command is taken (10 s at most), `b`'s message and command are taken on their own loop while `a`'s first frame still
    /// is, and each session's frames in their order on its own loop.
    #[test]
    fn runner_fan_a_sessions_frames_reach_its_shard_without_waiting_behind_anothers() -> anyhow::Result<()> {
        let shards = 2;
        let (a, b) = (owned_by(0, shards, 0), owned_by(1, shards, 1));
        let events = vec![to_runner(&session_message(&a, 1))?, to_runner(&session_command(&a, 1)?)?, to_runner(&session_message(&b, 1))?, to_runner(&session_command(&b, 1)?)?];
        let peer = crate::net::SERVER_PEER;
        let hold = ((peer, format!("Session({a:?})/HarnessSend")), (peer, format!("Session({b:?})/Wire")));
        let taken = take_all_with(Server, servers_frame, shards, events, Duration::from_millis(20), Some(hold))?;
        let of = |id: &str| taken.iter().filter(|t| t.2.starts_with(&format!("Session({id:?})"))).collect::<Vec<_>>();
        let (fa, fb) = (of(&a), of(&b));
        anyhow::ensure!(fa.len() == 2 && fb.len() == 2, "each frame taken once: {:?}", taken.iter().map(|t| (t.0, t.2.clone())).collect::<Vec<_>>());
        anyhow::ensure!(fa.iter().all(|t| t.0 == session_loop(0)) && fb.iter().all(|t| t.0 == session_loop(1)), "each session's on its owner's loop");
        let a_first = fa.first().map(|t| t.4).ok_or_else(|| anyhow::anyhow!("a's first"))?;
        anyhow::ensure!(fb.iter().all(|t| t.4 <= a_first), "b's frames waited for a's message");
        anyhow::ensure!(fa.windows(2).all(|w| w.first().zip(w.get(1)).is_some_and(|(x, y)| y.3 >= x.4)), "a's in order");
        Ok(())
    }

    /// runner-fan: random schedules of a server's frames through a runner's fan, each loop taking its own as long as a random
    /// draw says. Every frame is taken once; no barrier (a session's command on the process's own stream, the server's caps)
    /// before the session frames handed over before it; no session frame before the barriers before it; each session's
    /// frames (its messages, the commands on its own stream) in their order.
    #[test]
    fn runner_fan_random_schedules_keep_the_servers_order_on_its_runner() -> anyhow::Result<()> {
        use crate::proto::ServerMsg;
        for seed in 0..12u64 {
            let mut rng = Rng(seed);
            let shards = 1 + usize::try_from(rng.below(3)).unwrap_or(0);
            let sessions: Vec<String> = (0..4).map(|i| format!("sess-{seed}-{i}")).collect();
            let mut seqs: HashMap<usize, u64> = HashMap::new();
            let mut events = Vec::new();
            for n in 0..40u64 {
                let i = usize::try_from(rng.below(4)).unwrap_or(0);
                let id = sessions.get(i).map_or("x", String::as_str);
                let m = match rng.below(7) {
                    0 => ServerMsg::Caps { caps: 0 },
                    1 => ServerMsg::Cluster { json: "{}".into(), raft: vec![1] },
                    2 => ServerMsg::Wire(Frame::Item { stream: "l:7.1".into(), seq: n + 1, low: n, body: crate::proto::encode(&ServerMsg::HarnessClose { session: id.into() })? }),
                    3 => ServerMsg::Wire(Frame::Item { stream: "p:7.1".into(), seq: n + 1, low: n, body: crate::proto::encode(&ServerMsg::Cancel { job: "j".into(), reason: "r".into() })? }),
                    4 => {
                        let seq = seqs.entry(i).or_insert(0);
                        *seq += 1;
                        session_command(id, *seq)?
                    }
                    _ => session_message(id, n),
                };
                events.push(to_runner(&m)?);
            }
            let handed: Vec<String> = events.iter().map(|ev| if let NetEvent::Frame { data, kind, lane, .. } = ev { format!("{:?}", peek_server(*kind, *lane, data).0) } else { String::new() }).collect();
            let taken = take_all_with(Server, servers_frame, shards, events, Duration::from_millis(1 + rng.below(4)), None)?;
            anyhow::ensure!(taken.len() == handed.len(), "seed {seed}: {} of {} taken", taken.len(), handed.len());
            let mut place: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, h) in handed.iter().enumerate() {
                place.entry(h.clone()).or_default().push(i);
            }
            let mut when: Vec<Option<(Instant, Instant)>> = vec![None; handed.len()];
            let mut next: HashMap<String, usize> = HashMap::new();
            let mut by_start: Vec<_> = taken.iter().collect();
            by_start.sort_by_key(|t| t.3);
            for t in by_start {
                let key = t.2.split('/').next().unwrap_or("").to_owned();
                let k = next.entry(key.clone()).or_insert(0);
                let i = place.get(&key).and_then(|p| p.get(*k)).copied().ok_or_else(|| anyhow::anyhow!("seed {seed}: {key} taken more often than handed"))?;
                *k += 1;
                if let Some(w) = when.get_mut(i) {
                    *w = Some((t.3, t.4));
                }
            }
            for (i, what) in handed.iter().enumerate() {
                let Some((began, _)) = when.get(i).copied().flatten() else { anyhow::bail!("seed {seed}: frame {i} never taken") };
                for (j, w) in handed.iter().enumerate().take(i) {
                    let Some((_, ended)) = when.get(j).copied().flatten() else { continue };
                    let session = |h: &str| h.starts_with("Session");
                    let ordered = (session(w) && what == "Barrier") || (w == "Barrier" && session(what)) || (session(w) && w == what);
                    anyhow::ensure!(!ordered || began >= ended, "seed {seed}: {what} (frame {i}) was taken before {w} (frame {j}) was");
                }
            }
        }
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae steps 4 and 5: an order wait says whether it waited, as its loop's turn starts
    /// afresh after one that did (the wait gave its shard to its other work, as an empty queue's does): not for
    /// nothing to wait for, nor for marks already reached; yes for a mark it found unreached and that's reached
    /// since.
    #[test]
    fn g10_s4_an_order_wait_says_whether_it_waited() {
        use std::future::Future as _;
        use std::task::{Context, Poll};
        let marks = Marks::new(3);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(std::pin::pin!(marks.wait(&[])).poll(&mut cx), Poll::Ready(false), "nothing to wait for");
        marks.took(1, 5);
        assert_eq!(std::pin::pin!(marks.wait(&[(1, 3), (1, 5)])).poll(&mut cx), Poll::Ready(false), "marks reached already");
        let after = [(1, 5), (2, 7)];
        let mut wait = std::pin::pin!(marks.wait(&after));
        assert!(wait.as_mut().poll(&mut cx).is_pending(), "loop 2 hasn't taken event 7 yet");
        marks.took(2, 7);
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(true), "it waited for it");
        // A loop that ended, however it ended (its end dropped as it went), is waited for no more.
        let ends = Arc::new(Marks::new(1));
        let mut gone = std::pin::pin!(ends.wait(&[(0, 9)]));
        assert!(gone.as_mut().poll(&mut cx).is_pending());
        drop(ends.ending(0));
        assert_eq!(gone.as_mut().poll(&mut cx), Poll::Ready(true), "its end let it go");
    }

    /// Goal 10, plan 01a1a87b2dae step 4, the order waits on no lock (goal 5's rule): their code names none, nor
    /// event-listener, whose list of listeners sits behind one.
    #[test]
    fn g10_s4_the_order_waits_take_no_lock() {
        let code = include_str!("fan.rs");
        let code = code.split("#[cfg(test)]").next().unwrap_or_default();
        for lock in ["Mutex", "RwLock", "Condvar", "parking_lot", "event_listener", "async_lock"] {
            assert!(!code.contains(lock), "wire::fan's code names {lock}");
        }
    }

    /// Goal 10, plan 01a1a87b2dae step 4, with wire/link.rs as it is: a runner's streams split between the hub's
    /// loops as the fan splits them (each session's to its owner's session loop, the rest to the control loop),
    /// each loop with a `Links` of its own taking its streams on a thread of its own, say again what one loop's
    /// receivers that took them all would say: every stream in its order, with what's answered and refused of it,
    /// its key's older streams let go of (`Hub::wire_have`'s merge of the loops' own).
    #[test]
    fn g10_s4_streams_split_between_loops_say_what_one_loop_would() -> anyhow::Result<()> {
        let now = Instant::now();
        let shards = 3;
        // A loop each: the control loop's, then each shard's session loop's.
        let links: Arc<Vec<Links>> = Arc::new((0..session_loop(shards)).map(|_| Links::new()).collect());
        let loop_of = |stream: &str| crate::wire::uplink::session_of(stream).map_or(CONTROL, |id| session_loop(crate::harness::copies::owner_of(id, shards)));
        let keys = ["s:a", "s:b", "s:c", "s:d", "s:e", "t:t1", "t:t2"];
        // Each key's items: its first stream, then from item 5 a new generation (the old let go of), some refused.
        let steps = |key: &str| -> Vec<(String, Seq, Option<(Refusal, String)>)> {
            (1..=9u64)
                .map(|n| {
                    let (stream, seq) = if n <= 4 { (format!("{key}#b1.0"), n) } else { (format!("{key}#b1.1"), n - 4) };
                    let refusal = n.is_multiple_of(3).then(|| (Refusal::Refused, format!("no {n}")));
                    (stream, seq, refusal)
                })
                .collect()
        };
        let mut handles = Vec::new();
        for me in 0..session_loop(shards) {
            let mine: Vec<&str> = keys.iter().copied().filter(|k| loop_of(&format!("{k}#x")) == me).collect();
            let all: Vec<Vec<(String, Seq, Option<(Refusal, String)>)>> = mine.iter().map(|k| steps(k)).collect();
            let links = Arc::clone(&links);
            handles.push(std::thread::spawn(move || {
                let Some(own) = links.get(me) else { return };
                // The keys' items interleaved, each key's in its order.
                for i in 0..9 {
                    for key_steps in &all {
                        let Some((stream, seq, refusal)) = key_steps.get(i).cloned() else { continue };
                        if matches!(own.on_item("a", &stream, seq, seq.saturating_sub(1), vec![1], now), Arrival::Take(_)) {
                            let _ = own.answer("a", &stream, seq, refusal);
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("a loop's thread panicked"))?;
        }
        // One loop's receivers, taking them all.
        let mut one = Receiver::new();
        for key in keys {
            for (stream, seq, refusal) in steps(key) {
                if seq == 1
                    && let Some(k) = key_of(&stream)
                {
                    let prefix = format!("{k}#");
                    one.forget_where(|s| s != stream && s.starts_with(&prefix));
                }
                if matches!(one.on_item(&stream, seq, seq.saturating_sub(1), vec![1], now), Arrival::Take(_)) {
                    let _ = match refusal {
                        None => one.took(&stream, seq),
                        Some((code, why)) => one.refused(&stream, seq, code, why),
                    };
                }
            }
        }
        let said = have_of(links.iter(), "a");
        let (Frame::Have(said), Frame::Have(has)) = (said, one.have()) else { anyhow::bail!("not a Have") };
        assert_eq!(said, has, "what the loops say they have is what one loop's receivers would");
        assert_eq!(said.len(), keys.len(), "each key's newest stream, its older one let go of");
        let Frame::Have(none) = have_of(links.iter(), "b") else { anyhow::bail!("not a Have") };
        assert!(none.is_empty(), "another runner's are its own");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 4: wire::channel's schedules (frames dropped, copied, delayed and reordered
    /// below the channel; the link cut and resumed; the runner's sender restarting from what it kept) with the
    /// server's end as the hub has it now, wire/link.rs as it is. A runner's streams are split between loops as
    /// the fan splits them, each loop with a `Links` of its own; the loops take what reached them in whatever order
    /// they get to it, some of it after the link it came on was cut (their answers lost with it); and what's said
    /// as a link comes up is the loops' together (`Hub::wire_have`). Every item is applied once, in order, and
    /// answered once, as it was.
    #[test]
    fn g10_s4_nothing_is_lost_or_doubled_with_streams_split_between_loops() {
        use std::collections::{BTreeMap, VecDeque};

        use crate::wire::channel::{Event, Outcome, Sender};

        // Two sessions on one loop, one on another, a terminal on the control loop.
        const STREAMS: [(&str, usize); 4] = [("s:a#b1.0", 1), ("s:b#b1.0", 1), ("s:d#b1.0", 2), ("t:c#b1.0", 0)];
        let loop_of = |stream: &str| STREAMS.iter().find(|(s, _)| *s == stream).map_or(0, |(_, l)| *l);
        let refuses = |body: &[u8]| body.first().is_some_and(|b| b % 7 == 0);

        // What went wrong in one schedule of `count` items a stream, if anything.
        let check = |seed: u64, count: u64| -> Option<String> {
            let mut rng = Rng(seed);
            let base = Instant::now();
            let (room, patience) = (64, Duration::from_millis(400));
            let mut s = Sender::new(room, patience);
            let links: Vec<Links> = (0..3).map(|_| Links::new()).collect();
            let mut queues: Vec<VecDeque<(String, Seq, Seq, Vec<u8>)>> = (0..3).map(|_| VecDeque::new()).collect();
            let mut kept: BTreeMap<String, BTreeMap<Seq, Vec<u8>>> = BTreeMap::new();
            let mut next: BTreeMap<String, Seq> = BTreeMap::new();
            let mut pushed: BTreeMap<String, u64> = BTreeMap::new();
            let mut bodies: BTreeMap<(String, Seq), Vec<u8>> = BTreeMap::new();
            let mut applied: BTreeMap<String, Vec<Seq>> = BTreeMap::new();
            let mut answers: BTreeMap<(String, Seq), Vec<Outcome>> = BTreeMap::new();
            let (mut to_r, mut to_s): (Vec<(u64, Frame)>, Vec<(u64, Frame)>) = (Vec::new(), vec![(0, have_of(links.iter(), "r"))]);
            let mut up = true;
            let faulty_until = 1_200;
            for tick in 0..40_000u64 {
                let now = base + Duration::from_millis(tick * 10);
                let faults = tick < faulty_until;
                for (stream, _) in STREAMS {
                    let n = pushed.entry(stream.to_owned()).or_default();
                    if *n < count && rng.one_in(30) {
                        let body = vec![u8::try_from(rng.below(256)).unwrap_or(0)];
                        if let Ok(seq) = s.push(stream, body.clone()) {
                            *n += 1;
                            bodies.insert((stream.to_owned(), seq), body.clone());
                            kept.entry(stream.to_owned()).or_default().insert(seq, body);
                            next.insert(stream.to_owned(), seq + 1);
                        }
                    }
                }
                // The link cut, then up again a while after: the server says what its loops have.
                if faults && up && rng.one_in(150) {
                    up = false;
                    to_r.clear();
                    to_s.clear();
                    s.down();
                } else if !up && rng.one_in(8) {
                    up = true;
                    to_s.push((tick, have_of(links.iter(), "r")));
                }
                // The runner starts again from what its durable streams kept.
                if faults && rng.one_in(900) {
                    let mut again = Sender::new(room, patience);
                    for (stream, _) in STREAMS {
                        let pending: Vec<(Seq, Vec<u8>)> = kept.get(stream).map(|m| m.iter().map(|(k, v)| (*k, v.clone())).collect()).unwrap_or_default();
                        again.restore(stream, next.get(stream).copied().unwrap_or(1), pending);
                    }
                    s = again;
                    if up {
                        to_s.push((tick, have_of(links.iter(), "r")));
                    }
                }
                if up {
                    for f in s.poll(now) {
                        if faults && rng.one_in(12) {
                            continue;
                        }
                        if faults && rng.one_in(20) {
                            to_r.push((tick + rng.below(6), f.clone()));
                        }
                        to_r.push((tick + if faults { rng.below(6) } else { 0 }, f));
                    }
                }
                // What's due reaches the loop its stream is: in its queue, taken as that loop gets to it.
                let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut to_r).into_iter().partition(|(at, _)| *at <= tick);
                to_r = later;
                for (_, f) in due {
                    if let Frame::Item { stream, seq, low, body } = f
                        && let Some(queue) = queues.get_mut(loop_of(&stream))
                    {
                        queue.push_back((stream, seq, low, body));
                    }
                }
                // The loops, in an order of the draw's, each taking as many as it gets to with its own links.
                let first = usize::try_from(rng.below(3)).unwrap_or(0);
                for k in (0..3).map(|i| (first + i) % 3) {
                    let take = if faults { rng.below(4) } else { u64::MAX };
                    let (Some(own), Some(queue)) = (links.get(k), queues.get_mut(k)) else { continue };
                    for _ in 0..take {
                        let Some((stream, seq, low, body)) = queue.pop_front() else { break };
                        let answer = match own.on_item("r", &stream, seq, low, body.clone(), now) {
                            Arrival::Take(_) => {
                                applied.entry(stream.clone()).or_default().push(seq);
                                let refusal = refuses(&body).then(|| (Refusal::Refused, "seven".to_owned()));
                                Some(own.answer("r", &stream, seq, refusal))
                            }
                            Arrival::Answered(a) => a,
                            Arrival::Gap(w) => Some(w),
                        };
                        if let Some(a) = answer
                            && up
                            && !(faults && rng.one_in(12))
                        {
                            to_s.push((tick + if faults { rng.below(6) } else { 0 }, a));
                        }
                    }
                }
                let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut to_s).into_iter().partition(|(at, _)| *at <= tick);
                to_s = later;
                for (_, f) in due {
                    for e in s.on_frame(f) {
                        match e {
                            Event::Answered { stream, seq, outcome } => {
                                if let Some(m) = kept.get_mut(&stream) {
                                    m.remove(&seq);
                                }
                                answers.entry((stream, seq)).or_default().push(outcome);
                            }
                            Event::Regressed { stream, .. } => return Some(format!("seed {seed}: {stream} regressed: the loops said less than they had answered")),
                        }
                    }
                }
                let all = STREAMS.iter().all(|(st, _)| pushed.get(*st).copied().unwrap_or(0) >= count);
                if !faults && all && s.unanswered() == 0 {
                    break;
                }
            }
            for (stream, _) in STREAMS {
                let got = applied.get(stream).cloned().unwrap_or_default();
                if got != (1..=count).collect::<Vec<_>>() {
                    return Some(format!("seed {seed}: {stream} applied {got:?}, want 1..={count} once each in order"));
                }
                for seq in 1..=count {
                    let key = (stream.to_owned(), seq);
                    let refused = bodies.get(&key).is_some_and(|b| refuses(b));
                    match answers.get(&key).map(Vec::as_slice) {
                        Some([Outcome::Taken]) if !refused => {}
                        Some([Outcome::Refused { .. }]) if refused => {}
                        Some(a) => return Some(format!("seed {seed}: {stream}#{seq} answered {a:?} (refused: {refused})")),
                        None => return Some(format!("seed {seed}: {stream}#{seq} never answered")),
                    }
                }
            }
            None
        };
        let failed: Vec<String> = (0..2_000u64).filter_map(|seed| check(seed, 12)).take(5).collect();
        assert!(failed.is_empty(), "replay a schedule with its seed:\n{}", failed.join("\n"));
        assert_eq!(check(7, 200), None, "a long schedule");
    }
}
