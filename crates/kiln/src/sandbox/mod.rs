//! Where kiln's harnesses run (docs/isolation.md; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17;
//! CP-T4.1): the `Sandbox` seam's implementations in kiln (`kiln_seams::sandbox`), and an isolated
//! agent's launch through them.
//!
//! - [`host`]: today's launch, a process on the machine. It holds none of the seam's limits, and
//!   refuses them typed.
//! - The machine's microVMs ([`microvm`]): an isolated agent's harness runs as a member of its
//!   Agentfile's image in a microVM of its own (Shards' D231), through Hex; its stdio are the
//!   member's streams. A kiln that carries Shards holds them ([`hold`]: Hex's image runtime under
//!   kiln's state, [`hexvm`]); one that carries none holds no isolated agent, and refuses one rather
//!   than run it as a process.
//! - [`isolated`]: an isolated session's launch, its image, and what it can't do yet.
//! - [`narrow`]: what its microVM reaches past it, of what its image grants: its session's own
//!   provider's or sign-in's destinations alone, else its image's whole and why.
//! - [`image`]: what its microVM is made from: its Agentfile's image, built by the carried `shards`
//!   once for each content, pinned by its digest.
//! - [`hexvm`]: the machine's microVMs through Hex: each launch a Shards image box, its harness member driven.
//! - [`held`]: a boundary's run as the `Duplex` a session drives its harness through, so a session's
//!   loop and its adapter treat a microVM's member as they treat a process of the machine's.
//! - [`pools`]: the warm pools kiln names on the machine's microVMs from its agents' starts, and lets
//!   go of after their keep window (kiln plan 01a1dd2e…, P3.2), and each runner's as the server shows them.
//! - [`windows_path`]: a Windows folder's place, judged from its text as Windows judges it, on any host: what an
//!   isolated session's harness on a Windows machine is told of its folders (docs/agentic.md W8, piece 2).

#[cfg(test)]
pub mod container;
pub mod fake;
pub mod held;
pub mod hexvm;
pub mod host;
pub mod image;
pub mod isolated;
pub mod narrow;
pub mod pools;
pub mod served;
pub mod windows_path;

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use kiln_seams::Sandbox;
use kiln_seams::sandbox::{Exit, Link, Output, Refusal, Run, Signal};

/// A machine's microVMs: the sandbox an isolated agent's harness runs in, a microVM of its own, and
/// the images they boot.
pub struct Microvms {
    pub sandbox: Arc<dyn Sandbox>,
    pub images: image::Images,
    /// The sandbox as Hex's, where it is: what launches a run in the box placement named for it.
    pub hex: Option<Arc<hexvm::HexVms>>,
    /// The carried Shards they run, written out and held as long as they are (`crate::shards::write_out`).
    shards: Option<crate::shards::Held>,
    /// kiln's MCP lane of the runs launched here (Shards D254): Hex's, where they are Hex's; a stand-in's.
    lanes: Option<Arc<dyn Lanes>>,
    /// kiln's state they're held beneath ([`held_in`]), which no isolated agent's project is, holds or lies in
    /// (`isolated::kept_out`); none for a stand-in's.
    state: Option<std::path::PathBuf>,
    /// The warm pools kiln names on them, from the starts launched here (kiln plan 01a1dd2e…, P3.2).
    pub pools: Arc<pools::Pools>,
    /// Hex's image runtime they're held in, where [`held_in`] held them: its node, whose listing of the class its
    /// image VMs run as their facts say (kiln plan 01a1dd2e…, P2.4; [`facts_of`]).
    node: Option<Arc<hex_node::images::ImageRuntime>>,
}

/// One connection a run's harness made to its MCP bridge in its microVM (Shards D254, `Execs::mcp`'s
/// `McpConnection`): what it sends, newline-delimited JSON-RPC, and what it is sent. Each way ends on its own.
pub struct McpConnection {
    pub from: Box<dyn std::io::Read + Send>,
    pub to: Box<dyn std::io::Write + Send>,
}

/// kiln's MCP lane of the runs a machine's microVMs launch (Shards D254; #75 step 3): each connection a run's
/// harness makes to its bridge, which the runner serves kiln's MCP over in its own process
/// (`isolated::bridge`). No network: nothing of it is an address in the microVM.
pub trait Lanes: Send + Sync {
    /// Whether the runs launched here have the lane, and why not where they don't.
    fn carried(&self) -> Result<(), Refusal>;
    /// The next connection run `run`'s harness makes to its bridge, waited for until it comes. `Ended` once its
    /// run, its box or its link is gone; another refusal says why it has none.
    fn next(&self, run: &str) -> Result<McpConnection, Refusal>;
}

/// Why runs launched in microVMs held with no lane have kiln's MCP.
pub const NO_LANES: &str = "kiln's MCP in a microVM: these microVMs carry no MCP lane (Shards D254)";

impl Microvms {
    /// Runs in `sandbox`, booting `images`: a stand-in's, launched as the seam launches.
    pub fn new(sandbox: Arc<dyn Sandbox>, images: image::Images) -> Self {
        let pools = pools::Pools::new(Arc::clone(&sandbox), crate::rt::clock());
        Self { sandbox, images, hex: None, shards: None, lanes: None, state: None, pools, node: None }
    }

    /// kiln's state they're held beneath, where [`held_in`] held them.
    pub fn state(&self) -> Option<&std::path::Path> {
        self.state.as_deref()
    }

    /// Their runs' MCP connections taken through `lanes` from now on (a stand-in's).
    pub fn with_lanes(self, lanes: Arc<dyn Lanes>) -> Self {
        Self { lanes: Some(lanes), ..self }
    }

    /// kiln's MCP lane of the runs launched here, or why they have none (Shards D254).
    pub fn mcp(&self) -> Result<Arc<dyn Lanes>, Refusal> {
        let lanes = self.lanes.clone().ok_or_else(|| Refusal::Unsupported(NO_LANES.into()))?;
        lanes.carried()?;
        Ok(lanes)
    }

    /// `launch` in a microVM of its own, `size` (vCPUs, MiB) where its session gives one, its harness granted
    /// `sign_ins` (plan step 33; `hexvm`'s module docs): in box `name` where placement named one for its run (its
    /// reservation's, `Holder::box_of`) and its sandbox is Hex's; else named as its sandbox names one (placement's
    /// numbering). A stand-in's sandbox sizes none, and refuses a sign-in, which only Hex's microVMs grant.
    pub async fn launch(&self, name: Option<kiln_ci::place::holder::BoxName>, launch: kiln_seams::sandbox::Launch, size: Option<(u32, u64)>, sign_ins: &[crate::secrets::signin::SignIn]) -> Result<Box<dyn Run>, Refusal> {
        match (&self.hex, name) {
            (Some(hex), Some(name)) => hex.launch_as(name, launch, size, sign_ins).await,
            (Some(hex), None) => hex.launch_sized(launch, size, sign_ins).await,
            (None, _) if !sign_ins.is_empty() => Err(Refusal::Unsupported("a harness's sign-in captured outside its microVM".into())),
            (None, _) => self.sandbox.launch(launch).await,
        }
    }

    /// `held`'s member swapped for `launch`'s in the microVM it runs in (`Run::swap`; kiln plan 01a1dd2e…, P6c.5: a
    /// harness switched in place), its harness granted `sign_ins` as [`Microvms::launch`] grants a new microVM's (plan
    /// step 33), never left to sign in itself there, its tokens in its microVM. Hex grants a sign-in only as a box is
    /// made (its C8), and has no call yet that swaps a member in place and grants it one (`hexvm::NO_SWAP_SIGN_IN`); a
    /// stand-in grants none: a swap whose harness has a sign-in is refused, typed, by either, and its run goes on.
    pub async fn swap(&self, held: &held::Held, launch: kiln_seams::sandbox::Launch, sign_ins: &[crate::secrets::signin::SignIn]) -> Result<Box<dyn Run>, Refusal> {
        match swap_refused(self.hex.is_some(), sign_ins) {
            Some(refused) => Err(refused),
            None => held.swap(launch).await,
        }
    }
}

/// Why a swap whose harness has `sign_ins` is refused by microVMs that are Hex's (`hex`) or a stand-in's
/// ([`Microvms::swap`]); none for one with none, which its boundary swaps as before.
fn swap_refused(hex: bool, sign_ins: &[crate::secrets::signin::SignIn]) -> Option<Refusal> {
    match (hex, sign_ins.is_empty()) {
        (_, true) => None,
        (true, false) => Some(Refusal::Unsupported(hexvm::NO_SWAP_SIGN_IN.into())),
        (false, false) => Some(Refusal::Unsupported("a harness's sign-in captured outside its microVM".into())),
    }
}

/// What leases a machine's microVMs' credentials (plan step 33, Shards' D242): Hex's lease seam over the one run-lease
/// broker its server opens over kiln's credential store (`secrets::lessor`): the broker itself for the server's own
/// runner, and every other runner's asks put to it over that runner's link to its server (`secrets::lessor::Remote`).
/// A runner given none (its server's broker didn't open, or it has no link to ask over) holds no credentials
/// (`hexvm::NO_BROKER`).
#[derive(Clone)]
pub struct Leasing(pub Arc<dyn hex_api::lease::Lessors>);

impl std::fmt::Debug for Leasing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Leasing")
    }
}

/// The machine's microVMs held under kiln's `state` through Hex, the carried `shards` (`shards`) running them
/// (#73 part 7): Hex's image runtime, its boxes' records in `<state>/hex-records`, Shards' home and cache beneath
/// kiln's state ([`shards_home`], `<state>/shards-cache`), its images built there (in `<state>/shards-builds`), each
/// launch's box named by placement's numbering ([`box_name`]), each run's charge kept in `ledger` ([`of_hex`]), and
/// its VMs' credentials leased through `leasing` where it is given (plan step 33; else a launch granting one is
/// refused), and the folders another server serves them asked of this machine's Slates ([`served::SlatesVolumes`]:
/// an isolated agent's volume, served to its microVM with no mount; kiln goal 01a1ec34…). They keep `state` as kiln's,
/// which no isolated agent's project is, holds or lies in
/// ([`Microvms::state`]). Nothing boots until a launch. Blocking.
pub fn held_in(state: &std::path::Path, shards: std::path::PathBuf, ledger: Option<Arc<dyn kiln_seams::Records>>, leasing: Option<Leasing>) -> anyhow::Result<Microvms> {
    let state = std::path::absolute(state)?;
    let paths = shards_api::RuntimePaths::new(state.join("shards-cache"), shards_home(&state)).map_err(|r| anyhow::anyhow!("Shards' home and cache beneath {}: {r:?}", state.display()))?;
    let records = state.join("hex-records");
    let leases = leasing.is_some();
    let runtime = match leasing {
        Some(Leasing(lessors)) => hex_node::images::ImageRuntime::leasing(shards.clone(), paths, &records, lessors),
        None => hex_node::images::ImageRuntime::new(shards.clone(), paths, &records),
    }
    .map_err(|r| anyhow::anyhow!("Hex's image runtime under {}: {r}", state.display()))?;
    // The folders Slates serves its microVMs, each an isolated agent's volume (kiln goal 01a1ec34…, Shards' D286).
    runtime.bind_servers(Arc::new(served::SlatesVolumes)).map_err(|r| anyhow::anyhow!("Hex's image runtime's server of served folders: {r}"))?;
    let images = image::Images::new(shards, shards_home(&state), state.join("shards-builds"));
    let node = Arc::clone(&runtime);
    let hex = hexvm::HexVms::new(runtime, box_name).serving();
    Ok(Microvms { state: Some(state), node: Some(node), ..of_hex(if leases { hex.leasing() } else { hex }, images, ledger) })
}

/// The machine's microVMs as `hex` holds them, booting `images`: each run's charge kept in `ledger` as it ends (the
/// reach node's records, `hexvm::USE`; #73 part 7 slice 3), where there are any.
pub fn of_hex(hex: hexvm::HexVms, images: image::Images, ledger: Option<Arc<dyn kiln_seams::Records>>) -> Microvms {
    let hex = Arc::new(match ledger {
        Some(ledger) => hex.with_ledger(ledger),
        None => hex,
    });
    let sandbox = Arc::clone(&hex) as Arc<dyn Sandbox>;
    let pools = pools::Pools::new(Arc::clone(&sandbox), crate::rt::clock());
    Microvms { sandbox, images, lanes: Some(Arc::clone(&hex) as Arc<dyn Lanes>), hex: Some(hex), shards: None, state: None, pools, node: None }
}

/// The group placement reads run `run`'s use from, launched in `vms`: its VM's meter, where Hex holds it
/// (`reach::place::Group::Hex`, #73 part 7 slice 3); none for another sandbox's run, or one let go of.
pub fn group_of(vms: &Microvms, run: &dyn Run) -> Option<crate::reach::place::Group> {
    let meter = vms.hex.as_ref()?.meter(&run.id()?)?;
    Some(crate::reach::place::Group::Hex(meter))
}

/// The name of a launch's box no start here named: placement's numbering of this process's boxes that reserve nothing
/// (`reach::node::Handle::unreserved_box`, its holding's own where it holds through a keeper); none where placement
/// isn't running.
fn box_name() -> Option<kiln_ci::place::holder::BoxName> {
    crate::reach::node::global().map(crate::reach::node::Handle::unreserved_box)
}

/// The microVMs this process holds, once [`hold`] held them.
static HELD: std::sync::OnceLock<Arc<Microvms>> = std::sync::OnceLock::new();

/// The microVMs this machine holds isolated agents' harnesses in: those [`hold`] held, where this kiln carries their
/// runtime (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 16 and 17); else none.
pub fn microvm() -> Option<Arc<Microvms>> {
    HELD.get().cloned()
}

/// This machine's microVMs, held under kiln's `state` from now on, where this kiln carries Shards: it written out
/// beneath `state` (`crate::shards::write_out`) and held, then [`held_in`], each run's charge kept in `ledger`, their
/// credentials leased through `leasing` where it is given. None where it carries none. The first held is this
/// process's ([`microvm`], its facts' `virt.microvm`).
pub async fn hold(state: &std::path::Path, ledger: Option<Arc<dyn kiln_seams::Records>>, leasing: Option<Leasing>) -> anyhow::Result<Option<Arc<Microvms>>> {
    if let Some(held) = HELD.get() {
        return Ok(Some(Arc::clone(held)));
    }
    let state = state.to_path_buf();
    let vms = crate::rt::blocking(move || -> anyhow::Result<Microvms> {
        let shards = crate::shards::write_out(&state)?;
        let mut vms = held_in(&state, shards.shards.clone(), ledger, leasing)?;
        vms.shards = Some(shards);
        Ok(vms)
    })
    .await??;
    let vms = Arc::new(vms);
    Ok(Some(Arc::clone(HELD.get_or_init(|| vms))))
}

/// Where a runner keeps its microVMs' Shards home (`SHARDS_HOME`: the store their images are built
/// into and booted from): beneath kiln's `state`, which outlives its instances (`upgrade::own_dir`), as
/// its people's volumes are (`harness::isolated::home::root`).
pub fn shards_home(state: &std::path::Path) -> std::path::PathBuf {
    state.join("shards-home")
}

/// Why this machine holds no isolated agent, where it holds none.
pub const NO_MICROVM: &str = "this machine's kiln carries no microVM runtime yet, so it can't hold an isolated agent (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 16 and 17)";

/// The facts a machine says of isolation, for its runner's page and for placing isolated agents
/// (`RunnerFacts::isolation`; `virt.microvm` is its `available`).
pub type Facts = kiln_ci::facts::Isolation;

/// This machine's isolation facts: whether it holds isolated agents, on what, and why not. Blocking: where it holds
/// them with the carried Shards, Shards is asked ([`facts_of`]).
pub fn facts(kvm: bool) -> Facts {
    facts_of(kvm, microvm().as_deref())
}

/// The isolation facts of a machine holding isolated agents in `held`, or in none. Held with the carried Shards
/// ([`hold`]), they're what Shards' own check of this host says now (`shards_api::host_check`, Shards' D262: the check
/// its VM process makes before it boots, made without booting one, by one launch of the carried `shards`), so a runner
/// whose hypervisor can't run microVMs says so in Shards' words, and its engine is `host` (kiln plan 01a1dd2e…, P2.3).
/// A stand-in's ([`Microvms::new`], [`held_in`] alone) carries no `shards` to ask. Held through Hex, they say whether its
/// node lists the class its image VMs run as, as that node says at each probe ([`image_class`], P2.4). Blocking.
pub fn facts_of(kvm: bool, held: Option<&Microvms>) -> Facts {
    let hypervisor = hypervisor(kvm);
    let mut facts = match held.map(|vms| vms.shards.as_ref()) {
        Some(Some(carried)) => checked(hypervisor, shards_api::host_check(Some(carried.shards.as_path()))),
        Some(None) => Facts { available: true, hypervisor, why_not: None, engine: None, image_class: None },
        None => Facts { available: false, hypervisor, why_not: Some(NO_MICROVM.to_owned()), engine: None, image_class: None },
    };
    facts.image_class = held.and_then(|vms| vms.node.as_deref()).map(image_class);
    facts
}

/// Why a node that lists no class for its image VMs gives none (Hex's own listing and its image class disagreeing).
const UNLISTED: &str = "its Hex node lists no `shards-image` class, and its image class says it serves";

/// Whether Hex's image runtime `node` lists the class its image VMs run as (Hex's `shards-image`; kiln plan 01a1dd2e…,
/// P2.2 and P2.4): from its own listing (`Runtime::classes`), and where it lists none, why, in Hex's words (its image
/// class's refusal: Shards' check of this host, held for the runtime's life, or an image boot unconfigured).
fn image_class(node: &hex_node::images::ImageRuntime) -> kiln_ci::facts::NodeImageClass {
    use hex_api::Runtime as _;
    use kiln_ci::facts::NodeImageClass;
    match (node.classes().contains(&hex_api::Class::ShardsImage), node.class().serves()) {
        (true, _) => NodeImageClass::Listed,
        (false, Err(why)) => NodeImageClass::Unlisted { why: why.to_string() },
        (false, Ok(())) => NodeImageClass::Unlisted { why: UNLISTED.to_owned() },
    }
}

/// Why a machine holds no isolated agent where its Shards couldn't say whether this host runs microVMs, before what
/// Shards said.
pub const UNCHECKED: &str = "this machine's Shards couldn't say whether it runs microVMs";

/// The isolation facts of a machine whose Shards answered `answer` to whether this host runs microVMs now: available
/// where it does, its backend the hypervisor; else not, and why in Shards' words, `hypervisor` the one the runner's
/// probe found.
fn checked(hypervisor: Option<String>, answer: Result<shards_api::HostCheck, shards_api::CheckRefusal>) -> Facts {
    match answer {
        Ok(shards_api::HostCheck::Usable(backend)) => Facts { available: true, hypervisor: Some(hypervisor_of(backend).to_owned()), why_not: None, engine: None, image_class: None },
        Ok(shards_api::HostCheck::Unusable { why, .. }) => Facts { available: false, hypervisor, why_not: Some(why.to_string()), engine: None, image_class: None },
        Err(e) => Facts { available: false, hypervisor, why_not: Some(format!("{UNCHECKED}: {e}")), engine: None, image_class: None },
    }
}

/// A Shards backend as the facts name its hypervisor (`RunnerFacts::isolation`'s `hypervisor`).
fn hypervisor_of(backend: shards_api::Backend) -> &'static str {
    match backend {
        shards_api::Backend::Kvm => "KVM",
        shards_api::Backend::HypervisorFramework => "Hypervisor.framework",
        shards_api::Backend::WindowsHypervisorPlatform => "WHP",
    }
}

/// The hypervisor a microVM here would run on, where the runner can use one: KVM on Linux (its
/// `/dev/kvm`, as the runner's probe found it); where the carried Shards says this host runs
/// microVMs, its backend ([`checked`]).
fn hypervisor(kvm: bool) -> Option<String> {
    (cfg!(target_os = "linux") && kvm).then(|| "KVM".to_owned())
}

/// The seam's `Run` over a process kiln started on the machine ([`host::HostSandbox`]).
pub struct ProcRun {
    proc: crate::proc::Duplex,
    reading: futures::lock::Mutex<Reading>,
    killed: std::sync::atomic::AtomicBool,
    ended: std::sync::atomic::AtomicBool,
}

struct Reading {
    out_open: bool,
    err_open: bool,
    exit: Option<oneshot::Receiver<Option<i32>>>,
    given_end: bool,
}

impl ProcRun {
    pub fn new(mut proc: crate::proc::Duplex) -> Self {
        let (_, gone) = oneshot::channel();
        let exit = std::mem::replace(&mut proc.exit, gone);
        Self { proc, reading: futures::lock::Mutex::new(Reading { out_open: true, err_open: true, exit: Some(exit), given_end: false }), killed: false.into(), ended: false.into() }
    }

    fn over(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.ended.load(Ordering::Acquire) || self.killed.load(Ordering::Acquire)
    }
}

/// A process's end as the seam says it: killed through [`Run::kill`] is `Killed`, whatever its
/// status; else its code (-1 for one a signal ended, whose status has none).
fn plain_end(code: Option<i32>, killed: bool) -> Exit {
    if killed {
        return Exit::Killed;
    }
    Exit::Code(code.unwrap_or(-1))
}

impl Run for ProcRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            if self.over() {
                return Err(Refusal::Ended);
            }
            self.proc.stdin.send(bytes).await.map_err(|_| Refusal::Ended)
        })
    }

    fn close_stdin(&self) {
        self.proc.stdin.close();
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            let mut r = self.reading.lock().await;
            if r.given_end {
                return None;
            }
            while r.out_open || r.err_open {
                let (out_open, err_open) = (r.out_open, r.err_open);
                let next = futures::select_biased! {
                    o = read(&self.proc.stdout, out_open).fuse() => (true, o),
                    e = read(&self.proc.stderr, err_open).fuse() => (false, e),
                };
                match next {
                    (true, Some(b)) => return Some(Output::Stdout(b)),
                    (false, Some(b)) => return Some(Output::Stderr(b)),
                    (true, None) => r.out_open = false,
                    (false, None) => r.err_open = false,
                }
            }
            let code = match r.exit.take() {
                Some(exit) => exit.await.ok().flatten(),
                None => None,
            };
            self.ended.store(true, Ordering::Release);
            self.proc.stdin.close();
            r.given_end = true;
            Some(Output::Exited(plain_end(code, self.killed.load(Ordering::Acquire))))
        })
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        if self.over() {
            return Err(Refusal::Ended);
        }
        signal_group(self.proc.pid, signal)
    }

    fn kill(&self) {
        use std::sync::atomic::Ordering;
        if self.ended.load(Ordering::Acquire) || self.killed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.proc.terminate(Duration::ZERO);
    }

    // A process of the machine's: its pid names it, and its stdio are its pipes.
    fn id(&self) -> Option<String> {
        None
    }

    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        Box::pin(async { None })
    }
}

/// `signal` to process group `pgid` (each of the host's runs leads one: `crate::proc::spawn_duplex`).
#[cfg(unix)]
fn signal_group(pgid: u32, signal: Signal) -> Result<(), Refusal> {
    use nix::sys::signal::{Signal as Sig, killpg};
    let pgid = i32::try_from(pgid).ok().filter(|p| *p > 0).ok_or(Refusal::Ended)?;
    let sig = match signal {
        Signal::Interrupt => Sig::SIGINT,
        Signal::Terminate => Sig::SIGTERM,
    };
    killpg(nix::unistd::Pid::from_raw(pgid), sig).map_err(|_| Refusal::Ended)
}

/// Windows has no signals for a job's processes: a run there is only killed.
#[cfg(not(unix))]
fn signal_group(_pgid: u32, _signal: Signal) -> Result<(), Refusal> {
    Err(Refusal::Unsupported("a signal to a process on Windows".into()))
}

/// The next chunk from `rx`, or `None` once it's closed (or was already).
async fn read(rx: &crate::memory::queue::Receiver<Vec<u8>>, open: bool) -> Option<Vec<u8>> {
    if !open {
        return futures::future::pending().await;
    }
    rx.recv().await.ok()
}

#[cfg(test)]
mod tests {
    /// Plan step 33 (D242): a harness switched in place into the microVM its session runs in (P6c.5) is granted its
    /// sign-in as a launch is, or its swap is refused, typed, never left to sign in itself there with its tokens in its
    /// microVM: Hex's microVMs, which grant one only as a box is made (its C8), refuse it until their swap call carries
    /// one; a stand-in's, which grant none, refuse it as their launch does; a swap whose harness has none goes to its
    /// boundary as before. Written, not run (the person's rule of 2026-11-16).
    #[test]
    fn s33_a_harness_swapped_in_is_granted_its_sign_in_or_refused() -> anyhow::Result<()> {
        use kiln_seams::sandbox::Refusal;
        let egress = vec!["api.anthropic.com:443".to_owned(), "platform.claude.com:443".to_owned()];
        let sign_in = crate::secrets::signin::Provider::ClaudeCode.grant("owner", &egress, None).ok_or_else(|| anyhow::anyhow!("no sign-in granted toward its hosts"))?;
        let one = [sign_in];
        anyhow::ensure!(matches!(super::swap_refused(true, &one), Some(Refusal::Unsupported(why)) if why == super::hexvm::NO_SWAP_SIGN_IN), "Hex's microVMs");
        anyhow::ensure!(matches!(super::swap_refused(false, &one), Some(Refusal::Unsupported(why)) if why.contains("sign-in")), "a stand-in's");
        anyhow::ensure!(super::swap_refused(true, &[]).is_none() && super::swap_refused(false, &[]).is_none(), "one with no sign-in goes to its boundary");
        Ok(())
    }

    /// Step 17: a machine whose kiln carries no microVM runtime says so in its facts, and why (one
    /// answer, which `virt.microvm` says too); its hypervisor is the one the runner found.
    #[test]
    fn s17_a_machine_without_microvms_says_why_in_its_facts() {
        let with_kvm = super::facts(true);
        assert!(!with_kvm.available);
        assert_eq!(with_kvm.why_not.as_deref(), Some(super::NO_MICROVM));
        assert_eq!(with_kvm.hypervisor.as_deref(), cfg!(target_os = "linux").then_some("KVM"));
        assert_eq!(super::facts(false).hypervisor, None);
    }

    /// Kiln plan 01a1dd2e…, P2.3 AC2: a machine holding microVMs with the carried Shards says what Shards' own check of
    /// its host says (`shards_api::host_check`): available where it runs them, its backend the hypervisor; else not,
    /// in Shards' words (`/dev/kvm` named where it's missing or this user can't open it, an Intel Mac's platform
    /// named), and its engine is `host` with that why; not available either where Shards couldn't say, with its words.
    #[test]
    fn p2_3_a_runner_says_what_shards_check_of_its_host_says() {
        use crate::harness::engine::{Asked, capable, held};
        use kiln_ci::facts::{Engine, RunnerFacts};
        use shards_api::{Backend, CheckRefusal, HostCheck, Unusable};
        let usable = super::checked(None, Ok(HostCheck::Usable(Backend::HypervisorFramework)));
        assert_eq!((usable.available, usable.hypervisor.as_deref(), usable.why_not.as_deref()), (true, Some("Hypervisor.framework"), None));
        for (why, says) in [(Unusable::KvmDenied, "cannot open /dev/kvm: permission denied"), (Unusable::KvmMissing, "/dev/kvm does not exist")] {
            let f = super::checked(None, Ok(HostCheck::Unusable { backend: Some(Backend::Kvm), why }));
            assert!(!f.available && f.hypervisor.is_none() && f.why_not.as_deref().is_some_and(|w| w.contains(says)), "{f:?}");
            let runner = RunnerFacts { isolation: Some(f.clone()), ..RunnerFacts::default() };
            let h = held(Asked::default(), &capable(&runner));
            assert_eq!((h.engine, h.why.as_deref()), (Engine::Host, f.why_not.as_deref()));
        }
        let intel = super::checked(None, Ok(HostCheck::Unusable { backend: None, why: Unusable::Unsupported { os: "macos".into(), arch: "x86_64".into() } }));
        assert!(!intel.available && intel.why_not.as_deref().is_some_and(|w| w.contains("macos/x86_64")), "{intel:?}");
        let unasked = super::checked(Some("KVM".into()), Err(CheckRefusal::Ended { status: Some(3), said: "shards: no such command: api-check".into() }));
        assert!(!unasked.available && unasked.hypervisor.as_deref() == Some("KVM"), "{unasked:?}");
        assert!(unasked.why_not.as_deref().is_some_and(|w| w.starts_with(super::UNCHECKED) && w.contains("no such command")), "{unasked:?}");
    }

    /// Kiln plan 01a1dd2e…, P2.4 (on Hex's P2.2): a machine holding its microVMs through Hex says whether its node lists
    /// the class its image VMs run as, as its node says it, and why not in Hex's words. Do read the facts of microVMs held
    /// through Hex whose `shards` can't be asked (none is here, so Shards' check of this host never answers), of a
    /// stand-in's, and of none. Expect the first to say its node lists no `shards-image` class, with Hex's why; the
    /// others to say nothing of a node. Written, not run (the person's rule of 2026-11-16).
    #[test]
    fn p2_4_a_runner_says_whether_its_hex_node_lists_the_image_class() -> anyhow::Result<()> {
        use kiln_ci::facts::NodeImageClass;
        let state = tempfile::tempdir()?;
        let hex = super::held_in(state.path(), state.path().join("shards"), None, None)?;
        let facts = super::facts_of(false, Some(&hex));
        anyhow::ensure!(matches!(&facts.image_class, Some(NodeImageClass::Unlisted { why }) if !why.is_empty()), "{facts:?}");
        let images = super::image::Images::new(state.path().join("shards"), state.path().join("home"), state.path().join("builds"));
        let stand_in = super::Microvms::new(std::sync::Arc::new(kiln_seams::sandbox::SimSandbox::new()), images);
        anyhow::ensure!(super::facts_of(false, Some(&stand_in)).image_class.is_none(), "a stand-in's");
        anyhow::ensure!(super::facts_of(false, None).image_class.is_none(), "none held");
        Ok(())
    }

    /// #75 step 3 (Shards D254): a machine's microVMs say whether their runs carry kiln's MCP lane, typed: those held
    /// with none say so; Hex's carry it; a lane that carries it is theirs.
    #[test]
    fn s35_mcp_microvms_say_whether_their_runs_carry_kilns_mcp_lane() -> anyhow::Result<()> {
        use kiln_seams::sandbox::Refusal;
        let state = tempfile::tempdir()?;
        let images = || super::image::Images::new(state.path().join("shards"), state.path().join("home"), state.path().join("builds"));
        let none = super::Microvms::new(std::sync::Arc::new(kiln_seams::sandbox::SimSandbox::new()), images());
        anyhow::ensure!(none.mcp().err() == Some(Refusal::Unsupported(super::NO_LANES.into())), "no lane");
        let hex = super::held_in(state.path(), state.path().join("shards"), None, None)?;
        anyhow::ensure!(hex.mcp().is_ok(), "Hex's carries it");
        let carried = super::Microvms::new(std::sync::Arc::new(kiln_seams::sandbox::SimSandbox::new()), images()).with_lanes(std::sync::Arc::new(super::fake::FakeLanes::default()));
        anyhow::ensure!(carried.mcp().is_ok(), "a lane that carries it");
        Ok(())
    }

    /// #73 part 7 slice 2 (step 17): a runner whose kiln carries Shards holds its isolated agents' harnesses in Hex's
    /// microVMs, the sandbox `HexVms` over Hex's image runtime (its records, Shards' home and cache beneath kiln's
    /// state), and its facts say it holds them (`virt.microvm` is their `available`). They keep kiln's state, which no
    /// isolated agent's project is, holds or lies in (CP-T4.1's AC-T4.1.5). No VM boots.
    #[test]
    fn s17_p7_a_carrying_runner_holds_harnesses_in_hex_and_says_so() -> anyhow::Result<()> {
        let state = tempfile::tempdir()?;
        let vms = super::held_in(state.path(), state.path().join("shards"), None, None)?;
        anyhow::ensure!(vms.hex.is_some(), "its sandbox isn't Hex's");
        anyhow::ensure!(vms.state() == Some(state.path()), "kiln's state isn't kept: {:?}", vms.state());
        // It holds folders Slates serves its microVMs: its runtime is bound this machine's Slates (`served::SlatesVolumes`).
        anyhow::ensure!(vms.sandbox.boundaries() == vec!["microvm"] && vms.sandbox.holds() == kiln_seams::sandbox::Holds { served: true, ..super::hexvm::HOLDS }, "its sandbox isn't HexVms");
        anyhow::ensure!(state.path().join("hex-records").join("state").exists(), "Hex keeps no records beneath kiln's state");
        let facts = super::facts_of(true, Some(&vms));
        anyhow::ensure!(facts.available && facts.why_not.is_none(), "{facts:?}");
        crate::rt::block_on(async move {
            let (sink, _updates) = async_channel::unbounded::<(String, crate::harness::core::Update)>();
            let host = crate::harness::host::Host::new(sink, None, crate::harness::host::Limits::default());
            host.hold_isolated_in(vms);
            anyhow::ensure!(host.microvm().is_some_and(|m| m.hex.is_some()), "its harness host holds them elsewhere");
            Ok(())
        })
    }

    /// #74 credentials (plan step 33): a runner given the broker's lessors holds its microVMs through Hex's image
    /// runtime given them (`ImageRuntime::leasing`), and says it holds credentials; one given none says it doesn't.
    /// No VM boots.
    #[test]
    fn s33_a_runner_given_lessors_holds_credentials_and_one_without_doesnt() -> anyhow::Result<()> {
        use std::sync::Arc;

        struct Refusing;
        impl hex_api::lease::Lessors for Refusing {
            fn lessor(&self, _: &hex_api::BoxId) -> Result<Arc<dyn hex_api::lease::Lessor>, hex_api::Refusal> {
                Err(hex_api::Refusal::Broken("no run boots here".into()))
            }
        }
        let state = tempfile::tempdir()?;
        let leasing = super::Leasing(Arc::new(Refusing));
        let vms = super::held_in(state.path(), state.path().join("shards"), None, Some(leasing))?;
        anyhow::ensure!(vms.sandbox.holds() == kiln_seams::sandbox::Holds { credentials: true, served: true, ..super::hexvm::HOLDS }, "{:?}", vms.sandbox.holds());
        let other = tempfile::tempdir()?;
        let vms = super::held_in(other.path(), other.path().join("shards"), None, None)?;
        anyhow::ensure!(!vms.sandbox.holds().credentials, "a runner with no broker says it holds credentials");
        Ok(())
    }

    /// #73 part 7 slices 2 and 3: a run launched in the box placement holds for it is grouped for placement by its
    /// VM's meter (`Group::Hex`), and its charge is kept in the ledger the machine's microVMs were given (the reach
    /// node's records) as it ends. Over Hex's simulator: no VM boots.
    #[test]
    fn s17_p7_a_started_runs_group_is_hexs_meter_and_its_charge_is_kept() -> anyhow::Result<()> {
        use std::sync::Arc;

        use kiln_ci::place::holder::BoxName;
        use kiln_seams::Records as _;
        use kiln_seams::sandbox::{Boundary, Egress, Launch, Limits, Refusal};
        use super::hexvm::tests::Metered;
        crate::rt::block_on(async {
            let state = tempfile::tempdir()?;
            let rt = Arc::new(Metered::default());
            let records = Arc::new(kiln_seams::records::SimRecords::new());
            let hex = super::hexvm::HexVms::new(Arc::<Metered>::clone(&rt), || None);
            let images = super::image::Images::new(state.path().join("shards"), state.path().join("home"), state.path().join("builds"));
            let vms = super::of_hex(hex, images, Some(Arc::<kiln_seams::records::SimRecords>::clone(&records) as Arc<dyn kiln_seams::Records>));
            let launch = Launch { limits: Limits::NONE, egress: Egress::Only(Vec::new()), folder: Some("/work/project".into()), boundary: Boundary::MicroVm { image: hex_api::sim::SIM_IMAGE.to_owned() }, ..Launch::of("claude", &[]) };
            let reserved = BoxName::Reserved(hex_api::capacity::ReservationId { epoch: 3, seq: 9 });
            let run = vms.launch(Some(reserved), launch.clone(), None, &[]).await?;
            anyhow::ensure!(run.id().as_deref() == Some("kiln-r3-9"), "its box isn't placement's: {:?}", run.id());
            let group = super::group_of(&vms, run.as_ref());
            anyhow::ensure!(matches!(&group, Some(crate::reach::place::Group::Hex(m)) if format!("{m:?}") == "Meter(\"kiln-r3-9\")"), "{group:?}");
            run.kill();
            while run.next().await.is_some() {}
            let kept = records.get(super::hexvm::USE, b"kiln-r3-9", kiln_seams::records::Consistency::Linearizable).await?;
            anyhow::ensure!(kept.is_some(), "its charge wasn't kept in the ledger it was given");
            let unnamed = vms.launch(None, launch, None, &[]).await.err();
            anyhow::ensure!(matches!(&unnamed, Some(Refusal::Unsupported(why)) if why == super::hexvm::UNNAMED), "no placement here names its box: {unnamed:?}");
            Ok(())
        })
    }
}
