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
//!   delete its box (`Runtime::delete_with_host_use`), once: a delete answering `NoSuchBox` found it ended.
//! - **What it refuses, typed:** egress to whatever its image grants (`Egress::Any`): Hex's grants are default
//!   deny, so a run names its destinations; limits and credentials it doesn't hold ([`HOLDS`]); and what Hex
//!   refuses, in Hex's words (`Awaits` as `Unsupported`).

use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::channel::oneshot;
use futures::future::BoxFuture;
use hex_api::{Allocation, BoxId, BoxSpec, CapacityResources, ClassSpec, Destination, Grant, ImageMember, Member, MemberKind, Mount, Ports, Proc, Proto, Runtime, RuntimeCost, ShardsImage};
use kiln_ci::place::holder::BoxName;
use kiln_seams::sandbox::{Boundary, Egress, Exit, Holds, Launch, Link, Output, Refusal, Run, Sandbox, Signal};

/// What a box of Hex's holds its run to: no egress, egress to named destinations (each by TCP, to its harness
/// member), and folders shared at any path, read-only or not (`ShardsImage::shares`). Not memory or process limits
/// (Hex reads no OOM kill or refused fork of an image VM's guest: its `usage_of`), weights, signals (Hex awaits
/// `MemberSignal`, Shards' agent-control endpoint: they're asked of Hex all the same, so a Hex that delivers them
/// needs only this flag), or credentials (not mapped yet).
pub const HOLDS: Holds = Holds { memory: false, processes: false, weights: false, no_egress: true, egress_by_host: true, shares: true, signals: false, credentials: false };

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
}

/// Why a launch with no box named is refused where no placement numbers one.
pub const UNNAMED: &str = "a microVM whose box no placement here names: kiln's placement isn't running on this machine";

impl HexVms {
    /// Over Hex's runtime `runtime`, naming each launch's box with `names`, which answers none where nothing numbers
    /// one (a launch then refused).
    pub fn new(runtime: Arc<dyn Runtime>, names: impl Fn() -> Option<BoxName> + Send + Sync + 'static) -> Self {
        Self { runtime, names: Box::new(names) }
    }

    /// `launch` in a box named `name`: created, started and its harness member's streams taken, or refused, typed,
    /// with no box left behind.
    pub async fn launch_as(&self, name: BoxName, launch: Launch) -> Result<Box<dyn Run>, Refusal> {
        HOLDS.check(&launch)?;
        let Boundary::MicroVm { image } = &launch.boundary else { return Err(Refusal::Unsupported("a run outside a microVM".into())) };
        let member = Member { kind: MemberKind::Harness, name: launch.program.clone() };
        let grants = grants_of(&member, &launch.egress)?;
        let mut shares: Vec<Mount> = launch.shares.iter().map(|s| Mount { source: PathBuf::from(&s.host), destination: s.at.clone(), read_only: !s.writable }).collect();
        if let Some(folder) = &launch.folder
            && !shares.iter().any(|s| &s.destination == folder)
        {
            shares.push(Mount { source: PathBuf::from(folder), destination: folder.clone(), read_only: false });
        }
        let driven = ImageMember { member: member.clone(), env: launch.env.clone(), workdir: launch.folder.clone(), args: launch.args.clone() };
        let class = ShardsImage { grants, members: vec![driven], shares, ..ShardsImage::new(image) };
        let id = name.id().map_err(seam)?;
        let spec = BoxSpec { id: id.try_clone().map_err(seam)?, bounds: hex_api::Bounds::default(), class: ClassSpec::ShardsImage(Box::new(class)), owner: None, devices: Vec::new(), allocation: Some(allocation()?), route: None, band: 0 };
        let (runtime, at) = (Arc::clone(&self.runtime), id.try_clone().map_err(seam)?);
        let made = crate::rt::blocking(move || boot(&*runtime, spec, &at, &member)).await.map_err(|e| Refusal::Invalid { what: "its microVM", why: format!("{e:#}") })??;
        HexRun::start(Arc::clone(&self.runtime), id, made)
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
        HOLDS
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

/// Creates `spec`'s box, starts it and takes `member`'s run and streams: or deletes what it made and says why.
fn boot(runtime: &dyn Runtime, spec: BoxSpec, id: &BoxId, member: &Member) -> Result<(Box<dyn Proc>, hex_api::Given), Refusal> {
    runtime.create(spec).map_err(seam)?;
    // A start answering `Lifecycle { Pending }` (its boot past Hex's learned patience, decision 9) is refused with
    // the rest: Hex's `Runtime` has no call that waits on a booting box (`member` and `inspect` answer at once), and
    // kiln asks again on no clock of its own (R03). Its delete ends the boot (Hex's change (b), in its push after
    // c7603b0b, which kiln vendors: c7603b0b may still answer that delete `Pending`, said as it is).
    let taken = runtime.start(id).and_then(|()| runtime.member(id, member)).and_then(|mut p| {
        let given = p.streams()?;
        Ok((p, given))
    });
    taken.map_err(|why| {
        if let Err(e) = delete(runtime, id) {
            tracing::warn!(vm = %id, error = %e, "a microVM whose launch failed wasn't deleted");
        }
        seam(why)
    })
}

/// Deletes box `id`, once, as kill, drop and a run's end do: gone where Hex deleted it, or found it gone
/// (`NoSuchBox`: another delete ended it). What its VM cost its host over its run is said at debug.
fn delete(runtime: &dyn Runtime, id: &BoxId) -> Result<(), hex_api::Refusal> {
    match runtime.delete_with_host_use(id) {
        Ok(used) => {
            if let Some(u) = used {
                tracing::debug!(vm = %id, cpu_ns = u.cpu_ns, footprint_bytes = u.footprint_bytes, window_ms = u.window.as_millis(), "its VM's cost over its run");
            }
            Ok(())
        }
        Err(hex_api::Refusal::NoSuchBox(_)) => Ok(()),
        Err(e) => Err(e),
    }
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

/// Where its box's delete stands: none asked, under way (its answer to come), or done.
enum Life {
    Running,
    Deleting(oneshot::Receiver<()>),
    Deleted,
}

/// A harness member's run in its box.
struct HexRun {
    /// Hex's runtime, as Hex hands it out ([`HexVms::runtime`]).
    runtime: Arc<dyn Runtime>,
    id: BoxId,
    name: String,
    stdin: crate::memory::queue::Sender<Vec<u8>>,
    said: crate::memory::queue::Receiver<Said>,
    reading: futures::lock::Mutex<()>,
    life: parking_lot::Mutex<Life>,
    killed: AtomicBool,
    given_end: AtomicBool,
}

impl HexRun {
    /// The run of the member [`boot`] took in box `id`: a thread writes its stdin, one reads each of its stdout and
    /// stderr, and one waits for its end, then closes its stdin and, all it wrote read, says its end. Its box is
    /// deleted where a thread can't start.
    fn start(runtime: Arc<dyn Runtime>, id: BoxId, (proc, given): (Box<dyn Proc>, hex_api::Given)) -> Result<Box<dyn Run>, Refusal> {
        let failed = |why: String| {
            if let Err(e) = delete(&*runtime, &id) {
                tracing::warn!(vm = %id, error = %e, "a microVM whose run couldn't start wasn't deleted");
            }
            Refusal::Invalid { what: "its microVM's streams", why }
        };
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
        let name = id.to_string();
        Ok(Box::new(HexRun { runtime, id, name, stdin: in_tx, said, reading: futures::lock::Mutex::new(()), life: parking_lot::Mutex::new(Life::Running), killed: AtomicBool::new(false), given_end: AtomicBool::new(false) }))
    }

    fn over(&self) -> bool {
        self.killed.load(Ordering::Acquire) || self.given_end.load(Ordering::Acquire)
    }

    /// Deletes its box on the blocking pool, unless a delete was asked already: kill's and drop's.
    fn delete_soon(&self) {
        let mut life = self.life.lock();
        if !matches!(*life, Life::Running) {
            return;
        }
        let id = match self.id.try_clone() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(vm = %self.name, error = %e, "a microVM wasn't deleted");
                return;
            }
        };
        let (done, wait) = oneshot::channel();
        *life = Life::Deleting(wait);
        let runtime = Arc::clone(&self.runtime);
        drop(crate::rt::spawn(async move {
            let deleted = crate::rt::blocking(move || delete(&*runtime, &id).map_err(|e| (id, e))).await;
            match deleted {
                Ok(Ok(())) => {}
                Ok(Err((id, e))) => tracing::warn!(vm = %id, error = %e, "a microVM wasn't deleted"),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "a microVM's delete didn't run"),
            }
            let _ = done.send(());
        }));
    }

    /// Its box gone: deleted here, or the delete under way answered.
    async fn gone(&self) {
        let life = std::mem::replace(&mut *self.life.lock(), Life::Deleted);
        match life {
            Life::Running => {
                let (runtime, id) = (Arc::clone(&self.runtime), self.id.try_clone());
                let deleted = match id {
                    Ok(id) => crate::rt::blocking(move || delete(&*runtime, &id).map_err(|e| e.to_string())).await.unwrap_or_else(|e| Err(format!("{e:#}"))),
                    Err(e) => Err(e.to_string()),
                };
                if let Err(e) = deleted {
                    tracing::warn!(vm = %self.name, error = %e, "a microVM whose run ended wasn't deleted");
                }
            }
            Life::Deleting(wait) => {
                let _ = wait.await;
            }
            Life::Deleted => {}
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
        self.runtime.signal(&self.id, to).map_err(seam)
    }

    fn kill(&self) {
        if self.given_end.load(Ordering::Acquire) {
            return;
        }
        self.killed.store(true, Ordering::Release);
        self.delete_soon();
    }

    fn id(&self) -> Option<String> {
        Some(self.name.clone())
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
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hex_api::sim::{SIM_IMAGE, SimRuntime};
    use hex_api::{BoxId, Destination, Grant, Member, MemberKind, Ports, Proto, Runtime as _};
    use kiln_ci::place::holder::BoxName;
    use kiln_seams::sandbox::{Boundary, Egress, Exit, Launch, Limits, Output, Refusal, Run, Sandbox as _, Signal};

    use super::HexVms;

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
}
