//! The Windows runtime: [`hex_api::Runtime`] over the `process` and `oci` classes
//! (docs/design/runtime.md). A `process` box is a job object; its bounds are the kernel's and hard
//! (`process`, `sys`, [`hex_core::job`]), and a member is started suspended inside the job and resumed, so
//! nothing runs outside it. An `oci` box is a Windows image run process-isolated through the Host Compute
//! Service with such a job nested in its silo (`oci`, [`hex_core::hcs`]). Where a Windows feature Hex's
//! installer turns on is off ([`features`]), the class is refused by name, naming it. This is the node's
//! runtime seam on Windows; the records, recovery and socket that wrap it on Linux (`hex-node`) are not
//! yet cross-OS, so this holds its boxes in memory and is driven in process.

pub mod features;
pub mod keeper;
mod managed;
pub mod oci;
mod preflight;
pub mod proc;
pub mod process;
pub mod sys;
mod wmi;

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use triomphe::Arc as NativeArc;

use hex_api::{
    BoxId, BoxInfo, BoxSpec, BoxState, Capacity, CapacityResources, Class, ClassSpec, DeviceId,
    DeviceInfo, Proc, Program, Refusal, Rootfs, Runtime, Signal, Stdio, Usage,
};

use crate::devices::{Binding, Catalog, Route, SharedCatalog};
use crate::guest::Side;
use hex_core::record::{NativeRecord, OwnerKey, OwnerRef};
use hex_core::setup::{self, WindowsFeature};
use oci::OciBox;
use process::JobBox;
use sys::{Spawn, Started};

/// A box of either class.
#[derive(Clone)]
enum Held {
    Process(NativeArc<JobBox>),
    Oci(Arc<OciBox>),
}

fn native_held(boxed: JobBox) -> Result<Held, Refusal> {
    NativeArc::try_new(boxed)
        .map(Held::Process)
        .map_err(|_| allocation(std::mem::size_of::<JobBox>()))
}

fn remove_vm_keeper_file(state: &Path, id: OwnerRef<'_>, boxed: &JobBox) -> Result<(), Refusal> {
    let path = keeper::vm::path(state, id)?;
    let Some(held) = keeper::vm::read(&path)? else {
        return Ok(());
    };
    if !held.commits(boxed.vm_commitment()) {
        return Err(pending(
            "the retained keeper file belongs to another native scope",
        ));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Refusal::os(
            "removing the released VM keeper receipt",
            error,
        )),
    }
}

impl Held {
    /// The box's job handle value, for the node's keeper to hold a second handle to it.
    fn job_token(&self) -> Result<usize, Refusal> {
        match self {
            Held::Process(b) => b.job_token(),
            Held::Oci(b) => b.job_token(),
        }
    }
}

/// One entry in the existing native holding table, including work not yet safe to publish or forget.
struct Slot {
    devices: Vec<DeviceId>,
    marker: Option<PathBuf>,
    route: Option<hex_api::RouteReceipt>,
    keeper: KeeperHold,
    state: SlotState,
}

impl Slot {
    fn physical(&self) -> Option<&Held> {
        match &self.state {
            SlotState::Finishing(held)
            | SlotState::CancelFinishing(held)
            | SlotState::Ready(held)
            | SlotState::Removing(held)
            | SlotState::CleanupPending(held) => Some(held),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct KeeperReceipt {
    pid: u32,
    created: u64,
    value: usize,
}

#[derive(Clone, Copy)]
enum KeeperHold {
    Unheld,
    Held(KeeperReceipt),
    Closed,
    Uncertain,
}

enum SlotState {
    Creating,
    Cancelled,
    PreparedCleanupPending,
    Finishing(Held),
    CancelFinishing(Held),
    Ready(Held),
    Removing(Held),
    CleanupPending(Held),
    Uncertain,
    RemovingUncertain,
    RemovingPrepared,
}

enum Removal {
    Physical(Held),
    Prepared,
    UnknownAssigned,
    UnknownLegacy,
}

fn allocation(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "Windows device state",
        bound,
    }
}

fn wrong_backend(spec: &BoxSpec) -> Refusal {
    Refusal::Platform {
        what: "runtime class",
        platform: "windows native OCI",
        why: format!(
            "{} belongs to its initialized microVM adapter",
            spec.class.class().name()
        ),
    }
}

fn pending(why: &'static str) -> Refusal {
    Refusal::Invalid {
        what: "native box lifecycle",
        why: why.to_owned(),
    }
}

fn inconsistent_slots() -> Refusal {
    Refusal::Broken("native holding entry changed during its owned operation".to_owned())
}

fn owner_index(boxes: &[(OwnerKey, Slot)], id: OwnerRef<'_>) -> Result<usize, usize> {
    boxes.binary_search_by(|(key, _)| key.as_ref().cmp(&id))
}

fn slot_index(boxes: &[(OwnerKey, Slot)], id: &BoxId) -> Result<usize, usize> {
    owner_index(boxes, OwnerRef::Box(id))
}

fn copy_devices(devices: &[DeviceId]) -> Result<Vec<DeviceId>, Refusal> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(devices.len())
        .map_err(|_| allocation(devices.len()))?;
    copy.extend_from_slice(devices);
    Ok(copy)
}

fn copy_text(value: &str) -> Result<String, Refusal> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| allocation(value.len()))?;
    copy.push_str(value);
    Ok(copy)
}

/// A node's keeper and the state folder its boxes' jobs are recorded in. The keeper is started by
/// the post-preflight transition (or an existing one adopted from the state folder) and holds a second handle to each
/// box's job, so a box outlives the node (`keeper`).
struct Keeping {
    state: PathBuf,
    command: Vec<String>,
    keeper: Mutex<KeeperState>,
}

enum KeeperState {
    Configured,
    Running(keeper::NodeKeeper),
}

impl KeeperState {
    fn running(&self) -> Result<&keeper::NodeKeeper, Refusal> {
        match self {
            Self::Running(keeper) => Ok(keeper),
            Self::Configured => Err(preflight::invalid(
                "the keeper is not initialized after preflight",
            )),
        }
    }
}

impl Keeping {
    /// Retain the initialized launch configuration without starting a keeper or dispatching requests.
    /// Recorded's whole-owner preflight precedes the Configured -> Running transition.
    fn configured(state: &Path, command: &[String]) -> Result<Self, Refusal> {
        let bound = hex_core::hcs::COMMAND_LINE_UNITS;
        let units = command.iter().try_fold(0usize, |units, word| {
            units
                .checked_add(word.encode_utf16().count())?
                .checked_add(1)
        });
        if command.is_empty() || units.is_none_or(|units| units > bound) {
            return Err(preflight::invalid(
                "the keeper command is empty or exceeds the native command bound",
            ));
        }
        let mut copied = Vec::new();
        copied
            .try_reserve_exact(command.len())
            .map_err(|_| allocation(bound))?;
        for word in command {
            copied.push(copy_text(word)?);
        }
        Ok(Self {
            state: copy_path(state)?,
            command: copied,
            keeper: Mutex::new(KeeperState::Configured),
        })
    }

    fn adopt_or_spawn(state: &Path, command: &[String]) -> Result<keeper::NodeKeeper, Refusal> {
        if let Some((pid, created)) = read_keeper(state)
            && let Ok(k) = keeper::NodeKeeper::open(pid, created, state)
            && k.alive()
        {
            return Ok(k);
        }
        Self::spawn_at(state, command)
    }

    /// Starts a fresh keeper and records it, for [`Holding::ensure_keeper`] when the old one is gone.
    fn spawn(&self) -> Result<keeper::NodeKeeper, Refusal> {
        Self::spawn_at(&self.state, &self.command)
    }

    fn spawn_at(state: &Path, command: &[String]) -> Result<keeper::NodeKeeper, Refusal> {
        let ready = state.join(format!("keeper-ready-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        // The keeper outlives its node, so it inherits none of the node's handles. std's spawn hands a child
        // every inheritable handle its parent holds, the node's own stdout and stderr among them (which it
        // inherited), so the keeper is started as a box's member is ([`sys::create`]): only its three
        // standard handles inherited, each the NUL device, and assigned to no job of Hex's. What reads the
        // node's output (a supervisor, a test) then sees its end when the node ends. It says it is ready
        // through its ready file.
        let mut spawn = keeper_spawn(command, state, &ready)?;
        let Started {
            process, thread, ..
        } = sys::create(&mut spawn, &[]).map_err(|e| Refusal::os("starting the keeper", e))?;
        if let Err(e) = sys::resume(&thread) {
            let _ = sys::terminate_process(&process, 1);
            return Err(Refusal::os("starting the keeper", e));
        }
        drop(thread);
        // Wait for the keeper to write its identity, waiting on it between polls (no blocking sleep): a bound
        // of the node's EMPTY_PATIENCE, 60 s, at ticks of 10 ms.
        let mut ticks: u32 = 6_000;
        let (pid, created) = loop {
            if let Some((pid, created)) = read_ready(&ready) {
                break (pid, created);
            }
            if matches!(sys::wait(&process, 0), Ok(true)) {
                let code = sys::exit_code(&process)
                    .map_or_else(|e| format!("unread ({e})"), |c| c.to_string());
                return Err(Refusal::Broken(format!(
                    "the keeper exited before it was ready: exit code {code}"
                )));
            }
            ticks = match ticks.checked_sub(1) {
                Some(n) => n,
                None => {
                    let _ = sys::terminate_process(&process, 1);
                    return Err(Refusal::Os {
                        op: "starting the keeper",
                        why: "it didn't say it was ready in 60 s".to_owned(),
                    });
                }
            };
            // Wait up to 10 ms on the keeper; its ending wakes us, else the next poll reads the ready file.
            let _ = sys::wait(&process, 10);
        };
        let k = keeper::NodeKeeper::open(pid, created, state)?;
        write_keeper(state, pid, created).map_err(|e| Refusal::os("recording the keeper", e))?;
        Ok(k)
    }
}

/// The keeper's spawn: `command` (`hex keep`, or a test's own keeper body) with the node's environment and
/// the keeper's state folder and ready path added (`keeper::Args` reads them there, so the command names only
/// how to launch it), the node's working folder, and the NUL device as its three standard handles. A variable
/// whose name or value isn't Unicode, or whose name holds an `=` (Windows' per-drive folders, `=C:`), isn't
/// passed on. Refused for an empty command, or a state folder or ready path that isn't Unicode.
fn keeper_spawn(command: &[String], state: &Path, ready: &Path) -> Result<Spawn, Refusal> {
    if command.is_empty() {
        return Err(Refusal::Invalid {
            what: "keeper",
            why: "an empty keeper command".to_owned(),
        });
    }
    let unicode = |what: &str, p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Refusal::Invalid {
                what: "keeper",
                why: format!("its {what} {} isn't Unicode", p.display()),
            })
    };
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| {
            !k.is_empty()
                && !k.contains('=')
                && !k.eq_ignore_ascii_case("HEX_KEEP_STATE")
                && !k.eq_ignore_ascii_case("HEX_KEEP_READY")
        })
        .collect();
    env.push(("HEX_KEEP_STATE".to_owned(), unicode("state folder", state)?));
    env.push(("HEX_KEEP_READY".to_owned(), unicode("ready path", ready)?));
    Ok(Spawn {
        command_line: hex_core::job::command_line(command)
            .encode_utf16()
            .chain([0])
            .collect(),
        environment: hex_core::job::environment(&env),
        cwd: None,
        stdio: std_handles(Stdio::null())?,
    })
}

fn read_ready(path: &Path) -> Option<(u32, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut p = text.split_whitespace();
    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?))
}

fn read_keeper(state: &Path) -> Option<(u32, u64)> {
    read_ready(&keeper::keeper_file(state))
}

fn write_keeper(state: &Path, pid: u32, created: u64) -> std::io::Result<()> {
    std::fs::write(keeper::keeper_file(state), format!("{pid} {created}"))
}

/// Why this runtime alone refuses a Linux image once the Windows Hypervisor Platform is on: a Linux image
/// runs in the node's Shards guest, where a node built with [`crate::guest::WithGuest::routing`] by
/// [`WinRuntime::oci_side`] sends it (native plan step 5; docs/design/shards-guest.md).
const LINUX_IN_GUEST: &str = "a Linux image runs in the node's Shards guest through the Windows Hypervisor Platform, which this runtime alone doesn't boot (hex_runtime::guest::WithGuest routes it there)";

/// The Windows node's runtime, its boxes held in memory, keyed by id.
pub struct WinRuntime {
    /// The machine's logical processors, the CPU hard cap's denominator (read once at node start).
    processors: NonZeroU32,
    catalog: NativeCatalog,
    /// Where an `oci` box made from an `OciSpec` keeps its scratch, one folder per box.
    scratch_root: PathBuf,
    /// The folder holding the `hex` binary every `oci` box maps read-only as its launcher: the node's
    /// own folder unless told otherwise.
    launcher: Option<PathBuf>,
    /// Its boxes and their keeper, shared with a run's thread, which removes its box as `delete` does
    /// ([`Holding::remove`]).
    holding: Arc<Holding>,
    /// The node's name for its boxes' jobs, where its `process` boxes outlive it ([`WinRuntime::outliving`]):
    /// a box is the job `Local\hex-<node>-<box>`, which a node on the same name takes back. `None`: a
    /// runtime driven in process, whose boxes end with it (kill-on-close).
    node: Option<String>,
    /// The installer's features that are off, read when the runtime is made (node start), so no box's
    /// create waits on the query: a feature turns on or off only across a reboot, which starts the
    /// node again.
    features_off: OnceLock<BTreeSet<WindowsFeature>>,
    /// How many times [`features::off`] ran, for the test that no create runs it.
    #[cfg(test)]
    feature_reads: std::sync::atomic::AtomicU32,
    /// What reclaiming a killed node's scratch at start failed to do, each typed (its markers kept for
    /// the next start).
    reclaim_failures: Vec<Refusal>,
}

/// Legacy constructors advertise an empty catalog without allocating; configured runtimes retain
/// the caller's one fallibly allocated immutable catalog, also passed to the guest.
enum NativeCatalog {
    Empty(Catalog),
    Shared(SharedCatalog),
}
impl std::ops::Deref for NativeCatalog {
    type Target = Catalog;
    fn deref(&self) -> &Catalog {
        match self {
            Self::Empty(catalog) => catalog,
            Self::Shared(catalog) => catalog,
        }
    }
}

/// A runtime's boxes and the keeper holding their jobs: shared with a run's thread, which removes its box
/// through [`Holding::remove`] as the runtime's delete does.
struct Holding {
    boxes: Mutex<Vec<(OwnerKey, Slot)>>,
    /// Where its boxes outlive it: the keeper holding a second handle to each box's job, and the state
    /// folder their handle values are kept in. `None` for a runtime driven in process.
    keeper: Option<Keeping>,
}

impl Holding {
    /// Discharge this exact VM receipt only after a positive same-job empty observation. Pointer
    /// identity pins the native lifetime, so an old callback cannot remove a recreated same-ID slot.
    fn release_vm(&self, boxed: &JobBox) -> Result<(), Refusal> {
        if boxed.released() {
            return Ok(());
        }
        let id = boxed.owner();
        let held = {
            let mut boxes = self.boxes()?;
            let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
            let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
            let held = match &slot.state {
                SlotState::Ready(Held::Process(current))
                | SlotState::Finishing(Held::Process(current))
                | SlotState::CleanupPending(Held::Process(current))
                | SlotState::CancelFinishing(Held::Process(current))
                    if std::ptr::eq(NativeArc::as_ptr(current), boxed) && current.is_vm() =>
                {
                    Held::Process(NativeArc::clone(current))
                }
                _ => {
                    return Err(pending(
                        "the exact native VM receipt is busy or no longer held",
                    ));
                }
            };
            slot.state = SlotState::Removing(held.clone());
            held
        };
        let result = (|| {
            if boxed
                .scope_population()
                .map_err(proc::ObserveError::refusal)?
                != 0
            {
                return Err(pending("the native VM job still has a live member"));
            }
            self.drop_vm_from_keeper(id, boxed)
        })();
        let mut boxes = self.boxes()?;
        let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
        if !matches!(&slot.state, SlotState::Removing(Held::Process(current))
            if std::ptr::eq(NativeArc::as_ptr(current), boxed))
        {
            return Err(inconsistent_slots());
        }
        match result {
            Ok(()) => {
                boxed.mark_released()?;
                boxes.remove(at);
                Ok(())
            }
            Err(error) => {
                slot.state = SlotState::CleanupPending(held);
                Err(error)
            }
        }
    }

    fn boxes(&self) -> Result<std::sync::MutexGuard<'_, Vec<(OwnerKey, Slot)>>, Refusal> {
        self.boxes
            .lock()
            .map_err(|_| Refusal::Broken("the node's box map lock is poisoned".to_owned()))
    }

    fn reserve(
        &self,
        id: OwnerRef<'_>,
        devices: &[DeviceId],
        marker: Option<PathBuf>,
        route: Option<hex_api::RouteReceipt>,
    ) -> Result<(), Refusal> {
        let key = id.try_owned()?;
        let owned = copy_devices(devices)?;
        let mut selected = copy_devices(devices)?;
        selected.sort_unstable();
        let mut boxes = self.boxes()?;
        let at = match owner_index(&boxes, id) {
            Ok(_) => {
                return Err(match key {
                    OwnerKey::Box(id) => Refusal::Exists(id),
                    OwnerKey::Pool(_) => pending("the native pool owner is already held"),
                });
            }
            Err(at) => at,
        };
        if boxes.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        // Every retained ID is visited once, with binary lookup in the temporary sorted selection.
        // No prefix scan, stored reverse index or second holder ledger is introduced.
        for (_, slot) in boxes.iter() {
            if slot
                .devices
                .iter()
                .any(|id| selected.binary_search(id).is_ok())
            {
                return Err(pending("requested physical device is already held"));
            }
        }
        boxes
            .try_reserve(1)
            .map_err(|_| allocation(crate::MAX_BOXES))?;
        if at > boxes.len() {
            return Err(inconsistent_slots());
        }
        boxes.insert(
            at,
            (
                key,
                Slot {
                    devices: owned,
                    marker,
                    route,
                    keeper: KeeperHold::Unheld,
                    state: SlotState::Creating,
                },
            ),
        );
        Ok(())
    }

    fn unmade(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let at = slot_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get(at).ok_or_else(inconsistent_slots)?;
        if !slot.devices.is_empty()
            || !matches!(slot.state, SlotState::Creating | SlotState::Cancelled)
        {
            return Err(inconsistent_slots());
        }
        boxes.remove(at);
        Ok(())
    }

    fn made(&self, id: OwnerRef<'_>, held: Held) -> Result<bool, Refusal> {
        let mut boxes = self.boxes()?;
        let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
        let cancelled = match slot.state {
            SlotState::Creating => false,
            SlotState::Cancelled => true,
            _ => return Err(inconsistent_slots()),
        };
        slot.state = if cancelled {
            SlotState::CleanupPending(held)
        } else {
            SlotState::Finishing(held)
        };
        Ok(cancelled)
    }

    fn ready(&self, id: OwnerRef<'_>) -> Result<bool, Refusal> {
        let mut boxes = self.boxes()?;
        let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
        match &slot.state {
            SlotState::Finishing(held) => {
                slot.state = SlotState::Ready(held.clone());
                Ok(true)
            }
            SlotState::CancelFinishing(held) => {
                slot.state = SlotState::CleanupPending(held.clone());
                Ok(false)
            }
            _ => Err(inconsistent_slots()),
        }
    }

    fn uncertain(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
        slot.state = match &slot.state {
            SlotState::Creating | SlotState::Cancelled => SlotState::Uncertain,
            SlotState::Finishing(held) | SlotState::CancelFinishing(held) => {
                SlotState::CleanupPending(held.clone())
            }
            // A creator cancelled while finishing has already handed its box to cleanup (`ready`).
            SlotState::CleanupPending(_) => return Ok(()),
            _ => return Err(inconsistent_slots()),
        };
        Ok(())
    }

    /// The uncertainty mark of a path that failed with `failure`. A refused mark leaves the slot
    /// unmarked, so it returns beside `failure`, typed as the node's broken state.
    fn uncertain_after(&self, id: OwnerRef<'_>, failure: &Refusal) -> Result<(), Refusal> {
        self.uncertain(id).map_err(|mark| {
            Refusal::Broken(format!(
                "{failure}; its native slot's uncertainty couldn't be recorded ({mark})"
            ))
        })
    }

    /// The uncertainty mark of THIS creator's VM slot after `failure`. It is keyed by the creator's own
    /// job receipt, not by its owner's name: a slot a newer creator holds (this one's released and the
    /// owner relaunched) is never marked, and a slot already gone, already cleanup, or being removed by
    /// its own release needs no mark. A refused mark returns beside `failure`.
    fn uncertain_vm_after(&self, boxed: &JobBox, failure: &Refusal) -> Result<(), Refusal> {
        let mark = (|| {
            let mut boxes = self.boxes()?;
            let Ok(at) = owner_index(&boxes, boxed.owner()) else {
                return Ok(());
            };
            let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
            let own = slot.physical().is_some_and(|held| {
                matches!(held, Held::Process(current) if std::ptr::eq(NativeArc::as_ptr(current), boxed))
            });
            if !own {
                return Ok(());
            }
            slot.state = match &slot.state {
                SlotState::Finishing(held) | SlotState::CancelFinishing(held) => {
                    SlotState::CleanupPending(held.clone())
                }
                SlotState::CleanupPending(_) | SlotState::Removing(_) => return Ok(()),
                _ => return Err(inconsistent_slots()),
            };
            Ok(())
        })();
        mark.map_err(|mark| {
            Refusal::Broken(format!(
                "{failure}; its native slot's uncertainty couldn't be recorded ({mark})"
            ))
        })
    }

    /// Only a fresh assigned create returning before HcsCreateComputeSystem can take this path.
    /// Pending/terminal native attempts return an owned OciBox, even when publication must refuse.
    fn preparation_failed(&self, id: &BoxId) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let at = slot_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
        if !matches!(slot.state, SlotState::Creating | SlotState::Cancelled) {
            return Err(inconsistent_slots());
        }
        slot.state = SlotState::PreparedCleanupPending;
        Ok(())
    }

    fn keeper_receipt(&self, id: OwnerRef<'_>, receipt: KeeperHold) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        boxes.get_mut(at).ok_or_else(inconsistent_slots)?.1.keeper = receipt;
        Ok(())
    }

    /// Only fully published boxes are re-held. A creator hands its own Finishing box over once.
    fn ensure_keeper(&self, keeping: &Keeping) -> Result<(), Refusal> {
        let mut keeper = keeping
            .keeper
            .lock()
            .map_err(|_| Refusal::Broken("the node's keeper lock is poisoned".to_owned()))?;
        if matches!(&*keeper, KeeperState::Running(keeper) if keeper.alive()) {
            return Ok(());
        }
        let next = match &*keeper {
            KeeperState::Configured => Keeping::adopt_or_spawn(&keeping.state, &keeping.command)?,
            KeeperState::Running(_) => keeping.spawn()?,
        };
        *keeper = KeeperState::Running(next);
        let keeper = keeper.running()?;
        self.rehold_ready_using(
            keeper.pid(),
            keeper.created(),
            |held| keeper.hold(held.job_token()?),
            |id, receipt| self.write_keeper_receipt(keeping, id, receipt),
        )
    }

    fn write_keeper_receipt(
        &self,
        keeping: &Keeping,
        id: OwnerRef<'_>,
        receipt: KeeperReceipt,
    ) -> Result<(), Refusal> {
        let commitment = {
            let boxes = self.boxes()?;
            let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
            let (_, slot) = boxes.get(at).ok_or_else(inconsistent_slots)?;
            match slot.physical() {
                Some(Held::Process(boxed)) => boxed.vm_commitment(),
                Some(Held::Oci(_)) => None,
                None => return Err(inconsistent_slots()),
            }
        };
        if let Some((scope, commitment)) = commitment {
            let path = keeper::vm::path(&keeping.state, id)?;
            keeper::vm::write(
                &path,
                &keeper::vm::Held {
                    scope,
                    commitment,
                    pid: receipt.pid,
                    started: receipt.created,
                    value: receipt.value,
                },
            )
        } else {
            let OwnerRef::Box(id) = id else {
                return Err(inconsistent_slots());
            };
            keeper::write_held(
                &keeping.state,
                id.as_str(),
                receipt.pid,
                receipt.created,
                receipt.value,
            )
            .map_err(|error| Refusal::os("recording a box's keeper handle", error))
        }
    }

    /// The caller holds the keeper mutex through replacement and handoff. Removal may mark a slot
    /// Removing meanwhile, but must acquire that same keeper before closing any remote receipt.
    fn rehold_ready_using(
        &self,
        pid: u32,
        created: u64,
        mut hold: impl FnMut(&Held) -> Result<usize, Refusal>,
        mut record: impl FnMut(OwnerRef<'_>, KeeperReceipt) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let ready = {
            let boxes = self.boxes()?;
            let mut ready = Vec::new();
            ready
                .try_reserve_exact(boxes.len())
                .map_err(|_| allocation(boxes.len()))?;
            for (id, slot) in boxes.iter() {
                if let SlotState::Ready(held) = &slot.state {
                    ready.push((id.try_clone()?, held.clone()));
                }
            }
            ready
        };
        for (id, held) in ready {
            let receipt = KeeperReceipt {
                pid,
                created,
                value: hold(&held)?,
            };
            // Store ownership before disk I/O. A failed record write retains the exact remote handle.
            self.keeper_receipt(id.as_ref(), KeeperHold::Held(receipt))?;
            record(id.as_ref(), receipt)?;
        }
        Ok(())
    }

    fn keeping(&self) -> Result<Option<&Keeping>, Refusal> {
        let Some(keeping) = self.keeper.as_ref() else {
            return Ok(None);
        };
        self.ensure_keeper(keeping)?;
        Ok(Some(keeping))
    }

    fn keep(&self, id: OwnerRef<'_>, held: &Held) -> Result<(), Refusal> {
        if let Held::Process(boxed) = held
            && boxed.is_vm()
        {
            return self.keep_vm(id, boxed);
        }
        let OwnerRef::Box(id) = id else {
            return Err(inconsistent_slots());
        };
        let Some(keeping) = self.keeping()? else {
            return Ok(());
        };
        let keeper = keeping
            .keeper
            .lock()
            .map_err(|_| Refusal::Broken("the node's keeper lock is poisoned".to_owned()))?;
        let keeper = keeper.running()?;
        let receipt = match keeper::read_held(&keeping.state, id.as_str()) {
            Some(old) if old.held_by(keeper.pid(), keeper.created()) => {
                if let Held::Process(boxed) = held
                    && boxed.is_vm()
                    && !sys::same_in(keeper.process(), old.value, boxed.job()?).map_err(
                        |error| Refusal::os("checking the original VM keeper job", error),
                    )?
                {
                    self.keeper_receipt(OwnerRef::Box(id), KeeperHold::Uncertain)?;
                    return Err(pending(
                        "the VM keeper value names a different kernel object",
                    ));
                }
                KeeperReceipt {
                    pid: old.keeper_pid,
                    created: old.keeper_created,
                    value: old.value,
                }
            }
            _ => KeeperReceipt {
                pid: keeper.pid(),
                created: keeper.created(),
                value: keeper.hold(held.job_token()?)?,
            },
        };
        // Save the exact owned remote handle before the file write: an error cannot lose its cleanup.
        self.keeper_receipt(OwnerRef::Box(id), KeeperHold::Held(receipt))?;
        keeper::write_held(
            &keeping.state,
            id.as_str(),
            receipt.pid,
            receipt.created,
            receipt.value,
        )
        .map_err(|e| Refusal::os("recording a box's keeper handle", e))
    }

    fn keep_vm(&self, id: OwnerRef<'_>, boxed: &JobBox) -> Result<(), Refusal> {
        let Some(keeping) = self.keeping()? else {
            return Ok(());
        };
        let (scope, commitment) = boxed
            .vm_commitment()
            .ok_or_else(|| pending("VM scope commitment is absent"))?;
        let path = keeper::vm::path(&keeping.state, id)?;
        let keeper = keeping
            .keeper
            .lock()
            .map_err(|_| pending("the native keeper lock is poisoned"))?;
        let keeper = keeper.running()?;
        let receipt = match keeper::vm::read(&path)? {
            Some(old) => {
                if old.scope != scope
                    || old.commitment != commitment
                    || old.pid != keeper.pid()
                    || old.started != keeper.created()
                {
                    return Err(pending(
                        "the retained VM keeper receipt names another native lifetime",
                    ));
                }
                if !sys::same_in(keeper.process(), old.value, boxed.job()?).map_err(|error| {
                    Refusal::os("checking the committed VM keeper object", error)
                })? {
                    self.keeper_receipt(id, KeeperHold::Uncertain)?;
                    return Err(pending(
                        "the committed keeper value names another kernel object",
                    ));
                }
                KeeperReceipt {
                    pid: old.pid,
                    created: old.started,
                    value: old.value,
                }
            }
            None => KeeperReceipt {
                pid: keeper.pid(),
                created: keeper.created(),
                value: keeper.hold(boxed.job_token()?)?,
            },
        };
        self.keeper_receipt(id, KeeperHold::Held(receipt))?;
        keeper::vm::write(
            &path,
            &keeper::vm::Held {
                scope,
                commitment,
                pid: receipt.pid,
                started: receipt.created,
                value: receipt.value,
            },
        )
    }

    fn drop_from_keeper(&self, id: &BoxId) -> Result<(), Refusal> {
        let Some(keeping) = self.keeping()? else {
            return Ok(());
        };
        let known = {
            let boxes = self.boxes()?;
            slot_index(&boxes, id)
                .ok()
                .and_then(|at| boxes.get(at))
                .map(|(_, slot)| slot.keeper)
        };
        let receipt = match known {
            Some(KeeperHold::Uncertain) => {
                return Err(pending(
                    "keeper close outcome is uncertain; resources remain held",
                ));
            }
            Some(KeeperHold::Closed | KeeperHold::Unheld) => None,
            Some(KeeperHold::Held(receipt)) => Some(receipt),
            None => keeper::read_held(&keeping.state, id.as_str()).map(|old| KeeperReceipt {
                pid: old.keeper_pid,
                created: old.keeper_created,
                value: old.value,
            }),
        };
        if let Some(receipt) = receipt {
            let keeper = keeping
                .keeper
                .lock()
                .map_err(|_| Refusal::Broken("the node's keeper lock is poisoned".to_owned()))?;
            let keeper = keeper.running()?;
            if receipt.pid == keeper.pid()
                && receipt.created == keeper.created()
                && let Err(error) = keeper.drop_held(receipt.value)
            {
                if known.is_some() {
                    self.keeper_receipt(OwnerRef::Box(id), KeeperHold::Uncertain)?;
                }
                return Err(error);
            }
            if known.is_some() {
                self.keeper_receipt(OwnerRef::Box(id), KeeperHold::Closed)?;
            }
        }
        match std::fs::remove_file(keeper::held_file(&keeping.state, id.as_str())) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Refusal::os("removing a box's keeper handle record", e)),
        }
    }

    fn drop_vm_from_keeper(&self, id: OwnerRef<'_>, boxed: &JobBox) -> Result<(), Refusal> {
        let Some(keeping) = self.keeper.as_ref() else {
            return Ok(());
        };
        let state = {
            let boxes = self.boxes()?;
            let at = owner_index(&boxes, id).map_err(|_| inconsistent_slots())?;
            boxes.get(at).ok_or_else(inconsistent_slots)?.1.keeper
        };
        let receipt = match state {
            KeeperHold::Held(receipt) => receipt,
            KeeperHold::Uncertain => return Err(pending("native VM keeper disposal is uncertain")),
            KeeperHold::Closed => return remove_vm_keeper_file(&keeping.state, id, boxed),
            KeeperHold::Unheld => {
                if keeper::vm::read(&keeper::vm::path(&keeping.state, id)?)?.is_some() {
                    return Err(pending(
                        "unadopted VM keeper metadata has no native ownership proof",
                    ));
                }
                return Ok(());
            }
        };
        // The same existing lock serializes observation and remote close. A keeper restart or a
        // reused value never supplies authority over another job's remote handle.
        let current = keeping
            .keeper
            .lock()
            .map_err(|_| pending("native keeper lock is poisoned"))?;
        let native = current.running()?;
        let previous;
        let process = if native.pid() == receipt.pid && native.created() == receipt.created {
            Some(native.process())
        } else {
            previous = sys::open_process(receipt.pid)
                .map_err(|error| Refusal::os("observing the original VM keeper", error))?;
            match previous.as_ref() {
                Some(process)
                    if sys::created(process).map_err(|error| {
                        Refusal::os("checking the original VM keeper birth", error)
                    })? == receipt.created =>
                {
                    Some(process)
                }
                _ => None,
            }
        };
        if let Some(process) = process
            && !sys::wait(process, 0)
                .map_err(|error| Refusal::os("observing the original VM keeper end", error))?
        {
            match sys::close_in_if_same(process, receipt.value, boxed.job()?) {
                Ok(true) => {}
                Ok(false) => {
                    self.keeper_receipt(id, KeeperHold::Uncertain)?;
                    return Err(pending(
                        "VM keeper handle no longer names the owned kernel job",
                    ));
                }
                Err(error) => {
                    self.keeper_receipt(id, KeeperHold::Uncertain)?;
                    return Err(Refusal::os("disposing the owned VM keeper job", error));
                }
            }
        }
        self.keeper_receipt(id, KeeperHold::Closed)?;
        drop(current);
        remove_vm_keeper_file(&keeping.state, id, boxed)
    }

    /// Keep the slot through every external cleanup operation; only an actual successful removal
    /// releases claims. Concurrent cancellation requests never remove a creator's slot underneath it.
    fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
        self.remove_using(id, oci::end)
    }

    fn remove_using(
        &self,
        id: &BoxId,
        end: impl FnOnce(&Path) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        let (removal, marker) = {
            let mut boxes = self.boxes()?;
            let at = slot_index(&boxes, id).map_err(|_| Refusal::NoSuchBox(id.clone()))?;
            let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
            let removal = match &slot.state {
                SlotState::Creating => {
                    slot.state = SlotState::Cancelled;
                    return Err(pending(
                        "creation cancellation requested; cleanup is pending",
                    ));
                }
                SlotState::Cancelled | SlotState::CancelFinishing(_) => {
                    return Err(pending("creation cancellation is pending"));
                }
                SlotState::Finishing(held) => {
                    slot.state = SlotState::CancelFinishing(held.clone());
                    return Err(pending(
                        "creation cancellation requested; keeper handoff is pending",
                    ));
                }
                SlotState::Ready(held) | SlotState::CleanupPending(held) => {
                    Removal::Physical(held.clone())
                }
                SlotState::PreparedCleanupPending => Removal::Prepared,
                SlotState::Uncertain if slot.devices.is_empty() => Removal::UnknownLegacy,
                SlotState::Uncertain => Removal::UnknownAssigned,
                SlotState::Removing(_)
                | SlotState::RemovingUncertain
                | SlotState::RemovingPrepared => {
                    return Err(pending("physical cleanup is already in progress"));
                }
            };
            let marker = slot.marker.as_deref().map(copy_path).transpose()?;
            slot.state = match &removal {
                Removal::Physical(held) => SlotState::Removing(held.clone()),
                Removal::Prepared => SlotState::RemovingPrepared,
                Removal::UnknownAssigned | Removal::UnknownLegacy => SlotState::RemovingUncertain,
            };
            (removal, marker)
        };
        let result = (|| {
            self.drop_from_keeper(id)?;
            match &removal {
                Removal::Physical(Held::Process(boxed)) => boxed.delete(),
                Removal::Physical(Held::Oci(boxed)) => boxed.delete(),
                Removal::Prepared => oci::discard_prepared(
                    marker
                        .as_deref()
                        .ok_or_else(|| pending("prepared native cleanup has no marker path"))?,
                ),
                Removal::UnknownAssigned | Removal::UnknownLegacy => {
                    let marker = marker.as_deref().ok_or_else(|| {
                        pending("no durable native marker establishes the create outcome")
                    })?;
                    if marker.exists() {
                        let record = oci::read_marker(marker)?;
                        if (matches!(removal, Removal::UnknownAssigned)
                            || !record.devices.is_empty())
                            && record.system.as_ref() != Some(id)
                        {
                            return Err(pending(
                                "native marker system identity disagrees; cleanup is uncertain",
                            ));
                        }
                        end(marker)
                    } else {
                        oci::require_absent(id)?;
                        if matches!(removal, Removal::UnknownAssigned) {
                            Err(pending(
                                "assigned native marker is missing; terminal create state is unproved",
                            ))
                        } else {
                            Ok(())
                        }
                    }
                }
            }
        })();
        let mut boxes = self.boxes()?;
        let at = slot_index(&boxes, id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get_mut(at).ok_or_else(inconsistent_slots)?;
        if result.is_ok() {
            if boxes.get(at).is_none() {
                return Err(inconsistent_slots());
            }
            boxes.remove(at);
        } else {
            slot.state = match removal {
                Removal::Physical(held) => SlotState::CleanupPending(held),
                Removal::Prepared => SlotState::PreparedCleanupPending,
                Removal::UnknownAssigned | Removal::UnknownLegacy => SlotState::Uncertain,
            };
        }
        result
    }

    fn report(&self, catalog: &Catalog) -> Result<Vec<DeviceInfo>, Refusal> {
        let boxes = self.boxes()?;
        let count = boxes.iter().try_fold(0usize, |n, (_, slot)| {
            n.checked_add(slot.devices.len())
                .ok_or_else(|| allocation(hex_api::MAX_DEVICES))
        })?;
        if count > hex_api::MAX_DEVICES {
            return Err(inconsistent_slots());
        }
        let mut holders = Vec::new();
        holders
            .try_reserve_exact(count)
            .map_err(|_| allocation(count))?;
        for (id, slot) in boxes.iter() {
            if let Some(Held::Oci(boxed)) = slot.physical() {
                let selected = boxed.device_bindings();
                if selected.len() != slot.devices.len()
                    || !selected
                        .iter()
                        .zip(&slot.devices)
                        .all(|(binding, id)| binding.id == *id)
                {
                    return Err(inconsistent_slots());
                }
            }
            if !slot.devices.is_empty() {
                let OwnerKey::Box(id) = id else {
                    return Err(inconsistent_slots());
                };
                holders.extend(slot.devices.iter().map(|device| (*device, id)));
            }
        }
        holders.sort_unstable_by_key(|(device, _)| *device);
        if holders
            .windows(2)
            .any(|pair| pair.first().map(|p| p.0) == pair.last().map(|p| p.0))
        {
            return Err(inconsistent_slots());
        }
        let mut report = Vec::new();
        report
            .try_reserve_exact(catalog.entries().len())
            .map_err(|_| allocation(catalog.entries().len()))?;
        for entry in catalog.entries() {
            if entry.binding(Route::Hcs).is_some() {
                let holder = holders
                    .binary_search_by_key(&entry.info.id, |(device, _)| *device)
                    .ok()
                    .and_then(|at| holders.get(at))
                    .map(|(_, id)| *id);
                let holder = holder
                    .map(|id| id.try_clone().map(hex_api::DeviceHolder::Box))
                    .transpose()?;
                report.push(entry.report(holder)?);
            }
        }
        Ok(report)
    }
}

fn copy_path(path: &Path) -> Result<PathBuf, Refusal> {
    let mut copy = std::ffi::OsString::new();
    copy.try_reserve_exact(path.as_os_str().len())
        .map_err(|_| allocation(path.as_os_str().len()))?;
    copy.push(path.as_os_str());
    Ok(PathBuf::from(copy))
}

impl WinRuntime {
    /// A runtime for this machine, its processor count read from Windows, its `oci` scratch under
    /// `%ProgramData%\hex\boxes`.
    pub fn new() -> Self {
        Self::with(sys::active_processors(), default_scratch_root())
    }

    /// A runtime told the machine has `processors` logical processors (at least one), keeping `oci`
    /// scratch under `scratch_root`, first reclaiming what a killed node left there ([`oci::reclaim`]).
    /// The installer's features are read here, before any box, so the first `oci` create doesn't wait
    /// on PowerShell's query (p50 1,183 ms on jupiter, bench/install/records/windows-jupiter-20261105.txt
    /// entry 19). A read that fails here isn't lost: the first box that needs the features reads them
    /// again, and is refused by name if that fails too.
    pub fn with(processors: u32, scratch_root: PathBuf) -> Self {
        let reclaim_failures = oci::reclaim(&scratch_root);
        Self::configured_runtime(processors, scratch_root, reclaim_failures)
    }

    fn configured_runtime(
        processors: u32,
        scratch_root: PathBuf,
        reclaim_failures: Vec<Refusal>,
    ) -> Self {
        let runtime = Self {
            processors: NonZeroU32::new(processors).unwrap_or(NonZeroU32::MIN),
            catalog: NativeCatalog::Empty(Catalog::default()),
            scratch_root,
            launcher: None,
            holding: Arc::new(Holding {
                boxes: Mutex::new(Vec::new()),
                keeper: None,
            }),
            node: None,
            features_off: OnceLock::new(),
            #[cfg(test)]
            feature_reads: std::sync::atomic::AtomicU32::new(0),
            reclaim_failures,
        };
        // Read now so no create waits on it; a failure is read again, and refused, at the first ask.
        let _ = runtime.features_off();
        // Warm only the SAME DLL/export owner. Its pure initialized view bounds possible route
        // encoding; actual availability continues to query the service at its existing seam.
        oci::initialize_api();
        runtime
    }

    /// A node's runtime on this machine whose boxes outlive it: each box's job is the one named for `node`
    /// and the box, kill-on-close, held by the node and by a keeper (`keeper`, a command line started apart
    /// from the node, its state in `dir`), and an `oci` box's container outlives the node with it (HCS
    /// doesn't end it with its last handle), so a node killed with boxes running leaves them to the keeper,
    /// and the next node on the same name takes them back ([`WinRuntime::adopt`]) or ends them
    /// ([`WinRuntime::end`]). With node and keeper both gone, kill-on-close ends them. `node` names one
    /// node's boxes apart from another's in the session. Its `oci` boxes' records and spec-made scratch are
    /// its own, under `dir\boxes`, so its start-time reclaim sweeps no other runtime's boxes (the machine's
    /// shared `%ProgramData%\hex\boxes` holds those of runtimes driven in process, a bench's among them).
    pub fn outliving(node: &str, dir: &Path, keeper: &[String]) -> Result<Self, Refusal> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Refusal::os("making the node's state folder", e))?;
        let keeping = Keeping::configured(dir, keeper)?;
        let mut runtime =
            Self::configured_runtime(sys::active_processors(), dir.join("boxes"), Vec::new());
        runtime.node = Some(node.to_owned());
        // Nothing shares the runtime's holding yet, so its keeper is set in place.
        Arc::get_mut(&mut runtime.holding)
            .ok_or_else(|| Refusal::Broken("a new runtime's holding was shared".to_owned()))?
            .keeper = Some(keeping);
        Ok(runtime)
    }

    /// Loads the shared administrator catalog before starting a keeper or creating native state.
    pub fn outliving_with_devices(
        node: &str,
        dir: &Path,
        keeper: &[String],
        path: &Path,
    ) -> Result<Self, Refusal> {
        let catalog = SharedCatalog::try_new(Catalog::open(path)?)
            .map_err(|_| allocation(std::mem::size_of::<Catalog>()))?;
        Self::outliving_with_catalog(node, dir, keeper, catalog)
    }

    pub fn outliving_with_catalog(
        node: &str,
        dir: &Path,
        keeper: &[String],
        catalog: SharedCatalog,
    ) -> Result<Self, Refusal> {
        let mut runtime = Self::outliving(node, dir, keeper)?;
        runtime.catalog = NativeCatalog::Shared(catalog);
        Ok(runtime)
    }

    pub fn device_catalog(&self) -> Option<SharedCatalog> {
        match &self.catalog {
            NativeCatalog::Shared(catalog) => Some(catalog.clone()),
            NativeCatalog::Empty(_) => None,
        }
    }

    /// A conservative encoding reservation, never fresh advertised OCI eligibility. It includes
    /// the possible native OCI form even if the service query is currently unavailable, so later
    /// service recovery cannot exceed the route protocol bound. No OS query occurs here.
    pub fn project_native_encoding_upper_bound(
        &self,
        projection: &mut crate::devices::RouteProjection,
    ) -> Result<(), Refusal> {
        native_encoding_upper_bound(
            oci::initialized_api_loaded(),
            self.features_off.get(),
            projection,
        )
    }

    fn bindings(&self, spec: &BoxSpec) -> Result<Vec<hex_core::hcs::DeviceBinding>, Refusal> {
        spec.check()?;
        if matches!(spec.class, ClassSpec::Process) {
            spec.check_no_devices("windows process")?;
        }
        let mut index: Vec<&crate::devices::Entry> = Vec::new();
        index
            .try_reserve_exact(self.catalog.entries().len())
            .map_err(|_| allocation(self.catalog.entries().len()))?;
        index.extend(self.catalog.entries());
        index.sort_unstable_by_key(|entry| entry.info.id);
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(spec.devices.len())
            .map_err(|_| allocation(spec.devices.len()))?;
        for id in &spec.devices {
            let at = index
                .binary_search_by_key(id, |entry| entry.info.id)
                .map_err(|_| Refusal::Invalid {
                    what: "device attachment",
                    why: "requested identity has no configured native binding".to_owned(),
                })?;
            let entry = index.get(at).ok_or_else(inconsistent_slots)?;
            let Some(Binding::Hcs { location }) = entry.binding(Route::Hcs) else {
                return Err(Refusal::Platform {
                    what: "device attachment",
                    platform: "windows OCI",
                    why: "selected binding is not an exact HCS location".to_owned(),
                });
            };
            bindings.push(hex_core::hcs::DeviceBinding {
                id: *id,
                location: copy_text(location)?,
            });
        }
        Ok(bindings)
    }

    fn validate_bindings(
        &self,
        spec: &BoxSpec,
        record: &hex_core::hcs::BoxRecord,
    ) -> Result<(), Refusal> {
        let configured = self.bindings(spec)?;
        if record.devices != configured
            || (!record.devices.is_empty() && record.system.as_ref() != Some(&spec.id))
        {
            return Err(Refusal::Invalid{what:"native device recovery",why:"recorded physical identity or exact location disagrees with the current configuration".to_owned()});
        }
        oci::validate_storage(record, &self.container(spec)?.scratch)
    }

    /// Contradictory or absent native metadata cannot free either recorded or declared claims: their
    /// union stays held in this box's slot, marked uncertain, and the caller gets `failure`. A slot
    /// the box already has (an earlier alive or adopt of it) is never reserved twice or changed: it
    /// retains them only if it holds every one. A retention refused returns beside `failure`, never
    /// in its place.
    fn retain_recovery(
        &self,
        spec: &BoxSpec,
        record: &hex_core::hcs::BoxRecord,
        failure: Refusal,
    ) -> Refusal {
        match self.retain_claims(spec, record) {
            Ok(()) => failure,
            Err(retained) => Refusal::Broken(format!(
                "{failure}; its recorded claims couldn't be retained ({retained})"
            )),
        }
    }

    fn retain_claims(
        &self,
        spec: &BoxSpec,
        record: &hex_core::hcs::BoxRecord,
    ) -> Result<(), Refusal> {
        // The temporary union is bounded by the two existing u16-index lists, not a new admission cap.
        let count = record
            .devices
            .len()
            .checked_add(spec.devices.len())
            .ok_or_else(|| allocation(hex_api::MAX_DEVICES))?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(count)
            .map_err(|_| allocation(count))?;
        ids.extend(record.devices.iter().map(|binding| binding.id));
        ids.extend_from_slice(&spec.devices);
        ids.sort_unstable();
        ids.dedup();
        {
            let boxes = self.holding.boxes()?;
            if let Ok(at) = slot_index(&boxes, &spec.id) {
                let (_, slot) = boxes.get(at).ok_or_else(inconsistent_slots)?;
                return if ids.iter().all(|id| slot.devices.contains(id)) {
                    Ok(())
                } else {
                    Err(pending(
                        "this box's existing native slot doesn't hold all its recorded claims",
                    ))
                };
            }
        }
        self.holding.reserve(
            OwnerRef::Box(&spec.id),
            &ids,
            Some(self.marker(&spec.id)?),
            spec.route,
        )?;
        self.holding.uncertain(OwnerRef::Box(&spec.id))
    }

    /// The pid of the keeper holding this node's boxes' jobs, where its boxes outlive it; a keeper found
    /// dead is replaced first (the node's own watch does this on its port; here, for a runtime driven in
    /// process, it is done when asked).
    pub fn keeper_pid(&self) -> Option<u32> {
        let keeping = self.holding.keeper.as_ref()?;
        self.holding.ensure_keeper(keeping).ok()?;
        Some(keeping.keeper.lock().ok()?.running().ok()?.pid())
    }

    /// The job name a node's box `id` has (`Local\hex-<node>-<box>`), where its boxes outlive it.
    fn job_name(&self, id: &BoxId) -> Option<String> {
        self.node
            .as_ref()
            .map(|node| format!("Local\\hex-{node}-{id}"))
    }

    pub fn check_vm_cost(&self, cost: &hex_api::RuntimeCost) -> Result<(), Refusal> {
        managed::check_cost(self, cost)
    }

    pub fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &crate::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: std::time::Duration,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        managed::launch(self, record, plan, io, patience, publish)
    }

    pub fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        managed::attach(self, record, identity)
    }

    pub fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        managed::finalize(self, record, identity)
    }

    /// Joint plan step 14 (C4 on Windows): the managed VM's job is capped at `rate` milli-CPU, its share as the node's
    /// charge learner water-fills it.
    pub fn set_managed_rate(&self, record: NativeRecord<'_>, rate: u64) -> Result<(), Refusal> {
        managed::set_rate(self, record, rate)
    }

    /// Joint plan step 14 (C5 on Windows): the node's last resort ends the managed VM through its receipt, the job this
    /// node holds.
    pub fn stop_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        managed::stop(self, record)
    }
    pub fn managed_usage(&self, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
        managed::usage(self, record)
    }

    /// Whether this node takes back box `spec` a node before it made (native plan step 8, T-T7.1.2; asked
    /// by the node's recovery before it takes a box back or removes it): for an `oci` box, whether its
    /// record names a box [`WinRuntime::adopt`] takes back ([`oci::alive`]); `None` for a `process` box,
    /// which has no init.
    /// Validate the persisted route before an ordinary native create, adoption or cleanup.
    pub fn check_native_spec(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        spec.check()?;
        if !matches!(
            spec.class,
            ClassSpec::Process | ClassSpec::Oci(_) | ClassSpec::Bundle(_)
        ) {
            return Err(wrong_backend(spec));
        }
        if let Some(route) = spec.route {
            #[cfg(target_arch = "x86_64")]
            let arch = hex_api::ImageArch::Amd64;
            #[cfg(target_arch = "aarch64")]
            let arch = hex_api::ImageArch::Arm64;
            if route.key.execution != hex_api::Execution::Native
                || route.key.image.os != hex_api::ImageOs::Windows
                || route.key.image.arch != arch
            {
                return Err(Refusal::Invalid {
                    what: "Windows native route",
                    why: copy_text(
                        "the committed execution or image platform belongs to another initialized backend",
                    )?,
                });
            }
        }
        Ok(())
    }

    pub fn alive(&self, spec: &BoxSpec) -> Result<Option<bool>, Refusal> {
        self.check_native_spec(spec)?;
        match &spec.class {
            ClassSpec::Process => {
                spec.check_no_devices("windows process")?;
                Ok(None)
            }
            ClassSpec::Oci(_) | ClassSpec::Bundle(_) => {
                let marker = self.marker(&spec.id)?;
                if marker.exists() {
                    let record = oci::read_marker(&marker)?;
                    if let Err(error) = self.validate_bindings(spec, &record) {
                        return Err(self.retain_recovery(spec, &record, error));
                    }
                } else if !spec.devices.is_empty() {
                    return Err(pending(
                        "device-bearing recovery has no native marker; absence is unproved",
                    ));
                }
                oci::alive(&marker).map(Some)
            }
            ClassSpec::Firecracker(_) | ClassSpec::Shards(_) | ClassSpec::ShardsImage(_) => {
                Err(wrong_backend(spec))
            }
        }
    }

    /// Takes back box `spec` a node before this one made: a `process` box with whatever runs in it (made
    /// again, empty, where nothing does); an `oci` box with its container, scratch and init, refused by name
    /// where any of them is gone ([`OciBox::take_back`]), for recovery to end it. Refused for a runtime whose
    /// boxes don't outlive it.
    pub fn adopt(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        self.check_native_spec(spec)?;
        if matches!(spec.class, ClassSpec::Process) {
            spec.check_no_devices("windows process")?;
        }
        let name = self
            .job_name(&spec.id)
            .ok_or_else(|| pending("this runtime has no outliving boxes to adopt"))?;
        let marker = if matches!(spec.class, ClassSpec::Process) {
            None
        } else {
            Some(self.marker(&spec.id)?)
        };
        if let Some(marker) = marker.as_ref() {
            let record = oci::read_marker(marker)?;
            if let Err(error) = self.validate_bindings(spec, &record) {
                return Err(self.retain_recovery(spec, &record, error));
            }
        }
        self.holding
            .reserve(OwnerRef::Box(&spec.id), &spec.devices, marker, spec.route)?;
        let made = match &spec.class {
            ClassSpec::Process => {
                JobBox::take_back(spec.id.try_clone()?, &spec.bounds, self.processors, &name)
                    .and_then(native_held)
            }
            _ => self
                .take_back_oci(spec, &name)
                .map(|b| Held::Oci(Arc::new(b))),
        };
        self.finish_make(&spec.id, made).map(drop)
    }

    /// Ends what still runs of box `id` a node before this one made, for a box recovery removes: its job's
    /// members, and an `oci` box's container, scratch and record too ([`oci::end`]); one that left nothing
    /// has nothing to end. A runtime whose boxes end with it has none to end.
    pub fn end(&self, id: &BoxId) -> Result<(), Refusal> {
        if slot_index(self.boxes()?.as_slice(), id).is_ok() {
            return self.holding.remove(id);
        }
        match self.job_name(id) {
            Some(name) => {
                let marker = self.marker(id)?;
                if marker.exists() {
                    let record = oci::read_marker(&marker)?;
                    if !record.devices.is_empty() {
                        if record.system.as_ref() != Some(id) {
                            return Err(pending(
                                "native marker system identity disagrees; cleanup is uncertain",
                            ));
                        }
                        let mut devices = Vec::new();
                        devices
                            .try_reserve_exact(record.devices.len())
                            .map_err(|_| allocation(record.devices.len()))?;
                        devices.extend(record.devices.iter().map(|binding| binding.id));
                        self.holding
                            .reserve(OwnerRef::Box(id), &devices, Some(marker), None)?;
                        self.holding.uncertain(OwnerRef::Box(id))?;
                        return self.holding.remove(id);
                    }
                }
                JobBox::end_named(&name)?;
                oci::end(&self.marker(id)?)?;
                self.drop_from_keeper(id)
            }
            None => Ok(()),
        }
    }

    /// The installer's features that are off, read once ([`features::off`]), at the runtime's making; a
    /// failed read is refused and tried again at the next ask.
    fn features_off(&self) -> Result<&BTreeSet<WindowsFeature>, Refusal> {
        if let Some(off) = self.features_off.get() {
            return Ok(off);
        }
        #[cfg(test)]
        self.feature_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let off = features::off()?;
        Ok(self.features_off.get_or_init(|| off))
    }

    /// The `oci` class refused for an image that needs `feature`: naming it while it is off, else saying
    /// what the image still waits on (`waits`).
    fn refused(&self, feature: WindowsFeature, waits: &str) -> Result<Refusal, Refusal> {
        let off: BTreeSet<WindowsFeature> = self
            .features_off()?
            .iter()
            .copied()
            .filter(|f| *f == feature)
            .collect();
        Ok(setup::windows_oci(&off, waits))
    }

    /// Where a node whose Linux images are its Shards guest's runs a box ([`crate::guest::WithGuest::routing`]):
    /// a `process` box and a Windows image here, a Linux image in the guest once the Windows Hypervisor
    /// Platform it needs is on, refused naming it before then. A bundle is a Windows image when its
    /// config has a `windows` section (config.md:530 makes it a Windows container's MUST); a spec, when its
    /// root is layers, as step 4's class reads one (a plain folder is a Linux root).
    pub fn oci_side(&self, class: &ClassSpec) -> Result<Side, Refusal> {
        let windows = match class {
            ClassSpec::Process => true,
            ClassSpec::Bundle(dir) => oci::bundle_config(dir)?.get("windows").is_some(),
            ClassSpec::Oci(o) => matches!(o.rootfs, Rootfs::Layers(_)),
            ClassSpec::Shards(_) | ClassSpec::ShardsImage(_) => false,
            ClassSpec::Firecracker(_) => {
                return Err(Refusal::Platform {
                    what: "Firecracker runtime",
                    platform: "windows native",
                    why: "Firecracker requires an initialized Linux KVM endpoint".to_owned(),
                });
            }
        };
        if windows {
            return Ok(Side::Host);
        }
        self.refused_without(WindowsFeature::HypervisorPlatform)?;
        Ok(Side::Guest)
    }

    /// `Ok` while `feature` is on, else the class refused naming it.
    fn refused_without(&self, feature: WindowsFeature) -> Result<(), Refusal> {
        if self.features_off()?.contains(&feature) {
            return Err(self.refused(feature, "")?);
        }
        Ok(())
    }

    /// What this runtime's start-time reclaim of a killed node's scratch couldn't do.
    pub fn reclaim_failures(&self) -> &[Refusal] {
        &self.reclaim_failures
    }

    /// This runtime with `oci` boxes' launcher taken from `dir` (which holds `hex.exe`), not the node's
    /// own folder: for a node run from another binary, as a test is.
    pub fn with_launcher(self, dir: PathBuf) -> Self {
        Self {
            launcher: Some(dir),
            ..self
        }
    }

    /// A runtime told the machine has `processors` logical processors (at least one).
    pub fn with_processors(processors: u32) -> Self {
        Self::with(processors, default_scratch_root())
    }

    fn boxes(&self) -> Result<std::sync::MutexGuard<'_, Vec<(OwnerKey, Slot)>>, Refusal> {
        self.holding.boxes()
    }

    fn drop_from_keeper(&self, id: &BoxId) -> Result<(), Refusal> {
        self.holding.drop_from_keeper(id)
    }

    fn get(&self, id: &BoxId) -> Result<Held, Refusal> {
        let boxes = self.boxes()?;
        let at = slot_index(&boxes, id).map_err(|_| Refusal::NoSuchBox(id.clone()))?;
        let (_, slot) = boxes.get(at).ok_or_else(inconsistent_slots)?;
        match &slot.state {
            SlotState::Ready(Held::Process(boxed)) if boxed.is_vm() => {
                Err(Refusal::NoSuchBox(id.try_clone()?))
            }
            SlotState::Ready(held) => Ok(held.clone()),
            _ => Err(pending("box is not ready; its resources remain held")),
        }
    }

    fn info(&self, b: &Held) -> Result<BoxInfo, Refusal> {
        let mut info = match b {
            Held::Process(b) => Ok(BoxInfo {
                published: Vec::new(),
                grants: Vec::new(),
                route: None,
                id: b.id()?.try_clone()?,
                class: Class::Process,
                state: if b.running()? {
                    BoxState::Running
                } else {
                    BoxState::Empty
                },
                bounds: b.bounds(),
                adopted: false,
                membership: None,
                // A process box has no init, so no container exit to report (HEX-RT-11).
                exit: None,
                abandoned: 0,
                allocation: None,
                last_resort: None,
                ended: None,
                credentials: Vec::new(),
            }),
            Held::Oci(b) => oci::info(b),
        }?;
        let boxes = self.boxes()?;
        let at = slot_index(&boxes, &info.id).map_err(|_| inconsistent_slots())?;
        let (_, slot) = boxes.get(at).ok_or_else(inconsistent_slots)?;
        let same = match (b, slot.physical()) {
            (Held::Process(expected), Some(Held::Process(actual))) => {
                NativeArc::ptr_eq(expected, actual)
            }
            (Held::Oci(expected), Some(Held::Oci(actual))) => Arc::ptr_eq(expected, actual),
            _ => false,
        };
        if !same || info.route.is_some_and(|route| Some(route) != slot.route) {
            return Err(inconsistent_slots());
        }
        info.route = slot.route;
        Ok(info)
    }

    /// The container `oci` box `spec` asks for, refused by name where a feature it needs is off.
    fn container(&self, spec: &BoxSpec) -> Result<hex_core::hcs::Container, Refusal> {
        let mut container = match &spec.class {
            ClassSpec::Oci(o) => {
                self.refused_without(WindowsFeature::Containers)?;
                hex_core::hcs::from_spec(o, &oci::scratch_folder(&self.scratch_root, &spec.id)?)?
            }
            ClassSpec::Bundle(dir) => {
                let config = oci::bundle_config(dir)?;
                if config.get("windows").is_none() {
                    return Err(self.refused(WindowsFeature::HypervisorPlatform, LINUX_IN_GUEST)?);
                }
                self.refused_without(WindowsFeature::Containers)?;
                hex_core::hcs::from_bundle(&config)?
            }
            ClassSpec::Process => {
                return Err(Refusal::Broken(
                    "a process spec reached the oci class".to_owned(),
                ));
            }
            ClassSpec::Firecracker(_) | ClassSpec::Shards(_) | ClassSpec::ShardsImage(_) => {
                return Err(wrong_backend(spec));
            }
        };
        let bindings = self.bindings(spec)?;
        container
            .devices
            .try_reserve_exact(bindings.len())
            .map_err(|_| allocation(bindings.len()))?;
        for binding in bindings {
            container
                .devices
                .push(hex_core::hcs::AssignedDevice::LocationPath(
                    binding.location,
                ));
        }
        Ok(container)
    }

    /// The folder holding the `hex` every `oci` box maps read-only as its launcher.
    fn launcher(&self) -> Result<PathBuf, Refusal> {
        match &self.launcher {
            Some(d) => Ok(d.clone()),
            None => std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(std::path::Path::to_path_buf))
                .ok_or_else(|| Refusal::Broken("the node can't name its own folder".to_owned())),
        }
    }

    /// Where `oci` box `id`'s record is kept: its scratch marker under the runtime's scratch root.
    fn marker(&self, id: &BoxId) -> Result<PathBuf, Refusal> {
        let suffix = ".scratch.json";
        let mut name = copy_text(id.as_str())?;
        name.try_reserve_exact(suffix.len())
            .map_err(|_| allocation(suffix.len()))?;
        name.push_str(suffix);
        let extra = name
            .len()
            .checked_add(1)
            .ok_or_else(|| allocation(usize::MAX))?;
        let mut path = copy_path(&self.scratch_root)?;
        path.try_reserve_exact(extra)
            .map_err(|_| allocation(extra))?;
        path.push(name);
        Ok(path)
    }

    /// The `oci` box `spec` asks for, made. Where this node's boxes outlive it, its job is the one named for
    /// the node and the box (the keeper holds it too: [`Runtime::create`]) and its container outlives the
    /// node with it; otherwise both end with the node.
    fn create_oci(
        &self,
        spec: &BoxSpec,
        container: &hex_core::hcs::Container,
    ) -> Result<OciBox, Refusal> {
        let launcher = self.launcher()?;
        let bindings = self.bindings(spec)?;
        let physical_id = spec.id.try_clone()?;
        let (job, lifetime) = match self.job_name(&spec.id) {
            Some(name) => (
                JobBox::create_named(physical_id, &spec.bounds, self.processors, &name)?,
                hex_core::hcs::Lifetime::Outlives,
            ),
            None => (
                JobBox::create(physical_id, &spec.bounds, self.processors)?,
                hex_core::hcs::Lifetime::EndsWithNode,
            ),
        };
        if let Some(keeping) = self.holding.keeper.as_ref() {
            self.holding.ensure_keeper(keeping)?;
        }
        OciBox::create_assigned_at(
            job,
            container,
            &launcher,
            Some(self.marker(&spec.id)?),
            lifetime,
            bindings,
            self.holding
                .keeper
                .as_ref()
                .map(|keeping| keeping.state.as_path()),
        )
    }

    fn finish_make(&self, id: &BoxId, made: Result<Held, Refusal>) -> Result<BoxInfo, Refusal> {
        let held = match made {
            Ok(held) => held,
            Err(error) => {
                self.holding.uncertain_after(OwnerRef::Box(id), &error)?;
                return match self.holding.remove(id) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(Refusal::Broken(format!(
                        "native creation failed ({error}); cleanup remains uncertain ({cleanup})"
                    ))),
                };
            }
        };
        if self.holding.made(OwnerRef::Box(id), held.clone())? {
            self.holding.remove(id)?;
            return Err(pending(
                "creation was cancelled and its physical box was removed",
            ));
        }
        let result = self
            .info(&held)
            .and_then(|info| self.holding.keep(OwnerRef::Box(id), &held).map(|()| info));
        match result {
            Ok(info) if self.holding.ready(OwnerRef::Box(id))? => Ok(info),
            Ok(_) => {
                self.holding.remove(id)?;
                Err(pending(
                    "creation was cancelled and its physical box was removed",
                ))
            }
            Err(error) => {
                self.holding.uncertain_after(OwnerRef::Box(id), &error)?;
                match self.holding.remove(id) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(Refusal::Broken(format!(
                        "native publication failed ({error}); cleanup remains uncertain ({cleanup})"
                    ))),
                }
            }
        }
    }

    /// Takes back `oci` box `spec` a node before this one made on the same name: its record from its
    /// marker, its job by name, then its container, scratch and init ([`OciBox::take_back`]).
    fn take_back_oci(&self, spec: &BoxSpec, name: &str) -> Result<OciBox, Refusal> {
        let container = self.container(spec)?;
        let marker = self.marker(&spec.id)?;
        let record = oci::read_marker(&marker)?;
        self.validate_bindings(spec, &record)?;
        let job = JobBox::take_back(spec.id.clone(), &spec.bounds, self.processors, name)?;
        OciBox::take_back_assigned(job, &container, marker, record, self.bindings(spec)?)
    }
}

impl Default for WinRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn native_platform() -> hex_api::ImagePlatform {
    #[cfg(target_arch = "x86_64")]
    let arch = hex_api::ImageArch::Amd64;
    #[cfg(target_arch = "aarch64")]
    let arch = hex_api::ImageArch::Arm64;
    hex_api::ImagePlatform {
        os: hex_api::ImageOs::Windows,
        arch,
    }
}

fn native_encoding_upper_bound(
    loaded: Option<bool>,
    features_off: Option<&BTreeSet<WindowsFeature>>,
    projection: &mut crate::devices::RouteProjection,
) -> Result<(), Refusal> {
    let loaded = loaded.ok_or_else(|| pending("native HCS API has not been initialized"))?;
    let off = features_off
        .ok_or_else(|| pending("native Containers configuration has not been initialized"))?;
    let form = if loaded && !off.contains(&WindowsFeature::Containers) {
        Class::Oci
    } else {
        Class::Process
    };
    let (process, oci) = crate::devices::native_route_keys(form, native_platform())?;
    projection.retain(process, None)?;
    if let Some(oci) = oci {
        projection.retain(oci, None)?;
    }
    Ok(())
}

/// A native form's routes, as Linux offers them (`devices::native_routes`): the `process` route, and in
/// an initialized `oci` form the `oci` route beside it, the keys its encoding bound reserves
/// (`native_encoding_upper_bound`), so a node that runs containers still takes `process` boxes, and so
/// does one whose `oci` form isn't initialized (its service unavailable, or Containers off), offering its
/// `process` route alone (joint plan step 13). Only Holding::report's Hcs-route entries enter the `oci`
/// snapshot, each eligible for the `oci` route alone; the `process` snapshot supplies no devices.
fn native_route_offer(
    report: &mut Capacity,
    class: Class,
    initialized: bool,
) -> Result<(), Refusal> {
    let form = if initialized { class } else { Class::Process };
    crate::devices::native_routes(report, form, native_platform())
}

impl Runtime for WinRuntime {
    fn capacity(&self, class: Class) -> Result<Option<Capacity>, Refusal> {
        if !matches!(class, Class::Process | Class::Oci) {
            return Ok(None);
        }
        // The native capability is initialized independently of whether a GPU was configured.
        // The guest augments this same physical envelope with its separately initialized routes.
        let initialized = class == Class::Process
            || (oci::available().is_ok()
                && !self.features_off()?.contains(&WindowsFeature::Containers));
        let (memory, free_memory) =
            sys::memory().map_err(|e| Refusal::os("reading native memory capacity", e))?;
        let (disk, free_disk) = scratch_volume(&self.scratch_root)
            .map_err(|e| Refusal::os("reading native disk capacity", e))?;
        let cpu = u64::from(self.processors.get())
            .checked_mul(1000)
            .ok_or_else(|| allocation(usize::MAX))?;
        let identity = self
            .node
            .as_deref()
            .or_else(|| self.scratch_root.to_str())
            .ok_or_else(|| pending("native pool identity is not Unicode"))?;
        let mut id = copy_text("windows:")?;
        id.try_reserve_exact(identity.len())
            .map_err(|_| allocation(identity.len()))?;
        id.push_str(identity);
        let mut report = Capacity {
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            cost: None,
            id,
            total: CapacityResources {
                cpu_milli: cpu,
                mem_bytes: memory,
                disk_bytes: disk,
            },
            free: CapacityResources {
                cpu_milli: cpu,
                mem_bytes: free_memory,
                disk_bytes: free_disk,
            },
            devices: if class == Class::Oci {
                self.holding.report(&self.catalog)?
            } else {
                Vec::new()
            },
            unavailable: Vec::new(),
            learned: None,
        };
        native_route_offer(&mut report, class, initialized)?;
        report.check()?;
        Ok(Some(report))
    }

    fn classes(&self) -> Vec<Class> {
        if oci::available().is_ok() {
            vec![Class::Process, Class::Oci]
        } else {
            vec![Class::Process]
        }
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.check_native_spec(&spec)?;
        // This ordinary native route advertises legacy identity cost (None). Managed VM launches
        // enter the canonical Books transaction and configure their explicit allocation instead.
        if spec.allocation.is_some() {
            return Err(Refusal::Invalid {
                what: "native runtime allocation",
                why: copy_text("this ordinary native route advertises identity cost")?,
            });
        }
        if matches!(spec.class, ClassSpec::Process) {
            spec.check_no_devices("windows process")?;
        }
        // Refuse invalid/missing/native-incompatible bindings before taking a slot or creating jobs.
        self.bindings(&spec)?;
        let container = if matches!(spec.class, ClassSpec::Process) {
            None
        } else {
            Some(self.container(&spec)?)
        };
        let marker = if matches!(spec.class, ClassSpec::Process) {
            None
        } else {
            Some(self.marker(&spec.id)?)
        };
        if marker.as_ref().is_some_and(|path| path.exists()) {
            return Err(Refusal::Exists(spec.id));
        }
        let physical_id = spec.id.try_clone()?;
        self.holding
            .reserve(OwnerRef::Box(&spec.id), &spec.devices, marker, spec.route)?;
        let held = match &spec.class {
            ClassSpec::Process => match self.job_name(&spec.id) {
                Some(name) => {
                    JobBox::create_named(physical_id, &spec.bounds, self.processors, &name)
                }
                None => JobBox::create(physical_id, &spec.bounds, self.processors),
            }
            .and_then(native_held),
            _ => match container.as_ref() {
                Some(container) => self
                    .create_oci(&spec, container)
                    .map(|boxed| Held::Oci(Arc::new(boxed))),
                None => Err(inconsistent_slots()),
            },
        };
        if held.is_err() && matches!(spec.class, ClassSpec::Process) {
            self.holding.unmade(&spec.id)?;
            return held.and_then(|boxed| self.info(&boxed));
        }
        if let Err(error) = held.as_ref()
            && !spec.devices.is_empty()
        {
            self.holding.preparation_failed(&spec.id)?;
            return match self.holding.remove(&spec.id) {
                Ok(()) => held.and_then(|boxed| self.info(&boxed)),
                Err(cleanup) => Err(Refusal::Broken(format!(
                    "native preparation failed ({error}); owned scratch cleanup remains pending ({cleanup})"
                ))),
            };
        }
        self.finish_make(&spec.id, held)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        match self.get(id)? {
            // A process box has no init to start: exec runs its programs.
            Held::Process(_) => Err(Refusal::Invalid {
                what: "start",
                why: format!("box {id} is a process box, which has no init to start"),
            }),
            Held::Oci(b) => b.start(),
        }
    }

    /// HEX-RT-17: the streams the caller asks for are made as pipes ([`Stdio::make_asked`]), the program's
    /// ends handed to it as a caller's own handles are and the caller's given back once by the program's
    /// [`Proc::streams`], so the host copies no byte between them, in either class.
    fn exec(
        &self,
        id: &BoxId,
        program: Program,
        mut stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        let given = stdio.make_asked()?;
        match self.get(id)? {
            Held::Process(b) => {
                let spawn = build_spawn(&program, stdio)?;
                let Started {
                    process,
                    thread,
                    pid,
                } = b.exec(spawn)?;
                // The initial thread is resumed; the node holds only the process handle to wait on.
                drop(thread);
                let proc = proc::BoxProc::new(process, pid, b.shared());
                Ok(Box::new(hex_api::Asked::new(proc, given)))
            }
            Held::Oci(b) => {
                let process = hex_core::hcs::process("program", &program)?;
                let proc = b.exec(&process, std_handles(stdio)?)?;
                Ok(Box::new(hex_api::Asked::new(proc, given)))
            }
        }
    }

    /// A run's box goes once its program ends (decision 7): a thread of the run's own waits on the program's
    /// process handle and removes the box through the delete's own path ([`Holding::remove`], its job and the
    /// keeper's handle with it), whether or not its caller still holds the run, and only then gives the caller
    /// the end. Its kill is the box's KILL, the box's job holding that program alone.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_no_devices("windows")?;
        spec.check_run()?;
        let id = spec.id.clone();
        self.create(spec)?;
        let begun = self.exec(&id, program, stdio).and_then(|proc| {
            let b = match self.get(&id)? {
                Held::Process(b) => b,
                Held::Oci(_) => {
                    return Err(Refusal::Invalid {
                        what: "run",
                        why: format!("box {id} is an oci box; a run's box is the process class's"),
                    });
                }
            };
            let (holding, gone) = (Arc::clone(&self.holding), id.clone());
            crate::run::Run::start(
                proc,
                &id,
                Box::new(move || b.signal(Signal::KILL)),
                Box::new(move || holding.remove(&gone)),
            )
        });
        match begun {
            Ok(run) => Ok(Box::new(run)),
            Err(why) => Err(crate::run::undone(why, self.delete(&id), &id)),
        }
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        match self.get(id)? {
            Held::Process(b) => b.signal(signal),
            Held::Oci(b) => b.signal(signal),
        }
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let b = self.get(id)?;
        self.info(&b)
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        match self.get(id)? {
            Held::Process(b) => b.usage(),
            Held::Oci(b) => b.usage(),
        }
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        let held = {
            let boxes = self.boxes()?;
            let mut held = Vec::new();
            held.try_reserve_exact(boxes.len())
                .map_err(|_| allocation(boxes.len()))?;
            for (_, slot) in boxes.iter() {
                if let Some(Held::Process(boxed)) = slot.physical()
                    && boxed.is_vm()
                {
                    continue;
                }
                match &slot.state {
                    SlotState::Ready(boxed) => held.push(boxed.clone()),
                    _ => return Err(pending("native boxes have pending lifecycle work")),
                }
            }
            held
        };
        held.iter().map(|boxed| self.info(boxed)).collect()
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        {
            let boxes = self.boxes()?;
            if let Ok(at) = slot_index(&boxes, id)
                && let Some((_, slot)) = boxes.get(at)
                && let Some(Held::Process(boxed)) = slot.physical()
                && boxed.is_vm()
            {
                return Err(Refusal::NoSuchBox(id.try_clone()?));
            }
        }
        self.holding.remove(id)
    }
}

/// `%ProgramData%\hex\boxes`, where `oci` boxes made from an `OciSpec` keep their scratch.
fn default_scratch_root() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("hex")
        .join("boxes")
}

/// The size of the volume `dir` is on and the bytes this user may still write there ([`sys::disk_space`]),
/// read at `dir`, or while it isn't made yet at its nearest folder that is: the volume it will be made on,
/// as no folder Hex makes there is a mount point. An outliving node's scratch folder is made by its first
/// `oci` box, and a scheduler opening over a fresh node reads its capacity before any (joint plan step 13,
/// HEX-CV-L1). It only reads, and is bounded by `dir`'s components.
fn scratch_volume(dir: &Path) -> std::io::Result<(u64, u64)> {
    let mut at = dir;
    loop {
        match sys::disk_space(at) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => at = at.parent().ok_or(e)?,
            read => return read,
        }
    }
}

/// Every standard stream the NUL device: an `oci` box's init has no caller to read it.
fn null_stdio() -> Result<[OwnedHandle; 3], Refusal> {
    Ok([handle(None)?, handle(None)?, handle(None)?])
}

/// The three standard handles for `stdio`, each the NUL device where the caller gave none.
fn std_handles(stdio: Stdio) -> Result<[OwnedHandle; 3], Refusal> {
    Ok([
        handle(stdio.stdin.map(OwnedHandle::from))?,
        handle(stdio.stdout.map(OwnedHandle::from))?,
        handle(stdio.stderr.map(OwnedHandle::from))?,
    ])
}

/// Builds the [`Spawn`] for `program` with `stdio`: its command line (argv quoted as the C runtime
/// parses it), its whole environment as a sorted block (nothing inherited), its working folder, and its
/// three standard handles (each the NUL device where the caller gave none).
fn build_spawn(program: &Program, stdio: Stdio) -> Result<Spawn, Refusal> {
    program.check()?;
    // One Windows exec bound for both classes, so a program moves between them unchanged (HEX-RT-14).
    program.fits(&hex_core::hcs::exec_limit())?;
    if program.user.is_some() {
        return Err(Refusal::Invalid {
            what: "program",
            why: "a process box runs its programs as the node agent's user; a user is the oci class's"
                .to_owned(),
        });
    }
    let cwd = match &program.cwd {
        Some(c) if c.contains('\0') => {
            return Err(Refusal::Invalid {
                what: "program",
                why: format!("working directory {c:?} holds a NUL"),
            });
        }
        Some(c) => Some(c.encode_utf16().chain([0]).collect()),
        None => None,
    };
    Ok(Spawn {
        command_line: hex_core::job::command_line(&program.args)
            .encode_utf16()
            .chain([0])
            .collect(),
        environment: hex_core::job::environment(&program.env),
        cwd,
        stdio: std_handles(stdio)?,
    })
}

/// The handle for a standard stream: the caller's where it gave one, else the NUL device (read and
/// write, so it serves stdin or stdout).
fn handle(given: Option<OwnedHandle>) -> Result<OwnedHandle, Refusal> {
    match given {
        Some(h) => Ok(h),
        None => {
            let nul = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("NUL")
                .map_err(|e| Refusal::os("opening NUL", e))?;
            Ok(OwnedHandle::from(nul))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn device_runtime(scratch: &Path) -> WinRuntime {
        let catalog=Catalog::parse(b"hex-devices-1\ngpu\t256\tfixture-a\t4096\thcs\tPCIROOT(0)#PCI(0100)\t\ngpu\t512\tfixture-b\t8192\thcs\tPCIROOT(0)#PCI(0200)\t\ngpu\t768\tfixture-c\t4096\tcdi\t/private/fixture.json\tfixture.test/gpu=c\n").unwrap();
        WinRuntime {
            processors: NonZeroU32::MIN,
            catalog: NativeCatalog::Shared(SharedCatalog::try_new(catalog).unwrap()),
            scratch_root: scratch.to_path_buf(),
            launcher: None,
            holding: Arc::new(Holding {
                boxes: Mutex::new(Vec::new()),
                keeper: None,
            }),
            node: None,
            features_off: OnceLock::from(BTreeSet::new()),
            feature_reads: std::sync::atomic::AtomicU32::new(0),
            reclaim_failures: Vec::new(),
        }
    }

    /// AC-T7.1.1: initialized CPU-only HCS remains an actual native route without a GPU catalog.
    #[test]
    fn native_windows_route_does_not_require_a_configured_gpu() {
        let mut report = Capacity {
            id: "native-control".into(),
            total: CapacityResources::default(),
            free: CapacityResources::default(),
            devices: Vec::new(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        };
        native_route_offer(&mut report, Class::Oci, true).unwrap();
        let route = report
            .routes
            .iter()
            .find(|route| route.key.class == Class::Oci)
            .unwrap();
        assert_eq!(route.key.image.os, hex_api::ImageOs::Windows);
        assert_eq!(route.key.execution, hex_api::Execution::Native);
        assert!(report.devices.is_empty() && report.device_routes.is_empty());
        report.check().unwrap();
    }

    /// AC-T7.4.1 (joint plan step 13, HEX-CV-L1 on Windows): do make the `oci` form's offer of a node whose
    /// Containers are on, as `capacity(Oci)` makes it, with its HCS devices, beside the encoding bound such a
    /// node reserves; expect every key of the bound offered and no other, its `process` route beside its `oci`
    /// one as Linux's `devices::native_routes` offers them, and each device eligible for the `oci` route alone:
    /// the scheduler places a `process` box on a Windows node that runs containers.
    #[test]
    fn a_containers_nodes_oci_form_offers_its_process_route_beside_its_oci_one() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let mut report = Capacity {
            id: "containers-node".into(),
            total: CapacityResources::default(),
            free: CapacityResources::default(),
            devices: rt.holding.report(&rt.catalog).unwrap(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        };
        native_route_offer(&mut report, Class::Oci, true).unwrap();
        report.check().unwrap();
        let (process, oci) =
            crate::devices::native_route_keys(Class::Oci, native_platform()).unwrap();
        let oci = oci.unwrap();
        let offered: Vec<_> = report.routes.iter().map(|route| route.key).collect();
        let mut bound = crate::devices::RouteProjection::new();
        native_encoding_upper_bound(Some(true), Some(&BTreeSet::new()), &mut bound).unwrap();
        let reserved = bound.count().unwrap();
        for key in &offered {
            bound.retain(*key, None).unwrap();
        }
        assert_eq!(
            bound.count().unwrap(),
            reserved,
            "{offered:?} offers a key past its bound"
        );
        assert_eq!(
            offered,
            [process, oci],
            "the oci form offers {} of the {reserved} routes its bound reserves",
            offered.len()
        );
        let at = offered.iter().position(|key| *key == oci).unwrap();
        assert!(!report.devices.is_empty());
        assert!(
            report.device_routes.iter().all(|mask| *mask == 1 << at),
            "{:?}",
            report.device_routes
        );
    }

    /// AC-T7.4.1: reserve possible fixed encoding forms without promoting a currently unavailable
    /// service into fresh OCI eligibility. Initialization unknowns refuse before collector mutation.
    /// An `oci` form not initialized (its service unavailable, or Containers off) still offers its
    /// `process` route, which needs neither, so the node still takes `process` boxes (joint plan step 13).
    #[test]
    fn encoding_upper_bound_does_not_advertise_an_unavailable_oci_service() {
        let enabled = BTreeSet::new();
        for (api, features) in [(None, Some(&enabled)), (Some(true), None)] {
            let mut projection = crate::devices::RouteProjection::new();
            assert!(native_encoding_upper_bound(api, features, &mut projection).is_err());
            assert_eq!(projection.count().unwrap(), 0);
        }
        let mut projection = crate::devices::RouteProjection::new();
        native_encoding_upper_bound(Some(true), Some(&enabled), &mut projection).unwrap();
        assert_eq!(projection.count().unwrap(), 2); // Possible Process + OCI, not current service facts.
        let mut unavailable = Capacity {
            id: "service-unavailable-control".into(),
            total: CapacityResources::default(),
            free: CapacityResources::default(),
            devices: Vec::new(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        };
        native_route_offer(&mut unavailable, Class::Oci, false).unwrap();
        let (process, _) =
            crate::devices::native_route_keys(Class::Process, native_platform()).unwrap();
        let offered: Vec<_> = unavailable.routes.iter().map(|route| route.key).collect();
        assert_eq!(
            offered,
            [process],
            "encoding room cannot grant fresh native OCI, and the process route needs no service"
        );
        assert!(unavailable.device_routes.is_empty() && unavailable.backings.is_empty());
        let disabled = BTreeSet::from([WindowsFeature::Containers]);
        for (api, features) in [(false, &enabled), (true, &disabled)] {
            let mut projection = crate::devices::RouteProjection::new();
            native_encoding_upper_bound(Some(api), Some(features), &mut projection).unwrap();
            assert_eq!(projection.count().unwrap(), 1); // Only the actual native Process form can exist.
        }
    }

    /// AC-T7.4.1: one bounded native table, with real Box and Pool keys and no fabricated BoxId.
    #[test]
    fn native_owner_table_keeps_pool_and_box_names_distinct() {
        let holding = Holding {
            boxes: Mutex::new(Vec::new()),
            keeper: None,
        };
        let id = BoxId::new("pool-identity").unwrap();
        let pool = [7; 32];
        holding
            .reserve(OwnerRef::Pool(&pool), &[], None, None)
            .unwrap();
        holding
            .reserve(OwnerRef::Box(&id), &[], None, None)
            .unwrap();
        assert!(
            holding
                .reserve(OwnerRef::Pool(&pool), &[], None, None)
                .is_err()
        );
        let boxes = holding.boxes().unwrap();
        assert_eq!(boxes.len(), 2);
        assert_ne!(
            owner_index(&boxes, OwnerRef::Pool(&pool)),
            slot_index(&boxes, &id)
        );
        assert!(boxes.iter().all(|(_, slot)| slot.devices.is_empty()));
    }

    fn device_spec(name: &str, key: u64) -> BoxSpec {
        BoxSpec {
            route: None,
            id: BoxId::new(name).unwrap(),
            bounds: hex_api::Bounds::default(),
            owner: None,
            allocation: None,
            class: ClassSpec::Oci(Box::new(hex_api::OciSpec {
                rootfs: Rootfs::Layers(vec![PathBuf::from(r"C:\fixture\base")]),
                init: Program::of(&["cmd.exe", "/c", "exit", "0"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            devices: vec![DeviceId {
                kind: hex_api::DeviceKind::Gpu,
                key,
            }],
            band: 0,
        }
    }

    /// AC-T7.4.1: exercise the exact container builder used by native create, without HCS or WMI.
    #[test]
    fn device_selected_binding_reaches_the_existing_hcs_document_with_schema22() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = device_spec("selected", 512);
        let container = rt.container(&spec).unwrap();
        assert_eq!(
            container.devices,
            vec![hex_core::hcs::AssignedDevice::LocationPath(
                "PCIROOT(0)#PCI(0200)".to_owned()
            )]
        );
        let storage = hex_core::hcs::Storage {
            volume: "fixture-volume".to_owned(),
            layers: Vec::new(),
        };
        let document = hex_core::hcs::document(
            &container,
            &storage,
            r"C:\fixture\launcher",
            hex_core::hcs::Lifetime::Outlives,
        );
        assert_eq!(
            document["SchemaVersion"],
            serde_json::json!({"Major":2,"Minor":2})
        );
        assert_eq!(
            document["Container"]["AssignedDevices"],
            serde_json::json!([{"Type":"DeviceInstance","LocationPath":"PCIROOT(0)#PCI(0200)"}])
        );
        assert!(!document.to_string().contains("PCI(0100)"));
        let mut legacy = spec;
        legacy.devices.clear();
        let document = hex_core::hcs::document(
            &rt.container(&legacy).unwrap(),
            &storage,
            r"C:\fixture\launcher",
            hex_core::hcs::Lifetime::Outlives,
        );
        assert_eq!(
            document["SchemaVersion"],
            serde_json::json!({"Major":2,"Minor":1})
        );
        assert!(document["Container"].get("AssignedDevices").is_none());
        assert_eq!(rt.feature_reads.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1: the real native bundle builder accepts an already-mounted root with exact assignment.
    #[test]
    fn device_borrowed_volume_bundle_keeps_precise_assignment_and_same_native_marker() {
        let scratch = tempfile::tempdir().unwrap();
        let bundle = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let volume = r"\\?\Volume{ec84d99e-3f02-11e7-ac6c-00155d7682cf}\";
        let config = serde_json::json!({
            "ociVersion":"1.2.0", "root":{"path":volume},
            "process":{"args":["cmd.exe","/c","exit","0"],"cwd":"C:\\"},
            "windows":{"layerFolders":["C:\\fixture\\base","C:\\fixture\\caller-scratch"]}
        });
        std::fs::write(
            bundle.path().join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let mut spec = device_spec("borrowed-bundle", 512);
        spec.class = ClassSpec::Bundle(bundle.path().to_path_buf());
        let container = rt.container(&spec).unwrap();
        assert_eq!(
            container.scratch,
            hex_core::hcs::Scratch::Volume(volume.to_owned())
        );
        assert_eq!(
            container.devices,
            vec![hex_core::hcs::AssignedDevice::LocationPath(
                "PCIROOT(0)#PCI(0200)".to_owned()
            )]
        );
        let marker = rt.marker(&spec.id).unwrap();
        assert_eq!(marker, scratch.path().join("borrowed-bundle.scratch.json"));
        let record = hex_core::hcs::BoxRecord {
            folder: volume.to_owned(),
            made: Vec::new(),
            outlives: true,
            system: Some(spec.id.clone()),
            started: false,
            init: None,
            devices: rt.bindings(&spec).unwrap(),
            storage: hex_core::hcs::RecordStorage::BorrowedVolume,
            creation: hex_core::hcs::NativeCreate::Created,
        };
        assert!(rt.validate_bindings(&spec, &record).is_ok());
        let mut wrong = record;
        wrong.storage = hex_core::hcs::RecordStorage::OwnedFolder;
        assert!(matches!(
            rt.validate_bindings(&spec, &wrong),
            Err(Refusal::Invalid {
                what: "native storage recovery",
                ..
            })
        ));
        assert_eq!(rt.feature_reads.load(Ordering::SeqCst), 0);
        assert!(!marker.exists()); // Container construction does not create a native system/marker.
    }

    /// AC-T7.4.1: unsupported and missing bindings refuse before any native job or pending slot.
    #[test]
    fn device_missing_cdi_and_process_requests_refuse_before_native_creation() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        assert!(matches!(
            rt.create(device_spec("unknown", 1024)),
            Err(Refusal::Invalid {
                what: "device attachment",
                ..
            })
        ));
        assert!(matches!(
            rt.create(device_spec("cdi", 768)),
            Err(Refusal::Platform {
                what: "device attachment",
                ..
            })
        ));
        let mut process = device_spec("process", 256);
        process.class = ClassSpec::Process;
        assert!(matches!(
            rt.create(process),
            Err(Refusal::Platform {
                what: "devices",
                platform: "windows process",
                ..
            })
        ));
        assert!(rt.boxes().unwrap().is_empty());
        assert_eq!(rt.feature_reads.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1: reservations/cancel/unknown cleanup retain the same ID; a sibling remains independent.
    #[test]
    fn device_pending_cancel_and_unknown_cleanup_never_publish_free_capacity() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let first = device_spec("first", 256);
        let sibling = device_spec("sibling", 512);
        rt.holding
            .reserve(OwnerRef::Box(&first.id), &first.devices, None, None)
            .unwrap();
        assert!(matches!(
            rt.holding
                .reserve(OwnerRef::Box(&first.id), &first.devices, None, None),
            Err(Refusal::Exists(_))
        ));
        assert!(
            rt.holding
                .reserve(
                    OwnerRef::Box(&BoxId::new("overlap").unwrap()),
                    &first.devices,
                    None,
                    None
                )
                .is_err()
        );
        rt.holding
            .reserve(OwnerRef::Box(&sibling.id), &sibling.devices, None, None)
            .unwrap();
        assert!(matches!(
            rt.delete(&first.id),
            Err(Refusal::Invalid {
                what: "native box lifecycle",
                ..
            })
        ));
        assert!(!matches!(rt.inspect(&first.id), Err(Refusal::NoSuchBox(_))));
        rt.holding.uncertain(OwnerRef::Box(&first.id)).unwrap();
        assert!(rt.delete(&first.id).is_err()); // No marker/absence witness: cannot release it.
        let report = rt.holding.report(&rt.catalog).unwrap();
        assert_eq!(
            report[0].holder,
            Some(hex_api::DeviceHolder::Box(first.id.clone()))
        );
        assert_eq!(
            report[1].holder,
            Some(hex_api::DeviceHolder::Box(sibling.id))
        );
        assert_eq!(rt.boxes().unwrap().len(), 2);
        assert!(
            rt.holding
                .reserve(
                    OwnerRef::Box(&BoxId::new("still-overlap").unwrap()),
                    &first.devices,
                    None,
                    None
                )
                .is_err()
        );
    }

    /// T-T7.1.2: changed exact mapping is refused before native adoption, with durable bytes intact.
    #[test]
    fn device_recovery_refuses_changed_or_missing_mapping_without_rewriting_the_marker() {
        let scratch = tempfile::tempdir().unwrap();
        let mut rt = device_runtime(scratch.path());
        rt.node = Some("owned-fixture".to_owned());
        let spec = device_spec("recorded", 256);
        let mut record = hex_core::hcs::BoxRecord {
            storage: hex_core::hcs::RecordStorage::OwnedFolder,
            creation: hex_core::hcs::NativeCreate::Created,
            folder: scratch.path().to_str().unwrap().to_owned(),
            made: Vec::new(),
            outlives: true,
            system: Some(spec.id.clone()),
            started: false,
            init: None,
            devices: rt.bindings(&spec).unwrap(),
        };
        assert_eq!(rt.validate_bindings(&spec, &record), Ok(())); // Matching positive data control.
        record.devices[0].location = "synthetic-private-old-location".to_owned();
        let bytes = hex_core::hcs::write_record(&record).unwrap();
        let marker = rt.marker(&spec.id).unwrap();
        std::fs::write(&marker, &bytes).unwrap();
        let error = rt.adopt(&spec).unwrap_err();
        assert!(matches!(
            error,
            Refusal::Invalid {
                what: "native device recovery",
                ..
            }
        ));
        assert!(!error.to_string().contains("synthetic-private"));
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        assert_eq!(
            rt.holding.report(&rt.catalog).unwrap()[0].holder,
            Some(hex_api::DeviceHolder::Box(spec.id.clone()))
        );
        assert!(!matches!(rt.inspect(&spec.id), Err(Refusal::NoSuchBox(_))));
        let mut missing = device_spec("missing", 1024);
        missing.id = spec.id.clone();
        assert!(rt.validate_bindings(&missing, &record).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        let other_scratch = tempfile::tempdir().unwrap();
        let mut missing_metadata = device_runtime(other_scratch.path());
        missing_metadata.node = Some("owned-missing-metadata".to_owned());
        let mut old_record = record;
        old_record.devices.clear();
        let old_bytes = hex_core::hcs::write_record(&old_record).unwrap();
        let old_marker = missing_metadata.marker(&spec.id).unwrap();
        std::fs::write(&old_marker, &old_bytes).unwrap();
        assert!(missing_metadata.adopt(&spec).is_err());
        assert_eq!(
            missing_metadata
                .holding
                .report(&missing_metadata.catalog)
                .unwrap()[0]
                .holder,
            Some(hex_api::DeviceHolder::Box(spec.id))
        );
        assert_eq!(std::fs::read(old_marker).unwrap(), old_bytes);
        assert_eq!(rt.feature_reads.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1: keeper-close uncertainty is retained as ownership, without repeating a handle close.
    #[test]
    fn device_keeper_uncertainty_and_removal_state_still_report_the_holder() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = device_spec("keeper-uncertain", 256);
        rt.holding
            .reserve(OwnerRef::Box(&spec.id), &spec.devices, None, spec.route)
            .unwrap();
        rt.holding.uncertain(OwnerRef::Box(&spec.id)).unwrap();
        rt.holding
            .keeper_receipt(OwnerRef::Box(&spec.id), KeeperHold::Uncertain)
            .unwrap();
        assert_eq!(
            rt.holding.report(&rt.catalog).unwrap()[0].holder,
            Some(hex_api::DeviceHolder::Box(spec.id.clone()))
        );
        assert!(!matches!(rt.inspect(&spec.id), Err(Refusal::NoSuchBox(_))));
        assert!(
            rt.holding
                .reserve(
                    OwnerRef::Box(&BoxId::new("contender").unwrap()),
                    &spec.devices,
                    None,
                    None
                )
                .is_err()
        );
    }

    #[test]
    fn device_missing_native_metadata_cannot_redirect_retained_claim_cleanup() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = device_spec("retained-declared", 256);
        let marker = rt.marker(&spec.id).unwrap();
        let sibling = scratch.path().join("sibling-sentinel");
        std::fs::write(&sibling, b"unaffected").unwrap();
        let record = hex_core::hcs::BoxRecord {
            folder: scratch.path().to_str().unwrap().to_owned(),
            made: Vec::new(),
            outlives: true,
            system: Some(BoxId::new("unrelated-system").unwrap()),
            started: false,
            init: None,
            devices: Vec::new(),
            storage: hex_core::hcs::RecordStorage::OwnedFolder,
            creation: hex_core::hcs::NativeCreate::Created,
        };
        let bytes = hex_core::hcs::write_record(&record).unwrap();
        std::fs::write(&marker, &bytes).unwrap();
        rt.holding
            .reserve(
                OwnerRef::Box(&spec.id),
                &spec.devices,
                Some(marker.clone()),
                spec.route,
            )
            .unwrap();
        rt.holding.uncertain(OwnerRef::Box(&spec.id)).unwrap();
        let calls = std::cell::Cell::new(0);
        let error = rt
            .holding
            .remove_using(&spec.id, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(
            error,
            Refusal::Invalid {
                what: "native box lifecycle",
                ..
            }
        ));
        assert_eq!(calls.get(), 0);
        assert_eq!(std::fs::read(marker).unwrap(), bytes);
        assert_eq!(std::fs::read(sibling).unwrap(), b"unaffected");
        assert_eq!(
            rt.holding.report(&rt.catalog).unwrap()[0].holder,
            Some(hex_api::DeviceHolder::Box(spec.id.clone()))
        );
        assert!(!matches!(rt.inspect(&spec.id), Err(Refusal::NoSuchBox(_))));
    }

    /// Ordinary Windows job smoke only: empty claims still create/delete through the same holding table.
    #[test]
    fn device_empty_claims_keep_ordinary_native_job_create_and_delete_working() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = process_box("owned-empty-device-smoke");
        let info = rt.create(spec.clone()).unwrap();
        assert_eq!(info.state, BoxState::Empty);
        assert_eq!(rt.list().unwrap().len(), 1);
        rt.delete(&spec.id).unwrap();
        assert!(rt.list().unwrap().is_empty());
        assert!(matches!(rt.inspect(&spec.id), Err(Refusal::NoSuchBox(_))));
    }

    /// T-T7.1.2: native handoff/durable writes run without the holding-map mutex; errors keep ownership.
    #[test]
    fn device_keeper_rehold_callbacks_release_map_mutex_and_keep_failed_write_receipt() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = process_box("owned-rehold-smoke");
        rt.create(spec.clone()).unwrap();
        let held_calls = std::cell::Cell::new(0);
        let write_calls = std::cell::Cell::new(0);
        let result = rt.holding.rehold_ready_using(
            11,
            22,
            |_| {
                assert!(rt.holding.boxes.try_lock().is_ok());
                assert_eq!(rt.list().unwrap().len(), 1);
                held_calls.set(held_calls.get() + 1);
                Ok(33) // Synthetic receipt, never a native remote handle.
            },
            |id, receipt| {
                assert!(rt.holding.boxes.try_lock().is_ok());
                assert_eq!(id, OwnerRef::Box(&spec.id));
                assert_eq!((receipt.pid, receipt.created, receipt.value), (11, 22, 33));
                write_calls.set(write_calls.get() + 1);
                Err(Refusal::os(
                    "recording a box's keeper handle",
                    "owned injected write refusal",
                ))
            },
        );
        assert!(matches!(
            result,
            Err(Refusal::Os {
                op: "recording a box's keeper handle",
                ..
            })
        ));
        assert_eq!((held_calls.get(), write_calls.get()), (1, 1));
        {
            let boxes = rt.boxes().unwrap();
            let at = slot_index(&boxes, &spec.id).unwrap();
            assert!(matches!(
                boxes[at].1.keeper,
                KeeperHold::Held(KeeperReceipt {
                    pid: 11,
                    created: 22,
                    value: 33
                })
            ));
        }
        rt.delete(&spec.id).unwrap(); // No keeper exists; no synthetic remote value is closed.
        assert!(rt.list().unwrap().is_empty());
    }

    /// Poisons the native holding table, as a panic elsewhere while holding it does.
    fn poison(holding: &Arc<Holding>) {
        let holding = Arc::clone(holding);
        assert!(
            std::thread::spawn(move || {
                let _guard = holding.boxes.lock().unwrap();
                panic!("owned injected native holding poison");
            })
            .join()
            .is_err()
        );
    }

    /// The poisoned table's slots, read without restoring the table to the runtime.
    fn poisoned(holding: &Holding) -> std::sync::MutexGuard<'_, Vec<(OwnerKey, Slot)>> {
        match holding.boxes.lock() {
            Err(poisoned) => poisoned.into_inner(),
            Ok(_) => panic!("the native holding table isn't poisoned"),
        }
    }

    /// AC-T7.4.1: do fail a create whose uncertainty mark the poisoned holding table refuses; expect
    /// the creation failure kept beside that refusal, typed Broken, and its slot held, never Ready.
    #[test]
    fn a_refused_uncertainty_mark_after_a_failed_create_keeps_the_failure() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let id = BoxId::new("unmarked-create").unwrap();
        rt.holding
            .reserve(OwnerRef::Box(&id), &[], None, None)
            .unwrap();
        poison(&rt.holding);
        let failure = Refusal::os("creating a box's job", "owned injected creation refusal");
        let error = rt.finish_make(&id, Err(failure.clone())).unwrap_err();
        let Refusal::Broken(why) = &error else {
            panic!("a refused uncertainty mark isn't typed as broken state: {error:?}");
        };
        assert!(
            why.contains(&failure.to_string()),
            "the creation failure was dropped: {why}"
        );
        assert!(why.contains("the node's box map lock is poisoned"), "{why}");
        let boxes = poisoned(&rt.holding);
        let at = slot_index(&boxes, &id).unwrap();
        assert!(matches!(boxes[at].1.state, SlotState::Creating));
    }

    /// AC-T7.4.1: do launch a managed VM whose controller exists but whose publication fails, with
    /// the holding table poisoned first so the failure path's uncertainty mark is refused; expect the
    /// launch failure kept beside that refusal, typed Broken, and the slot still holding the
    /// creator's job, never Ready.
    #[test]
    fn a_refused_uncertainty_mark_after_a_failed_managed_launch_keeps_the_failure() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let guest = CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 256 << 20,
            disk_bytes: 0,
        };
        let cost = hex_api::RuntimeCost {
            id: [1; 32],
            floor: guest,
            ceiling: guest,
            quantum: CapacityResources {
                cpu_milli: 1_000,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        // A dedicated Shards VM's granted row, as the canonical Store hands it to the launch.
        let pool = hex_core::record::PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: hex_api::BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            execution: hex_api::Execution::ShardsDedicated,
            cost,
            allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
            devices: Vec::new(),
            phase: hex_api::BackingPhase::Granted,
            init: None,
            exit: None,
        };
        let record = NativeRecord::Pool(&pool);
        let spec = shards_api::GuestSpec {
            kernel: "C:\\kernel".into(),
            init: shards_api::Init::Executable("C:\\init".into()),
            cmdline: "console=ttyS0 quiet".into(),
            vcpus: 1,
            memory_mib: 256,
            gpu: false,
            gpu_devices: Vec::new(),
            disks: Vec::new(),
            shares: Vec::new(),
            ports: vec![1024],
            binary: None,
        };
        // The controller is made suspended and never resumed, so this test's own executable serves.
        let plan = shards_api::prepare_boot(
            &spec,
            &std::env::current_exe().unwrap(),
            Path::new("C:\\v"),
            0,
        )
        .unwrap()
        .with_runtime_paths(
            &shards_api::RuntimePaths::new(
                scratch.path().join("cache"),
                scratch.path().join("home"),
            )
            .unwrap(),
        )
        .unwrap();
        let (_stdout, stdout) = std::io::pipe().unwrap();
        let (_stderr, stderr) = std::io::pipe().unwrap();
        let failure = Refusal::os(
            "publishing the VM controller",
            "owned injected publication refusal",
        );
        let published = std::cell::Cell::new(false);
        let mut publish = |_: &shards_api::Identity| -> Result<(), Refusal> {
            published.set(true);
            poison(&rt.holding);
            Err(failure.clone())
        };
        let Err(error) = rt.launch_managed(
            record,
            &crate::devices::ManagedLaunch::Shards(&plan),
            shards_api::LaunchIo {
                stdout,
                stderr,
                gpu_report: None,
            },
            // A finite launch bound this test never spends: publication refuses first.
            std::time::Duration::from_secs(3_600),
            &mut publish,
        ) else {
            panic!("a launch whose publication was refused succeeded");
        };
        assert!(
            published.get(),
            "the launch failed before publication: {error:?}"
        );
        let Refusal::Broken(why) = &error else {
            panic!("the refused uncertainty mark was dropped: {error:?}");
        };
        assert!(
            why.contains(&failure.to_string()),
            "the launch failure was dropped: {why}"
        );
        assert!(why.contains("the node's box map lock is poisoned"), "{why}");
        let boxes = poisoned(&rt.holding);
        let at = owner_index(&boxes, record.owner()).unwrap();
        assert!(matches!(
            boxes[at].1.state,
            SlotState::Finishing(Held::Process(_))
        ));
    }

    /// AC-T7.4.1: do cancel a create while it finishes, so its own readiness step hands the slot to
    /// cleanup, then run its failure path's uncertainty mark; expect the mark satisfied, not refused
    /// as broken state, and the slot keeping its job until a delete removes it.
    #[test]
    fn a_cancelled_finishing_slot_already_satisfies_its_failure_mark() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = process_box("cancelled-finishing");
        let id = &spec.id;
        rt.holding
            .reserve(OwnerRef::Box(id), &[], None, None)
            .unwrap();
        let job = JobBox::create(id.clone(), &spec.bounds, NonZeroU32::MIN).unwrap();
        assert!(
            !rt.holding
                .made(OwnerRef::Box(id), native_held(job).unwrap())
                .unwrap()
        );
        assert!(rt.holding.remove(id).is_err()); // The cancel: its keeper handoff is pending.
        assert!(!rt.holding.ready(OwnerRef::Box(id)).unwrap());
        let failure = Refusal::os("finishing a box", "owned injected cancelled creation");
        rt.holding
            .uncertain_after(OwnerRef::Box(id), &failure)
            .unwrap();
        {
            let boxes = rt.boxes().unwrap();
            let at = slot_index(&boxes, id).unwrap();
            assert!(matches!(
                boxes[at].1.state,
                SlotState::CleanupPending(Held::Process(_))
            ));
        }
        rt.delete(id).unwrap();
        assert!(rt.list().unwrap().is_empty());
    }

    /// HEX-INST-9 (native plan step 6): the installer's features are read when the runtime is made, so
    /// a box's create doesn't wait on PowerShell's query. Do: make a runtime, then route a Linux image's
    /// bundle, the first ask that needs the features. Expect: one query, made with the runtime, and
    /// none by the routing.
    #[test]
    fn the_features_are_read_at_the_runtimes_making_not_at_a_create() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = WinRuntime::with(1, scratch.path().to_path_buf());
        assert_eq!(rt.feature_reads.load(Ordering::SeqCst), 1);
        assert!(
            rt.features_off.get().is_some(),
            "the query at the runtime's making failed"
        );
        let bundle = tempfile::tempdir().unwrap();
        std::fs::write(
            bundle.path().join("config.json"),
            r#"{"ociVersion": "1.2.0", "process": {"args": ["/bin/true"], "cwd": "/"}, "root": {"path": "rootfs"}, "linux": {}}"#,
        )
        .unwrap();
        let _ = rt.oci_side(&ClassSpec::Bundle(bundle.path().to_path_buf()));
        assert_eq!(rt.feature_reads.load(Ordering::SeqCst), 1);
    }

    /// A program that runs until it is ended (PING's 600 one-second echoes to this machine).
    fn waiting() -> Program {
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let ping = format!("{system}\\System32\\PING.EXE");
        Program::of(&[ping.as_str(), "-n", "600", "127.0.0.1"])
    }

    /// Whether process `pid` runs, by `tasklist` (its output names the pid only while it runs).
    fn alive(pid: u32) -> bool {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }

    fn wait_for(what: &str, mut fact: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !fact() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::park_timeout(std::time::Duration::from_millis(20));
        }
    }

    fn process_box(id: &str) -> BoxSpec {
        BoxSpec {
            route: None,
            devices: Vec::new(),
            id: BoxId::new(id).unwrap(),
            bounds: hex_api::Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            allocation: None,
            band: 0,
        }
    }

    /// A node name no other test run holds in this session.
    fn node(test: &str) -> String {
        format!("t{}-{test}", std::process::id())
    }

    /// The keeper for the tests: this test binary re-invoked to run only [`keeper_body`], the real keeper
    /// (`keeper::run`). The runtime hands it the state folder and ready path through the environment, as it
    /// hands `hex keep` them, so the command is just how to launch it.
    fn keeper(_test: &str) -> Vec<String> {
        vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--exact".to_owned(),
            "windows::tests::keeper_body".to_owned(),
            "--ignored".to_owned(),
            "--nocapture".to_owned(),
            "--test-threads".to_owned(),
            "1".to_owned(),
        ]
    }

    /// The keeper, run as a process of its own by [`keeper`]: it reads its state folder and ready path from
    /// the environment and runs [`keeper::run`]. `#[ignore]`d so the harness runs it only when named.
    #[test]
    #[ignore]
    fn keeper_body() {
        let args = keeper::Args::parse(&[]).expect("keeper args from the environment");
        keeper::run(&args).expect("the keeper runs");
    }

    /// Ends process `pid` as a crash would (taskkill /F, TerminateProcess).
    fn crash(pid: u32) {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
            .unwrap();
    }

    /// T-T7.1.2 (native plan step 8): do make a box on a node's runtime, run a program in it, and drop the
    /// runtime (the node's every handle closed, as when it is killed); expect the program still running,
    /// held by the keeper, the next runtime on the node's name taking the box back as running, and its
    /// delete ending the program.
    #[test]
    fn a_nodes_box_outlives_its_runtime_and_the_next_takes_it_back() {
        let (name, state) = (node("back"), tempfile::tempdir().unwrap());
        let spec = process_box("kept");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("back")).unwrap();
        rt.create(spec.clone()).unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        drop(rt);
        assert!(alive(pid), "the box's program ended with its runtime");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("back")).unwrap();
        rt.adopt(&spec).unwrap();
        assert_eq!(rt.inspect(&spec.id).unwrap().state, BoxState::Running);
        rt.delete(&spec.id).unwrap();
        wait_for("the taken-back box's delete to end its program", || {
            !alive(pid)
        });
        if let Some(k) = rt.keeper_pid() {
            crash(k);
        }
    }

    /// T-T7.1.2: do leave a node's box running past its runtime, then end it by name from the next runtime
    /// (a box recovery removes); expect its program ended, and a second end, of nothing, accepted.
    #[test]
    fn a_box_no_node_takes_back_is_ended_by_name() {
        let (name, state) = (node("end"), tempfile::tempdir().unwrap());
        let spec = process_box("left");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("end")).unwrap();
        rt.create(spec.clone()).unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        drop(rt);
        assert!(alive(pid), "the box's program ended with its runtime");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("end")).unwrap();
        rt.end(&spec.id).unwrap();
        wait_for("the end to end the box's program", || !alive(pid));
        rt.end(&spec.id).unwrap();
        if let Some(k) = rt.keeper_pid() {
            crash(k);
        }
    }

    /// T-T7.1.2: do run a program in a node's box, let the node go, then end its keeper too; expect
    /// kill-on-close to end the program with the job's last handle, so nothing is left running in a job no
    /// one can open.
    #[test]
    fn a_box_whose_node_and_keeper_are_both_gone_is_ended() {
        let (name, state) = (node("both"), tempfile::tempdir().unwrap());
        let spec = process_box("orphan");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("both")).unwrap();
        rt.create(spec.clone()).unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        let kept = rt.keeper_pid().expect("a keeper holds the node's boxes");
        drop(rt);
        assert!(alive(pid), "the box's program ended with its runtime");
        crash(kept);
        wait_for("kill-on-close to end the program", || !alive(pid));
    }

    /// T-T7.1.2: do end a node's keeper while the node lives; expect the node to start another and give it
    /// the boxes' jobs, so a box still outlives the node afterwards and the next node takes it back.
    #[test]
    fn a_keeper_that_dies_while_its_node_lives_is_replaced() {
        let (name, state) = (node("again"), tempfile::tempdir().unwrap());
        let spec = process_box("rekept");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("again")).unwrap();
        rt.create(spec.clone()).unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        let first = rt.keeper_pid().expect("a keeper holds the node's boxes");
        crash(first);
        wait_for("a new keeper", || {
            rt.keeper_pid().is_some_and(|k| k != first && alive(k))
        });
        drop(rt);
        assert!(alive(pid), "the box's program ended with its runtime");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("again")).unwrap();
        rt.adopt(&spec).unwrap();
        assert_eq!(rt.inspect(&spec.id).unwrap().state, BoxState::Running);
        rt.delete(&spec.id).unwrap();
        wait_for("the delete to end the program", || !alive(pid));
        if let Some(k) = rt.keeper_pid() {
            crash(k);
        }
    }

    /// T-T7.1.2 (native plan step 8): do end a node's keeper, then make a box on the node's runtime before
    /// anything replaces the keeper; expect the create to finish, the box held by a new keeper, rather than
    /// wait forever on its own runtime's box-map lock (found by s4), and the box to outlive the runtime.
    #[test]
    fn a_box_made_after_its_keeper_ended_is_made_and_held_by_a_new_keeper() {
        let (name, state) = (node("late"), tempfile::tempdir().unwrap());
        let spec = process_box("late");
        let rt = Arc::new(WinRuntime::outliving(&name, state.path(), &keeper("late")).unwrap());
        let first = rt.keeper_pid().expect("a keeper holds the node's boxes");
        crash(first);
        wait_for("the keeper to end", || !alive(first));
        let (made, making) = std::sync::mpsc::channel();
        let (runtime, asked) = (Arc::clone(&rt), spec.clone());
        let maker = std::thread::spawn(move || {
            let _ = made.send(runtime.create(asked).map(drop));
        });
        making
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the create never finished: it waits on its own runtime's lock")
            .unwrap();
        maker.join().unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        let second = rt.keeper_pid().expect("a new keeper holds the box");
        assert_ne!(second, first);
        drop(rt);
        assert!(alive(pid), "the box's program ended with its runtime");
        crash(second);
        wait_for("kill-on-close to end the program", || !alive(pid));
    }

    /// T-T7.1.2: do make a node's runtime on a state folder; expect its `oci` boxes' records and scratch
    /// kept inside that folder, so its start-time reclaim sweeps only its own boxes, not the machine's shared
    /// folder where a runtime driven in process (a bench's) keeps the records of boxes it is running.
    #[test]
    fn an_outliving_nodes_records_are_in_its_own_folder() {
        let (name, state) = (node("own"), tempfile::tempdir().unwrap());
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("own")).unwrap();
        let own = rt.scratch_root.starts_with(state.path());
        if let Some(k) = rt.keeper_pid() {
            crash(k);
        }
        assert!(own, "the node keeps its records in {:?}", rt.scratch_root);
    }

    /// AC-T7.4.1 (joint plan step 13, HEX-CV-L1 on Windows): do make a node's runtime as `hex node` does, on a
    /// keeper folder in a fresh state folder, and read its capacity before any `oci` box has made its scratch
    /// folder, as a scheduler opening over a fresh node reads it; expect the size of the volume that folder will
    /// be made on, and the folder still not made.
    #[test]
    fn a_fresh_nodes_capacity_is_read_before_its_scratch_folder_is_made() {
        let (name, state) = (node("fresh"), tempfile::tempdir().unwrap());
        let rt =
            WinRuntime::outliving(&name, &state.path().join("keeper"), &keeper("fresh")).unwrap();
        let read = rt.capacity(Class::Process);
        let made = rt.scratch_root.exists();
        if let Some(k) = rt.keeper_pid() {
            crash(k);
        }
        let (volume, _) = sys::disk_space(state.path()).unwrap();
        assert_eq!(read.unwrap().map(|c| c.total.disk_bytes), Some(volume));
        assert!(!made, "reading the capacity made {:?}", rt.scratch_root);
    }

    /// T-T7.1.2 (native plan step 8): a box taken back after its node AND its keeper both went — its job
    /// outliving them, as an `oci` box's container does (a `process` box's is modelled here by a handle the
    /// test holds, since kill-on-close would end a real one) — is re-held by the taking-back node's keeper,
    /// which isn't the one its record names. Do: make a box, hold its job apart (the "survives both-gone"
    /// stand-in), drop the node and crash its keeper, take the box back on a new node, then drop that node
    /// and the stand-in handle. Expect: the box's program still runs, held by the new keeper; before the
    /// re-holding fix the new keeper never held it, so it ended here.
    #[test]
    fn a_box_taken_back_after_a_both_gone_crash_is_re_held_by_the_new_keeper() {
        let (name, state) = (node("regone"), tempfile::tempdir().unwrap());
        let spec = process_box("regone");
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("regone")).unwrap();
        rt.create(spec.clone()).unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        let first = rt.keeper_pid().expect("a keeper holds the node's boxes");
        // A third handle to the box's job, so it outlives node and keeper both going, as an oci container
        // outlives them through HCS.
        let held = sys::Job::open(&format!("Local\\hex-{name}-regone"))
            .unwrap()
            .expect("the box's named job");
        drop(rt);
        crash(first);
        wait_for("the first keeper to end", || !alive(first));
        assert!(
            alive(pid),
            "the box's program ended though its job was still held"
        );
        let rt = WinRuntime::outliving(&name, state.path(), &keeper("regone")).unwrap();
        let second = rt
            .keeper_pid()
            .expect("a new keeper holds the node's boxes");
        assert_ne!(second, first, "the new node adopted the gone keeper");
        rt.adopt(&spec).unwrap();
        assert_eq!(rt.inspect(&spec.id).unwrap().state, BoxState::Running);
        // The stand-in handle and the new node both go: only the new keeper can keep the box now.
        drop(held);
        drop(rt);
        assert!(
            alive(pid),
            "the taken-back box ended when its node went, so the new keeper never re-held its job"
        );
        // Ends the box: the new keeper's handle is its last, so crashing it ends the program.
        crash(second);
        wait_for("the new keeper's end to end the program", || !alive(pid));
    }

    /// A runtime driven in process keeps kill-on-close: do make a box, run a program, drop the runtime;
    /// expect the program ended with it.
    #[test]
    fn an_in_process_runtimes_box_ends_with_it() {
        let spec = process_box("in-process");
        let rt = WinRuntime::new();
        rt.create(spec.clone()).unwrap();
        let pid = rt.exec(&spec.id, waiting(), Stdio::null()).unwrap().pid();
        drop(rt);
        wait_for("the program to end with its runtime", || !alive(pid));
    }

    fn invalid_recovery() -> Refusal {
        Refusal::Invalid {
            what: "native device recovery",
            why: "fixture marker disagrees".to_owned(),
        }
    }

    fn marker_record(rt: &WinRuntime, spec: &BoxSpec) -> hex_core::hcs::BoxRecord {
        hex_core::hcs::BoxRecord {
            folder: r"C:\fixture\scratch".to_owned(),
            made: Vec::new(),
            outlives: true,
            system: Some(spec.id.clone()),
            started: false,
            init: None,
            devices: rt.bindings(spec).unwrap(),
            storage: hex_core::hcs::RecordStorage::OwnedFolder,
            creation: hex_core::hcs::NativeCreate::Created,
        }
    }

    /// AC-T7.4.1: recovery of a box whose marker fails validation returns that validation failure, its
    /// claims held once in one uncertain slot. A second alive or adopt of the same box (a duplicate
    /// adopt) reserves nothing and returns the same failure; an existing slot lacking a recorded claim
    /// returns that refusal beside the failure, never in its place.
    #[test]
    fn retained_recovery_keeps_the_validation_failure_and_one_slot() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let spec = device_spec("retained", 512);
        let record = marker_record(&rt, &spec);
        let first = rt.retain_recovery(&spec, &record, invalid_recovery());
        assert!(matches!(
            first,
            Refusal::Invalid {
                what: "native device recovery",
                ..
            }
        ));
        assert_eq!(rt.boxes().unwrap().len(), 1);
        let second = rt.retain_recovery(&spec, &record, invalid_recovery());
        assert!(matches!(
            second,
            Refusal::Invalid {
                what: "native device recovery",
                ..
            }
        ));
        assert_eq!(rt.boxes().unwrap().len(), 1);
        let mut wider = marker_record(&rt, &spec);
        wider.devices.push(hex_core::hcs::DeviceBinding {
            id: DeviceId {
                kind: hex_api::DeviceKind::Gpu,
                key: 256,
            },
            location: "PCIROOT(0)#PCI(0100)".to_owned(),
        });
        let refused = rt.retain_recovery(&spec, &wider, invalid_recovery());
        assert!(
            matches!(&refused, Refusal::Broken(why) if why.contains("fixture marker disagrees") && why.contains("recorded claims")),
            "{refused:?}"
        );
        assert_eq!(rt.boxes().unwrap().len(), 1);
    }

    fn vm_record(name: &str) -> hex_core::record::BoxRecord {
        let fixed = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
            disk_bytes: 0,
        };
        let cost = hex_api::RuntimeCost {
            id: [0x77; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        let mut spec = device_spec(name, 512);
        spec.devices.clear();
        spec.class = ClassSpec::Shards(Box::new(hex_api::OciSpec {
            rootfs: Rootfs::Layers(vec![PathBuf::from(r"C:\fixture\base")]),
            init: Program::of(&["cmd.exe", "/c", "exit", "0"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        }));
        spec.allocation = Some(cost.allocate(CapacityResources::default()).unwrap());
        let mut record =
            hex_core::record::BoxRecord::new(spec, hex_core::record::Phase::Creating, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([0x78; 16]));
        record
    }

    /// AC-T7.4.1: a creator marks only the slot holding its own job receipt. A stale creator whose
    /// slot was released and relaunched never marks the newer creator's slot; its own goes to
    /// cleanup, and one already in cleanup needs no mark.
    #[test]
    fn a_stale_creator_never_marks_a_newer_creators_slot() {
        let scratch = tempfile::tempdir().unwrap();
        let rt = device_runtime(scratch.path());
        let record = vm_record("relaunched");
        let native = hex_core::record::NativeRecord::Box(&record);
        let bounds = hex_api::Bounds::default();
        let stale =
            JobBox::prepare_vm(native, bounds.clone(), rt.processors, None, &rt.holding).unwrap();
        let newer = JobBox::prepare_vm(native, bounds, rt.processors, None, &rt.holding).unwrap();
        rt.holding.reserve(native.owner(), &[], None, None).unwrap();
        assert!(
            !rt.holding
                .made(native.owner(), Held::Process(NativeArc::clone(&newer)))
                .unwrap()
        );
        let failure = Refusal::Broken("stale creator failed".to_owned());
        rt.holding.uncertain_vm_after(&stale, &failure).unwrap();
        {
            let boxes = rt.boxes().unwrap();
            let (_, slot) = boxes.first().unwrap();
            assert!(
                matches!(&slot.state, SlotState::Finishing(Held::Process(current)) if NativeArc::ptr_eq(current, &newer))
            );
        }
        rt.holding.uncertain_vm_after(&newer, &failure).unwrap();
        rt.holding.uncertain_vm_after(&newer, &failure).unwrap();
        let boxes = rt.boxes().unwrap();
        let (_, slot) = boxes.first().unwrap();
        assert!(
            matches!(&slot.state, SlotState::CleanupPending(Held::Process(current)) if NativeArc::ptr_eq(current, &newer))
        );
    }
}
