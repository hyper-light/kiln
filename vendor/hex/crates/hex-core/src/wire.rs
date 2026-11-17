//! The node agent's local socket as bytes (decision 3 of docs/design/runtime-architecture.md): the
//! `Runtime` seam's requests and answers as messages of a Unix `SOCK_SEQPACKET` socket, and back.
//! Pure: the shells, `hex-node`'s server and `hex-client`, move the messages and the descriptors that
//! ride with them (exec's stdio, by `SCM_RIGHTS`); this decides what each message says, and refuses by
//! type every one that says nothing valid.
//!
//! A message is one `SOCK_SEQPACKET` record, so the kernel keeps its bounds (unix(7)): a 22-byte
//! header, then the body its kind defines.
//!
//! | bytes | field |
//! |---|---|
//! | 0..4 | `HEXW` |
//! | 4..12 | the sender's wire identity ([`IDENTITY`]) |
//! | 12..14 | kind: a request below `0x8000`, an answer from it; [`SEALED`] marks a sealed body |
//! | 14..22 | id: a request's, chosen by its client; its answer, and an exec'd program's end, carry it |
//!
//! A message that carries a secret ([`carries_secret`]) goes sealed on a connection that seals ([`seal`], [`open`]: a
//! node's over QUIC, docs/design/hex-quic.md Q13): its header, its kind marked, then its nonce, its body sealed and its
//! tag, the header bound to the body.
//!
//! Integers are little-endian. A string or a path is a `u32` length and its bytes, an option a byte
//! (0 none, 1 some) before the value, a list a `u32` count before its items, and an enum a byte naming
//! its variant before its fields.
//!
//! One wire: no versions. Both sides are built from the same wire definitions or they don't talk. Every message
//! carries its sender's wire identity, a digest of this module's source and [`crate::stream`]'s (the kinds and
//! their bodies, and the frames a stream carries them in) made at compile time, so a change to either is a new
//! identity with no number to bump. A reader refuses a message of another identity before it reads its body, so
//! nothing of another wire is misread (§3). The one layout every build keeps is a hello's head: the header, then
//! its sender's build as a string ([`build`]), first in a hello and first in its answer. So a peer built from
//! another wire is refused at the hello, typed, naming both identities and both builds ([`Fault::Wire`]): the agent
//! answers a message of another identity, its hello first, with its own hello and ends the connection, and the client
//! refuses an answer of another identity. A reader also refuses bytes past what a kind's body holds rather than
//! skip them, so nothing is dropped silently.
//!
//! The client says it took a given answer's ends ([`Request::Taken`], not answered, its header's id the exec's)
//! before anything it sends after the answer; the node agent keeps its own copies of them until then, or until the
//! connection closes, so no end is ever in flight with no other reference to it: XNU's collector of descriptors in
//! flight flushes a socket whose only reference is a message in flight, and it arrives reading nothing (on macOS
//! 26.4, 719 of 20,000 sent so arrived dead with the sender's copy closed at once, none with it kept until
//! received). Kept ends hold their program's place among the programs until taken ([`Limits::programs`],
//! [`Budgets::programs`]).

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hex_api::{
    Allocation, BackendUnavailable, BackingGrant, BackingKey, BackingOffer, BackingPhase, Bounds,
    BoxId, BoxInfo, BoxOwner, BoxSpec, BoxState, Capacity, CapacityResources, Class, ClassSpec,
    ConnectFailure, ConnectPhase, ConnectReply, ControlWake, CredentialGrant, Destination,
    DeviceHolder, DeviceId, DeviceInfo, DeviceKind, DeviceUnavailable, Execution, Exit,
    GpuEligibility, Grant, GuestEnd, GuestFailure, GuestTime, ImageArch, ImageMember, ImageOs,
    ImagePlatform, ImageVolume, IoKind, IoRates, Learned, LearnedAwait, LearnedCeiling, LearnedEnd,
    LearnedHolder, LearnedHolding, LearnedOverhead, LearnedPrior, LearnedRank, LearnedRate,
    LearnedReservations, LearnedUnacted, LearnedUnread, LearnedUnrecorded, LearnedUse,
    LearnedWindow, LifecycleCause, MAX_DEVICES, MAX_LEARNED_HOLDINGS, MAX_LEARNED_PRIORS,
    MAX_RUNTIME_ROUTES, Member, MemberKind, Mount, Network, NodeVmPhase, NodeVmReceipt, OciSpec,
    PhysicalDevice, PhysicalReport, Ports, Program, Proto, Published, RateHeld, Receiver, Refusal,
    Rootfs, RouteKey, RouteOffer, RouteReceipt, RuntimeCost, ShardsImage, Signal, StartedFrom,
    Upstream, Usage, User, VolumeCause, check_credentials, check_grants, check_published,
    check_sign_ins,
};

use hex_api::lease::{Lease, LeaseRefused, SignInRefused, SignInTokens, Value};

use crate::netns;

/// Every message's first bytes.
pub const MAGIC: [u8; 4] = *b"HEXW";
/// This build's wire identity: FNV-1a (64-bit) of the wire definitions' source, this module's and
/// [`crate::stream`]'s, made at compile time, carriage returns skipped so a checkout's line endings can't split it.
/// Two builds talk only when theirs are the same.
// Its evaluation visits each byte of its sources once, so their length bounds it. rustc's long_running_const_eval lint
// takes an evaluation that long for one that may never end: on a CRLF checkout (git's core.autocrlf, on by default on
// Windows) the loop visits every carriage return it skips too, and at main a8f6bfe4 jupiter's 673,005 bytes of
// sources tripped it, so hex-core didn't build there; LF checkouts are close behind. The lint's own note names this
// remedy for an evaluation long by design: allowing it here. A static, evaluated once, here, where the allow is: a
// const is evaluated again in each crate that uses it, beyond this allow's reach (at be251865 hex-client's and
// hex-runtime's builds tripped the lint on jupiter so).
#[allow(long_running_const_eval)]
// Its sources are every file that defines a type the wire writes or reads (the review's D-11, 2026-11-14: each name in
// this module's `use` lists, found where it's defined): netns.rs's reason bound and counts, and hex-api's leases and
// sign-ins, among them.
pub static IDENTITY: u64 = identity(&[
    include_bytes!("wire.rs"),
    include_bytes!("stream.rs"),
    include_bytes!("schedule.rs"),
    include_bytes!("netns.rs"),
    include_bytes!("../../hex-sim/src/peer.rs"),
    include_bytes!("../../hex-sim/src/model.rs"),
    include_bytes!("../../hex-api/src/spec.rs"),
    include_bytes!("../../hex-api/src/id.rs"),
    include_bytes!("../../hex-api/src/bounds.rs"),
    include_bytes!("../../hex-api/src/devices.rs"),
    include_bytes!("../../hex-api/src/runtime.rs"),
    include_bytes!("../../hex-api/src/grants.rs"),
    include_bytes!("../../hex-api/src/credentials.rs"),
    include_bytes!("../../hex-api/src/lease.rs"),
]);
/// The header's length: magic, wire identity, kind and id.
pub const HEADER_BYTES: usize = 22;
/// Largest message either side sends or reads. It sits under the kernel's own bound on a
/// `SOCK_SEQPACKET` record, the sender's send buffer less 32 bytes (net/unix/af_unix.c,
/// `unix_dgram_sendmsg`), which is 212,960 bytes under the default 212,992-byte buffer
/// (`net.core.wmem_default`): measured on adacachy, Linux 7.2.9, 2026-10-27. 128 KiB holds any box spec
/// or program a caller passes in practice (an id is at most 128 bytes, a path 4 KiB); a larger one is
/// refused by type before it is sent. The in-process seam has no such bound: a program whose arguments
/// and environment pass it runs only there.
pub const MAX_MESSAGE: usize = 128 << 10;
/// Most descriptors one message carries: exec's stdin, stdout and stderr.
pub const MAX_FDS: usize = 3;

/// FNV-1a (64-bit) of `sources` in turn, every carriage return skipped: the offset basis and prime are FNV-1a's own
/// (IETF draft-eastlake-fnv, "FNV Basics" and "FNV Constants"). A digest, not a seal: it tells two wires apart, and
/// FNV-1a because it is a few lines of `const fn`, where a cryptographic hash isn't.
const fn identity(sources: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut sources = sources;
    while let Some((source, more)) = sources.split_first() {
        let mut bytes = *source;
        while let Some((&byte, rest)) = bytes.split_first() {
            if byte != b'\r' {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(PRIME);
            }
            bytes = rest;
        }
        sources = more;
    }
    hash
}

/// This build as a hello names it: its crate's version and the commit it was built from (`HEX_COMMIT` at build
/// time, which kiln CI sets), else "unknown commit".
pub fn build() -> String {
    format!(
        "hex {} ({})",
        env!("CARGO_PKG_VERSION"),
        option_env!("HEX_COMMIT").unwrap_or("unknown commit")
    )
}

/// The kinds. Requests sit below `0x8000`, answers from it.
mod kind {
    pub const HELLO: u16 = 0x0001;
    pub const CREATE: u16 = 0x0002;
    pub const START: u16 = 0x0003;
    pub const EXEC: u16 = 0x0004;
    pub const SIGNAL: u16 = 0x0005;
    pub const INSPECT: u16 = 0x0006;
    pub const USAGE: u16 = 0x0007;
    pub const LIST: u16 = 0x0008;
    pub const DELETE: u16 = 0x0009;
    pub const RELEASE: u16 = 0x000a;
    pub const HELLO_ANSWER: u16 = 0x8001;
    pub const INFO: u16 = 0x8002;
    pub const DONE: u16 = 0x8003;
    pub const STARTED: u16 = 0x8004;
    pub const USAGE_ANSWER: u16 = 0x8005;
    pub const BOXES: u16 = 0x8006;
    pub const REFUSED: u16 = 0x8007;
    pub const EXITED: u16 = 0x8008;
    pub const NETNS: u16 = 0x000b;
    pub const NETNS_ANSWER: u16 = 0x8009;
    pub const KILL: u16 = 0x000c;
    pub const EXEC_STREAMS: u16 = 0x000d;
    pub const GIVEN: u16 = 0x800a;
    pub const TAKEN: u16 = 0x000e;
    pub const RUN: u16 = 0x000f;
    pub const STREAMS_READY: u16 = 0x0010;
    pub const SCHEDULE: u16 = 0x0011;
    pub const SCHEDULED: u16 = 0x800c;
    pub const OFFERED: u16 = 0x800b;
    pub const CAPACITY: u16 = 0x0012;
    pub const CAPACITY_ANSWER: u16 = 0x800d;
    pub const PHYSICAL_REPORT: u16 = 0x0013;
    pub const PHYSICAL_REPORT_ANSWER: u16 = 0x800e;
    pub const INITIALIZE_BACKING: u16 = 0x0014;
    pub const RELEASE_BACKING: u16 = 0x0015;
    pub const BACKING_OFFER: u16 = 0x800f;
    pub const BACKING_USAGE: u16 = 0x0016;
    pub const MEMBER: u16 = 0x0017;
    pub const LINKS: u16 = 0x0018;
    pub const LINKS_ANSWER: u16 = 0x8010;
    pub const LESSORS: u16 = 0x0019;
    pub const LESSOR: u16 = 0x001a;
    pub const LEASED: u16 = 0x001b;
    pub const LESSOR_ASK: u16 = 0x8011;
    pub const LEASE_ASK: u16 = 0x8012;
    pub const LEASE_GIVEN: u16 = 0x8013;
    pub const LESSOR_ENDED: u16 = 0x8014;
    pub const CAPTURED: u16 = 0x001c;
    pub const REFRESHED: u16 = 0x001d;
    pub const CAPTURE_ASK: u16 = 0x8015;
    pub const REFRESH_ASK: u16 = 0x8016;
    pub const SIGN_IN_REVOKED: u16 = 0x8017;
    pub const MEMBER_SIGNAL: u16 = 0x001e;
    pub const START_FOLLOWED: u16 = 0x001f;
    pub const RESERVED: u16 = 0x0020;
    pub const MCP: u16 = 0x0021;
}

/// A message's header, once its wire identity is read as this side's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u16,
    pub id: u64,
}

/// What a client sends.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// A connection's first message, naming the client's build ([`build`]): answered with the node agent's build,
    /// what it runs and its bounds.
    Hello { build: String },
    /// A call of the seam, answered once.
    Call(Call),
    /// The client no longer waits for the end of the program its header's id started (its `Proc` was
    /// dropped): the node agent stops watching it and lets it run on, as dropping an in-process `Proc`
    /// does. Not answered.
    Release,
    /// The client took the ends of the streams its header's id's exec was given ([`Answer::Given`]), so the node
    /// agent lets its own copies go. Not answered.
    Taken,
    /// The caller opened every connection in an Offered answer. The original exec may now start.
    /// Not answered separately; the exec's Started/Refused and Exited keep its original header id.
    StreamsReady,
    /// The orchestrator's answer to the node's [`Answer::LessorAsk`] its header's id names (docs/design/credential-grants.md
    /// C7): the run's lessor made, or why not. Not answered.
    Lessor(Result<(), Refusal>),
    /// The orchestrator's answer to the node's [`Answer::LeaseAsk`] its header's id names, of run `run`: the lease, its
    /// value wiped as it drops and never printed ([`hex_api::lease::Value`]), or why not, of Shards' kinds. Not answered.
    Leased {
        run: u64,
        answer: Result<Lease, LeaseRefused>,
    },
    /// The orchestrator's answer to the node's [`Answer::CaptureAsk`] its header's id names, of run `run`
    /// (docs/design/credential-grants.md C8): the placeholders the sign-in's CLI is to hold and its access token's
    /// reference, or why not, of Shards' kinds. Not answered.
    Captured {
        run: u64,
        answer: Result<hex_api::Captured, SignInRefused>,
    },
    /// The orchestrator's answer to the node's [`Answer::RefreshAsk`] its header's id names, of run `run` (C8): the token
    /// response the sign-in's CLI is to have, a value wiped as it drops, or why not. Not answered.
    Refreshed {
        run: u64,
        answer: Result<Value, SignInRefused>,
    },
}

/// A call of the node agent: the `Runtime` seam's, its network namespace counts, and a program's kill.
#[derive(Debug, PartialEq, Eq)]
pub enum Call {
    /// Scheduler control, using the same bounded connection and reader as Runtime calls.
    Schedule(Box<crate::schedule::Command>),
    Create(Box<BoxSpec>),
    Start(BoxId),
    /// `program` in box `id`, with the streams `streams` names as the message's descriptors, in order.
    Exec {
        id: BoxId,
        program: Program,
        streams: Streams,
    },
    Signal {
        id: BoxId,
        signal: Signal,
    },
    Inspect(BoxId),
    Usage(BoxId),
    List,
    Delete(BoxId),
    /// The node agent's network namespace counts ([`netns::Counts`]).
    Netns,
    /// KILL to the program exec `exec` of this connection started (`Proc::kill`), and only it.
    /// Answered `Done` once sent, or when that program has ended already.
    Kill {
        exec: u64,
    },
    /// An exec, as [`Call::Exec`], that also asks the node to make the streams `ask` names
    /// (`Stdio::ask`); answered [`Answer::Given`] with the caller's ends of them. A stream both carried
    /// and asked for is refused.
    ExecStreams {
        id: BoxId,
        program: Program,
        streams: Streams,
        ask: Streams,
    },
    /// `program` in a `process` box made from `spec` for its life (`Runtime::run`, decision 7 of
    /// docs/design/runtime-architecture.md): a create's body, then an exec's program, the streams it carries as the
    /// message's descriptors, in order, and the streams it asks the node to make, as [`Call::ExecStreams`]'s. Answered
    /// as an exec is, [`Answer::Started`] or [`Answer::Given`], and later [`Answer::Exited`], which comes once the
    /// box is gone.
    Run {
        spec: Box<BoxSpec>,
        program: Program,
        streams: Streams,
        ask: Streams,
    },
    Capacity(Class),
    /// AC-T7.4.1: factual native observations from the existing Store/turn owner, never grants.
    PhysicalReport,
    /// AC-T7.4.1: one fallibly allocated payload, moved uniquely before native effects.
    InitializeBacking(triomphe::Arc<BackingGrant>),
    ReleaseBacking(BackingKey),
    /// Factual host counters from the same native backing owner, answered with Usage.
    BackingUsage(BackingKey),
    /// Member `member` of running Shards image box `id` that its spec says its caller drives (`Runtime::member`;
    /// joint plan step 17): the streams `streams` names travel with it as an exec's do, each joined to the member's
    /// own, and those `ask` names the node gives as an exec's asked streams, the member's own ends ([`Answer::Given`]).
    /// Answered as an exec is, [`Answer::Started`] (no pid: the member is its guest's) or [`Answer::Given`], and later
    /// [`Answer::Exited`] with its end. A stream both carried and asked for is refused.
    Member {
        id: BoxId,
        member: Member,
        streams: Streams,
        ask: Streams,
    },
    /// The node's links over QUIC (docs/design/hex-quic.md Q12): answered [`Answer::Links`], refused by type where the
    /// node serves no QUIC listener.
    Links,
    /// This connection's orchestrator leases the credentials of the Shards image VMs it creates from now on
    /// (docs/design/credential-grants.md C7): the node asks it over this connection ([`Answer::LessorAsk`],
    /// [`Answer::LeaseAsk`]). Answered [`Answer::Done`], or refused by type where the node leases through no connection.
    Lessors,
    /// `signal` to member `member` of running Shards image box `id` alone (`Runtime::signal_member`; goal 2's item 2),
    /// on its own lane of its run's agent-control endpoint: answered [`Answer::Done`], or refused by type. Appended.
    MemberSignal {
        id: BoxId,
        member: Member,
        signal: Signal,
    },
    /// Box `id` once its start has come to its end (`Runtime::started`): a box still booting followed to its boot's
    /// answer on the node, a wait on the fact (decision 9), so a remote caller of a start answered Pending never looks;
    /// answered [`Answer::Info`], or refused by type. Appended (ROOT, 2026-11-14).
    Started(BoxId),
    /// The machine's capacity view's word to its node (`Runtime::reserved`; the review's A-10): what every holder has
    /// reserved through its keeper, which the node's charge learner takes off its fleet's budget; answered
    /// [`Answer::Done`], or refused by type. Appended (ROOT, 2026-11-14).
    Reserved(hex_api::Reserved),
    /// The next connection member `member`'s harness makes to its MCP bridge in running Shards image box `id`
    /// (`Runtime::mcp`; goal 2's item 3, D254), handed over as [`Call::Member`] hands its member: the streams `streams`
    /// names travel with it, each joined to the connection's own, and those `ask` names the node gives with its answer.
    /// Answered once the harness connects, as an exec is ([`Answer::Started`] or [`Answer::Given`]), then
    /// [`Answer::Exited`] with its end; or refused by type. Appended (ROOT, 2026-11-14).
    Mcp {
        id: BoxId,
        member: Member,
        streams: Streams,
        ask: Streams,
    },
}

/// Which of a program's standard streams travel with its exec, in this order; the rest are the null
/// device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Streams {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl Streams {
    /// How many descriptors travel with the exec.
    pub fn count(self) -> usize {
        [self.stdin, self.stdout, self.stderr]
            .into_iter()
            .filter(|s| *s)
            .count()
    }

    fn bits(self) -> u8 {
        [(self.stdin, 0b1), (self.stdout, 0b10), (self.stderr, 0b100)]
            .into_iter()
            .filter(|(on, _)| *on)
            .fold(0, |bits, (_, bit)| bits | bit)
    }

    fn of(bits: u8) -> Result<Self, Fault> {
        if bits & !0b111 != 0 {
            return Err(Fault::value(
                "streams",
                format!("{bits:#04x} names more than stdin, stdout and stderr"),
            ));
        }
        Ok(Self {
            stdin: bits & 1 != 0,
            stdout: bits & 0b10 != 0,
            stderr: bits & 0b100 != 0,
        })
    }
}

/// What the node agent sends.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Scheduled(Box<crate::schedule::Reply>),
    Hello(Hello),
    /// create's and inspect's.
    Info(BoxInfo),
    /// start's, signal's and delete's.
    Done,
    /// exec's: the program runs, with this process id; its end comes later as [`Answer::Exited`].
    Started {
        pid: u32,
    },
    Usage(Usage),
    /// list's: its boxes, in as many messages as they need ([`boxes`]), all with the list's id, each
    /// but the last with `more`.
    Boxes {
        infos: Vec<BoxInfo>,
        more: bool,
    },
    Refused(Refusal),
    /// The program the header's id started has ended. Sent unasked, after that exec's answer.
    Exited(Exit),
    /// netns's.
    Netns(netns::Counts),
    /// An exec that asked for streams ([`Call::ExecStreams`]), or whose program can't take the
    /// pipes it carried (a guest's), runs, with this process id; the caller's ends of the streams `given` names
    /// ride as the message's descriptors, in order. Its end comes later as [`Answer::Exited`].
    Given {
        pid: u32,
        given: Streams,
    },
    /// HEX-RT-17: open these guest connections in the caller before acknowledging StreamsReady.
    /// A preparation, not a started program; no descriptor or cross-process socket reservation rides with it.
    Offered(StreamOffer),
    Capacity(Option<Capacity>),
    PhysicalReport(PhysicalReport),
    BackingOffer(BackingOffer),
    /// links': each peer connected to the node's QUIC endpoint, in as many messages as they need ([`links`]), all
    /// with the call's id, each but the last with `more`.
    Links {
        links: Vec<Link>,
        more: bool,
    },
    /// A run of box `0`, which this connection created granting a credential, boots: its orchestrator is asked for the
    /// run's lessor (docs/design/credential-grants.md C7, `Lessors::lessor`). The header's id names the run in what
    /// follows. Sent unasked; answered [`Request::Lessor`].
    LessorAsk(BoxId),
    /// Run `run`'s lease of credential `reference`, under the run's `key` where it binds one (`Lessor::lease`). Sent
    /// unasked, the header's id the ask's; answered [`Request::Leased`].
    LeaseAsk {
        run: u64,
        reference: String,
        key: Option<[u8; 32]>,
    },
    /// The run its header's id names gave lease `0` back (`Lessor::release`). Sent unasked; not answered.
    LeaseGiven(String),
    /// The run its header's id names has ended: its lessor is let go. Sent unasked; not answered.
    LessorEnded,
    /// Run `run`'s capture of sign-in `sign_in`'s tokens, under the run's `key` where it binds one (`Lessor::capture`;
    /// docs/design/credential-grants.md C8), each token a value wiped as it drops. Sent unasked, the header's id the ask's;
    /// answered [`Request::Captured`].
    CaptureAsk {
        run: u64,
        sign_in: String,
        tokens: SignInTokens,
        key: Option<[u8; 32]>,
    },
    /// Run `run`'s refresh of sign-in `sign_in` (`Lessor::refresh`). Sent unasked, the header's id the ask's; answered
    /// [`Request::Refreshed`].
    RefreshAsk {
        run: u64,
        sign_in: String,
        key: Option<[u8; 32]>,
    },
    /// Run `run`'s VM revoked sign-in `sign_in` (`Lessor::revoked`). Sent unasked; not answered.
    SignInRevoked {
        run: u64,
        sign_in: String,
        key: Option<[u8; 32]>,
    },
}

/// A peer's connection to the node over QUIC, as the node's own TLS negotiated it (docs/design/hex-quic.md Q12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The name the peer's certificate carries, which kiln's CA bound at its enrollment.
    pub peer: String,
    /// The TLS 1.3 cipher suite, its value in IANA's TLS Cipher Suites registry (RFC 8446 §B.4).
    pub suite: u16,
    /// The key exchange group, its value in IANA's TLS Supported Groups registry (RFC 8446 §4.2.7), where the
    /// handshake named one.
    pub group: Option<u16>,
}

/// A Shards guest's identity and the names of an exec's caller-owned streams (HEX-RT-17).
/// The node holds a dedicated control connection until the offer is committed or cancelled, so dropping an
/// uncommitted offer removes only its own early streams and cannot end another caller's program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOffer {
    pub pid: u32,
    pub started: u64,
    pub vsock: PathBuf,
    pub token: u64,
    pub exec: u64,
    pub streams: Streams,
    /// The guest link's own bound on a connection's setup (`hex_client::guest::Link::setup_bound`): where the caller
    /// gave no deadline, how long each of a stream's CONNECTs waits for its OK, Shards' borrowed connector waiting only
    /// within a finite deadline; past it the CONNECT is made again while the VM lives, never a bound that refuses
    /// (decision 9). None where the link has none. Last (AC-T7.4.1).
    pub setup: Option<std::time::Duration>,
}

/// The answer to a hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The node agent's build ([`build`]): first in the answer, as the client's is in its hello.
    pub build: String,
    pub classes: Vec<Class>,
    pub limits: Limits,
    /// The node's budgets on all its connections together.
    pub budgets: Budgets,
}

/// The node agent's budgets on all its connections together: past either, a call or an exec is refused
/// by type, whichever connection it comes on, but for what each connection's limits keep for it ([`Limits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// Requests asked and not yet answered, on every connection together.
    pub in_flight: usize,
    /// Programs execs started that the connections still watch, or whose given ends their clients haven't yet said
    /// they took ([`Request::Taken`]), together.
    pub programs: usize,
}

/// The node agent's bounds on its socket, each refused past by type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Connections open at once.
    pub connections: usize,
    /// Bytes in one message, at most [`MAX_MESSAGE`].
    pub message_bytes: usize,
    /// Requests one connection may have asked and not yet had answered: the node keeps this many for each
    /// connection it admits, so one within it is never refused.
    pub in_flight: usize,
    /// Programs one connection's execs started that it may still watch, or whose given ends its client hasn't yet
    /// said it took ([`Request::Taken`]): kept for each connection likewise.
    pub programs: usize,
}

/// Why bytes aren't a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    #[error("it ends at byte {have}, short of the {need} it needs")]
    Truncated { need: usize, have: usize },
    #[error("{bytes} bytes is over the {bound}-byte bound on one message")]
    Oversized { bytes: usize, bound: usize },
    #[error("it doesn't start with HEXW")]
    Magic,
    /// A version the other protocols this codec carries don't speak: the seeds', the macOS box helper's, a stdio
    /// connection's name (the node socket's wire has none: [`Fault::Wire`]).
    #[error("version {got}: this side speaks 1 to {speaks}")]
    Version { got: u16, speaks: u16 },
    /// A message of another wire: its sender's identity, and its build where the message is a hello or a hello's
    /// answer, whose head every build lays out alike.
    #[error("{}", other_wire(*theirs, build.as_deref()))]
    Wire { theirs: u64, build: Option<String> },
    /// A kind the node socket's wire doesn't define.
    #[error("kind {kind:#06x} isn't {expected} this wire defines")]
    Undefined { kind: u16, expected: &'static str },
    /// Bytes past what the node socket's wire defines for a message's kind.
    #[error("{bytes} bytes follow what this wire defines for kind {kind:#06x}")]
    Leftover { bytes: usize, kind: u16 },
    /// A kind a message's version doesn't define, in the other protocols this codec carries: the seeds', the macOS
    /// box helper's.
    #[error("kind {kind:#06x} isn't {expected} version {version} defines")]
    Kind {
        kind: u16,
        version: u16,
        expected: &'static str,
    },
    /// Bytes past what a message's version defines for its kind, in the other protocols this codec carries.
    #[error("{bytes} bytes follow what version {version} defines for kind {kind:#06x}")]
    Trailing {
        bytes: usize,
        kind: u16,
        version: u16,
    },
    #[error("{what} isn't valid: {why}")]
    Value { what: &'static str, why: String },
    #[error("it carries {got} descriptors where {what} takes {want}")]
    Descriptors {
        what: &'static str,
        want: usize,
        got: usize,
    },
    #[error("it carries more descriptors than the {MAX_FDS} a message takes")]
    DescriptorsCut,
    #[error("{what} storage for {bound} bytes is unavailable")]
    Storage { what: &'static str, bound: usize },
    /// Its seal ([`seal`], [`open`]): docs/design/hex-quic.md Q13.
    #[error("its seal: {0}")]
    Seal(SealFault),
}

/// Why a message wasn't sealed or opened, typed (docs/design/hex-quic.md Q13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealFault {
    /// Its kind carries a secret, and it came unsealed on a connection that seals every message that does.
    #[error("kind {kind:#06x} carries a secret, and came unsealed on a connection that seals it")]
    Unsealed { kind: u16 },
    /// It came sealed, or was given to be sealed, yet its kind carries no secret: a connection seals only those that do.
    #[error("kind {kind:#06x} carries no secret, and only a message that does is sealed")]
    NoSecret { kind: u16 },
    /// It came sealed on a connection that seals nothing.
    #[error("kind {kind:#06x} came sealed on a connection that seals nothing")]
    Unsealing { kind: u16 },
    /// Its seal doesn't open under the peer's key: its header, nonce, body or tag isn't what was sealed.
    #[error("its seal doesn't open under the peer's key")]
    Broken,
    /// This side's key has sealed all it may: its nonces, one more of which would repeat one, or AES-GCM's limit on what
    /// one key seals (RFC 8446 §5.5). Nothing more is sealed under it; a new connection's key seals on.
    #[error("this side's key has sealed all it may")]
    Spent,
    /// The cipher refused what it was given.
    #[error("the cipher refused {what}")]
    Cipher { what: &'static str },
}

impl Fault {
    pub(crate) fn value(what: &'static str, why: impl Into<String>) -> Self {
        Self::Value {
            what,
            why: why.into(),
        }
    }
}

/// What a message of another wire is refused saying: both identities and both builds.
fn other_wire(theirs: u64, build: Option<&str>) -> String {
    format!(
        "the peer's wire is {theirs:016x} ({}), this side's {IDENTITY:016x} ({}): two sides talk only when built from \
         the same wire",
        build.unwrap_or("its build unsaid"),
        self::build()
    )
}

impl From<Fault> for Refusal {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Storage { what, bound } => Refusal::Exhausted { what, bound },
            fault => {
                let what = match &fault {
                    Fault::Wire { .. } => "wire",
                    Fault::Seal(_) => "seal",
                    _ => "message",
                };
                Refusal::Invalid {
                    what,
                    why: fault.to_string(),
                }
            }
        }
    }
}

/// A message as a socket received it: the bytes its buffer held, its whole size (the kernel's, which
/// is larger when the message was cut to the buffer), how many descriptors came with it, and whether
/// more came than the receiver had room for (`MSG_CTRUNC`; the kernel closed the rest, unix(7)).
#[derive(Debug, Clone, Copy)]
pub struct Received<'a> {
    pub bytes: &'a [u8],
    pub size: usize,
    pub descriptors: usize,
    pub descriptors_cut: bool,
}

/// A request refused: the fault, and where to send the refusal (the request's id; 0 when its header couldn't be
/// read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bad {
    pub id: u64,
    pub fault: Fault,
}

/// A request as a message of at most `bound` bytes.
pub fn request(id: u64, request: &Request, bound: usize) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(bound);
    match request {
        Request::Hello { build } => {
            w.header(kind::HELLO, id);
            w.string(build)?;
        }
        Request::Release => w.header(kind::RELEASE, id),
        Request::Taken => w.header(kind::TAKEN, id),
        Request::StreamsReady => w.header(kind::STREAMS_READY, id),
        Request::Call(call) => put_call(&mut w, id, call)?,
        Request::Lessor(made) => {
            w.header(kind::LESSOR, id);
            match made {
                Ok(()) => w.u8(0),
                Err(refusal) => {
                    w.u8(1);
                    put_refusal(&mut w, refusal)?;
                }
            }
        }
        Request::Leased { run, answer } => {
            w.header(kind::LEASED, id);
            w.u64(*run);
            match answer {
                Ok(lease) => put_lease(&mut w, lease)?,
                Err(refused) => {
                    w.u8(1);
                    put_lease_refused(&mut w, refused)?;
                }
            }
        }
        Request::Captured { run, answer } => {
            w.header(kind::CAPTURED, id);
            // A captured sign-in's placeholders are its capability: wiped where held, as a value is.
            w.secret();
            w.u64(*run);
            match answer {
                Ok(captured) => {
                    w.u8(0);
                    put_captured(&mut w, captured)?;
                }
                Err(refused) => {
                    w.u8(1);
                    put_sign_in_refused(&mut w, refused)?;
                }
            }
        }
        Request::Refreshed { run, answer } => {
            w.header(kind::REFRESHED, id);
            w.u64(*run);
            match answer {
                Ok(response) => {
                    w.secret();
                    let room = 1usize
                        .checked_add(size_of::<u32>())
                        .and_then(|n| n.checked_add(response.expose().len()))
                        .ok_or_else(|| {
                            Fault::value("refresh", "its size passes this machine's addresses")
                        })?;
                    w.reserve_fixed(room)?;
                    w.u8(0);
                    w.bytes(response.expose())?;
                }
                Err(refused) => {
                    w.u8(1);
                    put_sign_in_refused(&mut w, refused)?;
                }
            }
        }
    }
    w.done()
}

/// Whether `message` carries a secret, a lease's value ([`Request::Leased`]) or a sign-in's tokens, so whoever holds its
/// bytes in the clear wipes them as it lets them go (docs/design/credential-grants.md C7, C8), and a connection that
/// seals seals it ([`seal`]): read from its kind alone, before or without reading the rest. A sealed one's kind says
/// none: it holds its secret sealed.
pub fn carries_secret(message: &[u8]) -> bool {
    kind_of(message).is_some_and(secret_kind)
}

/// Whether a message of kind `of` carries a secret.
fn secret_kind(of: u16) -> bool {
    matches!(
        of,
        kind::LEASED | kind::CAPTURED | kind::REFRESHED | kind::CAPTURE_ASK
    )
}

/// `message`'s kind as its header has it, its [`SEALED`] mark among it; none for one too short to name one.
fn kind_of(message: &[u8]) -> Option<u16> {
    message
        .get(12..14)
        .and_then(|got| <[u8; 2]>::try_from(got).ok())
        .map(u16::from_le_bytes)
}

/// The mark of a sealed message's kind ([`seal`]; docs/design/hex-quic.md Q13): bit 14, which no kind this wire defines
/// has (its requests run up from `0x0001` and its answers from `0x8001`, each far short of it), so a sealed kind is
/// never another kind.
pub const SEALED: u16 = 0x4000;
/// A sealed body's nonce: AES-GCM's 96 bits, the length NIST SP 800-38D §5.2.1.1 recommends, which needs no hash of its
/// own to make a counter.
pub const NONCE_BYTES: usize = 12;
/// A sealed body's tag: AES-GCM's whole 128 bits (NIST SP 800-38D §5.2.1.2).
pub const TAG_BYTES: usize = 16;
/// What sealing adds to a message: its nonce and its tag.
pub const SEAL_BYTES: usize = NONCE_BYTES + TAG_BYTES;

/// What seals and opens the bodies of the messages a connection carries that carry a secret (docs/design/hex-quic.md
/// Q13): AES-256-GCM, a key each way, hex-quic's from the connection's TLS session. The seal keeps a secret out of the
/// buffers beneath Hex's lanes in the clear, which Hex can't wipe; it is no second channel security, the connection's TLS
/// being that.
pub trait Seal: Send + Sync {
    /// Seals `body` into `sealed`, as long as it, under this side's key with `aad` bound to it: the nonce it took, this
    /// side's next, never one taken before, and its tag.
    fn seal(
        &self,
        aad: &[u8],
        body: &[u8],
        sealed: &mut [u8],
    ) -> Result<([u8; NONCE_BYTES], [u8; TAG_BYTES]), SealFault>;
    /// Opens `sealed`, which the peer sealed under its key with `nonce` and `aad` bound to it, into `body`, as long as
    /// it, checked against `tag`.
    fn open(
        &self,
        aad: &[u8],
        nonce: &[u8; NONCE_BYTES],
        sealed: &[u8],
        tag: &[u8; TAG_BYTES],
        body: &mut [u8],
    ) -> Result<(), SealFault>;
}

/// Whether `message` came sealed: its kind marked [`SEALED`].
pub fn sealed(message: &[u8]) -> bool {
    kind_of(message).is_some_and(|marked| marked & SEALED != 0)
}

/// `message`, which carries a secret, as a connection that seals sends it (docs/design/hex-quic.md Q13): its header,
/// its kind marked [`SEALED`], then its nonce, its body sealed under `seal` with that header bound to it, and its tag, at
/// most [`MAX_MESSAGE`] in all. `message` stays its holder's to wipe; the sealed copy holds none of its body in the
/// clear. One whose kind carries no secret is refused ([`SealFault::NoSecret`]): it goes as it is.
pub fn seal(message: &[u8], seal: &dyn Seal) -> Result<Vec<u8>, Fault> {
    let short = || Fault::Truncated {
        need: HEADER_BYTES,
        have: message.len(),
    };
    let kind = kind_of(message).ok_or_else(short)?;
    if !secret_kind(kind) {
        return Err(Fault::Seal(SealFault::NoSecret { kind }));
    }
    let (header, body) = message.split_at_checked(HEADER_BYTES).ok_or_else(short)?;
    let size = [NONCE_BYTES, body.len(), TAG_BYTES]
        .into_iter()
        .try_fold(HEADER_BYTES, usize::checked_add)
        .filter(|size| *size <= MAX_MESSAGE)
        .ok_or(Fault::Oversized {
            bytes: message.len().saturating_add(SEAL_BYTES),
            bound: MAX_MESSAGE,
        })?;
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| Fault::Storage {
        what: "a sealed message",
        bound: size,
    })?;
    out.extend_from_slice(header);
    out.resize(size, 0);
    let laid = || Fault::value("a sealed message", "its parts don't fit its length");
    let (head, rest) = out.split_at_mut_checked(HEADER_BYTES).ok_or_else(laid)?;
    if let Some(at) = head.get_mut(12..14) {
        at.copy_from_slice(&(kind | SEALED).to_le_bytes());
    }
    let (nonce_at, rest) = rest
        .split_first_chunk_mut::<NONCE_BYTES>()
        .ok_or_else(laid)?;
    let (body_at, tag_at) = rest.split_last_chunk_mut::<TAG_BYTES>().ok_or_else(laid)?;
    let (nonce, tag) = seal.seal(head, body, body_at).map_err(Fault::Seal)?;
    *nonce_at = nonce;
    *tag_at = tag;
    Ok(out)
}

/// `message` as it came on a connection that seals under `seal`, or seals nothing (none; docs/design/hex-quic.md Q13):
/// one that came sealed opened, its header back as it was before its kind was marked; any other as it came. Refused,
/// typed: one whose kind carries a secret and that came unsealed where the connection seals ([`SealFault::Unsealed`]),
/// wiped as it goes; one sealed where the connection seals nothing ([`SealFault::Unsealing`]) or whose kind carries no
/// secret ([`SealFault::NoSecret`]); one whose seal doesn't open under the peer's key ([`SealFault::Broken`]), its
/// header, nonce, body or tag not what was sealed. A message too short to name its kind comes back as it came, for its
/// reader to refuse.
pub fn open(mut message: Vec<u8>, seal: Option<&dyn Seal>) -> Result<Vec<u8>, Fault> {
    let Some(marked) = kind_of(&message) else {
        return Ok(message);
    };
    let kind = marked & !SEALED;
    let Some(seal) = seal else {
        if marked == kind {
            return Ok(message);
        }
        return Err(Fault::Seal(SealFault::Unsealing { kind }));
    };
    if marked == kind {
        if secret_kind(kind) {
            hex_api::lease::wipe(&mut message);
            return Err(Fault::Seal(SealFault::Unsealed { kind }));
        }
        return Ok(message);
    }
    if !secret_kind(kind) {
        return Err(Fault::Seal(SealFault::NoSecret { kind }));
    }
    let short = || Fault::Truncated {
        need: HEADER_BYTES.saturating_add(SEAL_BYTES),
        have: message.len(),
    };
    let (header, rest) = message.split_at_checked(HEADER_BYTES).ok_or_else(short)?;
    let (nonce, rest) = rest.split_first_chunk::<NONCE_BYTES>().ok_or_else(short)?;
    let (sealed, tag) = rest.split_last_chunk::<TAG_BYTES>().ok_or_else(short)?;
    let size = HEADER_BYTES.saturating_add(sealed.len());
    let mut opened = Vec::new();
    opened.try_reserve_exact(size).map_err(|_| Fault::Storage {
        what: "an opened message",
        bound: size,
    })?;
    opened.extend_from_slice(header);
    if let Some(at) = opened.get_mut(12..14) {
        at.copy_from_slice(&kind.to_le_bytes());
    }
    opened.resize(size, 0);
    let read = match opened.get_mut(HEADER_BYTES..) {
        Some(body) => seal.open(header, nonce, sealed, tag, body),
        None => Err(SealFault::Broken),
    };
    if let Err(why) = read {
        hex_api::lease::wipe(&mut opened);
        return Err(Fault::Seal(why));
    }
    Ok(opened)
}

/// A lease as [`Request::Leased`] carries it: given (0), its name, its life, then its value last, so nothing can refuse
/// the message once the value is in it, and the message's room reserved before, so no copy of the value is left behind
/// as it grows ([`Writer::secret`]).
fn put_lease(w: &mut Writer, lease: &Lease) -> Result<(), Fault> {
    let value = lease.value.expose();
    let life = lease
        .life
        .map(|life| {
            u64::try_from(life.as_nanos())
                .map_err(|_| Fault::value("lease life", "it passes a u64 of nanoseconds"))
        })
        .transpose()?;
    let lengths = |n: usize| n.checked_add(size_of::<u32>());
    let rest = [
        Some(1usize),
        lengths(lease.lease.len()),
        Some(1usize.saturating_add(if life.is_some() { size_of::<u64>() } else { 0 })),
        lengths(value.len()),
    ]
    .into_iter()
    .try_fold(0usize, |n, part| part.and_then(|part| n.checked_add(part)))
    .ok_or_else(|| Fault::value("lease", "its size passes this machine's addresses"))?;
    w.secret();
    w.reserve_fixed(rest)?;
    w.u8(0);
    w.string(&lease.lease)?;
    w.option(life, |w, nanos| w.u64(nanos));
    w.bytes(value)
}

/// A lease read from [`Request::Leased`]: its value copied into a [`Value`] at once, wiped as it drops; the message's own
/// bytes are their holder's to wipe ([`carries_secret`]).
fn lease(r: &mut Reader<'_>) -> Result<Lease, Fault> {
    let lease = r.string("lease")?;
    let life = r.option(|r| r.u64().map(std::time::Duration::from_nanos))?;
    let value = r.bytes()?;
    let mut held: Vec<u8> = Vec::new();
    held.try_reserve_exact(value.len())
        .map_err(|_| Fault::Storage {
            what: "lease value",
            bound: value.len(),
        })?;
    held.extend_from_slice(value);
    Ok(Lease {
        lease,
        value: Value::new(held),
        life,
    })
}

/// Why a lease is refused, by Shards' kinds (`LeaseRefused`), each with its words.
fn put_lease_refused(w: &mut Writer, refused: &LeaseRefused) -> Result<(), Fault> {
    match refused {
        LeaseRefused::Denied(why) => {
            w.u8(0);
            w.string(why)
        }
        LeaseRefused::KeyReplaced { asked, current } => {
            w.u8(1);
            w.string(asked)?;
            w.option_with(current.as_deref(), |w, current| w.string(current))
        }
        LeaseRefused::Unavailable(why) => {
            w.u8(2);
            w.string(why)
        }
        LeaseRefused::Ended => {
            w.u8(3);
            Ok(())
        }
        LeaseRefused::Lapsed => {
            w.u8(4);
            Ok(())
        }
    }
}

fn lease_refused(r: &mut Reader<'_>) -> Result<LeaseRefused, Fault> {
    Ok(match r.u8()? {
        0 => LeaseRefused::Denied(r.string("a denied lease's words")?),
        1 => LeaseRefused::KeyReplaced {
            asked: r.string("a replaced key")?,
            current: r.option(|r| r.string("the run's key"))?,
        },
        2 => LeaseRefused::Unavailable(r.string("an unavailable lease's words")?),
        3 => LeaseRefused::Ended,
        4 => LeaseRefused::Lapsed,
        tag => {
            return Err(Fault::value(
                "lease refusal",
                format!("{tag} names a refusal the wire doesn't define"),
            ));
        }
    })
}

/// What a sign-in's capture gave (C8): its access token's reference, its placeholder, and its refresh token's where one.
fn put_captured(w: &mut Writer, captured: &hex_api::Captured) -> Result<(), Fault> {
    w.string(&captured.access)?;
    w.string(&captured.access_placeholder)?;
    w.option_with(captured.refresh_placeholder.as_deref(), |w, p| w.string(p))
}

fn captured(r: &mut Reader<'_>) -> Result<hex_api::Captured, Fault> {
    Ok(hex_api::Captured {
        access: r.string("a captured access reference")?,
        access_placeholder: r.string("a captured access placeholder")?,
        refresh_placeholder: r.option(|r| r.string("a captured refresh placeholder"))?,
    })
}

/// Why a sign-in's ask is refused, by Shards' kinds (`SignInRefusal`), each with its words.
fn put_sign_in_refused(w: &mut Writer, refused: &SignInRefused) -> Result<(), Fault> {
    match refused {
        SignInRefused::Denied(why) => {
            w.u8(0);
            w.string(why)
        }
        SignInRefused::Unavailable(why) => {
            w.u8(1);
            w.string(why)
        }
        SignInRefused::Ended => {
            w.u8(2);
            Ok(())
        }
        SignInRefused::Unanswered => {
            w.u8(3);
            Ok(())
        }
    }
}

fn sign_in_refused(r: &mut Reader<'_>) -> Result<SignInRefused, Fault> {
    Ok(match r.u8()? {
        0 => SignInRefused::Denied(r.string("a denied sign-in's words")?),
        1 => SignInRefused::Unavailable(r.string("an unavailable sign-in's words")?),
        2 => SignInRefused::Ended,
        3 => SignInRefused::Unanswered,
        tag => {
            return Err(Fault::value(
                "sign-in refusal",
                format!("{tag} names a refusal the wire doesn't define"),
            ));
        }
    })
}

/// A value read into a [`Value`] at once, wiped as it drops; the message's own bytes are their holder's to wipe.
fn value(r: &mut Reader<'_>, what: &'static str) -> Result<Value, Fault> {
    let bytes = r.bytes()?;
    let mut held: Vec<u8> = Vec::new();
    held.try_reserve_exact(bytes.len())
        .map_err(|_| Fault::Storage {
            what,
            bound: bytes.len(),
        })?;
    held.extend_from_slice(bytes);
    Ok(Value::new(held))
}

/// A sign-in's tokens as a capture's ask carries them (C8): the message's room reserved before any token goes in, so no
/// copy is left as it grows, and the writer wipes what it held of one that fails ([`Writer::secret`]).
fn put_tokens(w: &mut Writer, tokens: &SignInTokens) -> Result<(), Fault> {
    let field = |n: usize| n.checked_add(size_of::<u32>());
    let option = |v: Option<usize>| v.map_or(Some(1), |n| field(n).and_then(|n| n.checked_add(1)));
    let room = [
        field(tokens.access.expose().len()),
        option(tokens.refresh.as_ref().map(|v| v.expose().len())),
        1usize.checked_add(size_of::<u64>()),
        option(tokens.id_token.as_ref().map(|v| v.expose().len())),
        option(tokens.scope.as_ref().map(String::len)),
    ]
    .into_iter()
    .try_fold(0usize, |n, part| part.and_then(|part| n.checked_add(part)))
    .ok_or_else(|| {
        Fault::value(
            "sign-in tokens",
            "their size passes this machine's addresses",
        )
    })?;
    w.secret();
    w.reserve_fixed(room)?;
    w.bytes(tokens.access.expose())?;
    w.option_with(tokens.refresh.as_ref(), |w, v| w.bytes(v.expose()))?;
    w.option(tokens.expires_in, |w, s| w.u64(s));
    w.option_with(tokens.id_token.as_ref(), |w, v| w.bytes(v.expose()))?;
    w.option_with(tokens.scope.as_deref(), |w, s| w.string(s))
}

fn tokens(r: &mut Reader<'_>) -> Result<SignInTokens, Fault> {
    Ok(SignInTokens {
        access: value(r, "an access token")?,
        refresh: r.option(|r| value(r, "a refresh token"))?,
        expires_in: r.option(|r| r.u64())?,
        id_token: r.option(|r| value(r, "an ID token"))?,
        scope: r.option(|r| r.string("a token's scope"))?,
    })
}

/// An answer as a message of at most `bound` bytes.
pub fn answer(id: u64, answer: &Answer, bound: usize) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(bound);
    match answer {
        Answer::Hello(h) => {
            w.header(kind::HELLO_ANSWER, id);
            w.string(&h.build)?;
            w.list(&h.classes, |w, c| {
                w.u8(class_tag(*c));
                Ok(())
            })?;
            for n in [
                h.limits.connections,
                h.limits.message_bytes,
                h.limits.in_flight,
                h.limits.programs,
            ] {
                w.usize(n)?;
            }
            w.usize(h.budgets.in_flight)?;
            w.usize(h.budgets.programs)?;
        }
        Answer::Scheduled(reply) => {
            w.header(kind::SCHEDULED, id);
            w.string(
                &serde_json::to_string(reply)
                    .map_err(|e| Fault::value("scheduler reply", e.to_string()))?,
            )?;
        }
        Answer::Capacity(capacity) => {
            w.header(kind::CAPACITY_ANSWER, id);
            w.option_with(capacity.as_ref(), |w, capacity| {
                check_capacity(capacity)?;
                w.string(&capacity.id)?;
                for room in [capacity.total, capacity.free] {
                    w.u64(room.cpu_milli);
                    w.u64(room.mem_bytes);
                    w.u64(room.disk_bytes);
                }
                w.device_count(capacity.devices.len(), 22)?;
                for device in &capacity.devices {
                    put_device_id(w, &device.id);
                    w.string(&device.model)?;
                    w.u64(device.size);
                    put_device_holder(w, device.holder.as_ref())?;
                }
                put_cost_option(w, capacity.cost)?;
                put_capacity_routes(w, capacity)?;
                put_unavailable_devices(w, &capacity.unavailable)?;
                // Joint plan step 14: after `unavailable`.
                w.reserve_fixed(1)?;
                w.option_with(capacity.learned.as_ref(), put_learned)?;
                // Joint plan step 14, C7: the machine's CPU time and uptime, after `learned`.
                w.reserve_fixed(2)?;
                w.option(capacity.own.cpu_ns, Writer::u64);
                w.option(capacity.own.uptime_ns, Writer::u64);
                // Joint plan step 39: after `own`.
                w.reserve_fixed(size_of::<u32>())?;
                w.device_count(capacity.node_vms.len(), NODE_VM_RECEIPT_BYTES)?;
                for receipt in &capacity.node_vms {
                    put_node_vm_receipt(w, receipt)?;
                }
                // A-3 (ROOT, 2026-11-13): why its learner learned nothing for the class, after its node VMs.
                w.reserve_fixed(1)?;
                w.option_with(capacity.unlearned.as_deref(), |w, why| w.string(why))?;
                // Last (the review's A-8): its boxes' CPU time together, after `unlearned`.
                w.reserve_fixed(1)?;
                w.option(capacity.own.boxes_cpu_ns, Writer::u64);
                Ok(())
            })?;
        }
        Answer::PhysicalReport(report) => {
            w.header(kind::PHYSICAL_REPORT_ANSWER, id);
            put_physical_report(&mut w, report)?;
        }
        Answer::BackingOffer(offer) => {
            w.header(kind::BACKING_OFFER, id);
            put_backing_offer(&mut w, offer)?;
        }
        Answer::Info(info) => {
            w.header(kind::INFO, id);
            put_info(&mut w, info)?;
        }
        Answer::Done => w.header(kind::DONE, id),
        Answer::Started { pid } => {
            w.header(kind::STARTED, id);
            w.u32(*pid);
        }
        Answer::Offered(offer) => {
            w.header(kind::OFFERED, id);
            w.u32(offer.pid);
            w.u64(offer.started);
            w.path(&offer.vsock)?;
            w.u64(offer.token);
            w.u64(offer.exec);
            w.u8(offer.streams.bits());
            w.option_with(offer.setup, |w, setup| {
                let nanos = u64::try_from(setup.as_nanos())
                    .map_err(|_| Fault::value("offer setup", "it passes a u64 of nanoseconds"))?;
                w.u64(nanos);
                Ok(())
            })?;
        }
        Answer::Given { pid, given } => {
            w.header(kind::GIVEN, id);
            w.u32(*pid);
            w.u8(given.bits());
        }
        Answer::Usage(u) => {
            w.header(kind::USAGE_ANSWER, id);
            for n in [
                u.memory_bytes,
                u.memory_peak_bytes,
                u.oom_kills,
                u.pids,
                u.pids_refused,
                u.cpu_usec,
                u.io_read_bytes,
                u.io_write_bytes,
            ] {
                w.u64(n);
            }
            // Appended after the counters (joint plan step 12): absent, a byte 0.
            w.option_with(u.guest.as_ref(), put_guest_time)?;
            // Appended after guest time (D253; joint plan step 12): absent, a byte 0.
            w.option(u.started_from, put_started_from);
        }
        Answer::Boxes { infos, more } => {
            w.header(kind::BOXES, id);
            w.bool(*more);
            w.list(infos, put_info)?;
        }
        Answer::Refused(r) => {
            w.header(kind::REFUSED, id);
            put_refusal(&mut w, r)?;
        }
        Answer::Exited(exit) => {
            w.header(kind::EXITED, id);
            put_exit(&mut w, *exit);
        }
        Answer::Netns(counts) => {
            w.header(kind::NETNS_ANSWER, id);
            for n in [
                counts.made_ahead,
                counts.taken,
                counts.made_on_path,
                counts.discarded,
                counts.failed,
            ] {
                w.u64(n);
            }
            let by = counts.failed_by;
            for n in [by.home, by.unshare, by.open, by.loopback, by.back] {
                w.u64(n);
            }
            w.u8(counts.refused.map_or(0, |m| {
                u8::from(m.sys_admin()) | (u8::from(m.net_admin()) << 1)
            }));
            w.u64(counts.refused_on_path);
            let last = &counts.last_failed;
            for reason in [
                &last.home,
                &last.unshare,
                &last.open,
                &last.loopback,
                &last.back,
            ] {
                w.option_with(reason.as_ref(), |w, reason| {
                    w.option(reason.errno(), |w, errno| w.i32(errno));
                    w.string(reason.text())
                })?;
            }
        }
        Answer::Links { links, more } => {
            w.header(kind::LINKS_ANSWER, id);
            w.bool(*more);
            w.list(links, put_link)?;
        }
        Answer::LessorAsk(box_id) => {
            w.header(kind::LESSOR_ASK, id);
            w.string(box_id.as_str())?;
        }
        Answer::LeaseAsk {
            run,
            reference,
            key,
        } => {
            w.header(kind::LEASE_ASK, id);
            w.u64(*run);
            w.string(reference)?;
            w.option(key.as_ref(), |w, key| w.raw(key));
        }
        Answer::LeaseGiven(lease) => {
            w.header(kind::LEASE_GIVEN, id);
            w.string(lease)?;
        }
        Answer::LessorEnded => w.header(kind::LESSOR_ENDED, id),
        Answer::CaptureAsk {
            run,
            sign_in,
            tokens,
            key,
        } => {
            w.header(kind::CAPTURE_ASK, id);
            w.u64(*run);
            w.string(sign_in)?;
            w.option(key.as_ref(), |w, key| w.raw(key));
            // Its tokens last, so nothing can refuse the message once they are in it.
            put_tokens(&mut w, tokens)?;
        }
        Answer::RefreshAsk { run, sign_in, key } => {
            w.header(kind::REFRESH_ASK, id);
            w.u64(*run);
            w.string(sign_in)?;
            w.option(key.as_ref(), |w, key| w.raw(key));
        }
        Answer::SignInRevoked { run, sign_in, key } => {
            w.header(kind::SIGN_IN_REVOKED, id);
            w.u64(*run);
            w.string(sign_in)?;
            w.option(key.as_ref(), |w, key| w.raw(key));
        }
    }
    w.done()
}

/// A retained refusal's canonical Answer envelope, id zero, without cloning its body.
/// Both sizing and writing use the ordinary refusal codec; output allocation is fallible.
pub fn snapshot_refusal(refusal: &Refusal, bound: usize) -> Result<Vec<u8>, Fault> {
    snapshot(kind::REFUSED, bound, |w| put_refusal(w, refusal))
}

/// A retained exit's canonical Answer envelope, id zero, using the ordinary exit codec.
pub fn snapshot_exit(exit: Exit, bound: usize) -> Result<Vec<u8>, Fault> {
    snapshot(kind::EXITED, bound, |w| {
        put_exit(w, exit);
        Ok(())
    })
}

fn snapshot(
    kind: u16,
    bound: usize,
    put: impl Fn(&mut Writer) -> Result<(), Fault>,
) -> Result<Vec<u8>, Fault> {
    let mut count = Writer::counting(bound);
    count.header(kind, 0);
    put(&mut count)?;
    let bytes = count.counted()?;
    let mut output = Writer::fixed(bytes)?;
    output.header(kind, 0);
    put(&mut output)?;
    output.done()
}

/// A list's answer as messages of at most `bound` bytes each: as many boxes in each as fit, every
/// message but the last marked `more`, so a node holding more boxes than one message carries still
/// answers a list whole. An empty list is one message.
pub fn boxes(id: u64, infos: &[BoxInfo], bound: usize) -> Result<Vec<Vec<u8>>, Fault> {
    parts(id, infos, bound, put_info, |infos, more| Answer::Boxes {
        infos,
        more,
    })
}

/// A links answer as messages of at most `bound` bytes each, split as [`boxes`] splits a list: a node's connections
/// bound admits more peers than one message names.
pub fn links(id: u64, links: &[Link], bound: usize) -> Result<Vec<Vec<u8>>, Fault> {
    parts(id, links, bound, put_link, |links, more| Answer::Links {
        links,
        more,
    })
}

/// `items` as `whole`'s answers of at most `bound` bytes each: as many in each as fit, every one but the last marked
/// `more`; none is one answer.
fn parts<T: Clone>(
    id: u64,
    items: &[T],
    bound: usize,
    put: fn(&mut Writer, &T) -> Result<(), Fault>,
    whole: fn(Vec<T>, bool) -> Answer,
) -> Result<Vec<Vec<u8>>, Fault> {
    // A message's bytes before its items: the header, `more` and the list's count.
    const OVERHEAD: usize = HEADER_BYTES + 1 + 4;
    let room = bound.checked_sub(OVERHEAD).ok_or(Fault::Oversized {
        bytes: OVERHEAD,
        bound,
    })?;
    let mut parts: Vec<Vec<T>> = vec![Vec::new()];
    let mut used = 0usize;
    for item in items {
        let mut w = Writer::new(room);
        put(&mut w, item)?;
        let size = w.done()?.len();
        let fits = used.checked_add(size).filter(|u| *u <= room);
        match (fits, parts.last_mut()) {
            (Some(u), Some(part)) => {
                used = u;
                part.push(item.clone());
            }
            _ => {
                used = size;
                parts.push(vec![item.clone()]);
            }
        }
    }
    let last = parts.len().saturating_sub(1);
    parts
        .into_iter()
        .enumerate()
        .map(|(n, items)| answer(id, &whole(items, n < last), bound))
        .collect()
}

/// The request a received message holds, at most `bound` bytes, or why it holds none.
pub fn read_request(msg: &Received<'_>, bound: usize) -> Result<(Header, Request), Bad> {
    parse_request(msg, bound, true)
}

/// The request `bytes` hold, at most `bound` bytes, from a transport whose exec's stdio travels apart
/// from its message ([`crate::stream`]'s, on connections of its own): the message carries no
/// descriptors, and an exec's streams are what it names.
pub fn read_request_apart(bytes: &[u8], bound: usize) -> Result<(Header, Request), Bad> {
    let msg = Received {
        bytes,
        size: bytes.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    parse_request(&msg, bound, false)
}

/// A request, its descriptors counted against its exec's streams when they ride with it (`carried`).
fn parse_request(
    msg: &Received<'_>,
    bound: usize,
    carried: bool,
) -> Result<(Header, Request), Bad> {
    let mut r = Reader::new(msg.bytes);
    let (identity, kind, id) = r.header().map_err(|fault| Bad { id: 0, fault })?;
    let bad = |fault| Bad { id, fault };
    if identity != IDENTITY {
        return Err(bad(other(&mut r, identity, kind == kind::HELLO)));
    }
    check(msg, bound).map_err(bad)?;
    let request = match kind {
        kind::HELLO => Request::Hello {
            build: r.string("build").map_err(bad)?,
        },
        kind::RELEASE => Request::Release,
        kind::TAKEN => Request::Taken,
        kind::STREAMS_READY => Request::StreamsReady,
        kind::SCHEDULE => {
            let json = r.string("scheduler command").map_err(bad)?;
            Request::Call(Call::Schedule(Box::new(
                serde_json::from_str(&json)
                    .map_err(|e| bad(Fault::value("scheduler command", e.to_string())))?,
            )))
        }
        kind::CAPACITY => Request::Call(Call::Capacity(class(&mut r).map_err(bad)?)),
        kind::PHYSICAL_REPORT => Request::Call(Call::PhysicalReport),
        kind::INITIALIZE_BACKING => {
            let grant = backing_grant(&mut r).map_err(bad)?;
            let grant = triomphe::Arc::try_new(grant).map_err(|_| {
                bad(Fault::Storage {
                    what: "native backing grant",
                    bound: size_of::<BackingGrant>(),
                })
            })?;
            Request::Call(Call::InitializeBacking(grant))
        }
        kind::RELEASE_BACKING => {
            Request::Call(Call::ReleaseBacking(backing_key(&mut r).map_err(bad)?))
        }
        kind::BACKING_USAGE => Request::Call(Call::BackingUsage(backing_key(&mut r).map_err(bad)?)),
        kind::MEMBER => {
            let id = box_id(&mut r).map_err(bad)?;
            let member = member(&mut r).map_err(bad)?;
            let streams = r.u8().and_then(Streams::of).map_err(bad)?;
            let ask = r.u8().and_then(Streams::of).map_err(bad)?;
            Request::Call(Call::Member {
                id,
                member,
                streams,
                ask,
            })
        }
        kind::CREATE => Request::Call(Call::Create(Box::new(box_spec(&mut r).map_err(bad)?))),
        kind::START => Request::Call(Call::Start(box_id(&mut r).map_err(bad)?)),
        kind::EXEC => {
            let id = box_id(&mut r).map_err(bad)?;
            let program = program(&mut r).map_err(bad)?;
            let streams = r.u8().and_then(Streams::of).map_err(bad)?;
            Request::Call(Call::Exec {
                id,
                program,
                streams,
            })
        }
        kind::SIGNAL => {
            let id = box_id(&mut r).map_err(bad)?;
            let signal = r.u8().map_err(bad)?;
            let signal = Signal::new(i64::from(signal))
                .map_err(|e| bad(Fault::value("signal", e.to_string())))?;
            Request::Call(Call::Signal { id, signal })
        }
        kind::INSPECT => Request::Call(Call::Inspect(box_id(&mut r).map_err(bad)?)),
        kind::USAGE => Request::Call(Call::Usage(box_id(&mut r).map_err(bad)?)),
        kind::LIST => Request::Call(Call::List),
        kind::DELETE => Request::Call(Call::Delete(box_id(&mut r).map_err(bad)?)),
        kind::NETNS => Request::Call(Call::Netns),
        kind::LINKS => Request::Call(Call::Links),
        kind::LESSORS => Request::Call(Call::Lessors),
        kind::MEMBER_SIGNAL => {
            let id = box_id(&mut r).map_err(bad)?;
            let member = member(&mut r).map_err(bad)?;
            let signal = r.u8().map_err(bad)?;
            let signal = Signal::new(i64::from(signal))
                .map_err(|e| bad(Fault::value("signal", e.to_string())))?;
            Request::Call(Call::MemberSignal { id, member, signal })
        }
        kind::START_FOLLOWED => Request::Call(Call::Started(box_id(&mut r).map_err(bad)?)),
        kind::RESERVED => Request::Call(Call::Reserved(hex_api::Reserved {
            keeper: r.u64().map_err(bad)?,
            reserved: resources(&mut r).map_err(bad)?,
        })),
        kind::MCP => {
            let id = box_id(&mut r).map_err(bad)?;
            let member = member(&mut r).map_err(bad)?;
            let streams = r.u8().and_then(Streams::of).map_err(bad)?;
            let ask = r.u8().and_then(Streams::of).map_err(bad)?;
            Request::Call(Call::Mcp {
                id,
                member,
                streams,
                ask,
            })
        }
        kind::LESSOR => Request::Lessor(match r.u8().map_err(bad)? {
            0 => Ok(()),
            1 => Err(refusal(&mut r).map_err(bad)?),
            tag => {
                return Err(bad(Fault::value(
                    "lessor answer",
                    format!("{tag} names neither a lessor made nor a refusal"),
                )));
            }
        }),
        kind::LEASED => {
            let run = r.u64().map_err(bad)?;
            let answer = match r.u8().map_err(bad)? {
                0 => Ok(lease(&mut r).map_err(bad)?),
                1 => Err(lease_refused(&mut r).map_err(bad)?),
                tag => {
                    return Err(bad(Fault::value(
                        "lease answer",
                        format!("{tag} names neither a lease nor a refusal"),
                    )));
                }
            };
            Request::Leased { run, answer }
        }
        kind::CAPTURED => {
            let run = r.u64().map_err(bad)?;
            let answer = match r.u8().map_err(bad)? {
                0 => Ok(captured(&mut r).map_err(bad)?),
                1 => Err(sign_in_refused(&mut r).map_err(bad)?),
                tag => {
                    return Err(bad(Fault::value(
                        "capture answer",
                        format!("{tag} names neither a capture nor a refusal"),
                    )));
                }
            };
            Request::Captured { run, answer }
        }
        kind::REFRESHED => {
            let run = r.u64().map_err(bad)?;
            let answer = match r.u8().map_err(bad)? {
                0 => Ok(value(&mut r, "a refreshed token response").map_err(bad)?),
                1 => Err(sign_in_refused(&mut r).map_err(bad)?),
                tag => {
                    return Err(bad(Fault::value(
                        "refresh answer",
                        format!("{tag} names neither a token response nor a refusal"),
                    )));
                }
            };
            Request::Refreshed { run, answer }
        }
        kind::KILL => Request::Call(Call::Kill {
            exec: r.u64().map_err(bad)?,
        }),
        kind::EXEC_STREAMS => {
            let id = box_id(&mut r).map_err(bad)?;
            let program = program(&mut r).map_err(bad)?;
            let streams = r.u8().and_then(Streams::of).map_err(bad)?;
            let ask = r.u8().and_then(Streams::of).map_err(bad)?;
            Request::Call(Call::ExecStreams {
                id,
                program,
                streams,
                ask,
            })
        }
        kind::RUN => {
            let spec = box_spec(&mut r).map_err(bad)?;
            let program = program(&mut r).map_err(bad)?;
            let streams = r.u8().and_then(Streams::of).map_err(bad)?;
            let ask = r.u8().and_then(Streams::of).map_err(bad)?;
            Request::Call(Call::Run {
                spec: Box::new(spec),
                program,
                streams,
                ask,
            })
        }
        _ => {
            return Err(bad(Fault::Undefined {
                kind,
                expected: "a request",
            }));
        }
    };
    let want = match &request {
        Request::Call(
            Call::Exec { streams, .. }
            | Call::ExecStreams { streams, .. }
            | Call::Run { streams, .. }
            | Call::Member { streams, .. }
            | Call::Mcp { streams, .. },
        ) if carried => streams.count(),
        _ => 0,
    };
    if msg.descriptors != want {
        return Err(bad(Fault::Descriptors {
            what: request_name(kind),
            want,
            got: msg.descriptors,
        }));
    }
    r.all_read(kind).map_err(bad)?;
    Ok((Header { kind, id }, request))
}

/// What a message of another wire, whose sender's identity is `theirs`, is refused as: its sender's build read
/// where the message is a hello or a hello's answer (`hello`), whose head every build lays out alike.
fn other(r: &mut Reader<'_>, theirs: u64, hello: bool) -> Fault {
    Fault::Wire {
        theirs,
        build: hello.then(|| r.string("build").ok()).flatten(),
    }
}

/// The answer a received message holds, at most `bound` bytes, or why it holds none. An answer carries
/// no descriptors but [`Answer::Given`]'s, one for each stream it names.
pub fn read_answer(msg: &Received<'_>, bound: usize) -> Result<(Header, Answer), Fault> {
    let mut r = Reader::new(msg.bytes);
    let (identity, kind, id) = r.header()?;
    if identity != IDENTITY {
        return Err(other(&mut r, identity, kind == kind::HELLO_ANSWER));
    }
    check(msg, bound)?;
    if kind != kind::GIVEN && msg.descriptors != 0 {
        return Err(Fault::Descriptors {
            what: "an answer",
            want: 0,
            got: msg.descriptors,
        });
    }
    let answer = match kind {
        kind::HELLO_ANSWER => {
            let build = r.string("build")?;
            let classes = r.list(class)?;
            let mut usize = || r.usize();
            let limits = Limits {
                connections: usize()?,
                message_bytes: usize()?,
                in_flight: usize()?,
                programs: usize()?,
            };
            let budgets = Budgets {
                in_flight: usize()?,
                programs: usize()?,
            };
            Answer::Hello(Hello {
                build,
                classes,
                limits,
                budgets,
            })
        }
        kind::SCHEDULED => {
            let json = r.string("scheduler reply")?;
            Answer::Scheduled(Box::new(
                serde_json::from_str(&json)
                    .map_err(|e| Fault::value("scheduler reply", e.to_string()))?,
            ))
        }
        kind::PHYSICAL_REPORT_ANSWER => Answer::PhysicalReport(physical_report(&mut r)?),
        kind::BACKING_OFFER => Answer::BackingOffer(backing_offer(&mut r)?),
        kind::CAPACITY_ANSWER => Answer::Capacity(r.option(|r| {
            let id = r.string("capacity pool")?;
            let mut capacity = Capacity {
                id,
                total: resources(r)?,
                free: resources(r)?,
                devices: r.device_list(22, |r| {
                    let id = device_id(r)?;
                    let model = r.device_string("devices.model")?;
                    let size = r.u64()?;
                    let holder = device_holder(r)?;
                    Ok(DeviceInfo {
                        id,
                        model,
                        size,
                        holder,
                    })
                })?,
                cost: r.option(runtime_cost)?,
                routes: r.bounded_list(
                    ROUTE_OFFER_MIN_BYTES,
                    MAX_RUNTIME_ROUTES,
                    "routes",
                    route_offer,
                )?,
                device_routes: r.device_list(size_of::<u64>(), Reader::u64)?,
                backings: r.bounded_list(
                    BACKING_OFFER_MIN_BYTES,
                    MAX_RUNTIME_ROUTES,
                    "backings",
                    backing_offer,
                )?,
                // Only a build of this same wire reaches here (IDENTITY), so it and `learned` after it are always
                // present, and a capacity cut before either is refused as any truncation is; an older JSON report
                // reads each as none (serde default).
                unavailable: r.device_list(UNAVAILABLE_MIN_BYTES, |r| {
                    Ok(DeviceUnavailable {
                        id: device_id(r)?,
                        why: r.device_string("unavailable.why")?,
                    })
                })?,
                // Then (joint plan step 14).
                learned: r.option(learned)?,
                // Then (joint plan step 14, C7); its boxes' CPU time is the last field, read below.
                own: hex_api::MachineTime {
                    cpu_ns: r.option(Reader::u64)?,
                    uptime_ns: r.option(Reader::u64)?,
                    boxes_cpu_ns: None,
                },
                // Then (joint plan step 39).
                node_vms: r.bounded_list(
                    NODE_VM_RECEIPT_BYTES,
                    MAX_LEARNED_HOLDINGS,
                    "node VM receipts",
                    node_vm_receipt,
                )?,
                // Then (A-3).
                unlearned: r.option(|r| r.string("why a learner learned nothing"))?,
            };
            // Last (the review's A-8).
            capacity.own.boxes_cpu_ns = r.option(Reader::u64)?;
            check_capacity(&capacity)?;
            Ok(capacity)
        })?),
        kind::INFO => Answer::Info(info(&mut r)?),
        kind::DONE => Answer::Done,
        kind::STARTED => Answer::Started { pid: r.u32()? },
        kind::OFFERED => Answer::Offered(StreamOffer {
            pid: r.u32()?,
            started: r.u64()?,
            vsock: r.path()?,
            token: r.u64()?,
            exec: r.u64()?,
            streams: r.u8().and_then(Streams::of)?,
            setup: r.option(|r| r.u64().map(std::time::Duration::from_nanos))?,
        }),
        kind::USAGE_ANSWER => Answer::Usage(Usage {
            memory_bytes: r.u64()?,
            memory_peak_bytes: r.u64()?,
            oom_kills: r.u64()?,
            pids: r.u64()?,
            pids_refused: r.u64()?,
            cpu_usec: r.u64()?,
            io_read_bytes: r.u64()?,
            io_write_bytes: r.u64()?,
            guest: r.option(guest_time)?,
            started_from: r.option(started_from)?,
        }),
        kind::BOXES => Answer::Boxes {
            more: r.bool("more")?,
            infos: r.list(info)?,
        },
        kind::REFUSED => Answer::Refused(refusal(&mut r)?),
        kind::EXITED => Answer::Exited(exit(&mut r)?),
        kind::NETNS_ANSWER => {
            let mut counts = netns::Counts {
                made_ahead: r.u64()?,
                taken: r.u64()?,
                made_on_path: r.u64()?,
                discarded: r.u64()?,
                failed: r.u64()?,
                ..netns::Counts::default()
            };
            counts.failed_by = netns::FailedBy {
                home: r.u64()?,
                unshare: r.u64()?,
                open: r.u64()?,
                loopback: r.u64()?,
                back: r.u64()?,
            };
            counts.refused = missing(r.u8()?)?;
            counts.refused_on_path = r.u64()?;
            counts.last_failed = netns::LastFailed {
                home: reason(&mut r)?,
                unshare: reason(&mut r)?,
                open: reason(&mut r)?,
                loopback: reason(&mut r)?,
                back: reason(&mut r)?,
            };
            Answer::Netns(counts)
        }
        kind::LINKS_ANSWER => Answer::Links {
            more: r.bool("more")?,
            links: r.list(link)?,
        },
        kind::LESSOR_ASK => Answer::LessorAsk(box_id(&mut r)?),
        kind::LEASE_ASK => Answer::LeaseAsk {
            run: r.u64()?,
            reference: r.string("a lease's reference")?,
            key: r.option(|r| r.array::<32>())?,
        },
        kind::LEASE_GIVEN => Answer::LeaseGiven(r.string("a lease given back")?),
        kind::LESSOR_ENDED => Answer::LessorEnded,
        kind::CAPTURE_ASK => {
            let run = r.u64()?;
            let sign_in = r.string("a capture's sign-in")?;
            let key = r.option(|r| r.array::<32>())?;
            Answer::CaptureAsk {
                run,
                sign_in,
                key,
                tokens: tokens(&mut r)?,
            }
        }
        kind::REFRESH_ASK => Answer::RefreshAsk {
            run: r.u64()?,
            sign_in: r.string("a refresh's sign-in")?,
            key: r.option(|r| r.array::<32>())?,
        },
        kind::SIGN_IN_REVOKED => Answer::SignInRevoked {
            run: r.u64()?,
            sign_in: r.string("a revoked sign-in")?,
            key: r.option(|r| r.array::<32>())?,
        },
        kind::GIVEN => {
            let pid = r.u32()?;
            let given = r.u8().and_then(Streams::of)?;
            if msg.descriptors != given.count() {
                return Err(Fault::Descriptors {
                    what: "a given answer",
                    want: given.count(),
                    got: msg.descriptors,
                });
            }
            Answer::Given { pid, given }
        }
        _ => {
            return Err(Fault::Undefined {
                kind,
                expected: "an answer",
            });
        }
    };
    r.all_read(kind)?;
    Ok((Header { kind, id }, answer))
}

/// What every message of this wire is checked for once its header is read: its size and its descriptors' room.
fn check(msg: &Received<'_>, bound: usize) -> Result<(), Fault> {
    if msg.size > bound {
        return Err(Fault::Oversized {
            bytes: msg.size,
            bound,
        });
    }
    if msg.descriptors_cut || msg.descriptors > MAX_FDS {
        return Err(Fault::DescriptorsCut);
    }
    Ok(())
}

/// A step's latest reason for a failed make: an option of its errno and its text, a text past
/// [`netns::MAX_REASON`] bytes refused rather than cut, so what is read is what was written.
fn reason(r: &mut Reader<'_>) -> Result<Option<netns::Reason>, Fault> {
    r.option(|r| {
        let errno = r.option(Reader::i32)?;
        let text = r.string("a failed make's reason")?;
        if text.len() > netns::MAX_REASON {
            return Err(Fault::value(
                "a failed make's reason",
                format!(
                    "{} bytes is over the {}-byte bound",
                    text.len(),
                    netns::MAX_REASON
                ),
            ));
        }
        Ok(netns::Reason::new(errno, &text))
    })
}

/// What a node lacks, from its byte in the network namespace counts: nothing, or what its bits name; a bit the wire
/// doesn't define is refused.
fn missing(bits: u8) -> Result<Option<netns::Missing>, Fault> {
    match bits {
        0 => Ok(None),
        1..=3 => Ok(netns::Missing::of(bits & 1 == 0, bits & 2 == 0)),
        _ => Err(Fault::value(
            "missing capabilities",
            format!("{bits:#04x} names a capability the wire doesn't define"),
        )),
    }
}

fn request_name(kind: u16) -> &'static str {
    match kind {
        kind::HELLO => "a hello",
        kind::CREATE => "a create",
        kind::START => "a start",
        kind::EXEC => "an exec",
        kind::SIGNAL => "a signal",
        kind::INSPECT => "an inspect",
        kind::USAGE => "a usage",
        kind::LIST => "a list",
        kind::DELETE => "a delete",
        kind::RELEASE => "a release",
        kind::NETNS => "a netns",
        kind::KILL => "a kill",
        kind::EXEC_STREAMS => "an exec asking for streams",
        kind::TAKEN => "a taken",
        kind::RUN => "a run",
        kind::INITIALIZE_BACKING => "an initialize backing",
        kind::RELEASE_BACKING => "a release backing",
        kind::BACKING_USAGE => "a backing usage",
        kind::MEMBER => "a member",
        kind::LINKS => "a links",
        kind::LESSORS => "a lessors",
        kind::LESSOR => "a lessor",
        kind::LEASED => "a leased",
        kind::CAPTURED => "a captured",
        kind::REFRESHED => "a refreshed",
        kind::MEMBER_SIGNAL => "a member's signal",
        kind::START_FOLLOWED => "a start followed",
        kind::RESERVED => "a keeper's word",
        kind::MCP => "an MCP connection",
        _ => "a request",
    }
}

fn put_call(w: &mut Writer, id: u64, call: &Call) -> Result<(), Fault> {
    match call {
        Call::InitializeBacking(grant) => {
            w.header(kind::INITIALIZE_BACKING, id);
            put_backing_grant(w, grant)
        }
        Call::ReleaseBacking(key) => {
            w.header(kind::RELEASE_BACKING, id);
            put_backing_key(w, *key)
        }
        Call::BackingUsage(key) => {
            w.header(kind::BACKING_USAGE, id);
            put_backing_key(w, *key)
        }
        Call::Member {
            id: b,
            member,
            streams,
            ask,
        } => {
            w.header(kind::MEMBER, id);
            w.string(b.as_str())?;
            put_member(w, member)?;
            w.u8(streams.bits());
            w.u8(ask.bits());
            Ok(())
        }
        Call::Mcp {
            id: b,
            member,
            streams,
            ask,
        } => {
            w.header(kind::MCP, id);
            w.string(b.as_str())?;
            put_member(w, member)?;
            w.u8(streams.bits());
            w.u8(ask.bits());
            Ok(())
        }
        Call::PhysicalReport => {
            w.header(kind::PHYSICAL_REPORT, id);
            Ok(())
        }
        Call::Capacity(class) => {
            w.header(kind::CAPACITY, id);
            w.u8(class_tag(*class));
            Ok(())
        }
        Call::Schedule(command) => {
            w.header(kind::SCHEDULE, id);
            w.string(
                &serde_json::to_string(command)
                    .map_err(|e| Fault::value("scheduler command", e.to_string()))?,
            )
        }
        Call::Create(spec) => {
            w.header(kind::CREATE, id);
            put_box_spec(w, spec)
        }
        Call::Start(b) => {
            w.header(kind::START, id);
            w.string(b.as_str())
        }
        Call::Started(b) => {
            w.header(kind::START_FOLLOWED, id);
            w.string(b.as_str())
        }
        Call::Reserved(word) => {
            w.header(kind::RESERVED, id);
            w.reserve_fixed(size_of::<u64>().saturating_add(RESOURCE_BYTES))?;
            w.u64(word.keeper);
            put_resources(w, word.reserved);
            Ok(())
        }
        Call::Exec {
            id: b,
            program,
            streams,
        } => {
            w.header(kind::EXEC, id);
            w.string(b.as_str())?;
            put_program(w, program)?;
            w.u8(streams.bits());
            Ok(())
        }
        Call::Signal { id: b, signal } => {
            w.header(kind::SIGNAL, id);
            w.string(b.as_str())?;
            let n = u8::try_from(signal.number())
                .map_err(|_| Fault::value("signal", format!("{} isn't a byte", signal.number())))?;
            w.u8(n);
            Ok(())
        }
        Call::Inspect(b) => {
            w.header(kind::INSPECT, id);
            w.string(b.as_str())
        }
        Call::Usage(b) => {
            w.header(kind::USAGE, id);
            w.string(b.as_str())
        }
        Call::List => {
            w.header(kind::LIST, id);
            Ok(())
        }
        Call::Delete(b) => {
            w.header(kind::DELETE, id);
            w.string(b.as_str())
        }
        Call::Netns => {
            w.header(kind::NETNS, id);
            Ok(())
        }
        Call::Links => {
            w.header(kind::LINKS, id);
            Ok(())
        }
        Call::Lessors => {
            w.header(kind::LESSORS, id);
            Ok(())
        }
        Call::MemberSignal {
            id: b,
            member,
            signal,
        } => {
            w.header(kind::MEMBER_SIGNAL, id);
            w.string(b.as_str())?;
            put_member(w, member)?;
            let n = u8::try_from(signal.number())
                .map_err(|_| Fault::value("signal", format!("{} isn't a byte", signal.number())))?;
            w.u8(n);
            Ok(())
        }
        Call::Kill { exec } => {
            w.header(kind::KILL, id);
            w.u64(*exec);
            Ok(())
        }
        Call::ExecStreams {
            id: b,
            program,
            streams,
            ask,
        } => {
            w.header(kind::EXEC_STREAMS, id);
            w.string(b.as_str())?;
            put_program(w, program)?;
            w.u8(streams.bits());
            w.u8(ask.bits());
            Ok(())
        }
        Call::Run {
            spec,
            program,
            streams,
            ask,
        } => {
            w.header(kind::RUN, id);
            put_box_spec(w, spec)?;
            put_program(w, program)?;
            w.u8(streams.bits());
            w.u8(ask.bits());
            Ok(())
        }
    }
}

fn put_box_spec(w: &mut Writer, spec: &BoxSpec) -> Result<(), Fault> {
    check_spec_route(spec)?;
    if let Some(allocation) = spec.allocation {
        allocation
            .covers(workload_caps(spec))
            .map_err(|e| Fault::value("allocation", e.to_string()))?;
    }
    spec.check_devices()
        .map_err(|e| Fault::value("devices", e.to_string()))?;
    w.string(spec.id.as_str())?;
    put_bounds(w, &spec.bounds);
    match &spec.class {
        ClassSpec::Process => w.u8(0),
        ClassSpec::Oci(oci) => {
            w.u8(1);
            put_oci(w, oci)?;
        }
        ClassSpec::Bundle(path) => {
            w.u8(2);
            w.path(path)?;
        }
        ClassSpec::Firecracker(oci) => {
            w.u8(3);
            put_oci(w, oci)?;
        }
        ClassSpec::Shards(oci) => {
            w.u8(4);
            put_oci(w, oci)?;
        }
        ClassSpec::ShardsImage(image) => {
            w.u8(5);
            put_shards_image(w, image)?;
        }
    }
    w.option_with(spec.owner.as_ref(), |w, owner| {
        w.string(&owner.name)?;
        w.bool(owner.reuse);
        Ok(())
    })?;
    w.device_count(spec.devices.len(), 9)?;
    for id in &spec.devices {
        put_device_id(w, id);
    }
    w.reserve_fixed(if spec.allocation.is_some() {
        OPTIONAL_ALLOCATION_BYTES
    } else {
        1
    })?;
    w.option_with(spec.allocation, |w, allocation| {
        put_resources(w, allocation.resources);
        put_cost(w, allocation.cost)?;
        w.option(allocation.booked, put_resources);
        Ok(())
    })?;
    put_route_option(w, spec.route)?;
    // Joint plan step 14: its priority band follows the route, last (C5).
    w.reserve_fixed(1)?;
    w.u8(spec.band);
    Ok(())
}

fn box_spec(r: &mut Reader<'_>) -> Result<BoxSpec, Fault> {
    let id = box_id(r)?;
    let bounds = bounds(r)?;
    let class = match r.u8()? {
        0 => ClassSpec::Process,
        1 => ClassSpec::Oci(Box::new(oci(r)?)),
        2 => ClassSpec::Bundle(r.path()?),
        3 => ClassSpec::Firecracker(Box::new(oci(r)?)),
        4 => ClassSpec::Shards(Box::new(oci(r)?)),
        5 => ClassSpec::ShardsImage(Box::new(shards_image(r)?)),
        t => return Err(Fault::value("class spec", format!("variant {t}"))),
    };
    let owner = r.option(|r| {
        let name = r.string("owner.name")?;
        let reuse = r.bool("owner.reuse")?;
        Ok(BoxOwner { name, reuse })
    })?;
    let spec = BoxSpec {
        id,
        bounds,
        class,
        owner,
        devices: r.device_list(9, device_id)?,
        allocation: r.option(|r| {
            Ok(Allocation {
                resources: resources(r)?,
                cost: runtime_cost(r)?,
                booked: r.option(resources)?,
            })
        })?,
        route: r.option(route_receipt)?,
        band: r.u8()?,
    };
    check_spec_route(&spec)?;
    spec.check_devices()
        .map_err(|e| Fault::value("devices", e.to_string()))?;
    if let Some(allocation) = spec.allocation {
        allocation
            .covers(workload_caps(&spec))
            .map_err(|e| Fault::value("allocation", e.to_string()))?;
    }
    Ok(spec)
}

// AC-T7.4.1: fixed native integer widths; new optional fields follow the complete old device tail.
const RESOURCE_BYTES: usize = 3 * size_of::<u64>();
const COST_BYTES: usize = 32 + 5 * RESOURCE_BYTES;
const OPTIONAL_COST_BYTES: usize = 1 + COST_BYTES;
// Joint plan step 14: what was booked follows the cost, an option of its own.
const OPTIONAL_ALLOCATION_BYTES: usize = 1 + RESOURCE_BYTES + COST_BYTES + 1 + RESOURCE_BYTES;

// Append-only route/backing widths: five enum bytes per key; opaque pool/generation bytes;
// optional tags; and a u32 device count followed by the existing nine-byte DeviceId codec.
const ROUTE_KEY_BYTES: usize = 5;
const BACKING_KEY_BYTES: usize = 32 + 16;
const ROUTE_OFFER_MIN_BYTES: usize = ROUTE_KEY_BYTES + 2;
/// A backing offer's guest at its least (joint plan step 14, C7): its three dimensions none, a byte each.
const BACKING_OFFER_GUEST_MIN_BYTES: usize = 3;
/// A backing offer at its least: its key, cost and phase, its claims' count, then its guest at its least.
const BACKING_OFFER_MIN_BYTES: usize =
    BACKING_KEY_BYTES + COST_BYTES + 1 + size_of::<u32>() + BACKING_OFFER_GUEST_MIN_BYTES;
const BACKING_GRANT_MIN_BYTES: usize =
    BACKING_KEY_BYTES + RESOURCE_BYTES + COST_BYTES + size_of::<u32>();

fn check_capacity(capacity: &Capacity) -> Result<(), Fault> {
    capacity
        .check()
        .map_err(|e| Fault::value("capacity", e.to_string()))
}

fn check_spec_route(spec: &BoxSpec) -> Result<(), Fault> {
    // Legacy None retains the old codec's validation. A chosen route must also satisfy the
    // original form, physical claims and finite dedicated allocation in the public API.
    if spec.route.is_some() {
        spec.check()
            .map_err(|_| Fault::value("route", "invalid original workload route or allocation"))?;
    }
    Ok(())
}

fn check_info_route(info: &BoxInfo) -> Result<(), Fault> {
    if let Some(route) = info.route {
        // BoxInfo carries no device claims. Check only its declared class/route relationship;
        // Required here validates that mode's shape, and is not evidence of a selected GPU.
        route
            .check(info.class, route.key.gpu == GpuEligibility::Required)
            .map_err(|_| Fault::value("route", "invalid factual workload route"))?;
    }
    Ok(())
}

fn put_route_key(w: &mut Writer, key: RouteKey) -> Result<(), Fault> {
    key.check()
        .map_err(|_| Fault::value("route", "inconsistent route key"))?;
    w.reserve_fixed(ROUTE_KEY_BYTES)?;
    w.u8(class_tag(key.class));
    w.u8(match key.image.os {
        ImageOs::Linux => 0,
        ImageOs::Windows => 1,
        ImageOs::Macos => 2,
    });
    w.u8(match key.image.arch {
        ImageArch::Amd64 => 0,
        ImageArch::Arm64 => 1,
    });
    w.u8(match key.execution {
        Execution::Native => 0,
        Execution::ShardsShared => 1,
        Execution::ShardsDedicated => 2,
        Execution::Firecracker => 3,
        Execution::NodeVm => 4,
    });
    w.u8(match key.gpu {
        GpuEligibility::Any => 0,
        GpuEligibility::None => 1,
        GpuEligibility::Required => 2,
    });
    Ok(())
}

fn route_key(r: &mut Reader<'_>) -> Result<RouteKey, Fault> {
    let class = class(r).map_err(|fault| match fault {
        Fault::Value { .. } => Fault::value("route.class", "unsupported tag"),
        other => other,
    })?;
    let os = match r.u8()? {
        0 => ImageOs::Linux,
        1 => ImageOs::Windows,
        2 => ImageOs::Macos,
        _ => return Err(Fault::value("route.os", "unsupported tag")),
    };
    let arch = match r.u8()? {
        0 => ImageArch::Amd64,
        1 => ImageArch::Arm64,
        _ => return Err(Fault::value("route.arch", "unsupported tag")),
    };
    let execution = match r.u8()? {
        0 => Execution::Native,
        1 => Execution::ShardsShared,
        2 => Execution::ShardsDedicated,
        3 => Execution::Firecracker,
        4 => Execution::NodeVm,
        _ => return Err(Fault::value("route.execution", "unsupported tag")),
    };
    let gpu = match r.u8()? {
        0 => GpuEligibility::Any,
        1 => GpuEligibility::None,
        2 => GpuEligibility::Required,
        _ => return Err(Fault::value("route.gpu", "unsupported tag")),
    };
    let key = RouteKey {
        class,
        image: ImagePlatform { os, arch },
        execution,
        gpu,
    };
    key.check()
        .map_err(|_| Fault::value("route", "inconsistent route key"))?;
    Ok(key)
}

fn put_backing_key(w: &mut Writer, key: BackingKey) -> Result<(), Fault> {
    w.reserve_fixed(BACKING_KEY_BYTES)?;
    w.raw(&key.pool);
    w.raw(&key.generation);
    Ok(())
}

fn backing_key(r: &mut Reader<'_>) -> Result<BackingKey, Fault> {
    Ok(BackingKey {
        pool: r.array()?,
        generation: r.array()?,
    })
}

fn put_backing_option(w: &mut Writer, key: Option<BackingKey>) -> Result<(), Fault> {
    let bytes = key
        .map_or(0, |_| BACKING_KEY_BYTES)
        .checked_add(1)
        .ok_or(Fault::Oversized {
            bytes: usize::MAX,
            bound: w.bound,
        })?;
    w.reserve_fixed(bytes)?;
    w.option_with(key, put_backing_key)
}

fn put_route_option(w: &mut Writer, route: Option<RouteReceipt>) -> Result<(), Fault> {
    let bytes = match route {
        Some(route) => ROUTE_KEY_BYTES
            .checked_add(1)
            .and_then(|bytes| bytes.checked_add(route.backing.map_or(0, |_| BACKING_KEY_BYTES))),
        None => Some(0),
    }
    .and_then(|bytes| bytes.checked_add(1))
    .ok_or(Fault::Oversized {
        bytes: usize::MAX,
        bound: w.bound,
    })?;
    w.reserve_fixed(bytes)?;
    w.option_with(route, |w, route| {
        route
            .check(route.key.class, route.key.gpu == GpuEligibility::Required)
            .map_err(|_| Fault::value("route", "inconsistent route receipt"))?;
        put_route_key(w, route.key)?;
        put_backing_option(w, route.backing)
    })
}

fn route_receipt(r: &mut Reader<'_>) -> Result<RouteReceipt, Fault> {
    let route = RouteReceipt {
        key: route_key(r)?,
        backing: r.option(backing_key)?,
    };
    route
        .check(route.key.class, route.key.gpu == GpuEligibility::Required)
        .map_err(|_| Fault::value("route", "inconsistent route receipt"))?;
    Ok(route)
}

fn put_route_offer(w: &mut Writer, route: RouteOffer) -> Result<(), Fault> {
    route
        .check()
        .map_err(|_| Fault::value("route", "inconsistent initialized route"))?;
    let bytes = ROUTE_OFFER_MIN_BYTES
        .checked_add(route.cost.map_or(0, |_| COST_BYTES))
        .and_then(|bytes| bytes.checked_add(route.backing.map_or(0, |_| BACKING_KEY_BYTES)))
        .ok_or(Fault::Oversized {
            bytes: usize::MAX,
            bound: w.bound,
        })?;
    w.reserve_fixed(bytes)?;
    put_route_key(w, route.key)?;
    put_cost_option(w, route.cost)?;
    put_backing_option(w, route.backing)
}

fn route_offer(r: &mut Reader<'_>) -> Result<RouteOffer, Fault> {
    let route = RouteOffer {
        key: route_key(r)?,
        cost: r.option(runtime_cost)?,
        backing: r.option(backing_key)?,
    };
    route
        .check()
        .map_err(|_| Fault::value("route", "inconsistent initialized route"))?;
    Ok(route)
}

fn provisioning_bytes(count: usize, prefix: usize) -> Result<usize, Fault> {
    count
        .checked_mul(9)
        .and_then(|bytes| prefix.checked_add(bytes))
        .ok_or_else(|| Fault::value("backing", "encoded physical claims overflow"))
}

fn put_provisioning(w: &mut Writer, devices: &[DeviceId]) -> Result<(), Fault> {
    w.device_count(devices.len(), 9)?;
    for id in devices {
        put_device_id(w, id);
    }
    Ok(())
}

fn put_backing_offer(w: &mut Writer, offer: &BackingOffer) -> Result<(), Fault> {
    offer
        .allocation()
        .map_err(|_| Fault::value("backing", "invalid fixed profile or physical claims"))?;
    w.reserve_fixed(provisioning_bytes(
        offer.devices.len(),
        BACKING_OFFER_MIN_BYTES,
    )?)?;
    put_backing_key(w, offer.key)?;
    put_cost(w, offer.cost)?;
    w.u8(match offer.phase {
        BackingPhase::Prepared => 0,
        BackingPhase::Granted => 1,
        BackingPhase::Live => 2,
        BackingPhase::Draining => 3,
        BackingPhase::Released => 4,
    });
    put_provisioning(w, &offer.devices)?;
    // Its guest's own overhead as its runtime measured it, each dimension none while cold (joint plan step 14, C7).
    let guest = offer.guest;
    for dim in [guest.cpu_milli, guest.mem_bytes, guest.disk_bytes] {
        w.option(dim, Writer::u64);
    }
    Ok(())
}

fn backing_offer(r: &mut Reader<'_>) -> Result<BackingOffer, Fault> {
    let key = backing_key(r)?;
    let cost = runtime_cost(r)?;
    let phase = match r.u8()? {
        0 => BackingPhase::Prepared,
        1 => BackingPhase::Granted,
        2 => BackingPhase::Live,
        3 => BackingPhase::Draining,
        4 => BackingPhase::Released,
        _ => return Err(Fault::value("backing.phase", "unsupported tag")),
    };
    let devices = r.device_list(9, device_id)?;
    let offer = BackingOffer {
        key,
        cost,
        phase,
        devices,
        guest: LearnedOverhead {
            cpu_milli: r.option(Reader::u64)?,
            mem_bytes: r.option(Reader::u64)?,
            disk_bytes: r.option(Reader::u64)?,
        },
    };
    offer
        .allocation()
        .map_err(|_| Fault::value("backing", "invalid fixed profile or physical claims"))?;
    Ok(offer)
}

fn put_backing_grant(w: &mut Writer, grant: &BackingGrant) -> Result<(), Fault> {
    grant
        .check()
        .map_err(|_| Fault::value("backing", "invalid fixed grant or physical claims"))?;
    w.reserve_fixed(provisioning_bytes(
        grant.devices.len(),
        BACKING_GRANT_MIN_BYTES,
    )?)?;
    put_backing_key(w, grant.key)?;
    put_resources(w, grant.allocation.resources);
    put_cost(w, grant.allocation.cost)?;
    put_provisioning(w, &grant.devices)
}

fn backing_grant(r: &mut Reader<'_>) -> Result<BackingGrant, Fault> {
    let grant = BackingGrant {
        key: backing_key(r)?,
        allocation: Allocation {
            resources: resources(r)?,
            cost: runtime_cost(r)?,
            booked: None,
        },
        devices: r.device_list(9, device_id)?,
    };
    grant
        .check()
        .map_err(|_| Fault::value("backing", "invalid fixed grant or physical claims"))?;
    Ok(grant)
}

/// A device a capacity won't offer: its kind and key, then its reason's length.
const UNAVAILABLE_MIN_BYTES: usize = size_of::<u8>() + size_of::<u64>() + size_of::<u32>();

/// The devices a capacity lists but won't offer, each with why: after every other field of it (AC-T7.4.1).
fn put_unavailable_devices(w: &mut Writer, unavailable: &[DeviceUnavailable]) -> Result<(), Fault> {
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(unavailable.len(), UNAVAILABLE_MIN_BYTES)?;
    for device in unavailable {
        put_device_id(w, &device.id);
        w.string(&device.why)?;
    }
    Ok(())
}

/// One learned prior: its profile's id, its ceiling and its booking (joint plan step 14).
const LEARNED_PRIOR_BYTES: usize = 32 + 2 * RESOURCE_BYTES;
/// A learned use: what the receipts booked and what their VMs are learned to take.
const LEARNED_USE_BYTES: usize = 2 * RESOURCE_BYTES;
/// A holder at its least ([`put_holder`]): its tag, then a one-byte box id with its length.
const HOLDER_MIN_BYTES: usize = 1 + size_of::<u32>() + 1;
/// One learned holding at its least: its holder at its least, then its resources.
const LEARNED_HOLDING_MIN_BYTES: usize = HOLDER_MIN_BYTES + RESOURCE_BYTES;
/// One learned end past its box id, at its most: what the VM held and its budget, its band, its fleet's held, growth
/// and budget, then its unmoved capability's option tag and that capability's tag.
const LEARNED_END_BYTES: usize = 5 * size_of::<u64>() + 1 + 2;
/// One learned end at its least: its holder at its least, then the rest with no unmoved capability.
const LEARNED_END_MIN_BYTES: usize = HOLDER_MIN_BYTES + LEARNED_END_BYTES - 1;

/// One learned ceiling at its least (C1): its profile's id, its envelope, and each dimension of its overhead none.
const LEARNED_CEILING_MIN_BYTES: usize = 32 + RESOURCE_BYTES + 3;
/// One learned ceiling at its most: each dimension of its overhead learned.
const LEARNED_CEILING_BYTES: usize = 32 + RESOURCE_BYTES + 3 * (1 + size_of::<u64>());
/// One learned rate at its least (D255): its holder at its least, then what holds it with no reason.
const LEARNED_RATE_MIN_BYTES: usize = HOLDER_MIN_BYTES + 1;
/// A learned rate's VMM own taking its share (D260): its tag, then its own and its share.
const OWN_TAKES_SHARE_BYTES: usize = 1 + 2 * size_of::<u64>();
/// One VM a learner couldn't read at its least (place 4): its holder at its least, then its reason's length.
const LEARNED_UNREAD_MIN_BYTES: usize = HOLDER_MIN_BYTES + size_of::<u32>();
/// One guest's room's await past its holder (place 4): its capability's tag.
const LEARNED_AWAIT_BYTES: usize = 1;
/// One guest's room's await at its least: its holder at its least, then its capability's tag.
const LEARNED_AWAIT_MIN_BYTES: usize = HOLDER_MIN_BYTES + LEARNED_AWAIT_BYTES;
/// One node VM ranked above every box at its least (place 4): its run's 16 bytes, then its reason's length.
const LEARNED_RANK_MIN_BYTES: usize = 16 + size_of::<u32>();
/// One box whose record can't be read at its least (the review's dropped records): its holder at its least, then its
/// reason's length.
const LEARNED_UNRECORDED_MIN_BYTES: usize = HOLDER_MIN_BYTES + size_of::<u32>();
/// One VM its window couldn't cap or tell its room at its least (A-7): its holder at its least, then its reason's
/// length.
const LEARNED_UNACTED_MIN_BYTES: usize = HOLDER_MIN_BYTES + size_of::<u32>();

/// Joint plan step 14 (C8): keeps as many of `capacity`'s learned holdings, in their order, as its answer has room
/// for within [`MAX_MESSAGE`], so a node's capacity answer is never refused for them. The node orders them most past
/// their receipts first, and a holding left out counts at its receipt: only those least past theirs are dropped.
/// Then, in the room the holdings leave, as many of its ends as fit (C5), in their order: the node puts the latest
/// first, so only the oldest are left out.
pub fn fit_learned(capacity: &mut Capacity) -> Result<(), Fault> {
    let Some(learned) = capacity.learned.as_mut() else {
        return Ok(());
    };
    let holdings = std::mem::take(&mut learned.holdings);
    let ended = std::mem::take(&mut learned.ended);
    let rates = std::mem::take(&mut learned.rates);
    let unread = std::mem::take(&mut learned.unread);
    let awaits = std::mem::take(&mut learned.awaits);
    let ranked = std::mem::take(&mut learned.ranked);
    let unrecorded = std::mem::take(&mut learned.unrecorded);
    let unacted = std::mem::take(&mut learned.unacted);
    let probe = capacity
        .try_clone()
        .map_err(|e| Fault::value("capacity", e.to_string()))?;
    let base = answer(0, &Answer::Capacity(Some(probe)), MAX_MESSAGE)?.len();
    let mut room = MAX_MESSAGE.saturating_sub(base);
    let mut kept = Vec::new();
    kept.try_reserve_exact(holdings.len())
        .map_err(|_| Fault::Storage {
            what: "learned holdings",
            bound: holdings.len(),
        })?;
    for holding in holdings {
        let bytes = holder_bytes(&holding.id).saturating_add(RESOURCE_BYTES);
        let Some(left) = room.checked_sub(bytes) else {
            break;
        };
        room = left;
        kept.push(holding);
    }
    let mut ends = Vec::new();
    ends.try_reserve_exact(ended.len())
        .map_err(|_| Fault::Storage {
            what: "learned ends",
            bound: ended.len(),
        })?;
    for end in ended {
        let bytes = holder_bytes(&end.id).saturating_add(LEARNED_END_BYTES);
        let Some(left) = room.checked_sub(bytes) else {
            break;
        };
        room = left;
        ends.push(end);
    }
    // Then, in the room the ends leave, as many of its rates as fit (D255), in their order: the node puts those its
    // governor doesn't hold alone first.
    let mut told = Vec::new();
    told.try_reserve_exact(rates.len())
        .map_err(|_| Fault::Storage {
            what: "learned rates",
            bound: rates.len(),
        })?;
    for rate in rates {
        let Some(left) = room.checked_sub(learned_rate_bytes(&rate)) else {
            break;
        };
        room = left;
        told.push(rate);
    }
    // Then, in the room the rates leave, as many of the VMs it couldn't read, then of the rooms' awaits, then of the
    // node VMs it ranks above every box, as fit (place 4), then of the boxes whose records can't be read (the review's
    // dropped records), then of the VMs its window couldn't cap or tell their room (A-7), each list in its order.
    let unread = fitted(unread, &mut room, "learned unread VMs", |unread| {
        holder_bytes(&unread.id).saturating_add(words_bytes(&unread.why))
    })?;
    let awaits = fitted(awaits, &mut room, "learned awaits", |awaited| {
        holder_bytes(&awaited.id).saturating_add(LEARNED_AWAIT_BYTES)
    })?;
    let ranked = fitted(ranked, &mut room, "learned ranked VMs", |ranked| {
        16usize.saturating_add(words_bytes(&ranked.why))
    })?;
    let unrecorded = fitted(unrecorded, &mut room, "learned unrecorded boxes", |told| {
        holder_bytes(&told.id).saturating_add(words_bytes(&told.why))
    })?;
    let unacted = fitted(unacted, &mut room, "learned unacted VMs", |told| {
        holder_bytes(&told.id).saturating_add(words_bytes(&told.why))
    })?;
    if let Some(learned) = capacity.learned.as_mut() {
        learned.holdings = kept;
        learned.ended = ends;
        learned.rates = told;
        learned.unread = unread;
        learned.awaits = awaits;
        learned.ranked = ranked;
        learned.unrecorded = unrecorded;
        learned.unacted = unacted;
    }
    Ok(())
}

/// As many of `values`, in their order, as fit `room`, each `bytes` of it, taken from `room`: the first that doesn't
/// fit, and every one after it, left out (place 4).
fn fitted<T>(
    values: Vec<T>,
    room: &mut usize,
    what: &'static str,
    bytes: impl Fn(&T) -> usize,
) -> Result<Vec<T>, Fault> {
    let mut kept = Vec::new();
    kept.try_reserve_exact(values.len())
        .map_err(|_| Fault::Storage {
            what,
            bound: values.len(),
        })?;
    for value in values {
        let Some(left) = room.checked_sub(bytes(&value)) else {
            break;
        };
        *room = left;
        kept.push(value);
    }
    Ok(kept)
}

/// What [`Writer::string`] writes of `words`: their length, then their bytes.
fn words_bytes(words: &str) -> usize {
    size_of::<u32>().saturating_add(words.len())
}

/// What a node learned of a class's microVMs (joint plan step 14): its priors, then its learned use.
/// `Capacity::check` bounded its priors before this point.
fn put_learned(w: &mut Writer, learned: &Learned) -> Result<(), Fault> {
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.priors.len(), LEARNED_PRIOR_BYTES)?;
    for prior in &learned.priors {
        w.reserve_fixed(LEARNED_PRIOR_BYTES)?;
        w.raw(&prior.profile);
        put_resources(w, prior.ceiling);
        put_resources(w, prior.booking);
    }
    w.reserve_fixed(LEARNED_USE_BYTES)?;
    put_resources(w, learned.used.booked);
    put_resources(w, learned.used.reservation);
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.holdings.len(), LEARNED_HOLDING_MIN_BYTES)?;
    for holding in &learned.holdings {
        put_holder(w, &holding.id)?;
        w.reserve_fixed(RESOURCE_BYTES)?;
        put_resources(w, holding.holding);
    }
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.ended.len(), LEARNED_END_MIN_BYTES)?;
    for end in &learned.ended {
        put_learned_end(w, end)?;
    }
    // Last (C1): each profile's ceiling, its envelope and the overhead learned past it, a dimension none while cold.
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.ceilings.len(), LEARNED_CEILING_MIN_BYTES)?;
    for ceiling in &learned.ceilings {
        w.reserve_fixed(LEARNED_CEILING_BYTES)?;
        w.raw(&ceiling.profile);
        put_resources(w, ceiling.envelope);
        let overhead = ceiling.overhead;
        for dim in [overhead.cpu_milli, overhead.mem_bytes, overhead.disk_bytes] {
            w.option(dim, Writer::u64);
        }
    }
    // Then (D255): what holds each managed VM's rate, after the ceilings.
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.rates.len(), LEARNED_RATE_MIN_BYTES)?;
    for rate in &learned.rates {
        put_learned_rate(w, rate)?;
    }
    // Then (C1; ROOT, 2026-11-13): the learner's window and the VM whose cap's cycle sets it, where one does.
    w.reserve_fixed(1)?;
    w.option_with(learned.window.as_ref(), |w, window| {
        w.reserve_fixed(size_of::<u64>().saturating_add(1))?;
        w.u64(window.window_ns);
        w.option_with(window.slowed_by.as_ref(), put_holder)
    })?;
    // Then (place 4; ROOT, 2026-11-13): each VM its window couldn't read and why, each guest's room's await, and each
    // node VM it ranks above every box and why.
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.unread.len(), LEARNED_UNREAD_MIN_BYTES)?;
    for unread in &learned.unread {
        put_holder(w, &unread.id)?;
        w.string(&unread.why)?;
    }
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.awaits.len(), LEARNED_AWAIT_MIN_BYTES)?;
    for awaited in &learned.awaits {
        put_holder(w, &awaited.id)?;
        w.reserve_fixed(LEARNED_AWAIT_BYTES)?;
        put_upstream(w, awaited.needs)?;
    }
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.ranked.len(), LEARNED_RANK_MIN_BYTES)?;
    for ranked in &learned.ranked {
        w.reserve_fixed(16)?;
        w.raw(ranked.run.bytes());
        w.string(&ranked.why)?;
    }
    // Then (the review's dropped records; ROOT, 2026-11-14): each box whose record can't be read and why.
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.unrecorded.len(), LEARNED_UNRECORDED_MIN_BYTES)?;
    for told in &learned.unrecorded {
        put_holder(w, &told.id)?;
        w.string(&told.why)?;
    }
    // Last (A-7; ROOT, 2026-11-14): each VM its window couldn't cap or tell its room, held at what was last set, and
    // why.
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(learned.unacted.len(), LEARNED_UNACTED_MIN_BYTES)?;
    for told in &learned.unacted {
        put_holder(w, &told.id)?;
        w.string(&told.why)?;
    }
    // Last (the review's A-10; ROOT, 2026-11-14): what the learner takes off its fleet's budget for the capacity view's
    // holders, where told: 0 none served, 1 unheard, 2 heard, its keeper, what is reserved and whether unconfigured.
    w.reserve_fixed(1)?;
    w.option_with(learned.reservations.as_ref(), |w, reservations| {
        w.reserve_fixed(1)?;
        match reservations {
            LearnedReservations::Unserved => w.u8(0),
            LearnedReservations::Unheard => w.u8(1),
            LearnedReservations::Heard {
                keeper,
                reserved,
                unconfigured,
            } => {
                w.u8(2);
                w.reserve_fixed(
                    size_of::<u64>()
                        .saturating_add(RESOURCE_BYTES)
                        .saturating_add(1),
                )?;
                w.u64(*keeper);
                put_resources(w, *reserved);
                w.u8(u8::from(*unconfigured));
            }
        }
        Ok(())
    })
}

/// One learned rate (D255, D260): its holder, then 0 its governor, 1 its governor under its job, 2 and why its
/// platform's cap, 3 and why its governor with its VMM's own unheld, or 4 and its VMM's own and its share, its VMM's own
/// taking its share.
fn put_learned_rate(w: &mut Writer, rate: &LearnedRate) -> Result<(), Fault> {
    put_holder(w, &rate.id)?;
    w.reserve_fixed(1)?;
    match &rate.held {
        RateHeld::Governor => {
            w.u8(0);
            Ok(())
        }
        RateHeld::GovernorUnderJob => {
            w.u8(1);
            Ok(())
        }
        RateHeld::Platform(why) => {
            w.u8(2);
            w.string(why)
        }
        RateHeld::GovernorOwnUnheld(why) => {
            w.u8(3);
            w.string(why)
        }
        RateHeld::OwnTakesShare { own, share } => {
            w.reserve_fixed(OWN_TAKES_SHARE_BYTES)?;
            w.u8(4);
            w.u64(*own);
            w.u64(*share);
            Ok(())
        }
    }
}

/// What [`put_learned_rate`] writes of `rate`.
fn learned_rate_bytes(rate: &LearnedRate) -> usize {
    let held = match &rate.held {
        RateHeld::Governor | RateHeld::GovernorUnderJob => 1,
        RateHeld::Platform(why) | RateHeld::GovernorOwnUnheld(why) => 1usize
            .saturating_add(size_of::<u32>())
            .saturating_add(why.len()),
        RateHeld::OwnTakesShare { .. } => OWN_TAKES_SHARE_BYTES,
    };
    holder_bytes(&rate.id).saturating_add(held)
}

/// The learned rate [`put_learned_rate`] wrote.
fn learned_rate(r: &mut Reader<'_>) -> Result<LearnedRate, Fault> {
    let id = holder(r)?;
    let held = match r.u8()? {
        0 => RateHeld::Governor,
        1 => RateHeld::GovernorUnderJob,
        2 => RateHeld::Platform(r.string("a managed VM's rate's reason")?),
        3 => RateHeld::GovernorOwnUnheld(r.string("a managed VM's rate's reason")?),
        4 => RateHeld::OwnTakesShare {
            own: r.u64()?,
            share: r.u64()?,
        },
        tag => {
            return Err(Fault::value(
                "a learned rate",
                format!("tag {tag} names nothing that holds a rate"),
            ));
        }
    };
    Ok(LearnedRate { id, held })
}

/// A holder of a node's room (joint plan step 39): its tag, then its id: 0 and a box's id, or 1 and a node VM's run,
/// its 16 bytes.
fn put_holder(w: &mut Writer, holder: &LearnedHolder) -> Result<(), Fault> {
    match holder {
        LearnedHolder::Box(id) => {
            w.reserve_fixed(1)?;
            w.u8(0);
            w.string(id.as_str())
        }
        LearnedHolder::Node(run) => {
            w.reserve_fixed(1 + 16)?;
            w.u8(1);
            w.raw(run.bytes());
            Ok(())
        }
    }
}

/// What [`put_holder`] writes of `holder`: its tag, then a box's id with its length, or a node VM's run.
fn holder_bytes(holder: &LearnedHolder) -> usize {
    match holder {
        LearnedHolder::Box(id) => 1usize
            .saturating_add(size_of::<u32>())
            .saturating_add(id.as_str().len()),
        LearnedHolder::Node(_) => 1 + 16,
    }
}

/// One node VM's receipt (joint plan step 39): its run, its booking, its phase's tag, then its guest's room (V2).
const NODE_VM_RECEIPT_BYTES: usize = 16 + RESOURCE_BYTES + 1 + RESOURCE_BYTES;

/// A node VM's receipt: its run's 16 bytes, its booking, its phase, 0 Booting, 1 Ready and 2 Ended, then what its guest
/// is admitted at (V2).
fn put_node_vm_receipt(w: &mut Writer, receipt: &NodeVmReceipt) -> Result<(), Fault> {
    w.reserve_fixed(NODE_VM_RECEIPT_BYTES)?;
    w.raw(receipt.run.bytes());
    put_resources(w, receipt.booking);
    w.u8(match receipt.phase {
        NodeVmPhase::Booting => 0,
        NodeVmPhase::Ready => 1,
        NodeVmPhase::Ended => 2,
    });
    put_resources(w, receipt.guest);
    Ok(())
}

/// The receipt [`put_node_vm_receipt`] wrote.
fn node_vm_receipt(r: &mut Reader<'_>) -> Result<NodeVmReceipt, Fault> {
    Ok(NodeVmReceipt {
        run: hex_api::NodeRun::from_bytes(r.array::<16>()?),
        booking: resources(r)?,
        phase: match r.u8()? {
            0 => NodeVmPhase::Booting,
            1 => NodeVmPhase::Ready,
            2 => NodeVmPhase::Ended,
            tag => {
                return Err(Fault::value(
                    "a node VM's phase",
                    format!("tag {tag} names no phase"),
                ));
            }
        },
        guest: resources(r)?,
    })
}

/// The holder [`put_holder`] wrote.
fn holder(r: &mut Reader<'_>) -> Result<LearnedHolder, Fault> {
    match r.u8()? {
        0 => Ok(LearnedHolder::Box(box_id(r)?)),
        1 => Ok(LearnedHolder::Node(hex_api::NodeRun::from_bytes(
            r.array::<16>()?,
        ))),
        tag => Err(Fault::value(
            "a learned holder",
            format!("tag {tag} names no holder"),
        )),
    }
}

/// One learned end (C5): its holder, then what the VM held and its budget, its band, its fleet's held, growth and
/// budget, and what moving it instead awaited.
fn put_learned_end(w: &mut Writer, end: &LearnedEnd) -> Result<(), Fault> {
    put_holder(w, &end.id)?;
    w.reserve_fixed(LEARNED_END_BYTES)?;
    w.u64(end.held);
    w.u64(end.budget);
    w.u8(end.band);
    w.u64(end.fleet_held);
    w.u64(end.fleet_growth);
    w.u64(end.fleet_budget);
    w.option_with(end.unmoved, put_upstream)
}

fn learned_end(r: &mut Reader<'_>) -> Result<LearnedEnd, Fault> {
    Ok(LearnedEnd {
        id: holder(r)?,
        held: r.u64()?,
        budget: r.u64()?,
        band: r.u8()?,
        fleet_held: r.u64()?,
        fleet_growth: r.u64()?,
        fleet_budget: r.u64()?,
        unmoved: r.option(upstream)?,
    })
}

fn learned(r: &mut Reader<'_>) -> Result<Learned, Fault> {
    let priors = r.bounded_list(
        LEARNED_PRIOR_BYTES,
        MAX_LEARNED_PRIORS,
        "learned priors",
        |r| {
            Ok(LearnedPrior {
                profile: r.array::<32>()?,
                ceiling: resources(r)?,
                booking: resources(r)?,
            })
        },
    )?;
    let used = LearnedUse {
        booked: resources(r)?,
        reservation: resources(r)?,
    };
    let holdings = r.bounded_list(
        LEARNED_HOLDING_MIN_BYTES,
        MAX_LEARNED_HOLDINGS,
        "learned holdings",
        |r| {
            Ok(LearnedHolding {
                id: holder(r)?,
                holding: resources(r)?,
            })
        },
    )?;
    let ended = r.bounded_list(
        LEARNED_END_MIN_BYTES,
        MAX_LEARNED_HOLDINGS,
        "learned ends",
        learned_end,
    )?;
    let ceilings = r.bounded_list(
        LEARNED_CEILING_MIN_BYTES,
        MAX_LEARNED_PRIORS,
        "learned ceilings",
        |r| {
            Ok(LearnedCeiling {
                profile: r.array::<32>()?,
                envelope: resources(r)?,
                overhead: LearnedOverhead {
                    cpu_milli: r.option(Reader::u64)?,
                    mem_bytes: r.option(Reader::u64)?,
                    disk_bytes: r.option(Reader::u64)?,
                },
            })
        },
    )?;
    let rates = r.bounded_list(
        LEARNED_RATE_MIN_BYTES,
        MAX_LEARNED_HOLDINGS,
        "learned rates",
        learned_rate,
    )?;
    let window = r.option(|r| {
        Ok(LearnedWindow {
            window_ns: r.u64()?,
            slowed_by: r.option(holder)?,
        })
    })?;
    Ok(Learned {
        priors,
        used,
        holdings,
        ended,
        ceilings,
        rates,
        window,
        unread: r.bounded_list(
            LEARNED_UNREAD_MIN_BYTES,
            MAX_LEARNED_HOLDINGS,
            "learned unread VMs",
            |r| {
                Ok(LearnedUnread {
                    id: holder(r)?,
                    why: r.string("why a learner couldn't read a VM")?,
                })
            },
        )?,
        awaits: r.bounded_list(
            LEARNED_AWAIT_MIN_BYTES,
            MAX_LEARNED_HOLDINGS,
            "learned awaits",
            |r| {
                Ok(LearnedAwait {
                    id: holder(r)?,
                    needs: upstream(r)?,
                })
            },
        )?,
        ranked: r.bounded_list(
            LEARNED_RANK_MIN_BYTES,
            MAX_LEARNED_HOLDINGS,
            "learned ranked VMs",
            |r| {
                Ok(LearnedRank {
                    run: hex_api::NodeRun::from_bytes(r.array::<16>()?),
                    why: r.string("why a node VM ranks above every box")?,
                })
            },
        )?,
        unrecorded: r.bounded_list(
            LEARNED_UNRECORDED_MIN_BYTES,
            MAX_LEARNED_HOLDINGS,
            "learned unrecorded boxes",
            |r| {
                Ok(LearnedUnrecorded {
                    id: holder(r)?,
                    why: r.string("why a node couldn't read a box's record")?,
                })
            },
        )?,
        unacted: r.bounded_list(
            LEARNED_UNACTED_MIN_BYTES,
            MAX_LEARNED_HOLDINGS,
            "learned unacted VMs",
            |r| {
                Ok(LearnedUnacted {
                    id: holder(r)?,
                    why: r.string("why a window couldn't act on a VM")?,
                })
            },
        )?,
        reservations: r.option(learned_reservations)?,
    })
}

/// What a learner takes off its fleet's budget for the capacity view's holders ([`put_learned`]'s last field).
fn learned_reservations(r: &mut Reader<'_>) -> Result<LearnedReservations, Fault> {
    Ok(match r.u8()? {
        0 => LearnedReservations::Unserved,
        1 => LearnedReservations::Unheard,
        2 => LearnedReservations::Heard {
            keeper: r.u64()?,
            reserved: resources(r)?,
            unconfigured: match r.u8()? {
                0 => false,
                1 => true,
                other => {
                    return Err(Fault::value(
                        "learned reservations",
                        format!("{other} is neither of a flag's values"),
                    ));
                }
            },
        },
        other => {
            return Err(Fault::value(
                "learned reservations",
                format!("{other} names no state of kiln's reservations"),
            ));
        }
    })
}

fn put_capacity_routes(w: &mut Writer, capacity: &Capacity) -> Result<(), Fault> {
    // Capacity::check bounded both lists by the single u64 eligibility word before this point.
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(capacity.routes.len(), ROUTE_OFFER_MIN_BYTES)?;
    for route in &capacity.routes {
        put_route_offer(w, *route)?;
    }
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(capacity.device_routes.len(), size_of::<u64>())?;
    for mask in &capacity.device_routes {
        w.reserve_fixed(size_of::<u64>())?;
        w.u64(*mask);
    }
    w.reserve_fixed(size_of::<u32>())?;
    w.device_count(capacity.backings.len(), BACKING_OFFER_MIN_BYTES)?;
    for backing in &capacity.backings {
        put_backing_offer(w, backing)?;
    }
    Ok(())
}

// Device kind/key, model length, byte size and two optional-counter tags. Holder is absent by
// definition: this factual RPC cannot authorize a reservation through telemetry (AC-T7.4.1).
const PHYSICAL_MIN_BYTES: usize =
    size_of::<u8>() + size_of::<u64>() + size_of::<u32>() + size_of::<u64>() + 2 * size_of::<u8>();
const PHYSICAL_PREFIX_BYTES: usize = 2 * size_of::<u32>();
// Existing class, Refusal::Backing, fixed key and lifecycle-cause tags, then its reason's option tag (joint plan step
// 14): at its least, with no reason; one with a reason adds its length and bytes (`backing_why_bytes`).
const PHYSICAL_UNAVAILABLE_BYTES: usize = 4 * size_of::<u8>() + BACKING_KEY_BYTES;

/// What a Backing refusal's reason adds past its least: its length and its bytes, where it gives one.
fn backing_why_bytes(cause: &Refusal) -> Option<usize> {
    match cause {
        Refusal::Backing { why: Some(why), .. } => size_of::<u32>().checked_add(why.len()),
        _ => Some(0),
    }
}

fn put_physical_report(w: &mut Writer, report: &PhysicalReport) -> Result<(), Fault> {
    report
        .check()
        .map_err(|e| Fault::value("physical report", e.to_string()))?;
    let overflow = || Fault::value("physical report", "encoded snapshot size overflows");
    let mut bytes = PHYSICAL_PREFIX_BYTES
        .checked_add(report.authority.len())
        .ok_or_else(overflow)?;
    for device in &report.devices {
        bytes = bytes
            .checked_add(PHYSICAL_MIN_BYTES)
            .and_then(|n| n.checked_add(device.info.model.len()))
            .and_then(|n| {
                n.checked_add(if device.busy_per_mille.is_some() {
                    size_of::<u16>()
                } else {
                    0
                })
            })
            .and_then(|n| {
                n.checked_add(if device.memory_used_bytes.is_some() {
                    size_of::<u64>()
                } else {
                    0
                })
            })
            .ok_or_else(overflow)?;
    }
    if !report.unavailable.is_empty() {
        bytes = report
            .unavailable
            .len()
            .checked_mul(PHYSICAL_UNAVAILABLE_BYTES)
            .and_then(|tail| tail.checked_add(size_of::<u32>()))
            .and_then(|tail| bytes.checked_add(tail))
            .ok_or_else(overflow)?;
        for observation in &report.unavailable {
            bytes = backing_why_bytes(&observation.cause)
                .and_then(|why| bytes.checked_add(why))
                .ok_or_else(overflow)?;
        }
    }
    w.reserve_fixed(bytes)?;
    w.string(&report.authority)?;
    w.device_count(report.devices.len(), PHYSICAL_MIN_BYTES)?;
    for device in &report.devices {
        put_device_id(w, &device.info.id);
        w.string(&device.info.model)?;
        w.u64(device.info.size);
        w.option(device.busy_per_mille, |w, value| w.u16(value));
        w.option(device.memory_used_bytes, |w, value| w.u64(value));
    }
    // Omit the empty tail so a report without diagnostics retains its previous body bytes.
    if !report.unavailable.is_empty() {
        w.device_count(report.unavailable.len(), PHYSICAL_UNAVAILABLE_BYTES)?;
        for observation in &report.unavailable {
            w.u8(class_tag(observation.backend));
            put_refusal(w, &observation.cause)?;
        }
    }
    Ok(())
}

fn physical_report(r: &mut Reader<'_>) -> Result<PhysicalReport, Fault> {
    let authority = r.device_string("physical authority")?;
    let devices = r.device_list(PHYSICAL_MIN_BYTES, |r| {
        Ok(PhysicalDevice {
            info: DeviceInfo {
                id: device_id(r)?,
                model: r.device_string("physical device model")?,
                size: r.u64()?,
                holder: None,
            },
            busy_per_mille: r.option(|r| r.u16())?,
            memory_used_bytes: r.option(|r| r.u64())?,
        })
    })?;
    let unavailable = if r.rest.is_empty() {
        Vec::new()
    } else {
        r.bounded_list(
            PHYSICAL_UNAVAILABLE_BYTES,
            BackendUnavailable::MAX_COUNT,
            "physical report.unavailable",
            |r| {
                let backend = class(r)?;
                if !matches!(backend, Class::Firecracker | Class::Shards) {
                    return Err(Fault::value(
                        "physical report.unavailable",
                        "only VM backend classes may be unavailable",
                    ));
                }
                // Refuse every other refusal tag before its strings or static-name interner.
                // The accepted body is decoded by the same canonical refusal codec as answers.
                if r.rest.first() != Some(&10) {
                    return Err(Fault::value(
                        "physical report.unavailable",
                        "only qualified backing refusals may be unavailable",
                    ));
                }
                let observation = BackendUnavailable {
                    backend,
                    cause: refusal(r)?,
                };
                observation.check().map_err(|_| {
                    Fault::value(
                        "physical report.unavailable",
                        "invalid qualified backing observation",
                    )
                })?;
                Ok(observation)
            },
        )?
    };
    let report = PhysicalReport {
        authority,
        devices,
        unavailable,
    };
    report
        .check()
        .map_err(|e| Fault::value("physical report", e.to_string()))?;
    Ok(report)
}

fn workload_caps(spec: &BoxSpec) -> CapacityResources {
    CapacityResources {
        cpu_milli: spec.bounds.cpu_max_milli.map_or(0, u64::from),
        mem_bytes: spec.bounds.memory_max.unwrap_or(0),
        disk_bytes: 0,
    }
}

fn put_resources(w: &mut Writer, resources: CapacityResources) {
    w.u64(resources.cpu_milli);
    w.u64(resources.mem_bytes);
    w.u64(resources.disk_bytes);
}

fn resources(r: &mut Reader<'_>) -> Result<CapacityResources, Fault> {
    Ok(CapacityResources {
        cpu_milli: r.u64()?,
        mem_bytes: r.u64()?,
        disk_bytes: r.u64()?,
    })
}

fn put_cost_option(w: &mut Writer, cost: Option<RuntimeCost>) -> Result<(), Fault> {
    w.reserve_fixed(if cost.is_some() {
        OPTIONAL_COST_BYTES
    } else {
        1
    })?;
    w.option_with(cost, put_cost)
}

fn put_cost(w: &mut Writer, cost: RuntimeCost) -> Result<(), Fault> {
    cost.check()
        .map_err(|e| Fault::value("runtime cost", e.to_string()))?;
    w.reserve_fixed(COST_BYTES)?;
    w.raw(&cost.id);
    for resources in [
        cost.floor,
        cost.ceiling,
        cost.quantum,
        cost.guest_overhead,
        cost.overhead,
    ] {
        put_resources(w, resources);
    }
    Ok(())
}

fn runtime_cost(r: &mut Reader<'_>) -> Result<RuntimeCost, Fault> {
    let cost = RuntimeCost {
        id: r.array::<32>()?,
        floor: resources(r)?,
        ceiling: resources(r)?,
        quantum: resources(r)?,
        guest_overhead: resources(r)?,
        overhead: resources(r)?,
    };
    cost.check()
        .map_err(|e| Fault::value("runtime cost", e.to_string()))?;
    Ok(cost)
}

// Fixed tags independent of Rust discriminants; the catalog order is retained because scheduler
// indices refer to it. DeviceId is one tag plus eight key bytes, DeviceInfo at least 22 bytes
// (identity, model length, size and holder-option tag), before any model or holder bytes.
fn put_device_id(w: &mut Writer, id: &DeviceId) {
    w.u8(match id.kind {
        DeviceKind::Gpu => 0,
        DeviceKind::Vfio => 1,
        DeviceKind::Block => 2,
        DeviceKind::Net => 3,
    });
    w.u64(id.key);
}

fn device_id(r: &mut Reader<'_>) -> Result<DeviceId, Fault> {
    let kind = match r.u8()? {
        0 => DeviceKind::Gpu,
        1 => DeviceKind::Vfio,
        2 => DeviceKind::Block,
        3 => DeviceKind::Net,
        _ => return Err(Fault::value("devices.kind", "unsupported tag")),
    };
    let id = DeviceId {
        kind,
        key: r.u64()?,
    };
    id.check()
        .map_err(|e| Fault::value("devices.id", e.to_string()))?;
    Ok(id)
}

fn put_device_holder(w: &mut Writer, holder: Option<&DeviceHolder>) -> Result<(), Fault> {
    match holder {
        None => {
            w.reserve_fixed(1)?;
            w.u8(0);
            Ok(())
        }
        Some(DeviceHolder::Box(id)) => {
            let bytes = 1usize
                .checked_add(size_of::<u32>())
                .and_then(|n| n.checked_add(id.as_str().len()))
                .ok_or_else(|| Fault::value("devices.holder", "encoded box holder overflows"))?;
            w.reserve_fixed(bytes)?;
            w.u8(1);
            w.string(id.as_str())
        }
        Some(DeviceHolder::Backing(key)) => {
            w.reserve_fixed(1 + BACKING_KEY_BYTES)?;
            w.u8(2);
            put_backing_key(w, *key)
        }
    }
}

fn device_holder(r: &mut Reader<'_>) -> Result<Option<DeviceHolder>, Fault> {
    match r.u8()? {
        0 => Ok(None),
        1 => {
            let bytes = r.bytes()?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| Fault::value("devices.holder", "is not UTF-8"))?;
            let id = BoxId::new(text)
                .map_err(|_| Fault::value("devices.holder", "is not a valid box id"))?;
            Ok(Some(DeviceHolder::Box(id)))
        }
        2 => backing_key(r).map(|key| Some(DeviceHolder::Backing(key))),
        _ => Err(Fault::value("devices.holder", "unsupported holder tag")),
    }
}

fn put_bounds(w: &mut Writer, b: &Bounds) {
    w.option(b.cpu_weight, Writer::u16);
    w.option(b.cpu_max_milli, Writer::u32);
    w.option(b.memory_max, Writer::u64);
    w.option(b.memory_high, Writer::u64);
    w.option(b.io_weight, Writer::u16);
    w.option(b.io_latency_us, Writer::u32);
    w.option(b.io_max, |w, rates: IoRates| {
        for rate in [
            rates.read_bps,
            rates.write_bps,
            rates.read_iops,
            rates.write_iops,
        ] {
            w.option(rate, Writer::u64);
        }
    });
    w.option(b.pids_max, Writer::u32);
}

fn bounds(r: &mut Reader<'_>) -> Result<Bounds, Fault> {
    Ok(Bounds {
        cpu_weight: r.option(Reader::u16)?,
        cpu_max_milli: r.option(Reader::u32)?,
        memory_max: r.option(Reader::u64)?,
        memory_high: r.option(Reader::u64)?,
        io_weight: r.option(Reader::u16)?,
        io_latency_us: r.option(Reader::u32)?,
        io_max: r.option(|r| {
            Ok(IoRates {
                read_bps: r.option(Reader::u64)?,
                write_bps: r.option(Reader::u64)?,
                read_iops: r.option(Reader::u64)?,
                write_iops: r.option(Reader::u64)?,
            })
        })?,
        pids_max: r.option(Reader::u32)?,
    })
}

fn put_oci(w: &mut Writer, spec: &OciSpec) -> Result<(), Fault> {
    match &spec.rootfs {
        Rootfs::Dir(dir) => {
            w.u8(0);
            w.path(dir)?;
        }
        Rootfs::Layers(layers) => {
            w.u8(1);
            w.list(layers, |w, p| w.path(p))?;
        }
    }
    put_program(w, &spec.init)?;
    w.list(&spec.mounts, put_mount)?;
    w.option_with(spec.hostname.as_deref(), Writer::string)?;
    w.u8(match spec.network {
        Network::Host => 0,
        Network::Private => 1,
    });
    w.list(&spec.hosts, |w, (name, addr)| {
        w.string(name)?;
        w.string(addr)
    })
}

fn oci(r: &mut Reader<'_>) -> Result<OciSpec, Fault> {
    let rootfs = match r.u8()? {
        0 => Rootfs::Dir(r.path()?),
        1 => Rootfs::Layers(r.list(|r| r.path())?),
        t => return Err(Fault::value("rootfs", format!("variant {t}"))),
    };
    let init = program(r)?;
    let mounts = r.list(|r| mount(r, "mount destination"))?;
    let hostname = r.option(|r| r.string("hostname"))?;
    let network = match r.u8()? {
        0 => Network::Host,
        1 => Network::Private,
        t => return Err(Fault::value("network", format!("variant {t}"))),
    };
    let hosts = r.list(|r| Ok((r.string("host name")?, r.string("host address")?)))?;
    Ok(OciSpec {
        rootfs,
        init,
        mounts,
        hostname,
        network,
        hosts,
    })
}

/// A Shards image VM's spec: its pinned image, then its run's command, an option of a list (joint plan
/// step 12), then its network grants (joint plan step 15), then the members its caller drives, each its member,
/// its variables, an option of its working directory (joint plan step 17), its run-time arguments (D256, after
/// the working directory) and whether it asks an MCP bridge (Shards' D254, after its arguments), then its shares (kiln's step 18), each its host directory, where its guest sees it and
/// whether read-only, and the members it is given to (Shards' D277), as a mount's ([`put_mount`]), then its credential
/// grants (joint plan step 33, [`put_credentials`]), then its sign-ins ([`put_sign_ins`]), then its volumes (kiln plan
/// 01a1dd2e…, P6.2), each its name, its bound, where its members see it, its members, each as a grant's member, and
/// whether read-only. Fields a later step adds go after the volumes (append-only).
fn put_shards_image(w: &mut Writer, image: &ShardsImage) -> Result<(), Fault> {
    w.string(&image.image)?;
    w.option_with(image.command.as_deref(), |w, command| {
        w.list(command, |w, arg| w.string(arg))
    })?;
    put_grants(w, &image.grants)?;
    w.list(&image.members, |w, m| {
        put_member(w, &m.member)?;
        w.list(&m.env, |w, (k, v)| {
            w.string(k)?;
            w.string(v)
        })?;
        w.option_with(m.workdir.as_deref(), Writer::string)?;
        w.list(&m.args, |w, arg| w.string(arg))?;
        w.bool(m.mcp);
        Ok(())
    })?;
    w.list(&image.shares, put_mount)?;
    put_credentials(w, &image.credentials, &image.grants)?;
    put_sign_ins(w, &image.sign_ins, &image.credentials, &image.grants)?;
    w.list(&image.volumes, |w, v| {
        w.string(&v.name)?;
        w.u64(v.bound.get());
        w.string(&v.at)?;
        w.list(&v.members, put_member)?;
        w.bool(v.read_only);
        Ok(())
    })
}

fn shards_image(r: &mut Reader<'_>) -> Result<ShardsImage, Fault> {
    let mut image = ShardsImage {
        image: r.string("shards image")?,
        command: r.option(|r| r.list(|r| r.string("shards image command")))?,
        grants: grants(r)?,
        members: r.list(|r| {
            Ok(ImageMember {
                member: member(r)?,
                env: r.list(|r| Ok((r.string("member variable")?, r.string("member value")?)))?,
                workdir: r.option(|r| r.string("member workdir"))?,
                args: r.list(|r| r.string("member argument"))?,
                mcp: r.bool("member mcp")?,
            })
        })?,
        shares: r.list(|r| mount(r, "share destination"))?,
        credentials: Vec::new(),
        sign_ins: Vec::new(),
        volumes: Vec::new(),
    };
    image.credentials = credentials(r, &image.grants)?;
    image.sign_ins = sign_ins(r, &image.credentials, &image.grants)?;
    image.volumes = r.list(|r| {
        Ok(ImageVolume {
            name: r.string("volume name")?,
            bound: std::num::NonZeroU64::new(r.u64()?).ok_or_else(|| {
                Fault::value("volume bound", "0: a volume's bound is at least a byte")
            })?,
            at: r.string("volume place")?,
            members: r.list(member)?,
            read_only: r.bool("volume read_only")?,
        })
    })?;
    Ok(image)
}

// Joint plan step 33 (Shards' D242): credential grants, a Shards image VM's after its shares and an inspect's after its
// run's words. Each its member as a grant's, its reference, its hosts, and its placeholder (after its hosts, as Shards'
// image channel carries it). Both sides run `check_credentials` over the network grants beside them, so a list the
// check refuses is neither sent nor read.
fn put_credentials(
    w: &mut Writer,
    credentials: &[CredentialGrant],
    grants: &[Grant],
) -> Result<(), Fault> {
    check_credentials(credentials, grants)
        .map_err(|e| Fault::value("credentials", e.to_string()))?;
    w.list(credentials, |w, grant| {
        put_member(w, &grant.member)?;
        w.string(&grant.reference)?;
        w.list(&grant.toward, |w, host| w.string(host))?;
        w.string(&grant.placeholder)
    })
}

// Joint plan step 33's sign-ins (Shards' D242, step 8; docs/design/credential-grants.md C8): a Shards image VM's after
// its credentials. Each its member, its name, its CLI (1 Claude Code, 2 Codex, as Shards' wires number them), its hosts,
// and an earlier capture where there was one. Both sides run `check_sign_ins`, so a list it refuses is neither sent nor
// read.
fn put_sign_ins(
    w: &mut Writer,
    sign_ins: &[hex_api::SignInGrant],
    credentials: &[CredentialGrant],
    grants: &[Grant],
) -> Result<(), Fault> {
    check_sign_ins(sign_ins, credentials, grants)
        .map_err(|e| Fault::value("sign_ins", e.to_string()))?;
    w.list(sign_ins, |w, grant| {
        put_member(w, &grant.member)?;
        w.string(&grant.sign_in)?;
        w.u8(match grant.cli {
            hex_api::SignInCli::ClaudeCode => 1,
            hex_api::SignInCli::Codex => 2,
        });
        w.list(&grant.toward, |w, host| w.string(host))?;
        w.option_with(grant.captured.as_ref(), put_captured)
    })
}

fn sign_ins(
    r: &mut Reader<'_>,
    credentials: &[CredentialGrant],
    grants: &[Grant],
) -> Result<Vec<hex_api::SignInGrant>, Fault> {
    let sign_ins = r.list(|r| {
        Ok(hex_api::SignInGrant {
            member: member(r)?,
            sign_in: r.string("sign-in")?,
            cli: match r.u8()? {
                1 => hex_api::SignInCli::ClaudeCode,
                2 => hex_api::SignInCli::Codex,
                code => {
                    return Err(Fault::value(
                        "sign-in CLI",
                        format!("{code} names a CLI the wire doesn't define"),
                    ));
                }
            },
            toward: r.list(|r| r.string("sign-in host"))?,
            captured: r.option(captured)?,
        })
    })?;
    check_sign_ins(&sign_ins, credentials, grants)
        .map_err(|e| Fault::value("sign_ins", e.to_string()))?;
    Ok(sign_ins)
}

fn credentials(r: &mut Reader<'_>, grants: &[Grant]) -> Result<Vec<CredentialGrant>, Fault> {
    let credentials = r.list(|r| {
        Ok(CredentialGrant {
            member: member(r)?,
            reference: r.string("credential reference")?,
            toward: r.list(|r| r.string("credential host"))?,
            placeholder: r.string("credential placeholder")?,
        })
    })?;
    check_credentials(&credentials, grants)
        .map_err(|e| Fault::value("credentials", e.to_string()))?;
    Ok(credentials)
}

/// A microVM's guest time per vCPU: tag 0 and a list of nanoseconds, at most
/// [`GuestTime::MAX_VCPUS`]; or tag 1 and what its backend awaits.
fn put_guest_time(w: &mut Writer, guest: &GuestTime) -> Result<(), Fault> {
    match guest {
        GuestTime::PerVcpuNs(per_vcpu) => {
            if per_vcpu.len() > GuestTime::MAX_VCPUS {
                return Err(Fault::value(
                    "guest time",
                    "more vCPUs than a VM's vCPU index space",
                ));
            }
            w.u8(0);
            w.list(per_vcpu, |w, ns| {
                w.u64(*ns);
                Ok(())
            })
        }
        GuestTime::Awaits(needs) => {
            w.u8(1);
            put_upstream(w, *needs)
        }
    }
}

fn guest_time(r: &mut Reader<'_>) -> Result<GuestTime, Fault> {
    match r.u8()? {
        0 => {
            let per_vcpu = r.list(|r| r.u64())?;
            if per_vcpu.len() > GuestTime::MAX_VCPUS {
                return Err(Fault::value(
                    "guest time",
                    "more vCPUs than a VM's vCPU index space",
                ));
            }
            Ok(GuestTime::PerVcpuNs(per_vcpu))
        }
        1 => Ok(GuestTime::Awaits(upstream(r)?)),
        t => Err(Fault::value("guest time", format!("variant {t}"))),
    }
}

/// How a Shards image VM's microVM started (D253): a tag, its place in [`StartedFrom`], so append only.
fn put_started_from(w: &mut Writer, from: StartedFrom) {
    w.u8(match from {
        StartedFrom::Booted => 0,
        StartedFrom::Saved => 1,
        StartedFrom::Restored => 2,
        StartedFrom::Replaced => 3,
        StartedFrom::Warm => 4,
    });
}

fn started_from(r: &mut Reader<'_>) -> Result<StartedFrom, Fault> {
    match r.u8()? {
        0 => Ok(StartedFrom::Booted),
        1 => Ok(StartedFrom::Saved),
        2 => Ok(StartedFrom::Restored),
        3 => Ok(StartedFrom::Replaced),
        4 => Ok(StartedFrom::Warm),
        t => Err(Fault::value("started from", format!("variant {t}"))),
    }
}

// Joint plan step 15 (AC-T7.7.2): a box's network grants (docs/design/network-grants.md). Fixed tags independent of
// Rust discriminants: a grant's way first (0 egress, 1 host, 2 publish), a member's kind as Shards' image wire tags
// it (0 agent, 1 harness), a protocol by its IANA number, an address by its family (4, 6) then its octets. Both
// sides run `check_grants`, so a list the check refuses is neither sent nor read.
fn put_grants(w: &mut Writer, grants: &[Grant]) -> Result<(), Fault> {
    check_grants(grants).map_err(|e| Fault::value("grants", e.to_string()))?;
    w.list(grants, put_grant)
}

fn put_grant(w: &mut Writer, grant: &Grant) -> Result<(), Fault> {
    match grant {
        Grant::Egress { member, to, ports } => {
            w.u8(0);
            put_member(w, member)?;
            match to {
                Destination::Name(name) => {
                    w.u8(0);
                    w.string(name)?;
                }
                Destination::Address(address) => {
                    w.u8(1);
                    put_address(w, *address);
                }
            }
            put_ports(w, *ports);
        }
        Grant::Host { member, ports } => {
            w.u8(1);
            put_member(w, member)?;
            put_ports(w, *ports);
        }
        Grant::Publish {
            member,
            proto,
            port,
            host,
            host_port,
        } => {
            w.u8(2);
            put_member(w, member)?;
            w.u8(proto_tag(*proto));
            w.u16(*port);
            put_address(w, *host);
            w.option(*host_port, Writer::u16);
        }
    }
    Ok(())
}

/// A mount, a container's or a Shards image VM's share: its host path, where it is seen, whether read-only, and its
/// member scope (Shards' D277, after `read_only`): the members it is given to, none for a container's, which its spec's
/// check refuses, so a scope a caller names is never dropped on its way.
fn put_mount(w: &mut Writer, m: &Mount) -> Result<(), Fault> {
    w.path(&m.source)?;
    w.string(&m.destination)?;
    w.bool(m.read_only);
    w.list(&m.members, put_member)
}

/// [`put_mount`]'s, `destination` naming where it is seen in a refusal.
fn mount(r: &mut Reader<'_>, destination: &'static str) -> Result<Mount, Fault> {
    Ok(Mount {
        source: r.path()?,
        destination: r.string(destination)?,
        read_only: r.bool("read_only")?,
        members: r.list(member)?,
    })
}

fn put_member(w: &mut Writer, member: &Member) -> Result<(), Fault> {
    w.u8(match member.kind {
        MemberKind::Agent => 0,
        MemberKind::Harness => 1,
    });
    w.string(&member.name)
}

fn put_ports(w: &mut Writer, ports: Ports) {
    w.u8(proto_tag(ports.proto));
    w.u16(ports.first);
    w.u16(ports.last);
}

fn proto_tag(proto: Proto) -> u8 {
    match proto {
        Proto::Tcp => 6,
        Proto::Udp => 17,
    }
}

fn put_address(w: &mut Writer, address: IpAddr) {
    match address {
        IpAddr::V4(v4) => {
            w.u8(4);
            w.raw(&v4.octets());
        }
        IpAddr::V6(v6) => {
            w.u8(6);
            w.raw(&v6.octets());
        }
    }
}

fn grants(r: &mut Reader<'_>) -> Result<Vec<Grant>, Fault> {
    let grants = r.list(grant)?;
    check_grants(&grants).map_err(|e| Fault::value("grants", e.to_string()))?;
    Ok(grants)
}

fn grant(r: &mut Reader<'_>) -> Result<Grant, Fault> {
    Ok(match r.u8()? {
        0 => {
            let member = member(r)?;
            let to = match r.u8()? {
                0 => Destination::Name(r.string("grant destination")?),
                1 => Destination::Address(address(r)?),
                t => return Err(Fault::value("grant destination", format!("variant {t}"))),
            };
            Grant::Egress {
                member,
                to,
                ports: ports(r)?,
            }
        }
        1 => Grant::Host {
            member: member(r)?,
            ports: ports(r)?,
        },
        2 => Grant::Publish {
            member: member(r)?,
            proto: proto(r)?,
            port: r.u16()?,
            host: address(r)?,
            host_port: r.option(Reader::u16)?,
        },
        t => return Err(Fault::value("grant", format!("variant {t}"))),
    })
}

fn member(r: &mut Reader<'_>) -> Result<Member, Fault> {
    let kind = match r.u8()? {
        0 => MemberKind::Agent,
        1 => MemberKind::Harness,
        t => return Err(Fault::value("grant member", format!("kind {t}"))),
    };
    Ok(Member {
        kind,
        name: r.string("grant member")?,
    })
}

fn ports(r: &mut Reader<'_>) -> Result<Ports, Fault> {
    Ok(Ports {
        proto: proto(r)?,
        first: r.u16()?,
        last: r.u16()?,
    })
}

fn proto(r: &mut Reader<'_>) -> Result<Proto, Fault> {
    match r.u8()? {
        6 => Ok(Proto::Tcp),
        17 => Ok(Proto::Udp),
        t => Err(Fault::value("grant protocol", format!("number {t}"))),
    }
}

fn address(r: &mut Reader<'_>) -> Result<IpAddr, Fault> {
    match r.u8()? {
        4 => Ok(IpAddr::from(r.array::<4>()?)),
        6 => Ok(IpAddr::from(r.array::<16>()?)),
        t => Err(Fault::value("grant address", format!("family {t}"))),
    }
}

pub(crate) fn put_program(w: &mut Writer, p: &Program) -> Result<(), Fault> {
    w.list(&p.args, |w, a| w.string(a))?;
    w.list(&p.env, |w, (k, v)| {
        w.string(k)?;
        w.string(v)
    })?;
    w.option_with(p.cwd.as_deref(), Writer::string)?;
    w.option_with(p.user.as_ref(), |w, u| {
        w.u32(u.uid);
        w.u32(u.gid);
        w.list(&u.groups, |w, g| {
            w.u32(*g);
            Ok(())
        })
    })
}

pub(crate) fn program(r: &mut Reader<'_>) -> Result<Program, Fault> {
    Ok(Program {
        args: r.list(|r| r.string("argument"))?,
        env: r.list(|r| Ok((r.string("variable")?, r.string("value")?)))?,
        cwd: r.option(|r| r.string("cwd"))?,
        user: r.option(|r| {
            Ok(User {
                uid: r.u32()?,
                gid: r.u32()?,
                groups: r.list(Reader::u32)?,
            })
        })?,
    })
}

pub(crate) fn box_id(r: &mut Reader<'_>) -> Result<BoxId, Fault> {
    let s = r.string("box id")?;
    BoxId::new(&s).map_err(|e| Fault::value("box id", e.to_string()))
}

fn class_tag(c: Class) -> u8 {
    match c {
        Class::Process => 0,
        Class::Oci => 1,
        Class::Firecracker => 2,
        Class::Shards => 3,
        // P2.2: a Shards image VM's own class, after every older one.
        Class::ShardsImage => 4,
    }
}

fn class(r: &mut Reader<'_>) -> Result<Class, Fault> {
    match r.u8()? {
        0 => Ok(Class::Process),
        1 => Ok(Class::Oci),
        2 => Ok(Class::Firecracker),
        3 => Ok(Class::Shards),
        4 => Ok(Class::ShardsImage),
        t => Err(Fault::value("class", format!("variant {t}"))),
    }
}

fn put_link(w: &mut Writer, link: &Link) -> Result<(), Fault> {
    w.string(&link.peer)?;
    w.u16(link.suite);
    w.option(link.group, Writer::u16);
    Ok(())
}

fn link(r: &mut Reader<'_>) -> Result<Link, Fault> {
    Ok(Link {
        peer: r.string("a link's peer")?,
        suite: r.u16()?,
        group: r.option(Reader::u16)?,
    })
}

fn put_info(w: &mut Writer, info: &BoxInfo) -> Result<(), Fault> {
    check_info_route(info)?;
    w.string(info.id.as_str())?;
    w.u8(class_tag(info.class));
    w.u8(match info.state {
        BoxState::Empty => 0,
        BoxState::Running => 1,
        BoxState::Created => 2,
    });
    put_bounds(w, &info.bounds);
    w.bool(info.adopted);
    w.option(info.exit, put_exit);
    w.u64(info.abandoned);
    // AC-T7.4.1: the retained native receipt follows every old info field. The wire identity
    // changes with this schema; list entries carry their own option tag, including None.
    w.reserve_fixed(if info.allocation.is_some() {
        OPTIONAL_ALLOCATION_BYTES
    } else {
        1
    })?;
    w.option_with(info.allocation, |w, allocation| {
        allocation
            .guest()
            .map_err(|e| Fault::value("allocation", e.to_string()))?;
        put_resources(w, allocation.resources);
        put_cost(w, allocation.cost)?;
        w.option(allocation.booked, put_resources);
        Ok(())
    })?;
    put_route_option(w, info.route)?;
    // Joint plan step 15: the record's grants, after every older field, then their ports as bound.
    put_grants(w, &info.grants)?;
    check_published(&info.grants, &info.published)
        .map_err(|e| Fault::value("published", e.to_string()))?;
    w.list(&info.published, |w, published| {
        w.u32(published.grant);
        put_address(w, published.at.ip());
        w.u16(published.at.port());
        match published.receiver {
            Receiver::Declared => w.u8(0),
            Receiver::Named { port } => {
                w.u8(1);
                w.u16(port);
            }
        }
        Ok(())
    })?;
    // Joint plan step 14 (C5): why its node's last resort ended it, after every older field.
    w.option_with(info.last_resort.as_ref(), put_learned_end)?;
    // Joint plan step 12: why its run ended where it failed, in its runtime's words, after every older field.
    w.option_with(info.ended.as_ref(), |w, words| w.string(words))?;
    // Joint plan step 33: the record's credential grants, after every older field.
    put_credentials(w, &info.credentials, &info.grants)?;
    // Joint plan step 15 (N6): its run's agent-control endpoint, after every older field.
    w.option(info.control, |w, at| {
        put_address(w, at.ip());
        w.u16(at.port());
    });
    // Shards' D252 amended (image channel v30): that endpoint's waker and wake, after every older field.
    w.option(info.control_wake, |w, held| {
        for at in [held.waker, held.wake] {
            put_address(w, at.ip());
            w.u16(at.port());
        }
    });
    Ok(())
}

fn info(r: &mut Reader<'_>) -> Result<BoxInfo, Fault> {
    let mut info = BoxInfo {
        membership: None,
        id: box_id(r)?,
        class: class(r)?,
        state: match r.u8()? {
            0 => BoxState::Empty,
            1 => BoxState::Running,
            2 => BoxState::Created,
            t => return Err(Fault::value("box state", format!("variant {t}"))),
        },
        bounds: bounds(r)?,
        adopted: r.bool("adopted")?,
        exit: r.option(exit)?,
        abandoned: r.u64()?,
        allocation: r.option(|r| {
            let allocation = Allocation {
                resources: resources(r)?,
                cost: runtime_cost(r)?,
                booked: r.option(resources)?,
            };
            allocation
                .guest()
                .map_err(|e| Fault::value("allocation", e.to_string()))?;
            Ok(allocation)
        })?,
        route: r.option(route_receipt)?,
        grants: grants(r)?,
        published: r.list(|r| {
            Ok(Published {
                grant: r.u32()?,
                at: SocketAddr::new(address(r)?, r.u16()?),
                receiver: match r.u8()? {
                    0 => Receiver::Declared,
                    1 => Receiver::Named { port: r.u16()? },
                    t => return Err(Fault::value("published receiver", format!("variant {t}"))),
                },
            })
        })?,
        last_resort: r.option(learned_end)?,
        ended: r.option(|r| r.string("ended"))?,
        control: None,
        control_wake: None,
        credentials: Vec::new(),
    };
    info.credentials = credentials(r, &info.grants)?;
    info.control = r.option(|r| Ok(SocketAddr::new(address(r)?, r.u16()?)))?;
    info.control_wake = r.option(|r| {
        Ok(ControlWake {
            waker: SocketAddr::new(address(r)?, r.u16()?),
            wake: SocketAddr::new(address(r)?, r.u16()?),
        })
    })?;
    if info.control.is_none() && info.control_wake.is_some() {
        return Err(Fault::value(
            "control_wake",
            "an agent-control endpoint's waker and wake told without the endpoint",
        ));
    }
    check_info_route(&info)?;
    check_published(&info.grants, &info.published)
        .map_err(|e| Fault::value("published", e.to_string()))?;
    Ok(info)
}

fn put_exit(w: &mut Writer, exit: Exit) {
    match exit {
        Exit::Code(c) => {
            w.u8(0);
            w.i32(c);
        }
        Exit::Signal(s) => {
            w.u8(1);
            w.i32(s);
        }
        Exit::Unknown => w.u8(2),
    }
}

fn exit(r: &mut Reader<'_>) -> Result<Exit, Fault> {
    match r.u8()? {
        0 => Ok(Exit::Code(r.i32()?)),
        1 => Ok(Exit::Signal(r.i32()?)),
        2 => Ok(Exit::Unknown),
        t => Err(Fault::value("exit", format!("variant {t}"))),
    }
}

/// A refusal: an `Uncertain` one's tag, id and its two causes, each written by [`put_plain_refusal`], which refuses
/// another `Uncertain` there, so no message nests them past one; any other, by [`put_plain_refusal`] alone. Neither
/// reaches itself (CLAUDE.md §11).
fn put_refusal(w: &mut Writer, refusal: &Refusal) -> Result<(), Fault> {
    match refusal {
        Refusal::Uncertain {
            id,
            refusal,
            cleanup,
        } => {
            w.u8(12);
            w.string(id.as_str())?;
            put_plain_refusal(w, refusal)?;
            put_plain_refusal(w, cleanup)
        }
        plain => put_plain_refusal(w, plain),
    }
}

/// Any refusal but an `Uncertain` one, and so each of an `Uncertain` refusal's causes: an `Uncertain` here, a cause
/// that is itself uncertain, is refused before any of it is written.
fn put_plain_refusal(w: &mut Writer, refusal: &Refusal) -> Result<(), Fault> {
    match refusal {
        Refusal::Invalid { what, why } => {
            w.u8(0);
            w.string(what)?;
            w.string(why)
        }
        Refusal::NoSuchBox(b) => {
            w.u8(1);
            w.string(b.as_str())
        }
        Refusal::Exists(b) => {
            w.u8(2);
            w.string(b.as_str())
        }
        Refusal::NotRunning(b, op) => {
            w.u8(3);
            w.string(b.as_str())?;
            w.string(op)
        }
        Refusal::Platform {
            what,
            platform,
            why,
        } => {
            w.u8(4);
            w.string(what)?;
            w.string(platform)?;
            w.string(why)
        }
        Refusal::Full(n) => {
            w.u8(5);
            w.usize(*n)
        }
        Refusal::Exhausted { what, bound } => {
            w.u8(6);
            w.string(what)?;
            w.usize(*bound)
        }
        Refusal::Os { op, why } => {
            w.u8(7);
            w.string(op)?;
            w.string(why)
        }
        Refusal::Broken(why) => {
            w.u8(8);
            w.string(why)
        }
        Refusal::Lifecycle { id, cause, why } => {
            w.u8(9);
            w.string(id.as_str())?;
            w.u8(match cause {
                LifecycleCause::Pending => 0,
                LifecycleCause::Uncertain => 1,
                LifecycleCause::Busy => 2,
                LifecycleCause::Retired => 3,
                LifecycleCause::NodeVmEnded => 4,
            });
            // Appended after its cause (joint plan step 12): its reason in the refusing site's words, absent a byte 0.
            w.option_with(why.as_deref(), |w, why| w.string(why))
        }
        Refusal::Backing { key, cause, why } => {
            // Its tag, key, cause and reason's option tag, and its reason, reserved whole before any of it is written.
            let body = backing_why_bytes(refusal)
                .and_then(|why| why.checked_add(1 + BACKING_KEY_BYTES + 1 + 1))
                .ok_or_else(|| Fault::value("backing refusal", "encoded size overflows"))?;
            w.reserve_fixed(body)?;
            w.u8(10);
            put_backing_key(w, *key)?;
            w.u8(match cause {
                LifecycleCause::Pending => 0,
                LifecycleCause::Uncertain => 1,
                LifecycleCause::Busy => 2,
                LifecycleCause::Retired => 3,
                LifecycleCause::NodeVmEnded => 4,
            });
            // Appended after its cause (joint plan step 14): its reason in the refusing site's words, absent a byte 0.
            w.option_with(why.as_deref(), |w, why| w.string(why))
        }
        Refusal::Connect { port, failure } => {
            w.reserve_fixed(
                connect_refusal_bytes(*failure)
                    .ok_or_else(|| Fault::value("connect refusal", "encoded size overflows"))?,
            )?;
            w.u8(11);
            w.u32(*port);
            put_connect_failure(w, *failure)
        }
        Refusal::Uncertain { .. } => Err(Fault::value(
            "uncertain refusal",
            "a cause is itself uncertain",
        )),
        Refusal::Awaits { needs } => {
            // Its tag and the capability's, reserved whole as a Backing refusal's are.
            w.reserve_fixed(2)?;
            w.u8(13);
            put_upstream(w, *needs)
        }
        // Joint plan step 12: a box whose run failed, in its runtime's words, after every older refusal.
        Refusal::Failed { id, why } => {
            w.u8(14);
            w.string(id.as_str())?;
            w.string(why)
        }
        // Joint plan step 33: what the node isn't configured to serve, after every older refusal.
        Refusal::Unconfigured { what, why } => {
            w.u8(15);
            w.string(what)?;
            w.string(why)
        }
        // The review's B-15: a Shards guest's failure in its own kind, after every older refusal.
        Refusal::Guest { op, failure } => {
            w.u8(16);
            w.string(op)?;
            put_guest_failure(w, failure)
        }
        // Kiln plan 01a1dd2e…, P6.2: a Shards home's volume refused by what held it (D281), after every older refusal.
        Refusal::Volume { cause, why } => {
            w.u8(17);
            w.u8(match cause {
                VolumeCause::Bound => 0,
                VolumeCause::Clearing => 1,
                VolumeCause::InUse => 2,
                VolumeCause::Unknown => 3,
            });
            w.string(why)
        }
    }
}

/// A Shards guest's failure ([`Refusal::Guest`]): its kind's tag, then its fields, each duration in whole nanoseconds.
fn put_guest_failure(w: &mut Writer, failure: &GuestFailure) -> Result<(), Fault> {
    match failure {
        GuestFailure::Stalled {
            waited,
            over,
            slowest,
        } => {
            w.u8(0);
            for d in [waited, over, slowest] {
                let nanos = u64::try_from(d.as_nanos())
                    .map_err(|_| Fault::value("guest stall", "it passes a u64 of nanoseconds"))?;
                w.u64(nanos);
            }
            Ok(())
        }
        GuestFailure::Ended(end) => {
            w.u8(1);
            match end {
                GuestEnd::PowerOff => w.u8(0),
                GuestEnd::Reset => w.u8(1),
                GuestEnd::Stopped => w.u8(2),
                GuestEnd::Failed(why) => {
                    w.u8(3);
                    return w.string(why);
                }
            }
            Ok(())
        }
        GuestFailure::Gone { pid, why } => {
            w.u8(2);
            w.u32(*pid);
            w.string(why)
        }
        GuestFailure::Rate(why) => {
            w.u8(3);
            w.string(why)
        }
        GuestFailure::HostUse(why) => {
            w.u8(4);
            w.string(why)
        }
    }
}

fn guest_failure(r: &mut Reader<'_>) -> Result<GuestFailure, Fault> {
    Ok(match r.u8()? {
        0 => GuestFailure::Stalled {
            waited: std::time::Duration::from_nanos(r.u64()?),
            over: std::time::Duration::from_nanos(r.u64()?),
            slowest: std::time::Duration::from_nanos(r.u64()?),
        },
        1 => GuestFailure::Ended(match r.u8()? {
            0 => GuestEnd::PowerOff,
            1 => GuestEnd::Reset,
            2 => GuestEnd::Stopped,
            3 => GuestEnd::Failed(r.string("guest end")?),
            t => return Err(Fault::value("guest end", format!("variant {t}"))),
        }),
        2 => GuestFailure::Gone {
            pid: r.u32()?,
            why: r.string("guest gone")?,
        },
        3 => GuestFailure::Rate(r.string("guest rate")?),
        4 => GuestFailure::HostUse(r.string("guest host use")?),
        t => return Err(Fault::value("guest failure", format!("variant {t}"))),
    })
}

/// A capability by its place in [`Upstream::ALL`] (append-only).
fn put_upstream(w: &mut Writer, needs: Upstream) -> Result<(), Fault> {
    let tag = Upstream::ALL
        .iter()
        .position(|known| *known == needs)
        .and_then(|at| u8::try_from(at).ok())
        .ok_or_else(|| Fault::value("upstream", format!("{needs:?} has no tag")))?;
    w.u8(tag);
    Ok(())
}

fn upstream(r: &mut Reader<'_>) -> Result<Upstream, Fault> {
    let tag = r.u8()?;
    Upstream::ALL
        .get(usize::from(tag))
        .copied()
        .ok_or_else(|| Fault::value("upstream", format!("variant {tag}")))
}

/// A Connect refusal's body, exactly, reserved whole before any of it is written as a Backing
/// refusal's is: its tag and port, the failure's tag, then its fixed-width fields, an option's tag
/// before each optional one.
fn connect_refusal_bytes(failure: ConnectFailure) -> Option<usize> {
    const TAG: usize = size_of::<u8>();
    let fields = match failure {
        ConnectFailure::Closed => 0,
        ConnectFailure::Expired { .. } | ConnectFailure::Reply(_) => TAG,
        ConnectFailure::Io { kind, code, .. } => TAG
            .checked_add(TAG)?
            .checked_add(kind.named().map_or(0, |_| TAG))?
            .checked_add(TAG)?
            .checked_add(code.map_or(0, |_| size_of::<i32>()))?,
    };
    TAG.checked_add(size_of::<u32>())?
        .checked_add(TAG)?
        .checked_add(fields)
}

/// A refusal: an `Uncertain` one's id and its two causes ([`uncertain_cause`]), or any other ([`plain_refusal`]).
/// None of them reaches itself (CLAUDE.md §11).
fn refusal(r: &mut Reader<'_>) -> Result<Refusal, Fault> {
    match r.u8()? {
        12 => Ok(Refusal::Uncertain {
            id: box_id(r)?,
            refusal: Box::new(uncertain_cause(r)?),
            cleanup: Box::new(uncertain_cause(r)?),
        }),
        tag => plain_refusal(r, tag),
    }
}

/// One of an `Uncertain` refusal's causes: any refusal but another `Uncertain`, refused at its tag before any of it
/// is read, so no message nests them past one.
fn uncertain_cause(r: &mut Reader<'_>) -> Result<Refusal, Fault> {
    let tag = r.u8()?;
    plain_refusal(r, tag)
}

/// Any refusal but an `Uncertain` one, its `tag` read: an `Uncertain` tag here is a cause that is itself uncertain,
/// refused.
fn plain_refusal(r: &mut Reader<'_>, tag: u8) -> Result<Refusal, Fault> {
    Ok(match tag {
        0 => Refusal::Invalid {
            what: name(&r.string("refusal name")?)?,
            why: r.string("refusal reason")?,
        },
        1 => Refusal::NoSuchBox(box_id(r)?),
        2 => Refusal::Exists(box_id(r)?),
        3 => Refusal::NotRunning(box_id(r)?, name(&r.string("refusal name")?)?),
        4 => Refusal::Platform {
            what: name(&r.string("refusal name")?)?,
            platform: name(&r.string("refusal name")?)?,
            why: r.string("refusal reason")?,
        },
        5 => Refusal::Full(r.usize()?),
        6 => Refusal::Exhausted {
            what: name(&r.string("refusal name")?)?,
            bound: r.usize()?,
        },
        7 => Refusal::Os {
            op: name(&r.string("refusal name")?)?,
            why: r.string("refusal reason")?,
        },
        8 => Refusal::Broken(r.string("refusal reason")?),
        9 => Refusal::Lifecycle {
            id: box_id(r)?,
            cause: match r.u8()? {
                0 => LifecycleCause::Pending,
                1 => LifecycleCause::Uncertain,
                2 => LifecycleCause::Busy,
                3 => LifecycleCause::Retired,
                4 => LifecycleCause::NodeVmEnded,
                t => return Err(Fault::value("lifecycle cause", format!("variant {t}"))),
            },
            why: r.option(|r| r.string("lifecycle refusal reason"))?,
        },
        10 => Refusal::Backing {
            key: backing_key(r)?,
            cause: match r.u8()? {
                0 => LifecycleCause::Pending,
                1 => LifecycleCause::Uncertain,
                2 => LifecycleCause::Busy,
                3 => LifecycleCause::Retired,
                4 => LifecycleCause::NodeVmEnded,
                _ => return Err(Fault::value("backing.cause", "unsupported tag")),
            },
            why: r.option(|r| r.string("backing refusal reason"))?,
        },
        11 => Refusal::Connect {
            port: r.u32()?,
            failure: connect_failure(r)?,
        },
        12 => {
            return Err(Fault::value(
                "uncertain refusal",
                "a cause is itself uncertain",
            ));
        }
        13 => Refusal::Awaits {
            needs: upstream(r)?,
        },
        14 => Refusal::Failed {
            id: box_id(r)?,
            why: r.string("refusal reason")?,
        },
        15 => Refusal::Unconfigured {
            what: name(&r.string("refusal name")?)?,
            why: r.string("refusal reason")?,
        },
        16 => Refusal::Guest {
            op: name(&r.string("refusal name")?)?,
            failure: guest_failure(r)?,
        },
        17 => Refusal::Volume {
            cause: match r.u8()? {
                0 => VolumeCause::Bound,
                1 => VolumeCause::Clearing,
                2 => VolumeCause::InUse,
                3 => VolumeCause::Unknown,
                t => return Err(Fault::value("volume cause", format!("variant {t}"))),
            },
            why: r.string("refusal reason")?,
        },
        t => return Err(Fault::value("refusal", format!("variant {t}"))),
    })
}

fn put_connect_failure(w: &mut Writer, failure: ConnectFailure) -> Result<(), Fault> {
    match failure {
        ConnectFailure::Expired { phase } => {
            w.u8(0);
            w.u8(connect_phase_tag(phase));
        }
        ConnectFailure::Io { phase, kind, code } => {
            w.u8(1);
            w.u8(connect_phase_tag(phase));
            put_io_kind(w, kind)?;
            w.option(code, |w, code| w.i32(code));
        }
        ConnectFailure::Closed => w.u8(2),
        ConnectFailure::Reply(reply) => {
            w.u8(3);
            w.u8(match reply {
                ConnectReply::TooLong => 0,
                ConnectReply::NotOk => 1,
            });
        }
    }
    Ok(())
}

fn connect_phase_tag(phase: ConnectPhase) -> u8 {
    match phase {
        ConnectPhase::Dial => 0,
        ConnectPhase::Write => 1,
        ConnectPhase::Read => 2,
        ConnectPhase::ClearTimeouts => 3,
    }
}

/// Fixed-width fields alone: no string copied, no name kept.
fn connect_failure(r: &mut Reader<'_>) -> Result<ConnectFailure, Fault> {
    Ok(match r.u8()? {
        0 => ConnectFailure::Expired {
            phase: connect_phase(r)?,
        },
        1 => ConnectFailure::Io {
            phase: connect_phase(r)?,
            kind: io_kind(r)?,
            code: r.option(|r| r.i32())?,
        },
        2 => ConnectFailure::Closed,
        3 => ConnectFailure::Reply(match r.u8()? {
            0 => ConnectReply::TooLong,
            1 => ConnectReply::NotOk,
            t => return Err(Fault::value("connect reply", format!("variant {t}"))),
        }),
        t => return Err(Fault::value("connect failure", format!("variant {t}"))),
    })
}

fn connect_phase(r: &mut Reader<'_>) -> Result<ConnectPhase, Fault> {
    Ok(match r.u8()? {
        0 => ConnectPhase::Dial,
        1 => ConnectPhase::Write,
        2 => ConnectPhase::Read,
        3 => ConnectPhase::ClearTimeouts,
        t => return Err(Fault::value("connect phase", format!("variant {t}"))),
    })
}

/// A named kind is its place in [`IoKind::NAMED`]; an unnamed one is none.
fn put_io_kind(w: &mut Writer, kind: IoKind) -> Result<(), Fault> {
    w.option_with(kind.named(), |w, kind| {
        let tag = IoKind::NAMED
            .iter()
            .position(|named| *named == kind)
            .and_then(|at| u8::try_from(at).ok())
            .ok_or_else(|| Fault::value("io kind", format!("{kind:?} has no tag")))?;
        w.u8(tag);
        Ok(())
    })
}

fn io_kind(r: &mut Reader<'_>) -> Result<IoKind, Fault> {
    let named = r.option(|r| {
        let tag = r.u8()?;
        IoKind::NAMED
            .get(usize::from(tag))
            .copied()
            .ok_or_else(|| Fault::value("io kind", format!("variant {tag}")))
    })?;
    Ok(named.map_or(IoKind::UNNAMED, IoKind::new))
}

/// Most distinct refusal names a process keeps, and the longest: a node agent's refusals name what
/// and why from a fixed set in its code (a few hundred at most), so these bound only a hostile one.
const NAMES: usize = 1024;
const NAME_BYTES: usize = 256;

/// The names decoded refusals carry. hex-api's refusals name what they refuse with `&'static str`,
/// so a name read from the wire is kept for the process's life, once however often it comes (at
/// most [`NAMES`] of at most [`NAME_BYTES`] each: 256 KiB). A name past either bound is refused.
static KEPT: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

fn name(s: &str) -> Result<&'static str, Fault> {
    if s.len() > NAME_BYTES {
        return Err(Fault::value(
            "refusal name",
            format!("{} bytes, over the {NAME_BYTES} kept", s.len()),
        ));
    }
    let mut kept = KEPT
        .lock()
        .map_err(|_| Fault::value("refusal name", "the kept names' lock was poisoned"))?;
    if let Some(k) = kept.get(s) {
        return Ok(k);
    }
    if kept.len() >= NAMES {
        return Err(Fault::value(
            "refusal name",
            format!("{s:?}: the {NAMES} names this process keeps are spent"),
        ));
    }
    let leaked: &'static str = s.to_owned().leak();
    kept.insert(leaked);
    Ok(leaked)
}

/// A message being written, refused once it would pass its bound.
pub(crate) struct Writer {
    out: Vec<u8>,
    bound: usize,
    mode: WriteMode,
    failure: Option<Fault>,
    /// Whether what it holds carries a lease's value ([`Writer::secret`]): wiped as it drops, unless handed on whole.
    secret: bool,
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.secret {
            hex_api::lease::wipe(&mut self.out);
        }
    }
}

#[derive(Clone, Copy)]
enum WriteMode {
    Ordinary,
    Counting(usize),
    Fixed,
}

impl Writer {
    pub(crate) fn new(bound: usize) -> Self {
        Self {
            out: Vec::with_capacity(HEADER_BYTES.min(bound)),
            bound,
            mode: WriteMode::Ordinary,
            failure: None,
            secret: false,
        }
    }

    /// What it writes from now carries a lease's value: wiped as it drops unless handed on whole ([`Writer::done`]),
    /// its room reserved before the value goes in (`reserve_fixed`), so no copy is left as it grows.
    pub(crate) fn secret(&mut self) {
        self.secret = true;
    }

    fn counting(bound: usize) -> Self {
        Self {
            out: Vec::new(),
            bound,
            mode: WriteMode::Counting(0),
            failure: None,
            secret: false,
        }
    }

    fn fixed(bytes: usize) -> Result<Self, Fault> {
        let mut out = Vec::new();
        out.try_reserve_exact(bytes).map_err(|_| Fault::Storage {
            what: "wire snapshot bytes",
            bound: bytes,
        })?;
        Ok(Self {
            out,
            bound: bytes,
            mode: WriteMode::Fixed,
            failure: None,
            secret: false,
        })
    }

    fn position(&self) -> usize {
        match self.mode {
            WriteMode::Counting(bytes) => bytes,
            WriteMode::Ordinary | WriteMode::Fixed => self.out.len(),
        }
    }

    fn status(&mut self) -> Result<(), Fault> {
        match self.failure.take() {
            Some(fault) => Err(fault),
            None => Ok(()),
        }
    }

    fn checked_growth(&self, bytes: usize) -> Result<usize, Fault> {
        let after = self.position().checked_add(bytes);
        if after.is_none_or(|after| after > self.bound) {
            return Err(Fault::Oversized {
                bytes: after.unwrap_or(usize::MAX),
                bound: self.bound,
            });
        }
        let after = after.ok_or(Fault::Oversized {
            bytes: usize::MAX,
            bound: self.bound,
        })?;
        if matches!(self.mode, WriteMode::Fixed) && after > self.out.capacity() {
            return Err(Fault::Storage {
                what: "wire snapshot fixed capacity",
                bound: self.bound,
            });
        }
        Ok(after)
    }

    fn counted(mut self) -> Result<usize, Fault> {
        self.status()?;
        Ok(self.position())
    }

    pub(crate) fn header(&mut self, kind: u16, id: u64) {
        self.raw(&MAGIC);
        self.u64(IDENTITY);
        self.u16(kind);
        self.u64(id);
    }

    /// The new fixed-width cost/charge tail is checked and reserved before writing any of it.
    fn reserve_fixed(&mut self, bytes: usize) -> Result<(), Fault> {
        self.status()?;
        self.checked_growth(bytes)?;
        if !matches!(self.mode, WriteMode::Ordinary) {
            return Ok(());
        }
        self.out
            .try_reserve_exact(bytes)
            .map_err(|_| Fault::Storage {
                what: "wire allocation receipt",
                bound: bytes,
            })
    }

    /// Bytes as they are, with no length before them: another message's header.
    pub(crate) fn raw(&mut self, b: &[u8]) {
        if matches!(self.mode, WriteMode::Ordinary) {
            self.out.extend_from_slice(b);
            return;
        }
        if self.failure.is_some() {
            return;
        }
        match self.checked_growth(b.len()) {
            Ok(after) => match self.mode {
                WriteMode::Counting(_) => self.mode = WriteMode::Counting(after),
                WriteMode::Ordinary | WriteMode::Fixed => self.out.extend_from_slice(b),
            },
            Err(fault) => self.failure = Some(fault),
        }
    }

    pub(crate) fn done(mut self) -> Result<Vec<u8>, Fault> {
        self.status()?;
        if matches!(self.mode, WriteMode::Fixed) && self.out.len() != self.bound {
            return Err(Fault::Storage {
                what: "wire snapshot exact size",
                bound: self.bound,
            });
        }
        if self.out.len() > self.bound {
            return Err(Fault::Oversized {
                bytes: self.out.len(),
                bound: self.bound,
            });
        }
        // Handed on whole: its holder wipes it, if it carries a value ([`carries_secret`]).
        self.secret = false;
        Ok(std::mem::take(&mut self.out))
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.raw(&[v]);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.raw(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.raw(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.raw(&v.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.raw(&v.to_le_bytes());
    }

    pub(crate) fn usize(&mut self, v: usize) -> Result<(), Fault> {
        let v =
            u64::try_from(v).map_err(|_| Fault::value("count", format!("{v} is over 64 bits")))?;
        self.u64(v);
        Ok(())
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    /// A length, refused when what follows it would pass the bound: a caller's huge argument list is
    /// refused before it is copied.
    pub(crate) fn len(&mut self, n: usize) -> Result<(), Fault> {
        self.status()?;
        let after = self
            .position()
            .checked_add(4)
            .and_then(|l| l.checked_add(n));
        if after.is_none_or(|after| after > self.bound) {
            return Err(Fault::Oversized {
                bytes: after.unwrap_or(usize::MAX),
                bound: self.bound,
            });
        }
        let n = u32::try_from(n).map_err(|_| Fault::Oversized {
            bytes: n,
            bound: self.bound,
        })?;
        self.u32(n);
        Ok(())
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) -> Result<(), Fault> {
        self.len(b.len())?;
        self.raw(b);
        Ok(())
    }

    pub(crate) fn string(&mut self, s: &str) -> Result<(), Fault> {
        self.bytes(s.as_bytes())
    }

    pub(crate) fn path(&mut self, p: &Path) -> Result<(), Fault> {
        self.bytes(path_bytes(p)?)
    }

    pub(crate) fn option<T>(&mut self, v: Option<T>, put: impl FnOnce(&mut Self, T)) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                put(self, v);
            }
        }
    }

    pub(crate) fn option_with<T>(
        &mut self,
        v: Option<T>,
        put: impl FnOnce(&mut Self, T) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        match v {
            None => {
                self.u8(0);
                Ok(())
            }
            Some(v) => {
                self.u8(1);
                put(self, v)
            }
        }
    }

    pub(crate) fn list<T>(
        &mut self,
        items: &[T],
        mut put: impl FnMut(&mut Self, &T) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        let n = u32::try_from(items.len()).map_err(|_| Fault::Oversized {
            bytes: items.len(),
            bound: self.bound,
        })?;
        self.u32(n);
        for item in items {
            put(self, item)?;
        }
        Ok(())
    }

    fn device_count(&mut self, count: usize, minimum_bytes: usize) -> Result<(), Fault> {
        self.status()?;
        if count > MAX_DEVICES {
            return Err(Fault::value(
                "devices",
                "count exceeds the u16 device index address space",
            ));
        }
        let after = count
            .checked_mul(minimum_bytes)
            .and_then(|bytes| self.position().checked_add(4)?.checked_add(bytes));
        if after.is_none_or(|after| after > self.bound) {
            return Err(Fault::Oversized {
                bytes: after.unwrap_or(usize::MAX),
                bound: self.bound,
            });
        }
        self.u32(u32::try_from(count).map_err(|_| Fault::value("devices", "count exceeds u32"))?);
        Ok(())
    }
}

/// A message being read, from the front.
pub(crate) struct Reader<'a> {
    rest: &'a [u8],
    len: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            len: bytes.len(),
        }
    }

    pub(crate) fn at(&self) -> usize {
        self.len.saturating_sub(self.rest.len())
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], Fault> {
        let Some((head, rest)) = self.rest.split_at_checked(n) else {
            return Err(Fault::Truncated {
                need: self.at().saturating_add(n),
                have: self.len,
            });
        };
        self.rest = rest;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], Fault> {
        let at = self.at();
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| Fault::Truncated {
            need: at.saturating_add(N),
            have: self.len,
        })
    }

    /// The header: magic first, so a message of something else is refused as that; then its sender's wire identity,
    /// its kind and its id.
    pub(crate) fn header(&mut self) -> Result<(u64, u16, u64), Fault> {
        if self.array::<4>()? != MAGIC {
            return Err(Fault::Magic);
        }
        Ok((self.u64()?, self.u16()?, self.u64()?))
    }

    /// That nothing follows what the node socket's wire defines for the message's kind.
    pub(crate) fn all_read(&self, kind: u16) -> Result<(), Fault> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Fault::Leftover {
                bytes: self.rest.len(),
                kind,
            })
        }
    }

    /// That nothing follows what the message's version defines, in the other protocols this codec carries.
    pub(crate) fn end(&self, kind: u16, version: u16) -> Result<(), Fault> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Fault::Trailing {
                bytes: self.rest.len(),
                kind,
                version,
            })
        }
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Fault> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Fault> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Fault> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Fault> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, Fault> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub(crate) fn usize(&mut self) -> Result<usize, Fault> {
        let v = self.u64()?;
        usize::try_from(v)
            .map_err(|_| Fault::value("count", format!("{v} is over this platform's")))
    }

    pub(crate) fn bool(&mut self, what: &'static str) -> Result<bool, Fault> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(Fault::value(what, format!("byte {b} is neither 0 nor 1"))),
        }
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], Fault> {
        let n = self.u32()?;
        let n = usize::try_from(n)
            .map_err(|_| Fault::value("length", format!("{n} is over this platform's")))?;
        self.take(n)
    }

    pub(crate) fn string(&mut self, what: &'static str) -> Result<String, Fault> {
        let b = self.bytes()?;
        String::from_utf8(b.to_vec()).map_err(|e| Fault::value(what, format!("isn't UTF-8: {e}")))
    }

    pub(crate) fn path(&mut self) -> Result<PathBuf, Fault> {
        path_of(self.bytes()?)
    }

    pub(crate) fn option<T>(
        &mut self,
        get: impl FnOnce(&mut Self) -> Result<T, Fault>,
    ) -> Result<Option<T>, Fault> {
        match self.u8()? {
            0 => Ok(None),
            1 => get(self).map(Some),
            b => Err(Fault::value(
                "option",
                format!("byte {b} is neither 0 nor 1"),
            )),
        }
    }

    /// A list: its count first, refused when more items are claimed than bytes are left, since each
    /// takes at least one; so a short message never makes room for a long list.
    pub(crate) fn list<T>(
        &mut self,
        mut get: impl FnMut(&mut Self) -> Result<T, Fault>,
    ) -> Result<Vec<T>, Fault> {
        let n = self.u32()?;
        let n = usize::try_from(n)
            .map_err(|_| Fault::value("count", format!("{n} is over this platform's")))?;
        if n > self.rest.len() {
            return Err(Fault::Truncated {
                need: self.at().saturating_add(n),
                have: self.len,
            });
        }
        // Room for a few: each item's room past that comes as it is read, so a claimed count costs
        // nothing until its items arrive.
        let mut out = Vec::with_capacity(n.min(16));
        for _ in 0..n {
            out.push(get(self)?);
        }
        Ok(out)
    }

    fn device_string(&mut self, what: &'static str) -> Result<String, Fault> {
        let bytes = self.bytes()?;
        let text = std::str::from_utf8(bytes).map_err(|_| Fault::value(what, "is not UTF-8"))?;
        let mut out = String::new();
        out.try_reserve_exact(text.len())
            .map_err(|_| Fault::value(what, "cannot reserve string"))?;
        out.push_str(text);
        Ok(out)
    }

    fn device_list<T>(
        &mut self,
        minimum_bytes: usize,
        get: impl FnMut(&mut Self) -> Result<T, Fault>,
    ) -> Result<Vec<T>, Fault> {
        self.bounded_list(minimum_bytes, MAX_DEVICES, "devices", get)
    }

    fn bounded_list<T>(
        &mut self,
        minimum_bytes: usize,
        maximum: usize,
        what: &'static str,
        mut get: impl FnMut(&mut Self) -> Result<T, Fault>,
    ) -> Result<Vec<T>, Fault> {
        let count = usize::try_from(self.u32()?)
            .map_err(|_| Fault::value(what, "count exceeds this platform"))?;
        if count > maximum {
            return Err(Fault::value(
                what,
                if what == "devices" {
                    "count exceeds the u16 device index address space"
                } else if what == "physical report.unavailable" {
                    "count exceeds the supported VM backend classes"
                } else if what == "learned priors" || what == "learned ceilings" {
                    "count exceeds a class's profiles"
                } else if what == "learned holdings" || what == "learned ends" {
                    "count exceeds a node's boxes"
                } else {
                    "count exceeds the route eligibility word"
                },
            ));
        }
        let bytes = count
            .checked_mul(minimum_bytes)
            .ok_or_else(|| Fault::value(what, "encoded list size overflows"))?;
        if bytes > self.rest.len() {
            return Err(Fault::Truncated {
                need: self.at().saturating_add(bytes),
                have: self.len,
            });
        }
        let mut out = Vec::new();
        for _ in 0..count {
            // Invalid tags/identities are refused before making room for that item. The count
            // preflight bounds work, but does not allocate a full caller-selected list up front.
            let item = get(self)?;
            out.try_reserve(1)
                .map_err(|_| Fault::value(what, "cannot reserve list"))?;
            out.push(item);
        }
        Ok(out)
    }
}

/// A path's bytes: a Unix path is any bytes, so it crosses as they are.
#[cfg(unix)]
fn path_bytes(p: &Path) -> Result<&[u8], Fault> {
    use std::os::unix::ffi::OsStrExt;
    Ok(p.as_os_str().as_bytes())
}

/// A path's bytes where paths aren't bytes: its UTF-8, or a refusal for a path that has none.
#[cfg(not(unix))]
fn path_bytes(p: &Path) -> Result<&[u8], Fault> {
    p.to_str()
        .map(str::as_bytes)
        .ok_or_else(|| Fault::value("path", format!("{} isn't Unicode", p.display())))
}

#[cfg(unix)]
fn path_of(b: &[u8]) -> Result<PathBuf, Fault> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(b)))
}

#[cfg(not(unix))]
fn path_of(b: &[u8]) -> Result<PathBuf, Fault> {
    std::str::from_utf8(b)
        .map(PathBuf::from)
        .map_err(|e| Fault::value("path", format!("isn't UTF-8: {e}")))
}

#[cfg(test)]
mod tests;
