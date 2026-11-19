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
//! by start, ended by KILL, its members the ones its spec names (taken, signalled, and running the
//! simulator's programs as a member's exec, `Runtime::exec_member`; an exec naming no member refused as
//! the class refuses it), and its guest time per vCPU read while it runs. The simulated VM runs every
//! vCPU the whole time it runs but while its owner holds it paused (kiln plan P5.5b): the most guest time a real
//! one can report, so a consumer's bound holds against it, and none while paused, as a real one's stays flat. Its
//! members' components are swapped at once (Shards' D270 slice 4): its image runs one harness of its own
//! ([`SIM_HARNESS`]) beside the members its spec says its caller drives, and its node's store holds one harness's
//! component ([`SIM_COMPONENT`]). The
//! warm pools it's asked to keep ([`Runtime::keep_pool`]) are names alone, at most one for each box it
//! may hold: it boots no host ahead of a box, so none is ever warm or starting. Its volumes ([`ShardsImage::volumes`],
//! kiln plan 01a1dd2e…, P6.2) are the simulator's home's, as the image class holds them: their bounds within the box's
//! admitted disk, made as its start plugs them, held while its VM runs, and listed and cleared as Shards' store answers
//! ([`Runtime::volumes`], [`Runtime::clear_volume`]); their files take no bytes, its members writing none. Its shares
//! another server serves ([`ShardsImage::served`], Shards' D286) are asked of the server it was given
//! ([`SimRuntime::with_servers`]) as its start boots it, each stream held while its VM runs, as the class asks them;
//! given none, it refuses them as the class does a node given none, `Unconfigured`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::conformance::{Act, Fixture};
use crate::{
    Allocation, Bounds, BoxId, BoxInfo, BoxSpec, BoxState, CapacityResources, Class, ClassSpec,
    ComponentAt, Exit, GuestTime, Member, MemberKind, Network, OciSpec, PauseCause, Proc, Program,
    Refusal, Rootfs, Runtime, RuntimeCost, ShardsImage, Signal, Stdio, SwapCause, SwapWhen,
    Swapped, Usage, WarmHeld, WarmPool,
};
use crate::{HomeVolume, VolumeCause};

/// The echo program: what comes in on `stdin` goes out on `stdout`, at most [`SIM_ECHO_BYTES`] of it, and it ends
/// 0 at its input's end (1 past the most it copies), on a thread of its own.
fn echo(
    proc: Arc<SimProc>,
    stdin: Option<std::io::PipeReader>,
    stdout: Option<std::io::PipeWriter>,
) -> Result<(), Refusal> {
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
            let _ = proc.end(Exit::Code(code));
        })
        .map(|_| ())
        .map_err(|e| Refusal::os("starting the echo program", e))
}

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
    /// The warm pools it keeps, by name alone, at most [`SIM_BOXES`].
    pools: Mutex<std::collections::BTreeSet<WarmPool>>,
    /// Its home's volumes, by name: each its bound and when it was made. Locked after `boxes` where a call takes both.
    volumes: Mutex<BTreeMap<String, SimVolume>>,
    /// What serves its image VMs' shares another server serves (Shards' D286; [`SimRuntime::with_servers`]): none, so a
    /// box naming one is refused as it is created.
    servers: Option<Arc<dyn crate::serve::Servers>>,
}

/// A volume of the simulator's home (kiln plan 01a1dd2e…, P6.2): the bound its last first holder set, and when it was
/// made, in nanoseconds since the Unix epoch.
struct SimVolume {
    bound: u64,
    created_ns: u64,
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

/// A simulated Shards image VM: its vCPUs, from its allocation, when its start booted it, the members whose
/// streams its caller took ([`Runtime::member`]), each with its program, at most the members its spec names, its
/// owner's pauses (kiln plan P5.5b): whether its vCPUs run now, and every pause's time that has ended, and its members'
/// components and generations (Shards' D270 slices 3b and 4).
struct SimVm {
    vcpus: u32,
    booted: Option<Instant>,
    /// The members whose current generation's streams its caller took, each with its program: a member started again
    /// leaves it, its next generation's to take.
    taken: Vec<(Member, Arc<SimProc>)>,
    /// The streams of its shares another server serves, asked as its start booted it and held while it runs, as its
    /// run host and VM hold theirs (Shards' D286).
    served: Vec<crate::serve::Stream>,
    hold: SimHold,
    /// Every pause's time that has ended.
    pauses: Duration,
    /// Each member's component by its path, as its run holds it: the one its next start lays there and the one its
    /// current generation runs with, by digest, none for its image's own. A path neither names is left out.
    components: BTreeMap<String, (Option<String>, Option<String>)>,
    /// Each member's current generation by its path, where a swap or restore now started it again: 0, its first, else.
    generations: BTreeMap<String, u32>,
}

/// Whether a simulated VM's vCPUs run its guest, as its owner last moved them ([`Runtime::pause`], [`Runtime::resume`]):
/// a move is made at once, so none is ever under way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimHold {
    Going,
    /// Held out of its guest since this instant.
    Paused(Instant),
}

impl SimVm {
    /// Every pause's time to `now`, the one under way among it.
    fn paused(&self, now: Instant) -> Result<Duration, Refusal> {
        match self.hold {
            SimHold::Going => Ok(self.pauses),
            SimHold::Paused(since) => self
                .pauses
                .checked_add(now.saturating_duration_since(since))
                .ok_or_else(|| {
                    Refusal::Broken("a simulated VM's paused time passes a Duration".to_owned())
                }),
        }
    }
}

#[derive(Default)]
struct SimProc {
    exit: hex_wait::Mutex<Option<Exit>>,
    ended: hex_wait::Condvar,
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

/// A wait on a simulator lock that couldn't wait: its lock poisoned, or the OS refused it (macOS's kqueue).
fn wait_refused(why: hex_wait::Failed) -> Refusal {
    match why {
        hex_wait::Failed::Poisoned => Refusal::Broken("a simulator lock was poisoned".to_owned()),
        hex_wait::Failed::Os(why) => Refusal::Os {
            op: "waiting on a simulator lock",
            why: why.to_string(),
        },
    }
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

    /// Its image VMs' shares another server serves asked of `servers` as each boots (Shards' D286), as the class asks
    /// its node's: a box naming one is no longer refused as it is created.
    pub fn with_servers(self, servers: Arc<dyn crate::serve::Servers>) -> Self {
        Self {
            servers: Some(servers),
            ..self
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
            } else if b
                .vm
                .as_ref()
                .is_some_and(|vm| matches!(vm.hold, SimHold::Paused(_)))
            {
                BoxState::Paused
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
            ended: None,
            control: None,
            control_wake: None,
            credentials: match &b.spec.class {
                ClassSpec::ShardsImage(image) => image.credentials.clone(),
                _ => Vec::new(),
            },
        })
    }
}

impl SimRuntime {
    /// The streams of `image`'s shares another server serves, for box `id`'s boot, each asked of its server in its spec's
    /// order (Shards' D286): refused `Unconfigured` where it was given none, as the class refuses them, and as its
    /// server refuses one.
    fn served_streams(
        &self,
        id: &BoxId,
        image: &ShardsImage,
    ) -> Result<Vec<crate::serve::Stream>, Refusal> {
        if image.served.is_empty() {
            return Ok(Vec::new());
        }
        let servers = self
            .servers
            .as_ref()
            .ok_or_else(crate::serve::unconfigured)?;
        image
            .served
            .iter()
            .map(|share| servers.stream(id, share))
            .collect()
    }

    /// `image`'s volumes as its run starts, as the class plugs them (kiln plan 01a1dd2e…, P6.2): refused
    /// `Volume { Bound }` where the running boxes, `held`, serve one with another bound; else each made where it isn't,
    /// its bound the one asked.
    fn plug_volumes(
        &self,
        held: &BTreeMap<String, u64>,
        image: &ShardsImage,
    ) -> Result<(), Refusal> {
        if let Some(v) = image.volumes.iter().find(|v| {
            held.get(&v.name)
                .is_some_and(|bound| *bound != v.bound.get())
        }) {
            return Err(Refusal::Volume {
                cause: VolumeCause::Bound,
                why: format!(
                    "volume {} is served with another bound than the {} bytes asked: its bound changes only while no \
                     run holds it",
                    v.name, v.bound
                ),
            });
        }
        let made_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            });
        let mut volumes = self.volumes.lock().map_err(poisoned)?;
        for v in &image.volumes {
            volumes
                .entry(v.name.clone())
                .or_insert(SimVolume {
                    bound: v.bound.get(),
                    created_ns: made_ns,
                })
                .bound = v.bound.get();
        }
        Ok(())
    }

    /// `program` started in box `b` with `stdio`, the caller's asked ends `given` handed with it: the simulator's
    /// programs, each run as an exec's is, at most [`SIM_PROCS`] in a box.
    fn started(
        b: &mut SimBox,
        program: &Program,
        mut stdio: Stdio,
        given: crate::Given,
    ) -> Result<Box<dyn Proc>, Refusal> {
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
                let code: i32 = arg(program, 1)?;
                write(stdio.stderr.as_mut(), b"failing\n");
                proc.end(Exit::Code(code))?;
            }
            "wait" => {}
            "hog" => {
                let bytes: u64 = arg(program, 1)?;
                if bounds.memory_max.is_some_and(|m| bytes > m) {
                    b.usage.oom_kills = b.usage.oom_kills.saturating_add(1);
                    proc.end(Exit::Signal(Signal::KILL.number()))?;
                } else {
                    b.usage.memory_peak_bytes = b.usage.memory_peak_bytes.max(bytes);
                    proc.end(Exit::Code(0))?;
                }
            }
            "spawn" => {
                let n: u64 = arg(program, 1)?;
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
            "echo" => echo(Arc::clone(&proc), stdio.stdin.take(), stdio.stdout.take())?,
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

    /// Box `b`'s VM, its run going, for its pause or resume, `op` (kiln plan P5.5b); or why not, as the image class refuses
    /// it: a box of any other class as one that pauses none, its run ended, or none started yet.
    fn movable<'b>(
        b: &'b mut SimBox,
        id: &BoxId,
        op: &'static str,
    ) -> Result<&'b mut SimVm, Refusal> {
        let refused = |cause| Refusal::Pause {
            id: id.clone(),
            op,
            cause,
        };
        let class = b.spec.class.class();
        let ended = match &b.init {
            Some(init) => init.exit()?.is_some(),
            None => false,
        };
        let started = b.started;
        let vm =
            b.vm.as_mut()
                .ok_or_else(|| refused(PauseCause::Class(class)))?;
        if ended {
            return Err(refused(PauseCause::Ended));
        }
        if !started {
            return Err(refused(PauseCause::NotRunning));
        }
        Ok(vm)
    }

    /// Box `b`'s VM, its run going, for `op`: a swap, a restore or a read of its components (Shards' D270 slice 4); or why
    /// not, as the image class refuses it: a box of any other class as one with no components, its run ended, or none
    /// started yet.
    fn swappable<'b>(
        b: &'b mut SimBox,
        id: &BoxId,
        op: &'static str,
    ) -> Result<&'b mut SimVm, Refusal> {
        let refused = |cause| Refusal::Swap {
            id: id.clone(),
            op,
            cause,
            why: None,
        };
        let class = b.spec.class.class();
        let ended = match &b.init {
            Some(init) => init.exit()?.is_some(),
            None => false,
        };
        let started = b.started;
        let vm =
            b.vm.as_mut()
                .ok_or_else(|| refused(SwapCause::Class(class)))?;
        if ended {
            return Err(refused(SwapCause::Ended));
        }
        if !started {
            return Err(refused(SwapCause::NotRunning));
        }
        Ok(vm)
    }

    /// Box `id`'s member at path `at` given component `digest`, or its image's own back where none (`op`, a swap or a
    /// restore), at its next start or now, as Shards' run host makes it (D270 slice 4) and the image class refuses it:
    /// typed where it can't be made, `Pause(Paused)` while its VM is paused; now, its current generation ends as TERM
    /// ends a program with no handler, its streams no caller's any longer, and its next is current, its streams taken
    /// anew. The simulator's members say nothing no caller reads, so none is let go unread.
    fn change(
        &self,
        id: &BoxId,
        at: &str,
        digest: Option<&str>,
        when: SwapWhen,
        op: &'static str,
    ) -> Result<Swapped, Refusal> {
        let refused = |cause, why: String| Refusal::Swap {
            id: id.clone(),
            op,
            cause,
            why: Some(why),
        };
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let member = match &b.spec.class {
            ClassSpec::ShardsImage(image) => member_at(image, at),
            _ => None,
        };
        let vm = Self::swappable(b, id, op)?;
        if matches!(vm.hold, SimHold::Paused(_)) {
            return Err(Refusal::Pause {
                id: id.clone(),
                op,
                cause: PauseCause::Paused,
            });
        }
        let member = member.ok_or_else(|| {
            refused(
                SwapCause::Unknown,
                format!("no member of the image runs at {at}"),
            )
        })?;
        if let Some(digest) = digest {
            if digest != SIM_COMPONENT {
                return Err(refused(
                    SwapCause::Artifact,
                    format!("its node's store holds no artifact {digest}"),
                ));
            }
            if member.kind != MemberKind::Harness {
                return Err(refused(
                    SwapCause::Kind,
                    format!("{digest} is a harness's, and {at} an agent's path"),
                ));
            }
        }
        let held = vm.components.get(at).and_then(|(_, held)| held.clone());
        let desired = digest.map(str::to_owned);
        let started = match when {
            SwapWhen::NextStart => None,
            SwapWhen::Now => {
                // A member whose generation ended with no start again under way ended whole: Shards' restart refuses
                // it, and its swap says so (D270 slice 3b).
                if let Some((_, proc)) = vm.taken.iter().find(|(taken, _)| *taken == member) {
                    if proc.exit()?.is_some() {
                        return Err(refused(
                            SwapCause::Failed,
                            format!(
                                "its member at {at} ended with its generation, and runs no more"
                            ),
                        ));
                    }
                    proc.set(Exit::Signal(Signal::TERM.number()))?;
                }
                vm.taken.retain(|(taken, _)| *taken != member);
                let next = vm
                    .generations
                    .get(at)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or_else(|| {
                        refused(
                            SwapCause::Failed,
                            format!("{at} has no generation past {}", u32::MAX),
                        )
                    })?;
                vm.generations.insert(at.to_owned(), next);
                Some(next)
            }
        };
        let held = if started.is_some() {
            desired.clone()
        } else {
            held
        };
        if desired.is_none() && held.is_none() {
            vm.components.remove(at);
        } else {
            vm.components.insert(at.to_owned(), (desired, held));
        }
        Ok(Swapped {
            components: listed(&vm.components),
            started,
            unread: 0,
        })
    }

    /// A signal to an image VM: KILL stops it, ending its run as Shards' stop does, paused or not; any other is its run's
    /// members', which its image runs and take it as their lanes give it (goal 2's item 2), the VM running on, and which
    /// a paused VM's members can't answer, refused at once as the class refuses it (P5.5b); once it has ended, none
    /// reaches it.
    fn signal_vm(b: &SimBox, vm: &SimVm, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        let init = match &b.init {
            Some(init) if init.exit()?.is_none() => init,
            _ => return Err(Refusal::NotRunning(id.clone(), "signal")),
        };
        if signal == Signal::KILL {
            return init.end(Exit::Signal(Signal::KILL.number()));
        }
        if matches!(vm.hold, SimHold::Paused(_)) {
            return Err(Refusal::Pause {
                id: id.clone(),
                op: "signal",
                cause: PauseCause::Paused,
            });
        }
        Ok(())
    }

    /// A running image VM's usage: every vCPU ran the guest the whole time since its boot that it wasn't paused, and its
    /// paused time (P5.5b). Before its boot and after its end there is no VM to read.
    fn usage_vm(b: &SimBox, vm: &SimVm, id: &BoxId) -> Result<Usage, Refusal> {
        let running = match &b.init {
            Some(init) => init.exit()?.is_none(),
            None => false,
        };
        let Some(booted) = vm.booted.filter(|_| running) else {
            return Err(Refusal::NotRunning(id.clone(), "usage"));
        };
        // Its life since its boot and its paused time, both to one instant, so a VM paused throughout two reads shows the
        // same guest time at each; every pause began after its boot, so its life holds them all, and what ran is never
        // less than none.
        let now = Instant::now();
        let paused = u64::try_from(vm.paused(now)?.as_nanos()).map_err(|_| {
            Refusal::Broken("a simulated VM was paused past u64 nanoseconds".to_owned())
        })?;
        let ran = u64::try_from(now.saturating_duration_since(booted).as_nanos())
            .map_err(|_| Refusal::Broken("a simulated VM ran past u64 nanoseconds".to_owned()))?
            .saturating_sub(paused);
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
            paused_ns: Some(paused),
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
                Class::ShardsImage,
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
                // Grants as the class holds them (joint plan step 15, HEX-IMG-7): publishes alone, on any run,
                // the rest awaiting Shards (N4), refused before anything is made.
                if let Some(needs) = crate::unheld(&image.grants) {
                    return Err(Refusal::Awaits { needs });
                }
                // Its shares another server serves, where it was given none to serve them, as the class refuses them
                // (Shards' D286).
                if !image.served.is_empty() && self.servers.is_none() {
                    return Err(crate::serve::unconfigured());
                }
                // Its volumes' bounds within the disk it was admitted with, as the class holds them (P6.2).
                let admitted = spec.allocation.map_or(u64::MAX, |a| a.resources.disk_bytes);
                let bounds = image
                    .volumes
                    .iter()
                    .try_fold(0u64, |all, v| all.checked_add(v.bound.get()));
                if bounds.is_none_or(|bounds| bounds > admitted) {
                    return Err(Refusal::Invalid {
                        what: "shards image",
                        why: format!(
                            "its volumes' bounds pass the disk it was admitted with, {admitted} bytes"
                        ),
                    });
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
                taken: Vec::new(),
                served: Vec::new(),
                hold: SimHold::Going,
                pauses: Duration::ZERO,
                components: BTreeMap::new(),
                generations: BTreeMap::new(),
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
        // What the running boxes hold before this one starts, which holds nothing until it does.
        let held = held_bounds(&boxes)?;
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
        // Its shares another server serves, each one's stream asked of its server as it boots, in its spec's order (Shards'
        // D286): the first refused refuses its start before anything of its run, the box left created, as the class asks
        // them before its boot's intent.
        let served = match &b.spec.class {
            ClassSpec::ShardsImage(image) => self.served_streams(id, image)?,
            _ => Vec::new(),
        };
        if let ClassSpec::ShardsImage(image) = &b.spec.class
            && let Err(why) = self.plug_volumes(&held, image)
        {
            // Its run stopped, as the class stops one a volume is refused to: the box ends, its exit unread.
            init.end(Exit::Unknown)?;
            b.started = true;
            return Err(why);
        }
        if let Some(vm) = &mut b.vm {
            vm.booted = Some(Instant::now());
            vm.served = served;
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
        // A program in an image VM runs in a member it names, as the class refuses one that names none.
        if b.vm.is_some() {
            return Err(Refusal::Invalid {
                what: "exec",
                why: format!(
                    "box {id} is a Shards image VM: a program runs in one of its members, which exec_member names"
                ),
            });
        }
        Self::started(b, &program, stdio, given)
    }

    /// A program in a member of a running image VM its spec names (`Runtime::exec_member`), run as an exec's program is:
    /// its run's members the spec's here, as `signal_member` has them. Refused as the class refuses it: a program naming
    /// a user, Invalid; a member the spec names not, Invalid; a VM not running, or a member its caller took that has
    /// ended, NotRunning.
    fn exec_member(
        &self,
        id: &BoxId,
        member: &Member,
        program: Program,
        mut stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        program.check()?;
        program.fits(&SIM_EXEC_LIMIT)?;
        if program.user.is_some() {
            return Err(Refusal::Invalid {
                what: "program",
                why: "a program in a Shards image VM's member runs as its member does: it names no user"
                    .to_owned(),
            });
        }
        let given = stdio.make_asked()?;
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let ClassSpec::ShardsImage(image) = &b.spec.class else {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members a program runs in"
                ),
            });
        };
        if !image.members.iter().any(|m| &m.member == member) {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("box {id}'s run declares no {member}"),
            });
        }
        let running = match &b.init {
            Some(init) => b.started && init.exit()?.is_none(),
            None => false,
        };
        if !running {
            return Err(Refusal::NotRunning(id.clone(), "exec in a member"));
        }
        // A paused VM's members answer nothing until it resumes (P5.5b), refused at once as the class refuses it.
        if b.vm
            .as_ref()
            .is_some_and(|vm| matches!(vm.hold, SimHold::Paused(_)))
        {
            return Err(Refusal::Pause {
                id: id.clone(),
                op: "exec in a member",
                cause: PauseCause::Paused,
            });
        }
        let taken = b.vm.as_ref().and_then(|vm| {
            vm.taken
                .iter()
                .find(|(taken, _)| taken == member)
                .map(|(_, proc)| Arc::clone(proc))
        });
        if let Some(proc) = taken
            && proc.exit()?.is_some()
        {
            return Err(Refusal::NotRunning(
                id.clone(),
                "exec in a member: its member refused it, ended or confined",
            ));
        }
        Self::started(b, &program, stdio, given)
    }

    /// A member of a running image VM its spec names takes a signal alone (goal 2's item 2), its run's members the
    /// spec's here: one its caller took ends by it, as its echo program keeps no handler, its end `Signal(n)`; one not
    /// taken runs no program here, and takes it as nothing. Refused NotRunning where the VM doesn't run or the member
    /// has ended, as the class's own lane refuses one.
    fn signal_member(&self, id: &BoxId, member: &Member, signal: Signal) -> Result<(), Refusal> {
        let boxes = self.boxes()?;
        let b = boxes
            .get(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let ClassSpec::ShardsImage(image) = &b.spec.class else {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!(
                    "box {id} isn't a Shards image box: only one has members a signal reaches"
                ),
            });
        };
        if !image.members.iter().any(|m| &m.member == member) {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("box {id}'s run declares no {member}"),
            });
        }
        let running = match &b.init {
            Some(init) => b.started && init.exit()?.is_none(),
            None => false,
        };
        if !running {
            return Err(Refusal::NotRunning(id.clone(), "signal a member"));
        }
        // A paused VM's members answer nothing until it resumes (P5.5b), refused at once as the class refuses it.
        if b.vm
            .as_ref()
            .is_some_and(|vm| matches!(vm.hold, SimHold::Paused(_)))
        {
            return Err(Refusal::Pause {
                id: id.clone(),
                op: "signal a member",
                cause: PauseCause::Paused,
            });
        }
        let taken = b.vm.as_ref().and_then(|vm| {
            vm.taken
                .iter()
                .find(|(taken, _)| taken == member)
                .map(|(_, proc)| proc)
        });
        match taken {
            Some(proc) if proc.exit()?.is_some() => Err(Refusal::NotRunning(
                id.clone(),
                "signal a member: it has ended",
            )),
            Some(proc) => proc.set(Exit::Signal(signal.number())),
            None => Ok(()),
        }
    }

    /// A member its image box's spec names (joint plan step 17): the echo program, its stdin, stdout and stderr the
    /// caller's, taken once, while the box's VM runs; its end the echo's, or KILL with its VM.
    fn member(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, Refusal> {
        let mut stdio = Stdio {
            ask: crate::Ask {
                stdin: true,
                stdout: true,
                stderr: true,
            },
            ..Stdio::null()
        };
        let given = stdio.make_asked()?;
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let named = match &b.spec.class {
            ClassSpec::ShardsImage(image) => image.members.iter().any(|m| &m.member == member),
            _ => {
                return Err(Refusal::Invalid {
                    what: "member",
                    why: format!(
                        "box {id} isn't a Shards image box: only one has members its caller drives"
                    ),
                });
            }
        };
        if !named {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("box {id}'s spec names no {member} its caller drives"),
            });
        }
        let init_ended = match &b.init {
            Some(init) => init.exit()?.is_some(),
            None => true,
        };
        if !b.started || init_ended {
            return Err(Refusal::NotRunning(id.clone(), "member"));
        }
        if b.procs.len() >= SIM_PROCS {
            return Err(Refusal::Full(SIM_PROCS));
        }
        let vm =
            b.vm.as_mut()
                .ok_or_else(|| Refusal::Broken("a Shards image box has no VM".to_owned()))?;
        if vm.taken.iter().any(|(taken, _)| taken == member) {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("{member}'s streams in box {id} were taken already"),
            });
        }
        let proc = Arc::new(SimProc::default());
        vm.taken.push((member.clone(), Arc::clone(&proc)));
        echo(Arc::clone(&proc), stdio.stdin.take(), stdio.stdout.take())?;
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
        if let Some(vm) = &b.vm {
            return Self::signal_vm(b, vm, id, signal);
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

    /// Kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1: at most one pool for each box it may hold, as a node keeps at
    /// most one for each of its boxes, refused `Full` past them.
    fn keep_pool(&self, pool: &WarmPool) -> Result<(), Refusal> {
        pool.check()?;
        let mut pools = self.pools.lock().map_err(poisoned)?;
        if !pools.contains(pool) && pools.len() >= SIM_BOXES {
            return Err(Refusal::Full(SIM_BOXES));
        }
        pools.insert(pool.clone());
        Ok(())
    }

    fn let_go_pool(&self, pool: &WarmPool) -> Result<(), Refusal> {
        self.pools.lock().map_err(poisoned)?.remove(pool);
        Ok(())
    }

    fn pools(&self) -> Result<Vec<WarmHeld>, Refusal> {
        Ok(self
            .pools
            .lock()
            .map_err(poisoned)?
            .iter()
            .map(|pool| WarmHeld {
                pool: pool.clone(),
                warm: 0,
                starting: 0,
            })
            .collect())
    }

    /// Its home's volumes, each with its bound, its files taking no bytes.
    fn volumes(&self) -> Result<Vec<HomeVolume>, Refusal> {
        Ok(self
            .volumes
            .lock()
            .map_err(poisoned)?
            .iter()
            .map(|(name, v)| HomeVolume {
                name: name.clone(),
                bytes: Some(0),
                bound: Some(v.bound),
                charge: Some(0),
                created_ns: v.created_ns,
                anonymous: false,
            })
            .collect())
    }

    /// Volume `name` removed, refused `InUse` while a running box holds it and `Unknown` where there is none.
    fn clear_volume(&self, name: &str) -> Result<(), Refusal> {
        let boxes = self.boxes()?;
        if held_bounds(&boxes)?.contains_key(name) {
            return Err(Refusal::Volume {
                cause: VolumeCause::InUse,
                why: format!("volume {name} is held by a running box"),
            });
        }
        let mut volumes = self.volumes.lock().map_err(poisoned)?;
        match volumes.remove(name) {
            Some(_) => Ok(()),
            None => Err(Refusal::Volume {
                cause: VolumeCause::Unknown,
                why: format!("no volume {name} is in the simulator's home"),
            }),
        }
    }

    /// A running image VM's vCPUs held out of its guest (kiln plan P5.5b), at once, its guest time stopping and its
    /// paused time growing until its resume; refused, typed, as the image class refuses it ([`SimRuntime::movable`]), and
    /// `Paused` where it is paused already.
    fn pause(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let vm = Self::movable(b, id, "pause")?;
        match vm.hold {
            SimHold::Going => {
                vm.hold = SimHold::Paused(Instant::now());
                Ok(())
            }
            SimHold::Paused(_) => Err(Refusal::Pause {
                id: id.clone(),
                op: "pause",
                cause: PauseCause::Paused,
            }),
        }
    }

    /// A paused image VM's vCPUs let run its guest again (kiln plan P5.5b), at once, its pause's time added to its
    /// paused time; refused as [`Runtime::pause`] is here, and `Running` where it isn't paused.
    fn resume(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let vm = Self::movable(b, id, "resume")?;
        match vm.hold {
            SimHold::Paused(_) => {
                vm.pauses = vm.paused(Instant::now())?;
                vm.hold = SimHold::Going;
                Ok(())
            }
            SimHold::Going => Err(Refusal::Pause {
                id: id.clone(),
                op: "resume",
                cause: PauseCause::Running,
            }),
        }
    }

    /// A running image VM's member given the one component its node's store holds, at once ([`SimRuntime::change`]).
    fn swap(&self, id: &BoxId, at: &str, digest: &str, when: SwapWhen) -> Result<Swapped, Refusal> {
        self.change(id, at, Some(digest), when, "swap")
    }

    /// A running image VM's member given its image's own component back, at once ([`SimRuntime::change`]).
    fn restore(&self, id: &BoxId, at: &str, when: SwapWhen) -> Result<Swapped, Refusal> {
        self.change(id, at, None, when, "restore")
    }

    /// A running image VM's members' components, paused or not; refused as a swap is.
    fn components(&self, id: &BoxId) -> Result<Vec<ComponentAt>, Refusal> {
        let mut boxes = self.boxes()?;
        let b = boxes
            .get_mut(id)
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let vm = Self::swappable(b, id, "components")?;
        Ok(listed(&vm.components))
    }
}

/// The bound each volume a running image box holds is served with, by its name (kiln plan 01a1dd2e…, P6.2): a box holds
/// its volumes from its start until its VM ends, as a run holds them from its start's plug to its end.
fn held_bounds(boxes: &BTreeMap<BoxId, SimBox>) -> Result<BTreeMap<String, u64>, Refusal> {
    let mut held = BTreeMap::new();
    for b in boxes.values() {
        let ClassSpec::ShardsImage(image) = &b.spec.class else {
            continue;
        };
        let running = match &b.init {
            Some(init) => b.started && init.exit()?.is_none(),
            None => false,
        };
        if running {
            for v in &image.volumes {
                held.insert(v.name.clone(), v.bound.get());
            }
        }
    }
    Ok(held)
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
            exit = self
                .0
                .ended
                .wait_timeout(exit, left)
                .map_err(wait_refused)?
                .0;
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

/// The harness the simulator's image runs of its own, at `/harness/hex-sim` (Shards' D270 slice 4's paths), beside the
/// members a box's spec says its caller drives, each of which the simulator takes as its image's too.
pub const SIM_HARNESS: &str = "hex-sim";

/// The one component the simulated node's Shards store holds (Shards' D270 slice 4): a harness's OSI artifact, by its
/// manifest digest, a sha256's 64 hex digits, the fixture's own.
pub const SIM_COMPONENT: &str =
    "sha256:c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0";

/// The member a simulated image runs at path `at`, `/agents/NAME` or `/harness/NAME` as Shards names a member's
/// directory (D270 slice 4): its own harness ([`SIM_HARNESS`]), or a member `image`'s spec says its caller drives.
fn member_at(image: &ShardsImage, at: &str) -> Option<Member> {
    let (kind, name) = match at.strip_prefix("/agents/") {
        Some(name) => (MemberKind::Agent, name),
        None => (MemberKind::Harness, at.strip_prefix("/harness/")?),
    };
    let member = Member {
        kind,
        name: name.to_owned(),
    };
    if (kind == MemberKind::Harness && name == SIM_HARNESS)
        || image.members.iter().any(|m| m.member == member)
    {
        Some(member)
    } else {
        None
    }
}

/// A simulated run's components as its run host lists them: each path's, in the paths' order.
fn listed(components: &BTreeMap<String, (Option<String>, Option<String>)>) -> Vec<ComponentAt> {
    components
        .iter()
        .map(|(at, (desired, held))| ComponentAt {
            at: at.clone(),
            desired: desired.clone(),
            held: held.clone(),
        })
        .collect()
}

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
            band: 0,
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
            band: 0,
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
    /// The simulated node's one component, for the image's own harness ([`SIM_HARNESS`], [`SIM_COMPONENT`]).
    fn component(&self) -> Option<crate::conformance::Component> {
        match self {
            Self::ShardsImage => Some(crate::conformance::Component {
                at: format!("/harness/{SIM_HARNESS}"),
                digest: SIM_COMPONENT.to_owned(),
            }),
            Self::Process | Self::Bundle | Self::Spec | Self::Firecracker | Self::Shards => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;

    /// Joint plan step 17, AC-T7.4.1: do take a running sim image box's member that its spec names; expect its
    /// streams once, what it is given echoed back, and its end 0 at its input's end. Expect a second take refused,
    /// a member its spec doesn't name refused, a take before the start NotRunning, and a process box's refused as
    /// having no members.
    #[test]
    fn a_sim_image_boxs_member_echoes_once_taken_and_ends_at_its_inputs_end() {
        use std::io::{Read as _, Write as _};
        let runtime = SimRuntime::new();
        let claude = Member {
            kind: crate::MemberKind::Harness,
            name: "claude".to_owned(),
        };
        let id = BoxId::new("sim-members").unwrap();
        runtime
            .create(BoxSpec {
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::ShardsImage(Box::new(ShardsImage {
                    members: vec![crate::ImageMember::new(claude.clone())],
                    ..ShardsImage::new(SIM_IMAGE)
                })),
                owner: None,
                devices: Vec::new(),
                allocation: Some(sim_vm_allocation().unwrap()),
                route: None,
                band: 0,
            })
            .unwrap();
        assert!(matches!(
            runtime.member(&id, &claude),
            Err(Refusal::NotRunning(_, "member"))
        ));
        runtime.start(&id).unwrap();
        let mut proc = runtime.member(&id, &claude).unwrap();
        let given = proc.streams().unwrap();
        let mut to = crate::OwnedStream::from(given.stdin.unwrap());
        let mut from = crate::OwnedStream::from(given.stdout.unwrap());
        to.write_all(b"hello").unwrap();
        drop(to);
        let mut echoed = String::new();
        from.read_to_string(&mut echoed).unwrap();
        assert_eq!(echoed, "hello");
        assert_eq!(
            proc.wait_for(Duration::from_secs(60)).unwrap(),
            Some(Exit::Code(0))
        );
        assert!(matches!(
            runtime.member(&id, &claude),
            Err(Refusal::Invalid { what: "member", .. })
        ));
        let helper = Member {
            kind: crate::MemberKind::Agent,
            name: "helper".to_owned(),
        };
        assert!(matches!(
            runtime.member(&id, &helper),
            Err(Refusal::Invalid { what: "member", .. })
        ));
        let plain = BoxId::new("sim-process").unwrap();
        runtime
            .create(BoxSpec {
                id: plain.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                devices: Vec::new(),
                allocation: None,
                route: None,
                band: 0,
            })
            .unwrap();
        assert!(matches!(
            runtime.member(&plain, &claude),
            Err(Refusal::Invalid { what: "member", .. })
        ));
    }

    /// Kiln's P7.1b (Shards' D243, `Runtime::exec_member`; AC-T7.4.1): do run `say` in a running sim image box's
    /// member its spec names, its stdout asked for; expect its words on its stdout and its end 0. Expect a program
    /// naming a user refused Invalid naming the program, a member its spec names not Invalid naming the member, an exec
    /// naming no member Invalid naming exec, one before the box's start and one in a process box refused as the class
    /// refuses them.
    #[test]
    fn a_sim_image_boxs_member_runs_a_program_named_for_it() {
        use std::io::Read as _;
        let runtime = SimRuntime::new();
        let claude = Member {
            kind: crate::MemberKind::Harness,
            name: "claude".to_owned(),
        };
        let id = BoxId::new("sim-exec").unwrap();
        runtime
            .create(BoxSpec {
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::ShardsImage(Box::new(ShardsImage {
                    members: vec![crate::ImageMember::new(claude.clone())],
                    ..ShardsImage::new(SIM_IMAGE)
                })),
                owner: None,
                devices: Vec::new(),
                allocation: Some(sim_vm_allocation().unwrap()),
                route: None,
                band: 0,
            })
            .unwrap();
        let out = || Stdio {
            ask: crate::Ask {
                stdout: true,
                ..crate::Ask::default()
            },
            ..Stdio::null()
        };
        assert!(matches!(
            runtime.exec_member(&id, &claude, Program::of(&["say", "hi"]), out()),
            Err(Refusal::NotRunning(_, "exec in a member"))
        ));
        runtime.start(&id).unwrap();
        let mut proc = runtime
            .exec_member(&id, &claude, Program::of(&["say", "hi"]), out())
            .unwrap();
        let mut from = crate::OwnedStream::from(proc.streams().unwrap().stdout.unwrap());
        let mut said = String::new();
        from.read_to_string(&mut said).unwrap();
        assert_eq!(said, "hi\n");
        assert_eq!(
            proc.wait_for(Duration::from_secs(60)).unwrap(),
            Some(Exit::Code(0))
        );
        let as_root = Program {
            user: Some(crate::User::default()),
            ..Program::of(&["say", "hi"])
        };
        assert!(matches!(
            runtime.exec_member(&id, &claude, as_root, Stdio::null()),
            Err(Refusal::Invalid {
                what: "program",
                ..
            })
        ));
        let helper = Member {
            kind: crate::MemberKind::Agent,
            name: "helper".to_owned(),
        };
        assert!(matches!(
            runtime.exec_member(&id, &helper, Program::of(&["say", "hi"]), Stdio::null()),
            Err(Refusal::Invalid { what: "member", .. })
        ));
        assert!(matches!(
            runtime.exec(&id, Program::of(&["say", "hi"]), Stdio::null()),
            Err(Refusal::Invalid { what: "exec", .. })
        ));
        let plain = BoxId::new("sim-exec-process").unwrap();
        runtime
            .create(BoxSpec {
                id: plain.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                devices: Vec::new(),
                allocation: None,
                route: None,
                band: 0,
            })
            .unwrap();
        assert!(matches!(
            runtime.exec_member(&plain, &claude, Program::of(&["say", "hi"]), Stdio::null()),
            Err(Refusal::Invalid { what: "member", .. })
        ));
    }

    /// Kiln plan P5.5b (AC-T7.4.1): do pause a running sim image box, then run `say` in its member and signal that
    /// member; resume it and run `say` again. Expect: while paused, both refused Pause(Paused) at once, each naming its
    /// call, as the class refuses them; once resumed, the program runs.
    #[test]
    fn a_paused_sim_image_boxs_member_runs_no_program_until_it_resumes() {
        let runtime = SimRuntime::new();
        let claude = Member {
            kind: crate::MemberKind::Harness,
            name: "claude".to_owned(),
        };
        let id = BoxId::new("sim-paused-exec").unwrap();
        runtime
            .create(BoxSpec {
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::ShardsImage(Box::new(ShardsImage {
                    members: vec![crate::ImageMember::new(claude.clone())],
                    ..ShardsImage::new(SIM_IMAGE)
                })),
                owner: None,
                devices: Vec::new(),
                allocation: Some(sim_vm_allocation().unwrap()),
                route: None,
                band: 0,
            })
            .unwrap();
        runtime.start(&id).unwrap();
        runtime.pause(&id).unwrap();
        let refused = |op: &'static str| Refusal::Pause {
            id: id.clone(),
            op,
            cause: PauseCause::Paused,
        };
        assert_eq!(
            runtime
                .exec_member(&id, &claude, Program::of(&["say", "hi"]), Stdio::null())
                .err(),
            Some(refused("exec in a member"))
        );
        assert_eq!(
            runtime.signal_member(&id, &claude, Signal::TERM),
            Err(refused("signal a member"))
        );
        runtime.resume(&id).unwrap();
        assert!(
            runtime
                .exec_member(&id, &claude, Program::of(&["say", "hi"]), Stdio::null())
                .is_ok()
        );
    }

    /// Shards' D270 slices 3b and 4 (AC-T7.4.1): a sim image box's member given a component now ends the program its
    /// caller took, as TERM ends a program with no handler, and starts its next generation, whose streams its caller
    /// takes anew; a component of an agent's at a harness's path is refused Kind, and one of a box of another class
    /// Class. Do: take a driven harness, swap the sim's component in at it now, take it again; swap at a driven agent's
    /// path; swap in a process box. Expect: its first program ended Signal(15), generation 1 started, its streams given
    /// again; Swap(Kind); Swap(Class(Process)).
    #[test]
    fn d270_a_sim_members_swap_now_ends_its_program_and_its_next_is_taken_anew() {
        let runtime = SimRuntime::new();
        let claude = Member {
            kind: MemberKind::Harness,
            name: "claude".to_owned(),
        };
        let helper = Member {
            kind: MemberKind::Agent,
            name: "helper".to_owned(),
        };
        let id = BoxId::new("sim-swaps").unwrap();
        runtime
            .create(BoxSpec {
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::ShardsImage(Box::new(ShardsImage {
                    members: vec![
                        crate::ImageMember::new(claude.clone()),
                        crate::ImageMember::new(helper),
                    ],
                    ..ShardsImage::new(SIM_IMAGE)
                })),
                owner: None,
                devices: Vec::new(),
                allocation: Some(sim_vm_allocation().unwrap()),
                route: None,
                band: 0,
            })
            .unwrap();
        runtime.start(&id).unwrap();
        let mut first = runtime.member(&id, &claude).unwrap();
        let swapped = runtime
            .swap(&id, "/harness/claude", SIM_COMPONENT, SwapWhen::Now)
            .unwrap();
        assert_eq!(swapped.started, Some(1));
        assert_eq!(first.try_wait().unwrap(), Some(Exit::Signal(15)));
        let mut next = runtime.member(&id, &claude).unwrap();
        assert!(next.streams().unwrap().stdin.is_some());
        assert!(matches!(
            runtime.swap(&id, "/agents/helper", SIM_COMPONENT, SwapWhen::NextStart),
            Err(Refusal::Swap {
                cause: SwapCause::Kind,
                ..
            })
        ));
        let plain = BoxId::new("sim-swaps-process").unwrap();
        runtime
            .create(SimFixture::Process.spec(plain.clone(), Bounds::default()))
            .unwrap();
        assert!(matches!(
            runtime.components(&plain),
            Err(Refusal::Swap {
                cause: SwapCause::Class(Class::Process),
                ..
            })
        ));
    }

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

    /// Kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1 (CLAUDE.md §2): do keep as many pools as the simulator may hold
    /// boxes, then one more. Expect: the one more refused `Full`, one kept already kept again, and a pool let go making
    /// room for the one more.
    #[test]
    fn p3_1_the_simulator_refuses_a_pool_past_its_box_bound() {
        let rt = SimRuntime::new();
        let size = sim_vm_allocation().unwrap().vm_size().unwrap();
        let pool = |n: usize| WarmPool {
            image: format!("hex-sim-pool-{n}@sha256:{}", "0".repeat(64)),
            size,
        };
        for n in 0..SIM_BOXES {
            rt.keep_pool(&pool(n)).unwrap();
        }
        assert_eq!(
            rt.keep_pool(&pool(SIM_BOXES)),
            Err(Refusal::Full(SIM_BOXES))
        );
        assert_eq!(rt.keep_pool(&pool(0)), Ok(()), "one kept already");
        assert_eq!(rt.pools().unwrap().len(), SIM_BOXES);
        rt.let_go_pool(&pool(0)).unwrap();
        assert_eq!(rt.keep_pool(&pool(SIM_BOXES)), Ok(()));
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

    /// Kiln plan 01a1dd2e…, P6.2 (AC-T7.4.1): the simulator holds an image VM's volumes as the image class does. Do
    /// create a VM whose volume's bound passes its admitted disk; start one given a 1 GiB volume and list the home's
    /// volumes; clear it while that VM runs; start a second VM asking it with another bound; end the first and clear
    /// the volume; clear it again. Expect the first refused Invalid with no box made; the volume listed with its bound
    /// once its VM starts; its clearing refused `Volume { InUse }` while the VM runs; the second's start refused
    /// `Volume { Bound }`, that box ended; the volume cleared once the first VM has ended, none listed then, and the
    /// next clearing refused `Volume { Unknown }`.
    #[test]
    fn p6_2_the_simulator_holds_an_image_vms_volumes_as_the_class_does() {
        let rt = SimRuntime::new();
        let given = |name: &str, disk: u64, bound: u64| {
            let mut spec =
                SimFixture::ShardsImage.spec(BoxId::new(name).unwrap(), Bounds::default());
            let ClassSpec::ShardsImage(image) = &mut spec.class else {
                panic!("the image fixture's spec is no image VM's");
            };
            image.volumes = vec![crate::ImageVolume {
                name: "kiln-agent-cache".to_owned(),
                bound: std::num::NonZeroU64::new(bound).unwrap(),
                at: "/volumes/cache".to_owned(),
                members: vec![Member {
                    kind: crate::MemberKind::Harness,
                    name: "claude".to_owned(),
                }],
                read_only: false,
            }];
            let mut cost = sim_vm_allocation().unwrap().cost;
            cost.ceiling.disk_bytes = disk;
            let mut workload = sim_vm_allocation().unwrap().resources;
            workload.disk_bytes = disk;
            spec.allocation = Some(cost.allocate(workload).unwrap());
            spec
        };
        assert!(matches!(
            rt.create(given("past", 1 << 20, 1 << 30)),
            Err(Refusal::Invalid {
                what: "shards image",
                ..
            })
        ));
        assert!(rt.list().unwrap().is_empty());
        let first = rt.create(given("first", 1 << 30, 1 << 30)).unwrap().id;
        rt.start(&first).unwrap();
        let listed: Vec<_> = rt
            .volumes()
            .unwrap()
            .into_iter()
            .map(|v| (v.name, v.bound, v.anonymous))
            .collect();
        assert_eq!(
            listed,
            [("kiln-agent-cache".to_owned(), Some(1 << 30), false)]
        );
        assert!(matches!(
            rt.clear_volume("kiln-agent-cache"),
            Err(Refusal::Volume {
                cause: VolumeCause::InUse,
                ..
            })
        ));
        let second = rt.create(given("second", 1 << 30, 1 << 29)).unwrap().id;
        assert!(matches!(
            rt.start(&second),
            Err(Refusal::Volume {
                cause: VolumeCause::Bound,
                ..
            })
        ));
        assert_eq!(rt.inspect(&second).unwrap().exit, Some(Exit::Unknown));
        rt.signal(&first, Signal::KILL).unwrap();
        assert_eq!(rt.clear_volume("kiln-agent-cache"), Ok(()));
        assert!(rt.volumes().unwrap().is_empty());
        assert!(matches!(
            rt.clear_volume("kiln-agent-cache"),
            Err(Refusal::Volume {
                cause: VolumeCause::Unknown,
                ..
            })
        ));
    }

    /// Shards' D286 (kiln goal 01a1ec34…): the simulator asks an image VM's shares another server serves as the class
    /// does. Do create one given a served share on a simulator given no server; then, on one given a server serving
    /// `project`, create and start one served `project` and one served a source the server serves none of. Expect the
    /// first refused `Unconfigured` with no box made; the second started, its stream asked once; the third created, its
    /// start refused `Invalid` naming the source, that box left created, nothing of its run begun.
    #[test]
    fn d286_the_simulator_asks_an_image_vms_served_shares_as_the_class_does() {
        let served = |name: &str, source: &str| {
            let mut spec =
                SimFixture::ShardsImage.spec(BoxId::new(name).unwrap(), Bounds::default());
            let ClassSpec::ShardsImage(image) = &mut spec.class else {
                panic!("the image fixture's spec is no image VM's");
            };
            image.served = vec![crate::ServedMount {
                source: source.to_owned(),
                destination: "/srv/project".to_owned(),
                read_only: false,
                members: Vec::new(),
            }];
            spec
        };
        let alone = SimRuntime::new();
        assert!(matches!(
            alone.create(served("unserved", "project")),
            Err(Refusal::Unconfigured { .. })
        ));
        assert!(alone.list().unwrap().is_empty());

        let servers = crate::serve::sim::SimServers::new();
        servers.serve("project").unwrap();
        let rt = SimRuntime::new().with_servers(Arc::new(servers.clone()));
        let first = rt.create(served("first", "project")).unwrap().id;
        rt.start(&first).unwrap();
        assert_eq!(
            servers.made().unwrap(),
            1,
            "its stream asked once as it booted"
        );
        let other = rt.create(served("other", "elsewhere")).unwrap().id;
        assert!(matches!(
            rt.start(&other),
            Err(Refusal::Invalid { why, .. }) if why.contains("elsewhere")
        ));
        let info = rt.inspect(&other).unwrap();
        assert_eq!((info.state, info.exit), (BoxState::Created, None));
        assert_eq!(
            servers.made().unwrap(),
            1,
            "no stream made for a source it serves none of"
        );
    }
}
