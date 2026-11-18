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
//! names alone, and never travel in a launch. A sandbox that holds them keeps warm pools for a
//! microVM's launches as kiln names them ([`Pool`]; kiln plan 01a1dd2e…, P3.2), each sized by its
//! boundary's own demand rule. One that holds them gives a launch volumes of its boundary's own store,
//! held to their bounds and kept past its run, and lists and clears them ([`Volume`],
//! [`Sandbox::volumes`]; kiln plan 01a1dd2e…, P6.2, Shards' D281).

use std::collections::VecDeque;
use std::num::NonZeroU64;
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
    /// Warm pools kept for its launches ([`Sandbox::keep`]: kiln plan 01a1dd2e…, P3.2).
    pub pools: bool,
    /// Volumes of its boundary's own store given to a launch's members as it starts, held to their bounds and kept past
    /// it, listed and cleared ([`Launch::volumes`], [`Sandbox::volumes`], [`Sandbox::clear_volume`]: kiln plan
    /// 01a1dd2e…, P6.2).
    pub volumes: bool,
}

impl Holds {
    /// Everything (the simulator's).
    pub const ALL: Holds = Holds { memory: true, processes: true, weights: true, no_egress: true, egress_by_host: true, shares: true, signals: true, credentials: true, live: true, swaps: true, components: true, pools: true, volumes: true };

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
        for (at, volume) in launch.volumes.iter().enumerate() {
            volume.check(at)?;
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
        if !launch.volumes.is_empty() && !self.volumes {
            return refuse("volumes of its boundary's store given as it starts");
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

/// A warm pool a sandbox keeps for the launches of one microVM (kiln plan 01a1dd2e…, P3.2): its image, pinned by its
/// digest, and its size, vCPUs and MiB (none: the least its boundary boots), as Hex keys a pool of Shards' (P3.1;
/// Shards' D253). How many hosts it keeps warm is its boundary's own demand rule's, within its machine's room (Shards'
/// D26): kiln names a pool, never its size.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pool {
    pub image: String,
    pub size: Option<(u32, u64)>,
}

/// What a pool a sandbox keeps holds now ([`Sandbox::pools`]): its hosts warm, ready for a launch, and those starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolHeld {
    pub pool: Pool,
    pub warm: usize,
    pub starting: usize,
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

/// A volume of its boundary's own store a launch's `members` are given as it starts (kiln plan 01a1dd2e…, P6.2;
/// Shards' D271 and D281): by its `name` there, made empty where the store has none, at `at` in its members, read-only
/// where it says, its files held to `bound` bytes, and kept past the run for the next run given it. A run holds it from
/// its start to its end: no clearing removes it meanwhile ([`VolumeCause::InUse`]), and a launch asking it with another
/// bound is refused ([`VolumeCause::Bound`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub bound: NonZeroU64,
    pub at: String,
    pub members: Vec<Member>,
    pub read_only: bool,
}

impl Volume {
    /// `Ok` where a store could be asked for it: a name, an absolute place, and a member to give it to; else `Invalid`,
    /// naming it by its place among its launch's volumes. The rest of its grammar is its boundary's to refuse.
    fn check(&self, at: usize) -> Result<(), Refusal> {
        let invalid = |why: String| Err(Refusal::Invalid { what: "a volume", why: format!("volumes[{at}] {why}") });
        if self.name.is_empty() {
            return invalid("has no name".into());
        }
        if !self.at.starts_with('/') {
            return invalid(format!("is at {:?}, which isn't absolute", self.at));
        }
        if self.members.is_empty() {
            return invalid("is given to no member".into());
        }
        Ok(())
    }
}

/// A volume its boundary's store keeps ([`Sandbox::volumes`]), as the store reads it as it's asked: its name; the bytes
/// its files take; its bound, none where it has none; its charge, what its files take by its bound's own measure, which
/// its bound holds it to (Shards' D281); and when it was made, in nanoseconds since the Unix epoch. A measure whose
/// files couldn't all be read is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptVolume {
    pub name: String,
    pub bytes: Option<u64>,
    pub bound: Option<u64>,
    pub charge: Option<u64>,
    pub created_ns: u64,
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
    /// The volumes of its boundary's store its members are given as it starts (kiln plan 01a1dd2e…, P6.2): each held
    /// by its run until it ends, and kept past it. None by default.
    pub volumes: Vec<Volume>,
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
            volumes: Vec::new(),
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
    /// A volume of its boundary's store held from a launch or a clearing by `cause`, in its boundary's words, `why`
    /// (kiln plan 01a1dd2e…, P6.2; Shards' D281).
    #[error("a volume, {cause}: {why}")]
    Volume { cause: VolumeCause, why: String },
    /// Its run failed as it started, in its boundary's words: its command never started, or its boundary failed it
    /// (Hex's `Failed`, Shards' D249). The launch was valid: the run is what failed.
    #[error("its run failed: {0}")]
    Failed(String),
}

/// What held a volume of a boundary's store from a launch or a clearing ([`Refusal::Volume`]; Shards' D281), each by
/// its own retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VolumeCause {
    /// Asked with another bound than the runs holding it serve it with: its bound changes only once no run holds it,
    /// so a launch asking it again then is given the bound it asks.
    #[error("asked with another bound than the runs holding it serve it with")]
    Bound,
    /// A clearing holds it alone: asked again once that clearing has answered, it's made anew, empty.
    #[error("being cleared")]
    Clearing,
    /// A run holds it until it ends: its files are kept.
    #[error("held by a run")]
    InUse,
    /// Its store has none of that name.
    #[error("not in its store")]
    Unknown,
}

/// The agent-control link a run is driven over into its boundary (a microVM's: Shards' agent control,
/// kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 11), as each end says it negotiated
/// ([`kiln_ci::facts::Negotiated`]): the controller's, on the machine, and the endpoint's own report
/// from inside; and the lanes its controller was granted on it, by Shards' names (`control`,
/// `stdout:0`, `stderr:0`, `stdin:0`, `exec`, `log:run`, `socket`, `mcp:0`). Its stdio ride it where
/// their lanes are among them; elsewhere they come another way (a Hex-held run's, its run host's
/// member streams).
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
    /// `member` on the named `network`, shared with the runs connected to it, on `grant`'s terms
    /// (Shards' D285): joining grants nothing else, and a flow between two runs' members is where
    /// the grants of both its ends take it.
    Connect { member: Member, network: String, grant: NetworkGrant },
    /// That grant taken back: a flow it let open is reset, and its member leaves the network with
    /// its last grant there.
    Disconnect { member: Member, network: String, grant: NetworkGrant },
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

/// What a member's connection to a network shared with other runs grants it, every part named
/// (Shards' D285; default deny): its name there, as its peers name it (one member's on the
/// network); its peer, a member of another run on it, by its name there, which outlasts that run's
/// restarts as its [`Run::id`] doesn't; the ports; and which way a flow opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkGrant {
    pub name: String,
    pub peer: String,
    pub ports: Ports,
    pub way: Way,
}

/// A grant's ports on a shared network, UDP's else TCP's: `first` to `last`, a range as Shards'
/// D285 takes one (one port: the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub udp: bool,
    pub first: u16,
    pub last: u16,
}

/// Which way a [`NetworkGrant`] lets a flow open: from its member toward its peer (`Out`), or from
/// its peer toward its member (`In`). A flow's replies come back by its state, whichever way it
/// opened (Shards' D285).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    In,
    Out,
}

impl NetworkGrant {
    /// `Ok` where it names its name and its peer, each a host's name and not the other, and ports
    /// from 1 to 65535, the first at most the last; else `Invalid`, saying what.
    pub fn check(&self) -> Result<(), Refusal> {
        let invalid = |why: String| Err(Refusal::Invalid { what: "a grant on a network", why });
        for (what, name) in [("its name", &self.name), ("its peer", &self.peer)] {
            if !host_name(name) {
                return invalid(format!("{what}, {name:?}, isn't a host's name: labels of letters, digits and hyphens, 63 bytes each and 253 in all (RFC 1035 §2.3.4, RFC 1123 §2.1)"));
            }
        }
        // RFC 4343: DNS names compare case-insensitively.
        if self.name.eq_ignore_ascii_case(&self.peer) {
            return invalid(format!("its peer is {}, itself: a peer is a member of another run", self.peer));
        }
        if self.ports.first == 0 || self.ports.first > self.ports.last {
            return invalid(format!("ports {} to {}: from 1 to 65535, the first at most the last (0 names no port)", self.ports.first, self.ports.last));
        }
        Ok(())
    }
}

/// Whether `name` is a host's name: labels of 1 to 63 letters, digits and hyphens, none starting or
/// ending with one, 253 bytes in all (RFC 1035 §2.3.4; RFC 1123 §2.1 lets a label start with a digit).
fn host_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 253 && name.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-'))
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
    /// The agent-control link it is driven over into its boundary ([`Link`]), as each end says it
    /// negotiated, once it's made: it waits for the link's handshake and its endpoint's report. None
    /// at once for a run driven over no link (a process of the machine's, whose stdio are its pipes),
    /// and none where its link ends before it's made. Asked again, it says the same.
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
    /// Pauses it where it is (`on`), or resumes it (kiln plan 01a1dd2e…, P5.5c; Shards' D267 through Hex's P5.5b):
    /// paused, its boundary runs none of it (a microVM's guest runs no CPU) and keeps its stdio and connections until
    /// it's resumed. Refused `Ended` once it has ended, `Unsupported` where its boundary pauses none ([`NO_PAUSE`]), and
    /// in its runtime's words where that refuses it: refused, it goes on as it was.
    fn pause(&self, on: bool) -> BoxFuture<'_, Result<(), Refusal>> {
        let _ = on;
        Box::pin(async { Err(Refusal::Unsupported(NO_PAUSE.into())) })
    }
}

/// What a run whose boundary pauses none refuses ([`Run::pause`]).
pub const NO_PAUSE: &str = "a run paused where it is and resumed";

/// Harnesses launched in boundaries.
pub trait Sandbox: Send + Sync + 'static {
    /// Starts `launch` in its boundary, within its limits; refused, typed, when it can't.
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>>;
    /// The boundaries it can hold runs in, by name (`host`, `container`, `microvm`).
    fn boundaries(&self) -> Vec<&'static str>;
    /// What it holds its runs to; a launch asking for more is refused `Unsupported`.
    fn holds(&self) -> Holds;
    /// Keeps a warm pool for `pool`'s launches from now on, as many hosts warm as its own demand rule sizes it within its
    /// machine's room, answered without waiting on a host's start; one kept already is kept. Refused `Unsupported`
    /// where it keeps none ([`Holds::pools`]).
    fn keep(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
        drop(pool);
        Box::pin(async { Err(Refusal::Unsupported(NO_POOLS.into())) })
    }
    /// Lets `pool` go: its warm hosts end and their room comes back, a host a launch took going on. One it doesn't
    /// keep is let go already. Refused `Unsupported` where it keeps none.
    fn let_go(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
        drop(pool);
        Box::pin(async { Err(Refusal::Unsupported(NO_POOLS.into())) })
    }
    /// The pools it keeps now, and what each holds; none where it keeps none.
    fn pools(&self) -> Vec<PoolHeld> {
        Vec::new()
    }
    /// The volumes its store keeps now, by name, as it reads them as it's asked: each one's files walked, a cost for a
    /// person's view or a placement, never for a launch. Refused `Unsupported` where it keeps none
    /// ([`Holds::volumes`]).
    fn volumes(&self) -> BoxFuture<'_, Result<Vec<KeptVolume>, Refusal>> {
        Box::pin(async { Err(Refusal::Unsupported(NO_VOLUMES.into())) })
    }
    /// Volume `name` of its store removed with its files, so the next launch given it finds it empty, once no run holds
    /// it. Refused at once, never waited for: `Volume { InUse }` while a run holds it, its files kept, and
    /// `Volume { Unknown }` where its store has none of that name. Refused `Unsupported` where it keeps none.
    fn clear_volume(&self, name: String) -> BoxFuture<'_, Result<(), Refusal>> {
        drop(name);
        Box::pin(async { Err(Refusal::Unsupported(NO_VOLUMES.into())) })
    }
}

/// What a sandbox that keeps no warm pools ([`Holds::pools`]) refuses.
pub const NO_POOLS: &str = "a warm pool kept for its launches";

/// What a sandbox that keeps no volumes ([`Holds::volumes`]) refuses.
pub const NO_VOLUMES: &str = "volumes of its boundary's store, given to a launch and kept past it";

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
/// [`SimSandbox::holding`] less, as a real boundary may. The pools it keeps are names alone: it
/// boots no microVM, so none holds a host, and it keeps at most one for each run it may hold
/// ([`SIM_RUNS`]), refusing one past them `Full`. The volumes it keeps are names alone too: none
/// holds a file, so each one's bytes and charge are 0, and none says when it was made (it keeps no
/// clock); at most one for each run it may hold, a launch making one past them refused `Full`.
#[derive(Clone)]
pub struct SimSandbox {
    runs: Arc<Mutex<usize>>,
    holds: Holds,
    pools: Arc<Mutex<std::collections::BTreeSet<Pool>>>,
    volumes: Arc<Mutex<std::collections::BTreeMap<String, SimVolume>>>,
}

/// A volume a [`SimSandbox`]'s store keeps: the bound the runs holding it were given, and how many hold it.
#[derive(Debug, Clone, Copy)]
struct SimVolume {
    bound: u64,
    held: usize,
}

impl Default for SimSandbox {
    fn default() -> Self {
        Self { runs: Arc::default(), holds: Holds::ALL, pools: Arc::default(), volumes: Arc::default() }
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

    /// `given`, a launch's volumes, held for its run, their names answered for it to let go of as it ends: each made
    /// where the store has none (past one for each run it may hold, refused `Full`), its bound the one asked where no
    /// run holds it; one a run holds with another bound refused `Volume { Bound }`. Refused, it holds none of them.
    fn hold_volumes(&self, given: &[Volume]) -> Result<Vec<String>, Refusal> {
        let mut kept = self.volumes.lock();
        let mut made = 0usize;
        for v in given {
            match kept.get(&v.name) {
                Some(k) if k.held > 0 && k.bound != v.bound.get() => {
                    return Err(Refusal::Volume { cause: VolumeCause::Bound, why: format!("{} is held with a bound of {} bytes, and asked with {}", v.name, k.bound, v.bound) });
                }
                Some(_) => {}
                None => made = made.saturating_add(1),
            }
        }
        if kept.len().saturating_add(made) > SIM_RUNS {
            return Err(Refusal::Full { limit: SIM_RUNS });
        }
        for v in given {
            let k = kept.entry(v.name.clone()).or_insert(SimVolume { bound: v.bound.get(), held: 0 });
            if k.held == 0 {
                k.bound = v.bound.get();
            }
            k.held = k.held.saturating_add(1);
        }
        Ok(given.iter().map(|v| v.name.clone()).collect())
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
    /// The volumes of its sandbox's store it holds, let go of as it's dropped.
    volumes: Vec<String>,
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
        {
            let mut n = self.runs.lock();
            *n = n.saturating_sub(1);
        }
        let mut kept = self.sandbox.volumes.lock();
        for name in &self.volumes {
            if let Some(k) = kept.get_mut(name) {
                k.held = k.held.saturating_sub(1);
            }
        }
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
            let granted = match &change {
                Change::Connect { grant, .. } | Change::Disconnect { grant, .. } => grant.check(),
                _ => Ok(()),
            };
            if st.ended {
                Err(Refusal::Ended)
            } else if let Err(e) = granted {
                Err(e)
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
            let volumes = self.hold_volumes(&launch.volumes)?;
            *n = n.saturating_add(1);
            let run: Box<dyn Run> = Box::new(SimRun { state: Arc::new(Mutex::new(st)), said: Arc::new(event_listener::Event::new()), runs: Arc::clone(&self.runs), signals: self.holds.signals, live: self.holds.live, components: self.holds.components, sandbox: self.clone(), volumes });
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

    fn keep(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = (|| {
            if !self.holds.pools {
                return Err(Refusal::Unsupported(NO_POOLS.into()));
            }
            let mut pools = self.pools.lock();
            if !pools.contains(&pool) && pools.len() >= SIM_RUNS {
                return Err(Refusal::Full { limit: SIM_RUNS });
            }
            pools.insert(pool);
            Ok(())
        })();
        Box::pin(async move { out })
    }

    fn let_go(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = if self.holds.pools {
            self.pools.lock().remove(&pool);
            Ok(())
        } else {
            Err(Refusal::Unsupported(NO_POOLS.into()))
        };
        Box::pin(async move { out })
    }

    fn pools(&self) -> Vec<PoolHeld> {
        self.pools.lock().iter().map(|pool| PoolHeld { pool: pool.clone(), warm: 0, starting: 0 }).collect()
    }

    fn volumes(&self) -> BoxFuture<'_, Result<Vec<KeptVolume>, Refusal>> {
        let out = if self.holds.volumes {
            Ok(self.volumes.lock().iter().map(|(name, v)| KeptVolume { name: name.clone(), bytes: Some(0), bound: Some(v.bound), charge: Some(0), created_ns: 0 }).collect())
        } else {
            Err(Refusal::Unsupported(NO_VOLUMES.into()))
        };
        Box::pin(async move { out })
    }

    fn clear_volume(&self, name: String) -> BoxFuture<'_, Result<(), Refusal>> {
        let out = if self.holds.volumes {
            let mut kept = self.volumes.lock();
            match kept.get(&name).map(|v| v.held) {
                None => Err(Refusal::Volume { cause: VolumeCause::Unknown, why: format!("no volume {name}") }),
                Some(0) => {
                    kept.remove(&name);
                    Ok(())
                }
                Some(held) => Err(Refusal::Volume { cause: VolumeCause::InUse, why: format!("{name} is held by {held} runs") }),
            }
        } else {
            Err(Refusal::Unsupported(NO_VOLUMES.into()))
        };
        Box::pin(async move { out })
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
        let grant = NetworkGrant { name: "alice".into(), peer: "bob".into(), ports: Ports { udp: false, first: 8080, last: 8081 }, way: Way::Out };
        let connect = |grant: NetworkGrant| Change::Connect { member: Member("harness".into()), network: "team".into(), grant };
        fine("a member connected to a network, its grant whole", run.change(connect(grant.clone())).await)?;
        let malformed = [
            ("no name", NetworkGrant { name: String::new(), ..grant.clone() }),
            ("no peer", NetworkGrant { peer: String::new(), ..grant.clone() }),
            ("a peer that's no host's name", NetworkGrant { peer: "-bob".into(), ..grant.clone() }),
            ("itself as its peer", NetworkGrant { peer: "ALICE".into(), ..grant.clone() }),
            ("port 0", NetworkGrant { ports: Ports { first: 0, ..grant.ports }, ..grant.clone() }),
            ("its ports out of order", NetworkGrant { ports: Ports { first: 8081, last: 8080, ..grant.ports }, ..grant.clone() }),
        ];
        for (what, bad) in malformed {
            let taken = run.change(connect(bad)).await;
            if !matches!(taken, Err(Refusal::Invalid { .. })) {
                run.kill();
                return Err(format!("a connection naming {what}: {taken:?}, not refused Invalid"));
            }
        }
        fine("its grant taken back", run.change(Change::Disconnect { member: Member("harness".into()), network: "team".into(), grant }).await)?;
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

fn c17<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let pool = Pool { image: "conformance@sha256:00".into(), size: Some((1, 512)) };
        let other = Pool { size: None, ..pool.clone() };
        if !f.sandbox().holds().pools {
            return match f.sandbox().keep(pool).await {
                Err(Refusal::Unsupported(_)) => same("the pools it keeps", f.sandbox().pools(), Vec::new()),
                Err(e) => Err(format!("a warm pool kept: refused as {e:?}, not Unsupported")),
                Ok(()) => Err("a warm pool kept: taken, though the sandbox says it doesn't hold it".into()),
            };
        }
        let kept = |held: Vec<PoolHeld>| {
            let mut pools: Vec<Pool> = held.into_iter().map(|h| h.pool).collect();
            pools.sort();
            pools
        };
        fine("a pool kept", f.sandbox().keep(pool.clone()).await)?;
        fine("the same pool kept again", f.sandbox().keep(pool.clone()).await)?;
        fine("another pool kept", f.sandbox().keep(other.clone()).await)?;
        let mut both = vec![pool.clone(), other.clone()];
        both.sort();
        same("the pools it keeps", kept(f.sandbox().pools()), both)?;
        fine("a pool let go", f.sandbox().let_go(pool.clone()).await)?;
        fine("a pool it doesn't keep let go", f.sandbox().let_go(pool).await)?;
        same("the pools it keeps once one is let go", kept(f.sandbox().pools()), vec![other])
    })
}

fn c18<F: SandboxFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        const NAME: &str = "conformance-volume";
        let given = |launch: Launch, bound: NonZeroU64| {
            let members = vec![Member(launch.program.clone())];
            Launch { volumes: vec![Volume { name: NAME.into(), bound, at: "/volumes/conformance".into(), members, read_only: false }], ..launch }
        };
        let (one, two) = (NonZeroU64::MIN, NonZeroU64::MIN.saturating_add(1));
        if !f.sandbox().holds().volumes {
            return match f.sandbox().launch(given(f.wait(), one)).await {
                Err(Refusal::Unsupported(_)) => match (f.sandbox().volumes().await, f.sandbox().clear_volume(NAME.into()).await) {
                    (Err(Refusal::Unsupported(_)), Err(Refusal::Unsupported(_))) => Ok(()),
                    other => Err(format!("a sandbox that keeps no volumes listed or cleared one: {other:?}")),
                },
                Err(e) => Err(format!("a launch given a volume: refused as {e:?}, not Unsupported")),
                Ok(run) => {
                    run.kill();
                    Err("a launch given a volume: taken, though the sandbox says it doesn't hold it".into())
                }
            };
        }
        let bounds = |v: Result<Vec<KeptVolume>, Refusal>| v.map(|kept| kept.into_iter().filter(|k| k.name == NAME).map(|k| k.bound).collect::<Vec<_>>());
        let refused_by = |v: Result<(), Refusal>| match v {
            Err(Refusal::Volume { cause, .. }) => Some(cause),
            Ok(()) | Err(_) => None,
        };
        let run = fine("a launch given a volume", f.sandbox().launch(given(f.wait(), one)).await)?;
        same("its volume, listed with its bound", bounds(f.sandbox().volumes().await), Ok(vec![Some(one.get())]))?;
        match f.sandbox().launch(given(f.wait(), two)).await {
            Err(Refusal::Volume { cause: VolumeCause::Bound, .. }) => {}
            Err(e) => return Err(format!("a launch asking a held volume with another bound: refused as {e:?}, not Volume {{ Bound }}")),
            Ok(other) => {
                other.kill();
                return Err("a launch asking a held volume with another bound: taken".into());
            }
        }
        same("a volume a run holds, cleared", refused_by(f.sandbox().clear_volume(NAME.into()).await), Some(VolumeCause::InUse))?;
        run.kill();
        drain(run.as_ref()).await;
        drop(run);
        same("its volume once its run has ended", bounds(f.sandbox().volumes().await), Ok(vec![Some(one.get())]))?;
        fine("a volume no run holds, cleared", f.sandbox().clear_volume(NAME.into()).await)?;
        same("the volumes once it's cleared", bounds(f.sandbox().volumes().await), Ok(Vec::new()))?;
        same("a volume cleared again", refused_by(f.sandbox().clear_volume(NAME.into()).await), Some(VolumeCause::Unknown))
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
            says: "a run takes a change to its network or folders while it runs, a member connected to a network and its grant there taken back among them; a connection whose grant names no host's name or peer, itself as its peer, port 0 or its ports out of order is refused Invalid; a change after its end is refused Ended; a sandbox that takes no change live refuses one Unsupported, and its run goes on",
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
        Case {
            id: "SANDBOX-17",
            says: "a sandbox keeps a warm pool it's asked to keep, once however often it's asked, and lists it until it's let go; a pool it doesn't keep is let go already; a sandbox that keeps no pools refuses one Unsupported, and lists none",
            check: c17::<F>,
        },
        Case {
            id: "SANDBOX-18",
            says: "a launch's volume is made where its store has none and held by its run: listed with its bound, refused Volume(Bound) to a launch asking another bound while the run holds it, kept past the run, and cleared only once no run holds it, a clearing refused Volume(InUse) while one does and Volume(Unknown) once it's gone; a sandbox that keeps no volumes refuses one Unsupported, and lists and clears none",
            check: c18::<F>,
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
        fn keep(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
            self.0.keep(pool)
        }
        fn let_go(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
            self.0.let_go(pool)
        }
        fn pools(&self) -> Vec<PoolHeld> {
            self.0.pools()
        }
        fn volumes(&self) -> BoxFuture<'_, Result<Vec<KeptVolume>, Refusal>> {
            self.0.volumes()
        }
        fn clear_volume(&self, name: String) -> BoxFuture<'_, Result<(), Refusal>> {
            self.0.clear_volume(name)
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
        assert_eq!(failed, ["SANDBOX-5", "SANDBOX-6", "SANDBOX-7", "SANDBOX-10", "SANDBOX-11", "SANDBOX-13", "SANDBOX-14", "SANDBOX-15", "SANDBOX-16", "SANDBOX-17", "SANDBOX-18"], "{:?}", report.failed);
        assert!(report.failed.iter().all(|(_, why)| why.contains("doesn't hold it")), "{:?}", report.failed);
    }

    /// Holds everything, as the simulator does, but its runs take a connection whatever its grant names.
    struct Lax(SimSandbox);

    /// A run of [`Lax`]'s: the simulator's, but for the connections it takes unchecked.
    struct LaxRun(Box<dyn Run>);

    impl Run for LaxRun {
        fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
            self.0.stdin(bytes)
        }
        fn close_stdin(&self) {
            self.0.close_stdin();
        }
        fn next(&self) -> BoxFuture<'_, Option<Output>> {
            self.0.next()
        }
        fn signal(&self, signal: Signal) -> Result<(), Refusal> {
            self.0.signal(signal)
        }
        fn kill(&self) {
            self.0.kill();
        }
        fn id(&self) -> Option<String> {
            self.0.id()
        }
        fn link(&self) -> BoxFuture<'_, Option<Link>> {
            self.0.link()
        }
        fn change(&self, change: Change) -> BoxFuture<'_, Result<(), Refusal>> {
            match change {
                Change::Connect { .. } | Change::Disconnect { .. } => Box::pin(async { Ok(()) }),
                other => self.0.change(other),
            }
        }
        fn swap(&self, launch: Launch) -> BoxFuture<'_, Result<Box<dyn Run>, Refusal>> {
            self.0.swap(launch)
        }
    }

    impl Sandbox for Lax {
        fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
            Box::pin(async move { Ok(Box::new(LaxRun(self.0.launch(launch).await?)) as Box<dyn Run>) })
        }
        fn boundaries(&self) -> Vec<&'static str> {
            self.0.boundaries()
        }
        fn holds(&self) -> Holds {
            self.0.holds()
        }
        fn keep(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
            self.0.keep(pool)
        }
        fn let_go(&self, pool: Pool) -> BoxFuture<'_, Result<(), Refusal>> {
            self.0.let_go(pool)
        }
        fn pools(&self) -> Vec<PoolHeld> {
            self.0.pools()
        }
        fn volumes(&self) -> BoxFuture<'_, Result<Vec<KeptVolume>, Refusal>> {
            self.0.volumes()
        }
        fn clear_volume(&self, name: String) -> BoxFuture<'_, Result<(), Refusal>> {
            self.0.clear_volume(name)
        }
    }

    /// The simulator's launches, made by [`Lax`].
    struct LaxFixture(SimSandboxFixture, Lax);

    impl SandboxFixture for LaxFixture {
        fn sandbox(&self) -> &dyn Sandbox {
            &self.1
        }
        fn say(&self, text: &str) -> Launch {
            self.0.say(text)
        }
        fn echo(&self) -> Launch {
            self.0.echo()
        }
        fn fail(&self, code: i32) -> Launch {
            self.0.fail(code)
        }
        fn wait(&self) -> Launch {
            self.0.wait()
        }
        fn hog(&self, bytes: u64, limit: Option<u64>) -> Launch {
            self.0.hog(bytes, limit)
        }
        fn spawn(&self, n: u32, limit: Option<u32>) -> Launch {
            self.0.spawn(n, limit)
        }
        fn connect(&self, host: &str, egress: Egress) -> Launch {
            self.0.connect(host, egress)
        }
        fn fill(&self, host: &str, reference: &str, credentials: Vec<CredentialGrant>, egress: Egress) -> Launch {
            self.0.fill(host, reference, credentials, egress)
        }
        fn pwd(&self, folder: &str) -> Launch {
            self.0.pwd(folder)
        }
        fn components(&self, laid: Vec<ComponentAt>) -> Launch {
            self.0.components(laid)
        }
        fn folder(&self) -> String {
            self.0.folder()
        }
    }

    /// Kiln plan 01a1dd2e…, P6b.4, for Shards' D285: the suite isn't vacuous about a connection's grant. A sandbox
    /// whose runs take a connection whatever its grant names fails SANDBOX-14 for it, and only that clause.
    #[test]
    fn p6b_4_a_sandbox_that_takes_any_connection_fails_its_clause() {
        let make = || async { LaxFixture(SimSandboxFixture::default(), Lax(SimSandbox::new())) };
        let report = futures::executor::block_on(run("Sandbox", &cases::<LaxFixture>(), make));
        let failed: Vec<&str> = report.failed.iter().map(|(id, _)| *id).collect();
        assert_eq!(failed, ["SANDBOX-14"], "{:?}", report.failed);
        assert!(report.failed.iter().all(|(_, why)| why.contains("not refused Invalid")), "{:?}", report.failed);
    }

    /// Kiln plan 01a1dd2e…, P6b.4, for Shards' D285: a connection's grant names its name and its peer, each a host's
    /// name (a label may start with a digit, RFC 1123 §2.1; 63 bytes a label and 253 in all, RFC 1035 §2.3.4) and not
    /// the other in any case (RFC 4343), and ports 1 to 65535, the first at most the last; else it's refused Invalid,
    /// saying which.
    #[test]
    fn p6b_4_a_network_grant_is_whole_or_refused_invalid() {
        let grant = NetworkGrant { name: "alice".into(), peer: "bob.team".into(), ports: Ports { udp: true, first: 53, last: 53 }, way: Way::In };
        assert_eq!(grant.check(), Ok(()));
        let longest = format!("{}.{}.{}.{}", "a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(61));
        assert_eq!(longest.len(), 253);
        assert_eq!(NetworkGrant { name: "1a-b".into(), peer: longest.clone(), ports: Ports { udp: false, first: 1, last: u16::MAX }, way: Way::Out }.check(), Ok(()));
        for (says, bad) in [
            ("its name", NetworkGrant { name: String::new(), ..grant.clone() }),
            ("its peer", NetworkGrant { peer: "bob_1".into(), ..grant.clone() }),
            ("its peer", NetworkGrant { peer: "bob.".into(), ..grant.clone() }),
            ("its peer", NetworkGrant { peer: "-bob".into(), ..grant.clone() }),
            ("its peer", NetworkGrant { peer: format!("{longest}x"), ..grant.clone() }),
            ("its peer", NetworkGrant { peer: format!("{}.b", "a".repeat(64)), ..grant.clone() }),
            ("itself", NetworkGrant { peer: "Alice".into(), ..grant.clone() }),
            ("0 names no port", NetworkGrant { ports: Ports { first: 0, last: 0, ..grant.ports }, ..grant.clone() }),
            ("the first at most the last", NetworkGrant { ports: Ports { first: 2, last: 1, ..grant.ports }, ..grant.clone() }),
        ] {
            assert!(matches!(bad.check(), Err(Refusal::Invalid { what: "a grant on a network", ref why }) if why.contains(says)), "{says}: {:?}", bad.check());
        }
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
        // Kiln plan 01a1dd2e…, P6.2: a volume given as it starts, only where volumes are held, and one that names no
        // volume, no absolute place or no member is invalid before it's unheld.
        let volume = Volume { name: "cache".into(), bound: NonZeroU64::MIN, at: "/volumes/cache".into(), members: vec![Member("say".into())], read_only: false };
        let given = |v: Volume| Launch { volumes: vec![v], ..l(Limits::NONE, Egress::Any) };
        assert_eq!(Holds::ALL.check(&given(volume.clone())), Ok(()));
        assert_eq!(only_memory.check(&given(volume.clone())), Err(Refusal::Unsupported("volumes of its boundary's store given as it starts".into())));
        for bad in [Volume { name: String::new(), ..volume.clone() }, Volume { at: "volumes/cache".into(), ..volume.clone() }, Volume { members: Vec::new(), ..volume.clone() }] {
            assert!(matches!(only_memory.check(&given(bad.clone())), Err(Refusal::Invalid { what: "a volume", ref why }) if why.starts_with("volumes[0]")), "{bad:?}");
        }
    }

    /// Kiln plan 01a1dd2e…, P6.2: a launch refused for one of its volumes holds none of them, and the simulator keeps at
    /// most one volume for each run it may hold, a launch making one past them refused `Full` and one given a volume it
    /// keeps taken.
    #[test]
    fn p6_2_a_launch_refused_for_a_volume_holds_none_and_the_simulators_volumes_are_bounded() -> Result<(), Refusal> {
        let s = SimSandbox::new();
        let volume = |name: &str, bound: NonZeroU64| Volume { name: name.into(), bound, at: format!("/volumes/{name}"), members: vec![Member("wait".into())], read_only: false };
        let given = |volumes: Vec<Volume>| Launch { volumes, ..Launch::of("wait", &[]) };
        let held = futures::executor::block_on(s.launch(given(vec![volume("b", NonZeroU64::MIN)])))?;
        let refused = futures::executor::block_on(s.launch(given(vec![volume("a", NonZeroU64::MIN), volume("b", NonZeroU64::MIN.saturating_add(1))])));
        assert!(matches!(refused, Err(Refusal::Volume { cause: VolumeCause::Bound, .. })), "{:?}", refused.err());
        assert_eq!(futures::executor::block_on(s.clear_volume("a".into())), Err(Refusal::Volume { cause: VolumeCause::Unknown, why: "no volume a".into() }), "the launch refused made none");
        drop(held);
        assert_eq!(futures::executor::block_on(s.clear_volume("b".into())), Ok(()), "a run dropped holds its volume no more");
        for n in 0..SIM_RUNS {
            drop(futures::executor::block_on(s.launch(given(vec![volume(&format!("v{n}"), NonZeroU64::MIN)])))?);
        }
        assert!(matches!(futures::executor::block_on(s.launch(given(vec![volume("past", NonZeroU64::MIN)]))), Err(Refusal::Full { limit: SIM_RUNS })));
        assert!(futures::executor::block_on(s.launch(given(vec![volume("v0", NonZeroU64::MIN)]))).is_ok(), "a volume it keeps is given again");
        assert_eq!(futures::executor::block_on(s.volumes())?.len(), SIM_RUNS);
        Ok(())
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

    /// Kiln plan 01a1dd2e…, P3.2: the simulator keeps at most one pool for each run it may hold, refusing one past them
    /// `Full`; one kept already is kept still, and one let go frees its place.
    #[test]
    fn p3_2_the_simulators_pools_are_bounded() {
        let s = SimSandbox::new();
        let pool = |n: usize| Pool { image: format!("p@sha256:{n}"), size: None };
        for n in 0..SIM_RUNS {
            assert_eq!(futures::executor::block_on(s.keep(pool(n))), Ok(()));
        }
        assert_eq!(futures::executor::block_on(s.keep(pool(SIM_RUNS))), Err(Refusal::Full { limit: SIM_RUNS }));
        assert_eq!(futures::executor::block_on(s.keep(pool(0))), Ok(()), "one kept already");
        assert_eq!(s.pools().len(), SIM_RUNS);
        assert_eq!(futures::executor::block_on(s.let_go(pool(0))), Ok(()));
        assert_eq!(futures::executor::block_on(s.keep(pool(SIM_RUNS))), Ok(()), "a pool let go frees its place");
    }
}
