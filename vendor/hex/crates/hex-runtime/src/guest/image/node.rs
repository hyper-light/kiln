//! A node VM the node boots for itself through the image class (joint plan step 39;
//! docs/design/node-under-shards-init.md §4 to §6): Hex's own init under shards-init, from Hex's node image, which its
//! node reaches through its run's agent-control endpoint alone (§5). It is an image VM, so its boot, its stop and its
//! use of its host go through the class's own runs (ROOT, 2026-11-13: one code path, not a third); and it is never a
//! box: its record is a node VM's, named by its run, which no box call reads.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::{Allocation, Exit, HostUse, NodeRun, NodeVmPhase, Refusal};
use hex_core::patience::{Patience, Piece};
use hex_core::record::{NodeVmRecord, RECORD_VERSION};
use shards_api::image::{
    BootStop, ControlAsk, ControlAt, ImageGuest, ImageRefusal, ImageShare, ImageSpec, Keep,
    Narrowing, Ready, SocketGrant, StatsScope,
};

use super::{
    Booted, End, ImageClass, Party, Run, Settled, Slot, Told, admitted_size, boot_on_its_thread,
    diagnostics_file, host_use_of, started_from, timed_stats,
};
use crate::devices::Books;

/// A node VM as its node boots it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeVm {
    /// Its run, minted by the node's store as the VM is booked in its ledger.
    pub run: NodeRun,
    /// The node image, pinned by digest (`HEX_GUEST_IMAGE`): Hex's own init at `/init`, declaring `NESTING`.
    pub image: String,
    /// Its admitted allocation: its receipt in the node's ledger, and the size Shards boots.
    pub allocation: Allocation,
    /// The host directories it shares, each where Hex's init finds it (`/run/hex/shares/<i>`).
    pub shares: Vec<ImageShare>,
    /// The SHA-256 of its node's own key: the one key its run's endpoint takes (D252).
    pub controller: [u8; 32],
    /// The most lanes its run admits at once to the relay's sockets: what its node fitted (hex-node's `serve::fit`).
    pub lanes: u32,
}

/// A node VM whose init has said it is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeBooted {
    pub run: NodeRun,
    /// Its run's agent-control endpoint, and its microVM's key, which its node pins (D252).
    pub control: ControlAt,
    /// How its init said it.
    pub ready: Ready,
    /// How its run started, as Shards said it: booted, its image's template saved or restored on the way, or a warm run
    /// host's, so a cold start is told from a restored one.
    pub started_from: hex_api::StartedFrom,
    /// From the boot's ask to its READY.
    pub took: Duration,
}

/// `guest.hosted.ready`, W*: the slowest READY of a node VM this process has seen, none before the first (decision 9).
static READY: Patience = Patience::measuring("guest.hosted.ready");

/// The command shards-init runs: Hex's own init, `argv[0]` `/init` as the kernel names an initramfs's first process,
/// so that `hex` runs as init under either host (hex-cli's `runs_as_init`).
const INIT: &str = "/init";

const BOOTING: &str = "booting the node VM under shards-init";
const WAITING: &str = "waiting for the node VM under shards-init to say it is ready";

impl ImageClass {
    /// Boots node VM `vm` and follows it to its init's READY.
    /// - **Its record** is written first, Booting, durable before any effect; Ready once its init says it is, and
    ///   Ended with its end where its boot fails.
    /// - **The boot** runs on a thread of its own, as a box's does, and is followed to its answer: within the boot's
    ///   W*, its answer; past it, the run-out told and the boot followed by its thread's life (decision 9), never
    ///   Pending, as the node waits for its own VM.
    /// - **READY** is followed as [`follow_ready`] says, its answer the sample W* rises to.
    /// - **A boot that fails** once its run has started has that run stopped, so none runs held by no one beside the
    ///   next boot's (C-2), and is refused, typed by what failed; one that fails before has its record Ended.
    pub fn boot_node(&self, vm: &NodeVm) -> Result<NodeBooted, Refusal> {
        let image = self.image()?;
        let images = Arc::clone(image.images()?);
        let waits = image.waits()?;
        let books = self.authority()?;
        let spec = self.node_spec(vm)?;
        let diagnostics = image
            .diagnostics
            .join(format!("node-{}.log", run_name(&vm.run)));
        let mut record = NodeVmRecord {
            version: RECORD_VERSION,
            run: vm.run,
            image: vm.image.clone(),
            allocation: vm.allocation,
            phase: NodeVmPhase::Booting,
            diagnostics: Some(diagnostics.clone()),
            exit: None,
            last_resort: None,
            started: None,
        };
        // The boot's intent, durable before any effect.
        books.put_node_vm(&record)?;
        // Its file made before its boot, so the run host keeps there all it says from its start on.
        let kept = match diagnostics_file(&diagnostics) {
            Ok(file) => file,
            Err(why) => {
                end_record(books.as_ref(), &mut record, Exit::Unknown)?;
                return Err(why);
            }
        };
        let slot = Arc::new(Slot::new());
        let stop = Arc::new(BootStop::new());
        if let Err(why) = image
            .runs
            .insert(vm.run, Run::Booting(Arc::clone(&slot), Arc::clone(&stop)))
        {
            // No boot began: its record Ended, never left Booting and booked (C-2).
            end_record(books.as_ref(), &mut record, Exit::Unknown)?;
            return Err(why);
        }
        // Its diagnostics held within the disk its allocation was admitted with, or the tail Shards keeps to quote a run's
        // end where that is less, as an image box's are (the review's B-9).
        let kept_within = vm.allocation.resources.disk_bytes;
        // A node VM is given no volume: its boot plugs none.
        if let Err(why) = boot_on_its_thread(
            images,
            spec,
            kept,
            kept_within,
            Vec::new(),
            Party(Arc::clone(&slot)),
            stop,
        ) {
            image.runs.insert(vm.run, Run::Ended(End::unknown(None)))?;
            image.runs.forget(vm.run)?;
            end_record(books.as_ref(), &mut record, Exit::Unknown)?;
            return Err(Refusal::os("starting a node VM's boot", why));
        }
        let told = match boot_told(&slot, waits, &vm.run) {
            Ok(told) => told,
            // Its wait refused (its lock poisoned, or the OS refused it): the boot is stopped, let go and its row Ended,
            // so none runs held by no one and booked (the review's F4 side finding 3).
            Err(why) => {
                let left = image
                    .runs
                    .stop_boot(vm.run)
                    .and_then(|()| image.runs.forget(vm.run))
                    .and_then(|()| end_record(books.as_ref(), &mut record, Exit::Unknown));
                return Err(match left {
                    Ok(()) => why,
                    Err(left) => Refusal::Broken(format!(
                        "node VM {}: {why}; and its boot's stop: {left}",
                        run_name(&vm.run)
                    )),
                });
            }
        };
        if !matches!(told, Told::Came) {
            // Its thread ended with no answer: the guest it would have answered, if any, went with it (D231 stops a run
            // it drops).
            end_record(books.as_ref(), &mut record, Exit::Unknown)?;
            return Err(Refusal::Broken(format!(
                "node VM {}: its boot's thread ended with no answer, so its run's start is unknown",
                run_name(&vm.run)
            )));
        }
        if let Err(why) = image.runs.booted(vm.run, &slot, waits)? {
            image.runs.forget(vm.run)?;
            end_record(books.as_ref(), &mut record, End::of_refused(&why).exit)?;
            return Err(refused(BOOTING, why));
        }
        // Its run is up: from here a failure stops it (C-2), so no node VM runs held by no one, its record unwritten,
        // beside the one the node's next boot starts.
        self.ready_node(vm.run, &slot, books.as_ref(), &mut record)
            .map_err(|why| match self.stop_node(vm.run) {
                Ok(_) => why,
                Err(cleanup) => Refusal::Broken(format!(
                    "node VM {}: {why}; and its stop: {cleanup}",
                    run_name(&vm.run)
                )),
            })
    }

    /// Node VM `run`, its run up since its boot's answer (`slot`'s), followed to its init's READY and its `record`
    /// written Ready: its endpoint and how its init said it.
    fn ready_node(
        &self,
        run: NodeRun,
        slot: &Slot<Booted>,
        books: &dyn Books,
        record: &mut NodeVmRecord,
    ) -> Result<NodeBooted, Refusal> {
        let guest = self.image()?.runs.guest(run)?.ok_or_else(|| {
            Refusal::Broken("a node VM's run started with no guest held".to_owned())
        })?;
        let control = guest.control().copied().ok_or_else(|| {
            Refusal::Broken(
                "the node VM's run host bound no agent-control endpoint, though its boot asked one"
                    .to_owned(),
            )
        })?;
        let ready = follow_ready(guest.as_ref(), READY.deadline(), slot.asked, &READY)?;
        let took = slot.asked.elapsed();
        READY.answered(took);
        record.phase = NodeVmPhase::Ready;
        books.put_node_vm(record)?;
        let from = started_from(guest.started_from());
        tell(&ready_line(&run, from, ready, took));
        Ok(NodeBooted {
            run,
            control,
            ready,
            started_from: from,
            took,
        })
    }

    /// Stops node VM `run`'s run, followed to its end: D231's stop is never refused, and its thread answers once the
    /// run host has gone. Past the stop's W*, the run-out told, the stop is followed by its thread's life (decision 9).
    /// Its end is kept in its record, Ended, and its run let go.
    pub fn stop_node(&self, run: NodeRun) -> Result<Exit, Refusal> {
        let image = self.image()?;
        let waits = image.waits()?;
        let books = self.authority()?;
        let slot = match image.runs.settle(run, Some(waits))? {
            Settled::Started(guest, _) | Settled::Running(guest, _) => {
                let slot = image.stop(guest, None)?;
                image.runs.stop_asked(run, &slot)?;
                slot
            }
            Settled::Stopping(slot, _) => slot,
            Settled::Ends(end) => return self.ended_node(books.as_ref(), run, &end),
            Settled::Booting => {
                return Err(Refusal::Invalid {
                    what: "node VM",
                    why: format!(
                        "run {}: its boot is under way, which the call that asked it follows",
                        run_name(&run)
                    ),
                });
            }
            Settled::None => return Err(not_running(&run)),
        };
        // The stop's thread followed as its party (B-1, C-3): its answer seen given though another call took it first,
        // and its end with none refusing the stop, never waited for forever.
        let bound = waits.stop.deadline();
        let mut told = slot.told_within(bound)?;
        if let (Told::NotYet, Some(bound)) = (&told, bound) {
            let name = run_name(&run);
            waits.stop.ran_out(
                bound,
                bound,
                &[
                    Piece::Kind("the stop of node VM "),
                    Piece::Name(&name),
                    Piece::Kind(", by its stop's thread"),
                ],
            );
            told = slot.told_within(None)?;
        }
        if !matches!(told, Told::Came) {
            return Err(Refusal::Broken(format!(
                "node VM {}: its stop's thread ended with no answer, so its run's end is unknown",
                run_name(&run)
            )));
        }
        match image.runs.settle(run, Some(waits))? {
            Settled::Ends(end) => self.ended_node(books.as_ref(), run, &end),
            _ => Err(Refusal::Broken(format!(
                "node VM {}: its stop answered, and its run hasn't ended",
                run_name(&run)
            ))),
        }
    }

    /// Stops node VM `run` as [`ImageClass::stop_node`] does, for a caller that can answer no refusal (its guest
    /// runtime's drop): one is told on the process's log, `hex.guest event=node_vm_unstopped`, its run and why, never
    /// dropped (the review's F4 side finding 3; CLAUDE.md §3).
    pub(in crate::guest) fn stop_node_told(&self, run: NodeRun) {
        if let Err(why) = self.stop_node(run) {
            tell(&unstopped_line(&run, &why));
        }
    }

    /// Whether node VM `run` still runs here: a run that ended on its own is reaped through its stop, its end kept in
    /// its record; one booting, stopping or never run here doesn't.
    pub fn node_runs(&self, run: NodeRun) -> Result<bool, Refusal> {
        let image = self.image()?;
        let guest = match image.runs.settle(run, image.waits.as_ref().ok())? {
            Settled::Started(guest, _) | Settled::Running(guest, _) => guest,
            Settled::Ends(end) => {
                self.ended_node(self.authority()?.as_ref(), run, &end)?;
                return Ok(false);
            }
            Settled::Booting | Settled::Stopping(..) | Settled::None => return Ok(false),
        };
        match guest.wait_for(Duration::ZERO) {
            Ok(None) => Ok(true),
            Ok(Some(_)) | Err(ImageRefusal::Ended(_)) => {
                drop(guest);
                self.stop_node(run)?;
                Ok(false)
            }
            Err(why) => Err(refused("reading a node VM's end", why)),
        }
    }

    /// What node VM `run` costs its host (ROOT, 2026-11-13: its HostUse readable, as an image VM's): its run's microVM
    /// part (D241), read through the same stats as an image box's, its window from its run's start to this read.
    pub fn host_use_node(&self, run: NodeRun) -> Result<HostUse, Refusal> {
        let image = self.image()?;
        let waits = image.waits()?;
        let guest = match image.runs.settle(run, Some(waits))? {
            Settled::Started(guest, _) | Settled::Running(guest, _) => guest,
            _ => return Err(not_running(&run)),
        };
        let vcpus = self
            .authority()?
            .node_vm(&run)?
            .ok_or_else(|| Refusal::Broken("a running node VM has no record".to_owned()))?
            .allocation
            .vm_size()?
            .vcpus;
        let stats = match timed_stats(waits, guest.as_ref()) {
            Ok(stats) => stats,
            Err(ImageRefusal::Ended(_)) => return Err(not_running(&run)),
            Err(why) => return Err(refused("reading a node VM's stats", why)),
        };
        let window = image
            .runs
            .started(run)?
            .map(|at| at.elapsed())
            .ok_or_else(|| not_running(&run))?;
        host_use_of(&stats, vcpus, window)
    }

    /// Holds node VM `run`'s vCPUs together to `rate` milli-CPU (joint plan step 39, place 3; D247), as an image box's
    /// are held: its answer waited for while its run lives, its end refusing it, never a timer.
    pub fn rate_node(
        &self,
        run: NodeRun,
        rate: Option<NonZeroU64>,
    ) -> Result<Option<NonZeroU64>, Refusal> {
        match self.node_guest(run)?.rate(rate, Duration::MAX) {
            Ok(held) => Ok(held),
            Err(ImageRefusal::Ended(_)) => Err(not_running(&run)),
            Err(why) => Err(refused("holding the node VM to its rate", why)),
        }
    }

    /// The period node VM `run`'s governor holds its rate over (joint plan step 39, place 3; D191, D247), as its VM said
    /// it with its last rate's answer (Shards' image channel v31), none before its first: the window its node's learner
    /// caps it over, never a number of Hex's.
    pub fn rate_period_node(&self, run: NodeRun) -> Result<Option<Duration>, Refusal> {
        Ok(self.node_guest(run)?.rate_period())
    }

    /// Tells node VM `run`'s guest its room (D188; joint plan step 39), `room` bytes past what it holds, as an image box's
    /// guest is told: its answer waited for while its run lives, its end refusing it, never a timer. What it holds then.
    pub fn room_node(&self, run: NodeRun, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        match self.node_guest(run)?.room(room, Duration::MAX) {
            Ok(held) => Ok(held),
            Err(ImageRefusal::Ended(_)) => Err(not_running(&run)),
            Err(why) => Err(refused("telling the node VM's guest its room", why)),
        }
    }

    /// Node VM `run`'s guest while its run runs here; refused, typed, where it doesn't.
    fn node_guest(&self, run: NodeRun) -> Result<Arc<dyn ImageGuest>, Refusal> {
        let image = self.image()?;
        match image.runs.settle(run, Some(image.waits()?))? {
            Settled::Started(guest, _) | Settled::Running(guest, _) => Ok(guest),
            _ => Err(not_running(&run)),
        }
    }

    /// Node VM `run`'s end, `end`, kept in its record where it lacks one, and its run let go: the end its record holds.
    fn ended_node(&self, books: &dyn Books, run: NodeRun, end: &End) -> Result<Exit, Refusal> {
        let mut record = books
            .node_vm(&run)?
            .ok_or_else(|| Refusal::Broken("an ended node VM has no record".to_owned()))?;
        let exit = match record.exit {
            Some(exit) => exit,
            None => {
                end_record(books, &mut record, end.exit)?;
                end.exit
            }
        };
        self.image()?.runs.forget(run)?;
        Ok(exit)
    }

    /// The image boot that runs Hex's own init under the nesting grant, with its run's agent-control endpoint (§5):
    /// - **command**: [`INIT`] alone, Hex's own init;
    /// - **no ready-by**: Hex's init would end the boot at one, and a node's learned bound is where its wait tells the
    ///   census a run-out, never where a boot ends (decision 9);
    /// - **no member, no published port, no grant past the VM, no credential** (default deny);
    /// - **its endpoint** takes its node's key alone and admits its lanes to the relay's sockets
    ///   ([`hex_client::guest::lanes::DIR`]), each lane's bytes in flight a guest frame each way, the most one control
    ///   message is (`hex_core::stream::MAX_FRAME`).
    fn node_spec(&self, vm: &NodeVm) -> Result<ImageSpec, Refusal> {
        let size = admitted_size(&vm.allocation)?;
        let window = u32::try_from(hex_core::stream::MAX_FRAME).map_err(|_| {
            Refusal::Broken("a guest frame is wider than a lane's window".to_owned())
        })?;
        Ok(ImageSpec {
            image: vm.image.clone(),
            vcpus: size.vcpus,
            memory_mib: size.memory_mib,
            command: Some(vec![INIT.to_owned()]),
            members: Vec::new(),
            publish: Vec::new(),
            egress: Narrowing {
                egress: Keep::Only(Vec::new()),
                mcp: Keep::Only(Vec::new()),
                destinations: Keep::Only(Vec::new()),
                dns: false,
                members: None,
            },
            credentials: Vec::new(),
            lessor: None,
            shares: vm.shares.clone(),
            nesting: true,
            ready_by: None,
            binary: Some(self.binary.clone()),
            env: self.run_host_env(),
            control: Some(ControlAsk {
                controller: vm.controller,
                sockets: Some(SocketGrant {
                    dir: hex_client::guest::lanes::DIR.to_owned(),
                    lanes: vm.lanes,
                    window,
                }),
                // No member's stdio on its lanes (Shards' D252, image channel v44): it runs no member.
                stdio: false,
            }),
            gpu_devices: Vec::new(),
            // No sign-in grant (Shards' D242 step 8): a node VM signs in to nothing.
            sign_ins: Vec::new(),
            // No upstream root (Shards' D242, image channel v26): it grants no credential to fill in.
            upstream_roots: Vec::new(),
            // No GPU report (Shards' D233, image channel v29): a node VM renders on no GPU.
            gpu_report: None,
            // No plug root (Shards' D271, image channel v35): a node VM's shares are its start's.
            plug_roots: Vec::new(),
            // No member's reach (Shards' D278, image channel v39): it runs no member, and nothing passes its VM.
            egress_ceiling: Vec::new(),
            // No share another server serves (Shards' D286, image channel v52): a node VM's shares are its host's.
            served: Vec::new(),
        })
    }
}

/// Writes `record`'s end, `exit`, Ended.
fn end_record(books: &dyn Books, record: &mut NodeVmRecord, exit: Exit) -> Result<(), Refusal> {
    record.phase = NodeVmPhase::Ended;
    record.exit = Some(exit);
    books.put_node_vm(record)
}

/// A node VM's run as its record and its diagnostics name it: its 16 bytes in lowercase hex.
fn run_name(run: &NodeRun) -> String {
    run.bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `hex.guest event=node_vm_ready run=<a JSON string> started_from=<a JSON string> ready=<a JSON string> took_ns=<n>`
/// (ROOT, 2026-11-13): node VM `run` Ready, how its run started (hex-api's `StartedFrom`, in its serde names), how its
/// init said READY (`on_time`, or `late` past its run's ready-by) and from its boot's ask to its READY.
pub(super) fn ready_line(
    run: &NodeRun,
    from: hex_api::StartedFrom,
    ready: Ready,
    took: Duration,
) -> String {
    use hex_api::event::Value::{Number, Text};
    let started = match from {
        hex_api::StartedFrom::Booted => "booted",
        hex_api::StartedFrom::Saved => "saved",
        hex_api::StartedFrom::Restored => "restored",
        hex_api::StartedFrom::Replaced => "replaced",
        hex_api::StartedFrom::Warm => "warm",
    };
    let said = match ready {
        Ready::OnTime => "on_time",
        Ready::Late(_) => "late",
        // follow_ready follows a ready-by passed on to its late READY, so it answers none; said as it is all the same.
        Ready::Passed(_) => "passed",
    };
    hex_api::event::line(
        "hex.guest",
        "node_vm_ready",
        &[
            ("run", Text(&run_name(run))),
            ("started_from", Text(started)),
            ("ready", Text(said)),
            ("took_ns", Number(took.as_nanos())),
        ],
    )
}

/// Writes event `line` to this process's stderr, the node's log, as the node VM store's lines go: a write that fails
/// has no other place to say it.
fn tell(line: &str) {
    #[cfg(test)]
    TOLD.with(|told| told.borrow_mut().push(line.to_owned()));
    use std::io::Write as _;
    let _ = std::io::stderr()
        .lock()
        .write_all(format!("{line}\n").as_bytes());
}

#[cfg(test)]
thread_local! {
    /// The lines this thread has told ([`tell`]), as its tests read them.
    pub(in crate::guest) static TOLD: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Node VM `run`'s boot answered within the boot's W*, or past it, its run-out told, followed by its thread's life
/// (decision 9): whether it came, or its thread ended with none.
fn boot_told(
    slot: &Slot<Booted>,
    waits: &super::ImageWaits,
    run: &NodeRun,
) -> Result<Told, Refusal> {
    let bound = waits.boot.deadline();
    let told = slot.told_within(bound)?;
    let (Told::NotYet, Some(bound)) = (&told, bound) else {
        return Ok(told);
    };
    let name = run_name(run);
    waits.boot.ran_out(
        bound,
        bound,
        &[
            Piece::Kind("the boot of node VM "),
            Piece::Name(&name),
            Piece::Kind(", by its boot's thread"),
        ],
    );
    slot.told_within(None)
}

/// `hex.guest event=node_vm_unstopped run=<a JSON string> why=<a JSON string>`: node VM `run`'s stop refused with `why`
/// where no caller could be answered it ([`ImageClass::stop_node_told`]).
fn unstopped_line(run: &NodeRun, why: &Refusal) -> String {
    use hex_api::event::Value::Text;
    hex_api::event::line(
        "hex.guest",
        "node_vm_unstopped",
        &[
            ("run", Text(&run_name(run))),
            ("why", Text(&why.to_string())),
        ],
    )
}

/// A call about node VM `run` that isn't running here.
fn not_running(run: &NodeRun) -> Refusal {
    Refusal::Invalid {
        what: "node VM",
        why: format!("run {} isn't running here", run_name(run)),
    }
}

/// Follows a node VM's run to its init's READY: the person's rule of 2026-11-13, as decision 9 builds it.
/// - **Within W*** (`bound`, the slowest READY its node has seen, from `began`), READY is the answer.
/// - **Past W*** the run-out is told to the census (`ready`), and is no sample (Karn's point). The wait goes on a W*
///   window at a time while the run progresses, and is refused, stalled, only where a whole window shows none. Its
///   progress is its VM process's CPU time as its host counts it (D241). A window with no reading of it judges no
///   stall: the run is then followed by its life alone, as at a cold start.
/// - **At the cold start**, or after a run-out (no W*), the wait follows the run's life, told once as it starts
///   ([`Patience::following`]; the review's C-10), so an init that hangs alive there is never silent.
/// - **The run's end**, its host's end or a refusal of Shards' ends the wait at any time, typed.
/// - **A ready-by passed**, of a run that carries one (Hex gives none), is waited past to its late READY: D234's table
///   moves from it to ready alone.
pub(in crate::guest) fn follow_ready(
    guest: &dyn ImageGuest,
    bound: Option<Duration>,
    began: Instant,
    ready: &Patience,
) -> Result<Ready, Refusal> {
    let mut seen = None;
    let Some(window) = bound else {
        // No W* to bound it (its process's first READY, or after a run-out): its init could hang alive with nothing to
        // judge it by, so its following is told, once, as it starts (the review's C-10).
        ready.following(&[Piece::Kind("the node VM's run, by its READY and its end")]);
        return wait_to(guest, &mut seen, Duration::MAX)?.ok_or_else(|| {
            Refusal::Broken("Shards answered no readiness to a wait with no timeout".to_owned())
        });
    };
    if let Some(said) = wait_to(guest, &mut seen, window.saturating_sub(began.elapsed()))? {
        return Ok(said);
    }
    ready.ran_out(
        window,
        began.elapsed(),
        &[Piece::Kind(
            "the node VM's run, by its READY and its VM process's CPU time",
        )],
    );
    let mut last = progress(guest, window)?;
    // Ends at READY, the run's end, or a whole window with its progress standing: the run's progress is its bound.
    loop {
        if let Some(said) = wait_to(guest, &mut seen, window)? {
            return Ok(said);
        }
        let now = progress(guest, window)?;
        if let (Some(was), Some(is)) = (last, now)
            && is <= was
        {
            return Err(Refusal::Os {
                op: WAITING,
                why: format!(
                    "no progress over a whole {window:?} past the slowest READY its node has seen: its VM \
                     process's CPU time stood at {was} ns"
                ),
            });
        }
        last = now.or(last);
    }
}

/// The run's READY within `timeout`, none by then. A ready-by passed, `seen` already or said now, is waited past to its
/// late READY: D234's table moves at most twice before ready, from starting to ready-by passed, then to ready late.
fn wait_to(
    guest: &dyn ImageGuest,
    seen: &mut Option<Ready>,
    timeout: Duration,
) -> Result<Option<Ready>, Refusal> {
    for _ in 0..2 {
        match guest.wait_ready(*seen, timeout) {
            Ok(Some(Ready::Passed(past))) => *seen = Some(Ready::Passed(past)),
            Ok(said) => return Ok(said),
            Err(why) => return Err(refused(WAITING, why)),
        }
    }
    Err(Refusal::Broken(
        "the node VM's readiness moved past D234's table".to_owned(),
    ))
}

/// The run's progress as its host counts it: its VM process's CPU time, D241's `Microvm` in the run's stats, read
/// within `window`. None where its run host has no such reading, or no answer came within the window.
fn progress(guest: &dyn ImageGuest, window: Duration) -> Result<Option<u64>, Refusal> {
    match guest.stats(StatsScope::Run, window) {
        Ok(stats) => Ok(stats
            .microvm
            .and_then(|microvm| microvm.host.ok())
            .map(|host| host.cpu_ns)),
        Err(ImageRefusal::Unsupported { .. }) => Ok(None),
        Err(why) => Err(refused(WAITING, why)),
    }
}

/// Shards' refusal of a node VM's call, typed by what it refused.
pub(in crate::guest) fn refused(op: &'static str, why: ImageRefusal) -> Refusal {
    const WHAT: &str = "the node VM under shards-init";
    match why {
        ImageRefusal::NoBackend { os, arch } => Refusal::Platform {
            what: WHAT,
            platform: os,
            why: format!("shards boots no guest on {os} {arch}"),
        },
        ImageRefusal::Spec { what, why } => Refusal::Invalid {
            what: WHAT,
            why: format!("{what}: {why}"),
        },
        ImageRefusal::Image(why) | ImageRefusal::Widening(why) => Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            why,
        },
        ImageRefusal::Unsupported { what, why } => Refusal::Platform {
            what,
            platform: std::env::consts::OS,
            why,
        },
        ImageRefusal::Ended(ended) => Refusal::Os {
            op,
            why: format!(
                "its run ended before its init said it was ready: {:?}",
                ended.end
            ),
        },
        // Its run host's budget had no room for its agent-control endpoint, charged as it binds (Shards' D252, image
        // channel v51): it may boot once that host has room.
        ImageRefusal::Capacity(why) => Refusal::Os {
            op,
            why: format!("its run host's budget had no room for its agent-control endpoint: {why}"),
        },
        // A member of the node image that could not enter its working directory (Shards' D277): the image's.
        ImageRefusal::Workdir(why) => Refusal::Invalid {
            what: "the node image (HEX_GUEST_IMAGE)",
            why: format!("a member's working directory: {why}"),
        },
        // Only a member's restart or a component's swap (Shards' D270) is refused so, and a node VM's run asks neither.
        ImageRefusal::Restart(cause, why) => Refusal::Broken(format!(
            "Shards refused {op} as a member's restart ({cause:?}): {why}"
        )),
        ImageRefusal::Swap(cause, why) => Refusal::Broken(format!(
            "Shards refused {op} as a component's swap ({cause:?}): {why}"
        )),
        // Its run host's, its VM's, or a call's its run makes, in Shards' words.
        other @ (ImageRefusal::Boot(_)
        | ImageRefusal::Host(_)
        | ImageRefusal::Os { .. }
        | ImageRefusal::Plug(..)
        | ImageRefusal::Room(_)
        | ImageRefusal::Rate(_)
        | ImageRefusal::Terminal(_)
        | ImageRefusal::Save(..)
        | ImageRefusal::GpuProbe(_)
        | ImageRefusal::DiagnosticsBound { .. }
        | ImageRefusal::OtherPool { .. }
        | ImageRefusal::Paused { .. }
        | ImageRefusal::Pause(_)
        | ImageRefusal::Net(..)) => Refusal::Os {
            op,
            why: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
