//! Where kiln's harnesses run (docs/isolation.md; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17;
//! CP-T4.1): the `Sandbox` seam's implementations in kiln (`kiln_seams::sandbox`), and an isolated
//! agent's launch through them.
//!
//! - [`host`]: today's launch, a process on the machine. It holds none of the seam's limits, and
//!   refuses them typed.
//! - The machine's microVMs ([`microvm`]): an isolated agent's harness runs as a member of its
//!   Agentfile's image in a microVM of its own (Shards' D231), through Hex; its stdio are the
//!   member's streams. This kiln carries their runtime once step 16's Shards and Hex are wired in;
//!   until then a machine holds no isolated agent, and refuses one rather than run it as a process.
//! - [`isolated`]: an isolated session's launch, its image, and what it can't do yet.
//! - [`narrow`]: what its microVM reaches past it, of what its image grants: its session's own
//!   provider's or sign-in's destinations alone, else its image's whole and why.
//! - [`image`]: what its microVM is made from: its Agentfile's image, built by the carried `shards`
//!   once for each content, pinned by its digest.
//! - [`held`]: a boundary's run as the `Duplex` a session drives its harness through, so a session's
//!   loop and its adapter treat a microVM's member as they treat a process of the machine's.

#[cfg(test)]
pub mod fake;
pub mod held;
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
}

/// The microVMs this machine holds isolated agents' harnesses in: none until this kiln carries their
/// runtime (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16's Shards and Hex, wired here by step
/// 17's next slice).
pub fn microvm() -> Option<Arc<Microvms>> {
    None
}

/// Why this machine holds no isolated agent, where it holds none.
pub const NO_MICROVM: &str = "this machine's kiln carries no microVM runtime yet, so it can't hold an isolated agent (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 16 and 17)";

/// The facts a machine says of isolation, for its runner's page and for placing isolated agents
/// (`RunnerFacts::isolation`; `virt.microvm` is its `available`).
pub type Facts = kiln_ci::facts::Isolation;

/// This machine's isolation facts: whether it holds isolated agents, on what, and why not.
pub fn facts(kvm: bool) -> Facts {
    let hypervisor = hypervisor(kvm);
    match microvm() {
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
}
