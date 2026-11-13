//! The machine's microVMs through Hex (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17; #73 part 7): the
//! `Sandbox` seam over Hex's runtime (`hex_api::Runtime`), each launch a Shards image box of its own, its harness
//! member driven.
//!
//! - **Its box** is named as placement names the boxes kiln runs on its machine (`reach::place::holder`,
//!   `BoxName`: `kiln-r…` or `kiln-z…`), so its keeper pairs it with what kiln holds there. The seam's launch
//!   carries no name, so the sandbox is given its namer ([`HexVms::new`]); a launch placement named goes through
//!   [`HexVms::launch_as`].
//! - **Its spec** is `ClassSpec::ShardsImage`: the image pinned by its digest; one `Grant::Egress` by TCP for each
//!   destination its launch names (`host:port`, to its harness member); its harness member, given the launch's
//!   environment, folder and arguments; the launch's shares, and its folder at its own path, writable.
//! - **Its size** is the least Shards boots (`shards_api::envelope` of no vCPU and no memory: one vCPU and
//!   shards-vmm's least memory for this architecture), as an allocation that envelope keeps: Shards gives an image
//!   run's microVM the host's memory to plug as its workload asks (D148, `api_run`'s `Shape::host_memory`), and the
//!   person's sizes reach it through placement (plan step 32). No number of kiln's.
//! - **Its run** is the member's: stdin, stdout and stderr its streams (`Proc::streams`), its end the member's,
//!   said once its box is gone (as Hex's own `Runtime::run` says a run's end), after all it wrote. Kill and drop
//!   delete its box (`Runtime::delete_with_host_use`). If a delete fails, it is asked again as the run's end is
//!   said. A delete answering `NoSuchBox` found the box already ended.
//! - **Its use** (#73 part 7 slice 3; plan step 32's charge) is read while it runs through Hex's `host_use`. Hex
//!   answers that on no event, so kiln reads it on its own charge cadence: placement's measures read a run's
//!   [`Meter`] as its group (`reach::place::Group::Hex`). As the run ends, what it cost is kept in kiln's records
//!   ([`USE`], [`HexVms::with_ledger`]) before anything acts on that end. The charge is Hex's final use (its
//!   window the whole run, read with its end), or the use last read where the delete answers none or
//!   `NoSuchBox`. A run never read and given no final use (it never started) is charged nothing.
//! - **Its credentials** (plan step 33, Shards' D242) are granted by reference: each of its launch's grants one of
//!   `ShardsImage::credentials`, to its harness member, toward the hosts it names, the reference the secret's name in
//!   kiln's store and never its value. Hex leases each value as the VM's egress first needs it, through the lessors
//!   its runtime was given (`hex_node::images::ImageRuntime::leasing`, kiln's `secrets::lessor::Lessors` over the
//!   node's run-lease broker), and fills it in outside the VM; the run sees placeholders alone. A sandbox whose
//!   runtime was given none ([`HexVms::leasing`] not called) holds no credentials: a launch granting one is refused,
//!   `Unsupported` ([`NO_BROKER`]), never run without them.
//! - **What it refuses, typed:** egress to whatever its image grants (`Egress::Any`): Hex's grants are default
//!   deny, so a run names its destinations; limits it doesn't hold ([`HOLDS`]), and credentials where no broker
//!   leases them ([`NO_BROKER`]); and what Hex refuses, in Hex's words (`Awaits` as `Unsupported`).
//! - **Its MCP lane** (Shards D254; #75 step 3): where its [`HexMcp`] carries one, its harness member is asked for its
//!   MCP bridge (`/run/shards/mcp`) and each connection its harness makes to it is taken through Hex
//!   (`super::Lanes`), for the runner to serve kiln's MCP over in its own process. The Hex kiln vendors carries none
//!   ([`Unvendored`]): its member is asked for nothing, and kiln's MCP is said to be missing, typed.

use std::io::{BufRead as _, Write as _};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::channel::oneshot;
use futures::future::BoxFuture;
use hex_api::{Allocation, BoxId, BoxSpec, CapacityResources, ClassSpec, Destination, Grant, HostUse, ImageMember, Member, MemberKind, Mount, Ports, Proc, Proto, Runtime, RuntimeCost, ShardsImage};
use kiln_ci::place::holder::BoxName;
use kiln_seams::Records;
use kiln_seams::records::{Tx, Write};
use serde::{Deserialize, Serialize};
use kiln_seams::sandbox::{Boundary, Egress, Exit, Holds, Launch, Link, Output, Refusal, Run, Sandbox, Signal};

/// What a box of Hex's holds its run to: no egress, egress to named destinations (each by TCP, to its harness
/// member), and folders shared at any path, read-only or not (`ShardsImage::shares`). Not memory or process limits
/// (Hex reads no OOM kill or refused fork of an image VM's guest: its `usage_of`), weights, signals (Hex awaits
/// `MemberSignal`, Shards' agent-control endpoint: they're asked of Hex all the same, so a Hex that delivers them
/// needs only this flag). Credentials it holds where its runtime leases them ([`HexVms::leasing`], its
/// [`Sandbox::holds`]); these say none.
pub const HOLDS: Holds = Holds { memory: false, processes: false, weights: false, no_egress: true, egress_by_host: true, shares: true, signals: false, credentials: false };

/// Why a launch granting a credential is refused where no broker leases this machine's credentials (module docs).
pub const NO_BROKER: &str = "credentials filled in at its egress: no run-lease broker leases this machine's microVMs' credentials (kiln's credential store is its server's, which this runner doesn't hold), and a run granted one is never run without it (Shards' D242)";
/// kiln's MCP lane through Hex (Shards D254; #75 step 3), the seam kiln reaches it by: Hex passes a member's
/// `MemberSpec::mcp` to Shards, beside the run's agent-control endpoint (`ImageSpec::control`, which Hex holds as the
/// run's controller), and takes each connection that member's harness makes to its bridge with `Execs::mcp(member)`
/// over that endpoint. Each answer is the seam's, typed.
pub trait HexMcp: Send + Sync {
    /// Whether this Hex carries the lane, and why not where it doesn't.
    fn carried(&self) -> Result<(), Refusal>;
    /// Asks `member` of `image` for its MCP bridge.
    fn ask(&self, image: &mut ShardsImage, member: &Member) -> Result<(), Refusal>;
    /// The next connection `member`'s harness makes to its bridge in box `id` of `runtime`, waited for until it
    /// comes. `Ended` once its box or its link is gone.
    fn next(&self, runtime: &dyn Runtime, id: &BoxId, member: &Member) -> Result<super::McpConnection, Refusal>;
}

/// Why the Hex kiln vendors gives no MCP lane: what Hex has to carry for it (Shards D254, on Shards' main at
/// af8f1ebe5).
pub const NO_MCP_LANE: &str = "kiln's MCP in a microVM: the Hex kiln vendors passes no member's MCP bridge to Shards (D254's `MemberSpec::mcp`, with the run's `control`) and takes none of its connections (`Execs::mcp`)";

/// The lane of the Hex kiln vendors (vendor/hex), which carries none ([`NO_MCP_LANE`]).
pub struct Unvendored;

impl HexMcp for Unvendored {
    fn carried(&self) -> Result<(), Refusal> {
        Err(Refusal::Unsupported(NO_MCP_LANE.into()))
    }

    fn ask(&self, _: &mut ShardsImage, _: &Member) -> Result<(), Refusal> {
        self.carried()
    }

    fn next(&self, _: &dyn Runtime, _: &BoxId, _: &Member) -> Result<super::McpConnection, Refusal> {
        Err(Refusal::Unsupported(NO_MCP_LANE.into()))
    }
}

/// The cost profile its allocations are made under: Shards' least VM, its envelope alone. No overhead and no guest
/// allowance: what a VM takes past its envelope Hex learns from its windows (decision 9, `RuntimeCost::census_free`).
const PROFILE: [u8; 32] = *b"kiln harness microVM, least size";

/// The machine's microVMs as Hex holds them: a Shards image box for each launch.
pub struct HexVms {
    /// Hex's runtime. Shared, as Hex hands it out: `hex_node::images::ImageRuntime::new` answers an `Arc` (its books
    /// hold a weak reference to it), and each run holds a clone for its box's delete, which outlives any borrow.
    runtime: Arc<dyn Runtime>,
    /// Names the box of a launch the seam names none for: placement's numbering (module docs); none where no
    /// placement runs to number it.
    names: Box<dyn Fn() -> Option<BoxName> + Send + Sync>,
    /// Kiln's records, where each run's charge is kept as it ends ([`USE`]). With none, nothing is kept and the use
    /// is said at debug.
    ledger: Option<Arc<dyn Records>>,
    /// Each live run's VM by its box's name, for its [`Meter`] and its MCP lane. Let go of with its run.
    vms: parking_lot::Mutex<HashMap<String, Weak<Vm>>>,
    /// Whether its runtime leases its VMs' credentials, given lessors over a run-lease broker ([`HexVms::leasing`]).
    leases: bool,
    /// kiln's MCP lane through Hex ([`HexMcp`]): [`Unvendored`] unless given one ([`HexVms::with_mcp`]).
    mcp: Box<dyn HexMcp>,
}

/// The family of kiln's records each microVM's charge is kept in as its run ends: keyed by its box's name, a
/// [`Charged`] in JSON.
pub const USE: &str = "hex.use";

/// Which use a run was charged: Hex's final one, read with its end, its window the whole run; or the last one read
/// while it ran, where its delete answered none or found it gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Charge {
    Final,
    LastRead,
}

/// A run's charge as [`USE`] keeps it: Hex's [`HostUse`], its window in nanoseconds, and which use it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charged {
    pub of: Charge,
    pub cpu_ns: u64,
    pub footprint_bytes: u64,
    pub resident_bytes: u64,
    pub guest_ns: Vec<u64>,
    pub window_ns: u64,
}

impl Charged {
    pub fn of(used: &HostUse, of: Charge) -> Self {
        let window_ns = u64::try_from(used.window.as_nanos()).unwrap_or(u64::MAX);
        Charged { of, cpu_ns: used.cpu_ns, footprint_bytes: used.footprint_bytes, resident_bytes: used.resident_bytes, guest_ns: used.guest_ns.clone(), window_ns }
    }
}

/// A run's VM as kiln charges it: Hex's runtime and its box, the use last read of it (Hex's final use once its
/// delete answered one), and the records its charge is kept in.
struct Vm {
    runtime: Arc<dyn Runtime>,
    id: BoxId,
    name: String,
    /// Its harness member, whose MCP connections its lane takes.
    member: Member,
    last: parking_lot::Mutex<Option<(HostUse, Charge)>>,
    ledger: Option<Arc<dyn Records>>,
}

/// What a run's VM uses, as one read of it says. Its CPUs are in thousandths, over the time since the read before
/// (over its whole window from its boot, at the first read). Its memory is counted as Hex counts a VM's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Used {
    pub cpu_milli: u64,
    pub footprint_bytes: u64,
}

/// A live run's VM, read on its charge's cadence ([`HexVms::meter`]).
#[derive(Clone)]
pub struct Meter(Arc<Vm>);

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Meter").field(&self.0.name).finish()
    }
}

impl Meter {
    /// Reads what its VM costs its host now, and keeps it as the use last read. This is Hex's `host_use`, short and
    /// synchronous as Hex's calls are. Its CPUs are its CPU time over its window's growth since the read before.
    /// Both are counters its run host reads together, so no clock of kiln's enters it. Refused, in Hex's words,
    /// where its box is gone or its run host read none.
    pub fn read(&self) -> Result<Used, hex_api::Refusal> {
        let now = self.0.runtime.host_use(&self.0.id)?;
        let mut last = self.0.last.lock();
        let (cpu0, window0) = match &*last {
            Some((u, Charge::LastRead)) if u.window <= now.window => (u.cpu_ns, u.window),
            _ => (0, Duration::ZERO),
        };
        let span = now.window.saturating_sub(window0).as_nanos();
        let milli = u128::from(now.cpu_ns.saturating_sub(cpu0)).saturating_mul(1000) / span.max(1);
        let used = Used { cpu_milli: u64::try_from(milli).unwrap_or(u64::MAX), footprint_bytes: now.footprint_bytes };
        // Once its delete has answered a final use, that use is what the run is charged: no read replaces it.
        if !matches!(*last, Some((_, Charge::Final))) {
            *last = Some((now, Charge::LastRead));
        }
        Ok(used)
    }
}

impl Vm {
    /// Keeps `used` as this run's charge in kiln's records, durably, before its caller acts on its end.
    async fn keep(&self, used: &HostUse, of: Charge) -> Result<(), String> {
        let Some(ledger) = &self.ledger else {
            tracing::debug!(vm = %self.name, ?of, cpu_ns = used.cpu_ns, footprint_bytes = used.footprint_bytes, window_ms = used.window.as_millis(), "its VM's cost over its run");
            return Ok(());
        };
        let value = serde_json::to_vec(&Charged::of(used, of)).map_err(|e| e.to_string())?;
        let put = Write::Put { family: USE.into(), key: self.name.as_bytes().to_vec(), value };
        ledger.commit(Tx { conds: Vec::new(), writes: vec![put] }).await.map(drop).map_err(|e| format!("its charge wasn't kept: {e}"))
    }
}

/// Deletes `vm`'s box and keeps what it cost before anything acts on its end (module docs).
/// - Where its delete answers a final use, that use is charged.
/// - Where it answers none (its end unread) or `NoSuchBox` (an earlier delete ended it and its answer was lost), the
///   use last read is charged. Where none was ever read (a run that never started), nothing is.
///
/// An error where its box may still be there or its charge wasn't kept, so a later delete asks again.
async fn finish(vm: &Arc<Vm>) -> Result<(), String> {
    let (runtime, id) = (Arc::clone(&vm.runtime), vm.id.try_clone().map_err(|e| e.to_string())?);
    let answered = crate::rt::blocking(move || runtime.delete_with_host_use(&id)).await.map_err(|e| format!("its delete didn't run: {e:#}"))?;
    match answered {
        Ok(Some(used)) => *vm.last.lock() = Some((used, Charge::Final)),
        Ok(None) | Err(hex_api::Refusal::NoSuchBox(_)) => {}
        Err(e) => return Err(e.to_string()),
    }
    let charge = vm.last.lock().clone();
    match charge {
        Some((used, of)) => vm.keep(&used, of).await,
        None => Ok(()),
    }
}

/// Why a launch with no box named is refused where no placement numbers one.
pub const UNNAMED: &str = "a microVM whose box no placement here names: kiln's placement isn't running on this machine";

impl HexVms {
    /// Over Hex's runtime `runtime`, naming each launch's box with `names`, which answers none where nothing numbers
    /// one (a launch then refused).
    pub fn new(runtime: Arc<dyn Runtime>, names: impl Fn() -> Option<BoxName> + Send + Sync + 'static) -> Self {
        Self { runtime, names: Box::new(names), ledger: None, vms: parking_lot::Mutex::new(HashMap::new()), mcp: Box::new(Unvendored), leases: false }
    }

    /// Takes kiln's MCP lane through `mcp` ([`HexMcp`]) from now on.
    pub fn with_mcp(self, mcp: impl HexMcp + 'static) -> Self {
        Self { mcp: Box::new(mcp), ..self }
    }

    /// Its runtime was given lessors over the node's run-lease broker (`hex_node::images::ImageRuntime::leasing`), so
    /// it holds launches granting credentials, each by reference (module docs).
    pub fn leasing(self) -> Self {
        Self { leases: true, ..self }
    }

    /// Keeps each run's charge in `ledger`, kiln's records ([`USE`]), as it ends.
    pub fn with_ledger(self, ledger: Arc<dyn Records>) -> Self {
        Self { ledger: Some(ledger), ..self }
    }

    /// The meter of live run `vm` (its box's name, `Run::id`), for placement to read its use on its charge's
    /// cadence. None once its run is let go of.
    pub fn meter(&self, vm: &str) -> Option<Meter> {
        self.vms.lock().get(vm).and_then(Weak::upgrade).map(Meter)
    }

    /// `launch` in a box named `name`: created, started and its harness member's streams taken, or refused, typed,
    /// with no box left behind.
    pub async fn launch_as(&self, name: BoxName, launch: Launch) -> Result<Box<dyn Run>, Refusal> {
        // Its grants checked as any sandbox's would be first (`Invalid`), then refused where no broker leases them.
        Holds { credentials: true, ..HOLDS }.check(&launch)?;
        if !launch.credentials.is_empty() && !self.leases {
            return Err(Refusal::Unsupported(NO_BROKER.into()));
        }
        let Boundary::MicroVm { image } = &launch.boundary else { return Err(Refusal::Unsupported("a run outside a microVM".into())) };
        let member = Member { kind: MemberKind::Harness, name: launch.program.clone() };
        let grants = grants_of(&member, &launch.egress)?;
        let credentials = credentials_of(&member, &launch.credentials);
        let mut shares: Vec<Mount> = launch.shares.iter().map(|s| Mount { source: PathBuf::from(&s.host), destination: s.at.clone(), read_only: !s.writable }).collect();
        if let Some(folder) = &launch.folder
            && !shares.iter().any(|s| &s.destination == folder)
        {
            shares.push(Mount { source: PathBuf::from(folder), destination: folder.clone(), read_only: false });
        }
        let driven = ImageMember { member: member.clone(), env: launch.env.clone(), workdir: launch.folder.clone(), args: launch.args.clone() };
        let mut class = ShardsImage { grants, members: vec![driven], shares, credentials, ..ShardsImage::new(image) };
        // Its MCP bridge where Hex carries the lane (Shards D254): nothing of it is a grant or an address.
        if self.mcp.carried().is_ok() {
            self.mcp.ask(&mut class, &member)?;
        }
        let id = name.id().map_err(seam)?;
        let spec = BoxSpec { id: id.try_clone().map_err(seam)?, bounds: hex_api::Bounds::default(), class: ClassSpec::ShardsImage(Box::new(class)), owner: None, devices: Vec::new(), allocation: Some(allocation()?), route: None, band: 0 };
        let name = id.to_string();
        let vm = Arc::new(Vm { runtime: Arc::clone(&self.runtime), id: id.try_clone().map_err(seam)?, name: name.clone(), member: member.clone(), last: parking_lot::Mutex::new(None), ledger: self.ledger.clone() });
        let runtime = Arc::clone(&self.runtime);
        let made = crate::rt::blocking(move || boot(&*runtime, spec, &id, &member)).await.map_err(|e| Refusal::Invalid { what: "its microVM", why: format!("{e:#}") })?;
        let started = match made {
            Ok(made) => HexRun::start(Arc::clone(&vm), made),
            // No box was made: none to delete (the name may be another's).
            Err((why, false)) => return Err(seam(why)),
            Err((why, true)) => Err(seam(why)),
        };
        match started {
            Ok(run) => {
                let mut vms = self.vms.lock();
                vms.retain(|_, v| v.strong_count() > 0);
                vms.insert(name, Arc::downgrade(&vm));
                Ok(Box::new(run))
            }
            Err(why) => {
                if let Err(e) = finish(&vm).await {
                    tracing::warn!(vm = %name, error = %e, "a microVM whose launch failed wasn't deleted");
                }
                Err(why)
            }
        }
    }
}

impl Sandbox for HexVms {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        match (self.names)() {
            Some(name) => Box::pin(self.launch_as(name, launch)),
            None => Box::pin(async { Err(Refusal::Unsupported(UNNAMED.into())) }),
        }
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["microvm"]
    }

    fn holds(&self) -> Holds {
        Holds { credentials: self.leases, ..HOLDS }
    }
}

/// `granted`'s credentials as Hex grants them to `member`: by reference alone, toward each host lowercased as its
/// egress grants name it ([`grants_of`]; RFC 4343), each once. Hex checks them against its grants as the box is made.
fn credentials_of(member: &Member, granted: &[kiln_seams::sandbox::CredentialGrant]) -> Vec<hex_api::CredentialGrant> {
    granted
        .iter()
        .map(|g| {
            let mut toward: Vec<String> = Vec::with_capacity(g.toward.len());
            for host in g.toward.iter().map(|h| h.to_ascii_lowercase()) {
                if !toward.contains(&host) {
                    toward.push(host);
                }
            }
            hex_api::CredentialGrant { member: member.clone(), reference: g.reference.clone(), toward }
        })
        .collect()
}

impl super::Lanes for HexVms {
    fn carried(&self) -> Result<(), Refusal> {
        self.mcp.carried()
    }

    /// Run `run`'s next MCP connection, taken through Hex. `Ended` once its run is let go of. Its VM isn't held
    /// while it waits, so a run let go of meanwhile is let go of.
    fn next(&self, run: &str) -> Result<super::McpConnection, Refusal> {
        let vm = self.vms.lock().get(run).and_then(Weak::upgrade).ok_or(Refusal::Ended)?;
        let (runtime, id, member) = (Arc::clone(&vm.runtime), vm.id.try_clone().map_err(seam)?, vm.member.clone());
        drop(vm);
        self.mcp.next(&*runtime, &id, &member)
    }
}

/// The grants of `egress` for `member`: one by TCP for each destination it names, `host:port` (a name, lowercase
/// as Hex takes one: RFC 4343 compares names case-insensitively; or an address, IPv6 in brackets); each once.
fn grants_of(member: &Member, egress: &Egress) -> Result<Vec<Grant>, Refusal> {
    let Egress::Only(hosts) = egress else {
        return Err(Refusal::Unsupported("egress to whatever its image grants: Hex's grants are default deny, so a microVM's run names each destination it reaches".into()));
    };
    let mut grants: Vec<Grant> = Vec::new();
    for h in hosts {
        let invalid = |why: &str| Refusal::Invalid { what: "its egress", why: format!("{h:?} {why}") };
        let (host, port) = h.rsplit_once(':').ok_or_else(|| invalid("names no port: a grant past a microVM names its port"))?;
        let port: u16 = port.parse().map_err(|_| invalid("names no port of 1 to 65535"))?;
        let bare = host.strip_prefix('[').and_then(|a| a.strip_suffix(']')).unwrap_or(host);
        let to = match bare.parse::<std::net::IpAddr>() {
            Ok(address) => Destination::Address(address),
            Err(_) => Destination::Name(host.to_ascii_lowercase()),
        };
        let grant = Grant::Egress { member: member.clone(), to, ports: Ports { proto: Proto::Tcp, first: port, last: port } };
        if !grants.contains(&grant) {
            grants.push(grant);
        }
    }
    Ok(grants)
}

/// Shards' least VM on this architecture as an allocation its envelope keeps (module docs): whole vCPUs and MiB,
/// in the units Hex's `Allocation::vm_size` reads them (1,000 CPU milli a vCPU, 2^20 bytes a MiB).
fn allocation() -> Result<Allocation, Refusal> {
    let size = |why: String| Refusal::Invalid { what: "its microVM's size", why };
    let (vcpus, mib) = shards_api::envelope(std::env::consts::ARCH, 0, 0).map_err(|e| size(format!("{e:?}")))?;
    let guest = CapacityResources {
        cpu_milli: u64::from(vcpus).checked_mul(1000).ok_or_else(|| size(format!("{vcpus} vCPUs")))?,
        mem_bytes: mib.checked_mul(1 << 20).ok_or_else(|| size(format!("{mib} MiB")))?,
        disk_bytes: 0,
    };
    let quantum = CapacityResources { cpu_milli: 1000, mem_bytes: 1 << 20, disk_bytes: 1 };
    let cost = RuntimeCost { id: PROFILE, floor: CapacityResources::default(), ceiling: guest, quantum, guest_overhead: CapacityResources::default(), overhead: CapacityResources::default() };
    cost.allocate(guest).map_err(seam)
}

/// Creates `spec`'s box, starts it and takes `member`'s run and streams. Otherwise says why, and whether it made the
/// box, which its caller then deletes ([`finish`]).
fn boot(runtime: &dyn Runtime, spec: BoxSpec, id: &BoxId, member: &Member) -> Result<(Box<dyn Proc>, hex_api::Given), (hex_api::Refusal, bool)> {
    runtime.create(spec).map_err(|why| (why, false))?;
    // A start answering `Lifecycle { Pending }` (its boot past Hex's learned patience, decision 9) is refused with
    // the rest: Hex's `Runtime` has no call that waits on a booting box (`member` and `inspect` answer at once), and
    // kiln asks again on no clock of its own (R03). Its delete ends the boot (Hex's change (b), in its push after
    // c7603b0b, which kiln vendors: c7603b0b may still answer that delete `Pending`, said as it is).
    let taken = runtime.start(id).and_then(|()| runtime.member(id, member)).and_then(|mut p| {
        let given = p.streams()?;
        Ok((p, given))
    });
    taken.map_err(|why| (why, true))
}

/// Hex's refusal as the seam's: an invalid request as it is; what Hex awaits, or its platform or configuration
/// doesn't give, unsupported; a node full, full; a box gone, ended; the rest invalid, in Hex's words.
fn seam(r: hex_api::Refusal) -> Refusal {
    use hex_api::Refusal as H;
    match r {
        H::Invalid { what, why } => Refusal::Invalid { what, why },
        H::Awaits { needs } => Refusal::Unsupported(needs.to_string()),
        H::Platform { what, platform, why } => Refusal::Unsupported(format!("{what} on {platform}: {why}")),
        H::Unconfigured { what, why } => Refusal::Unsupported(format!("{what}: {why}")),
        H::Full(limit) | H::Exhausted { bound: limit, .. } => Refusal::Full { limit },
        H::NoSuchBox(_) | H::NotRunning(..) => Refusal::Ended,
        other => Refusal::Invalid { what: "its microVM", why: other.to_string() },
    }
}

/// What a run's threads say to its `next`: its output, then its member's end.
enum Said {
    Out(Output),
    Ended(Result<hex_api::Exit, hex_api::Refusal>),
}

impl crate::memory::queue::Weigh for Said {
    fn weight(&self) -> usize {
        match self {
            Said::Out(Output::Stdout(b) | Output::Stderr(b)) => size_of::<Self>().saturating_add(b.capacity()),
            Said::Out(Output::Exited(_)) | Said::Ended(_) => size_of::<Self>(),
        }
    }
}

/// Where its box's delete stands: none asked, under way (whether it ended its box with its charge kept is still to
/// come), or done.
enum Life {
    Running,
    Deleting(oneshot::Receiver<bool>),
    Deleted,
}

/// A harness member's run in its box.
struct HexRun {
    /// Its VM: Hex's runtime, as Hex hands it out ([`HexVms::runtime`]), its box and its charge.
    vm: Arc<Vm>,
    stdin: crate::memory::queue::Sender<Vec<u8>>,
    said: crate::memory::queue::Receiver<Said>,
    reading: futures::lock::Mutex<()>,
    life: parking_lot::Mutex<Life>,
    killed: AtomicBool,
    given_end: AtomicBool,
}

impl HexRun {
    /// The run of the member [`boot`] took in box `id`: a thread writes its stdin, one reads each of its stdout and
    /// stderr, and one waits for its end, then closes its stdin and, all it wrote read, says its end. Refused where a
    /// thread can't start, and its caller deletes its box.
    fn start(vm: Arc<Vm>, (proc, given): (Box<dyn Proc>, hex_api::Given)) -> Result<HexRun, Refusal> {
        let failed = |why: String| Refusal::Invalid { what: "its microVM's streams", why };
        let (Some(stdin), Some(stdout), Some(stderr)) = (given.stdin, given.stdout, given.stderr) else {
            return Err(failed("Hex gave its member's streams short".into()));
        };
        let (in_tx, in_rx) = crate::memory::queue::pipe::<Vec<u8>>(&crate::memory::owners::CHANNELS);
        let (said_tx, said) = crate::memory::queue::pipe::<Said>(&crate::memory::owners::CHANNELS);
        let threads = (|| {
            let mut input = hex_api::OwnedStream::from(stdin);
            spawn("kiln-hexvm-stdin", move || {
                while let Ok(bytes) = in_rx.recv_blocking() {
                    if input.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })?;
            let out = spawn_reader("kiln-hexvm-stdout", hex_api::OwnedStream::from(stdout), said_tx.clone(), Output::Stdout)?;
            let err = spawn_reader("kiln-hexvm-stderr", hex_api::OwnedStream::from(stderr), said_tx.clone(), Output::Stderr)?;
            let (closing, mut proc) = (in_tx.clone(), proc);
            spawn("kiln-hexvm-end", move || {
                let end = loop {
                    match proc.wait_for(Duration::MAX) {
                        Ok(Some(end)) => break Ok(end),
                        Ok(None) => {}
                        Err(e) => break Err(e),
                    }
                };
                // Its input ends with it, so what it left reading it (a simulated member's program) lets its
                // output end too.
                closing.close();
                let _ = (out.join(), err.join());
                let _ = said_tx.send_blocking(Said::Ended(end));
            })
        })();
        if let Err(e) = threads {
            return Err(failed(format!("its threads: {e}")));
        }
        Ok(HexRun { vm, stdin: in_tx, said, reading: futures::lock::Mutex::new(()), life: parking_lot::Mutex::new(Life::Running), killed: AtomicBool::new(false), given_end: AtomicBool::new(false) })
    }

    fn over(&self) -> bool {
        self.killed.load(Ordering::Acquire) || self.given_end.load(Ordering::Acquire)
    }

    /// Deletes its box and keeps its charge ([`finish`]), unless a delete was asked already: kill's and drop's.
    fn delete_soon(&self) {
        let mut life = self.life.lock();
        if !matches!(*life, Life::Running) {
            return;
        }
        let (done, wait) = oneshot::channel();
        *life = Life::Deleting(wait);
        let vm = Arc::clone(&self.vm);
        drop(crate::rt::spawn(async move {
            let finished = finish(&vm).await;
            if let Err(e) = &finished {
                tracing::warn!(vm = %vm.name, error = %e, "a microVM wasn't deleted, or its charge wasn't kept: asked again as its end is said");
            }
            let _ = done.send(finished.is_ok());
        }));
    }

    /// Its box gone and its charge kept, by the delete under way or by one asked here. One is asked here again where
    /// the one under way failed: a delete whose answer was lost finds its box gone (`NoSuchBox`) and charges the use
    /// last read.
    async fn gone(&self) {
        let life = std::mem::replace(&mut *self.life.lock(), Life::Deleted);
        let finished = match life {
            Life::Running => false,
            Life::Deleting(wait) => wait.await.unwrap_or(false),
            Life::Deleted => true,
        };
        if !finished && let Err(e) = finish(&self.vm).await {
            tracing::warn!(vm = %self.vm.name, error = %e, "a microVM whose run ended wasn't deleted, or its charge wasn't kept");
        }
    }
}

/// Starts `f` on a thread named `name`.
fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name.to_owned()).spawn(f)
}

/// A thread handing on what `stream` says, as `wrap` says it, chunk by chunk as std's buffered reader takes them,
/// until its end, or until no one reads.
fn spawn_reader(name: &str, stream: hex_api::OwnedStream, said: crate::memory::queue::Sender<Said>, wrap: fn(Vec<u8>) -> Output) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn(name, move || {
        let mut r = std::io::BufReader::new(stream);
        loop {
            let chunk = match r.fill_buf() {
                Ok([]) => return,
                Ok(chunk) => chunk.to_vec(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            r.consume(chunk.len());
            if said.send_blocking(Said::Out(wrap(chunk))).is_err() {
                return;
            }
        }
    })
}

/// A member's end as the seam says it: killed through [`Run::kill`], or by KILL, `Killed`; its code; a signal's
/// as a shell says one (128 and its number); and an end no one collected (Hex's `Unknown`, or its wait refused)
/// with no code, -1, as the host's runs say theirs (`super::plain_end`).
fn end_of(end: Result<hex_api::Exit, hex_api::Refusal>, killed: bool) -> Exit {
    match end {
        _ if killed => Exit::Killed,
        Ok(hex_api::Exit::Code(c)) => Exit::Code(c),
        Ok(hex_api::Exit::Signal(n)) if n == hex_api::Signal::KILL.number() => Exit::Killed,
        Ok(hex_api::Exit::Signal(n)) => Exit::Code(n.saturating_add(128)),
        Ok(hex_api::Exit::Unknown) | Err(_) => Exit::Code(-1),
    }
}

impl Run for HexRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            if self.over() {
                return Err(Refusal::Ended);
            }
            self.stdin.send(bytes).await.map_err(|_| Refusal::Ended)
        })
    }

    fn close_stdin(&self) {
        self.stdin.close();
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        Box::pin(async move {
            let _reading = self.reading.lock().await;
            if self.given_end.load(Ordering::Acquire) {
                return None;
            }
            let end = match self.said.recv().await {
                Ok(Said::Out(o)) => return Some(o),
                Ok(Said::Ended(end)) => end,
                // Its threads gone with no end said: none was collected.
                Err(_) => Ok(hex_api::Exit::Unknown),
            };
            self.gone().await;
            self.given_end.store(true, Ordering::Release);
            Some(Output::Exited(end_of(end, self.killed.load(Ordering::Acquire))))
        })
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        if self.over() {
            return Err(Refusal::Ended);
        }
        let to = match signal {
            Signal::Interrupt => hex_api::Signal::INT,
            Signal::Terminate => hex_api::Signal::TERM,
        };
        self.vm.runtime.signal(&self.vm.id, to).map_err(seam)
    }

    fn kill(&self) {
        if self.given_end.load(Ordering::Acquire) {
            return;
        }
        self.killed.store(true, Ordering::Release);
        self.delete_soon();
    }

    fn id(&self) -> Option<String> {
        Some(self.vm.name.clone())
    }

    // Its stdio ride the member's streams from its run host, no link of Shards' agent control.
    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        Box::pin(async { None })
    }
}

impl Drop for HexRun {
    fn drop(&mut self) {
        self.delete_soon();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    use hex_api::sim::{SIM_IMAGE, SimRuntime};
    use hex_api::{BoxId, BoxInfo, BoxSpec, Class, Destination, Grant, HostUse, Member, MemberKind, Ports, Proc, Program, Proto, Refusal as H, Runtime, Stdio, Usage};
    use kiln_ci::place::holder::BoxName;
    use kiln_seams::Records as _;
    use kiln_seams::records::{Consistency, SimRecords};
    use kiln_seams::sandbox::{Boundary, Egress, Exit, Launch, Limits, Output, Refusal, Run, Sandbox as _, Signal};

    use super::{Charge, Charged, HexVms, USE, Used};

    /// The sandbox over Hex's simulator, its boxes numbered as a runner process of epoch 7 numbers those of its runs
    /// that reserve nothing.
    fn vms() -> (Arc<SimRuntime>, HexVms) {
        let sim = Arc::new(SimRuntime::new());
        let n = AtomicU64::new(0);
        let vms = HexVms::new(Arc::<SimRuntime>::clone(&sim), move || Some(BoxName::Unreserved { epoch: 7, n: n.fetch_add(1, Ordering::Relaxed) }));
        (sim, vms)
    }

    /// Claude Code's member of the simulator's image, resuming a conversation, in its project's folder.
    fn launch(egress: Egress) -> Launch {
        Launch { limits: Limits::NONE, egress, folder: Some("/work/project".into()), boundary: Boundary::MicroVm { image: SIM_IMAGE.to_owned() }, ..Launch::of("claude", &["--resume", "c1"]) }
    }

    /// Its output and its ends, to its last word.
    async fn drain(run: &dyn Run) -> (Vec<u8>, Vec<Exit>) {
        let (mut out, mut ends) = (Vec::new(), Vec::new());
        while let Some(said) = run.next().await {
            match said {
                Output::Stdout(b) => out.extend(b),
                Output::Stderr(_) => {}
                Output::Exited(e) => ends.push(e),
            }
        }
        (out, ends)
    }

    /// #73 part 7 slice 1 (plan step 17): a launch is a Shards image box of Hex's, named as placement names it, its
    /// harness member driven: what it's given on stdin comes back on its stdout, its end once its stdin closes is
    /// its own, said once, and by then its box is gone. Its destination by name is granted by TCP to its harness
    /// member: Hex's simulator at c7603b0b takes it (`unheld` awaits UDP and IPv6 alone, no longer every egress), so
    /// an IPv6 destination is what's refused, `Awaits` as `Unsupported`; egress to whatever its image grants is
    /// refused too; neither leaves a box.
    #[test]
    fn s17_p7_a_members_run_echoes_its_stdin_ends_and_its_box_goes() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (sim, vms) = vms();
            let run = vms.launch(launch(Egress::Only(vec!["API.anthropic.com:443".into(), "api.anthropic.com:443".into()]))).await?;
            assert_eq!(run.id().as_deref(), Some("kiln-z7-0"));
            let id = BoxId::new("kiln-z7-0")?;
            let harness = Member { kind: MemberKind::Harness, name: "claude".into() };
            let to = Destination::Name("api.anthropic.com".into());
            assert_eq!(sim.inspect(&id)?.grants, [Grant::Egress { member: harness, to, ports: Ports { proto: Proto::Tcp, first: 443, last: 443 } }], "one grant, by TCP to its port");
            run.stdin(b"one\n".to_vec()).await?;
            run.stdin(b"two\n".to_vec()).await?;
            run.close_stdin();
            let (out, ends) = drain(run.as_ref()).await;
            assert_eq!(String::from_utf8_lossy(&out), "one\ntwo\n");
            assert_eq!(ends, [Exit::Code(0)]);
            assert!(matches!(sim.inspect(&id), Err(hex_api::Refusal::NoSuchBox(_))), "its box is gone once its end is said");
            assert_eq!(run.stdin(b"late".to_vec()).await, Err(Refusal::Ended));
            assert_eq!(run.next().await, None);
            let any = vms.launch(launch(Egress::Any)).await.err();
            assert!(matches!(any, Some(Refusal::Unsupported(_))), "{any:?}");
            let v6 = vms.launch(launch(Egress::Only(vec!["[2001:db8::7]:443".into()]))).await.err();
            assert!(matches!(&v6, Some(Refusal::Unsupported(why)) if why.contains("IPv6")), "{v6:?}");
            let portless = vms.launch(launch(Egress::Only(vec!["api.anthropic.com".into()]))).await.err();
            assert!(matches!(portless, Some(Refusal::Invalid { what: "its egress", .. })), "{portless:?}");
            assert_eq!(sim.list()?, [], "no box is left of a refused launch");
            anyhow::Ok(())
        })
    }

    /// #73 part 7 slice 1: a signal Hex doesn't deliver yet (`MemberSignal`) is refused `Unsupported`, the run going
    /// on; a kill deletes its box, its end `Killed`, said once its box is gone; a run let go of takes its box with it.
    #[test]
    fn s17_p7_a_kill_and_a_drop_delete_the_box() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (sim, vms) = vms();
            let run = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            assert!(matches!(run.signal(Signal::Interrupt), Err(Refusal::Unsupported(_))));
            run.kill();
            let (_, ends) = drain(run.as_ref()).await;
            assert_eq!(ends, [Exit::Killed]);
            assert_eq!(sim.list()?, [], "its box is gone once its end is said");
            assert_eq!(run.signal(Signal::Terminate), Err(Refusal::Ended));
            let dropped = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            assert_eq!(sim.list()?.len(), 1);
            drop(dropped);
            // Its delete runs on the blocking pool: its box goes, and stays gone.
            for _ in 0..1000 {
                if sim.list()?.is_empty() {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(sim.list()?, [], "a run let go of takes its box with it");
            anyhow::Ok(())
        })
    }

    /// A stand-in for kiln's MCP lane through Hex (Shards D254's controller): each ask kept with the members of the
    /// image it was asked in, and each box and member a connection was asked of; a connection that ends at once handed
    /// out for each.
    #[derive(Default, Clone)]
    struct StandInMcp {
        asks: Arc<parking_lot::Mutex<Vec<(Vec<Member>, Member)>>>,
        nexts: Arc<parking_lot::Mutex<Vec<(String, Member)>>>,
    }

    impl super::HexMcp for StandInMcp {
        fn carried(&self) -> Result<(), Refusal> {
            Ok(())
        }

        fn ask(&self, image: &mut hex_api::ShardsImage, member: &Member) -> Result<(), Refusal> {
            self.asks.lock().push((image.members.iter().map(|m| m.member.clone()).collect(), member.clone()));
            Ok(())
        }

        fn next(&self, _: &dyn Runtime, id: &BoxId, member: &Member) -> Result<crate::sandbox::McpConnection, Refusal> {
            self.nexts.lock().push((id.to_string(), member.clone()));
            Ok(crate::sandbox::fake::connection(&[]).0)
        }
    }

    /// #75 step 3 (Shards D254): where Hex carries kiln's MCP lane, a launch asks its harness member's bridge in its
    /// box's image, and the lane takes that member's connections in its run's box, by the run's name; a run of no name
    /// here, or one let go of, has none (`Ended`). Nothing of it is a grant: its egress is what its launch named.
    #[test]
    fn s35_mcp_a_launch_asks_its_members_bridge_and_its_lane_takes_its_connections() -> anyhow::Result<()> {
        use crate::sandbox::Lanes as _;
        crate::rt::block_on(async {
            let (sim, vms) = vms();
            let mcp = StandInMcp::default();
            let vms = vms.with_mcp(mcp.clone());
            vms.carried()?;
            let run = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            let harness = Member { kind: MemberKind::Harness, name: "claude".into() };
            assert_eq!(*mcp.asks.lock(), [(vec![harness.clone()], harness.clone())], "its harness member's bridge, asked once");
            assert_eq!(sim.inspect(&BoxId::new("kiln-z7-0")?)?.grants, [], "no grant for kiln's MCP");
            let _conn = vms.next("kiln-z7-0")?;
            assert_eq!(*mcp.nexts.lock(), [("kiln-z7-0".to_owned(), harness)], "its member's connection, in its box");
            assert!(matches!(vms.next("kiln-z7-9"), Err(Refusal::Ended)), "no run of that name");
            drop(run);
            let mut gone = false;
            for _ in 0..1000 {
                if matches!(vms.next("kiln-z7-0"), Err(Refusal::Ended)) {
                    gone = true;
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(gone, "a run let go of has no more connections");
            anyhow::Ok(())
        })
    }

    /// #75 step 3: the Hex kiln vendors carries no MCP lane (Shards D254 isn't in its API): it says so, typed, naming
    /// what Hex must carry; a launch asks nothing of it and still runs; and no connection is taken.
    #[test]
    fn s35_mcp_the_vendored_hex_says_it_carries_no_lane() -> anyhow::Result<()> {
        use crate::sandbox::Lanes as _;
        crate::rt::block_on(async {
            let (_sim, vms) = vms();
            assert_eq!(vms.carried(), Err(Refusal::Unsupported(super::NO_MCP_LANE.into())));
            assert!(super::NO_MCP_LANE.contains("MemberSpec::mcp") && super::NO_MCP_LANE.contains("Execs::mcp"), "names what Hex must carry");
            let run = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            assert!(matches!(vms.next("kiln-z7-0"), Err(Refusal::Unsupported(_))));
            run.kill();
            let (_, ends) = drain(run.as_ref()).await;
            assert_eq!(ends, [Exit::Killed]);
            anyhow::Ok(())
        })
    }

    /// Hex's simulator with a VM's host use as Hex's image runtime answers it (`host_use`, and the final use
    /// `delete_with_host_use` answers, its window the whole run, `None` for a box never started), which the
    /// simulator doesn't model. Read `n` is `use_at(n)`. Its first delete may lose its answer (the box deleted,
    /// an error said), and its start may be refused. Its credentials as Hex's image class leases them (plan step 33),
    /// which the simulator doesn't model either: a box granting one refused `Unconfigured` as it is created where it
    /// was given no lessors (`ImageClass::can_lease`), else its run's lessor asked of them as it starts
    /// (`ImageClass::run_lessor`) and kept by box; and each spec it was asked to create kept as its wire writes it.
    #[derive(Default)]
    pub(crate) struct Metered {
        sim: SimRuntime,
        started: parking_lot::Mutex<std::collections::HashSet<String>>,
        reads: AtomicU64,
        deletes: AtomicU64,
        lose_first_answer: AtomicBool,
        refuse_start: AtomicBool,
        lessors: parking_lot::Mutex<Option<Arc<dyn hex_api::lease::Lessors>>>,
        lessees: parking_lot::Mutex<Vec<(String, Arc<dyn hex_api::lease::Lessor>)>>,
        specs: parking_lot::Mutex<Vec<String>>,
    }

    /// Read `n` of a VM's use: its CPU time growing as n squared, its window as n, so a read's CPUs are its
    /// growth since the read before, not its whole run's.
    fn use_at(n: u64) -> HostUse {
        HostUse { cpu_ns: n * n * 1_000_000, footprint_bytes: (64 << 20) + n, resident_bytes: (80 << 20) + n, guest_ns: vec![n * 500_000], window: Duration::from_millis(n * 4) }
    }

    /// A VM's final use, as its delete reads it with its end.
    fn final_use() -> HostUse {
        HostUse { cpu_ns: 90_000_000, footprint_bytes: 96 << 20, resident_bytes: 112 << 20, guest_ns: vec![70_000_000], window: Duration::from_secs(1) }
    }

    impl Runtime for Metered {
        fn classes(&self) -> Vec<Class> {
            self.sim.classes()
        }
        fn create(&self, spec: BoxSpec) -> Result<BoxInfo, H> {
            self.specs.lock().push(serde_json::to_string(&spec).map_err(|e| H::Broken(e.to_string()))?);
            if matches!(&spec.class, hex_api::ClassSpec::ShardsImage(image) if !image.credentials.is_empty()) && self.lessors.lock().is_none() {
                return Err(H::Unconfigured { what: "lessors for its image VMs' credentials", why: "given none".into() });
            }
            self.sim.create(spec)
        }
        fn start(&self, id: &BoxId) -> Result<(), H> {
            if self.refuse_start.load(Ordering::Acquire) {
                return Err(H::Invalid { what: "start", why: "refused by the test".into() });
            }
            if !self.sim.inspect(id)?.credentials.is_empty() {
                let lessors = self.lessors.lock().clone().ok_or_else(|| H::Broken("a box granting credentials with no lessors".into()))?;
                let lessor = lessors.lessor(id)?;
                self.lessees.lock().push((id.to_string(), lessor));
            }
            self.sim.start(id)?;
            self.started.lock().insert(id.to_string());
            Ok(())
        }
        fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, H> {
            self.sim.exec(id, program, stdio)
        }
        fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, H> {
            self.sim.run(spec, program, stdio)
        }
        fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), H> {
            self.sim.signal(id, signal)
        }
        fn inspect(&self, id: &BoxId) -> Result<BoxInfo, H> {
            self.sim.inspect(id)
        }
        fn usage(&self, id: &BoxId) -> Result<Usage, H> {
            self.sim.usage(id)
        }
        fn list(&self) -> Result<Vec<BoxInfo>, H> {
            self.sim.list()
        }
        fn delete(&self, id: &BoxId) -> Result<(), H> {
            self.sim.delete(id)
        }
        fn member(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, H> {
            self.sim.member(id, member)
        }
        fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<HostUse>, H> {
            self.deletes.fetch_add(1, Ordering::AcqRel);
            self.sim.delete(id)?;
            let started = self.started.lock().remove(id.as_str());
            if self.lose_first_answer.swap(false, Ordering::AcqRel) {
                return Err(H::Invalid { what: "delete", why: "its answer was lost".into() });
            }
            Ok(started.then(final_use))
        }
        fn host_use(&self, id: &BoxId) -> Result<HostUse, H> {
            self.sim.inspect(id)?;
            Ok(use_at(self.reads.fetch_add(1, Ordering::AcqRel) + 1))
        }
    }

    /// The sandbox over [`Metered`], its charges kept in records of its own.
    fn metered() -> (Arc<Metered>, Arc<SimRecords>, HexVms) {
        let (rt, records) = (Arc::new(Metered::default()), Arc::new(SimRecords::new()));
        let n = AtomicU64::new(0);
        let vms = HexVms::new(Arc::<Metered>::clone(&rt), move || Some(BoxName::Unreserved { epoch: 7, n: n.fetch_add(1, Ordering::Relaxed) })).with_ledger(Arc::<SimRecords>::clone(&records));
        (rt, records, vms)
    }

    /// The charge kept for VM `vm`, if any.
    async fn charged(records: &SimRecords, vm: &str) -> anyhow::Result<Option<Charged>> {
        let row = records.get(USE, vm.as_bytes(), Consistency::Linearizable).await?;
        Ok(row.map(|r| serde_json::from_slice(&r.value)).transpose()?)
    }

    /// Reads its output until its end is said: the charge kept for `vm` at that moment.
    async fn charged_as_it_ends(run: &dyn Run, records: &SimRecords, vm: &str) -> anyhow::Result<(Exit, Option<Charged>)> {
        while let Some(said) = run.next().await {
            if let Output::Exited(end) = said {
                return Ok((end, charged(records, vm).await?));
            }
        }
        anyhow::bail!("its run ended with no end said")
    }

    /// #73 part 7 slice 3 (plan step 17): a run's use is read on its charge's cadence through its meter, its CPUs
    /// over the window since the read before. A kill whose delete loses its answer (its box deleted) has its
    /// delete asked again as its end is said; that one answers `NoSuchBox`, so the run ended, and it's charged
    /// the use last read, kept in kiln's records.
    #[test]
    fn s17_p7_s3_a_retried_delete_finding_its_box_gone_charges_the_use_last_read() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (rt, records, vms) = metered();
            let run = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            let meter = vms.meter("kiln-z7-0").ok_or_else(|| anyhow::anyhow!("a live run has its meter"))?;
            assert_eq!(meter.read()?, Used { cpu_milli: 250, footprint_bytes: use_at(1).footprint_bytes }, "1 ms of CPU over its first 4 ms");
            assert_eq!(meter.read()?, Used { cpu_milli: 750, footprint_bytes: use_at(2).footprint_bytes }, "3 ms more over 4 ms more");
            rt.lose_first_answer.store(true, Ordering::Release);
            run.kill();
            let (end, kept) = charged_as_it_ends(run.as_ref(), &records, "kiln-z7-0").await?;
            assert_eq!(end, Exit::Killed);
            assert_eq!(rt.deletes.load(Ordering::Acquire), 2, "its delete asked again, which found it gone");
            assert_eq!(kept, Some(Charged::of(&use_at(2), Charge::LastRead)), "charged the use last read");
            assert_eq!(rt.sim.list()?, []);
            assert!(matches!(meter.read(), Err(H::NoSuchBox(_))), "a VM gone reads nothing");
            drop((run, meter));
            assert!(vms.meter("kiln-z7-0").is_none(), "a run let go of has no meter");
            anyhow::Ok(())
        })
    }

    /// #73 part 7 slice 3: the final use a delete answers is in kiln's records before the run's end is said, both
    /// for a run that ended on its own and for a kill, whose final use is charged over any use read before it.
    #[test]
    fn s17_p7_s3_the_final_use_is_kept_before_its_end_is_said() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (_rt, records, vms) = metered();
            let run = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            run.close_stdin();
            let (end, kept) = charged_as_it_ends(run.as_ref(), &records, "kiln-z7-0").await?;
            assert_eq!(end, Exit::Code(0));
            assert_eq!(kept, Some(Charged::of(&final_use(), Charge::Final)), "kept as its end is said");
            let killed = vms.launch(launch(Egress::Only(Vec::new()))).await?;
            vms.meter("kiln-z7-1").ok_or_else(|| anyhow::anyhow!("a live run has its meter"))?.read()?;
            killed.kill();
            let (end, kept) = charged_as_it_ends(killed.as_ref(), &records, "kiln-z7-1").await?;
            assert_eq!(end, Exit::Killed);
            assert_eq!(kept, Some(Charged::of(&final_use(), Charge::Final)), "its final use, not the one read before");
            anyhow::Ok(())
        })
    }

    /// #73 part 7 slice 3: a run that never started (its start refused) is refused, its box deleted, and charged
    /// nothing: its delete answers no use, and none was read.
    #[test]
    fn s17_p7_s3_a_run_that_never_started_is_charged_nothing() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (rt, records, vms) = metered();
            rt.refuse_start.store(true, Ordering::Release);
            let refused = vms.launch(launch(Egress::Only(Vec::new()))).await.err();
            assert!(matches!(refused, Some(Refusal::Invalid { what: "start", .. })), "{refused:?}");
            assert_eq!(rt.deletes.load(Ordering::Acquire), 1, "the box it made was deleted");
            assert_eq!(rt.sim.list()?, []);
            assert_eq!(charged(&records, "kiln-z7-0").await?, None, "charged nothing");
            assert!(vms.meter("kiln-z7-0").is_none());
            anyhow::Ok(())
        })
    }

    /// Credential `reference`'s value in the store the tests' broker leases from: never in a spec, a log or the VM.
    const REFERENCE: &str = "providers/anthropic";
    const VALUE: &str = "sk-ant-api03-s33-never-in-a-spec";

    /// Claude Code's launch reaching api.anthropic.com by TCP on 443, granted [`REFERENCE`] toward it.
    fn granted() -> Launch {
        let credentials = vec![kiln_seams::sandbox::CredentialGrant { reference: REFERENCE.to_owned(), toward: vec!["api.anthropic.com".to_owned()] }];
        Launch { credentials, ..launch(Egress::Only(vec!["api.anthropic.com:443".into()])) }
    }

    /// #74 credentials (plan step 33, Shards' D242): a launch granting a credential reaches Hex with it by reference,
    /// to its harness member toward its host, and a lessor of kiln's (`secrets::lessor::Lessors`, over the run-lease
    /// broker) asked for its run as it starts, which leases the value by that reference alone. No spec Hex was asked
    /// to create holds the value.
    #[test]
    fn s33_a_credential_grant_reaches_hex_by_reference_with_a_lessor() -> anyhow::Result<()> {
        use kiln_seams::secrets::{SecretValue, Secrets as _, SimSecrets};

        let clock = kiln_seams::clock::SimClock::new();
        let lessors = crate::rt::block_on(async move {
            let secrets = Arc::new(SimSecrets::new(Arc::new(clock.clone())));
            secrets.create(REFERENCE, SecretValue::new(VALUE.as_bytes().to_vec())).await.map_err(anyhow::Error::msg)?;
            let broker = crate::secrets::broker::Broker::open(secrets, crate::store::Store::in_memory()?, Arc::new(clock.clone())).await?;
            anyhow::Ok(crate::secrets::lessor::Lessors::new(Arc::new(broker), Arc::new(clock.clone())))
        })?;
        let (rt, _records, vms) = metered();
        *rt.lessors.lock() = Some(Arc::new(lessors));
        let vms = vms.leasing();
        anyhow::ensure!(vms.holds().credentials, "a sandbox whose runtime leases says it holds no credentials");
        let (vms, run) = crate::rt::block_on(async move {
            let run = vms.launch(granted()).await?;
            anyhow::Ok((vms, run))
        })?;
        let id = BoxId::new("kiln-z7-0")?;
        let harness = Member { kind: MemberKind::Harness, name: "claude".into() };
        let by_reference = hex_api::CredentialGrant { member: harness, reference: REFERENCE.to_owned(), toward: vec!["api.anthropic.com".to_owned()] };
        assert_eq!(rt.sim.inspect(&id)?.credentials, [by_reference], "its grant, by reference, to its harness member");
        let specs = rt.specs.lock().clone();
        anyhow::ensure!(specs.len() == 1 && specs.iter().all(|s| s.contains(REFERENCE) && !s.contains(VALUE)), "a spec holds the value or no reference: {specs:?}");
        let lessee = rt.lessees.lock().iter().find(|(b, _)| b == "kiln-z7-0").map(|(_, l)| Arc::clone(l));
        let lessor = lessee.ok_or_else(|| anyhow::anyhow!("no lessor was asked for its run"))?;
        // As Hex's run host asks, off kiln's runtime: the value leased by its reference.
        let leased = lessor.lease(REFERENCE, None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(leased.value.expose() == VALUE.as_bytes(), "the lessor leased another value");
        lessor.release(&leased.lease);
        crate::rt::block_on(async move {
            run.kill();
            let (_, ends) = drain(run.as_ref()).await;
            assert_eq!(ends, [Exit::Killed]);
            drop(vms);
            anyhow::Ok(())
        })
    }

    /// #74 credentials (plan step 33): where no broker leases this machine's credentials, a launch granting one is
    /// refused, typed (`Unsupported`, [`super::NO_BROKER`]), and leaves no box: never run without its credential.
    /// A grant toward a host its egress doesn't reach is `Invalid` first, as any sandbox's is.
    #[test]
    fn s33_a_credential_grant_with_no_broker_is_refused_typed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (sim, vms) = vms();
            anyhow::ensure!(!vms.holds().credentials, "a sandbox with no broker says it holds credentials");
            let refused = vms.launch(granted()).await.err();
            anyhow::ensure!(matches!(&refused, Some(Refusal::Unsupported(why)) if why == super::NO_BROKER), "{refused:?}");
            let unreached = Launch { egress: Egress::Only(vec!["example.com:443".into()]), ..granted() };
            let refused = vms.launch(unreached).await.err();
            anyhow::ensure!(matches!(&refused, Some(Refusal::Invalid { what: "a credential grant", .. })), "{refused:?}");
            assert_eq!(sim.list()?, [], "a refused launch leaves no box");
            anyhow::Ok(())
        })
    }
}
