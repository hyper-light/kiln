//! The Shards class on Agentfile images (joint plan step 12; docs/design/shards-image-class.md): each box
//! a microVM Shards boots its image in under shards-init (Shards' D231, `shards_api::image`), its members
//! started as the image's Agentfile declares them. The box's record is the guest's Books' (the node's
//! Store); its run is this process's, for a run ends with its owner in D231's first version.
//!
//! What the record and the run say of a box ([`State`], R04): Created (`Creating`), Booting and Running
//! (`Ready`, a boot or run of this process), Stopping, Ended (`Ready` with its exit), and Held (`Ready` or
//! `Deleting` with no exit and no run of this process: an earlier node process's run, which D231 can
//! neither take back nor prove gone, so its claim is kept and every call refused
//! `Awaits(AttachAfterCrash)` until a reboot of the host proves it ended).
//!
//! D231's boot waits at most the patience it is given, and its stop for its run's end: each runs on a thread
//! of its own and answers in a [`Slot`]. A caller waits at most its learned patience ([`ImageWaits`]), which
//! a boot is given as its own, and is refused `Lifecycle { Pending }` past it, the answer recorded when it
//! comes. No lock is held over a Shards call, and a run is let go of only once its stop has returned, since
//! D231 stops a run it drops.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hex_api::{
    BoxId, BoxInfo, BoxSpec, BoxState, ClassSpec, Exit, Given, GuestTime, ImageMember,
    LifecycleCause, Member, MemberKind, Proc, Refusal, ShardsImage, Signal, Upstream, Usage,
    VmSize,
};
use hex_core::patience::Patience;
use hex_core::record::{BootSession, BoxRecord, Phase};
use shards_api::image::{
    Channel, ImageGuest, ImageRefusal, ImageSpec, Images, MemberEnd, MemberName, MemberSpec,
    RunEnd, Secret, Stats, StatsScope,
};

use super::{GuestRuntime, poisoned};
use crate::devices;

/// The waits an image box's calls make, each learned (decision 9) and seeded from this host class's
/// slowest recorded answer (bench/native/guest/records.tsv): Shards' boot of an image to its members'
/// start, a stats answer, and a stop to its run's end.
pub struct ImageWaits {
    boot: Patience,
    stats: Patience,
    stop: Patience,
}

impl ImageWaits {
    /// Seeded from records.tsv ([`hex_client::guest::shards::recorded`]): `image_boot`, `image_stats` and
    /// `image_stop`. A measure no host class records is refused, naming the record to take, but where the
    /// waits census runs ([`hex_core::patience::measures_all`]): it arms no wait, so there each measures, and
    /// the census takes its first record (hex-cli's `image_real`; docs/design/shards-image-class.md §6).
    pub fn recorded() -> Result<Self, Refusal> {
        Self::from_records(
            hex_client::guest::shards::recorded,
            hex_core::patience::measures_all(),
        )
    }

    /// Each wait seeded from `record`'s slowest answer for its measure, or, where it has none, measuring
    /// while the `census` runs, else refused as `record` refuses it.
    fn from_records(
        record: impl Fn(&str) -> Result<Duration, Refusal>,
        census: bool,
    ) -> Result<Self, Refusal> {
        let wait = |name: &'static str, measure: &str| match record(measure) {
            Ok(slowest) => Ok(Patience::seeded(name, slowest)),
            Err(_) if census => Ok(Patience::measuring(name)),
            Err(why) => Err(why),
        };
        Ok(Self {
            boot: wait("hex.image.boot", "image_boot")?,
            stats: wait("hex.image.stats", "image_stats")?,
            stop: wait("hex.image.stop", "image_stop")?,
        })
    }
}

/// Shards' image boot as the guest runtime holds it: the booter or why this node has none, its waits or
/// why it has none, this process's runs, and the host's boot session, which proves a run of an earlier
/// boot ended.
pub(super) struct ImageBoot {
    images: Result<Arc<dyn Images>, Refusal>,
    waits: Result<ImageWaits, Refusal>,
    /// One for each box with a boot, run or stop of this process; at most a box's every own, so bounded
    /// by the node's boxes ([`crate::MAX_BOXES`]).
    runs: Mutex<BTreeMap<BoxId, Run>>,
    session: OnceLock<Option<BootSession>>,
}

/// A run of this process.
enum Run {
    /// Its boot, on a thread of its own, answers here.
    Booting(Arc<Slot<Result<Box<dyn ImageGuest>, ImageRefusal>>>),
    /// Its run, held. D231 stops a run it drops, so it leaves only through its stop.
    Running(Arc<dyn ImageGuest>),
    /// Its stop, on a thread of its own, answers here: the run's end, once its VM is gone.
    Stopping(Arc<Slot<RunEnd>>),
    /// Its end, known, until its record says it.
    Ended(Exit),
}

/// One answer a thread of its own gives: a boot's or a stop's.
struct Slot<T> {
    answer: hex_wait::Mutex<Option<T>>,
    came: hex_wait::Condvar,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            answer: hex_wait::Mutex::new(None),
            came: hex_wait::Condvar::new(),
        }
    }

    fn put(&self, value: T) {
        // A poisoned lock leaves the answer untold: its waiter's patience ends the wait, typed.
        if let Ok(mut answer) = self.answer.lock() {
            *answer = Some(value);
            self.came.notify_all();
        }
    }

    /// Whether the answer came within `wait` (`None`: a measuring patience, until it comes).
    fn came_within(&self, wait: Option<Duration>) -> Result<bool, Refusal> {
        let answer = self.answer.lock().map_err(poisoned)?;
        let answer = match wait {
            Some(wait) => {
                self.came
                    .wait_timeout_while(answer, wait, |a| a.is_none())
                    .map_err(wait_refused)?
                    .0
            }
            None => self
                .came
                .wait_while(answer, |a| a.is_none())
                .map_err(wait_refused)?,
        };
        Ok(answer.is_some())
    }

    fn take(&self) -> Result<Option<T>, Refusal> {
        Ok(self.answer.lock().map_err(poisoned)?.take())
    }
}

/// A wait for a slot's answer that couldn't wait: its lock poisoned, or the OS refused it (macOS's kqueue).
fn wait_refused(why: hex_wait::Failed) -> Refusal {
    match why {
        hex_wait::Failed::Poisoned => {
            Refusal::Broken("the guest runtime's lock is poisoned".to_owned())
        }
        hex_wait::Failed::Os(why) => Refusal::Os {
            op: "waiting for a Shards image's answer",
            why: why.to_string(),
        },
    }
}

/// A Shards image box, as its record and this process's run say.
enum State {
    Created,
    Booting,
    Running(Arc<dyn ImageGuest>),
    Stopping(Arc<Slot<RunEnd>>),
    Ended(Exit),
    Held,
}

/// A run's end as the box's exit: `docker run`'s status as Shards reports it (128 and a signal's number
/// where one ended it, kept as the number it is), KILL for a stop, and an unknown end for a failure.
fn exit_of(end: &RunEnd) -> Exit {
    match end {
        RunEnd::Exited(status) => i32::try_from(*status).map_or(Exit::Unknown, Exit::Code),
        RunEnd::Stopped => Exit::Signal(Signal::KILL.number()),
        RunEnd::Failed(_) => Exit::Unknown,
    }
}

/// D231's refusal, typed as the call it answers.
fn refused(id: &BoxId, op: &'static str, why: ImageRefusal) -> Refusal {
    match why {
        ImageRefusal::NoBackend { os, arch } => Refusal::Platform {
            what: "Shards image VMs",
            platform: os,
            why: format!("shards boots no guest on {os} {arch}"),
        },
        ImageRefusal::Spec { what, why } => Refusal::Invalid {
            what: "shards image",
            why: format!("{what}: {why}"),
        },
        ImageRefusal::Image(why) => Refusal::Invalid {
            what: "shards image",
            why: format!("the image: {why}"),
        },
        ImageRefusal::Widening(why) => Refusal::Invalid {
            what: "shards image",
            why: format!("past the image's grants: {why}"),
        },
        ImageRefusal::Boot(why) => Refusal::Os {
            op,
            why: format!("the run did not start: {why}"),
        },
        ImageRefusal::Unsupported { what, why } => Refusal::Platform {
            what,
            platform: std::env::consts::OS,
            why,
        },
        ImageRefusal::Ended(_) => match id.try_clone() {
            Ok(id) => Refusal::NotRunning(id, op),
            Err(why) => why,
        },
        ImageRefusal::Host(why) => Refusal::Os {
            op,
            why: format!("the run host: {why}"),
        },
        ImageRefusal::Os { what, kind } => Refusal::Os {
            op: what,
            why: format!("{kind:?}"),
        },
        // Only a plug or an unplug (D235) is refused so, and this class makes neither.
        ImageRefusal::Plug(cause, why) => {
            Refusal::Broken(format!("Shards refused {op} as a plug ({cause:?}): {why}"))
        }
    }
}

/// The box's VM size, held to what Shards boots: memory even and at least its least on this host
/// (`shards_api::min_memory_mib`, shards-vmm's own floor), refused naming the allocation.
fn vm_size(spec: &BoxSpec) -> Result<VmSize, Refusal> {
    let size = spec
        .allocation
        .ok_or_else(|| Refusal::Invalid {
            what: "shards image",
            why: "allocation is missing: a Shards image VM is sized by its admitted allocation"
                .to_owned(),
        })?
        .vm_size()?;
    let least = shards_api::min_memory_mib(std::env::consts::ARCH);
    if size.memory_mib < least || !size.memory_mib.is_multiple_of(2) {
        return Err(Refusal::Invalid {
            what: "shards image",
            why: format!(
                "allocation: {} MiB of guest memory; Shards boots an even number of MiB, at least {least}",
                size.memory_mib
            ),
        });
    }
    Ok(size)
}

/// What a running VM's usage is, from its run's STATS: CPU, memory, processes and I/O. Guest time per
/// vCPU awaits Shards ([`Upstream::GuestCpuTimePerVcpu`]), and STATS has no memory peak, OOM kills or
/// refused forks (docs/design/shards-image-class.md §4).
fn usage_of(stats: &Stats) -> Result<Usage, Refusal> {
    let read = |value: Option<u64>, what: &'static str| {
        value.ok_or_else(|| Refusal::Os {
            op: "reading a Shards image VM's stats",
            why: format!("its init said no {what}"),
        })
    };
    Ok(Usage {
        memory_bytes: read(stats.memory_bytes, "memory")?,
        memory_peak_bytes: 0,
        oom_kills: 0,
        pids: read(stats.pids, "pids")?,
        pids_refused: 0,
        cpu_usec: read(stats.cpu_ns, "cpu")?
            .checked_div(1_000)
            .ok_or_else(|| Refusal::Broken("CPU nanoseconds to microseconds".to_owned()))?,
        io_read_bytes: read(stats.read_bytes, "read")?,
        io_write_bytes: read(stats.write_bytes, "write")?,
        guest: Some(GuestTime::Awaits(Upstream::GuestCpuTimePerVcpu)),
    })
}

fn info(spec: &BoxSpec, state: BoxState, exit: Option<Exit>) -> Result<BoxInfo, Refusal> {
    Ok(BoxInfo {
        published: Vec::new(),
        id: spec.id.try_clone()?,
        class: spec.class.class(),
        state,
        bounds: spec.bounds.clone(),
        allocation: spec.allocation,
        adopted: false,
        membership: None,
        exit,
        abandoned: 0,
        route: spec.route,
        // Every grant as the record holds it (joint plan step 15, docs/design/network-grants.md N6).
        grants: match &spec.class {
            ClassSpec::ShardsImage(image) => image.grants.clone(),
            _ => Vec::new(),
        },
        last_resort: None,
    })
}

/// What Shards' image boot gives a member its caller drives (joint plan step 17): its stdin a pipe the caller
/// writes (`MemberSpec::stdin`), its variables and working directory as the box's spec says them, no file and no
/// terminal. Its stdout and stderr are D231's pipes for every member that runs.
fn member_spec(m: &ImageMember) -> MemberSpec {
    MemberSpec {
        member: member_name(&m.member),
        env: m
            .env
            .iter()
            .map(|(k, v)| (k.clone(), Secret::from(v.as_str())))
            .collect(),
        files: Vec::new(),
        workdir: m.workdir.clone(),
        stdin: true,
        terminal: false,
    }
}

/// A member as Shards' image boot names it.
fn member_name(m: &Member) -> MemberName {
    match m.kind {
        MemberKind::Harness => MemberName::harness(&m.name),
        MemberKind::Agent => MemberName::agent(&m.name),
    }
}

/// A member its caller drives, as a program (joint plan step 17): no process of this host, its end D231's
/// `wait_member` (the run's end, where it ended with its run), and the caller's ends of its streams, given once.
/// A signal to the member alone is the agent-control endpoint's (step 11), so its own kill awaits it; the box's
/// stop (`signal(KILL)`, `delete`) ends it with its VM.
struct MemberProc {
    id: BoxId,
    guest: Arc<dyn ImageGuest>,
    name: MemberName,
    given: Option<Given>,
    ended: Option<Exit>,
}

impl Proc for MemberProc {
    /// D231 names no member's pid in its guest: none is told.
    fn pid(&self) -> u32 {
        0
    }

    fn on_this_host(&self) -> bool {
        false
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.wait_for(Duration::ZERO)
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        if let Some(exit) = self.ended {
            return Ok(Some(exit));
        }
        let exit = match self.guest.wait_member(&self.name, timeout) {
            Ok(None) => return Ok(None),
            Ok(Some(MemberEnd::Exited(status))) => {
                i32::try_from(status).map_or(Exit::Unknown, Exit::Code)
            }
            // Its run ended first, and the member with it: the run's end is its own.
            Ok(Some(MemberEnd::WithRun)) => match self.guest.wait_for(Duration::ZERO) {
                Ok(Some(end)) | Err(ImageRefusal::Ended(end)) => exit_of(&end),
                Ok(None) | Err(_) => Exit::Unknown,
            },
            Err(ImageRefusal::Ended(end)) => exit_of(&end),
            Err(why) => return Err(refused(&self.id, "waiting for a member's end", why)),
        };
        self.ended = Some(exit);
        Ok(Some(exit))
    }

    fn kill(&mut self) -> Result<(), Refusal> {
        Err(Refusal::Awaits {
            needs: Upstream::MemberSignal,
        })
    }

    fn streams(&mut self) -> Result<Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }
}

fn pending(id: &BoxId) -> Result<Refusal, Refusal> {
    Ok(Refusal::Lifecycle {
        id: id.try_clone()?,
        cause: LifecycleCause::Pending,
    })
}

fn held() -> Refusal {
    Refusal::Awaits {
        needs: Upstream::AttachAfterCrash,
    }
}

impl ImageBoot {
    fn images(&self) -> Result<&Arc<dyn Images>, Refusal> {
        self.images.as_ref().map_err(Clone::clone)
    }

    fn waits(&self) -> Result<&ImageWaits, Refusal> {
        self.waits.as_ref().map_err(Clone::clone)
    }

    fn runs(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<BoxId, Run>>, Refusal> {
        self.runs.lock().map_err(poisoned)
    }

    /// Lets go of a run whose end is known, alone: a run still held goes only through its stop, since
    /// D231 stops a run it drops and that stop would wait on this lock's holder.
    fn forget(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut runs = self.runs()?;
        if matches!(runs.get(id), Some(Run::Ended(_))) {
            runs.remove(id);
        }
        Ok(())
    }

    fn insert(&self, id: &BoxId, run: Run) -> Result<(), Refusal> {
        let mut runs = self.runs()?;
        if !runs.contains_key(id) && runs.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        runs.insert(id.try_clone()?, run);
        Ok(())
    }

    /// Whether a record the host's boot session `record` names was written in an earlier boot of the
    /// host, whose end ended everything that ran of it (T-T7.1.2). Unknown either way proves nothing.
    fn earlier_boot(&self, record: &BoxRecord) -> bool {
        matches!(
            (&record.boot, self.session.get()),
            (Some(written), Some(Some(now))) if written != now
        )
    }

    /// Stops `guest` on a thread of its own: D231's stop returns once its run's end is told, its VM gone;
    /// the guest goes with the thread, and its run host then exits on its own.
    fn stop(&self, guest: Arc<dyn ImageGuest>) -> Result<Arc<Slot<RunEnd>>, Refusal> {
        let slot = Arc::new(Slot::new());
        let answer = Arc::clone(&slot);
        std::thread::Builder::new()
            .name("hex-image-stop".to_owned())
            .spawn(move || {
                let stopped = guest.stop();
                let end = match (stopped, guest.wait_for(Duration::ZERO)) {
                    (_, Ok(Some(end))) => end,
                    (Err(why), _) | (Ok(()), Err(why)) => RunEnd::Failed(why.to_string()),
                    (Ok(()), Ok(None)) => {
                        RunEnd::Failed("its stop returned with no end of its run".to_owned())
                    }
                };
                answer.put(end);
            })
            .map_err(|e| Refusal::os("starting a Shards image VM's stop", e))?;
        Ok(slot)
    }
}

/// Shards' image boot (D231), or why this node serves no image VM: every host runs it, its run host started
/// holding only its stdio (vendor/patches/shards 0001) and booting the guest the node's environment names, which
/// it inherits: `SHARDS_KERNEL` and `SHARDS_INIT`, read here once, as the node starts ([`guest_pair`];
/// docs/design/shards-image-class.md §1).
pub fn host_images() -> Result<Arc<dyn Images>, Refusal> {
    guest_pair(
        std::env::var_os("SHARDS_KERNEL").as_deref(),
        std::env::var_os("SHARDS_INIT").as_deref(),
    )?;
    Ok(Arc::new(shards_api::image::ImageShards::new()))
}

/// The guest a node's run hosts would boot, from its `SHARDS_KERNEL` and `SHARDS_INIT` (`kernel`, `init`) as D231's
/// run host reads them (Shards 66113379, api_run.rs `as_run`): each none or empty, or a UTF-8 absolute path, and both
/// or neither, as the daemon refuses one alone ("SHARDS_KERNEL and SHARDS_INIT go together"). A value the run host
/// would drop without a word (relative, or not UTF-8) is refused too: its VMs would boot Shards' own guest instead,
/// or be refused as the other alone. Refused `Platform` naming both variables (ROOT, 2026-11-12).
fn guest_pair(kernel: Option<&OsStr>, init: Option<&OsStr>) -> Result<(), Refusal> {
    let refused = |why: String| Refusal::Platform {
        what: "Shards image VMs",
        platform: std::env::consts::OS,
        why: format!(
            "{why}: Shards' run host boots the guest SHARDS_KERNEL and SHARDS_INIT name together, each an \
             absolute path, or its own with neither set"
        ),
    };
    let given = |name: &str, value: Option<&OsStr>| match value {
        None => Ok(false),
        Some(value) if value.is_empty() => Ok(false),
        Some(value) if value.to_str().is_some() && Path::new(value).is_absolute() => Ok(true),
        Some(value) => Err(refused(format!(
            "{name} is {value:?}, which the run host drops, as it takes only a UTF-8 absolute path"
        ))),
    };
    match (given("SHARDS_KERNEL", kernel)?, given("SHARDS_INIT", init)?) {
        (true, false) => Err(refused(
            "SHARDS_KERNEL is set and SHARDS_INIT is not".to_owned(),
        )),
        (false, true) => Err(refused(
            "SHARDS_INIT is set and SHARDS_KERNEL is not".to_owned(),
        )),
        _ => Ok(()),
    }
}

impl GuestRuntime {
    /// Shards' image boot (D231) or why this node has none ([`host_images`]), and the waits its calls
    /// make or why there are none: a create is refused with either refusal, so no box exists that this
    /// node couldn't boot or a wait of its own couldn't bound (docs/design/shards-image-class.md §6).
    pub fn with_images(
        mut self,
        images: Result<Arc<dyn Images>, Refusal>,
        waits: Result<ImageWaits, Refusal>,
    ) -> Self {
        self.image = Some(ImageBoot {
            images,
            waits,
            runs: Mutex::new(BTreeMap::new()),
            session: OnceLock::new(),
        });
        self
    }

    /// The host's boot session as the node's records carry it (T-T7.1.2), bound once beside the record
    /// authority: a held run whose record an earlier boot wrote ended with that boot.
    pub fn bind_boot_session(&self, session: Option<BootSession>) -> Result<(), Refusal> {
        let Some(image) = &self.image else {
            return Ok(());
        };
        match image.session.set(session) {
            Ok(()) => Ok(()),
            Err(again) if image.session.get() == Some(&again) => Ok(()),
            Err(_) => Err(Refusal::Broken(
                "the host's boot session cannot be rebound".to_owned(),
            )),
        }
    }

    fn image(&self) -> Result<&ImageBoot, Refusal> {
        self.image.as_ref().ok_or_else(|| Refusal::Platform {
            what: "Shards image VMs",
            platform: std::env::consts::OS,
            why: "this node configures no Shards image boot".to_owned(),
        })
    }

    fn image_books(&self) -> Result<&std::sync::Weak<dyn crate::devices::Books>, Refusal> {
        self.books.get().ok_or_else(|| {
            Refusal::Broken("a Shards image VM needs the guest's record authority".to_owned())
        })
    }

    /// The record of `id`, where it is a Shards image VM's.
    pub(super) fn image_record(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        Ok(self
            .recorded_box(id)?
            .filter(|record| matches!(record.spec.class, ClassSpec::ShardsImage(_))))
    }

    /// The image Shards boots for a box: its pinned image and command, its admitted size, its grants as
    /// `crate::grants::program` makes them (joint plan step 15: its publishes, and none of its image's own
    /// grants past the VM; D231 narrows, never widens), and the node's Shards binary and home, whose store
    /// holds the image.
    fn image_spec(&self, image: &ShardsImage, size: VmSize) -> Result<ImageSpec, Refusal> {
        let network = crate::grants::program(image)?;
        let mut env = Vec::new();
        if let Some(paths) = &self.native_paths {
            env.push((
                OsString::from("SHARDS_HOME"),
                paths.home().as_os_str().to_owned(),
            ));
            env.push((
                OsString::from("SHARDS_CACHE"),
                paths.cache().as_os_str().to_owned(),
            ));
        }
        Ok(ImageSpec {
            image: image.image.clone(),
            vcpus: size.vcpus,
            memory_mib: size.memory_mib,
            command: image.command.clone(),
            members: image.members.iter().map(member_spec).collect(),
            publish: network.publish,
            egress: network.egress,
            shares: Vec::new(),
            // No nesting grant (D234): a harness VM's command is never a nested runtime's here.
            nesting: false,
            ready_by: None,
            binary: Some(self.config.binary.clone()),
            env,
        })
    }

    /// Writes the box's end, once, where its record lacks it.
    fn record_end(&self, id: &BoxId, exit: Exit) -> Result<(), Refusal> {
        devices::with_transaction(self.image_books()?, id, &[], |transaction| {
            let mut record = transaction
                .read(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if record.exit.is_some() {
                return Ok(());
            }
            record.exit = Some(exit);
            transaction.put(&record)
        })
    }

    /// The box `record` names, as its record and this process's run say: an answer that came from a
    /// boot or a stop taken, an end its run reached written, and a reboot of the host taken as the end
    /// of a run of an earlier boot (T-T7.1.2). Decided under the runs' lock, acted on after it.
    fn settle(&self, record: &BoxRecord) -> Result<State, Refusal> {
        /// What the runs' lock found.
        enum Next {
            Is(State),
            Running(Arc<dyn ImageGuest>),
            Ends(Exit),
            NoRun,
        }
        let id = &record.spec.id;
        let Some(image) = &self.image else {
            return Ok(match (record.exit, record.phase) {
                (Some(exit), _) => State::Ended(exit),
                (None, Phase::Creating) => State::Created,
                (None, _) => State::Held,
            });
        };
        if let Some(exit) = record.exit {
            image.forget(id)?;
            return Ok(State::Ended(exit));
        }
        if record.phase == Phase::Creating {
            return Ok(State::Created);
        }
        let next = {
            let mut runs = image.runs()?;
            match runs.get_mut(id) {
                None => Next::NoRun,
                Some(run) => match run {
                    Run::Booting(slot) => match slot.take()? {
                        None => Next::Is(State::Booting),
                        Some(Ok(guest)) => {
                            let guest: Arc<dyn ImageGuest> = Arc::from(guest);
                            *run = Run::Running(Arc::clone(&guest));
                            Next::Is(State::Running(guest))
                        }
                        // The run host refused or ended the run before it started: its own answer.
                        Some(Err(_)) => {
                            *run = Run::Ended(Exit::Unknown);
                            Next::Ends(Exit::Unknown)
                        }
                    },
                    Run::Running(guest) => Next::Running(Arc::clone(guest)),
                    Run::Stopping(slot) => match slot.take()? {
                        None => Next::Is(State::Stopping(Arc::clone(slot))),
                        Some(end) => {
                            let exit = exit_of(&end);
                            *run = Run::Ended(exit);
                            Next::Ends(exit)
                        }
                    },
                    Run::Ended(exit) => Next::Ends(*exit),
                },
            }
        };
        match next {
            Next::Is(state) => Ok(state),
            Next::Running(guest) => self.settle_running(image, id, guest),
            Next::Ends(exit) => {
                self.record_end(id, exit)?;
                image.forget(id)?;
                Ok(State::Ended(exit))
            }
            Next::NoRun if image.earlier_boot(record) => {
                self.record_end(id, Exit::Unknown)?;
                Ok(State::Ended(Exit::Unknown))
            }
            Next::NoRun => Ok(State::Held),
        }
    }

    /// A running VM whose run may have ended on its own: its host is then reaped through its stop, so no
    /// run is let go of before its host has exited.
    fn settle_running(
        &self,
        image: &ImageBoot,
        id: &BoxId,
        guest: Arc<dyn ImageGuest>,
    ) -> Result<State, Refusal> {
        let ended = guest
            .wait_for(Duration::ZERO)
            .map_err(|why| refused(id, "reading a Shards image VM's end", why))?;
        if ended.is_none() {
            return Ok(State::Running(guest));
        }
        let slot = image.stop(guest)?;
        let mut runs = image.runs()?;
        // Another call may have moved it on first: then its stop is that call's, and this one's thread
        // returns with the same end.
        if let Some(run @ Run::Running(_)) = runs.get_mut(id) {
            *run = Run::Stopping(Arc::clone(&slot));
        }
        Ok(State::Stopping(slot))
    }

    pub(super) fn create_image(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        spec.check()?;
        let image = self.image()?;
        image.images()?;
        image.waits()?;
        vm_size(&spec)?;
        // Grants Shards can't hold exactly are refused before anything is made (joint plan step 15).
        if let ClassSpec::ShardsImage(image) = &spec.class {
            crate::grants::program(image)?;
        }
        let books = self.image_books()?;
        let id = spec.id.try_clone()?;
        let made = info(&spec, BoxState::Created, None)?;
        devices::with_transaction(books, &id, &[], |transaction| {
            if transaction.read(&id)?.is_some() {
                return Err(Refusal::Exists(id.try_clone()?));
            }
            if transaction.records()?.len() >= crate::MAX_BOXES {
                return Err(Refusal::Full(crate::MAX_BOXES));
            }
            transaction.put(&BoxRecord::new(spec, Phase::Creating, Vec::new()))
        })?;
        Ok(made)
    }

    pub(super) fn start_image(&self, record: BoxRecord) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let image = self.image()?;
        let images = Arc::clone(image.images()?);
        let waits = image.waits()?;
        match self.settle(&record)? {
            State::Created => {}
            State::Booting | State::Running(_) | State::Stopping(_) => {
                return Err(Refusal::Invalid {
                    what: "start",
                    why: format!("box {id} has started already"),
                });
            }
            State::Ended(_) => return Err(Refusal::NotRunning(id, "start")),
            State::Held => return Err(held()),
        }
        let ClassSpec::ShardsImage(shards) = &record.spec.class else {
            return Err(Refusal::Broken(
                "a Shards image record of another class".to_owned(),
            ));
        };
        let spec = self.image_spec(shards, vm_size(&record.spec)?)?;
        // The boot's intent, durable before any effect: a crash from here on holds the box.
        devices::with_transaction(self.image_books()?, &id, &[], |transaction| {
            let mut current = transaction
                .read(&id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if current.phase != Phase::Creating
                || current.exit.is_some()
                || current.spec != record.spec
            {
                return Err(super::books::uncertain(&id)?);
            }
            current.phase = Phase::Ready;
            transaction.put(&current)
        })?;
        let slot = Arc::new(Slot::new());
        image.insert(&id, Run::Booting(Arc::clone(&slot)))?;
        let answer = Arc::clone(&slot);
        // The boot's learned bound, Shards' patience too: past it D231 kills its run host and refuses the
        // boot, so no box stays Booting. None while it measures, as the census's waits do: Shards waits for
        // the run's start or its host's end alone, as the caller does.
        let bound = waits.boot.deadline();
        let patience = bound.unwrap_or(Duration::MAX);
        let asked = Instant::now();
        let spawned = std::thread::Builder::new()
            .name("hex-image-boot".to_owned())
            .spawn(move || answer.put(images.boot(&spec, patience)));
        if let Err(why) = spawned {
            // No boot began: the box ends, its intent's record kept with its end.
            image.insert(&id, Run::Ended(Exit::Unknown))?;
            self.record_end(&id, Exit::Unknown)?;
            image.forget(&id)?;
            return Err(Refusal::os("starting a Shards image VM's boot", why));
        }
        if !slot.came_within(bound)? {
            if let Some(bound) = bound {
                waits.boot.ran_out(bound);
            }
            return Err(pending(&id)?);
        }
        let took = asked.elapsed();
        // The answer, taken here unless another call took it first.
        let answer = {
            let mut runs = image.runs()?;
            match runs.get_mut(&id) {
                Some(run @ Run::Booting(_)) => match slot.take()? {
                    Some(Ok(guest)) => {
                        let guest: Arc<dyn ImageGuest> = Arc::from(guest);
                        *run = Run::Running(guest);
                        Ok(())
                    }
                    Some(Err(why)) => {
                        *run = Run::Ended(Exit::Unknown);
                        Err(why)
                    }
                    None => Ok(()),
                },
                _ => Ok(()),
            }
        };
        match answer {
            Ok(()) => {
                waits.boot.answered(took);
                match self.settle(&record_now(self, &id)?)? {
                    // What the run published is held to the box's grants (joint plan step 15, N6): a run
                    // that breaks them is stopped, and the start says why.
                    State::Running(guest) => {
                        match crate::grants::bound(
                            &shards.grants,
                            guest.members(),
                            guest.published(),
                        ) {
                            Ok(_) => Ok(()),
                            Err(why) => match self.stop_image(&id, State::Running(guest)) {
                                Ok(_) => Err(why),
                                Err(cleanup) => Err(Refusal::Uncertain {
                                    id,
                                    refusal: Box::new(why),
                                    cleanup: Box::new(cleanup),
                                }),
                            },
                        }
                    }
                    State::Stopping(_) | State::Booting => Ok(()),
                    State::Ended(_) => Err(Refusal::NotRunning(id, "start")),
                    State::Created | State::Held => Err(super::books::uncertain(&id)?),
                }
            }
            Err(why) => {
                waits.boot.refused(took);
                self.record_end(&id, Exit::Unknown)?;
                image.forget(&id)?;
                Err(refused(&id, "booting a Shards image VM", why))
            }
        }
    }

    /// Stops a running VM: its stop's answer within the stop's patience, else `Lifecycle { Pending }`,
    /// the stop going on and its end recorded when a later call finds it.
    fn stop_image(&self, id: &BoxId, state: State) -> Result<Exit, Refusal> {
        let image = self.image()?;
        let waits = image.waits()?;
        let slot = match state {
            State::Running(guest) => {
                let slot = image.stop(guest)?;
                let mut runs = image.runs()?;
                if let Some(run) = runs.get_mut(id) {
                    *run = Run::Stopping(Arc::clone(&slot));
                }
                slot
            }
            State::Stopping(slot) => slot,
            State::Ended(exit) => return Ok(exit),
            State::Booting => return Err(pending(id)?),
            State::Held => return Err(held()),
            State::Created => return Err(Refusal::NotRunning(id.try_clone()?, "stop")),
        };
        let asked = Instant::now();
        let bound = waits.stop.deadline();
        if !slot.came_within(bound)? {
            if let Some(bound) = bound {
                waits.stop.ran_out(bound);
            }
            return Err(pending(id)?);
        }
        waits.stop.answered(asked.elapsed());
        match self.settle(&record_now(self, id)?)? {
            State::Ended(exit) => Ok(exit),
            _ => Err(super::books::uncertain(id)?),
        }
    }

    pub(super) fn signal_image(&self, record: BoxRecord, signal: Signal) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let state = self.settle(&record)?;
        match state {
            State::Held => Err(held()),
            State::Ended(_) => Err(Refusal::NotRunning(id, "signal")),
            _ if signal != Signal::KILL => Err(Refusal::Awaits {
                needs: Upstream::MemberSignal,
            }),
            // Ended before it booted: no run, and its end its record's.
            State::Created => devices::with_transaction(self.image_books()?, &id, &[], |t| {
                let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
                if current.phase != Phase::Creating || current.exit.is_some() {
                    return Err(super::books::uncertain(&id)?);
                }
                current.phase = Phase::Ready;
                current.exit = Some(Exit::Signal(Signal::KILL.number()));
                t.put(&current)
            }),
            state => self.stop_image(&id, state).map(|_| ()),
        }
    }

    pub(super) fn inspect_image(&self, record: BoxRecord) -> Result<BoxInfo, Refusal> {
        match self.settle(&record)? {
            State::Created | State::Booting => info(&record.spec, BoxState::Created, None),
            State::Running(guest) => {
                let mut shown = info(&record.spec, BoxState::Running, None)?;
                // Its publishes as bound (joint plan step 15, N6).
                if let ClassSpec::ShardsImage(shards) = &record.spec.class {
                    shown.published =
                        crate::grants::bound(&shards.grants, guest.members(), guest.published())?;
                }
                Ok(shown)
            }
            State::Stopping(_) => info(&record.spec, BoxState::Running, None),
            State::Ended(exit) => info(&record.spec, BoxState::Empty, Some(exit)),
            State::Held => Err(held()),
        }
    }

    pub(super) fn usage_image(&self, record: BoxRecord) -> Result<Usage, Refusal> {
        let id = record.spec.id.try_clone()?;
        let guest = match self.settle(&record)? {
            State::Running(guest) => guest,
            State::Held => return Err(held()),
            _ => return Err(Refusal::NotRunning(id, "usage")),
        };
        let waits = self.image()?.waits()?;
        let asked = Instant::now();
        let bound = waits.stats.deadline();
        match guest.stats(StatsScope::Run, bound.unwrap_or(Duration::MAX)) {
            Ok(stats) => {
                waits.stats.answered(asked.elapsed());
                usage_of(&stats)
            }
            Err(ImageRefusal::Ended(_)) => Err(Refusal::NotRunning(id, "usage")),
            // D231's answer to a stats ask its patience passed, as its client says it.
            Err(ImageRefusal::Unsupported { what: "stats", why }) => {
                if let Some(bound) = bound {
                    waits.stats.ran_out(bound);
                }
                Err(Refusal::Os {
                    op: "reading a Shards image VM's stats",
                    why,
                })
            }
            Err(why) => Err(refused(&id, "reading a Shards image VM's stats", why)),
        }
    }

    /// Member `member` of a running image box, which its spec says its caller drives (joint plan step 17): its
    /// stdin, stdout and stderr taken from the run (D231's `take_input`, `take_output`: each once, so a second
    /// call is refused as Shards refuses it), with its end. Refused, typed, for a member the spec doesn't name and
    /// as every call is for a box that isn't running here; a take refused part-way leaves the member's earlier
    /// streams taken, named in the refusal.
    pub(super) fn member_image(
        &self,
        record: BoxRecord,
        member: &Member,
    ) -> Result<Box<dyn Proc>, Refusal> {
        let id = record.spec.id.try_clone()?;
        let driven = match &record.spec.class {
            ClassSpec::ShardsImage(image) => image.members.iter().any(|m| &m.member == member),
            _ => false,
        };
        if !driven {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("box {id}'s spec names no {member} its caller drives"),
            });
        }
        let guest = match self.settle(&record)? {
            State::Running(guest) => guest,
            State::Held => return Err(held()),
            State::Booting | State::Stopping(_) => return Err(pending(&id)?),
            State::Created | State::Ended(_) => return Err(Refusal::NotRunning(id, "member")),
        };
        let name = member_name(member);
        let stdin = guest
            .take_input(&name)
            .map_err(|why| refused(&id, "taking a member's stdin", why))?;
        let stdout = guest
            .take_output(&name, Channel::Stdout)
            .map_err(|why| refused(&id, "taking a member's stdout (its stdin taken)", why))?;
        let stderr = guest.take_output(&name, Channel::Stderr).map_err(|why| {
            refused(
                &id,
                "taking a member's stderr (its stdin and stdout taken)",
                why,
            )
        })?;
        let given = Given {
            stdin: Some(stdin.into()),
            stdout: Some(stdout.into()),
            stderr: Some(stderr.into()),
        };
        Ok(Box::new(MemberProc {
            id,
            guest,
            name,
            given: Some(given),
            ended: None,
        }))
    }

    pub(super) fn delete_image(&self, record: BoxRecord) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let state = self.settle(&record)?;
        match state {
            State::Held => return Err(held()),
            State::Booting => return Err(pending(&id)?),
            State::Running(_) | State::Stopping(_) => {
                // The delete's intent, durable before its stop.
                devices::with_transaction(self.image_books()?, &id, &[], |t| {
                    let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
                    current.phase = Phase::Deleting;
                    t.put(&current)
                })?;
                self.stop_image(&id, state)?;
            }
            State::Created | State::Ended(_) => {}
        }
        devices::with_transaction(self.image_books()?, &id, &[], |t| {
            let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if current.phase != Phase::Creating && current.exit.is_none() {
                return Err(super::books::uncertain(&id)?);
            }
            current.phase = Phase::Deleting;
            t.put(&current)?;
            t.remove(&id)
        })?;
        if let Some(image) = &self.image {
            image.forget(&id)?;
        }
        Ok(())
    }

    /// Every Shards image box the node's records hold. A held one is listed running: Hex can't prove it
    /// isn't (its inspect says why).
    pub(super) fn image_infos(&self) -> Result<Vec<BoxInfo>, Refusal> {
        if !self.uses_books() {
            return Ok(Vec::new());
        }
        let records = self.authority()?.records()?;
        let mut infos = Vec::new();
        for record in records.into_values() {
            let Ok(record) = record else {
                continue;
            };
            if !matches!(record.spec.class, ClassSpec::ShardsImage(_)) {
                continue;
            }
            let shown = match self.inspect_image(record.clone()) {
                Err(Refusal::Awaits {
                    needs: Upstream::AttachAfterCrash,
                }) => info(&record.spec, BoxState::Running, None)?,
                other => other?,
            };
            infos.try_reserve(1).map_err(|_| Refusal::Exhausted {
                what: "Shards image box listing",
                bound: crate::MAX_BOXES,
            })?;
            infos.push(shown);
        }
        Ok(infos)
    }

    /// Recovery's adoption of a Shards image box: an Ended or Created one is taken as it is; a held run
    /// is refused `Awaits(AttachAfterCrash)`, which recovery keeps as held, its claim with it.
    pub(super) fn recover_image(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        let record = self
            .image_record(&spec.id)?
            .ok_or(Refusal::NoSuchBox(spec.id.try_clone()?))?;
        if record.spec != *spec {
            return Err(super::books::uncertain(&spec.id)?);
        }
        match self.settle(&record)? {
            State::Held => Err(held()),
            _ => Ok(()),
        }
    }
}

/// `id`'s record as it is now.
fn record_now(runtime: &GuestRuntime, id: &BoxId) -> Result<BoxRecord, Refusal> {
    runtime
        .image_record(id)?
        .ok_or(Refusal::NoSuchBox(id.try_clone()?))
}

#[cfg(test)]
#[path = "image_tests.rs"]
mod tests;
