//! [`SimRuntime`]: boxes in memory, running built-in programs that do what the conformance suite
//! asks for (say words, fail, wait, use memory, start processes, echo stdin), with the bounds
//! enforced as the kernel would: past `memory_max` a program is killed and the kill counted; past
//! `pids_max` a fork is refused and counted. An `oci` box has an init, made at create and waiting
//! until start. From a [`ClassSpec::Oci`] it runs the spec's init program, so `fail N` ends it with N
//! at start and `say` with 0; any other init, and a bundle's, runs until it is killed or the box is
//! deleted: as the init of its PID namespace with no handler, it takes no other signal (unless made
//! [`SimRuntime::with_term_ending_init`], for a class that ends its init on TERM). Once the init has ended the box is empty and inspect reports how it ended, as the node
//! agent does for a container whose init it reaped. A box a run made ([`Runtime::run`]) goes once its
//! program has ended, ending what else ran in it, as the node agent removes it (decision 7).
//!
//! A Shards image VM ([`ClassSpec::ShardsImage`]) is a box whose init is its run's command, as the
//! image class's own cases ([`crate::conformance::IMAGE_CASES`]) hold it: created without a VM, booted
//! by start, ended by KILL, its members unreached (their exec and signals refused as awaiting Shards'
//! agent-control endpoint), and its guest time per vCPU read while it runs. The simulated VM runs every
//! vCPU the whole time it runs: the most guest time a real one can report, so a consumer's bound holds
//! against it.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::conformance::{Act, Fixture};
use crate::{
    Allocation, Bounds, BoxId, BoxInfo, BoxSpec, BoxState, CapacityResources, Class, ClassSpec,
    Destination, Exit, Grant, GuestTime, Network, OciSpec, Proc, Program, Refusal, Rootfs, Runtime,
    RuntimeCost, ShardsImage, Signal, Stdio, Upstream, Usage,
};

/// Most boxes a [`SimRuntime`] holds; past it `create` is refused with [`Refusal::Full`].
pub const SIM_BOXES: usize = 10_000;
/// The simulator's exec bound: Linux's floor, `ARG_MAX`, 131,072 bytes
/// (`include/uapi/linux/limits.h:8`, v6.19), the least any Linux's execve takes whatever its stack limit, so a
/// program the simulator takes runs on every Linux (docs/research/runtime/exec-size-bounds.md).
pub const SIM_EXEC_LIMIT: crate::ExecLimit = crate::ExecLimit {
    block: 131_072,
    counts: crate::BlockCount::Execve,
    string: None,
    unit: crate::StringUnit::Bytes,
    source: "the simulator (Linux's ARG_MAX floor, include/uapi/linux/limits.h:8)",
};
/// Most programs a simulated box holds, running or ended; past it `exec` is refused.
pub const SIM_PROCS: usize = 1_000;
/// Most bytes the `echo` program copies; past it, it stops and exits 1.
pub const SIM_ECHO_BYTES: u64 = 1 << 20;

/// A runtime in memory, of both classes.
#[derive(Default)]
pub struct SimRuntime {
    boxes: Mutex<BTreeMap<BoxId, SimBox>>,
    /// Whether TERM ends an `oci` box's init, as [`Fixture::term_ends_init`] says of the class it
    /// stands for.
    term_ends_init: bool,
    /// A specific initialized endpoint, when exercising routing between distinct backends.
    backend: Option<Class>,
}

struct SimBox {
    spec: BoxSpec,
    usage: Usage,
    procs: Vec<Arc<SimProc>>,
    /// An `oci` box's init.
    init: Option<Arc<SimProc>>,
    /// Whether its init has been started.
    started: bool,
    /// How its init ends once started, from its program; `None` runs until signalled.
    init_ends: Option<Exit>,
    /// The program a [`Runtime::run`] made the box for: once it has ended the box is gone.
    run: Option<Arc<SimProc>>,
    /// A Shards image box's VM: its vCPUs and when it booted.
    vm: Option<SimVm>,
}

/// A simulated Shards image VM: its vCPUs, from its allocation, and when its start booted it.
struct SimVm {
    vcpus: u32,
    booted: Option<Instant>,
}

#[derive(Default)]
struct SimProc {
    exit: Mutex<Option<Exit>>,
    ended: Condvar,
    /// A run's program: the other programs of its box, which its end ends, as the node agent's removal
    /// of the run's box does.
    left: Mutex<Vec<Arc<SimProc>>>,
}

impl SimProc {
    /// Ends it with `exit`, and, a run's program, what else runs in its box.
    fn end(&self, exit: Exit) -> Result<(), Refusal> {
        self.set(exit)?;
        let left = std::mem::take(&mut *self.left.lock().map_err(poisoned)?);
        for p in left {
            p.set(Exit::Signal(Signal::KILL.number()))?;
        }
        Ok(())
    }

    /// Ends it alone with `exit`, unless it has ended.
    fn set(&self, exit: Exit) -> Result<(), Refusal> {
        let mut e = self.exit.lock().map_err(poisoned)?;
        if e.is_none() {
            *e = Some(exit);
        }
        self.ended.notify_all();
        Ok(())
    }

    fn exit(&self) -> Result<Option<Exit>, Refusal> {
        Ok(*self.exit.lock().map_err(poisoned)?)
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a simulator lock was poisoned".to_owned())
}

impl SimRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// The same simulated implementation restricted to the initialized backend the caller names.
    pub fn for_backend(class: Class) -> Self {
        Self {
            backend: Some(class),
            ..Self::default()
        }
    }

    /// A simulator whose `oci` inits end on TERM when `ends`, as the class whose oracle it is does.
    pub fn with_term_ending_init(ends: bool) -> Self {
        Self {
            term_ends_init: ends,
            ..Self::default()
        }
    }

    /// The boxes, less each run's whose program has ended: removed before any call can see it, as the
    /// node agent removes it before the run's end is reported, what else ran in it ended.
    fn boxes(&self) -> Result<MutexGuard<'_, BTreeMap<BoxId, SimBox>>, Refusal> {
        let mut boxes = self.boxes.lock().map_err(poisoned)?;
        let mut ended = Vec::new();
        for (id, b) in boxes.iter() {
            if let Some(run) = &b.run
                && run.exit()?.is_some()
            {
                ended.push(id.clone());
            }
        }
        for id in ended {
            if let Some(b) = boxes.remove(&id) {
                for p in &b.procs {
                    p.set(Exit::Signal(Signal::KILL.number()))?;
                }
            }
        }
        Ok(boxes)
    }

    fn info(b: &SimBox) -> Result<BoxInfo, Refusal> {
        let mut running = false;
        for p in b.procs.iter().chain(&b.init) {
            running |= p.exit()?.is_none();
        }
        let waiting = match &b.init {
            Some(init) if !b.started => init.exit()?.is_none(),
            _ => false,
        };
        // An oci box is its init: once that has ended the box is empty, whatever else still runs.
        let exit = match &b.init {
            Some(init) => init.exit()?,
            None => None,
        };
        Ok(BoxInfo {
            published: Vec::new(),
            grants: match &b.spec.class {
                ClassSpec::ShardsImage(image) => image.grants.clone(),
                _ => Vec::new(),
            },
            membership: None,
            id: b.spec.id.clone(),
            class: b.spec.class.class(),
            state: if waiting {
                BoxState::Created
            } else if exit.is_some() {
                BoxState::Empty
            } else if running {
                BoxState::Running
            } else {
                BoxState::Empty
            },
            bounds: b.spec.bounds.clone(),
            allocation: b.spec.allocation,
            route: b.spec.route,
            adopted: false,
            exit,
            // A simulated program has no caller to lose: only the node agent's socket abandons one.
            abandoned: 0,
            last_resort: None,
        })
    }
}

impl SimRuntime {
    /// A signal to an image VM: KILL stops it, ending its run as Shards' stop does; any other is its
    /// members' and awaits Shards' agent-control endpoint; once it has ended, none reaches it.
    fn signal_vm(b: &SimBox, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        let init = match &b.init {
            Some(init) if init.exit()?.is_none() => init,
            _ => return Err(Refusal::NotRunning(id.clone(), "signal")),
        };
        if signal == Signal::KILL {
            return init.end(Exit::Signal(Signal::KILL.number()));
        }
        Err(Refusal::Awaits {
            needs: Upstream::MemberSignal,
        })
    }

    /// A running image VM's usage: every vCPU ran the guest the whole time since its boot. Before its
    /// boot and after its end there is no VM to read.
    fn usage_vm(b: &SimBox, vm: &SimVm, id: &BoxId) -> Result<Usage, Refusal> {
        let running = match &b.init {
            Some(init) => init.exit()?.is_none(),
            None => false,
        };
        let Some(booted) = vm.booted.filter(|_| running) else {
            return Err(Refusal::NotRunning(id.clone(), "usage"));
        };
        let ran = u64::try_from(booted.elapsed().as_nanos())
            .map_err(|_| Refusal::Broken("a simulated VM ran past u64 nanoseconds".to_owned()))?;
        let vcpus = usize::try_from(vm.vcpus)
            .ok()
            .filter(|n| *n <= GuestTime::MAX_VCPUS)
            .ok_or(Refusal::Exhausted {
                what: "guest time per vCPU",
                bound: GuestTime::MAX_VCPUS,
            })?;
        let mut per_vcpu = Vec::new();
        per_vcpu
            .try_reserve_exact(vcpus)
            .map_err(|_| Refusal::Exhausted {
                what: "guest time per vCPU",
                bound: vcpus,
            })?;
        per_vcpu.extend(std::iter::repeat_n(ran, vcpus));
        Ok(Usage {
            pids: 1,
            guest: Some(GuestTime::PerVcpuNs(per_vcpu)),
            ..b.usage.clone()
        })
    }
}

/// How a simulated init ends once started: `fail N` with N, `say` with 0, anything else (and a
/// bundle, whose program the simulator can't read, and an image VM running its members alone) not
/// until it is signalled. An image VM's init is its run's command.
fn init_ends(class: &ClassSpec) -> Result<Option<Exit>, Refusal> {
    let args: &[String] = match class {
        ClassSpec::Oci(spec) | ClassSpec::Firecracker(spec) | ClassSpec::Shards(spec) => {
            &spec.init.args
        }
        ClassSpec::ShardsImage(image) => image.command.as_deref().unwrap_or_default(),
        ClassSpec::Process | ClassSpec::Bundle(_) => return Ok(None),
    };
    match args.first().map(String::as_str) {
        Some("fail") => Ok(Some(Exit::Code(arg_in(args, 1)?))),
        Some("say") => Ok(Some(Exit::Code(0))),
        _ => Ok(None),
    }
}

fn arg<T: std::str::FromStr>(p: &Program, i: usize) -> Result<T, Refusal> {
    arg_in(&p.args, i)
}

fn arg_in<T: std::str::FromStr>(args: &[String], i: usize) -> Result<T, Refusal> {
    args.get(i)
        .and_then(|a| a.parse().ok())
        .ok_or(Refusal::Invalid {
            what: "program",
            why: format!("{:?} needs a number as argument {i}", args.first()),
        })
}

fn write(to: Option<&mut std::io::PipeWriter>, bytes: &[u8]) {
    // A reader that has gone is the caller's choice; the program runs on as a real one would.
    if let Some(w) = to {
        let _ = w.write_all(bytes);
    }
}

impl Runtime for SimRuntime {
    fn physical_report(&self) -> Result<crate::PhysicalReport, Refusal> {
        // This CPU-only simulated owner has no native inventory or attachment grant.
        let name = "simulator:cpu";
        let mut authority = String::new();
        authority
            .try_reserve_exact(name.len())
            .map_err(|_| Refusal::Exhausted {
                what: "simulated native authority bytes",
                bound: name.len(),
            })?;
        authority.push_str(name);
        Ok(crate::PhysicalReport {
            unavailable: Vec::new(),
            authority,
            devices: Vec::new(),
        })
    }

    fn classes(&self) -> Vec<Class> {
        match self.backend {
            Some(class) => vec![class],
            None => vec![
                Class::Process,
                Class::Oci,
                Class::Firecracker,
                Class::Shards,
            ],
        }
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        spec.check_no_devices("sim")?;
        if self
            .backend
            .is_some_and(|class| class != spec.class.class())
        {
            return Err(Refusal::Invalid {
                what: "simulated backend",
                why: "payload belongs to another initialized backend".into(),
            });
        }
        match &spec.class {
            ClassSpec::Oci(oci) | ClassSpec::Firecracker(oci) | ClassSpec::Shards(oci) => {
                oci.init.fits(&SIM_EXEC_LIMIT)?
            }
            // The run's command runs in the VM's Linux guest, held to the same bound.
            ClassSpec::ShardsImage(image) => {
                if let Some(command) = &image.command {
                    Program {
                        args: command.clone(),
                        ..Program::default()
                    }
                    .fits(&SIM_EXEC_LIMIT)?
                }
                // Grants as the class holds them (joint plan step 15, HEX-IMG-7): publishes alone, on a run of
                // the members alone, the rest awaiting Shards, refused before anything is made.
                for grant in &image.grants {
                    let needs = match grant {
                        Grant::Egress {
                            to: Destination::Name(_),
                            ..
                        } => Upstream::MemberGrantsByName,
                        Grant::Egress {
                            to: Destination::Address(_),
                            ..
                        } => Upstream::MemberGrantsByAddress,
                        Grant::Host { .. } => Upstream::MemberHostService,
                        Grant::Publish { .. } if image.command.is_some() => {
                            Upstream::PublishReceiver
                        }
                        Grant::Publish { .. } => continue,
                    };
                    return Err(Refusal::Awaits { needs });
                }
            }
            ClassSpec::Process | ClassSpec::Bundle(_) => {}
        }
        let vm = match &spec.class {
            ClassSpec::ShardsImage(_) => Some(SimVm {
                vcpus: spec
                    .allocation
                    .ok_or_else(|| {
                        Refusal::Broken("a checked Shards image spec has no allocation".to_owned())
                    })?
                    .vm_size()?
                    .vcpus,
                booted: None,
            }),
            _ => None,
        };
        let mut boxes = self.boxes()?;
        if boxes.contains_key(&spec.id) {
            return Err(Refusal::Exists(spec.id));
        }
        if boxes.len() >= SIM_BOXES {
            return Err(Refusal::Full(SIM_BOXES));
        }
        let init = (spec.class.class() != Class::Process).then(|| Arc::new(SimProc::default()));
        let init_ends = init_ends(&spec.class)?;
        let b = SimBox {
            spec,
            usage: Usage::default(),
            procs: Vec::new(),
            init,
            started: false,
            init_ends,
            run: None,
            vm,
        };
        let info = Self::info(&b)?;
        boxes.insert(b.spec.id.clone(), b);
        Ok(info)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let Some(init) = &b.init else {
            return Err(Refusal::Invalid {
                what: "start",
                why: format!("box {id} is a process box, which has no init to start"),
            });
        };
        if b.started {
            return Err(Refusal::Invalid {
                what: "start",
                why: format!("box {id} has started already"),
            });
        }
        if init.exit()?.is_some() {
            return Err(Refusal::NotRunning(id.clone(), "start"));
        }
        if let Some(vm) = &mut b.vm {
            vm.booted = Some(Instant::now());
        }
        if let Some(exit) = b.init_ends {
            init.end(exit)?;
        }
        b.started = true;
        Ok(())
    }

    fn exec(
        &self,
        id: &BoxId,
        program: Program,
        mut stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        program.check()?;
        program.fits(&SIM_EXEC_LIMIT)?;
        let given = stdio.make_asked()?;
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        if b.vm.is_some() {
            return Err(Refusal::Awaits {
                needs: Upstream::MemberExec,
            });
        }
        if b.procs.len() >= SIM_PROCS {
            return Err(Refusal::Full(SIM_PROCS));
        }
        let proc = Arc::new(SimProc::default());
        let name = program.args.first().map(String::as_str).unwrap_or_default();
        let bounds = &b.spec.bounds;
        match name {
            "say" => {
                let words = program.args.get(1..).unwrap_or_default().join(" ");
                write(stdio.stdout.as_mut(), format!("{words}\n").as_bytes());
                proc.end(Exit::Code(0))?;
            }
            "fail" => {
                let code: i32 = arg(&program, 1)?;
                write(stdio.stderr.as_mut(), b"failing\n");
                proc.end(Exit::Code(code))?;
            }
            "wait" => {}
            "hog" => {
                let bytes: u64 = arg(&program, 1)?;
                if bounds.memory_max.is_some_and(|m| bytes > m) {
                    b.usage.oom_kills = b.usage.oom_kills.saturating_add(1);
                    proc.end(Exit::Signal(Signal::KILL.number()))?;
                } else {
                    b.usage.memory_peak_bytes = b.usage.memory_peak_bytes.max(bytes);
                    proc.end(Exit::Code(0))?;
                }
            }
            "spawn" => {
                let n: u64 = arg(&program, 1)?;
                // The program is a process itself, so n more need n + 1 within the bound.
                if bounds.pids_max.is_some_and(|m| n >= u64::from(m)) {
                    b.usage.pids_refused = b.usage.pids_refused.saturating_add(1);
                    write(stdio.stderr.as_mut(), b"spawn: can't fork\n");
                    proc.end(Exit::Code(2))?;
                } else {
                    proc.end(Exit::Code(0))?;
                }
            }
            "pwd" => {
                let cwd = program.cwd.clone().unwrap_or_else(|| "/".to_owned());
                write(stdio.stdout.as_mut(), format!("{cwd}\n").as_bytes());
                proc.end(Exit::Code(0))?;
            }
            "echo" => {
                let p = Arc::clone(&proc);
                let (stdin, stdout) = (stdio.stdin.take(), stdio.stdout.take());
                std::thread::Builder::new()
                    .name("hex-sim-echo".to_owned())
                    .spawn(move || {
                        let code = match stdin {
                            None => 0,
                            Some(r) => {
                                let mut r = r.take(SIM_ECHO_BYTES);
                                let copied = match stdout {
                                    Some(mut w) => std::io::copy(&mut r, &mut w),
                                    None => std::io::copy(&mut r, &mut std::io::sink()),
                                };
                                if copied.is_ok_and(|n| n < SIM_ECHO_BYTES) {
                                    0
                                } else {
                                    1
                                }
                            }
                        };
                        let _ = p.end(Exit::Code(code));
                    })
                    .map_err(|e| Refusal::os("starting the echo program", e))?;
            }
            other => {
                return Err(Refusal::Invalid {
                    what: "program",
                    why: format!(
                        "{other:?}: the simulator runs say, fail, wait, hog, spawn, pwd and echo"
                    ),
                });
            }
        }
        if let Some(run) = &b.run {
            run.left.lock().map_err(poisoned)?.push(Arc::clone(&proc));
        }
        b.procs.push(Arc::clone(&proc));
        Ok(Box::new(SimHandle(proc, Some(given))))
    }

    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_no_devices("sim")?;
        spec.check_run()?;
        let id = spec.id.clone();
        self.create(spec)?;
        let proc = match self.exec(&id, program, stdio) {
            Ok(proc) => proc,
            Err(e) => {
                self.delete(&id)?;
                return Err(e);
            }
        };
        let mut boxes = self.boxes()?;
        if let Some(b) = boxes.get_mut(&id) {
            b.run = b.procs.last().cloned();
        }
        Ok(proc)
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        let boxes = self.boxes()?;
        let b = boxes
            .get(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        if b.vm.is_some() {
            return Self::signal_vm(b, id, signal);
        }
        let mut any = false;
        // Each program takes the signal itself, a run's among them, whose box then goes.
        for p in &b.procs {
            if p.exit()?.is_none() {
                p.set(Exit::Signal(signal.number()))?;
                any = true;
            }
        }
        // An init is its PID namespace's init, which the kernel gives only KILL (and STOP) unless it has
        // a handler for the signal (pid_namespaces(7)); the simulator's inits have none. A class with no
        // POSIX signals ends the init on TERM instead (Windows ends the container's job).
        if let Some(init) = &b.init
            && init.exit()?.is_none()
        {
            if signal == Signal::KILL || (signal == Signal::TERM && self.term_ends_init) {
                init.end(Exit::Signal(signal.number()))?;
            }
            any = true;
        }
        if any {
            Ok(())
        } else {
            Err(Refusal::NotRunning(id.clone(), "signal"))
        }
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let boxes = self.boxes()?;
        Self::info(
            boxes
                .get(id)
                .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?,
        )
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        let boxes = self.boxes()?;
        let b = boxes
            .get(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        if let Some(vm) = &b.vm {
            return Self::usage_vm(b, vm, id);
        }
        let mut u = b.usage.clone();
        let mut running = 0u64;
        for p in b.procs.iter().chain(&b.init) {
            if p.exit()?.is_none() {
                running = running.saturating_add(1);
            }
        }
        u.pids = running;
        Ok(u)
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.boxes()?.values().map(Self::info).collect()
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        let b = self
            .boxes()?
            .remove(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        for p in b.procs.iter().chain(&b.init) {
            p.end(Exit::Signal(Signal::KILL.number()))?;
        }
        Ok(())
    }
}

/// A simulated program, and the caller's ends of the streams its exec asked for until they are taken.
struct SimHandle(Arc<SimProc>, Option<crate::Given>);

impl Proc for SimHandle {
    fn pid(&self) -> u32 {
        0
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.0.exit()
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        let mut exit = self.0.exit.lock().map_err(poisoned)?;
        loop {
            if exit.is_some() {
                return Ok(*exit);
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => timeout,
            };
            if left.is_zero() {
                return Ok(None);
            }
            exit = self.0.ended.wait_timeout(exit, left).map_err(poisoned)?.0;
        }
    }
    fn kill(&mut self) -> Result<(), Refusal> {
        self.0.end(Exit::Signal(Signal::KILL.number()))
    }

    fn streams(&mut self) -> Result<crate::Given, Refusal> {
        Ok(self.1.take().unwrap_or_default())
    }
}

/// The simulator's programs, in boxes of either class; an `oci` box made from a bundle or from
/// the structured spec. The conformance suite's fixture for the simulator, and the oracle's in
/// [`crate::differential`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimFixture {
    Process,
    Bundle,
    Spec,
    Firecracker,
    Shards,
    /// A Shards image VM, for the image class's own cases ([`crate::conformance::IMAGE_CASES`]).
    ShardsImage,
}

/// The image the simulator's Shards image VMs boot, pinned by a digest: a name and a sha256's 64 hex
/// digits, the fixture's own.
pub const SIM_IMAGE: &str =
    "hex-sim/harness@sha256:5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";

/// The simulator's Shards image VM's admitted allocation, the fixture's own inputs: two vCPUs, so the
/// per-vCPU cases see more than one, and 256 MiB, even and past Shards' least on every host (128 MiB,
/// x86_64's; shards_api::min_memory_mib), under a cost profile of its own (a named identity, unit
/// quanta, no overhead), not a measured one.
pub fn sim_vm_allocation() -> Result<Allocation, Refusal> {
    let vm = CapacityResources {
        cpu_milli: 2_000,
        mem_bytes: 256 << 20,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: *b"hex-sim shards image VM fixture.",
        floor: CapacityResources::default(),
        ceiling: vm,
        quantum: CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
    .allocate(vm)
}

impl SimFixture {
    fn oci(&self, act: &Act) -> ClassSpec {
        let workload = Box::new(OciSpec {
            rootfs: Rootfs::Dir("/rootfs".into()),
            init: self.program(act),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        });
        match self {
            Self::Firecracker => ClassSpec::Firecracker(workload),
            Self::Shards => ClassSpec::Shards(workload),
            Self::Process | Self::Bundle | Self::Spec | Self::ShardsImage => {
                ClassSpec::Oci(workload)
            }
        }
    }

    /// An image VM whose run's command does `act`, or that runs its members alone.
    fn image(&self, act: Option<&Act>) -> ClassSpec {
        ClassSpec::ShardsImage(Box::new(ShardsImage {
            command: act.map(|act| self.program(act).args),
            ..ShardsImage::new(SIM_IMAGE)
        }))
    }

    /// An image VM's admitted size; none for any other class.
    fn allocation(&self) -> Option<Allocation> {
        match self {
            Self::ShardsImage => sim_vm_allocation().ok(),
            Self::Process | Self::Bundle | Self::Spec | Self::Firecracker | Self::Shards => None,
        }
    }
}

impl Fixture for SimFixture {
    fn exec_limit(&self) -> Option<crate::ExecLimit> {
        Some(SIM_EXEC_LIMIT)
    }
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        BoxSpec {
            devices: Vec::new(),
            allocation: self.allocation(),
            route: None,
            id,
            bounds,
            class: match self {
                Self::Process => ClassSpec::Process,
                Self::Bundle => ClassSpec::Bundle("/bundle".into()),
                Self::Spec | Self::Firecracker | Self::Shards => self.oci(&Act::Wait),
                Self::ShardsImage => self.image(None),
            },
            owner: None,
        }
    }
    fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
        BoxSpec {
            devices: Vec::new(),
            allocation: self.allocation(),
            route: None,
            id,
            bounds: Bounds::default(),
            class: match self {
                Self::Process => ClassSpec::Process,
                Self::Bundle | Self::Spec | Self::Firecracker | Self::Shards => self.oci(act),
                Self::ShardsImage => self.image(Some(act)),
            },
            owner: None,
        }
    }
    fn program(&self, act: &Act) -> Program {
        match act {
            Act::Say(words) => Program::of(&["say", words]),
            Act::Fail(code) => Program::of(&["fail", &code.to_string()]),
            Act::Wait => Program::of(&["wait"]),
            Act::Hog(bytes) => Program::of(&["hog", &bytes.to_string()]),
            Act::Spawn(n) => Program::of(&["spawn", &n.to_string()]),
            Act::Echo => Program::of(&["echo"]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;

    #[test]
    fn the_simulator_passes_the_conformance_suite() {
        for fixture in [
            SimFixture::Process,
            SimFixture::Bundle,
            SimFixture::Spec,
            SimFixture::Firecracker,
            SimFixture::Shards,
        ] {
            let failures = conformance::run(&SimRuntime::new(), &fixture);
            assert!(failures.is_empty(), "{fixture:?}: {failures:#?}");
        }
    }

    /// AC-T7.4.1 (joint plan step 12): run the Shards image class's own cases on the simulator. Expect:
    /// none fails.
    #[test]
    fn the_simulator_passes_the_shards_image_cases() {
        let failures = conformance::run_image(&SimRuntime::new(), &SimFixture::ShardsImage);
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// AC-T7.4.1 (joint plan step 12, the learned charge's input): read a running image VM's usage
    /// twice. Expect: one entry for each of its two vCPUs, each grown between the reads, as a VM that
    /// runs every vCPU does, and none past the wall time since its start.
    #[test]
    fn a_running_image_vms_guest_time_grows_by_vcpu_within_its_wall_time() {
        let rt = SimRuntime::new();
        let id = BoxId::new("guest-time").unwrap();
        rt.create(SimFixture::ShardsImage.spec(id.clone(), Bounds::default()))
            .unwrap();
        let before = Instant::now();
        rt.start(&id).unwrap();
        let per_vcpu = |u: Usage| match u.guest {
            Some(GuestTime::PerVcpuNs(ns)) => ns,
            other => panic!("{other:?}"),
        };
        let first = per_vcpu(rt.usage(&id).unwrap());
        let second = loop {
            let read = per_vcpu(rt.usage(&id).unwrap());
            if read.iter().zip(&first).all(|(b, a)| b > a) {
                break read;
            }
        };
        let wall = u64::try_from(before.elapsed().as_nanos()).unwrap();
        assert_eq!(second.len(), 2);
        assert!(
            second.iter().all(|ns| *ns <= wall),
            "{second:?} past {wall}"
        );
    }

    /// HEX-DIFF-1/2's oci half: signal TERM to an oci box ends its exec'd programs and leaves its init
    /// running, as the kernel does for a PID namespace's init with no handler (pid_namespaces(7)); KILL
    /// ends the init too.
    #[test]
    fn only_kill_ends_an_oci_boxs_init() {
        let rt = SimRuntime::new();
        let id = BoxId::new("init").unwrap();
        rt.create(SimFixture::Bundle.spec(id.clone(), Bounds::default()))
            .unwrap();
        rt.start(&id).unwrap();
        let mut p = rt
            .exec(&id, SimFixture::Bundle.program(&Act::Wait), Stdio::null())
            .unwrap();
        rt.signal(&id, Signal::TERM).unwrap();
        assert_eq!(p.try_wait().unwrap(), Some(Exit::Signal(15)));
        let info = rt.inspect(&id).unwrap();
        assert_eq!((info.state, info.exit), (BoxState::Running, None));
        rt.signal(&id, Signal::TERM).unwrap();
        rt.signal(&id, Signal::KILL).unwrap();
        let info = rt.inspect(&id).unwrap();
        assert_eq!(
            (info.state, info.exit),
            (BoxState::Empty, Some(Exit::Signal(9)))
        );
        assert!(matches!(
            rt.signal(&id, Signal::TERM),
            Err(Refusal::NotRunning(..))
        ));
    }

    /// HEX-DIFF-1/2's oci half where the class ends its init on TERM (Windows has no POSIX signals, so
    /// its oci class ends the container's job): a simulator told so ends the init on TERM.
    #[test]
    fn term_ends_the_init_where_the_class_says_it_does() {
        let rt = SimRuntime::with_term_ending_init(true);
        let id = BoxId::new("init").unwrap();
        rt.create(SimFixture::Bundle.spec(id.clone(), Bounds::default()))
            .unwrap();
        rt.start(&id).unwrap();
        rt.signal(&id, Signal::TERM).unwrap();
        let info = rt.inspect(&id).unwrap();
        assert_eq!(
            (info.state, info.exit),
            (BoxState::Empty, Some(Exit::Signal(15)))
        );
    }

    #[test]
    fn the_simulator_refuses_past_its_box_bound() {
        let rt = SimRuntime::new();
        for i in 0..SIM_BOXES {
            let id = BoxId::new(&format!("b{i}")).unwrap();
            rt.create(SimFixture::Process.spec(id, Bounds::default()))
                .unwrap();
        }
        let id = BoxId::new("one-more").unwrap();
        assert_eq!(
            rt.create(SimFixture::Process.spec(id, Bounds::default())),
            Err(Refusal::Full(SIM_BOXES))
        );
    }
}
