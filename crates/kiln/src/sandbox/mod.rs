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

#[cfg(test)]
pub mod fake;
pub mod held;
pub mod hexvm;
pub mod host;
pub mod image;
pub mod isolated;
pub mod narrow;

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
}

impl Microvms {
    /// Runs in `sandbox`, booting `images`: a stand-in's, launched as the seam launches.
    pub fn new(sandbox: Arc<dyn Sandbox>, images: image::Images) -> Self {
        Self { sandbox, images, hex: None, shards: None }
    }

    /// `launch` in a microVM of its own: in box `name` where placement named one for its run (its reservation's,
    /// `Holder::box_of`) and its sandbox is Hex's; else named as its sandbox names one (placement's numbering).
    pub async fn launch(&self, name: Option<kiln_ci::place::holder::BoxName>, launch: kiln_seams::sandbox::Launch) -> Result<Box<dyn Run>, Refusal> {
        match (&self.hex, name) {
            (Some(hex), Some(name)) => hex.launch_as(name, launch).await,
            _ => self.sandbox.launch(launch).await,
        }
    }
}

/// What leases a machine's microVMs' credentials (plan step 33, Shards' D242): Hex's lease seam, kiln's
/// `secrets::lessor::Lessors` over the run-lease broker its server opens over kiln's credential store. Only the
/// server's own runner holds one: a runner elsewhere reaches no store, so its microVMs hold no credentials
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
/// refused). Nothing boots until a launch. Blocking.
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
    let images = image::Images::new(shards, shards_home(&state), state.join("shards-builds"));
    let hex = hexvm::HexVms::new(runtime, box_name);
    Ok(of_hex(if leases { hex.leasing() } else { hex }, images, ledger))
}

/// The machine's microVMs as `hex` holds them, booting `images`: each run's charge kept in `ledger` as it ends (the
/// reach node's records, `hexvm::USE`; #73 part 7 slice 3), where there are any.
pub fn of_hex(hex: hexvm::HexVms, images: image::Images, ledger: Option<Arc<dyn kiln_seams::Records>>) -> Microvms {
    let hex = Arc::new(match ledger {
        Some(ledger) => hex.with_ledger(ledger),
        None => hex,
    });
    Microvms { sandbox: Arc::clone(&hex) as Arc<dyn Sandbox>, images, hex: Some(hex), shards: None }
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
    if !crate::shards::carried() {
        return Ok(None);
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

/// This machine's isolation facts: whether it holds isolated agents, on what, and why not.
pub fn facts(kvm: bool) -> Facts {
    facts_of(kvm, microvm().as_deref())
}

/// The isolation facts of a machine holding isolated agents in `held`, or in none.
pub fn facts_of(kvm: bool, held: Option<&Microvms>) -> Facts {
    let hypervisor = hypervisor(kvm);
    match held {
        Some(_) => Facts { available: true, hypervisor, why_not: None },
        None => Facts { available: false, hypervisor, why_not: Some(NO_MICROVM.to_owned()) },
    }
}

/// The hypervisor a microVM here would run on, where the runner can use one: KVM on Linux (its
/// `/dev/kvm`, as the runner's probe found it). macOS's and Windows' are what the microVMs'
/// runtime reports once it's carried (Shards' backend: Hypervisor.framework, WHP).
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

    /// #73 part 7 slice 2 (step 17): a runner whose kiln carries Shards holds its isolated agents' harnesses in Hex's
    /// microVMs, the sandbox `HexVms` over Hex's image runtime (its records, Shards' home and cache beneath kiln's
    /// state), and its facts say it holds them (`virt.microvm` is their `available`). No VM boots.
    #[test]
    fn s17_p7_a_carrying_runner_holds_harnesses_in_hex_and_says_so() -> anyhow::Result<()> {
        let state = tempfile::tempdir()?;
        let vms = super::held_in(state.path(), state.path().join("shards"), None, None)?;
        anyhow::ensure!(vms.hex.is_some(), "its sandbox isn't Hex's");
        anyhow::ensure!(vms.sandbox.boundaries() == vec!["microvm"] && vms.sandbox.holds() == super::hexvm::HOLDS, "its sandbox isn't HexVms");
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
        anyhow::ensure!(vms.sandbox.holds() == kiln_seams::sandbox::Holds { credentials: true, ..super::hexvm::HOLDS }, "{:?}", vms.sandbox.holds());
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
            let run = vms.launch(Some(reserved), launch.clone()).await?;
            anyhow::ensure!(run.id().as_deref() == Some("kiln-r3-9"), "its box isn't placement's: {:?}", run.id());
            let group = super::group_of(&vms, run.as_ref());
            anyhow::ensure!(matches!(&group, Some(crate::reach::place::Group::Hex(m)) if format!("{m:?}") == "Meter(\"kiln-r3-9\")"), "{group:?}");
            run.kill();
            while run.next().await.is_some() {}
            let kept = records.get(super::hexvm::USE, b"kiln-r3-9", kiln_seams::records::Consistency::Linearizable).await?;
            anyhow::ensure!(kept.is_some(), "its charge wasn't kept in the ledger it was given");
            let unnamed = vms.launch(None, launch).await.err();
            anyhow::ensure!(matches!(&unnamed, Some(Refusal::Unsupported(why)) if why == super::hexvm::UNNAMED), "no placement here names its box: {unnamed:?}");
            Ok(())
        })
    }
}
