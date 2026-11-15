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
//! D231's boot waits for its run's start or its host's end, given no bound of Hex's, and its stop for its run's end:
//! each runs on a thread of its own and answers in a [`Slot`], which a wait follows as its party
//! ([`Slot::told_within`]): an answer given is seen though another call took it since, and the thread's end with none
//! refuses the call, as the node's broken state. A caller waits at most its learned W* ([`ImageWaits`]), a bound only
//! while answers come within it (decision 9, the person's ruling of 2026-11-13): past it the run-out is told to the
//! census and the call is refused `Lifecycle { Pending }`, the boot or stop going on, and its answer, however long it
//! took, is the sample W* rises to, recorded by the call that takes it. No lock is held over a Shards call, and a run
//! is let go of only once its stop has returned, since D231 stops a run it drops.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use hex_api::{
    BoxId, BoxInfo, BoxSpec, BoxState, ClassSpec, Exit, Given, GuestTime, HostUse, ImageMember,
    LifecycleCause, Member, MemberKind, NodeRun, Proc, Refusal, ShardsImage, Signal, Upstream,
    Usage, VmSize,
};
use hex_core::patience::{Patience, Piece};
use hex_core::record::{BootSession, BoxRecord, Phase};
use shards_api::control::{
    Controller, ExecControl, Execs, Fault, Got, Hangup, Identity, Input, McpConnection,
    Member as MemberLane, MemberControl, Output, Subscriptions, Unstarted, Unsteered,
    exec_admissions,
};
use shards_api::image::{
    BootStop, Channel, ControlAsk, ImageGuest, ImageRefusal, ImageShare, ImageSpec, Images,
    MemberEnd, MemberName, MemberSpec, RunEnd, RunEnded, Secret, Stats, StatsScope,
};

use super::{GuestRuntime, poisoned};
use crate::devices::{self, Books};

pub(super) mod node;
pub use node::{NodeBooted, NodeVm};

/// The waits an image box's calls make, each learned (decision 9): Shards' boot of an image to its members' start, a
/// stats answer, a stop to its run host's exit, and a delete's following of a boot it ends to the boot's answer. Each
/// is the slowest answer this process has seen, a bound only while answers come within it; before its first, and from
/// a run-out to the next answer, it follows its party, the boot's or stop's thread and its run host, until the answer
/// comes. A boot or stop past W* goes on, and its answer is a sample when a later call takes it. No record is read, and
/// none is needed to make an image box (decision 9; the census keeps `image_boot`, `image_stats` and `image_stop` as
/// evidence).
pub struct ImageWaits {
    boot: Patience,
    stats: Patience,
    stop: Patience,
    /// A delete's wait for the boot of the box it deletes (kiln's choice (b), ROOT 2026-11-13): from the delete's ask to
    /// the boot's answer, its run started or its host ended. Past its W*, its run-out is told under its own name, the
    /// boot it ends still under way, and the delete waits on, refused only where the boot's thread ends with no answer.
    delete_boot: Patience,
    /// A `started` call's wait for the boot of the box it asks (kiln's HexVms adapter, ROOT 2026-11-13): from its ask to
    /// the boot's answer. Past its W*, its run-out is told under its own name, the boot it follows still under way, and
    /// the call waits on, refused only where the boot's thread ends with no answer.
    started: Patience,
}

impl ImageWaits {
    /// The five waits, each following its party until its first answer.
    pub const fn new() -> Self {
        Self {
            boot: Patience::measuring("hex.image.boot"),
            stats: Patience::measuring("hex.image.stats"),
            stop: Patience::measuring("hex.image.stop"),
            delete_boot: Patience::measuring("hex.image.delete_boot"),
            started: Patience::measuring("hex.image.started"),
        }
    }
}

impl Default for ImageWaits {
    fn default() -> Self {
        Self::new()
    }
}

/// Shards' image boot as the guest runtime holds it: the booter or why this node has none, its waits or
/// why it has none, where its runs' diagnostics are kept, this process's runs, and the host's boot session,
/// which proves a run of an earlier boot ended.
pub(super) struct ImageBoot {
    images: Result<Arc<dyn Images>, Refusal>,
    waits: Result<ImageWaits, Refusal>,
    /// The directory each box's run host's diagnostics are kept in, whole, one file each (D258's
    /// `ImageGuest::keep_diagnostics`), under the node's state.
    diagnostics: PathBuf,
    /// This process's runs ([`Runs`]).
    runs: Runs,
    session: OnceLock<Option<BootSession>>,
    /// Its runs' agent-control ([`Control`]): the key their endpoints take, and each box's controller.
    control: Arc<Control>,
}

/// The agent-control of this class's runs (D238, D252; goal 2's item 2): its own key, made once from the OS's random
/// numbers, which every run's endpoint takes alone ([`ControlAsk`]); each box's controller, dialed as the first
/// signal to one of its run's members goes, and kept for the run it was dialed for while that run is held, so later
/// signals ride its one connection; and the waits its members' lanes make ([`LaneWaits`]).
struct Control {
    identity: Mutex<Option<Arc<Identity>>>,
    dialed: Mutex<BTreeMap<BoxId, Arc<Steering>>>,
    waits: LaneWaits,
}

/// The waits a member's own lane makes (D238), each learned (decision 9): its first word, from its ask until its run's
/// endpoint says whether the member runs; and a signal's answer, from its send until the member's host takes or refuses
/// it. Shards sends each ask once and gives a handle its answer is waited for on, within each wait's patience
/// (`PendingMember`, `MemberSteering::steered`), so past W* the run-out is told once and the same ask is followed on by
/// the run's connection's life, never asked again.
struct LaneWaits {
    lane: Patience,
    signal: Patience,
    /// A member's harness's next connection to its MCP bridge (D254), from its ask until the harness connects, which may
    /// be never: past W* the wait follows the run's connection, however long that is ([`Steering::mcp`]).
    mcp: Patience,
}

/// A run's controller (Shards' own, `shards_api::control::Controller`), its driver on a thread of its own: where it opens
/// its members' lanes, and its hangup, which ends its connection, and its driver with it, as its last holder lets it go.
struct Steering {
    /// The run it steers: one dialed for another run of the box, or for one gone, is dialed anew.
    guest: Weak<dyn ImageGuest>,
    execs: Execs,
    _hangup: Hangup,
}

impl Control {
    /// No key yet and no controller, each lane wait following its party until its first answer.
    fn new() -> Self {
        Self {
            identity: Mutex::new(None),
            dialed: Mutex::new(BTreeMap::new()),
            waits: LaneWaits {
                lane: Patience::measuring("hex.image.member"),
                signal: Patience::measuring("hex.image.signal"),
                mcp: Patience::measuring("hex.image.mcp"),
            },
        }
    }

    /// This class's control key, made once.
    fn identity(&self) -> Result<Arc<Identity>, Refusal> {
        let mut kept = self.identity.lock().map_err(poisoned)?;
        if let Some(identity) = kept.as_ref() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(Identity::generate().map_err(|why| Refusal::Os {
            op: "making the key the node controls its image VMs' runs by",
            why: why.to_string(),
        })?);
        *kept = Some(Arc::clone(&identity));
        Ok(identity)
    }

    /// What every run asks of its agent-control endpoint: this class's key alone, and no lane to its command's sockets.
    fn ask(&self) -> Result<ControlAsk, Refusal> {
        Ok(ControlAsk {
            controller: self.identity()?.fingerprint(),
            sockets: None,
        })
    }

    /// Box `id`'s controller for its run `guest`, dialed now where it has none for that run; a run gone lets its own go.
    /// The dial waits on nothing: its lanes open once it has connected, each lane's ask following the connection.
    fn steering(&self, id: &BoxId, guest: &Arc<dyn ImageGuest>) -> Result<Arc<Steering>, Refusal> {
        let mut dialed = self.dialed.lock().map_err(poisoned)?;
        if let Some(steering) = dialed.get(id)
            && steering
                .guest
                .upgrade()
                .is_some_and(|held| Arc::ptr_eq(&held, guest))
        {
            return Ok(Arc::clone(steering));
        }
        dialed.retain(|_, steering| steering.guest.strong_count() > 0);
        let at = guest.control().ok_or_else(|| {
            Refusal::Broken(format!(
                "box {id}'s run told no agent-control endpoint, which its spec asked of it"
            ))
        })?;
        let identity = self.identity()?;
        let (controller, connected) = Controller::dial(
            &identity,
            at.vm,
            at.address,
            Subscriptions {
                outputs: &[],
                inputs: &[],
                execs: Some(exec_admissions()),
                sockets: None,
            },
        )
        .map_err(|why| Refusal::Os {
            op: "dialing a Shards image VM's agent-control endpoint",
            why: why.to_string(),
        })?;
        std::thread::Builder::new()
            .name("hex-image-control".to_owned())
            .spawn(move || controller.run())
            .map_err(|why| Refusal::os("starting a Shards image VM's controller", why))?;
        let execs = connected.execs.ok_or_else(|| {
            Refusal::Broken("a controller dialed for its members' lanes was given none".to_owned())
        })?;
        let steering = Arc::new(Steering {
            guest: Arc::downgrade(guest),
            execs,
            _hangup: connected.hangup,
        });
        dialed.insert(id.try_clone()?, Arc::clone(&steering));
        Ok(steering)
    }
}

impl Steering {
    /// `signal` to member `index` of box `id`'s run, on the member's own lane (D238): `true` where it took it, `false`
    /// where the member had ended, which takes none; a lane refused or lost refused in its words.
    ///
    /// Its two waits, for the lane's first word and for the signal's answer, are decision 9's ([`followed`], `waits`):
    /// each ask goes once, and its answer is waited for within its W*, past it on by the run's connection's life.
    fn signal(
        &self,
        waits: &LaneWaits,
        id: &BoxId,
        index: u32,
        signal: u32,
    ) -> Result<bool, Refusal> {
        let refused = |fault: Fault| Refusal::Os {
            op: "signalling a member of a Shards image VM",
            why: format!("box {id}'s member {index}: {fault}"),
        };
        let party = [
            Piece::Kind("the agent-control connection of box "),
            Piece::Name(id.as_str()),
            Piece::Kind(", the lane of its member "),
            Piece::Number(u64::from(index)),
        ];
        let mut asked = self.execs.member(index).map_err(refused)?;
        match followed(&waits.lane, &party, |patience| asked.wait(patience))?.map_err(refused)? {
            MemberLane::Ended(_) => Ok(false),
            MemberLane::Running(MemberControl { mut steering, .. }) => {
                // The first wait sends the signal; a wait after follows its answer, never sending it again.
                let mut unsent = Some(signal);
                let answer = followed(&waits.signal, &party, |patience| match unsent.take() {
                    Some(signal) => steering.signal(signal, patience),
                    None => steering.steered(patience),
                })?;
                match answer {
                    Ok(()) => Ok(true),
                    Err(Fault::Unsteered(Unsteered::Ended)) => Ok(false),
                    Err(fault) => Err(refused(fault)),
                }
            }
        }
    }

    /// Member `index`'s harness's next connection to its MCP bridge (D254), on a lane of its own (goal 2's item 3): its
    /// ask goes once, and its answer is waited for as decision 9 has it ([`followed`], `waits.mcp`), within its W*, past
    /// it on by the run's connection's life, however long the harness takes to connect. Refused, typed, as
    /// `Runtime::mcp` says ([`mcp_refused`]).
    fn mcp(&self, waits: &LaneWaits, id: &BoxId, index: u32) -> Result<McpConnection, Refusal> {
        let party = [
            Piece::Kind("the agent-control connection of box "),
            Piece::Name(id.as_str()),
            Piece::Kind(", the MCP bridge of its member "),
            Piece::Number(u64::from(index)),
        ];
        let mut asked = self
            .execs
            .mcp(index)
            .map_err(|fault| mcp_refused(id, index, fault))?;
        followed(&waits.mcp, &party, |patience| asked.wait(patience))?
            .map_err(|fault| mcp_refused(id, index, fault))
    }
}

/// Why an MCP connection of box `id`'s member `index` wasn't taken, or ended before its end was told, as `Runtime::mcp`
/// says it (ROOT's mapping, 2026-11-14): the run's member takes none (Shards' `Unstarted::Refused`: its run asked it no
/// bridge, or it ended), the run's connection ended, or what else Shards said, in its words.
fn mcp_refused(id: &BoxId, index: u32, fault: Fault) -> Refusal {
    let not_running = |why| match id.try_clone() {
        Ok(id) => Refusal::NotRunning(id, why),
        Err(refused) => refused,
    };
    match fault {
        Fault::Unstarted(Unstarted::Refused) => {
            not_running("mcp: its member takes no MCP connection")
        }
        Fault::Lost(_) => not_running("mcp: its run ended"),
        fault => Refusal::Os {
            op: "waiting for a member's MCP connection",
            why: format!("box {id}'s member {index}: {fault}"),
        },
    }
}

/// Decision 9's wait for a lane's answer, its ask sent: `wait` given `patience`'s W* where it has one, its run-out told
/// once past it (`party` what it follows) and then given `Duration::MAX`, which Shards' handles wait on until the answer
/// or the connection's end; with no W* (before any answer, and after a run-out), `Duration::MAX` from the start. The
/// answer, however late, is the sample W* rises to, and a fault the endpoint answered with is a refusal's sample; the
/// connection's end, which answered nothing, is none.
fn followed<T>(
    patience: &Patience,
    party: &[Piece<'_>],
    mut wait: impl FnMut(Duration) -> Result<Option<T>, Fault>,
) -> Result<Result<T, Fault>, Refusal> {
    let began = Instant::now();
    let bound = patience.deadline();
    let mut answer = wait(bound.unwrap_or(Duration::MAX));
    if let (Ok(None), Some(w_star)) = (&answer, bound) {
        patience.ran_out(w_star, began.elapsed(), party);
        answer = wait(Duration::MAX);
    }
    let took = began.elapsed();
    match answer {
        Ok(Some(answer)) => {
            patience.answered(took);
            Ok(Ok(answer))
        }
        Ok(None) => Err(Refusal::Broken(
            "a lane's wait with no bound returned with no answer".to_owned(),
        )),
        Err(fault @ Fault::Lost(_)) => Ok(Err(fault)),
        Err(fault) => {
            patience.refused(took);
            Ok(Err(fault))
        }
    }
}

/// What a signal but KILL to an image VM whose run's members a signal reaches none of is refused with (ROOT,
/// 2026-11-14): nothing in the run can take it, which says nothing of whether the VM runs.
const NO_MEMBER: &str = "signal: its run has no member a signal reaches";

/// A signal's Linux number as a member's lane carries it.
fn signal_number(signal: Signal) -> Result<u32, Refusal> {
    u32::try_from(signal.number()).map_err(|_| Refusal::Invalid {
        what: "signal",
        why: format!("{signal:?} has no number a member's lane carries"),
    })
}

/// This process's runs: one for each box or node VM with a boot, run or stop of this process, at most
/// [`crate::MAX_BOXES`] in all, the most VMs a node holds. A run moves on under their lock alone, which no Shards call
/// is made under: a call takes the answer its boot's or stop's thread gave ([`Runs::settle`]), or marks its stop.
struct Runs(Mutex<Maps>);

/// The runs by whose they are: a box's by its id, a node VM's by its run (joint plan step 39), an id space of its own,
/// so no box's key names one.
#[derive(Default)]
struct Maps {
    boxes: BTreeMap<BoxId, Run>,
    nodes: BTreeMap<NodeRun, Run>,
}

/// Whose run: a box's, or a node VM's the node runs for itself.
#[derive(Clone, Copy)]
enum Key<'a> {
    Box(&'a BoxId),
    Node(NodeRun),
}

impl<'a> From<&'a BoxId> for Key<'a> {
    fn from(id: &'a BoxId) -> Self {
        Self::Box(id)
    }
}

impl From<NodeRun> for Key<'_> {
    fn from(run: NodeRun) -> Self {
        Self::Node(run)
    }
}

impl Maps {
    fn get(&self, key: Key<'_>) -> Option<&Run> {
        match key {
            Key::Box(id) => self.boxes.get(id),
            Key::Node(run) => self.nodes.get(&run),
        }
    }

    fn get_mut(&mut self, key: Key<'_>) -> Option<&mut Run> {
        match key {
            Key::Box(id) => self.boxes.get_mut(id),
            Key::Node(run) => self.nodes.get_mut(&run),
        }
    }

    fn remove(&mut self, key: Key<'_>) {
        match key {
            Key::Box(id) => drop(self.boxes.remove(id)),
            Key::Node(run) => drop(self.nodes.remove(&run)),
        }
    }

    fn len(&self) -> usize {
        self.boxes.len().saturating_add(self.nodes.len())
    }
}

/// The Shards class on Agentfile images, whole (joint plan step 12's item B): Shards' image boot or why its node
/// serves none ([`ImageClass::with_boot`]), the record authority its boxes' records are kept by, bound once
/// ([`ImageClass::bind_books`]), and its run hosts: the `shards` binary each is, and the Shards home and cache whose
/// store holds the images ([`ImageClass::with_paths`]). A node's guest runtime holds one beside its guest
/// ([`GuestRuntime::with_images`]); a runtime of the class alone holds one with no guest of Hex's.
pub struct ImageClass {
    image: Option<ImageBoot>,
    books: OnceLock<Weak<dyn Books>>,
    binary: PathBuf,
    native_paths: Option<Arc<shards_api::RuntimePaths>>,
    /// What leases its VMs' credentials, its orchestrator's (joint plan step 33; [`ImageClass::with_lessors`]): none,
    /// so a box granting a credential is refused as it is created.
    pub(super) lessors: Option<Arc<dyn hex_api::lease::Lessors>>,
}

/// A boot's answer: its run's guest, or Shards' refusal.
type Booted = Result<Box<dyn ImageGuest>, ImageRefusal>;

/// A run of this process.
enum Run {
    /// Its boot, on a thread of its own, answers here; its stop (D231's `BootStop`) ends it mid-boot, as a delete
    /// asks it.
    Booting(Arc<Slot<Booted>>, Arc<BootStop>),
    /// Its run, held. D231 stops a run it drops, so it leaves only through its stop.
    Running(Arc<dyn ImageGuest>, Instant),
    /// Its stop, on a thread of its own, answers here: the run's end, once its VM is gone. Why it stops beside it.
    Stopping(Arc<Slot<RunEnd>>, Stop),
    /// Its end, known, until its record says it.
    Ended(End),
}

/// Why a run is stopping, so that a call that finds it so says which (goal 15's finding, 2026-11-13): its owner's stop
/// (a signal or a delete, whose own call answers the run's end), its run's own end, read as it came, its host reaped
/// through the stop, or its publishes breaking its box's grants, as the call that took its boot's answer found them
/// (joint plan step 15, N6), the grants' refusal kept in its end.
#[derive(Clone)]
enum Stop {
    Asked,
    Ended(End),
    Grants(Refusal),
}

/// One answer a thread of its own gives, a boot's or a stop's, with how long it took: from the slot's making, as its
/// call was asked, to its answer. Whether it was ever given, though a call has taken it since, and whether its thread
/// has ended ([`Party`]), are kept beside it, so that a wait that follows the thread knows its end.
struct Slot<T> {
    asked: Instant,
    answer: hex_wait::Mutex<Option<(T, Duration)>>,
    came: hex_wait::Condvar,
    given: AtomicBool,
    gone: AtomicBool,
}

#[cfg(test)]
thread_local! {
    /// Whether this thread's next wait on a slot is refused, as a poisoned lock or the OS refuses one: a test's fault.
    static WAIT_REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What a wait that follows a slot's thread found ([`Slot::told_within`]).
enum Told {
    /// Its answer was given (and may have been taken since).
    Came,
    /// Its thread ended with no answer: nothing can give one now.
    Gone,
    /// Neither, within the wait's bound.
    NotYet,
}

/// The thread a slot waits on, its party (decision 9): it gives the slot's answer, and as it ends, with its answer
/// given or with none, it tells the slot's waiters it is gone.
struct Party<T>(Arc<Slot<T>>);

impl<T> Drop for Party<T> {
    fn drop(&mut self) {
        // Told under the slot's lock, so no waiter between its look and its sleep misses it.
        if let Ok(_answer) = self.0.answer.lock() {
            self.0.gone.store(true, Ordering::Release);
            self.0.came.notify_all();
        }
    }
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            asked: Instant::now(),
            answer: hex_wait::Mutex::new(None),
            came: hex_wait::Condvar::new(),
            given: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        }
    }

    fn put(&self, value: T) {
        let took = self.asked.elapsed();
        // A poisoned lock leaves the answer untold: its waiter's patience ends the wait, typed.
        if let Ok(mut answer) = self.answer.lock() {
            *answer = Some((value, took));
            self.given.store(true, Ordering::Release);
            self.came.notify_all();
        }
    }

    /// Whether its answer was given within `wait`, or its thread ended with none (`None`: until either), as a wait that
    /// follows its party asks: an answer another call has taken since was given all the same.
    fn told_within(&self, wait: Option<Duration>) -> Result<Told, Refusal> {
        #[cfg(test)]
        if WAIT_REFUSED.with(|refused| refused.replace(false)) {
            return Err(Refusal::Os {
                op: "waiting for a Shards image's answer",
                why: "refused as the test asked".to_owned(),
            });
        }
        let answer = self.answer.lock().map_err(poisoned)?;
        let open = |_: &mut Option<(T, Duration)>| {
            !self.given.load(Ordering::Acquire) && !self.gone.load(Ordering::Acquire)
        };
        drop(match wait {
            Some(wait) => {
                self.came
                    .wait_timeout_while(answer, wait, open)
                    .map_err(wait_refused)?
                    .0
            }
            None => self.came.wait_while(answer, open).map_err(wait_refused)?,
        });
        Ok(if self.given.load(Ordering::Acquire) {
            Told::Came
        } else if self.gone.load(Ordering::Acquire) {
            Told::Gone
        } else {
            Told::NotYet
        })
    }

    /// The answer, once, with how long it took: the sample its wait records.
    fn take(&self) -> Result<Option<(T, Duration)>, Refusal> {
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

/// `guest`'s run's stats, its wait told to `waits`: Shards waits for the run's answer or its end alone (`Duration::MAX`),
/// and the stats wait's learned W* is where the census is told a run-out, never where the ask ends (decision 9); its
/// answer, however late, the sample W* rises to.
fn timed_stats(waits: &ImageWaits, guest: &dyn ImageGuest) -> Result<Stats, ImageRefusal> {
    let asked = Instant::now();
    let bound = waits.stats.deadline();
    let answer = guest.stats(StatsScope::Run, Duration::MAX);
    let took = asked.elapsed();
    if let Some(bound) = bound.filter(|bound| took > *bound) {
        waits.stats.ran_out(
            bound,
            took,
            &[Piece::Kind(
                "a Shards image VM's run host, answering its stats",
            )],
        );
    }
    if answer.is_ok() {
        waits.stats.answered(took);
    }
    answer
}

/// What a call does with a stop that has no answer by the stop's W* (decision 9), its run-out told either way: answer
/// `Lifecycle { Pending }`, the stop going on and its end recorded when a later call finds it (a start's, a signal's, a
/// retire's); or follow the stop to its end (a delete's: kiln's choice (b), ROOT 2026-11-13, a delete never Pending).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Past {
    Pending,
    Follow,
}

/// Who follows a box's boot to its answer ([`ImageClass::follow_boot`]): a delete of the box (kiln's choice (b)) or a
/// `started` call (kiln's HexVms adapter), each by a wait of its own, each saying where the boot's thread ends with no
/// answer which boot it is that is still under way.
#[derive(Clone, Copy)]
enum Follower {
    Delete,
    Started,
}

impl Follower {
    fn wait(self, waits: &ImageWaits) -> &Patience {
        match self {
            Self::Delete => &waits.delete_boot,
            Self::Started => &waits.started,
        }
    }

    /// Its refusal where the boot's thread ended with no answer, in its own words.
    fn gone(self, id: &BoxId) -> Refusal {
        match self {
            Self::Delete => {
                unanswered_boot(id, "the boot it ends", ", and its record is kept, deleting")
            }
            Self::Started => unanswered_boot(id, "the boot it follows", ""),
        }
    }
}

/// Box `id`'s `boot` (as its caller names it) is still under way, its thread ended with no answer: the node's broken
/// state, `record` saying what its record keeps.
fn unanswered_boot(id: &BoxId, boot: &str, record: &str) -> Refusal {
    Refusal::Broken(format!(
        "box {id}: {boot} is still under way: its boot's thread ended with no answer, so neither its run's start nor \
         its run host's end is known{record}"
    ))
}

/// A run as its runs' lock found it, the answer its boot's or stop's thread gave taken ([`Runs::settle`]).
enum Settled {
    /// No run of this process.
    None,
    /// Its boot, its answer not yet given.
    Booting,
    /// Its boot's answer, taken here: its run has started.
    Started(Arc<dyn ImageGuest>),
    /// Its run, started before.
    Running(Arc<dyn ImageGuest>),
    /// Its stop, its answer not yet given.
    Stopping(Arc<Slot<RunEnd>>, Stop),
    /// Its end, known: its boot refused, its stop's answer taken, or found so before.
    Ends(End),
}

/// A Shards image box, as its record and this process's run say.
enum State {
    Created,
    Booting,
    Running(Arc<dyn ImageGuest>),
    Stopping(Arc<Slot<RunEnd>>, Stop),
    Ended(End),
    Held,
}

/// How a box's run ended: its exit, and where the run failed, why, in its runtime's words ([`BoxInfo::ended`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct End {
    exit: Exit,
    why: Option<String>,
}

impl End {
    /// A run's end as Shards' run host tells it ([`exit_of`]), with its words where it failed: its VM, its host or its
    /// guest failed, or its own command never started (D249).
    fn of(end: &RunEnd) -> Self {
        Self {
            exit: exit_of(end),
            why: match end {
                RunEnd::Failed(why) => Some(why.clone()),
                RunEnd::Exited(_) | RunEnd::Stopped => None,
            },
        }
    }

    /// A stopped run's end, `end` as its run host told it, with the grants' refusal as why where its run was stopped for
    /// breaking them (N6): the why outlives the call that found it (ROOT, 2026-11-14).
    fn stopped(end: &RunEnd, stop: &Stop) -> Self {
        let mut stopped = Self::of(end);
        if let Stop::Grants(why) = stop {
            stopped.why = Some(why.to_string());
        }
        stopped
    }

    /// The end of a run whose boot Shards refused, `why`: one its stop ended mid-boot (D231's `BootStop`) as the stop
    /// ended it, KILL; any other an end Hex can't read the exit of, in Shards' words (the review's B-7).
    fn of_refused(why: &ImageRefusal) -> Self {
        match why {
            ImageRefusal::Ended(RunEnded {
                end: RunEnd::Stopped,
                ..
            }) => Self::of(&RunEnd::Stopped),
            why => Self::unknown(Some(why.to_string())),
        }
    }

    /// An end Hex can't read the exit of, with why where its runtime said it.
    fn unknown(why: Option<String>) -> Self {
        Self {
            exit: Exit::Unknown,
            why,
        }
    }
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

/// The file a box's run host's diagnostics are kept in, whole (D258), made new, the node's alone where the OS says so:
/// its directory made where it isn't there.
fn diagnostics_file(path: &Path) -> Result<std::fs::File, Refusal> {
    let os = |why: std::io::Error| Refusal::os("keeping a Shards image VM's diagnostics", why);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(os)?;
    }
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    open.open(path).map_err(os)
}

/// Removes a box's diagnostics file, as its delete does: one never made, or gone, is no failure.
fn remove_diagnostics(path: &Path) -> Result<(), Refusal> {
    match std::fs::remove_file(path) {
        Err(why) if why.kind() != std::io::ErrorKind::NotFound => {
            Err(Refusal::os("removing a Shards image VM's diagnostics", why))
        }
        _ => Ok(()),
    }
}

/// A failed run's `words`, then its run host's last diagnostics in `file`: its last `bound` bytes, as text, where it
/// says any, and where it can't be read, why.
fn with_last_said(words: &str, file: &Path, bound: usize) -> String {
    match last_bytes(file, bound) {
        Ok(said) if said.trim().is_empty() => words.to_owned(),
        Ok(said) => format!(
            "{words}\nits run host's last diagnostics ({}):\n{}",
            file.display(),
            said.trim_end()
        ),
        Err(why) => format!(
            "{words}\nits run host's diagnostics ({}) could not be read: {why}",
            file.display()
        ),
    }
}

/// The last `bound` bytes of `file`, as text (a cut character replaced).
fn last_bytes(file: &Path, bound: usize) -> std::io::Result<String> {
    use std::io::{Read as _, Seek as _};
    let mut from = std::fs::File::open(file)?;
    let len = from.metadata()?.len();
    let bound = u64::try_from(bound).unwrap_or(u64::MAX);
    from.seek(std::io::SeekFrom::Start(len.saturating_sub(bound)))?;
    let mut said = Vec::new();
    from.take(bound).read_to_end(&mut said)?;
    Ok(String::from_utf8_lossy(&said).into_owned())
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
        // Only a room (D241), a rate (D247) or a terminal's resize (D244) is refused so, and this class asks none.
        ImageRefusal::Room(why) => Refusal::Broken(format!("Shards refused {op} as a room: {why}")),
        ImageRefusal::Rate(why) => Refusal::Broken(format!("Shards refused {op} as a rate: {why}")),
        ImageRefusal::Terminal(why) => {
            Refusal::Broken(format!("Shards refused {op} as a terminal's size: {why}"))
        }
        // Only a save (D259) is refused so, and this class asks none.
        ImageRefusal::Save(cause, why) => {
            Refusal::Broken(format!("Shards refused {op} as a save ({cause:?}): {why}"))
        }
        // Only a run that asks its GPU report (D233) is refused so, and this class asks none.
        ImageRefusal::GpuProbe(why) => {
            Refusal::Broken(format!("Shards refused {op} as a GPU report: {why}"))
        }
        // Shards refused even the tail it keeps of its run host's diagnostics to quote a run's end, the bound a box
        // admitted with less disk keeps (the review's B-9): the box can't keep them.
        ImageRefusal::DiagnosticsBound { bound, tail } => Refusal::Invalid {
            what: "shards image",
            why: format!(
                "its admitted disk, {bound} bytes, is short of the {tail} bytes of its run host's diagnostics Shards \
                 keeps to quote a run's end"
            ),
        },
    }
}

/// The box's VM size, held to what Shards boots on this host: a size it boots as asked, which
/// `shards_api::envelope` (Shards' least memory and unit, its `check`'s rule from one source) gives back unchanged;
/// any other refused naming the allocation and what Shards would boot for it. Hex states none of Shards' numbers.
fn vm_size(spec: &BoxSpec) -> Result<VmSize, Refusal> {
    admitted_size(&spec.allocation.ok_or_else(|| {
        Refusal::Invalid {
            what: "shards image",
            why: "allocation is missing: a Shards image VM is sized by its admitted allocation"
                .to_owned(),
        }
    })?)
}

/// The VM size `allocation` admits, held to what Shards boots on this host, as [`vm_size`] holds a box's.
fn admitted_size(allocation: &hex_api::Allocation) -> Result<VmSize, Refusal> {
    let size = allocation.vm_size()?;
    let invalid = |why: String| Refusal::Invalid {
        what: "shards image",
        why,
    };
    match shards_api::envelope(std::env::consts::ARCH, size.vcpus, size.memory_mib) {
        Ok((vcpus, memory_mib)) if (vcpus, memory_mib) == (size.vcpus, size.memory_mib) => Ok(size),
        Ok((vcpus, memory_mib)) => Err(invalid(format!(
            "allocation: {} vCPUs and {} MiB of guest memory, which Shards boots as {vcpus} and {memory_mib} MiB \
             (shards_api::envelope)",
            size.vcpus, size.memory_mib
        ))),
        Err(why) => Err(invalid(format!("allocation: {why}"))),
    }
}

/// What a running VM's usage is, from its run's STATS: CPU, memory, processes and I/O from its guest's cgroup,
/// and each of its `vcpus` vCPUs' guest time ([`per_vcpu`]); with how its microVM started, `from` (D253). STATS has no
/// memory peak, OOM kills or refused forks (docs/design/shards-image-class.md §4).
fn usage_of(stats: &Stats, vcpus: u32, from: hex_api::StartedFrom) -> Result<Usage, Refusal> {
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
        guest: Some(GuestTime::PerVcpuNs(per_vcpu(stats, vcpus)?)),
        started_from: Some(from),
    })
}

/// How a run's microVM started (D253), as its run host said it as the run started, in Hex's words: Shards' five, one
/// for one.
fn started_from(from: shards_api::image::StartedFrom) -> hex_api::StartedFrom {
    use shards_api::image::StartedFrom as Said;
    match from {
        Said::Booted => hex_api::StartedFrom::Booted,
        Said::Saved => hex_api::StartedFrom::Saved,
        Said::Restored => hex_api::StartedFrom::Restored,
        Said::Replaced => hex_api::StartedFrom::Replaced,
        Said::Warm => hex_api::StartedFrom::Warm,
    }
}

/// What a running VM costs its host, from its run's microVM part (D241's `Stats::microvm`): its VM process's CPU time and
/// own memory, as its run host read the process, and each of its `vcpus` vCPUs' guest time ([`per_vcpu`]). A part the run
/// host did not read is refused, naming why, never read as none.
fn host_use_of(stats: &Stats, vcpus: u32, window: Duration) -> Result<HostUse, Refusal> {
    let unread = |why: String| Refusal::Os {
        op: "reading a Shards image VM's use of its host",
        why,
    };
    let microvm = stats
        .microvm
        .as_ref()
        .ok_or_else(|| unread("its run host told nothing of its microVM".to_owned()))?;
    let host = microvm.host.as_ref().map_err(|why| unread(why.clone()))?;
    Ok(HostUse {
        cpu_ns: host.cpu_ns,
        footprint_bytes: host.footprint_bytes,
        resident_bytes: host.resident_bytes,
        guest_ns: per_vcpu(stats, vcpus)?,
        window,
    })
}

/// What each of the VM's `vcpus` vCPUs has run of its guest, by index, as its run host read it (D241's
/// `Stats::microvm`, from the VM's `GUEST_TIME`): each only grows. A part the run host did not read is refused naming
/// why, never read as none (D241): no microVM part, no guest time, a vCPU this read missed (Shards keeps the most it
/// read before, which no longer says the vCPU's time now), a count other than the VM's size, or time of vCPUs the
/// VM no longer has, which no vCPU of its size holds.
fn per_vcpu(stats: &Stats, vcpus: u32) -> Result<Vec<u64>, Refusal> {
    let op = "reading a Shards image VM's guest time per vCPU";
    let unread = |why: String| Refusal::Os { op, why };
    let microvm = stats
        .microvm
        .as_ref()
        .ok_or_else(|| unread("its run host told nothing of its microVM".to_owned()))?;
    let times = microvm.guest.as_ref().map_err(|why| unread(why.clone()))?;
    vcpus_ns(times, vcpus, op)
}

/// What each of a VM's `vcpus` vCPUs has run of its guest, by index, as its VM process read it (`GUEST_TIME`; an image
/// run's stats, D241, and a managed guest's use, D260, one form): each only grows. Refused, naming why under `op`, never
/// read as none: time of vCPUs the VM no longer has, which no vCPU of its size holds; a count other than the VM's size;
/// or a vCPU this read missed (Shards keeps the most it read before, which no longer says the vCPU's time now).
pub(super) fn vcpus_ns(
    times: &shards_api::image::GuestTimes,
    vcpus: u32,
    op: &'static str,
) -> Result<Vec<u64>, Refusal> {
    let unread = |why: String| Refusal::Os { op, why };
    if times.departed_ns != 0 {
        return Err(unread(format!(
            "{} ns ran on vCPUs the VM no longer has",
            times.departed_ns
        )));
    }
    if u32::try_from(times.vcpus.len()).ok() != Some(vcpus) {
        return Err(unread(format!(
            "{} vCPUs' guest time told for a VM of {vcpus} vCPUs",
            times.vcpus.len()
        )));
    }
    times
        .vcpus
        .iter()
        .zip(0u32..)
        .map(|(vcpu, index)| match &vcpu.unread {
            _ if vcpu.index != index => Err(unread(format!(
                "vCPU {} told where vCPU {index} is",
                vcpu.index
            ))),
            Some(why) => Err(unread(format!("vCPU {index}: {why}"))),
            None => Ok(vcpu.guest_ns),
        })
        .collect()
}

/// What a listing of image boxes does with a record that can't be read ([`ImageClass::image_infos`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unread {
    /// Lists it by its id, Empty: the class's runtime alone, whose records are all image boxes'.
    Listed,
    /// Leaves it out: a guest runtime's records hold other classes' too, and an unreadable one names none.
    Unlisted,
}

/// Box `id`, whose record can't be read, as a listing names it: Empty, of the class, and nothing more known of it, as
/// Linux's node lists one.
fn unread_info(id: BoxId) -> BoxInfo {
    BoxInfo {
        published: Vec::new(),
        id,
        class: hex_api::Class::Shards,
        state: BoxState::Empty,
        bounds: hex_api::Bounds::default(),
        allocation: None,
        adopted: false,
        membership: None,
        exit: None,
        abandoned: 0,
        route: None,
        grants: Vec::new(),
        last_resort: None,
        ended: None,
        control: None,
        control_wake: None,
        credentials: Vec::new(),
    }
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
        ended: None,
        control: None,
        control_wake: None,
        // Every credential grant as the record holds it (joint plan step 33, docs/design/credential-grants.md C4).
        credentials: match &spec.class {
            ClassSpec::ShardsImage(image) => image.credentials.clone(),
            _ => Vec::new(),
        },
    })
}

/// What Shards' image boot gives a member its caller drives (joint plan step 17): its stdin a pipe the caller
/// writes (`MemberSpec::stdin`), its variables, working directory, run-time arguments (after its image's command,
/// D256) and MCP bridge (D254) as the box's spec says them, no file and no terminal. Its stdout and stderr are D231's
/// pipes for every member that runs.
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
        args: m.args.clone(),
        mcp: m.mcp,
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
    /// Its declared index, by which its run's endpoint names its lane (D238); none for one its image declares not.
    index: Option<u32>,
    /// Its run's agent-control, its kill's.
    control: Arc<Control>,
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
            Ok(Some(MemberEnd::Exited(code))) => {
                i32::try_from(code).map_or(Exit::Unknown, Exit::Code)
            }
            // Killed by a signal, its Linux number (image channel v27): its own end, never folded into the code
            // `docker run` reports for it (128 plus it), which an exit can be too.
            Ok(Some(MemberEnd::Signaled(signal))) => {
                i32::try_from(signal).map_or(Exit::Unknown, Exit::Signal)
            }
            // Its run ended first, and the member with it: the run's end is its own.
            Ok(Some(MemberEnd::WithRun)) => match self.guest.wait_for(Duration::ZERO) {
                Ok(Some(ended)) | Err(ImageRefusal::Ended(ended)) => exit_of(&ended.end),
                Ok(None) | Err(_) => Exit::Unknown,
            },
            Err(ImageRefusal::Ended(ended)) => exit_of(&ended.end),
            Err(why) => return Err(refused(&self.id, "waiting for a member's end", why)),
        };
        self.ended = Some(exit);
        Ok(Some(exit))
    }

    /// KILL on its member's own lane, through its run's agent-control endpoint, dialed as the run's first signal goes
    /// (D238; goal 2's item 2): its end then its member's, Signal(9). One that has ended, or ends as it is asked, has
    /// nothing left to end: answered at once.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.ended.is_some() {
            return Ok(());
        }
        let index = self.index.ok_or_else(|| {
            Refusal::Broken(format!(
                "box {}'s run declares no member {:?}, whose stdin it gave",
                self.id, self.name
            ))
        })?;
        self.control
            .steering(&self.id, &self.guest)?
            .signal(
                &self.control.waits,
                &self.id,
                index,
                signal_number(Signal::KILL)?,
            )
            .map(drop)
    }

    fn streams(&mut self) -> Result<Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }
}

/// The errno an MCP connection is aborted with as its kill: Linux's ECONNRESET (include/uapi/asm-generic/errno.h, 104),
/// what a reset connection reads in the guest, whose kernel is Linux's.
const ECONNRESET: std::num::NonZeroU16 = match std::num::NonZeroU16::new(104) {
    Some(errno) => errno,
    None => std::num::NonZeroU16::MIN,
};

/// A member's MCP connection (D254) as its caller's program (goal 2's item 3, `Runtime::mcp`): no process of this host;
/// the caller's ends of a pipe each way, given once ([`Proc::streams`]), and no stderr; its end the connection's
/// ([`ExecControl::wait`]), told once the harness's output has finished.
///
/// Two threads of its own pump it, one each way, each lasting as long as the connection does, through the reads and
/// writes of Shards' MCP lane that wait (its forms that don't, `Output::try_next_record` and `Input::try_write`, came at
/// Shards 5142c6a7 and aren't used here yet), so the cost of a connection is two threads, as a stream's copy is one (the
/// review's D-10).
/// What the harness sends goes to the caller's stdout record by record (a caller that lets its stdout go has the rest
/// read and let go, the connection ending as its stdin does); what the caller writes goes to the harness as it comes, its
/// end the harness's (`Input::end`). Its kill aborts what the harness is sent (ECONNRESET), the reset its lane's `Input`
/// gives (`ExecControl::hangup`, Shards 5142c6a7, isn't used here yet): a kill made while a record waits for the harness
/// to take it goes once it is taken, or the run ends. Its output's pump keeps the connection's `ExecControl` until it has
/// read the exit, since one let go before that hangs the connection up.
struct McpProc {
    /// What the caller's bytes go to the harness through, until its input ends: taken by its pump as the caller's end
    /// of input ends it, or by its kill.
    to: Arc<Mutex<Option<Input>>>,
    /// Its end as its output pump tells it.
    end: McpEnd,
    /// Its kill was sent: its end reads `Signal(9)`.
    killed: Arc<AtomicBool>,
    given: Option<Given>,
    ended: Option<Result<Exit, Refusal>>,
}

/// An MCP connection's end, as its output's pump tells it: its exit, or why none came.
type McpEnd = Arc<Slot<Result<Exit, Refusal>>>;

impl McpProc {
    /// Box `id`'s member `index`'s connection `connection`, its two pumps started; refused, the connection aborted, where
    /// either can't start.
    fn start(id: &BoxId, index: u32, connection: McpConnection) -> Result<Self, Refusal> {
        let McpConnection { from, to, end } = connection;
        let to = Arc::new(Mutex::new(Some(to)));
        let made = Self::pumps(id, index, from, end, &to);
        if made.is_err()
            && let Some(mut taken) = to.lock().ok().and_then(|mut to| to.take())
        {
            // A connection no one pumps is reset at once, so its harness isn't left waiting on it.
            let _ = taken.abort(ECONNRESET);
        }
        let (given, end, killed) = made?;
        Ok(Self {
            to,
            end,
            killed,
            given: Some(given),
            ended: None,
        })
    }

    /// The caller's ends of the connection's two pipes, its end's slot and its kill's mark, each pump on a thread of its
    /// own.
    fn pumps(
        id: &BoxId,
        index: u32,
        from: Output,
        end: ExecControl,
        to: &Arc<Mutex<Option<Input>>>,
    ) -> Result<(Given, McpEnd, Arc<AtomicBool>), Refusal> {
        let pipe = |what: &'static str| std::io::pipe().map_err(|why| Refusal::os(what, why));
        let (sent_r, sent_w) = pipe("making the pipe an MCP connection's harness is sent by")?;
        let (heard_r, heard_w) = pipe("making the pipe an MCP connection's harness is heard by")?;
        let slot = Arc::new(Slot::new());
        let killed = Arc::new(AtomicBool::new(false));
        let party = Party(Arc::clone(&slot));
        let (owner, told) = (id.try_clone()?, Arc::clone(&killed));
        std::thread::Builder::new()
            .name("hex-image-mcp-out".to_owned())
            .spawn(move || pump_out(&owner, index, from, end, heard_w, party, &told))
            .map_err(|why| Refusal::os("starting an MCP connection's output pump", why))?;
        let input = Arc::clone(to);
        std::thread::Builder::new()
            .name("hex-image-mcp-in".to_owned())
            .spawn(move || pump_in(sent_r, &input))
            .map_err(|why| Refusal::os("starting an MCP connection's input pump", why))?;
        let given = Given {
            stdin: Some(sent_w.into()),
            stdout: Some(heard_r.into()),
            stderr: None,
        };
        Ok((given, slot, killed))
    }
}

/// What the harness of an MCP connection sends, to the caller's stdout, record by record, then its end: the
/// connection's status as Shards tells it once its output has finished, `Signal(9)` where it was killed, or why none came
/// ([`mcp_refused`]), told to `party`'s slot.
fn pump_out(
    id: &BoxId,
    index: u32,
    mut from: Output,
    mut end: ExecControl,
    mut caller: std::io::PipeWriter,
    party: Party<Result<Exit, Refusal>>,
    killed: &AtomicBool,
) {
    use std::io::Write as _;
    let mut heard = true;
    let finished = loop {
        match from.next_record() {
            Ok(Got::Record { bytes, .. }) => {
                // A caller that let its stdout go hears no more: the rest is read and let go, so the harness is never
                // held by an output no one takes.
                if heard && caller.write_all(&bytes).is_err() {
                    heard = false;
                }
            }
            Ok(Got::Finished) => break Ok(()),
            Err(fault) => break Err(fault),
        }
    };
    drop(caller);
    let told = finished.and_then(|()| end.wait());
    let exit = match told {
        _ if killed.load(Ordering::Acquire) => Ok(Exit::Signal(Signal::KILL.number())),
        Ok(status) => Ok(Exit::Code(status)),
        Err(fault) => Err(mcp_refused(id, index, fault)),
    };
    party.0.put(exit);
}

/// What the caller writes, to the harness of an MCP connection as it comes, a record of at most Shards' run chunk
/// (`shards_abi::run::CHUNK`) at a time, under `to`'s lock, so a kill takes the connection between records; at the
/// caller's end of input, the harness's (`Input::end`). Its lane's end, or a kill, ends it: the caller's later writes
/// meet a closed pipe.
fn pump_in(mut caller: std::io::PipeReader, to: &Mutex<Option<Input>>) {
    use std::io::Read as _;
    let mut buf = vec![0u8; shards_abi::run::CHUNK];
    loop {
        let read = match caller.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let Ok(mut held) = to.lock() else {
            return;
        };
        let Some(input) = held.as_mut() else {
            return;
        };
        let mut rest = buf.get(..read).unwrap_or_default();
        while !rest.is_empty() {
            match input.write(rest) {
                Ok(took) if took > 0 => rest = rest.get(took..).unwrap_or_default(),
                // Its lane is over: the connection has ended or is ending, and takes no more.
                Ok(_) | Err(_) => {
                    *held = None;
                    return;
                }
            }
        }
    }
    let taken = to.lock().ok().and_then(|mut to| to.take());
    if let Some(mut input) = taken {
        // The harness's end of input; the connection's own end is told by its output's pump.
        let _ = input.end();
    }
}

impl Proc for McpProc {
    /// An MCP connection is its guest's: no process of this host, and no pid is told.
    fn pid(&self) -> u32 {
        0
    }

    fn on_this_host(&self) -> bool {
        false
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.wait_for(Duration::ZERO)
    }

    /// Its end, once its output's pump has told it, waited for at most `timeout` (a timeout past what the clock holds:
    /// until it comes); refused where the pump ended with none told.
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        if let Some(ended) = &self.ended {
            return ended.clone().map(Some);
        }
        let bound = Instant::now().checked_add(timeout).map(|_| timeout);
        match self.end.told_within(bound)? {
            Told::NotYet => Ok(None),
            Told::Gone => Err(Refusal::Broken(
                "an MCP connection's output pump ended with its end untold".to_owned(),
            )),
            Told::Came => {
                let (ended, _) = self.end.take()?.ok_or_else(|| {
                    Refusal::Broken("an MCP connection's end was taken twice".to_owned())
                })?;
                self.ended = Some(ended.clone());
                ended.map(Some)
            }
        }
    }

    /// Aborts what the harness is sent ([`abort_mcp`]).
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.ended.is_some() {
            return Ok(());
        }
        abort_mcp(&self.to, &self.killed, &self.end)
    }

    fn streams(&mut self) -> Result<Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }

    /// Its kill, apart from it, for a node that holds it for a remote caller ([`abort_mcp`]).
    fn killer(&mut self) -> Option<hex_api::Killer> {
        let (to, killed, end) = (
            Arc::clone(&self.to),
            Arc::clone(&self.killed),
            Arc::clone(&self.end),
        );
        Some(Box::new(move || abort_mcp(&to, &killed, &end)))
    }
}

/// An MCP connection's kill: aborts what the harness is sent (ECONNRESET), so its bridge's connection resets, its end then
/// reading `Signal(9)`; it waits while a record in flight to the harness is taken, and for the abort's answer, both on the
/// run's connection's life. One that has ended, or whose input has ended already (its caller's close, which ends it as its
/// harness closes), has nothing more to send: answered at once.
fn abort_mcp(to: &Mutex<Option<Input>>, killed: &AtomicBool, end: &McpEnd) -> Result<(), Refusal> {
    if !matches!(end.told_within(Some(Duration::ZERO))?, Told::NotYet) {
        return Ok(());
    }
    let taken = to.lock().map_err(poisoned)?.take();
    if let Some(mut to) = taken {
        killed.store(true, Ordering::Release);
        // Its lane's answer: the abort taken, or the lane over already, the connection ended or ending either way.
        let _ = to.abort(ECONNRESET);
    }
    Ok(())
}

fn pending(id: &BoxId) -> Result<Refusal, Refusal> {
    Ok(Refusal::Lifecycle {
        id: id.try_clone()?,
        cause: LifecycleCause::Pending,
        why: None,
    })
}

fn held() -> Refusal {
    Refusal::Awaits {
        needs: Upstream::AttachAfterCrash,
    }
}

impl Runs {
    fn new() -> Self {
        Self(Mutex::new(Maps::default()))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Maps>, Refusal> {
        self.0.lock().map_err(poisoned)
    }

    /// When `key`'s run started, as its boot answered, where it runs in this process.
    fn started<'k>(&self, key: impl Into<Key<'k>>) -> Result<Option<Instant>, Refusal> {
        Ok(match self.lock()?.get(key.into()) {
            Some(Run::Running(_, at)) => Some(*at),
            _ => None,
        })
    }

    /// `key`'s run's guest, where it runs in this process.
    fn guest<'k>(&self, key: impl Into<Key<'k>>) -> Result<Option<Arc<dyn ImageGuest>>, Refusal> {
        Ok(match self.lock()?.get(key.into()) {
            Some(Run::Running(guest, _)) => Some(Arc::clone(guest)),
            _ => None,
        })
    }

    /// Lets go of a run whose end is known, alone: a run still held goes only through its stop, since
    /// D231 stops a run it drops and that stop would wait on this lock's holder.
    fn forget<'k>(&self, key: impl Into<Key<'k>>) -> Result<(), Refusal> {
        let key = key.into();
        let mut runs = self.lock()?;
        if matches!(runs.get(key), Some(Run::Ended(_))) {
            runs.remove(key);
        }
        Ok(())
    }

    fn insert<'k>(&self, key: impl Into<Key<'k>>, run: Run) -> Result<(), Refusal> {
        let key = key.into();
        let mut runs = self.lock()?;
        if runs.get(key).is_none() && runs.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        match key {
            Key::Box(id) => drop(runs.boxes.insert(id.try_clone()?, run)),
            Key::Node(node) => drop(runs.nodes.insert(node, run)),
        }
        Ok(())
    }

    /// Asks `key`'s boot under way to stop (D231's `BootStop`: its run host killed mid-boot, the boot answering
    /// `Ended(Stopped)` once that host is gone), where it is booting; a boot that answered already is no longer its
    /// stop's.
    fn stop_boot<'k>(&self, key: impl Into<Key<'k>>) -> Result<(), Refusal> {
        let stop = match self.lock()?.get(key.into()) {
            Some(Run::Booting(_, stop)) => Some(Arc::clone(stop)),
            _ => None,
        };
        // Asked with the runs' lock let go: BootStop::stop kills the run host and returns at once.
        if let Some(stop) = stop {
            stop.stop();
        }
        Ok(())
    }

    /// `key`'s boot under way, where it is booting.
    fn booting<'k>(&self, key: impl Into<Key<'k>>) -> Result<Option<Arc<Slot<Booted>>>, Refusal> {
        Ok(match self.lock()?.get(key.into()) {
            Some(Run::Booting(slot, _)) => Some(Arc::clone(slot)),
            _ => None,
        })
    }

    /// `key`'s run as its lock finds it, an answer its boot's or stop's thread gave taken and its time told to `waits`
    /// (`None` where the node has none): the sample W* rises to, of an answer past W*, its call refused Pending.
    fn settle<'k>(
        &self,
        key: impl Into<Key<'k>>,
        waits: Option<&ImageWaits>,
    ) -> Result<Settled, Refusal> {
        let mut runs = self.lock()?;
        let Some(run) = runs.get_mut(key.into()) else {
            return Ok(Settled::None);
        };
        Ok(match run {
            Run::Booting(slot, _) => match slot.take()? {
                None => Settled::Booting,
                Some((Ok(guest), took)) => {
                    if let Some(waits) = waits {
                        waits.boot.answered(took);
                    }
                    // Its run started as its boot answered.
                    let at = slot.asked.checked_add(took).unwrap_or_else(Instant::now);
                    let guest: Arc<dyn ImageGuest> = Arc::from(guest);
                    *run = Run::Running(Arc::clone(&guest), at);
                    Settled::Started(guest)
                }
                // Its stop ended it mid-boot, as its delete asked (D231's `BootStop`): its run never started, and the
                // time to its stop is no boot's answer, so no sample.
                Some((
                    Err(ImageRefusal::Ended(RunEnded {
                        end: RunEnd::Stopped,
                        ..
                    })),
                    _,
                )) => {
                    let end = End::of(&RunEnd::Stopped);
                    *run = Run::Ended(end.clone());
                    Settled::Ends(end)
                }
                // The run host refused or ended the run before it started: its own answer, in its words.
                Some((Err(why), took)) => {
                    if let Some(waits) = waits {
                        waits.boot.refused(took);
                    }
                    let end = End::unknown(Some(why.to_string()));
                    *run = Run::Ended(end.clone());
                    Settled::Ends(end)
                }
            },
            Run::Running(guest, _) => Settled::Running(Arc::clone(guest)),
            Run::Stopping(slot, stop) => match slot.take()? {
                None => Settled::Stopping(Arc::clone(slot), stop.clone()),
                Some((end, took)) => {
                    if let Some(waits) = waits {
                        waits.stop.answered(took);
                    }
                    let end = End::stopped(&end, stop);
                    *run = Run::Ended(end.clone());
                    Settled::Ends(end)
                }
            },
            Run::Ended(end) => Settled::Ends(end.clone()),
        })
    }

    /// `key`'s boot answer, `slot`'s, taken by the call that asked it, unless another call took it first: its run
    /// started, or its boot refused, the refusal given back in Shards' words. Its time is told to `waits`.
    fn booted<'k>(
        &self,
        key: impl Into<Key<'k>>,
        slot: &Slot<Booted>,
        waits: &ImageWaits,
    ) -> Result<Result<(), ImageRefusal>, Refusal> {
        let mut runs = self.lock()?;
        Ok(match runs.get_mut(key.into()) {
            Some(run @ Run::Booting(..)) => match slot.take()? {
                Some((Ok(guest), took)) => {
                    waits.boot.answered(took);
                    // Its run started as its boot answered.
                    let at = slot.asked.checked_add(took).unwrap_or_else(Instant::now);
                    let guest: Arc<dyn ImageGuest> = Arc::from(guest);
                    *run = Run::Running(guest, at);
                    Ok(())
                }
                Some((Err(why), took)) => {
                    // A boot its delete's stop ended mid-boot, as `settle` finds it, is no boot's answer: the time to
                    // the stop is no sample (the review's B-7).
                    if !matches!(
                        why,
                        ImageRefusal::Ended(RunEnded {
                            end: RunEnd::Stopped,
                            ..
                        })
                    ) {
                        waits.boot.refused(took);
                    }
                    *run = Run::Ended(End::of_refused(&why));
                    Err(why)
                }
                None => Ok(()),
            },
            _ => Ok(()),
        })
    }

    /// `key`'s stop, its owner's, under way in `slot`, whatever its run was doing.
    fn stop_asked<'k>(
        &self,
        key: impl Into<Key<'k>>,
        slot: &Arc<Slot<RunEnd>>,
    ) -> Result<(), Refusal> {
        if let Some(run) = self.lock()?.get_mut(key.into()) {
            *run = Run::Stopping(Arc::clone(slot), Stop::Asked);
        }
        Ok(())
    }

    /// `key`'s stop under way in `slot` for `reached`, what the call that found it reached: its run's own end, or its
    /// grants broken (N6). Where it still runs: another call may have moved it on first, and then its stop is that
    /// call's.
    fn stop_reached<'k>(
        &self,
        key: impl Into<Key<'k>>,
        slot: &Arc<Slot<RunEnd>>,
        reached: Stop,
    ) -> Result<(), Refusal> {
        if let Some(run @ Run::Running(..)) = self.lock()?.get_mut(key.into()) {
            *run = Run::Stopping(Arc::clone(slot), reached);
        }
        Ok(())
    }
}

impl ImageBoot {
    fn images(&self) -> Result<&Arc<dyn Images>, Refusal> {
        self.images.as_ref().map_err(Clone::clone)
    }

    fn waits(&self) -> Result<&ImageWaits, Refusal> {
        self.waits.as_ref().map_err(Clone::clone)
    }

    /// Whether a record the host's boot session `record` names was written in an earlier boot of the
    /// host, whose end ended everything that ran of it (T-T7.1.2). Unknown either way proves nothing.
    fn earlier_boot(&self, record: &BoxRecord) -> bool {
        matches!(
            (&record.boot, self.session.get()),
            (Some(written), Some(Some(now))) if written != now
        )
    }

    /// Stops `guest` on a thread of its own: D231's stop returns once its run is released (Shards 2737fc4ec): its end
    /// told, its run host exited and reaped, and every process the host started, its VM, network and share processes,
    /// gone, so a delete that follows it frees nothing the run still holds (the review's B-4); the guest goes with the
    /// thread. Its answer is `seen`, the end its run
    /// reached on its own before the stop where one was seen ([`Self::settle_running`]'s, the stop then its host's
    /// reaping): shards-api's stop of a run host that exited without its run's end answers `Stopped`, where its end
    /// before was `Failed` in what the host said last. Else the end its stop leaves.
    fn stop(
        &self,
        guest: Arc<dyn ImageGuest>,
        seen: Option<RunEnd>,
    ) -> Result<Arc<Slot<RunEnd>>, Refusal> {
        let slot = Arc::new(Slot::new());
        let party = Party(Arc::clone(&slot));
        std::thread::Builder::new()
            .name("hex-image-stop".to_owned())
            .spawn(move || {
                let stopped = guest.stop();
                let end = match (seen, stopped, guest.wait_for(Duration::ZERO)) {
                    (Some(seen), _, _) => seen,
                    (None, _, Ok(Some(ended))) => ended.end,
                    (None, Err(why), _) | (None, Ok(()), Err(why)) => {
                        RunEnd::Failed(why.to_string())
                    }
                    (None, Ok(()), Ok(None)) => {
                        RunEnd::Failed("its stop returned with no end of its run".to_owned())
                    }
                };
                party.0.put(end);
            })
            .map_err(|e| Refusal::os("starting a Shards image VM's stop", e))?;
        Ok(slot)
    }
}

/// Boots `spec` on a thread of its own, its answer its `party`'s slot's, the party going with the thread, or with its
/// closure where no thread starts, so a follower sees the boot end with no answer: Shards waits for the run's start or
/// its host's end alone (`Duration::MAX`), and `stop` stops it mid-boot (D231's `BootStop`). A booted run's host keeps
/// its diagnostics in `kept` from then on, after what it kept until then, the file held within `bound` bytes, or where
/// that is short of the tail Shards keeps to quote a run's end (a VM whose root is in its RAM is admitted with no disk),
/// within that tail, Shards' own bound, its longest report of a VM's end (the review's B-9); a run whose host can't is
/// stopped, its host and every process it started followed to their end, and its boot is refused in those words.
fn boot_on_its_thread(
    images: Arc<dyn Images>,
    spec: ImageSpec,
    kept: std::fs::File,
    bound: u64,
    party: Party<Booted>,
    stop: Arc<BootStop>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("hex-image-boot".to_owned())
        .spawn(move || {
            let party = party;
            let booted = images
                .boot_stoppable(&spec, Duration::MAX, &stop)
                .and_then(|guest| {
                    let again = kept.try_clone();
                    let held = match guest.keep_diagnostics(kept, bound) {
                        // Shards keeps more of them to quote than the box's disk admits: the file holds that tail.
                        Err(ImageRefusal::DiagnosticsBound { tail, .. }) => match again {
                            Ok(kept) => guest.keep_diagnostics(kept, tail),
                            Err(why) => Err(ImageRefusal::Os {
                                what: "the run host's diagnostics",
                                kind: why.kind(),
                            }),
                        },
                        held => held,
                    };
                    match held {
                        Ok(()) => Ok(guest),
                        // Never left to its drop: the run is stopped, which answers once its host and every process
                        // it started are gone (D231's stop), and the boot refused in the keep's words.
                        Err(why) => {
                            let _ = guest.stop();
                            Err(why)
                        }
                    }
                });
            party.0.put(booted);
        })
        .map(drop)
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
    /// node couldn't boot or a wait of its own couldn't bound (docs/design/shards-image-class.md §6). Each
    /// box's run host's diagnostics are kept whole in `diagnostics`, a directory under the node's state.
    pub fn with_images(
        mut self,
        images: Result<Arc<dyn Images>, Refusal>,
        waits: Result<ImageWaits, Refusal>,
        diagnostics: PathBuf,
    ) -> Self {
        self.images.boot_with(images, waits, diagnostics);
        self
    }

    /// The host's boot session as the node's records carry it (T-T7.1.2), bound once beside the record
    /// authority: a held run whose record an earlier boot wrote ended with that boot.
    pub fn bind_boot_session(&self, session: Option<BootSession>) -> Result<(), Refusal> {
        self.images.bind_boot_session(session)
    }
}

impl ImageClass {
    /// The class whose run hosts are the `shards` binary `binary`, with no image boot yet, no record authority, and no
    /// Shards home or cache of its own, so that its run hosts take Shards' own.
    pub fn new(binary: PathBuf) -> Self {
        Self {
            image: None,
            books: OnceLock::new(),
            binary,
            native_paths: None,
            lessors: None,
        }
    }

    /// Shards' image boot (D231) or why this node has none ([`host_images`]), and the waits its calls
    /// make or why there are none: a create is refused with either refusal, so no box exists that this
    /// node couldn't boot or a wait of its own couldn't bound (docs/design/shards-image-class.md §6). Each
    /// box's run host's diagnostics are kept whole in `diagnostics`, a directory under the node's state.
    pub fn with_boot(
        mut self,
        images: Result<Arc<dyn Images>, Refusal>,
        waits: Result<ImageWaits, Refusal>,
        diagnostics: PathBuf,
    ) -> Self {
        self.boot_with(images, waits, diagnostics);
        self
    }

    pub(super) fn boot_with(
        &mut self,
        images: Result<Arc<dyn Images>, Refusal>,
        waits: Result<ImageWaits, Refusal>,
        diagnostics: PathBuf,
    ) {
        self.image = Some(ImageBoot {
            images,
            waits,
            diagnostics,
            runs: Runs::new(),
            session: OnceLock::new(),
            control: Arc::new(Control::new()),
        });
    }

    /// Its run hosts' Shards home and cache, `paths`, whose store holds the images: each run host's `SHARDS_HOME` and
    /// `SHARDS_CACHE`, and where Shards' console records say how much of a VM's end it quotes.
    pub fn with_paths(mut self, paths: Arc<shards_api::RuntimePaths>) -> Self {
        self.native_paths = Some(paths);
        self
    }

    pub(super) fn set_paths(&mut self, paths: Arc<shards_api::RuntimePaths>) {
        self.native_paths = Some(paths);
    }

    /// The record authority its boxes' records are kept by, bound once, as a node binds its guest's
    /// ([`GuestRuntime::bind_books`]).
    pub fn bind_books(&self, books: Weak<dyn Books>) -> Result<(), Refusal> {
        super::books::bind_once(&self.books, books)
    }

    fn uses_books(&self) -> bool {
        self.books.get().is_some()
    }

    fn authority(&self) -> Result<Arc<dyn Books>, Refusal> {
        self.books
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| Refusal::Broken("a Shards image VM has no live record authority".into()))
    }

    /// Box `id`'s record, as its record authority holds it now: read by its id alone (the review's B-8).
    fn recorded_box(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        if !self.uses_books() {
            return Ok(None);
        }
        self.authority()?
            .record(id)?
            .map(|record| {
                record.map_err(|why| {
                    Refusal::Broken(format!("a Shards image VM's record is unreadable: {why}"))
                })
            })
            .transpose()
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

    fn image_books(&self) -> Result<&Weak<dyn Books>, Refusal> {
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
        let env = self.run_host_env();
        Ok(ImageSpec {
            image: image.image.clone(),
            vcpus: size.vcpus,
            memory_mib: size.memory_mib,
            command: image.command.clone(),
            members: image.members.iter().map(member_spec).collect(),
            publish: network.publish,
            egress: network.egress,
            // The host directories its caller names, and only those (default deny), as Shards takes each: a share
            // Shards refuses refuses the start, in its words.
            shares: image
                .shares
                .iter()
                .map(|m| ImageShare {
                    host: m.source.clone(),
                    at: m.destination.clone(),
                    read_only: m.read_only,
                })
                .collect(),
            // No nesting grant (D234): a harness VM's command is never a nested runtime's here.
            nesting: false,
            ready_by: None,
            // Its credentials by reference (D242), as its spec grants them and no other (joint plan step 33); their
            // lessor is its run's, made as it starts ([`ImageClass::run_lessor`]).
            credentials: super::leases::lowered(image)?,
            lessor: None,
            binary: Some(self.binary.clone()),
            env,
            // The run's agent-control endpoint (D238, D252), of this class's key alone and no socket lane: its members'
            // own lanes carry their signals (goal 2's item 2).
            control: Some(self.image()?.control.ask()?),
            // No GPU (D232's identities): an image VM renders on none here.
            gpu_devices: Vec::new(),
            // Its CLIs' sign-ins (Shards' D242 step 8, image channel v24; docs/design/credential-grants.md C8), as its
            // spec grants them, run-wide as Shards keeps them; their lessor is its run's, as its credentials' is.
            sign_ins: super::leases::sign_ins(image)?,
            // No upstream root of its own (Shards' D242, image channel v26): its run's injector trusts its host's roots
            // alone.
            upstream_roots: Vec::new(),
            // No GPU report (Shards' D233, image channel v29): it renders on no GPU.
            gpu_report: None,
        })
    }

    /// Its run hosts' own environment: the Shards home and cache whose store holds the images, where it has its own.
    fn run_host_env(&self) -> Vec<(OsString, OsString)> {
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
        env
    }

    /// Writes the box's end, its exit and why where its run failed, once, where its record lacks it: a failed run's
    /// words, then its run host's last diagnostics from the box's file (D258), as many bytes as Shards quotes of a VM's
    /// end ([`GuestRuntime::quote_bound`]). Returns the end its record holds.
    fn record_end(&self, id: &BoxId, end: &End) -> Result<End, Refusal> {
        let bound = end.why.as_ref().map(|_| self.quote_bound());
        let mut recorded = None;
        devices::with_record_transaction(self.image_books()?, id, |transaction| {
            let mut record = transaction
                .read(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if let Some(exit) = record.exit {
                recorded = Some(End {
                    exit,
                    why: record.ended.clone(),
                });
                return Ok(());
            }
            let why = match (&end.why, &record.diagnostics, bound) {
                (Some(words), Some(file), Some(bound)) => Some(with_last_said(words, file, bound)),
                (words, ..) => words.clone(),
            };
            record.exit = Some(end.exit);
            record.ended.clone_from(&why);
            transaction.put(&record)?;
            recorded = Some(End {
                exit: end.exit,
                why,
            });
            Ok(())
        })?;
        recorded
            .ok_or_else(|| Refusal::Broken("a box's end was recorded with no record".to_owned()))
    }

    /// How many of its run host's last diagnostics a failed run's `ended` quotes: as many as Shards quotes of a VM's
    /// end, what its kernel's console records need (`shards_abi::bounds::console_said`, as the dedicated guest reads
    /// it), the kernel the node's `SHARDS_KERNEL` names, which its run hosts boot. With none named they boot Shards' own,
    /// which this node can't name: Shards reads a kernel it can't pin as one of no records of its own, so every
    /// kernel's records bound it (shards-api's `said::kept_at`).
    fn quote_bound(&self) -> usize {
        let kernel = std::env::var_os("SHARDS_KERNEL")
            .map(PathBuf::from)
            .unwrap_or_default();
        match &self.native_paths {
            Some(paths) => shards_api::recorded_console_bytes_at(&kernel, paths),
            None => shards_api::recorded_console_bytes(&kernel),
        }
    }

    /// Keeps how the run's microVM started (D253), as its run host said it as the run started, in the box's record, where
    /// its record lacks it: written once, so the box says how it came up across the node's restarts too.
    fn keep_started(&self, record: &BoxRecord, guest: &dyn ImageGuest) -> Result<(), Refusal> {
        if record.started_from.is_some() {
            return Ok(());
        }
        let from = started_from(guest.started_from());
        let id = &record.spec.id;
        devices::with_record_transaction(self.image_books()?, id, |transaction| {
            let mut current = transaction
                .read(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if current.started_from.is_some() {
                return Ok(());
            }
            current.started_from = Some(from);
            transaction.put(&current)
        })
    }

    /// The box `record` names, as its record and this process's run say: an answer that came from a
    /// boot or a stop taken, an end its run reached written, and a reboot of the host taken as the end
    /// of a run of an earlier boot (T-T7.1.2). Decided under the runs' lock, acted on after it.
    fn settle(&self, record: &BoxRecord) -> Result<State, Refusal> {
        let id = &record.spec.id;
        let recorded = |exit| End {
            exit,
            why: record.ended.clone(),
        };
        let Some(image) = &self.image else {
            return Ok(match (record.exit, record.phase) {
                (Some(exit), _) => State::Ended(recorded(exit)),
                (None, Phase::Creating) => State::Created,
                (None, _) => State::Held,
            });
        };
        if let Some(exit) = record.exit {
            image.runs.forget(id)?;
            return Ok(State::Ended(recorded(exit)));
        }
        if record.phase == Phase::Creating {
            return Ok(State::Created);
        }
        match image.runs.settle(id, image.waits.as_ref().ok())? {
            Settled::Booting => Ok(State::Booting),
            Settled::Stopping(slot, stop) => Ok(State::Stopping(slot, stop)),
            // How its microVM started kept in its record (D253), whichever call took its boot's answer: this one, or
            // its start, which settles it next. Its run then read as any running VM's is (the review's B-3): one that
            // ended as it started (D249: its command never started) is seen ended by whichever call took the answer, as
            // by its start, never Running.
            Settled::Started(guest) => {
                self.keep_started(record, guest.as_ref())?;
                match self.settle_running(image, id, guest)? {
                    // Held to its grants by whichever call took its boot's answer, as by its start (N6).
                    State::Running(guest) => self.hold_grants(image, &record.spec, guest),
                    state => Ok(state),
                }
            }
            Settled::Running(guest) => {
                self.keep_started(record, guest.as_ref())?;
                self.settle_running(image, id, guest)
            }
            Settled::Ends(end) => {
                let end = self.record_end(id, &end)?;
                image.runs.forget(id)?;
                Ok(State::Ended(end))
            }
            Settled::None => self.settle_unheld(image, id),
        }
    }

    /// Box `id`, which no run of this process holds, as its record says now, read under its turn (the review's B-2): a
    /// run is let go of only once its end is recorded, so a record read before another call recorded that end and let
    /// go of the run would show this process's own run as an earlier one's, held. Ended where its record has its end,
    /// or where an earlier boot of the host wrote it (T-T7.1.2); else held.
    fn settle_unheld(&self, image: &ImageBoot, id: &BoxId) -> Result<State, Refusal> {
        let now = devices::with_record_transaction(self.image_books()?, id, |t| t.read(id))?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        match now.exit {
            Some(exit) => Ok(State::Ended(End {
                exit,
                why: now.ended,
            })),
            None if image.earlier_boot(&now) => {
                Ok(State::Ended(self.record_end(id, &End::unknown(None))?))
            }
            None => Ok(State::Held),
        }
    }

    /// A run held to box `spec`'s grants as its boot's answer is taken, whichever call takes it (joint plan step 15, N6):
    /// one whose publishes break them has its stop begun, the grants' refusal beside it and kept in its end, which the
    /// start or `started` following it answers; any other runs on.
    fn hold_grants(
        &self,
        image: &ImageBoot,
        spec: &BoxSpec,
        guest: Arc<dyn ImageGuest>,
    ) -> Result<State, Refusal> {
        let ClassSpec::ShardsImage(shards) = &spec.class else {
            return Ok(State::Running(guest));
        };
        let Err(why) = crate::grants::bound(&shards.grants, guest.members(), guest.published())
        else {
            return Ok(State::Running(guest));
        };
        let broke = Stop::Grants(why);
        let slot = image.stop(guest, None)?;
        image.runs.stop_reached(&spec.id, &slot, broke.clone())?;
        Ok(State::Stopping(slot, broke))
    }

    /// What a start or `started` answers of box `id`'s run stopped for breaking its grants (N6), `slot` its stop: the
    /// grants' refusal `why` once the stop has ended (`past` saying how far it is followed), or, where it couldn't be
    /// followed to its end, `why` with the stop's own refusal beside it.
    fn grants_stopped(
        &self,
        id: &BoxId,
        slot: Arc<Slot<RunEnd>>,
        why: Refusal,
        past: Past,
    ) -> Result<Refusal, Refusal> {
        let stopping = State::Stopping(slot, Stop::Grants(why.clone()));
        Ok(match self.stop_image(id, stopping, past) {
            Ok(_) => why,
            Err(cleanup) => Refusal::Uncertain {
                id: id.try_clone()?,
                refusal: Box::new(why),
                cleanup: Box::new(cleanup),
            },
        })
    }

    /// A running VM whose run may have ended on its own: its host is then reaped through its stop, so no
    /// run is let go of before its host has exited, and the box's end is the one its run reached, in its words.
    fn settle_running(
        &self,
        image: &ImageBoot,
        id: &BoxId,
        guest: Arc<dyn ImageGuest>,
    ) -> Result<State, Refusal> {
        let Some(ended) = guest
            .wait_for(Duration::ZERO)
            .map_err(|why| refused(id, "reading a Shards image VM's end", why))?
        else {
            return Ok(State::Running(guest));
        };
        let reached = Stop::Ended(End::of(&ended.end));
        let slot = image.stop(guest, Some(ended.end))?;
        // Where another call moved it on first, its stop is that call's, and this one's thread returns with the same
        // end.
        image.runs.stop_reached(id, &slot, reached.clone())?;
        Ok(State::Stopping(slot, reached))
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
            // As are credentials no one can lease here (joint plan step 33).
            self.can_lease(image)?;
        }
        let books = self.image_books()?;
        let id = spec.id.try_clone()?;
        let made = info(&spec, BoxState::Created, None)?;
        // A box past the node's box budget is refused by its record authority as its record is first written, a place
        // taken under the bound in the same change (the review's B-14), so boxes made at once never pass it together.
        devices::with_record_transaction(books, &id, |transaction| {
            if transaction.read(&id)?.is_some() {
                return Err(Refusal::Exists(id.try_clone()?));
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
            State::Booting | State::Running(_) | State::Stopping(..) => {
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
        let mut spec = self.image_spec(shards, vm_size(&record.spec)?)?;
        // The run's lessor, its orchestrator's, made before any effect (joint plan step 33): a run granting no
        // credential has none and asks nothing.
        spec.lessor = self.run_lessor(&id, shards)?;
        // Where its run host's diagnostics are kept, whole (D258): named in its record with the boot's intent, and gone
        // with its delete.
        let diagnostics = image.diagnostics.join(format!("{id}.log"));
        let slot = Arc::new(Slot::new());
        let stop = Arc::new(BootStop::new());
        // Its boot's party, held here until its thread holds it: a call following a boot that never began sees it end
        // with no answer, never waits for good.
        let party = Party(Arc::clone(&slot));
        // Its boot in this process's runs before its Ready is written (the review's F4 side finding 1): a call that reads
        // the box Ready finds the boot this process runs, never a run of an earlier process, held. A call that reads it
        // Creating sees it Created, and never looks.
        image
            .runs
            .insert(&id, Run::Booting(Arc::clone(&slot), Arc::clone(&stop)))?;
        // The boot's intent, durable before any effect: a crash from here on holds the box.
        let intent = devices::with_record_transaction(self.image_books()?, &id, |transaction| {
            let mut current = transaction
                .read(&id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if current.phase != Phase::Creating
                || current.exit.is_some()
                || current.spec != record.spec
            {
                return Err(super::books::uncertain(
                    &id,
                    "its record isn't Creating, has an end, or isn't the spec its start read",
                )?);
            }
            current.phase = Phase::Ready;
            current.diagnostics = Some(diagnostics.clone());
            transaction.put(&current)
        });
        if let Err(why) = intent {
            // No call read it Ready, so none follows its boot: its run goes.
            image.runs.forget(&id)?;
            return Err(why);
        }
        // Its file made before its boot, so the run host keeps there all it says from its start on.
        let kept = match diagnostics_file(&diagnostics) {
            Ok(file) => file,
            Err(why) => {
                // No boot began: the box ends, its intent's record kept with its end, in the words why.
                let end = End::unknown(Some(why.to_string()));
                image.runs.insert(&id, Run::Ended(end.clone()))?;
                self.record_end(&id, &end)?;
                image.runs.forget(&id)?;
                return Err(why);
            }
        };
        // Shards waits for the run's start or its host's end alone (`Duration::MAX`): the boot's learned W* is where
        // the start answers Pending and tells the census, never where a boot ends (decision 9, the person's ruling of
        // 2026-11-13). D231 shows no run before it starts, so the boot is followed by its own answer: its run's start,
        // or its host's end, refused.
        let bound = waits.boot.deadline();
        // Its diagnostics are its own output on the node's disk, charged to it: their file is held within the disk its box
        // was admitted with, no number picked (the review's B-9, ROOT's ruling of 2026-11-14); a box admitted with no
        // receipt has no disk to hold it within.
        let kept_within = record
            .spec
            .allocation
            .as_ref()
            .map_or(u64::MAX, |allocation| allocation.resources.disk_bytes);
        if let Err(why) = boot_on_its_thread(images, spec, kept, kept_within, party, stop) {
            // No boot began: the box ends, its intent's record kept with its end.
            image.runs.insert(&id, Run::Ended(End::unknown(None)))?;
            self.record_end(&id, &End::unknown(None))?;
            image.runs.forget(&id)?;
            return Err(Refusal::os("starting a Shards image VM's boot", why));
        }
        // The boot's thread followed as its party (B-1): its answer seen given though another call took it first, and
        // its end with none the node's broken state.
        match slot.told_within(bound)? {
            Told::Came => {}
            Told::NotYet => {
                if let Some(bound) = bound {
                    waits.boot.ran_out(
                        bound,
                        bound,
                        &[
                            Piece::Kind("the boot of box "),
                            Piece::Name(id.as_str()),
                            Piece::Kind(", by its boot's thread"),
                        ],
                    );
                }
                return Err(pending(&id)?);
            }
            Told::Gone => return Err(unanswered_boot(&id, "its boot", "")),
        }
        // The answer, taken here unless another call took it first, and its time the sample.
        match image.runs.booted(&id, &slot, waits)? {
            Ok(()) => {
                // What the run published is held to the box's grants (joint plan step 15, N6), here or by the call
                // that took its boot's answer first: a run that breaks them is stopped, and the start says why once
                // its stop has ended, as the box's end keeps it.
                let state = match self.settle(&record_now(self, &id)?)? {
                    State::Running(guest) => self.hold_grants(image, &record.spec, guest)?,
                    state => state,
                };
                match state {
                    State::Running(_) => Ok(()),
                    State::Stopping(slot, Stop::Grants(why)) => {
                        Err(self.grants_stopped(&id, slot, why, Past::Pending)?)
                    }
                    // Its owner's own stop under way (a signal or a delete), whose call answers the run's end: this
                    // start succeeded.
                    State::Stopping(_, Stop::Asked) | State::Booting => Ok(()),
                    // Its run ended on its own as it started, or has been reaped since. A command that started and
                    // exited, at once or not, is a start that succeeded, its exit the box's (`docker start`'s); a run
                    // that failed (its command never started, or its VM, host or guest failed: D249) is a start that
                    // failed, in its runtime's words, which the box's `ended` keeps too.
                    State::Stopping(_, Stop::Ended(end)) | State::Ended(end) => match end.why {
                        Some(why) => Err(Refusal::Failed { id, why }),
                        None => Ok(()),
                    },
                    State::Created | State::Held => Err(super::books::uncertain(
                        &id,
                        "its start answered, yet its record says neither a run nor its end",
                    )?),
                }
            }
            Err(why) => {
                self.record_end(&id, &End::of_refused(&why))?;
                image.runs.forget(&id)?;
                Err(refused(&id, "booting a Shards image VM", why))
            }
        }
    }

    /// Stops a running VM: its stop's answer within the stop's W*; past it, the run-out told, `Lifecycle { Pending }`
    /// where `past` says so, the stop going on and its end recorded, its time the sample, when a later call finds it, or
    /// the stop followed to its end (a delete's), as its thread answers once its run host has gone.
    fn stop_image(&self, id: &BoxId, state: State, past: Past) -> Result<Exit, Refusal> {
        let image = self.image()?;
        let waits = image.waits()?;
        let slot = match state {
            State::Running(guest) => {
                let slot = image.stop(guest, None)?;
                image.runs.stop_asked(id, &slot)?;
                slot
            }
            State::Stopping(slot, _) => slot,
            State::Ended(end) => return Ok(end.exit),
            State::Booting => return Err(pending(id)?),
            State::Held => return Err(held()),
            State::Created => return Err(Refusal::NotRunning(id.try_clone()?, "stop")),
        };
        // The stop's thread followed as its party (B-1): its answer seen given though another call took it first, and
        // its end with none the node's broken state, nothing under way to be Pending on.
        let bound = waits.stop.deadline();
        let mut told = slot.told_within(bound)?;
        if let (Told::NotYet, Some(bound)) = (&told, bound) {
            waits.stop.ran_out(
                bound,
                bound,
                &[
                    Piece::Kind("the stop of box "),
                    Piece::Name(id.as_str()),
                    Piece::Kind(", by its stop's thread"),
                ],
            );
            match past {
                Past::Pending => return Err(pending(id)?),
                // On to its answer: D231's stop is never refused, and its thread answers once the run host has gone,
                // refused only where that thread ends with none.
                Past::Follow => told = slot.told_within(None)?,
            }
        }
        if !matches!(told, Told::Came) {
            let (stop, record) = match past {
                Past::Pending => ("its stop", ""),
                Past::Follow => (
                    "the stop its delete asked",
                    ", and its record is kept, deleting",
                ),
            };
            return Err(Refusal::Broken(format!(
                "box {id}: {stop} is still under way: its stop's thread ended with no answer, so its run's end is \
                 unknown{record}"
            )));
        }
        // Its answer, and its time the sample, is taken as the record is settled.
        match self.settle(&record_now(self, id)?)? {
            State::Ended(end) => Ok(end.exit),
            _ => Err(super::books::uncertain(
                id,
                "its stop answered, yet its record holds no end",
            )?),
        }
    }

    /// `signal`, but KILL, to every live member of box `id`'s run in `state`, each on its own lane of the run's
    /// agent-control endpoint (D238; goal 2's item 2; ROOT, 2026-11-14: a signal reaches every process in the box, a
    /// VM's its members'): the box runs on, a member that has ended skipped. Refused NotRunning ([`NO_MEMBER`]) where
    /// no member of its run runs a program to take it, or none runs one now; Pending while it boots; NotRunning before
    /// its start and once its stop has begun.
    fn signal_members(&self, id: &BoxId, state: State, signal: Signal) -> Result<(), Refusal> {
        let guest = match state {
            State::Running(guest) => guest,
            State::Booting => return Err(pending(id)?),
            State::Held => return Err(held()),
            State::Created | State::Stopping(..) | State::Ended(_) => {
                return Err(Refusal::NotRunning(id.try_clone()?, "signal"));
            }
        };
        let number = signal_number(signal)?;
        if !guest.members().iter().any(|member| member.runs) {
            return Err(Refusal::NotRunning(id.try_clone()?, NO_MEMBER));
        }
        let control = &self.image()?.control;
        let steering = control.steering(id, &guest)?;
        let mut reached = false;
        for member in guest.members().iter().filter(|member| member.runs) {
            reached |= steering.signal(&control.waits, id, member.index, number)?;
        }
        if !reached {
            return Err(Refusal::NotRunning(id.try_clone()?, NO_MEMBER));
        }
        Ok(())
    }

    /// `signal` to member `member` of image box `record` alone, on its own lane of its run's agent-control endpoint
    /// (goal 2's item 2; D238), KILL among them: the member's, never its VM's. Refused Invalid for a member its run
    /// declares not; NotRunning where it runs no program or has ended, and as a box's own signal is where its run isn't
    /// running; Pending while it boots.
    pub(super) fn signal_member_image(
        &self,
        record: BoxRecord,
        member: &Member,
        signal: Signal,
    ) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let guest = match self.settle(&record)? {
            State::Running(guest) => guest,
            State::Held => return Err(held()),
            State::Booting => return Err(pending(&id)?),
            State::Created | State::Stopping(..) | State::Ended(_) => {
                return Err(Refusal::NotRunning(id, "signal a member"));
            }
        };
        let name = member_name(member);
        let Some(declared) = guest
            .members()
            .iter()
            .find(|declared| declared.kind == name.kind && declared.name == name.name)
        else {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("box {id}'s run declares no {member}"),
            });
        };
        if !declared.runs {
            return Err(Refusal::NotRunning(
                id,
                "signal a member: it runs no program",
            ));
        }
        let index = declared.index;
        let control = &self.image()?.control;
        let taken = control.steering(&id, &guest)?.signal(
            &control.waits,
            &id,
            index,
            signal_number(signal)?,
        )?;
        if !taken {
            return Err(Refusal::NotRunning(id, "signal a member: it has ended"));
        }
        Ok(())
    }

    pub(super) fn signal_image(&self, record: BoxRecord, signal: Signal) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let state = self.settle(&record)?;
        match state {
            State::Held => Err(held()),
            State::Ended(_) => Err(Refusal::NotRunning(id, "signal")),
            _ if signal != Signal::KILL => self.signal_members(&id, state, signal),
            // Ended before it booted: no run, and its end its record's.
            State::Created => devices::with_record_transaction(self.image_books()?, &id, |t| {
                let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
                if current.phase != Phase::Creating || current.exit.is_some() {
                    return Err(super::books::uncertain(
                        &id,
                        "its record isn't Creating, or has an end, so its kill can't end it unbooted",
                    )?);
                }
                current.phase = Phase::Ready;
                current.exit = Some(Exit::Signal(Signal::KILL.number()));
                t.put(&current)
            }),
            state => self.stop_image(&id, state, Past::Pending).map(|_| ()),
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
                // Its run's agent-control endpoint, as its run host told it (N6), and that endpoint's waker and wake
                // (Shards' D252 amended, image channel v30), so a reading of the host tells each from a socket no grant
                // made.
                shown.control = guest.control().map(|at| at.address);
                shown.control_wake = guest.control().map(|at| hex_api::ControlWake {
                    waker: at.waker,
                    wake: at.wake,
                });
                Ok(shown)
            }
            State::Stopping(..) => info(&record.spec, BoxState::Running, None),
            State::Ended(end) => {
                let mut shown = info(&record.spec, BoxState::Empty, Some(end.exit))?;
                shown.ended = end.why;
                Ok(shown)
            }
            State::Held => Err(held()),
        }
    }

    pub(super) fn usage_image(&self, record: BoxRecord) -> Result<Usage, Refusal> {
        let (guest, stats) = self.stats_image(&record, "usage")?;
        usage_of(
            &stats,
            vm_size(&record.spec)?.vcpus,
            started_from(guest.started_from()),
        )
    }

    /// The guest of image box `record` running here, or why none: held, or not running, the refusal naming `call`.
    fn image_guest(
        &self,
        record: &BoxRecord,
        call: &'static str,
    ) -> Result<Arc<dyn ImageGuest>, Refusal> {
        match self.settle(record)? {
            State::Running(guest) => Ok(guest),
            State::Held => Err(held()),
            _ => Err(Refusal::NotRunning(record.spec.id.try_clone()?, call)),
        }
    }

    /// The run's guest and its stats (D231's STATS, D241's microVM part among them), for `call`. Shards waits for its
    /// run's answer or its end alone (`Duration::MAX`): the stats wait's learned W* is where the census is told a
    /// run-out, never where the ask ends (decision 9, the person's ruling of 2026-11-13). D231 shows no progress of an
    /// ask it holds, so the ask is followed by its run's life alone, refused only at its end; its answer, however late,
    /// the sample W* rises to.
    fn stats_image(
        &self,
        record: &BoxRecord,
        call: &'static str,
    ) -> Result<(Arc<dyn ImageGuest>, Stats), Refusal> {
        let id = &record.spec.id;
        let guest = self.image_guest(record, call)?;
        match timed_stats(self.image()?.waits()?, guest.as_ref()) {
            Ok(stats) => Ok((guest, stats)),
            Err(ImageRefusal::Ended(_)) => Err(Refusal::NotRunning(id.try_clone()?, call)),
            Err(why) => Err(refused(id, "reading a Shards image VM's stats", why)),
        }
    }

    /// Tells running image box `record`'s guest the room its caller's budget leaves it (`Runtime::room`; D188, D241),
    /// waiting for its answer while its run lives: its end refuses it, never a timer.
    pub(super) fn room_image(
        &self,
        record: BoxRecord,
        room: Option<i64>,
    ) -> Result<Option<u64>, Refusal> {
        let id = &record.spec.id;
        match self.image_guest(&record, "room")?.room(room, Duration::MAX) {
            Ok(held) => Ok(held),
            Err(ImageRefusal::Ended(_)) => Err(Refusal::NotRunning(id.try_clone()?, "room")),
            Err(why) => Err(refused(id, "telling a Shards image VM its room", why)),
        }
    }

    /// Holds running image box `record`'s vCPUs together to `rate` milli-CPU (`Runtime::rate`; D247), waiting for its
    /// answer while its run lives: its end refuses it, never a timer.
    pub(super) fn rate_image(
        &self,
        record: BoxRecord,
        rate: Option<NonZeroU64>,
    ) -> Result<Option<NonZeroU64>, Refusal> {
        let id = &record.spec.id;
        match self.image_guest(&record, "rate")?.rate(rate, Duration::MAX) {
            Ok(held) => Ok(held),
            Err(ImageRefusal::Ended(_)) => Err(Refusal::NotRunning(id.try_clone()?, "rate")),
            Err(why) => Err(refused(id, "holding a Shards image VM to its rate", why)),
        }
    }

    /// The period running image box `record`'s VM's governor holds its rate over (`GuestRuntime::rate_period`; D191,
    /// D247), as its VM said it with its last rate's answer (Shards' image channel v31): none before its first.
    pub(super) fn rate_period_image(&self, record: BoxRecord) -> Result<Option<Duration>, Refusal> {
        Ok(self.image_guest(&record, "rate_period")?.rate_period())
    }

    /// What running image box `record`'s VM costs its host (`Runtime::host_use`): its run's microVM part (D241).
    pub(super) fn host_use_image(&self, record: BoxRecord) -> Result<HostUse, Refusal> {
        let id = &record.spec.id;
        let (_, stats) = self.stats_image(&record, "host_use")?;
        // Its window from its run's start to this read; a run gone since its stats were read has none.
        let window = self
            .image()?
            .runs
            .started(id)?
            .map(|at| at.elapsed())
            .ok_or(Refusal::NotRunning(id.try_clone()?, "host_use"))?;
        host_use_of(&stats, vm_size(&record.spec)?.vcpus, window)
    }

    /// Deletes image box `record` as [`ImageClass::delete_image`] does, answering its VM's final use of its host (goal
    /// 54's change 3): its last read as it runs, just before its delete's stop, its window the whole run; none only
    /// where its run never started (docs/design/shards-image-class.md §3: kiln charges nothing then), never a use made
    /// up. A run that started has its use or is refused, typed, the box left as it was (the review's B-5): the read's
    /// own refusal where it is refused, and `NotRunning` where its run ended before its delete, Shards' stats answering
    /// only a running run.
    pub(super) fn delete_image_used(&self, record: BoxRecord) -> Result<Option<HostUse>, Refusal> {
        self.delete_run(record, true)
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
            State::Booting | State::Stopping(..) => return Err(pending(&id)?),
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
        let index = guest
            .members()
            .iter()
            .find(|declared| declared.kind == name.kind && declared.name == name.name)
            .map(|declared| declared.index);
        Ok(Box::new(MemberProc {
            id,
            guest,
            name,
            index,
            control: Arc::clone(&self.image()?.control),
            given: Some(given),
            ended: None,
        }))
    }

    /// Member `member`'s harness's next connection to its MCP bridge (D254) in image box `record` (goal 2's item 3,
    /// `Runtime::mcp`), as a program ([`McpProc`]). Its spec's ask is checked before anything of its run: `Invalid` where it
    /// asks the member no bridge; then, as `member` is, Pending while it boots, held, and `NotRunning(id, "mcp")` before its
    /// start and once its stop has begun; `Invalid` for a member its run declares not; then the connection's own refusals
    /// ([`Steering::mcp`]).
    pub(super) fn mcp_image(
        &self,
        record: BoxRecord,
        member: &Member,
    ) -> Result<Box<dyn Proc>, Refusal> {
        let id = record.spec.id.try_clone()?;
        let asked = match &record.spec.class {
            ClassSpec::ShardsImage(image) => image
                .members
                .iter()
                .any(|asked| &asked.member == member && asked.mcp),
            _ => false,
        };
        if !asked {
            return Err(Refusal::Invalid {
                what: "mcp",
                why: format!("box {id}'s spec asks no MCP bridge of {member}"),
            });
        }
        let guest = match self.settle(&record)? {
            State::Running(guest) => guest,
            State::Held => return Err(held()),
            State::Booting => return Err(pending(&id)?),
            State::Created | State::Stopping(..) | State::Ended(_) => {
                return Err(Refusal::NotRunning(id, "mcp"));
            }
        };
        let name = member_name(member);
        let Some(index) = guest
            .members()
            .iter()
            .find(|declared| declared.kind == name.kind && declared.name == name.name)
            .map(|declared| declared.index)
        else {
            return Err(Refusal::Invalid {
                what: "member",
                why: format!("box {id}'s run declares no {member}"),
            });
        };
        let control = &self.image()?.control;
        let connection = control
            .steering(&id, &guest)?
            .mcp(&control.waits, &id, index)?;
        Ok(Box::new(McpProc::start(&id, index, connection)?))
    }

    /// Retires image VM `record`, its receipt its profile's under its census numbers (joint plan step 14): its run stopped
    /// as its delete stops it, its run forgotten, and the box kept retired (`BoxRecord::retire`). One held or still
    /// booting is refused, typed, as its delete is.
    pub(super) fn retire_image(&self, record: BoxRecord) -> Result<(), Refusal> {
        let id = record.spec.id.try_clone()?;
        let state = self.settle(&record)?;
        let stopped = match state {
            State::Held => return Err(held()),
            State::Booting => return Err(pending(&id)?),
            State::Running(_) | State::Stopping(..) => {
                Some(self.stop_image(&id, state, Past::Pending)?)
            }
            State::Created | State::Ended(_) => None,
        };
        devices::with_record_transaction(self.image_books()?, &id, |t| {
            let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if let Some(exit) = stopped {
                current.exit = Some(exit);
            }
            current.retire();
            t.put(&current)
        })?;
        if let Some(image) = &self.image {
            image.runs.forget(&id)?;
        }
        Ok(())
    }

    /// Deletes image box `record`, answering once it is gone, never Pending (kiln's choice (b), ROOT 2026-11-13), so its
    /// caller needs no retry; a delete retried then finds no record, NoSuchBox.
    pub(super) fn delete_image(&self, record: BoxRecord) -> Result<(), Refusal> {
        self.delete_run(record, false).map(|_| ())
    }

    /// Box `id`'s boot followed to its answer, for its delete (kiln's choice (b)), which has stopped it first (D231's
    /// `BootStop`, so its answer is its run's start, where that came first, or `Ended(Stopped)` once its run host is
    /// gone), or for a `started` call (kiln's HexVms adapter): `follower` waits for the boot, as decision 9 has a wait
    /// follow its party, by a wait of its own (`delete_boot`, `started`), from its ask. Past that wait's W*, its run-out
    /// is told under its name, the boot it follows still under way, and it waits on, refused only where the boot's
    /// thread ends with no answer. The boot's answer is taken as any later call takes it, its time the boot's own sample
    /// (but a stopped boot's, which is none).
    fn follow_boot(&self, id: &BoxId, follower: Follower) -> Result<State, Refusal> {
        let image = self.image()?;
        let wait = follower.wait(image.waits()?);
        if let Some(slot) = image.runs.booting(id)? {
            let asked = Instant::now();
            let bound = wait.deadline();
            let mut told = slot.told_within(bound)?;
            if let (Told::NotYet, Some(bound)) = (&told, bound) {
                // The boot it follows is still under way.
                wait.ran_out(
                    bound,
                    asked.elapsed(),
                    &[
                        Piece::Kind("the boot of box "),
                        Piece::Name(id.as_str()),
                        Piece::Kind(", by its boot's thread"),
                    ],
                );
                told = slot.told_within(None)?;
            }
            match told {
                Told::Came => wait.answered(asked.elapsed()),
                Told::Gone | Told::NotYet => {
                    // A boot whose start was refused before its thread began has its end recorded first (the review's
                    // F4 side finding 1): that end is the answer; any other boot's thread ended with none.
                    return match self.settle(&record_now(self, id)?)? {
                        state @ State::Ended(_) => Ok(state),
                        _ => Err(follower.gone(id)),
                    };
                }
            }
        }
        self.settle(&record_now(self, id)?)
    }

    /// Box `record` once its start has come to its end (`Runtime::started`; kiln's HexVms adapter, ROOT 2026-11-13): one
    /// still booting is followed to its boot's answer ([`Self::follow_boot`]). A box whose run failed, as it started or
    /// since (its command never started, or its VM, host or guest failed: D249), is answered its failure in its
    /// runtime's words, as `start` answers such a run; any other as inspect answers it, at once where it wasn't booting.
    pub(super) fn started_image(&self, record: BoxRecord) -> Result<BoxInfo, Refusal> {
        let id = record.spec.id.try_clone()?;
        let state = match self.settle(&record)? {
            State::Booting => self.follow_boot(&id, Follower::Started)?,
            state => state,
        };
        match state {
            // Its run stopped for breaking its grants as its boot's answer was taken (N6): the grants' refusal, as its
            // start would answer it, once its stop has been followed to its end.
            State::Stopping(slot, Stop::Grants(why)) => {
                Err(self.grants_stopped(&id, slot, why, Past::Follow)?)
            }
            // Its run failed, as it started or since (D249), its end reached or recorded (the review's B-3); or was
            // stopped for its grants, its end keeping why (N6).
            State::Stopping(_, Stop::Ended(End { why: Some(why), .. }))
            | State::Ended(End { why: Some(why), .. }) => Err(Refusal::Failed { id, why }),
            _ => self.inspect_image(record_now(self, &id)?),
        }
    }

    /// Box `id`'s VM's final use of its host, in its run's state `state` (the review's B-5): a running one's read as it
    /// runs, its refusal the delete's; none where its run never started; and `NotRunning` where its run, which started,
    /// had ended or was being stopped before the delete, Shards' stats answering only a running run, so no use is left
    /// to read and none is made up.
    fn final_use(&self, id: &BoxId, state: &State) -> Result<Option<HostUse>, Refusal> {
        match state {
            State::Running(_) => self.host_use_image(record_now(self, id)?).map(Some),
            State::Stopping(..) | State::Ended(_) if ran(&record_now(self, id)?) => {
                Err(ended_first(id.try_clone()?))
            }
            State::Created
            | State::Booting
            | State::Stopping(..)
            | State::Ended(_)
            | State::Held => Ok(None),
        }
    }

    /// Deletes image box `record`, and where `used` answers its VM's final use of its host ([`Self::delete_image_used`]).
    /// A box booting, running or stopping has its delete's intent written first, durable before any wait; its boot is
    /// followed to its answer ([`Self::follow_boot`]); a run that started is stopped, its stop followed to its end; then
    /// its run host's diagnostics and its record go. Never Pending.
    fn delete_run(&self, record: BoxRecord, used: bool) -> Result<Option<HostUse>, Refusal> {
        let id = record.spec.id.try_clone()?;
        let mut state = self.settle(&record)?;
        // Its final use, where asked, read before any effect of the delete, so that a run that started answers its use
        // or refuses with the box as it was (the review's B-5); a boot under way has no run to read yet.
        let mut last = None;
        if used && !matches!(state, State::Booting) {
            last = self.final_use(&id, &state)?;
        }
        match state {
            State::Held => return Err(held()),
            State::Booting | State::Running(_) | State::Stopping(..) => {
                // The delete's intent, durable before its waits, and the phase it was in.
                let before = devices::with_record_transaction(self.image_books()?, &id, |t| {
                    let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
                    let before = current.phase;
                    current.phase = Phase::Deleting;
                    t.put(&current)?;
                    Ok(before)
                })?;
                if matches!(state, State::Booting) {
                    // Its boot stopped mid-boot (D231's `BootStop`: its run host killed), then followed to its end,
                    // `Ended(Stopped)`; a boot that answered first is a run, stopped below.
                    self.image()?.runs.stop_boot(&id)?;
                    state = self.follow_boot(&id, Follower::Delete)?;
                    // A boot whose answer came before its stop could end it is a run that started: its final use, where
                    // asked, as one up at the delete has it; where it has none, the intent is taken back and the delete
                    // refused, the box as it was, its run left as its boot left it.
                    if used {
                        match self.final_use(&id, &state) {
                            Ok(read) => last = read,
                            Err(why) => {
                                devices::with_record_transaction(self.image_books()?, &id, |t| {
                                    let mut current =
                                        t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
                                    current.phase = before;
                                    t.put(&current)
                                })?;
                                return Err(why);
                            }
                        }
                    }
                }
                match state {
                    State::Running(_) => {
                        self.stop_image(&id, state, Past::Follow)?;
                    }
                    State::Stopping(..) => {
                        self.stop_image(&id, state, Past::Follow)?;
                    }
                    // Its boot refused, its run never started: its end recorded as the boot's answer was taken.
                    State::Ended(_) => {}
                    State::Created | State::Booting | State::Held => {
                        return Err(super::books::uncertain(
                            &id,
                            "its boot answered, yet its record says neither a run nor its end",
                        )?);
                    }
                }
            }
            State::Created | State::Ended(_) => {}
        }
        // Its run host's diagnostics go with it, before its record does, so no file outlives the record naming it.
        if let Some(file) = &record.diagnostics {
            remove_diagnostics(file)?;
        }
        devices::with_record_transaction(self.image_books()?, &id, |t| {
            let mut current = t.read(&id)?.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if current.phase != Phase::Creating && current.exit.is_none() {
                return Err(super::books::uncertain(
                    &id,
                    "its record has no end, so its run's end isn't proved",
                )?);
            }
            current.phase = Phase::Deleting;
            t.put(&current)?;
            t.remove(&id)
        })?;
        if let Some(image) = &self.image {
            image.runs.forget(&id)?;
        }
        Ok(last)
    }

    /// Every Shards image box the node's records hold. A held one is listed running: Hex can't prove it
    /// isn't (its inspect says why). A record that can't be read names no class: where `unread` is the class's
    /// runtime alone, every record of whose authority is an image box's (its create admits no other), it is listed by
    /// its id, Empty, as Linux's node lists one, never hidden (the review's B-13); its inspect refuses it as unreadable.
    pub(super) fn image_infos(&self, unread: Unread) -> Result<Vec<BoxInfo>, Refusal> {
        if !self.uses_books() {
            return Ok(Vec::new());
        }
        let records = self.authority()?.records()?;
        let mut infos = Vec::new();
        for (id, record) in records {
            let record = match (record, unread) {
                (Ok(record), _) => record,
                (Err(_), Unread::Listed) => {
                    infos.try_reserve(1).map_err(|_| Refusal::Exhausted {
                        what: "Shards image box listing",
                        bound: crate::MAX_BOXES,
                    })?;
                    infos.push(unread_info(id));
                    continue;
                }
                (Err(_), Unread::Unlisted) => continue,
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
            return Err(super::books::uncertain(
                &spec.id,
                "its record's spec isn't the one asked",
            )?);
        }
        match self.settle(&record)? {
            State::Held => Err(held()),
            _ => Ok(()),
        }
    }
}

impl ImageClass {
    /// Box `id`'s record for a call of the class alone, refused `NoSuchBox` where its records hold no image box of that
    /// id, and as the box's backing refuses it where its backing was retired (joint plan step 14).
    fn called(&self, id: &BoxId) -> Result<BoxRecord, Refusal> {
        let record = self
            .image_record(id)?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        if record.retired {
            return Err(record.retired_refusal()?);
        }
        Ok(record)
    }
}

/// The class alone as a runtime (joint plan step 12's item B): a node with no guest of Hex's serves its boxes, each a
/// Shards image VM, through it, once its record authority is bound ([`ImageClass::bind_books`]). A box of any other
/// class is refused at its create, and a program in a member awaits the agent-control endpoint, as on any node.
impl hex_api::Runtime for ImageClass {
    fn classes(&self) -> Vec<hex_api::Class> {
        vec![hex_api::Class::Shards]
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        if !matches!(spec.class, ClassSpec::ShardsImage(_)) {
            return Err(Refusal::Invalid {
                what: "class",
                why: "this runtime serves Shards image VMs alone".to_owned(),
            });
        }
        self.create_image(spec)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.start_image(self.called(id)?)
    }

    fn exec(
        &self,
        id: &BoxId,
        _: hex_api::Program,
        _: hex_api::Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        self.called(id)?;
        // A member's program is the agent-control endpoint's (joint plan step 11).
        Err(Refusal::Awaits {
            needs: Upstream::MemberExec,
        })
    }

    fn run(
        &self,
        spec: BoxSpec,
        _: hex_api::Program,
        _: hex_api::Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_run()?;
        Err(Refusal::Invalid {
            what: "class",
            why: "a Shards image VM runs its image's members or its own command, never a run's program".to_owned(),
        })
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.signal_image(self.called(id)?, signal)
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let record = self
            .image_record(id)?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        if record.retired {
            return record.retired_info();
        }
        self.inspect_image(record)
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.usage_image(self.called(id)?)
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.image_infos(Unread::Listed)
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        let record = self
            .image_record(id)?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        self.delete_image(record)
    }

    fn member(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, Refusal> {
        self.member_image(self.called(id)?, member)
    }

    fn signal_member(&self, id: &BoxId, member: &Member, signal: Signal) -> Result<(), Refusal> {
        self.signal_member_image(self.called(id)?, member, signal)
    }

    fn mcp(&self, id: &BoxId, member: &Member) -> Result<Box<dyn Proc>, Refusal> {
        self.mcp_image(self.called(id)?, member)
    }

    fn room(&self, id: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        self.room_image(self.called(id)?, room)
    }

    fn rate(&self, id: &BoxId, rate: Option<NonZeroU64>) -> Result<Option<NonZeroU64>, Refusal> {
        self.rate_image(self.called(id)?, rate)
    }

    fn host_use(&self, id: &BoxId) -> Result<HostUse, Refusal> {
        self.host_use_image(self.called(id)?)
    }

    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<HostUse>, Refusal> {
        let record = self
            .image_record(id)?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        self.delete_image_used(record)
    }

    fn started(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let record = self
            .image_record(id)?
            .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        if record.retired {
            return record.retired_info();
        }
        self.started_image(record)
    }
}

/// `id`'s record as it is now.
/// Whether box `record`'s run started: its record keeps how its microVM started (D253) once a call took its boot's answer,
/// which no boot refused or stopped before its run gives.
fn ran(record: &BoxRecord) -> bool {
    record.started_from.is_some()
}

/// A delete with its host use of box `id`, whose run started and had ended, or was being stopped, before it: Shards'
/// stats answer only a running run, so no use is left to read (the review's B-5), and none is made up.
fn ended_first(id: BoxId) -> Refusal {
    Refusal::NotRunning(
        id,
        "read its final use of its host: its run had ended, or was being stopped, before its delete",
    )
}

fn record_now(class: &ImageClass, id: &BoxId) -> Result<BoxRecord, Refusal> {
    class
        .image_record(id)?
        .ok_or(Refusal::NoSuchBox(id.try_clone()?))
}

#[cfg(test)]
#[path = "image_tests.rs"]
mod tests;
