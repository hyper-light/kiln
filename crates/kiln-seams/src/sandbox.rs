//! `Sandbox`: a harness launched in a boundary (CP-T4.1; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
//! step 17): its program, its folder and the folders shared with it, its environment, its limits
//! (memory, processes) and the hosts it may reach, with its stdio streamed back, the signals it's
//! sent, and its end said once, last. Its implementations are `host` (today's launch), the microVMs
//! kiln's harnesses run in (Shards' members, through Hex), then Hex's other classes (CP-T4.1,
//! CP-T7.2). A sandbox says what it holds its runs to ([`Holds`]): what it holds it enforces, and a
//! launch that asks for what it doesn't hold is refused `Unsupported`, never taken and ignored. The
//! suite asks its fixture for programs by what they do (echo, exit with a code, use memory, start
//! processes, connect, ask a host with a credential's placeholder), so each implementation supplies
//! its own; [`SimSandbox`] runs built-in programs that do those things. A run's credentials are
//! granted by reference ([`CredentialGrant`], Shards' D242; kiln plan 01a1d2b88be57304b972761e2a5c6781
//! step 6): their values are filled in at its egress, outside its boundary, toward the hosts each
//! names alone, and never travel in a launch.

use std::collections::VecDeque;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, fine, same};

/// What a run may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Its memory. `None`: no limit of its boundary's own (a process on the host, as kiln runs
    /// harnesses today; a microVM sized as its boundary sizes one).
    pub memory_bytes: Option<u64>,
    /// Its processes, itself among them; `None` as for memory.
    pub processes: Option<u32>,
    /// Its share of the CPU against others', on cgroup v2's scale ([`WEIGHT_MIN`] to
    /// [`WEIGHT_MAX`]; [`WEIGHT_EVEN`] is an even share): a build held below an agent.
    pub cpu_weight: u16,
    /// Its share of the disk, on the same scale (2026-10-19's builds starved a disk, not a CPU).
    pub io_weight: u16,
}

/// The weights' scale (cgroup v2's `cpu.weight`, `io.weight`).
pub const WEIGHT_MIN: u16 = 1;
pub const WEIGHT_MAX: u16 = 10_000;
/// An even share (cgroup v2's default `cpu.weight` and `io.weight`).
pub const WEIGHT_EVEN: u16 = 100;

impl Default for Limits {
    fn default() -> Self {
        Self { memory_bytes: Some(1 << 30), processes: Some(64), cpu_weight: WEIGHT_EVEN, io_weight: WEIGHT_EVEN }
    }
}

impl Limits {
    /// No limits of the boundary's own, and even shares: a process on the host, as today.
    pub const NONE: Limits = Limits { memory_bytes: None, processes: None, cpu_weight: WEIGHT_EVEN, io_weight: WEIGHT_EVEN };
}

/// The hosts a run may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Egress {
    /// Whatever its boundary itself lets it reach, narrowed no further: any host from the host's;
    /// a microVM's image's own grants from a microVM (its Agentfile's, default deny), never wider.
    Any,
    /// These alone, each `host` or `host:port`; none at all when empty.
    Only(Vec<String>),
}

impl Default for Egress {
    fn default() -> Self {
        Egress::Only(Vec::new())
    }
}

impl Egress {
    /// Whether it lets a run reach `host`: any host, or one it names alone or with a port
    /// (`host:port`), named in either case (RFC 4343: DNS names compare case-insensitively).
    fn reaches(&self, host: &str) -> bool {
        match self {
            Egress::Any => true,
            Egress::Only(hosts) => hosts.iter().any(|h| {
                let named = match h.rsplit_once(':') {
                    Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
                    _ => h.as_str(),
                };
                named.eq_ignore_ascii_case(host)
            }),
        }
    }
}

/// A credential a run is granted by reference (Shards' D242; kiln plan 01a1d2b88be57304b972761e2a5c6781
/// step 6): the secret `reference` names in kiln's store ([`crate::Secrets`]), never its value, which
/// is filled in at the run's egress, outside its boundary, into its requests to each of `toward`'s
/// hosts and to no other, where `placeholder` stands. A grant grants no egress: each host is one its
/// launch's egress reaches.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialGrant {
    pub reference: String,
    pub toward: Vec<String>,
    /// What stands for the credential in the run's requests (Shards' D242): minted by whoever makes
    /// the launch, which gives it to the run where the run reads its credential (its environment), a
    /// token the run sends where the value would go (RFC 6750 §2.1's `b64token`). Holding it is the
    /// capability, so it never prints.
    pub placeholder: String,
}

/// Shows its reference and hosts, and of its placeholder its length alone.
impl std::fmt::Debug for CredentialGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialGrant").field("reference", &self.reference).field("toward", &self.toward).field("placeholder", &format_args!("{} bytes", self.placeholder.len())).finish()
    }
}

/// Whether `placeholder` is a token a request carries a credential as: RFC 6750 §2.1's `b64token`,
/// `1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`, as Hex and Shards take one.
fn token(placeholder: &str) -> bool {
    let body = placeholder.trim_end_matches('=');
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/'))
}

impl CredentialGrant {
    /// Whether it can ever be filled in for `launch`: a reference its store could name
    /// ([`crate::secrets::check_name`]), and one host at least, each named as TLS names a server
    /// (a DNS name, never an address, and without a port: RFC 6066 §3) and reached by `launch`'s
    /// egress; else `Invalid`, naming it by its place among `launch`'s grants.
    fn check(&self, at: usize, launch: &Launch) -> Result<(), Refusal> {
        let invalid = |why: String| Err(Refusal::Invalid { what: "a credential grant", why: format!("credentials[{at}]: {why}") });
        if let Err(e) = crate::secrets::check_name(&self.reference) {
            return invalid(format!("its reference: {e}"));
        }
        if self.toward.is_empty() {
            return invalid(format!("{} toward no host could never be filled in", self.reference));
        }
        if !token(&self.placeholder) {
            return invalid(format!("{}'s placeholder isn't a token a request carries a credential as (RFC 6750 §2.1's b64token)", self.reference));
        }
        for host in &self.toward {
            if host.is_empty() || host.contains(|c: char| c == ':' || c == '/' || c.is_whitespace()) || host.parse::<std::net::IpAddr>().is_ok() {
                return invalid(format!("{host:?} is no host a credential is filled in toward: a DNS name, without a port (RFC 6066 §3)"));
            }
            if !launch.egress.reaches(host) {
                return invalid(format!("{host} is no host its egress reaches: a credential grant grants no egress"));
            }
        }
        Ok(())
    }
}

/// A folder of the machine's a run sees: `host`'s files at `at`. At its own path (`at` is `host`),
/// what the run says of its files names them as the machine does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub host: String,
    pub at: String,
    pub writable: bool,
    /// In a microVM, the members it's given to at their start, at `at` (Shards' share member scope, as
    /// a plug's `PlugFor::Members`): none, its microVM's own root alone, which no member sees. A run on
    /// the host has no members, and every share is its process's.
    pub members: Vec<Member>,
}

impl Share {
    /// `path`, writable, at its own path, given to no member.
    pub fn at_its_path(path: &str) -> Self {
        Self { host: path.to_owned(), at: path.to_owned(), writable: true, members: Vec::new() }
    }
}

/// A signal a run's program is sent: to its process group on the host, to a microVM's member
/// through its boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT: stop what it's doing.
    Interrupt,
    /// SIGTERM: end, in its own time.
    Terminate,
}

impl Signal {
    /// Its number on Linux (signal(7)): what a microVM's Linux delivers, and what [`SimSandbox`]'s
    /// runs end by.
    pub const fn number(self) -> i32 {
        match self {
            Signal::Interrupt => 2,
            Signal::Terminate => 15,
        }
    }

    /// The code of a run it ended: 128 plus its number, as a POSIX shell says a command a signal
    /// ended (POSIX.1-2017, Shell Command Language §2.8.2), and as Docker reports one.
    pub const fn ended_code(self) -> i32 {
        match self {
            Signal::Interrupt => 130,
            Signal::Terminate => 143,
        }
    }
}

/// What a sandbox holds its runs to. What it holds it enforces; a launch that asks for what it
/// doesn't hold is refused [`Refusal::Unsupported`], never taken and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Holds {
    /// A memory limit: a run over it ends `OverLimit(Memory)`.
    pub memory: bool,
    /// A process limit: a run refused one past it, that then fails, ends `OverLimit(Processes)`.
    pub processes: bool,
    /// Shares of the CPU and the disk other than an even one.
    pub weights: bool,
    /// No egress at all.
    pub no_egress: bool,
    /// Egress to named hosts alone.
    pub egress_by_host: bool,
    /// A folder shared at another path than its own, or read-only.
    pub shares: bool,
    /// Signals to a run ([`Run::signal`]).
    pub signals: bool,
    /// Credentials granted by reference, filled in at a run's egress outside its boundary
    /// ([`Launch::credentials`]).
    pub credentials: bool,
    /// Changes to a run's network and folders while it runs ([`Run::change`]: kiln plan 01a1dd2e…, P6b.4).
    pub live: bool,
    /// A run's member swapped for another in its boundary ([`Run::swap`]: kiln plan 01a1dd2e…, P6c.5).
    pub swaps: bool,
    /// Components laid over its image's own: at a launch ([`Launch::components`]), and swapped in and given back while
    /// it runs ([`Change::Swap`], [`Change::Restore`]: kiln plan 01a1dd2e…, P6c.3).
    pub components: bool,
}

impl Holds {
    /// Everything (the simulator's).
    pub const ALL: Holds = Holds { memory: true, processes: true, weights: true, no_egress: true, egress_by_host: true, shares: true, signals: true, credentials: true, live: true, swaps: true, components: true };

    /// `Ok` when `launch`'s weights are on their scale, its credential grants could each be filled
    /// in, and it asks only for what these hold; else its refusal, `Invalid` or `Unsupported`,
    /// naming what it asked for.
    pub fn check(&self, launch: &Launch) -> Result<(), Refusal> {
        let l = &launch.limits;
        for (what, w) in [("a cpu weight", l.cpu_weight), ("an io weight", l.io_weight)] {
            if !(WEIGHT_MIN..=WEIGHT_MAX).contains(&w) {
                return Err(Refusal::Invalid { what, why: format!("{w} (from {WEIGHT_MIN} to {WEIGHT_MAX})") });
            }
        }
        for (at, grant) in launch.credentials.iter().enumerate() {
            grant.check(at, launch)?;
        }
        let refuse = |what: &str| Err(Refusal::Unsupported(what.to_owned()));
        if l.memory_bytes.is_some() && !self.memory {
            return refuse("a memory limit");
        }
        if l.processes.is_some() && !self.processes {
            return refuse("a process limit");
        }
        if (l.cpu_weight != WEIGHT_EVEN || l.io_weight != WEIGHT_EVEN) && !self.weights {
            return refuse("a share of the CPU or the disk other than an even one");
        }
        if !self.shares && launch.shares.iter().any(|s| s.at != s.host || !s.writable) {
            return refuse("a folder shared at another path or read-only");
        }
        match &launch.egress {
            Egress::Any => {}
            Egress::Only(hosts) if hosts.is_empty() && !self.no_egress => return refuse("a run with no egress"),
            Egress::Only(hosts) if !hosts.is_empty() && !self.egress_by_host => return refuse("egress to named hosts alone"),
            Egress::Only(_) => {}
        }
        if !launch.credentials.is_empty() && !self.credentials {
            return refuse("credentials filled in at its egress");
        }
        if !launch.components.is_empty() && !self.components {
            return refuse("components laid over its image's own");
        }
        Ok(())
    }
}

/// Where a run is held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    /// A process on the host, as kiln runs harnesses today.
    Host,
    /// An OCI image, run rootless (Hex's `oci` class).
    Container { image: String },
    /// A microVM from `image`: an Agentfile's, whose members run in their own domains as it declares
    /// them (Shards' D231), through Hex.
    MicroVm { image: String },
}

/// What a component of a microVM's image is: an Agentfile's AGENT, HARNESS or MCP install (Shards' D231 members).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Agent,
    Harness,
    Mcp,
}

/// A component built alone from its pinned source (kiln plan 01a1dd2e…, P6c.1 and P6c.3), as an Agentfile's AGENT,
/// HARNESS or MCP takes its source (Shards' D54): `source` as its `FROM` writes it (a Git URL whose `#REF` names its
/// commit, an http(s) URL, `s3://BUCKET/KEY`, or an OCI reference by its digest), and its pin where its source isn't
/// pinned in itself: `checksum` (`sha256:<hex>` of what an http(s) or s3 source serves) and `version_id` (an s3
/// object's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub kind: ComponentKind,
    pub source: String,
    pub checksum: Option<String>,
    pub version_id: Option<String>,
}

/// A component laid over its image's own at `at`, its member's directory there (`/harness/<name>`, `/agents/<name>`,
/// `/mcp/<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentAt {
    pub at: String,
    pub component: Component,
}

/// A run to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// What runs: a program by its path, or by a name its boundary finds it by (the host's `PATH`);
    /// in a microVM, the member of its image that runs (an Agentfile's HARNESS or AGENT, by name),
    /// which runs its image's command for it.
    pub program: String,
    /// Its arguments; in a microVM, those past its image's command for the member (none: the
    /// member runs as its image declares it).
    pub args: Vec<String>,
    /// Its working folder, which it may write.
    pub folder: Option<String>,
    /// More of the machine's folders it sees.
    pub shares: Vec<Share>,
    /// Its environment, over its boundary's own: the machine's on the host, its image's in a
    /// container or a microVM. Secrets don't belong here: they're injected outside the boundary
    /// (CP-T4.3), as its [`credentials`](Self::credentials) are.
    pub env: Vec<(String, String)>,
    pub limits: Limits,
    /// The hosts it may reach; every other is refused.
    pub egress: Egress,
    pub boundary: Boundary,
    /// Its credentials, granted by reference (Shards' D242): each filled in at its egress, outside
    /// its boundary, toward the hosts its grant names alone; none by default, when nothing is
    /// filled in toward any host. A value is never here.
    pub credentials: Vec<CredentialGrant>,
    /// Where a folder may be plugged into it while it runs (kiln plan 01a1dd2e…, P6b.4; Shards'
    /// `ImageSpec::plug_roots`, D271): top-level directories of its boundary (`/volumes`), none of
    /// its init's own (`/proc`, `/sys`, `/dev`, `/tmp`, `/etc`, `/run`), each once; beside them,
    /// each share's top-level directory. A plug ([`Change::Plug`]) goes strictly beneath one.
    pub plug_roots: Vec<String>,
    /// The most egress it may ever be granted while it runs (Shards' `ImageSpec::egress_ceiling`),
    /// each `host:port`: a grant past it is refused. None: none is granted live.
    pub egress_ceiling: Vec<String>,
    /// Components laid over its image's own, each at its member's directory, as its members start (kiln plan
    /// 01a1dd2e…, P6c.3; Shards' D270): those a person swapped in while it ran before, and the one a [`Run::swap`] of
    /// its member gives it. None: its image's own.
    pub components: Vec<ComponentAt>,
}

impl Launch {
    /// `program` with `args`, on the host, default limits, no egress.
    pub fn of(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            folder: None,
            shares: Vec::new(),
            env: Vec::new(),
            limits: Limits::default(),
            egress: Egress::default(),
            boundary: Boundary::Host,
            credentials: Vec::new(),
            plug_roots: Vec::new(),
            egress_ceiling: Vec::new(),
            components: Vec::new(),
        }
    }
}

/// Which limit a run went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Memory,
    Processes,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed, through `Run::kill`.
    Killed,
    /// Ended by a limit: memory when its boundary killed it for going over (cgroup v2's
    /// `memory.events` counting an `oom_kill`); processes when it was refused one past its limit
    /// (`pids.max`: the fork fails, the run isn't stopped) and then failed. A run refused a process
    /// that coped and ended well ends `Code(0)`.
    OverLimit(Limit),
}

/// What a run says, in order: its output, then its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exited(Exit),
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no program {0:?}")]
    NoSuchProgram(String),
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("this sandbox can't hold {0}")]
    Unsupported(String),
    #[error("the sandbox runs its most ({limit})")]
    Full { limit: usize },
    #[error("the run has ended")]
    Ended,
}

/// The link a run's stdio rides into its boundary (a microVM's: Shards' agent control, kiln plan
/// 01a1cd8e862e7169a074db3d5fa33f3d step 11), as each end says it negotiated
/// ([`kiln_ci::facts::Negotiated`]): the controller's, on the machine, and the endpoint's own report
/// from inside; and the lanes its controller was granted on it as it dialed, by Shards' names
/// (`control`, `stdout:0`, `stderr:0`, `stdin:0`, `exec`, `socket`, `mcp:0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub controller: kiln_ci::facts::Negotiated,
    pub endpoint: kiln_ci::facts::Negotiated,
    pub lanes: Vec<String>,
}

/// A member of a microVM's image a change is for (Shards' D231: its HARNESS, an AGENT or an MCP
/// server), by its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member(pub String);

/// A change to a run's network or folders while it runs ([`Run::change`]; kiln plan 01a1dd2e…, P6b.4,
/// through Hex's wire calls for Shards' P6b.1 and P6b.2): every grant explicit, who to whom, which
/// ports and which way, as a launch's are (default deny).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Its `guest_port` (UDP, else TCP) published on the host at `host` (an address; none, every
    /// one) and `host_port` (0: one the host picks), reaching the members its image gives that port
    /// coming in, as Shards' `ImageGuest::publish` takes one (D269).
    Publish { guest_port: u16, udp: bool, host: Option<String>, host_port: u16 },
    /// The port it publishes at `host` (`address:port`, as bound) let go of, as Shards'
    /// `ImageGuest::unpublish` takes one (D269).
    Unpublish { udp: bool, host: String },
    /// `member` may reach `host` (a DNS name or an address) on `port`.
    Grant { member: Member, host: String, port: u16 },
    /// That grant taken back: its next connection there is refused.
    Revoke { member: Member, host: String, port: u16 },
    /// `member` joins the named `network`, shared with the runs connected to it.
    Connect { member: Member, network: String },
    Disconnect { member: Member, network: String },
    /// `source` plugged into the members `to` at `at` (absolute, strictly beneath one of its plug
    /// roots: its launch's [`Launch::plug_roots`] and each share's top-level directory), read-only
    /// where it says, as Shards' `ImageGuest::plug` takes one (D271).
    Plug { source: PlugSource, to: PlugFor, at: String, read_only: bool },
    /// The folder plugged in at `at`, unplugged.
    Unplug { at: String },
    /// The component at `at`, a member's directory in its image, swapped for `component` (kiln plan 01a1dd2e…, P6c.3;
    /// Shards' P6c.1, through Hex's P6c.2): laid over the image's own read-only, and that member alone started again
    /// with it at once, the run's other members running on. The run's own member is swapped through [`Run::swap`],
    /// whose launch lays it ([`Launch::components`]).
    Swap { at: String, component: Component },
    /// The image's own component back at `at`, its member started again with it at once.
    Restore { at: String },
}

/// What a [`Change::Plug`] gives the members it plugs into, as Shards takes one (D271).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlugSource {
    /// A folder of the machine's, absolute.
    Host(String),
    /// A volume of its boundary's own store, by its name, made where it isn't there.
    Volume(String),
}

/// Whom a [`Change::Plug`] plugs into, as Shards takes one (D271): every member that runs, or those
/// named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlugFor {
    Every,
    Members(Vec<Member>),
}

/// A run under way: what it's given on stdin, what it says (its output, then its end, once), the
/// signals it's sent, a way to end it, and where its boundary holds it.
pub trait Run: Send + Sync {
    /// Gives the program `bytes` on its stdin, after what it was given before; refused `Ended`
    /// once the run has ended.
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Closes its stdin: the program reads the end of its input.
    fn close_stdin(&self);
    /// What it says next; `None` once its end has been given.
    fn next(&self) -> BoxFuture<'_, Option<Output>>;
    /// Sends its program `signal`, which it handles as it does (most end); refused `Ended` once it
    /// has ended, and `Unsupported` where its boundary delivers none, the run going on unsignalled.
    fn signal(&self, signal: Signal) -> Result<(), Refusal>;
    /// Ends it, unless it has ended: its end is then `Exited(Killed)`. A microVM's run ends with its
    /// microVM.
    fn kill(&self);
    /// Its boundary's own name for it, where its boundary names its runs (a microVM's, as its runtime
    /// names it); none for a process of the machine's, which its pid names. The same each time.
    fn id(&self) -> Option<String>;
    /// The link its stdio rides into its boundary, as each end says it negotiated, once it's made: it
    /// waits for the link's handshake and its endpoint's report. None at once for a run whose stdio
    /// ride no link (a process of the machine's, whose stdio are its pipes), and none where its link
    /// ends before it's made. Asked again, it says the same.
    fn link(&self) -> BoxFuture<'_, Option<Link>>;
    /// Changes its network or its folders while it runs ([`Change`]): taken, else refused: `Ended`
    /// once it has ended, `Unsupported` where its boundary takes no change live (`Holds::live`, and
    /// a run's own unless it says otherwise), and in its runtime's words where that refuses it.
    fn change(&self, change: Change) -> BoxFuture<'_, Result<(), Refusal>> {
        drop(change);
        Box::pin(async { Err(Refusal::Unsupported("a change to a run while it runs".into())) })
    }
    /// Its member swapped for `launch`'s in the same boundary (kiln plan 01a1dd2e…, P6c.5; Shards'
    /// P6c.1 component attached through Hex's P6c.2): its member ended as its input ends, then
    /// `launch`'s member started in its place, given what `launch` gives it. The run returned is the
    /// new member's, its boundary the same (the same [`id`](Self::id)); this one says its end, and its
    /// end and its kill end nothing more of its boundary. Refused `Unsupported` where its boundary
    /// swaps none (`Holds::swaps`, and a run's own unless it says otherwise), `Ended` once it has
    /// ended, and in its runtime's words where that refuses it: refused, this run goes on.
    fn swap(&self, launch: Launch) -> BoxFuture<'_, Result<Box<dyn Run>, Refusal>> {
        drop(launch);
        Box::pin(async { Err(Refusal::Unsupported("a member swapped for another in its boundary".into())) })
    }
}

/// Harnesses launched in boundaries.
pub trait Sandbox: Send + Sync + 'static {
    /// Starts `launch` in its boundary, within its limits; refused, typed, when it can't.
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>>;
    /// The boundaries it can hold runs in, by name (`host`, `container`, `microvm`).
    fn boundaries(&self) -> Vec<&'static str>;
    /// What it holds its runs to; a launch asking for more is refused `Unsupported`.
    fn holds(&self) -> Holds;
}

// ---------------------------------------------------------------------------------- simulator

/// Most runs a [`SimSandbox`] holds at once.
pub const SIM_RUNS: usize = 1000;
/// Most output a simulated run keeps unread; past it, the run is killed (its reader too slow).
pub const SIM_PENDING: usize = 10_000;

/// Built-in programs, run in memory:
///
/// - `say <words…>`: writes them to stdout, exits 0;
/// - `echo`: writes back what comes in on stdin, exits 0 once it closes;
/// - `fail <code>`: writes to stderr, exits with `code`;
/// - `wait`: runs until killed;
/// - `hog <bytes>`: uses that much memory, then exits 0;
/// - `spawn <n>`: starts `n` processes, then exits 0;
/// - `connect <host>`: exits 0 when the host is granted (or any is), 1 when it's refused;
/// - `fill <host> <reference>`: asks the host with the credential's placeholder; exits 0, writing
///   `filled`, when a grant of the credential toward that host fills it in, else 1, the request
///   gone bare (or refused, where its egress doesn't reach the host);
/// - `pwd`: writes its working folder;
/// - `components`: writes the components laid for it, one a line (`<at> <source>`).
///
/// A signal ends any of them with its [`Signal::ended_code`]. It holds everything, unless made
/// [`SimSandbox::holding`] less, as a real boundary may.
#[derive(Clone)]
pub struct SimSandbox {
    runs: Arc<Mutex<usize>>,
    holds: Holds,
}

impl Default for SimSandbox {
    fn default() -> Self {
        Self { runs: Arc::default(), holds: Holds::ALL }
    }
}

impl SimSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// One that holds only `holds`, refusing the rest `Unsupported`.
    pub fn holding(holds: Holds) -> Self {
        Self { holds, ..Self::default() }
    }
}

struct SimRun {
    state: Arc<Mutex<RunState>>,
    said: Arc<event_listener::Event>,
    runs: Arc<Mutex<usize>>,
    signals: bool,
    live: bool,
    /// Whether it takes components swapped in while it runs ([`Holds::components`]).
    components: bool,
    /// Its sandbox, which starts a member it's swapped for ([`Run::swap`]).
    sandbox: SimSandbox,
}

#[derive(Default)]
struct RunState {
    out: VecDeque<Output>,
    echo: bool,
    ended: bool,
    given_end: bool,
    /// The changes it took while it ran ([`Run::change`]), in order.
    changes: Vec<Change>,
}

impl RunState {
    fn say(&mut self, o: Output) {
        if self.ended {
            return;
        }
        if self.out.len() >= SIM_PENDING {
            self.out.push_back(Output::Exited(Exit::Killed));
            self.ended = true;
            return;
        }
        if matches!(o, Output::Exited(_)) {
            self.ended = true;
        }
        self.out.push_back(o);
    }
}

impl Drop for SimRun {
    fn drop(&mut self) {
        let mut n = self.runs.lock();
        *n = n.saturating_sub(1);
    }
}

impl Run for SimRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = {
            let mut st = self.state.lock();
            if st.ended {
                Err(Refusal::Ended)
            } else {
                if st.echo {
                    st.say(Output::Stdout(bytes));
                }
                Ok(())
            }
        };
        self.said.notify(usize::MAX);
        Box::pin(async move { out })
    }

    fn close_stdin(&self) {
        {
            let mut st = self.state.lock();
            if st.echo {
                st.say(Output::Exited(Exit::Code(0)));
            }
        }
        self.said.notify(usize::MAX);
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        Box::pin(async move {
            loop {
                let listener = self.said.listen();
                {
                    let mut st = self.state.lock();
                    if let Some(o) = st.out.pop_front() {
                        if matches!(o, Output::Exited(_)) {
                            st.given_end = true;
                        }
                        return Some(o);
                    }
                    if st.given_end {
                        return None;
                    }
                }
                listener.await;
            }
        })
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        {
            let mut st = self.state.lock();
            if st.ended {
                return Err(Refusal::Ended);
            }
            if !self.signals {
                return Err(Refusal::Unsupported("a signal to a run".into()));
            }
            st.say(Output::Exited(Exit::Code(signal.ended_code())));
        }
        self.said.notify(usize::MAX);
        Ok(())
    }

    fn kill(&self) {
        self.state.lock().say(Output::Exited(Exit::Killed));
        self.said.notify(usize::MAX);
    }

    // Its runs are the host boundary's: no name of a boundary's, and their stdio no link.
    fn id(&self) -> Option<String> {
        None
    }

    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        Box::pin(async { None })
    }

    fn change(&self, change: Change) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = {
            let mut st = self.state.lock();
            let component = matches!(change, Change::Swap { .. } | Change::Restore { .. });
            if st.ended {
                Err(Refusal::Ended)
            } else if component && !self.components {
                Err(Refusal::Unsupported("a component swapped into a run while it runs".into()))
            } else if !component && !self.live {
                Err(Refusal::Unsupported("a change to a run while it runs".into()))
            } else {
                st.changes.push(change);
                Ok(())
            }
        };
        Box::pin(async move { out })
    }

    fn swap(&self, launch: Launch) -> BoxFuture<'_, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            if self.state.lock().ended {
                return Err(Refusal::Ended);
            }
            if !self.sandbox.holds.swaps {
                return Err(Refusal::Unsupported("a member swapped for another in its boundary".into()));
            }
            let new = self.sandbox.launch(launch).await?;
            // Its member ended as the new one started: its end said, once.
            self.state.lock().say(Output::Exited(Exit::Code(0)));
            self.said.notify(usize::MAX);
            Ok(new)
        })
    }
}

impl Sandbox for SimSandbox {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        let out = (|| {
            self.holds().check(&launch)?;
            match launch.boundary {
                Boundary::Host => {}
                Boundary::Container { .. } => return Err(Refusal::Unsupported("a run in a container".into())),
                Boundary::MicroVm { .. } => return Err(Refusal::Unsupported("a run in a microVM".into())),
            }
            let arg = |i: usize| launch.args.get(i).cloned().unwrap_or_default();
            let mut st = RunState::default();
            match launch.program.as_str() {
                "say" => {
                    st.say(Output::Stdout(launch.args.join(" ").into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                "echo" => st.echo = true,
                "fail" => {
                    let code: i32 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "fail's code", why: arg(0) })?;
                    st.say(Output::Stderr(format!("failing with {code}").into_bytes()));
                    st.say(Output::Exited(Exit::Code(code)));
                }
                "wait" => {}
                "hog" => {
                    let bytes: u64 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "hog's bytes", why: arg(0) })?;
                    let over = launch.limits.memory_bytes.is_some_and(|m| bytes > m);
                    st.say(if over { Output::Exited(Exit::OverLimit(Limit::Memory)) } else { Output::Exited(Exit::Code(0)) });
                }
                "spawn" => {
                    let n: u32 = arg(0).parse().map_err(|_| Refusal::Invalid { what: "spawn's count", why: arg(0) })?;
                    // It and what it starts.
                    let over = launch.limits.processes.is_some_and(|p| n.saturating_add(1) > p);
                    st.say(if over { Output::Exited(Exit::OverLimit(Limit::Processes)) } else { Output::Exited(Exit::Code(0)) });
                }
                "connect" => {
                    let host = arg(0);
                    let granted = match &launch.egress {
                        Egress::Any => true,
                        Egress::Only(hosts) => hosts.contains(&host),
                    };
                    if granted {
                        st.say(Output::Stdout(format!("connected to {host}").into_bytes()));
                        st.say(Output::Exited(Exit::Code(0)));
                    } else {
                        st.say(Output::Stderr(format!("{host}: refused").into_bytes()));
                        st.say(Output::Exited(Exit::Code(1)));
                    }
                }
                "fill" => {
                    let (host, reference) = (arg(0), arg(1));
                    let filled = launch.egress.reaches(&host) && launch.credentials.iter().any(|g| g.reference == reference && g.toward.iter().any(|h| h.eq_ignore_ascii_case(&host)));
                    if filled {
                        st.say(Output::Stdout(b"filled".to_vec()));
                        st.say(Output::Exited(Exit::Code(0)));
                    } else {
                        st.say(Output::Stderr(format!("{host}: no credential filled in").into_bytes()));
                        st.say(Output::Exited(Exit::Code(1)));
                    }
                }
                "pwd" => {
                    st.say(Output::Stdout(launch.folder.clone().unwrap_or_else(|| "/".into()).into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                "components" => {
                    st.say(Output::Stdout(launch.components.iter().map(|c| format!("{} {}\n", c.at, c.component.source)).collect::<String>().into_bytes()));
                    st.say(Output::Exited(Exit::Code(0)));
                }
                other => return Err(Refusal::NoSuchProgram(other.to_owned())),
            }
            let mut n = self.runs.lock();
            if *n >= SIM_RUNS {
                return Err(Refusal::Full { limit: SIM_RUNS });
            }
            *n = n.saturating_add(1);
            let run: Box<dyn Run> = Box::new(SimRun { state: Arc::new(Mutex::new(st)), said: Arc::new(event_listener::Event::new()), runs: Arc::clone(&self.runs), signals: self.holds.signals, live: self.holds.live, components: self.holds.components, sandbox: self.clone() });
            Ok(run)
        })();
        Box::pin(async move { out })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["host"]
    }

    fn holds(&self) -> Holds {
        self.holds
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Sandbox` conformance suite runs against: launches for programs that do what each case
/// needs, in this sandbox's terms.
pub trait SandboxFixture: Send + Sync {
    fn sandbox(&self) -> &dyn Sandbox;
    /// Writes `text` to stdout and exits 0.
    fn say(&self, text: &str) -> Launch;
    /// Writes back what comes in on stdin; exits 0 once it closes.
    fn echo(&self) -> Launch;
    /// Writes to stderr and exits with `code`.
    fn fail(&self, code: i32) -> Launch;
    /// Runs until killed, or a signal ends it.
    fn wait(&self) -> Launch;
    /// Uses `bytes` of memory, under a limit of `limit` bytes (none: no limit).
    fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch;
    /// Starts `n` processes, under a limit of `limit` (none: no limit), and fails when one is
    /// refused it.
    fn spawn(&self, n: u32, limit: Option<u32>) -> Launch;
    /// Connects to `host` (the suite's names, `api.anthropic.com` and `example.com`, which the
    /// fixture may serve itself), granted `egress`.
    fn connect(&self, host: &str, egress: Egress) -> Launch;
    /// Asks `host` (one of the suite's names) with credential `reference`'s placeholder, granted
    /// `credentials` and `egress`: exits 0 when its request reached the host with the credential's
    /// value filled in, else nonzero. The suite's credentials are its own (`conformance/...`),
    /// whose values the fixture keeps.
    fn fill(&self, host: &str, reference: &str, credentials: Vec<CredentialGrant>, egress: Egress) -> Launch;
    /// Writes its working folder, given `folder`.
    fn pwd(&self, folder: &str) -> Launch;
    /// Writes the components laid for it, `laid`, one a line: `<at> <source>`.
    fn components(&self, laid: Vec<ComponentAt>) -> Launch;
    /// A folder that exists for a run.
    fn folder(&self) -> String;
}

/// The fixture for a [`SimSandbox`].
#[derive(Default)]
pub struct SimSandboxFixture(pub SimSandbox);

impl SandboxFixture for SimSandboxFixture {
    fn sandbox(&self) -> &dyn Sandbox {
        &self.0
    }
    fn say(&self, text: &str) -> Launch {
        Launch::of("say", &[text])
    }
    fn echo(&self) -> Launch {
        Launch::of("echo", &[])
    }
    fn fail(&self, code: i32) -> Launch {
        Launch::of("fail", &[&code.to_string()])
    }
    fn wait(&self) -> Launch {
        Launch::of("wait", &[])
    }
    fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch {
        Launch { limits: Limits { memory_bytes: limit, ..Limits::default() }, ..Launch::of("hog", &[&bytes.to_string()]) }
    }
    fn spawn(&self, n: u32, limit: Option<u32>) -> Launch {
        Launch { limits: Limits { processes: limit, ..Limits::default() }, ..Launch::of("spawn", &[&n.to_string()]) }
    }
    fn connect(&self, host: &str, egress: Egress) -> Launch {
        Launch { egress, ..Launch::of("connect", &[host]) }
    }
    fn fill(&self, host: &str, reference: &str, credentials: Vec<CredentialGrant>, egress: Egress) -> Launch {
        Launch { egress, credentials, ..Launch::of("fill", &[host, reference]) }
    }
    fn pwd(&self, folder: &str) -> Launch {
        Launch { folder: Some(folder.to_owned()), ..Launch::of("pwd", &[]) }
    }
    fn components(&self, laid: Vec<ComponentAt>) -> Launch {
        Launch { components: laid, ..Launch::of("components", &[]) }
    }
    fn folder(&self) -> String {
        "/work/conformance".into()
    }
}

/// Everything a run says until its end (at most 1000 outputs).
async fn drain(run: &dyn Run) -> Vec<Output> {
    let mut out = Vec::new();
    while out.len() < 1000 {
        match run.next().await {
            Some(o) => out.push(o),
            None => break,
        }
    }
    out
}

fn stdout(outs: &[Output]) -> Vec<u8> {
    outs.iter().filter_map(|o| if let Output::Stdout(b) = o { Some(b.clone()) } else { None }).flatten().collect()
}

fn end(outs: &[Output]) -> Option<Exit> {
    match outs.last() {
        Some(Output::Exited(e)) => Some(*e),
        _ => None,
    }
}

fn c1<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.say("hello there")).await)?;
        let outs = drain(run.as_ref()).await;
        same("its stdout", String::from_utf8_lossy(&stdout(&outs)).trim().to_owned(), "hello there".to_owned())?;
        same("its end", end(&outs), Some(Exit::Code(0)))?;
        same("after its end", run.next().await, None)
    })
}

fn c2<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.echo()).await)?;
        for line in ["one\n", "two\n", "three\n"] {
            fine("stdin", run.stdin(line.as_bytes().to_vec()).await)?;
        }
        run.close_stdin();
        let outs = drain(run.as_ref()).await;
        same("what came back", String::from_utf8_lossy(&stdout(&outs)).into_owned(), "one\ntwo\nthree\n".to_owned())?;
        same("its end once stdin closed", end(&outs), Some(Exit::Code(0)))
    })
}

fn c3<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.fail(3)).await)?;
        let outs = drain(run.as_ref()).await;
        ensure(outs.iter().any(|o| matches!(o, Output::Stderr(b) if !b.is_empty())), || "nothing on stderr".into())?;
        same("its end", end(&outs), Some(Exit::Code(3)))?;
        same("ends said", outs.iter().filter(|o| matches!(o, Output::Exited(_))).count(), 1)
    })
}

fn c4<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        run.kill();
        let outs = drain(run.as_ref()).await;
        same("its end", end(&outs), Some(Exit::Killed))?;
        ensure(run.stdin(b"late".to_vec()).await.is_err(), || "stdin to an ended run was taken".into())
    })
}

/// `Ok` when `v` was refused `Unsupported`, else a failure saying what it was.
fn unsupported(what: &str, v: Result<Box<dyn Run>, Refusal>) -> Result<(), Failure> {
    match v {
        Err(Refusal::Unsupported(_)) => Ok(()),
        Err(e) => Err(format!("{what}: refused as {e:?}, not Unsupported")),
        Ok(_) => Err(format!("{what}: taken, though the sandbox says it doesn't hold it")),
    }
}

fn c5<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !f.sandbox().holds().memory {
            unsupported("a memory limit", f.sandbox().launch(f.hog(4 << 20, Some(64 << 20))).await)?;
            let free = fine("launch", f.sandbox().launch(f.hog(4 << 20, None)).await)?;
            return same("with no limit", end(&drain(free.as_ref()).await), Some(Exit::Code(0)));
        }
        let over = fine("launch", f.sandbox().launch(f.hog(64 << 20, Some(16 << 20))).await)?;
        same("over its memory limit", end(&drain(over.as_ref()).await), Some(Exit::OverLimit(Limit::Memory)))?;
        let under = fine("launch", f.sandbox().launch(f.hog(4 << 20, Some(64 << 20))).await)?;
        same("under it", end(&drain(under.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c6<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !f.sandbox().holds().processes {
            unsupported("a process limit", f.sandbox().launch(f.spawn(2, Some(8))).await)?;
            let free = fine("launch", f.sandbox().launch(f.spawn(2, None)).await)?;
            return same("with no limit", end(&drain(free.as_ref()).await), Some(Exit::Code(0)));
        }
        let over = fine("launch", f.sandbox().launch(f.spawn(20, Some(8))).await)?;
        same("over its process limit", end(&drain(over.as_ref()).await), Some(Exit::OverLimit(Limit::Processes)))?;
        let under = fine("launch", f.sandbox().launch(f.spawn(2, Some(8))).await)?;
        same("under it", end(&drain(under.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c7<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let holds = f.sandbox().holds();
        let api = || Egress::Only(vec!["api.anthropic.com".to_owned()]);
        if holds.egress_by_host {
            let denied = fine("launch", f.sandbox().launch(f.connect("example.com", api())).await)?;
            ensure(!matches!(end(&drain(denied.as_ref()).await), Some(Exit::Code(0))), || "a host not granted was reached".into())?;
            let granted = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", api())).await)?;
            same("a granted host", end(&drain(granted.as_ref()).await), Some(Exit::Code(0)))?;
        } else {
            unsupported("egress to named hosts", f.sandbox().launch(f.connect("api.anthropic.com", api())).await)?;
        }
        if holds.no_egress {
            let cut = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", Egress::Only(Vec::new()))).await)?;
            ensure(!matches!(end(&drain(cut.as_ref()).await), Some(Exit::Code(0))), || "a run granted no host reached one".into())?;
        } else {
            unsupported("no egress", f.sandbox().launch(f.connect("api.anthropic.com", Egress::Only(Vec::new()))).await)?;
        }
        let any = fine("launch", f.sandbox().launch(f.connect("api.anthropic.com", Egress::Any)).await)?;
        same("a host, granted any", end(&drain(any.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c8<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let mut l = f.say("x");
        l.program = "conformance-no-such-program".into();
        match f.sandbox().launch(l).await {
            Err(Refusal::NoSuchProgram(_)) => Ok(()),
            Err(e) => Err(format!("refused as {e:?}, not NoSuchProgram")),
            Ok(_) => Err("a program that doesn't exist was launched".into()),
        }
    })
}

fn c9<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let folder = f.folder();
        let run = fine("launch", f.sandbox().launch(f.pwd(&folder)).await)?;
        let outs = drain(run.as_ref()).await;
        same("its working folder", String::from_utf8_lossy(&stdout(&outs)).trim().to_owned(), folder)
    })
}

fn c10<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        for (cpu, io) in [(0, 100), (100, 0), (WEIGHT_MAX.saturating_add(1), 100), (100, WEIGHT_MAX.saturating_add(1))] {
            let mut l = f.say("x");
            l.limits.cpu_weight = cpu;
            l.limits.io_weight = io;
            match f.sandbox().launch(l).await {
                Err(Refusal::Invalid { .. }) => {}
                Err(e) => return Err(format!("weights {cpu}/{io} refused as {e:?}, not Invalid")),
                Ok(_) => return Err(format!("weights {cpu}/{io} were taken")),
            }
        }
        let mut l = f.say("weighted");
        (l.limits.cpu_weight, l.limits.io_weight) = (WEIGHT_MIN, WEIGHT_MAX);
        if !f.sandbox().holds().weights {
            unsupported("weights other than an even share", f.sandbox().launch(l).await)?;
            let even = fine("a launch at even shares", f.sandbox().launch(f.say("even")).await)?;
            return same("its end", end(&drain(even.as_ref()).await), Some(Exit::Code(0)));
        }
        let run = fine("a launch at the weights' ends", f.sandbox().launch(l).await)?;
        same("its end", end(&drain(run.as_ref()).await), Some(Exit::Code(0)))
    })
}

fn c11<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        if !f.sandbox().holds().signals {
            match run.signal(Signal::Terminate) {
                Err(Refusal::Unsupported(_)) => {}
                Err(e) => return Err(format!("a signal refused as {e:?}, not Unsupported")),
                Ok(()) => return Err("a signal taken, though the sandbox says it doesn't hold it".into()),
            }
            // Unsignalled, it goes on: only its kill ends it.
            run.kill();
            return same("its end, unsignalled", end(&drain(run.as_ref()).await), Some(Exit::Killed));
        }
        fine("a signal", run.signal(Signal::Terminate))?;
        let outs = drain(run.as_ref()).await;
        ensure(matches!(end(&outs), Some(Exit::Code(_) | Exit::Killed)), || format!("no end after its signal: {outs:?}"))?;
        same("ends said", outs.iter().filter(|o| matches!(o, Output::Exited(_))).count(), 1)?;
        match run.signal(Signal::Terminate) {
            Err(Refusal::Ended) => Ok(()),
            other => Err(format!("a signal to an ended run: {other:?}, not Ended")),
        }
    })
}

fn c12<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let launch = f.say("where");
        let host = launch.boundary == Boundary::Host;
        let run = fine("launch", f.sandbox().launch(launch).await)?;
        let (id, link) = (run.id(), run.link().await);
        if host {
            same("a process of the machine's id", id.clone(), None)?;
            same("a process of the machine's link", link.clone(), None)?;
        }
        drain(run.as_ref()).await;
        same("its id, asked again after its end", run.id(), id)?;
        same("its link, asked again after its end", run.link().await, link)
    })
}

fn c13<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        const CLAUDE: &str = "conformance/anthropic";
        let egress = || Egress::Only(vec!["api.anthropic.com".to_owned(), "example.com:443".to_owned()]);
        const STANDS: &str = "kiln-placeholder-conformance";
        let granted = |toward: &[&str]| vec![CredentialGrant { reference: CLAUDE.to_owned(), toward: toward.iter().map(|h| (*h).to_owned()).collect(), placeholder: STANDS.to_owned() }];
        // A grant that could never be filled in is refused whatever the sandbox holds.
        let never = [
            (granted(&["other.example"]), "a host its egress doesn't reach"),
            (granted(&[]), "no host"),
            (granted(&["api.anthropic.com:443"]), "a host with a port"),
            (granted(&["192.0.2.7"]), "an address"),
            (vec![CredentialGrant { reference: "not a name".to_owned(), toward: vec!["api.anthropic.com".to_owned()], placeholder: STANDS.to_owned() }], "a reference no store names"),
            (vec![CredentialGrant { reference: CLAUDE.to_owned(), toward: vec!["api.anthropic.com".to_owned()], placeholder: "not a token".to_owned() }], "a placeholder no request carries"),
        ];
        for (credentials, what) in never {
            match f.sandbox().launch(f.fill("api.anthropic.com", CLAUDE, credentials, egress())).await {
                Err(Refusal::Invalid { .. }) => {}
                Err(e) => return Err(format!("a credential toward {what}: refused as {e:?}, not Invalid")),
                Ok(_) => return Err(format!("a credential toward {what} was taken")),
            }
        }
        if !f.sandbox().holds().credentials {
            return unsupported("a credential filled in at its egress", f.sandbox().launch(f.fill("api.anthropic.com", CLAUDE, granted(&["api.anthropic.com"]), egress())).await);
        }
        let filled = fine("launch", f.sandbox().launch(f.fill("api.anthropic.com", CLAUDE, granted(&["api.anthropic.com"]), egress())).await)?;
        same("its granted host, asked with its placeholder", end(&drain(filled.as_ref()).await), Some(Exit::Code(0)))?;
        let other = fine("launch", f.sandbox().launch(f.fill("example.com", CLAUDE, granted(&["api.anthropic.com"]), egress())).await)?;
        ensure(!matches!(end(&drain(other.as_ref()).await), Some(Exit::Code(0))), || "a credential was filled in toward a host its grant doesn't name".into())?;
        let ungranted = fine("launch", f.sandbox().launch(f.fill("api.anthropic.com", "conformance/openai", granted(&["api.anthropic.com"]), egress())).await)?;
        ensure(!matches!(end(&drain(ungranted.as_ref()).await), Some(Exit::Code(0))), || "a credential the run isn't granted was filled in".into())?;
        let none = fine("launch", f.sandbox().launch(f.fill("api.anthropic.com", CLAUDE, Vec::new(), egress())).await)?;
        ensure(!matches!(end(&drain(none.as_ref()).await), Some(Exit::Code(0))), || "a credential was filled in for a run granted none".into())
    })
}

fn c14<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        let change = Change::Grant { member: Member("harness".into()), host: "example.com".into(), port: 443 };
        if !f.sandbox().holds().live {
            let taken = run.change(change).await;
            let on = run.stdin(b"still here\n".to_vec()).await;
            run.kill();
            return match taken {
                Err(Refusal::Unsupported(_)) => fine("its run, after the change it refused", on),
                Err(e) => Err(format!("a change to a run while it runs: refused as {e:?}, not Unsupported")),
                Ok(()) => Err("a change to a run while it runs: taken, though the sandbox says it doesn't hold it".into()),
            };
        }
        fine("a change to a run while it runs", run.change(change.clone()).await)?;
        run.kill();
        same("its end", end(&drain(run.as_ref()).await), Some(Exit::Killed))?;
        same("a change after its end", run.change(change).await, Err(Refusal::Ended))
    })
}

fn c15<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        let id = run.id();
        if !f.sandbox().holds().swaps {
            let swapped = run.swap(f.echo()).await;
            let on = run.stdin(b"still here\n".to_vec()).await;
            run.kill();
            return match swapped {
                Err(Refusal::Unsupported(_)) => fine("its run, after the swap it refused", on),
                Err(e) => Err(format!("a member swapped in its boundary: refused as {e:?}, not Unsupported")),
                Ok(_) => Err("a member swapped in its boundary: taken, though the sandbox says it doesn't hold it".into()),
            };
        }
        let new = fine("a member swapped in its boundary", run.swap(f.echo()).await)?;
        let outs = drain(run.as_ref()).await;
        same("the member swapped out: its ends", outs.iter().filter(|o| matches!(o, Output::Exited(_))).count(), 1)?;
        same("the new member's boundary", new.id(), id)?;
        fine("stdin to the new member", new.stdin(b"swapped\n".to_vec()).await)?;
        new.close_stdin();
        let outs = drain(new.as_ref()).await;
        same("what the new member said", String::from_utf8_lossy(&stdout(&outs)).into_owned(), "swapped\n".to_owned())?;
        same("its end", end(&outs), Some(Exit::Code(0)))?;
        ensure(run.swap(f.echo()).await.is_err(), || "a run swapped out was swapped again".into())
    })
}

fn c16<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let component = Component { kind: ComponentKind::Agent, source: "https://example.com/helper.tar.gz".into(), checksum: Some(format!("sha256:{}", "ab".repeat(32))), version_id: None };
        let laid = vec![ComponentAt { at: "/agents/helper".into(), component: component.clone() }];
        let swap = Change::Swap { at: "/agents/helper".into(), component };
        if !f.sandbox().holds().components {
            match f.sandbox().launch(f.components(laid)).await {
                Err(Refusal::Unsupported(_)) => {}
                Err(e) => return Err(format!("components laid at its launch: refused as {e:?}, not Unsupported")),
                Ok(_) => return Err("components laid at its launch: taken, though the sandbox says it doesn't hold it".into()),
            }
            let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
            let taken = run.change(swap).await;
            let on = run.stdin(b"still here\n".to_vec()).await;
            run.kill();
            return match taken {
                Err(Refusal::Unsupported(_)) => fine("its run, after the swap it refused", on),
                Err(e) => Err(format!("a component swapped into a run while it runs: refused as {e:?}, not Unsupported")),
                Ok(()) => Err("a component swapped into a run while it runs: taken, though the sandbox says it doesn't hold it".into()),
            };
        }
        let laying = fine("launch", f.sandbox().launch(f.components(laid)).await)?;
        let outs = drain(laying.as_ref()).await;
        same("the components laid for it", String::from_utf8_lossy(&stdout(&outs)).into_owned(), "/agents/helper https://example.com/helper.tar.gz\n".to_owned())?;
        let run = fine("launch", f.sandbox().launch(f.wait()).await)?;
        fine("a component swapped into a run while it runs", run.change(swap.clone()).await)?;
        fine("its image's own given back", run.change(Change::Restore { at: "/agents/helper".into() }).await)?;
        run.kill();
        same("its end", end(&drain(run.as_ref()).await), Some(Exit::Killed))?;
        same("a swap after its end", run.change(swap).await, Err(Refusal::Ended))
    })
}

/// The `Sandbox` contract, clause by clause (docs/seams.md, "Sandbox").
pub fn cases<F: SandboxFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SANDBOX-1", says: "what a run writes reaches its stdout in order, then its end, then nothing", check: c1::<F> },
        Case { id: "SANDBOX-2", says: "stdin reaches the program in order, and closing it is the end of its input", check: c2::<F> },
        Case { id: "SANDBOX-3", says: "a run's stderr and exit code are said as they are, and its end comes once, last", check: c3::<F> },
        Case { id: "SANDBOX-4", says: "a run killed ends Killed, and takes no more input", check: c4::<F> },
        Case {
            id: "SANDBOX-5",
            says: "a run over its memory limit is ended OverLimit(Memory); under it, it runs to its end; a sandbox that holds no memory limit refuses one Unsupported, and runs a launch without one",
            check: c5::<F>,
        },
        Case {
            id: "SANDBOX-6",
            says: "a run refused processes past its process limit, that then fails, ends OverLimit(Processes); under it, it runs to its end; a sandbox that holds no process limit refuses one Unsupported, and runs a launch without one",
            check: c6::<F>,
        },
        Case {
            id: "SANDBOX-7",
            says: "a run reaches only the hosts it's granted, none when granted none, and a host when granted any; a sandbox refuses Unsupported the grants it doesn't hold",
            check: c7::<F>,
        },
        Case { id: "SANDBOX-8", says: "a launch of a program that doesn't exist is refused NoSuchProgram", check: c8::<F> },
        Case { id: "SANDBOX-9", says: "a run works in the folder it's given", check: c9::<F> },
        Case {
            id: "SANDBOX-10",
            says: "cpu and io weights outside cgroup v2's 1 to 10,000 are refused Invalid; at its ends a run goes ahead where the sandbox holds weights, and is refused Unsupported where it doesn't",
            check: c10::<F>,
        },
        Case {
            id: "SANDBOX-11",
            says: "a run sent a signal to end ends, its end said once, and a signal after its end is refused Ended; a sandbox that delivers no signals refuses one Unsupported, and its run goes on",
            check: c11::<F>,
        },
        Case {
            id: "SANDBOX-12",
            says: "a run on the host has no id of a boundary's and no link, and a run says the same id and link after its end as before",
            check: c12::<F>,
        },
        Case {
            id: "SANDBOX-13",
            says: "a run's credential, granted by reference toward a host its egress reaches, is filled in toward that host alone: never toward another host it reaches, for a credential it isn't granted, or for a run granted none; a grant toward a host its egress doesn't reach, an address, a host with a port or no host, or by a reference no store names, is refused Invalid; a sandbox that fills in no credentials refuses them Unsupported",
            check: c13::<F>,
        },
        Case {
            id: "SANDBOX-14",
            says: "a run takes a change to its network or folders while it runs, and one after its end is refused Ended; a sandbox that takes no change live refuses one Unsupported, and its run goes on",
            check: c14::<F>,
        },
        Case {
            id: "SANDBOX-15",
            says: "a run's member swapped for another in its boundary ends, its end said once, and the run it gives runs the other in the same boundary; one swapped out swaps no more; a sandbox that swaps none refuses one Unsupported, and its run goes on",
            check: c15::<F>,
        },
        Case {
            id: "SANDBOX-16",
            says: "a launch's components are laid for it, and a run takes a component swapped in and its image's own given back while it runs, and one after its end is refused Ended; a sandbox that holds no components refuses both Unsupported, and its run goes on",
            check: c16::<F>,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2: the simulator and its suite.
    #[test]
    fn the_simulated_sandbox_passes_the_sandbox_suite() -> Result<(), String> {
        futures::executor::block_on(run("Sandbox", &cases::<SimSandboxFixture>(), || async { SimSandboxFixture::default() })).into_result().map(|_| ())
    }

    /// A simulator for the suite's other half: launches without limits or grants (a host's), the
    /// sandbox under test said to hold `holds`, and `lies` a sandbox that takes everything anyway.
    struct Holding {
        sim: SimSandboxFixture,
        lies: Option<Liar>,
    }

    /// Says it holds nothing, and takes everything.
    struct Liar(SimSandbox);

    impl Sandbox for Liar {
        fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
            self.0.launch(launch)
        }
        fn boundaries(&self) -> Vec<&'static str> {
            self.0.boundaries()
        }
        fn holds(&self) -> Holds {
            Holds::default()
        }
    }

    fn free(l: Launch) -> Launch {
        Launch { limits: Limits::NONE, egress: Egress::Any, ..l }
    }

    impl SandboxFixture for Holding {
        fn sandbox(&self) -> &dyn Sandbox {
            match &self.lies {
                Some(l) => l,
                None => self.sim.sandbox(),
            }
        }
        fn say(&self, text: &str) -> Launch {
            free(self.sim.say(text))
        }
        fn echo(&self) -> Launch {
            free(self.sim.echo())
        }
        fn fail(&self, code: i32) -> Launch {
            free(self.sim.fail(code))
        }
        fn wait(&self) -> Launch {
            free(self.sim.wait())
        }
        fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch {
            Launch { limits: Limits { memory_bytes: limit, ..Limits::NONE }, egress: Egress::Any, ..self.sim.hog(bytes, limit) }
        }
        fn spawn(&self, n: u32, limit: Option<u32>) -> Launch {
            Launch { limits: Limits { processes: limit, ..Limits::NONE }, egress: Egress::Any, ..self.sim.spawn(n, limit) }
        }
        fn connect(&self, host: &str, egress: Egress) -> Launch {
            Launch { limits: Limits::NONE, ..self.sim.connect(host, egress) }
        }
        fn fill(&self, host: &str, reference: &str, credentials: Vec<CredentialGrant>, egress: Egress) -> Launch {
            Launch { limits: Limits::NONE, ..self.sim.fill(host, reference, credentials, egress) }
        }
        fn pwd(&self, folder: &str) -> Launch {
            free(self.sim.pwd(folder))
        }
        fn components(&self, laid: Vec<ComponentAt>) -> Launch {
            free(self.sim.components(laid))
        }
        fn folder(&self) -> String {
            self.sim.folder()
        }
    }

    /// CP-T4.1: a sandbox that holds nothing (the host's) passes the suite by refusing, typed, what
    /// it doesn't hold, and running what asks for none of it.
    #[test]
    fn a_sandbox_that_holds_nothing_passes_the_suite_refusing_what_it_does_not_hold() -> Result<(), String> {
        let make = || async { Holding { sim: SimSandboxFixture(SimSandbox::holding(Holds::default())), lies: None } };
        futures::executor::block_on(run("Sandbox", &cases::<Holding>(), make)).into_result().map(|_| ())
    }

    /// CP-T4.1: the suite isn't vacuous there: a sandbox that says it holds nothing but takes limits
    /// and grants anyway fails each clause about them, and only those.
    #[test]
    fn a_sandbox_that_takes_what_it_says_it_does_not_hold_fails_those_clauses() {
        let make = || async { Holding { sim: SimSandboxFixture::default(), lies: Some(Liar(SimSandbox::new())) } };
        let report = futures::executor::block_on(run("Sandbox", &cases::<Holding>(), make));
        let failed: Vec<&str> = report.failed.iter().map(|(id, _)| *id).collect();
        assert_eq!(failed, ["SANDBOX-5", "SANDBOX-6", "SANDBOX-7", "SANDBOX-10", "SANDBOX-11", "SANDBOX-13", "SANDBOX-14", "SANDBOX-15", "SANDBOX-16"], "{:?}", report.failed);
        assert!(report.failed.iter().all(|(_, why)| why.contains("doesn't hold it")), "{:?}", report.failed);
    }

    /// CP-T4.1: what each kind of launch is refused by a sandbox that holds only part of it.
    #[test]
    fn holds_refuse_what_they_do_not_hold_by_name() {
        let only_memory = Holds { memory: true, ..Holds::default() };
        let l = |limits: Limits, egress: Egress| Launch { limits, egress, ..Launch::of("say", &["x"]) };
        assert_eq!(only_memory.check(&l(Limits { processes: None, ..Limits::default() }, Egress::Any)), Ok(()));
        assert_eq!(only_memory.check(&l(Limits::default(), Egress::Any)), Err(Refusal::Unsupported("a process limit".into())));
        assert_eq!(only_memory.check(&l(Limits::NONE, Egress::default())), Err(Refusal::Unsupported("a run with no egress".into())));
        assert_eq!(only_memory.check(&l(Limits::NONE, Egress::Only(vec!["h".into()]))), Err(Refusal::Unsupported("egress to named hosts alone".into())));
        let weighted = Limits { cpu_weight: WEIGHT_MIN, ..Limits::NONE };
        assert!(matches!(only_memory.check(&l(weighted, Egress::Any)), Err(Refusal::Unsupported(_))));
        assert!(matches!(only_memory.check(&l(Limits { cpu_weight: 0, ..Limits::NONE }, Egress::Any)), Err(Refusal::Invalid { .. })), "a weight off the scale is invalid before it's unheld");
        assert_eq!(Holds::ALL.check(&l(Limits { cpu_weight: WEIGHT_MAX, ..Limits::default() }, Egress::Only(vec!["h".into()]))), Ok(()));
        // A folder at its own path, writable, is what a machine's own process sees anyway; at
        // another path, or read-only, only where shares are held.
        let shared = |s: Share| Launch { shares: vec![s], ..l(Limits::NONE, Egress::Any) };
        assert_eq!(only_memory.check(&shared(Share::at_its_path("/p"))), Ok(()));
        let moved = Share { at: "/workspace".into(), ..Share::at_its_path("/p") };
        assert_eq!(only_memory.check(&shared(moved.clone())), Err(Refusal::Unsupported("a folder shared at another path or read-only".into())));
        let read_only = Share { writable: false, ..Share::at_its_path("/p") };
        assert_eq!(only_memory.check(&shared(read_only)), Err(Refusal::Unsupported("a folder shared at another path or read-only".into())));
        assert_eq!(Holds::ALL.check(&shared(moved)), Ok(()));
    }

    /// Kiln plan 01a1d2b88be57304b972761e2a5c6781 step 6 (D242): a credential grant is held to its
    /// launch's egress (it grants none) and to the host forms TLS names a server by, refused
    /// `Invalid` before anything it asks is found unheld; one that could be filled in is taken where
    /// credentials are held and refused `Unsupported` where they aren't.
    #[test]
    fn cp_t8_3_a_credential_grant_is_held_to_its_egress_and_refused_where_unheld() {
        let only_memory = Holds { memory: true, ..Holds::default() };
        let granted = |toward: &[&str], egress: Egress| Launch {
            limits: Limits::NONE,
            egress,
            credentials: vec![CredentialGrant { reference: "anthropic/ada".into(), toward: toward.iter().map(|h| (*h).to_owned()).collect(), placeholder: "kiln-placeholder-ada".into() }],
            ..Launch::of("fill", &[])
        };
        let named = || Egress::Only(vec!["API.anthropic.com:443".into(), "example.com".into()]);
        assert_eq!(Holds::ALL.check(&granted(&["api.anthropic.com"], named())), Ok(()), "a host its egress names with a port, in another case");
        assert_eq!(Holds::ALL.check(&granted(&["example.com"], named())), Ok(()));
        assert_eq!(Holds::ALL.check(&granted(&["api.anthropic.com"], Egress::Any)), Ok(()));
        for (toward, egress) in [(&["other.example"][..], named()), (&[][..], named()), (&["api.anthropic.com:443"][..], named()), (&["192.0.2.7"][..], Egress::Any), (&["2001:db8::7"][..], Egress::Any), (&[""][..], Egress::Any)] {
            let got = only_memory.check(&granted(toward, egress));
            assert!(matches!(&got, Err(Refusal::Invalid { what: "a credential grant", why }) if why.starts_with("credentials[0]")), "{toward:?}: {got:?}");
        }
        let unnamed = Launch { credentials: vec![CredentialGrant { reference: "no spaces".into(), toward: vec!["example.com".into()], placeholder: "kiln-placeholder-ada".into() }], ..granted(&[], named()) };
        assert!(matches!(Holds::ALL.check(&unnamed), Err(Refusal::Invalid { .. })), "a reference no store could name");
        for placeholder in ["", "===", "has space", "quote\"d", "kiln-placeholder-ada=x"] {
            let unsent = Launch { credentials: vec![CredentialGrant { reference: "anthropic/ada".into(), toward: vec!["example.com".into()], placeholder: placeholder.into() }], ..granted(&[], named()) };
            assert!(matches!(Holds::ALL.check(&unsent), Err(Refusal::Invalid { what: "a credential grant", why }) if why.contains("placeholder")), "{placeholder:?}");
        }
        let padded = Launch { credentials: vec![CredentialGrant { reference: "anthropic/ada".into(), toward: vec!["example.com".into()], placeholder: "a+b/c~d.e_f-g==".into() }], ..granted(&[], named()) };
        assert_eq!(Holds::ALL.check(&padded), Ok(()), "every b64token character, and its padding");
        assert_eq!(only_memory.check(&granted(&["api.anthropic.com"], Egress::Any)), Err(Refusal::Unsupported("credentials filled in at its egress".into())));
        assert_eq!(only_memory.check(&Launch { credentials: Vec::new(), ..granted(&[], Egress::Any) }), Ok(()), "a run granted none asks nothing held");
    }

    /// Step 17: a signal ends a simulated run with the code a shell says of it, said once; once
    /// it has ended, a signal is refused `Ended`.
    #[test]
    fn a_signal_ends_a_run_with_its_shells_code() -> Result<(), Refusal> {
        let s = SimSandbox::new();
        for signal in [Signal::Interrupt, Signal::Terminate] {
            let run = futures::executor::block_on(s.launch(Launch::of("wait", &[])))?;
            run.signal(signal)?;
            let outs = futures::executor::block_on(drain(run.as_ref()));
            assert_eq!(end(&outs), Some(Exit::Code(128 + signal.number())), "{signal:?}");
            assert_eq!(run.signal(signal), Err(Refusal::Ended));
        }
        Ok(())
    }

    /// CP-M2, AC-M2.2: the simulator, bounded, refusing typed past its bounds.
    #[test]
    fn a_microvm_is_refused_as_unsupported_and_runs_are_bounded() {
        let s = SimSandbox::new();
        let vm = Launch { boundary: Boundary::MicroVm { image: "x".into() }, ..Launch::of("say", &["x"]) };
        assert!(matches!(futures::executor::block_on(s.launch(vm)), Err(Refusal::Unsupported(_))));
        let container = Launch { boundary: Boundary::Container { image: "x".into() }, ..Launch::of("say", &["x"]) };
        assert!(matches!(futures::executor::block_on(s.launch(container)), Err(Refusal::Unsupported(_))));
        let runs: Vec<_> = (0..SIM_RUNS).filter_map(|_| futures::executor::block_on(s.launch(Launch::of("wait", &[]))).ok()).collect();
        assert_eq!(runs.len(), SIM_RUNS);
        assert!(matches!(futures::executor::block_on(s.launch(Launch::of("wait", &[]))), Err(Refusal::Full { .. })));
        drop(runs);
        assert!(futures::executor::block_on(s.launch(Launch::of("wait", &[]))).is_ok(), "an ended run frees its place");
    }
}
