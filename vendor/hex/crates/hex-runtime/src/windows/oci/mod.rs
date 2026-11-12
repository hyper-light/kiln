//! The `oci` class on Windows (docs/design/runtime.md, decision "the Windows `oci` class is an HCS silo
//! holding a Hex job"): a Windows image run process-isolated through the Host Compute Service, as the OCI
//! runtime spec's Windows section defines it. A box is an HCS compute system on the image's layers over a
//! scratch of its own, with a job object of Hex's nested in its silo: every program Hex runs there is
//! started suspended inside that job by Hex's launcher ([`launch`]), so the box's bounds hold on its
//! members by the `process` class's own rule (`hex_core::job`), while HCS bounds the container as a whole.
//! What the spec asks is decided in `hex_core::hcs`; this does the calls.

pub mod launch;
pub(crate) mod service;
pub mod sys;

use std::io::Read;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hex_api::{Bounds, BoxId, BoxState, Exit, Proc, Refusal, Signal, Usage};
use hex_core::hcs::{self, Container, HcsError, Process, Scratch, Storage};

use super::proc::BoxProc;
use super::process::JobBox;
use super::sys::Handle;
use sys::{HcsFailure, System};

/// The scratch file a box's folder holds, a copy of the base layer's `blank.vhdx` (note §3).
const SANDBOX: &str = "sandbox.vhdx";

/// What the box made for its scratch, undone at delete: the scratch folder vmcompute activated (its
/// `sandbox.vhdx` attached as the volume), whether the storage filter was put over it, and the files Hex
/// created.
struct Mounted {
    volume: String,
    activated: Option<String>,
    prepared: bool,
    made: Vec<PathBuf>,
    /// The marker naming this scratch for a node that starts after this one was killed ([`reclaim`]),
    /// removed last, once everything else is.
    marker: Option<PathBuf>,
    /// The box's record as the marker holds it, rewritten as an outliving box's start goes ([`OciBox::start`]).
    record: Option<hcs::BoxRecord>,
    /// Set once the box's container exists and outlives its node: dropped without a delete (the node going
    /// down), its scratch stays the container's, for the next node to take back. Until then a failed create
    /// undoes it as any box's, so no create leaves a scratch attached without a container.
    owner: ScratchOwner,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScratchOwner {
    Node,
    Container,
    /// Assigned EndsWithNode boxes keep their marker until physical termination is observed.
    Retained,
    Uncertain,
}

/// The scratch markers under `root`, one per box, `<id>.scratch.json`.
const MARKER_SUFFIX: &str = ".scratch.json";

/// A marker's next record while it is written, `<id>.scratch.json.next`, moved over the marker once whole.
const NEXT_SUFFIX: &str = ".next";

/// Where `marker`'s next record is written before it is moved over it.
fn next_of(marker: &Path) -> PathBuf {
    let mut next = marker.as_os_str().to_owned();
    next.push(NEXT_SUFFIX);
    PathBuf::from(next)
}

/// Writes `record` as `marker` whole: to its next file, then moved over it, so a node killed while writing
/// leaves the record before or the record after, never a torn one.
fn write_marker(marker: &Path, record: &hcs::BoxRecord) -> Result<(), Refusal> {
    let bytes = hcs::write_record(record)?;
    let next = next_of(marker);
    std::fs::write(&next, bytes).map_err(|e| Refusal::os("writing a box's record", e))?;
    std::fs::rename(&next, marker).map_err(|e| Refusal::os("writing a box's record", e))
}

/// Reads the box's record at `marker`, at most [`hcs::MAX_RECORD`] bytes: what a node taking the box back
/// reopens ([`OciBox::take_back`]).
pub fn read_marker(marker: &Path) -> Result<hcs::BoxRecord, Refusal> {
    read_marker_if_present(marker)?
        .ok_or_else(|| Refusal::os("reading a scratch marker", std::io::ErrorKind::NotFound))
}

/// The same bounded reader, with absence kept separate from a read or parse failure.
pub(crate) fn read_marker_if_present(marker: &Path) -> Result<Option<hcs::BoxRecord>, Refusal> {
    let Some(bytes) = super::keeper::read_native_bytes(marker, hcs::record_bound()?)? else {
        return Ok(None);
    };
    hcs::read_record(&bytes).map(Some).map_err(|r| match r {
        Refusal::Invalid { what, why } => Refusal::Invalid {
            what,
            why: format!("{}: {why}", marker.display()),
        },
        other => other,
    })
}

impl Mounted {
    fn mark(&mut self, marker: PathBuf, record: hcs::BoxRecord) -> Result<(), Refusal> {
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Refusal::os("writing a scratch marker", e))?;
        }
        write_marker(&marker, &record)?;
        self.marker = Some(marker);
        self.record = Some(record);
        Ok(())
    }

    fn creation(&mut self, state: hcs::NativeCreate) -> Result<(), Refusal> {
        let (Some(marker), Some(record)) = (self.marker.as_ref(), self.record.as_mut()) else {
            return Err(creation_uncertain());
        };
        record.creation = state;
        write_marker(marker, record)
    }

    /// Takes the filter off, detaches the volume and removes what Hex made; the first failure is returned,
    /// every step is still tried. `UnprepareLayer` answering `ERROR_FLT_DO_NOT_DETACH` is the container's
    /// end having taken the filter (note §3), matched alone; `DeactivateLayer` then detaches the volume.
    fn undo(&mut self) -> Result<(), Refusal> {
        if let Some(marker) = self.marker.as_ref() {
            service::remove(marker)?;
        }
        if let Some(scratch) = self.activated.as_ref() {
            if self.prepared {
                match sys::unprepare_layer(scratch) {
                    Ok(()) => {}
                    Err(f) if hcs::hcs_error(f.code) == HcsError::FilterHeld => {}
                    Err(f) => {
                        return Err(failure(
                            "taking the storage filter off a container's scratch",
                            f,
                        ));
                    }
                }
                self.prepared = false;
            }
            sys::deactivate_layer(scratch)
                .map_err(|f| failure("detaching a container's scratch", f))?;
            self.activated = None;
        }
        for _ in 0..self.made.len() {
            let Some(path) = self.made.last() else {
                break;
            };
            let gone = if path.is_dir() {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            match gone {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Refusal::os("removing a container's scratch", e)),
            }
            self.made.pop();
        }
        if let Some(marker) = self.marker.as_ref() {
            match std::fs::remove_file(marker) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Refusal::os("removing a container's scratch marker", e)),
            }
            self.marker = None;
        }
        Ok(())
    }
}

/// Reclaims the scratch a killed node left under `root` (each `<id>.scratch.json` marker's folder):
/// detaches its volume where it is still activated (UnprepareLayer, then DeactivateLayer, which detaches it
/// whether or not the unprepare took: jupiter, 2026-11-06) and removes what Hex made, then the marker. The
/// containers of boxes that end with their node ended with it (ShouldTerminateOnLastHandleClosed, the
/// job's kill-on-close), so the scratch is all a crash leaves; an outliving box whose container HCS still
/// runs is left for the node's recovery to take back or end (`hcs::at_start`). Bounded by `root`'s entries, read once; each failure is returned, typed,
/// and its marker kept so the next start tries again.
pub fn reclaim(root: &Path) -> Vec<Refusal> {
    let mut failures = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return failures,
        Err(e) => {
            failures.push(Refusal::os("listing the scratch markers", e));
            return failures;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(e) => {
                failures.push(Refusal::os("listing the scratch markers", e));
                continue;
            }
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // A next record whose marker is gone is a first write a killed node didn't finish, made before
        // anything else was; one beside its marker goes with the marker.
        if let Some(marker) = name.strip_suffix(NEXT_SUFFIX)
            && marker.ends_with(MARKER_SUFFIX)
            && !root.join(marker).exists()
            && let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            failures.push(Refusal::os(
                "removing a scratch marker's unfinished record",
                e,
            ));
        }
        if !name.ends_with(MARKER_SUFFIX) {
            continue;
        }
        if let Err(r) = reclaim_one(&path) {
            failures.push(r);
        }
    }
    failures
}

fn reclaim_one(marker: &Path) -> Result<(), Refusal> {
    service::reconcile(marker, false)?;
    reclaim_one_using(
        marker,
        |record| match &record.system {
            Some(id) => sys::open_system(id.as_str())
                .map(|system| system.is_some())
                .map_err(|f| failure("reopening a killed node's container", f)),
            None => Ok(false),
        },
        remove_scratch,
    )
}

fn reclaim_one_using(
    marker: &Path,
    system_exists: impl FnOnce(&hcs::BoxRecord) -> Result<bool, Refusal>,
    cleanup: impl FnOnce(&hcs::BoxRecord, &Path) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    let m = read_marker(marker)?;
    // An assigned create may still own a live system after a returned error or closed handle.
    // Its marker is the only durable exact binding/cleanup identity, even for EndsWithNode.
    let system_runs = system_exists(&m)?;
    if (!m.devices.is_empty() && system_runs)
        || hcs::at_start(&m, system_runs) == hcs::AtStart::Leave
    {
        return Ok(());
    }
    if m.creation == hcs::NativeCreate::Pending {
        return Err(creation_uncertain());
    }
    cleanup(&m, marker)
}

/// What a node finds of the box a record names (native plan step 8, T-T7.1.2): its compute system, reopened
/// by id where HCS still holds it, and the process now at its init's pid where one is there, with
/// [`hcs::Found`], what [`hcs::take_back`] decides on.
struct Reopened {
    system: Option<System>,
    init: Option<Handle>,
    found: hcs::Found,
}

/// Reopens what `record` names of its box, for [`OciBox::take_back`] and [`alive`]: its compute system by
/// id, and its init by pid with that process's creation time and whether it still runs.
fn reopen(record: &hcs::BoxRecord) -> Result<Reopened, Refusal> {
    let system = match &record.system {
        Some(s) => {
            sys::open_system(s.as_str()).map_err(|f| failure("reopening a box's container", f))?
        }
        None => None,
    };
    let init = match record.init {
        Some(r) => {
            super::sys::open_process(r.pid).map_err(|e| Refusal::os("reopening a box's init", e))?
        }
        None => None,
    };
    let seen = match &init {
        Some(h) => Some(hcs::SeenProcess {
            created: super::sys::created(h)
                .map_err(|e| Refusal::os("reading when a box's init was created", e))?,
            running: !super::sys::wait(h, 0)
                .map_err(|e| Refusal::os("reading whether a box's init runs", e))?,
        }),
        None => None,
    };
    Ok(Reopened {
        found: hcs::Found {
            system: system.is_some(),
            init: seen,
        },
        system,
        init,
    })
}

/// Whether the box whose record is at `marker` is one its next node takes back (native plan step 8,
/// T-T7.1.2; the node's recovery asks it before it takes a box back or removes it): what
/// [`OciBox::take_back`] finds of it, decided as [`hcs::take_back`] decides, so recovery takes back only a
/// box its take-back takes and removes the rest: one made to end with its node, one whose container HCS no
/// longer holds, and one whose init ended though its container runs on (an outliving container doesn't end
/// with its init). A record already gone names nothing to take back.
pub fn alive(marker: &Path) -> Result<bool, Refusal> {
    if !marker.exists() {
        return Ok(false);
    }
    service::reconcile(marker, false)?;
    let record = read_marker(marker)?;
    if record.creation == hcs::NativeCreate::Pending {
        return Err(creation_uncertain());
    }
    if matches!(record.creation, hcs::NativeCreate::Failed { .. }) {
        return Ok(false);
    }
    let found = reopen(&record)?.found;
    Ok(hcs::take_back(&record, found).is_ok())
}

/// Ends what is left of the box the record at `marker` names, one a node before this one made that the
/// node's recovery removes rather than takes back (native plan step 8, T-T7.1.2): its container where HCS
/// still holds it, then its scratch and its record, as [`reclaim`] removes a killed node's. A record
/// already gone has nothing left to end.
pub fn end(marker: &Path) -> Result<(), Refusal> {
    if marker.exists() {
        service::reconcile(marker, true)?;
    }
    end_using(marker, end_system, remove_scratch)
}

fn creation_uncertain() -> Refusal {
    Refusal::Invalid {
        what: "native box lifecycle",
        why: "assigned create has no terminal cancellation or physical removal witness; claims remain held".to_owned(),
    }
}

fn end_using(
    marker: &Path,
    terminate: impl FnOnce(&hcs::BoxRecord) -> Result<bool, Refusal>,
    cleanup: impl FnOnce(&hcs::BoxRecord, &Path) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    if !marker.exists() {
        return Ok(());
    }
    let m = read_marker(marker)?;
    if !terminate(&m)? {
        return Err(creation_uncertain());
    }
    cleanup(&m, marker)
}

/// True only after actual termination or absence of a create already known terminal.
fn end_system(m: &hcs::BoxRecord) -> Result<bool, Refusal> {
    if let hcs::NativeCreate::Failed { owner, .. } = m.creation {
        match owner {
            hcs::create::SystemOwner::NotSubmitted => return Ok(true),
            hcs::create::SystemOwner::Unknown => return Err(creation_uncertain()),
            hcs::create::SystemOwner::Returned => {}
        }
    }
    if let Some(id) = &m.system
        && let Some(system) = sys::open_system(id.as_str())
            .map_err(|f| failure("reopening a box's container to end it", f))?
    {
        match system.terminate(launch::PATIENCE) {
            Ok(()) => {}
            Err(f)
                if matches!(
                    hcs::hcs_error(f.code),
                    HcsError::AlreadyStopped | HcsError::NotFound
                ) => {}
            Err(f) => return Err(failure("ending a box's container", f)),
        }
        return Ok(true);
    }
    Ok(m.creation.terminal())
}

/// A missing marker is not absence. Only the existing HCS open result can establish that outcome.
pub(crate) fn require_absent(id: &BoxId) -> Result<(), Refusal> {
    match sys::open_system(id.as_str())
        .map_err(|f| failure("establishing an uncertain container's absence", f))?
    {
        None => Ok(()),
        Some(_) => Err(Refusal::Broken(
            "native create outcome has no marker but its system still exists; claims remain held"
                .to_owned(),
        )),
    }
}

/// A fresh create that never entered HCS owns only its prepared storage/marker. In particular it must
/// not open or terminate some pre-existing compute system with the same textual ID.
pub(crate) fn discard_prepared(marker: &Path) -> Result<(), Refusal> {
    if marker.exists() {
        return remove_scratch(&read_marker(marker)?, marker);
    }
    match std::fs::remove_file(next_of(marker)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Refusal::os(
            "removing a prepared scratch marker's unfinished record",
            error,
        )),
    }
}

fn copy_bindings(bindings: &[hcs::DeviceBinding]) -> Result<Vec<hcs::DeviceBinding>, Refusal> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bindings.len())
        .map_err(|_| super::allocation(bindings.len()))?;
    for binding in bindings {
        copy.push(hcs::DeviceBinding {
            id: binding.id,
            location: super::copy_text(&binding.location)?,
        });
    }
    Ok(copy)
}

/// Removes the scratch record `m` names and then its marker: detaches the volume where it is still
/// attached (UnprepareLayer, then DeactivateLayer, which detaches it whether or not the unprepare took:
/// jupiter, 2026-11-06), removes what Hex made, the marker's unfinished next record, and the marker.
fn remove_scratch(m: &hcs::BoxRecord, marker: &Path) -> Result<(), Refusal> {
    remove_scratch_using(m, marker, remove_owned_scratch)
}

fn remove_scratch_using(
    m: &hcs::BoxRecord,
    marker: &Path,
    owned: impl FnOnce(&hcs::BoxRecord) -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    service::remove(marker)?;
    match m.storage {
        hcs::RecordStorage::OwnedFolder => owned(m)?,
        hcs::RecordStorage::BorrowedVolume if m.made.is_empty() => {}
        hcs::RecordStorage::BorrowedVolume => {
            return Err(Refusal::Invalid {
                what: "box record storage",
                why: "borrowed volume cannot own created paths".to_owned(),
            });
        }
    }
    if let Err(e) = std::fs::remove_file(next_of(marker))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(Refusal::os(
            "removing a scratch marker's unfinished record",
            e,
        ));
    }
    std::fs::remove_file(marker).map_err(|e| Refusal::os("removing a scratch marker", e))
}

fn remove_owned_scratch(m: &hcs::BoxRecord) -> Result<(), Refusal> {
    if sys::layer_mount_path(&m.folder).is_ok() {
        let unprepared = sys::unprepare_layer(&m.folder);
        sys::deactivate_layer(&m.folder).map_err(|f| {
            failure(
                "detaching a killed node's scratch",
                HcsFailure {
                    message: Some(format!(
                        "{} (unprepare: {unprepared:?})",
                        f.message.clone().unwrap_or_default()
                    )),
                    ..f
                },
            )
        })?;
    }
    for p in m.made.iter().rev().map(Path::new) {
        let gone = if p.is_dir() {
            std::fs::remove_dir(p)
        } else {
            std::fs::remove_file(p)
        };
        if let Err(e) = gone
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Refusal::os("removing a killed node's scratch", e));
        }
    }
    Ok(())
}

/// Storage ownership is revalidated before any native recovery callback.
pub(crate) fn validate_storage(record: &hcs::BoxRecord, scratch: &Scratch) -> Result<(), Refusal> {
    match (record.storage, scratch) {
        (hcs::RecordStorage::OwnedFolder, Scratch::Folder(_)) => Ok(()),
        (hcs::RecordStorage::BorrowedVolume, Scratch::Volume(volume))
            if record.folder == *volume && record.made.is_empty() =>
        {
            Ok(())
        }
        _ => Err(Refusal::Invalid {
            what: "native storage recovery",
            why: "recorded storage ownership or exact borrowed volume disagrees".to_owned(),
        }),
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        // The backstop for a box dropped without its delete; delete reports these failures. An outliving
        // box's scratch is its container's while that runs on.
        if self.owner == ScratchOwner::Node {
            let _ = self.undo();
        }
    }
}

fn failure(op: &'static str, f: HcsFailure) -> Refusal {
    hcs::refusal(op, f.code, sys::host(), f.message.as_deref())
}

/// Why box `id` isn't taken back, by name, for the node's recovery to end what is left of it.
fn lost_refusal(id: &BoxId, lost: hcs::Lost) -> Refusal {
    let why = match lost {
        hcs::Lost::CreationPending => "its native create has no terminal receipt",
        hcs::Lost::CreationFailed(_) => "its native create completed with a failure",
        hcs::Lost::EndsWithNode => "it ended with the node that made it",
        hcs::Lost::NoSystem => "HCS holds no container of it",
        hcs::Lost::InitEnded => "its init has ended, or its start wasn't finished",
    };
    Refusal::Invalid {
        what: "adopt",
        why: format!("box {id}: {why}"),
    }
}

/// How the box's life stands.
struct Life {
    started: bool,
    init: Option<BoxProc>,
    exit: Option<Exit>,
}

enum NativeSystem {
    Creating(sys::PendingCreate),
    ServiceCreating(service::Pending),
    Created(System),
    Failed {
        system: Option<System>,
        failure: HcsFailure,
    },
    Removed,
}

impl NativeSystem {
    fn observe(&mut self) -> Result<(), Refusal> {
        let terminal = match self {
            Self::Creating(create) => create.query(),
            Self::ServiceCreating(create) => create.query()?,
            _ => None,
        };
        if let Some(terminal) = terminal {
            *self = match (terminal.result, terminal.system) {
                (Ok(()), Some(system)) => Self::Created(system),
                (Ok(()), None) => Self::Failed {
                    system: None,
                    failure: HcsFailure {
                        code: u32::from_ne_bytes(
                            windows_sys::Win32::Foundation::E_UNEXPECTED.to_ne_bytes(),
                        ),
                        message: None,
                    },
                },
                (Err(failure), system) => Self::Failed { system, failure },
            };
        }
        Ok(())
    }

    fn ready(&self) -> Result<&System, Refusal> {
        match self {
            Self::Created(system) => Ok(system),
            Self::ServiceCreating(_) => Err(creation_uncertain()),
            Self::Creating(create) => Err(match create.cancellation() {
                sys::CreateCancellation::Refused(code) => Refusal::Invalid {
                    what: "native box lifecycle",
                    why: format!(
                        "assigned create remains pending; cancellation refused ({code:#010x}); claims remain held"
                    ),
                },
                _ => creation_uncertain(),
            }),
            Self::Failed { failure, .. } => Err(hcs::refusal(
                "creating an assigned container",
                failure.code,
                sys::host(),
                None,
            )),
            Self::Removed => Err(Refusal::Invalid {
                what: "native box lifecycle",
                why: "the compute system has been physically removed".to_owned(),
            }),
        }
    }

    fn remove(&mut self) -> Result<(), Refusal> {
        if let Self::Creating(create) = self {
            create.cancel();
        }
        if let Self::ServiceCreating(create) = self {
            // Cancellation can refuse while the keeper owns the native operation. Still observe
            // its durable terminal result; cancellation itself is never the terminal fact.
            let _ = create.cancel();
        }
        self.observe()?;
        let system = match self {
            Self::Created(system)
            | Self::Failed {
                system: Some(system),
                ..
            } => Some(system),
            Self::Failed { system: None, .. } | Self::Removed => None,
            Self::Creating(_) | Self::ServiceCreating(_) => return self.ready().map(|_| ()),
        };
        if let Some(system) = system {
            match system.terminate(launch::PATIENCE) {
                Ok(()) => {}
                Err(f)
                    if matches!(
                        hcs::hcs_error(f.code),
                        HcsError::AlreadyStopped | HcsError::NotFound
                    ) => {}
                Err(f) => return Err(failure("ending a container", f)),
            }
        }
        *self = Self::Removed;
        Ok(())
    }
}

/// A Windows `oci` box.
pub struct OciBox {
    job: JobBox,
    /// Dropped before the scratch: closing its last handle ends the container where it ends with its node
    /// (it was created to), and leaves it running where it outlives it.
    system: Mutex<NativeSystem>,
    mounted: Mutex<Option<Mounted>>,
    user: Option<String>,
    init: Process,
    life: Mutex<Life>,
    devices: Vec<hcs::DeviceBinding>,
}

/// Whether HCS is here; `Err` is the class refused by name, saying the installer turns it on.
pub fn available() -> Result<(), Refusal> {
    sys::available().map_err(|missing| hcs::containers_off(&missing))
}

pub(super) fn initialize_api() {
    sys::initialize_api();
}

pub(super) fn initialized_api_loaded() -> Option<bool> {
    sys::initialized_api_loaded()
}

/// A bundle's `config.json` at `dir`, read at most [`hex_core::bundle::MAX_CONFIG_BYTES`] bytes (the
/// Linux bundle reader's bound: one rule for both) and parsed.
pub fn bundle_config(dir: &Path) -> Result<serde_json::Value, Refusal> {
    const MAX_CONFIG: usize = hex_core::bundle::MAX_CONFIG_BYTES;
    let path = dir.join("config.json");
    let mut text = String::new();
    std::fs::File::open(&path)
        .and_then(|f| {
            f.take(
                u64::try_from(MAX_CONFIG)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .read_to_string(&mut text)
        })
        .map_err(|e| Refusal::os("reading a bundle's config.json", e))?;
    if text.len() > MAX_CONFIG {
        return Err(Refusal::Invalid {
            what: "oci spec",
            why: format!("config.json is past {MAX_CONFIG} bytes"),
        });
    }
    serde_json::from_str(&text).map_err(|e| Refusal::Invalid {
        what: "oci spec",
        why: format!("config.json isn't JSON: {e}"),
    })
}

impl OciBox {
    /// Makes the box `job` is for, for `container`, Hex's launcher mapped in from the folder `launcher`:
    /// checks the image runs on this host, prepares the scratch, and creates the compute system, which
    /// waits for [`OciBox::start`]. The node makes the box's job (named and held by its keeper where the box
    /// outlives it), so the class never names one. Its record (`marker`, an [`hcs::BoxRecord`]) and its
    /// container outlive its node where `lifetime` says so.
    pub fn create(
        job: JobBox,
        container: &Container,
        launcher: &Path,
        marker: Option<PathBuf>,
        lifetime: hcs::Lifetime,
    ) -> Result<Self, Refusal> {
        Self::create_assigned(job, container, launcher, marker, lifetime, Vec::new())
    }

    pub(crate) fn create_assigned(
        job: JobBox,
        container: &Container,
        launcher: &Path,
        marker: Option<PathBuf>,
        lifetime: hcs::Lifetime,
        devices: Vec<hcs::DeviceBinding>,
    ) -> Result<Self, Refusal> {
        Self::create_assigned_at(job, container, launcher, marker, lifetime, devices, None)
    }

    pub(crate) fn create_assigned_at(
        job: JobBox,
        container: &Container,
        launcher: &Path,
        marker: Option<PathBuf>,
        lifetime: hcs::Lifetime,
        devices: Vec<hcs::DeviceBinding>,
        keeper_state: Option<&Path>,
    ) -> Result<Self, Refusal> {
        available()?;
        let host = sys::host();
        let base = container.layers.last().ok_or_else(|| Refusal::Invalid {
            what: "oci spec",
            why: "the image has no layers".to_owned(),
        })?;
        let ntdll = format!(r"{base}\Files\Windows\System32\ntdll.dll");
        let image = sys::file_build(&ntdll)
            .map_err(|e| Refusal::os("reading the image's Windows build from its base layer", e))?;
        hcs::image_runs(host, image)?;
        let id = job.id()?.try_clone()?;
        let mut mounted = scratch(container, base, marker, &id, lifetime, &devices)?;
        if !container.hosts.is_empty() {
            append_hosts(&mounted.volume, &hcs::hosts_lines(&container.hosts))?;
        }
        let storage = Storage {
            volume: mounted.volume.clone(),
            layers: container
                .layers
                .iter()
                .map(|l| (hcs::layer_id(l), l.clone()))
                .collect(),
        };
        let launcher = launcher
            .to_str()
            .ok_or_else(|| Refusal::os("naming the launcher's folder", "not UTF-8"))?;
        let document = hcs::document(container, &storage, launcher, lifetime).to_string();
        // The marker already owns the exact system ID and bindings. A returned failure or a closed
        // handle does not prove cancellation completed; retain the scratch for explicit cleanup.
        if !devices.is_empty() {
            mounted.creation(hcs::NativeCreate::Pending)?;
            mounted.owner = ScratchOwner::Uncertain;
        }
        let system = if devices.is_empty() {
            match sys::create_system(id.as_str(), &document, launch::PATIENCE) {
                Ok(system) => NativeSystem::Created(system),
                Err(f) => {
                    mounted.undo()?;
                    return Err(failure("creating a container", f));
                }
            }
        } else if let Some(state) = keeper_state {
            let marker = mounted.marker.as_ref().ok_or_else(creation_uncertain)?;
            NativeSystem::ServiceCreating(service::submit(
                state,
                marker,
                &document,
                launch::PATIENCE,
            )?)
        } else {
            match sys::create_system_owned(id.as_str(), &document, launch::PATIENCE) {
                Ok(create) => NativeSystem::Creating(create),
                Err(f) => {
                    // This API returns Err only before HcsCreateComputeSystem was called.
                    // Returned native failures/pending operations remain owned by Creating instead.
                    mounted.owner = ScratchOwner::Node;
                    mounted.creation(hcs::NativeCreate::Created)?;
                    mounted.undo()?;
                    return Err(hcs::refusal(
                        "preparing an assigned container",
                        f.code,
                        host,
                        None,
                    ));
                }
            }
        };
        if devices.is_empty() {
            mounted.owner = if lifetime == hcs::Lifetime::Outlives {
                ScratchOwner::Container
            } else {
                ScratchOwner::Node
            };
        }
        Ok(Self {
            job,
            system: Mutex::new(system),
            mounted: Mutex::new(Some(mounted)),
            user: container.init.user.clone(),
            init: container.init.clone(),
            life: Mutex::new(Life {
                started: false,
                init: None,
                exit: None,
            }),
            devices,
        })
    }

    pub(crate) fn device_bindings(&self) -> &[hcs::DeviceBinding] {
        &self.devices
    }

    pub fn id(&self) -> Result<&BoxId, Refusal> {
        self.job.id()
    }

    pub fn bounds(&self) -> Bounds {
        self.job.bounds()
    }

    fn life(&self) -> Result<std::sync::MutexGuard<'_, Life>, Refusal> {
        self.life
            .lock()
            .map_err(|_| Refusal::Broken("an oci box's state lock is poisoned".to_owned()))
    }

    fn ready_system(&self) -> Result<std::sync::MutexGuard<'_, NativeSystem>, Refusal> {
        let mut system = self
            .system
            .lock()
            .map_err(|_| Refusal::Broken("an oci box's system lock is poisoned".to_owned()))?;
        system.observe()?;
        system.ready()?;
        if !self.devices.is_empty() {
            let mut mounted = self
                .mounted
                .lock()
                .map_err(|_| Refusal::Broken("an oci box's scratch lock is poisoned".to_owned()))?;
            let mounted = mounted.as_mut().ok_or_else(creation_uncertain)?;
            if mounted
                .record
                .as_ref()
                .is_some_and(|record| record.creation == hcs::NativeCreate::Pending)
            {
                mounted.creation(hcs::NativeCreate::Created)?;
                mounted.owner = if mounted
                    .record
                    .as_ref()
                    .is_some_and(|record| record.outlives)
                {
                    ScratchOwner::Container
                } else {
                    ScratchOwner::Retained
                };
            }
        }
        Ok(system)
    }

    /// Starts `process` in the box through Hex's launcher: its process handle and pid.
    fn launch_member(
        &self,
        process: &Process,
        stdio: [OwnedHandle; 3],
    ) -> Result<(Handle, u32), Refusal> {
        let system = self.ready_system()?;
        launch::start(
            system.ready()?,
            &self.job,
            self.user.as_deref(),
            process,
            stdio,
        )
    }

    fn launch(&self, process: &Process, stdio: [OwnedHandle; 3]) -> Result<BoxProc, Refusal> {
        let (member, pid) = self.launch_member(process, stdio)?;
        Ok(BoxProc::new(member, pid, self.job.shared()))
    }

    /// Whether the box's container and scratch outlive its node (set once its container exists).
    fn outlives(&self) -> Result<bool, Refusal> {
        Ok(self
            .mounted
            .lock()
            .map_err(|_| Refusal::Broken("an oci box's scratch lock is poisoned".to_owned()))?
            .as_ref()
            .is_some_and(|m| m.owner == ScratchOwner::Container))
    }

    /// Rewrites an outliving box's record with `change`; a box that ends with its node keeps its first.
    fn record(&self, change: impl FnOnce(&mut hcs::BoxRecord)) -> Result<(), Refusal> {
        let mut mounted = self
            .mounted
            .lock()
            .map_err(|_| Refusal::Broken("an oci box's scratch lock is poisoned".to_owned()))?;
        if let Some(m) = mounted.as_mut()
            && m.owner == ScratchOwner::Container
            && let (Some(marker), Some(record)) = (m.marker.as_ref(), m.record.as_mut())
        {
            change(record);
            write_marker(marker, record)?;
        }
        Ok(())
    }

    /// The box's job handle value, for the node's keeper to hold a second handle to it (`super::keeper`).
    pub fn job_token(&self) -> Result<usize, Refusal> {
        self.job.job_token()
    }

    /// Takes back the box a node before this one made and left running (native plan step 8, T-T7.1.2):
    /// `job` its job, taken back by its name, `container` what its spec asks, `record` what its marker at
    /// `marker` says. Reopens its compute system by id, its scratch (still attached: its container holds
    /// it), and its init where it started, told from a process given that pid since by its creation time
    /// ([`hcs::take_back`]). Refused by name where any of them is gone, for recovery to end what is left.
    pub fn take_back(
        job: JobBox,
        container: &Container,
        marker: PathBuf,
        record: hcs::BoxRecord,
    ) -> Result<Self, Refusal> {
        Self::take_back_assigned(job, container, marker, record, Vec::new())
    }

    pub(crate) fn take_back_assigned(
        job: JobBox,
        container: &Container,
        marker: PathBuf,
        record: hcs::BoxRecord,
        devices: Vec<hcs::DeviceBinding>,
    ) -> Result<Self, Refusal> {
        validate_storage(&record, &container.scratch)?;
        if record.devices != devices
            || record.creation != hcs::NativeCreate::Created
            || (!devices.is_empty() && record.system.as_ref() != Some(job.id()?))
        {
            return Err(Refusal::Invalid {
                what: "native device recovery",
                why: "recorded exact bindings or system identity disagree".to_owned(),
            });
        }
        let id = job.id()?.try_clone()?;
        let Reopened {
            system,
            init,
            found,
        } = reopen(&record)?;
        let (system, started, init) = match (hcs::take_back(&record, found), system, init) {
            (Ok(hcs::TakeBack::Created), Some(s), _) => (s, false, None),
            (Ok(hcs::TakeBack::Running(r)), Some(s), Some(h)) => {
                (s, true, Some(BoxProc::new(h, r.pid, job.shared())))
            }
            (Err(lost), _, _) => return Err(lost_refusal(&id, lost)),
            (Ok(_), _, _) => {
                return Err(Refusal::Broken(format!(
                    "box {id} was taken back without the container or init its record names"
                )));
            }
        };
        let (volume, activated, prepared) = match record.storage {
            hcs::RecordStorage::OwnedFolder => (
                sys::layer_mount_path(&record.folder)
                    .map_err(|f| failure("reading a taken-back box's scratch volume", f))?,
                Some(super::copy_text(&record.folder)?),
                true,
            ),
            hcs::RecordStorage::BorrowedVolume => (super::copy_text(&record.folder)?, None, false),
        };
        let mounted = Mounted {
            volume,
            activated,
            prepared,
            made: record.made.iter().map(PathBuf::from).collect(),
            marker: Some(marker),
            record: Some(record),
            owner: ScratchOwner::Container,
        };
        Ok(Self {
            job,
            system: Mutex::new(NativeSystem::Created(system)),
            mounted: Mutex::new(Some(mounted)),
            user: container.init.user.clone(),
            init: container.init.clone(),
            life: Mutex::new(Life {
                started,
                init,
                exit: None,
            }),
            devices,
        })
    }

    /// Starts the container and its init (the runtime-spec's `start`); a second start is refused.
    pub fn start(&self) -> Result<(), Refusal> {
        let mut life = self.life()?;
        if life.started {
            return Err(Refusal::Invalid {
                what: "start",
                why: format!("box {} has started already", self.id()?),
            });
        }
        // An outliving box's record says a start was asked before HCS is, and names its init once it runs,
        // so a node killed in between leaves a start no node takes back unfinished (hcs::take_back).
        let outlives = self.outlives()?;
        self.record(|r| r.started = true)?;
        {
            let system = self.ready_system()?;
            system
                .ready()?
                .start(launch::PATIENCE)
                .map_err(|f| failure("starting a container", f))?;
        }
        let (member, pid) = self.launch_member(&self.init, super::null_stdio()?)?;
        let created = outlives.then(|| super::sys::created(&member));
        // The init runs from here, so the box is started whatever its record's write says: one that fails
        // leaves a record naming no init, which no node takes back.
        life.started = true;
        life.init = Some(BoxProc::new(member, pid, self.job.shared()));
        if let Some(created) = created {
            let created =
                created.map_err(|e| Refusal::os("reading when a box's init was created", e))?;
            self.record(|r| r.init = Some(hcs::InitRecord { pid, created }))?;
        }
        Ok(())
    }

    /// Runs `process` in the started box with `stdio`; refused before the start and once the init ended.
    pub fn exec(
        &self,
        process: &Process,
        stdio: [OwnedHandle; 3],
    ) -> Result<Box<dyn Proc>, Refusal> {
        if !matches!(self.state()?, BoxState::Running) {
            return Err(Refusal::NotRunning(self.id()?.try_clone()?, "exec"));
        }
        Ok(Box::new(self.launch(process, stdio)?))
    }

    /// The box's state: `Created` until its start, `Running` while its init runs, `Empty` once it ended.
    pub fn state(&self) -> Result<BoxState, Refusal> {
        drop(self.ready_system()?);
        let exit = self.exit()?;
        Ok(if !self.life()?.started {
            BoxState::Created
        } else if exit.is_some() {
            BoxState::Empty
        } else {
            BoxState::Running
        })
    }

    /// How the init ended, once it has (read without blocking, kept once read).
    pub fn exit(&self) -> Result<Option<Exit>, Refusal> {
        let mut life = self.life()?;
        if life.exit.is_none()
            && let Some(init) = life.init.as_mut()
        {
            life.exit = init.try_wait()?;
        }
        Ok(life.exit)
    }

    pub fn usage(&self) -> Result<Usage, Refusal> {
        self.job.usage()
    }

    /// Ends every program in the box with `signal` (KILL or TERM, the `process` class's rule).
    pub fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        self.job.signal(signal)
    }

    /// Ends the box's programs, terminates its container, and removes its scratch.
    pub fn delete(&self) -> Result<(), Refusal> {
        self.job.delete()?;
        let mut system = self
            .system
            .lock()
            .map_err(|_| Refusal::Broken("an oci box's system lock is poisoned".to_owned()))?;
        system.remove()?;
        let mut mounted = self
            .mounted
            .lock()
            .map_err(|_| Refusal::Broken("an oci box's scratch lock is poisoned".to_owned()))?;
        if let Some(held) = mounted.as_mut() {
            if held
                .record
                .as_ref()
                .is_some_and(|record| record.creation == hcs::NativeCreate::Pending)
            {
                held.creation(hcs::NativeCreate::Created)?;
            }
            held.owner = ScratchOwner::Node;
            held.undo()?;
            mounted.take();
        }
        Ok(())
    }
}

/// Prepares `container`'s scratch over its layers, `base` the bottom-most: a scratch folder gets a copy of
/// the base layer's `blank.vhdx` (unless it holds a `sandbox.vhdx` already), which vmcompute activates as
/// a volume and puts the storage filter over, the layers its parents, as hcsshim and runhcs do; the
/// documented storage calls made a writable layer no container finished starting on (note §3, §9); a
/// volume the caller mounted is used as it is.
fn scratch(
    container: &Container,
    base: &str,
    marker: Option<PathBuf>,
    id: &BoxId,
    lifetime: hcs::Lifetime,
    devices: &[hcs::DeviceBinding],
) -> Result<Mounted, Refusal> {
    let outlives = lifetime == hcs::Lifetime::Outlives;
    let folder = match &container.scratch {
        Scratch::Volume(v) => {
            let mut mounted = Mounted {
                volume: super::copy_text(v)?,
                activated: None,
                prepared: false,
                made: Vec::new(),
                marker: None,
                record: None,
                owner: ScratchOwner::Node,
            };
            if !devices.is_empty() {
                let marker = marker.ok_or_else(|| Refusal::Invalid {
                    what: "native device marker",
                    why: "assigned borrowed volume requires its existing native marker".to_owned(),
                })?;
                let record = hcs::BoxRecord {
                    folder: super::copy_text(v)?,
                    made: Vec::new(),
                    outlives,
                    system: Some(id.try_clone()?),
                    started: false,
                    init: None,
                    devices: copy_bindings(devices)?,
                    storage: hcs::RecordStorage::BorrowedVolume,
                    creation: hcs::NativeCreate::Created,
                };
                mounted.mark(marker, record)?;
            }
            return Ok(mounted);
        }
        Scratch::Folder(f) => PathBuf::from(f),
    };
    let mut mounted = Mounted {
        volume: String::new(),
        activated: None,
        prepared: false,
        made: Vec::new(),
        marker: None,
        record: None,
        owner: ScratchOwner::Node,
    };
    let sandbox = folder.join(SANDBOX);
    // The marker first, naming what Hex is about to make, so a node killed at any later point leaves
    // nothing the next start can't find (reclaim).
    if let Some(m) = marker {
        let mut made = Vec::new();
        if !folder.exists() {
            made.push(folder.clone());
        }
        if !sandbox.exists() {
            made.push(sandbox.clone());
        }
        let path = |p: &Path| {
            p.to_str()
                .map(str::to_owned)
                .ok_or_else(|| Refusal::os("naming a container's scratch", "not UTF-8"))
        };
        let record = hcs::BoxRecord {
            storage: hcs::RecordStorage::OwnedFolder,
            creation: hcs::NativeCreate::Created,
            folder: path(&folder)?,
            made: made
                .iter()
                .map(|p| path(p.as_path()))
                .collect::<Result<_, _>>()?,
            outlives,
            system: if outlives || !devices.is_empty() {
                Some(id.try_clone()?)
            } else {
                None
            },
            started: false,
            init: None,
            devices: copy_bindings(devices)?,
        };
        mounted.mark(m, record)?;
    }
    if !folder.exists() {
        std::fs::create_dir_all(&folder)
            .map_err(|e| Refusal::os("making a container's scratch", e))?;
        mounted.made.push(folder.clone());
    }
    if !sandbox.exists() {
        std::fs::copy(Path::new(base).join("blank.vhdx"), &sandbox)
            .map_err(|e| Refusal::os("copying the base layer's blank.vhdx as a scratch", e))?;
        mounted.made.push(sandbox.clone());
    }
    let scratch = folder
        .to_str()
        .ok_or_else(|| Refusal::os("naming a container's scratch", "not UTF-8"))?
        .to_owned();
    sys::activate_layer(&scratch).map_err(|f| failure("attaching a container's scratch", f))?;
    mounted.activated = Some(scratch.clone());
    sys::prepare_layer(&scratch, &container.layers)
        .map_err(|f| failure("putting the storage filter over a container's scratch", f))?;
    mounted.prepared = true;
    mounted.volume = sys::layer_mount_path(&scratch)
        .map_err(|f| failure("reading a container's scratch volume", f))?;
    Ok(mounted)
}

/// Appends `lines` to the container's `hosts` file on its scratch volume, before it starts.
fn append_hosts(volume: &str, lines: &str) -> Result<(), Refusal> {
    use std::io::Write;
    let path = format!(
        r"{}Windows\System32\drivers\etc\hosts",
        hcs::volume_root(volume)
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(format!("\r\n{lines}").as_bytes()))
        .map_err(|e| Refusal::os("writing a container's hosts file", e))
}

/// A box's state for the runtime's `BoxInfo`.
pub fn info(b: &OciBox) -> Result<hex_api::BoxInfo, Refusal> {
    Ok(hex_api::BoxInfo {
        published: Vec::new(),
        grants: Vec::new(),
        route: None,
        id: b.id()?.try_clone()?,
        class: hex_api::Class::Oci,
        state: b.state()?,
        bounds: b.bounds(),
        adopted: false,
        membership: None,
        exit: b.exit()?,
        abandoned: 0,
        allocation: None,
        last_resort: None,
    })
}

/// The devices implementing interface class `class` (a GUID, braces optional) where this process runs,
/// listed and reachable: the probe that a class not granted to a box isn't reachable inside it.
pub fn devices(class: &str) -> Result<sys::DeviceView, Refusal> {
    let g = hcs::class_guid("device class", class)?;
    let field = |from: usize, to: usize| {
        g.get(from..to)
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .ok_or_else(|| Refusal::Broken(format!("a checked GUID {g} didn't parse")))
    };
    let narrow = |v: u64| u8::try_from(v).map_err(|e| Refusal::Broken(e.to_string()));
    let mut data4 = [0u8; 8];
    for (i, b) in data4.iter_mut().enumerate() {
        // The last two groups, 19..23 and 24..36, hold data4's eight bytes; skip the dash at 23.
        let at = if i < 2 {
            i.saturating_mul(2).saturating_add(19)
        } else {
            i.saturating_mul(2).saturating_add(20)
        };
        *b = narrow(field(at, at.saturating_add(2))?)?;
    }
    let guid = windows_sys::core::GUID {
        data1: u32::try_from(field(0, 8)?).map_err(|e| Refusal::Broken(e.to_string()))?,
        data2: u16::try_from(field(9, 13)?).map_err(|e| Refusal::Broken(e.to_string()))?,
        data3: u16::try_from(field(14, 18)?).map_err(|e| Refusal::Broken(e.to_string()))?,
        data4,
    };
    sys::device_interfaces(&guid).map_err(|e| Refusal::os("listing device interfaces", e))
}

/// The scratch folder for a box made from an `OciSpec` under `root`.
pub fn scratch_folder(root: &Path, id: &BoxId) -> Result<String, Refusal> {
    let mut path = super::copy_path(root)?;
    let room = id
        .as_str()
        .len()
        .checked_add(1)
        .ok_or_else(|| super::allocation(usize::MAX))?;
    path.try_reserve_exact(room)
        .map_err(|_| super::allocation(room))?;
    path.push(id.as_str());
    super::copy_text(
        path.to_str()
            .ok_or_else(|| Refusal::os("naming a box's scratch folder", "not UTF-8"))?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// T-T7.1.2: reclaim must not discard an assigned marker while its own system still exists.
    /// Private files and counted native callbacks only; this does not reproduce an HCS effect.
    #[test]
    fn device_retained_marker_survives_live_system_reclaim_and_absent_cleanup_is_owned() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("assigned.scratch.json");
        let scratch = root.path().join("assigned-owned");
        std::fs::create_dir(&scratch).unwrap();
        let sibling = root.path().join("sibling.scratch.json");
        let sibling_bytes = b"unrelated private sibling marker";
        std::fs::write(&sibling, sibling_bytes).unwrap();
        let record = hcs::BoxRecord {
            storage: hcs::RecordStorage::OwnedFolder,
            creation: hcs::NativeCreate::Created,
            folder: scratch.to_str().unwrap().to_owned(),
            made: vec![scratch.to_str().unwrap().to_owned()],
            outlives: false,
            system: Some(BoxId::new("assigned").unwrap()),
            started: false,
            init: None,
            devices: vec![hcs::DeviceBinding {
                id: hex_api::DeviceId {
                    kind: hex_api::DeviceKind::Gpu,
                    key: 256,
                },
                location: "PCIROOT(0)#PCI(0100)".to_owned(),
            }],
        };
        let bytes = hcs::write_record(&record).unwrap();
        std::fs::write(&marker, &bytes).unwrap();
        let opened = Cell::new(0);
        let removed = Cell::new(0);
        reclaim_one_using(
            &marker,
            |seen| {
                assert_eq!(seen.system, record.system);
                opened.set(opened.get() + 1);
                Ok(true)
            },
            |_, _| {
                removed.set(removed.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(opened.get(), 1);
        assert_eq!(
            removed.get(),
            0,
            "live assigned system must retain its durable cleanup identity"
        );
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        assert!(scratch.is_dir());
        assert_eq!(std::fs::read(&sibling).unwrap(), sibling_bytes);

        reclaim_one_using(
            &marker,
            |_| Ok(false),
            |seen, owned_marker| {
                assert_eq!(seen, &record);
                assert_eq!(owned_marker, marker);
                removed.set(removed.get() + 1);
                std::fs::remove_dir(&scratch).unwrap();
                std::fs::remove_file(owned_marker).unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(removed.get(), 1);
        assert!(!marker.exists());
        assert!(!scratch.exists());
        assert_eq!(std::fs::read(&sibling).unwrap(), sibling_bytes);
    }

    #[test]
    fn device_borrowed_volume_keeps_same_marker_and_never_cleans_owned_storage() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("borrowed.scratch.json");
        let unowned = root.path().join("caller-volume-sentinel");
        std::fs::write(&unowned, b"caller retains this storage").unwrap();
        let volume = r"\\?\Volume{ec84d99e-3f02-11e7-ac6c-00155d7682cf}\".to_owned();
        let expected = hcs::BoxRecord {
            folder: volume.clone(),
            made: Vec::new(),
            outlives: true,
            system: Some(BoxId::new("borrowed").unwrap()),
            started: false,
            init: None,
            devices: vec![hcs::DeviceBinding {
                id: hex_api::DeviceId {
                    kind: hex_api::DeviceKind::Gpu,
                    key: 256,
                },
                location: "PCIROOT(0)#PCI(0100)".to_owned(),
            }],
            storage: hcs::RecordStorage::BorrowedVolume,
            creation: hcs::NativeCreate::Created,
        };
        let config = serde_json::json!({
            "ociVersion":"1.2.0", "root":{"path":volume},
            "process":{"args":["cmd.exe"],"cwd":"C:\\"},
            "windows":{"layerFolders":["C:\\fixture\\base","C:\\fixture\\caller-scratch"]}
        });
        let container = hcs::from_bundle(&config).unwrap();
        let mounted = scratch(
            &container,
            "unused-borrowed-base",
            Some(marker.clone()),
            expected.system.as_ref().unwrap(),
            hcs::Lifetime::Outlives,
            &expected.devices,
        )
        .unwrap();
        assert_eq!(mounted.volume, volume);
        assert!(mounted.activated.is_none());
        assert!(!mounted.prepared);
        assert!(mounted.made.is_empty());
        let mut record = read_marker(&marker).unwrap();
        assert_eq!(record, expected);
        let bytes = std::fs::read(&marker).unwrap();
        assert!(validate_storage(&record, &Scratch::Volume(volume.clone())).is_ok());
        assert!(validate_storage(&record, &Scratch::Folder(volume.clone())).is_err());
        assert!(
            validate_storage(
                &record,
                &Scratch::Volume("synthetic-private-other-volume".to_owned())
            )
            .is_err()
        );
        let owned_calls = Cell::new(0);
        record.made.push(unowned.to_str().unwrap().to_owned());
        assert!(
            remove_scratch_using(&record, &marker, |_| {
                owned_calls.set(owned_calls.get() + 1);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        assert_eq!(owned_calls.get(), 0);
        record.made.clear();
        remove_scratch_using(&record, &marker, |_| {
            owned_calls.set(owned_calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(owned_calls.get(), 0);
        assert!(!marker.exists());
        assert_eq!(
            std::fs::read(unowned).unwrap(),
            b"caller retains this storage"
        );
    }

    #[test]
    fn device_pending_marker_refuses_absent_cleanup_until_owned_terminal_observation() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("pending.scratch.json");
        let sibling = root.path().join("sibling");
        std::fs::write(&sibling, b"unaffected").unwrap();
        let record = hcs::BoxRecord {
            folder: r"\\?\Volume{ec84d99e-3f02-11e7-ac6c-00155d7682cf}\".to_owned(),
            made: Vec::new(),
            outlives: false,
            system: Some(BoxId::new("pending").unwrap()),
            started: false,
            init: None,
            devices: vec![hcs::DeviceBinding {
                id: hex_api::DeviceId {
                    kind: hex_api::DeviceKind::Gpu,
                    key: 256,
                },
                location: "synthetic-private-location".to_owned(),
            }],
            storage: hcs::RecordStorage::BorrowedVolume,
            creation: hcs::NativeCreate::Pending,
        };
        write_marker(&marker, &record).unwrap();
        let bytes = std::fs::read(&marker).unwrap();
        let cleaned = Cell::new(0);
        let error = reclaim_one_using(
            &marker,
            |_| Ok(false),
            |_, _| {
                cleaned.set(cleaned.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Refusal::Invalid {
                what: "native box lifecycle",
                ..
            }
        ));
        assert!(!error.to_string().contains("synthetic-private"));
        assert!(
            end_using(
                &marker,
                |_| Ok(false),
                |_, _| {
                    cleaned.set(cleaned.get() + 1);
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(cleaned.get(), 0);
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
        end_using(
            &marker,
            |seen| {
                assert_eq!(seen.system, record.system);
                Ok(true)
            },
            |seen, path| {
                remove_scratch_using(seen, path, |_| {
                    cleaned.set(cleaned.get() + 1);
                    Ok(())
                })
            },
        )
        .unwrap();
        assert_eq!(cleaned.get(), 0); // No owned storage operation for the borrowed volume.
        assert!(!marker.exists());
        assert_eq!(std::fs::read(sibling).unwrap(), b"unaffected");
    }
}
