//! Native create ownership in the existing keeper. Files carry operation intent/results, not
//! reservations; Holding and the node's BoxRecord remain the only physical claim authorities.
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::sys::{self, HcsFailure, TerminalCreate};
use crate::windows::{keeper, sys as win};
use hex_api::Refusal;
use hex_core::hcs::{
    self,
    create::{Outcome, Phase, Receipt, Request, SystemOwner},
};

const SUFFIX: &str = ".create";
const INPUT: &str = "input.json";
const RECEIPT: &str = "receipt.json";
const OWNER: &str = "owner";

fn refused() -> Refusal {
    Refusal::Invalid {
        what: "native create ownership",
        why: "terminal service receipt is unavailable; exact claims remain held".to_owned(),
    }
}
fn directory(marker: &Path) -> PathBuf {
    let mut name = marker.as_os_str().to_os_string();
    name.push(SUFFIX);
    PathBuf::from(name)
}
fn wake_name(state: &Path) -> Vec<u16> {
    format!("Local\\hex-{}-native-create\0", keeper::node_name(state))
        .encode_utf16()
        .collect()
}
fn completion_name(marker: &Path) -> Vec<u16> {
    format!("Local\\hex-{}-native-result\0", keeper::node_name(marker))
        .encode_utf16()
        .collect()
}
fn read(path: &Path) -> Result<Vec<u8>, Refusal> {
    let bound = hcs::create::bound()?;
    let room = bound.checked_add(1).ok_or_else(refused)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| Refusal::Exhausted {
            what: "native create bytes",
            bound: room,
        })?;
    File::open(path)
        .and_then(|f| {
            f.take(u64::try_from(room).map_err(std::io::Error::other)?)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| Refusal::os("reading a native create receipt", e))?;
    if bytes.len() > bound {
        return Err(refused());
    }
    Ok(bytes)
}
fn write(path: &Path, bytes: &[u8]) -> Result<(), Refusal> {
    let next = path.with_extension("next");
    let mut file =
        File::create(&next).map_err(|e| Refusal::os("writing a native create receipt", e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| Refusal::os("flushing a native create receipt", e))?;
    drop(file);
    fs::rename(next, path).map_err(|e| Refusal::os("publishing a native create receipt", e))
}
fn lock(dir: &Path) -> Result<Option<File>, Refusal> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(OWNER))
        .map_err(|e| Refusal::os("opening native create ownership", e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(Refusal::os("locking native create ownership", e)),
    }
}

/// A pending node call observes its keeper-owned operation through a durable terminal receipt.
/// The event is a notification; neither a timeout nor the creator's exit supplies the result.
pub(crate) struct Pending {
    marker: PathBuf,
    completion: win::Handle,
}
impl Pending {
    pub(crate) fn query(&mut self) -> Result<Option<TerminalCreate>, Refusal> {
        notify(&self.marker)?;
        let mut record = super::read_marker(&self.marker)?;
        resolve(&self.marker, &mut record)?;
        let receipt = Receipt::decode(&read(&directory(&self.marker).join(RECEIPT))?)?;
        let Some(outcome) = receipt.outcome_for(&record)? else {
            return Ok(None);
        };
        let system = match receipt.system_owner {
            SystemOwner::NotSubmitted => None,
            SystemOwner::Unknown => return Err(refused()),
            SystemOwner::Returned => {
                let id = record.system.as_ref().ok_or_else(refused)?;
                sys::open_system(id.as_str())
                    .map_err(|e| super::failure("reopening a service-created container", e))?
            }
        };
        let result = match outcome {
            Outcome::Created => Ok(()),
            Outcome::Failed { code } => Err(HcsFailure {
                code,
                message: None,
            }),
        };
        Ok(Some(TerminalCreate { system, result }))
    }
    pub(crate) fn cancel(&self) -> Result<(), Refusal> {
        notify(&self.marker)?;
        let dir = directory(&self.marker);
        let Some(_owner) = lock(&dir)? else {
            return Err(refused());
        };
        let mut receipt = Receipt::decode(&read(&dir.join(RECEIPT))?)?;
        if receipt.phase == Phase::Queued {
            receipt.cancel_queued(u32::from_ne_bytes(
                windows_sys::Win32::Foundation::E_ABORT.to_ne_bytes(),
            ))?;
            write(&dir.join(RECEIPT), &receipt.encode()?)?;
            win::set(&self.completion)
                .map_err(|e| Refusal::os("notifying native cancellation", e))?;
        }
        Ok(())
    }
}

pub(crate) fn submit(
    state: &Path,
    marker: &Path,
    document: &str,
    patience: Duration,
) -> Result<Pending, Refusal> {
    let record = super::read_marker(marker)?;
    let request = Request {
        record,
        document: crate::windows::copy_text(document)?,
    };
    let input = request.encode()?;
    let document_digest = request.digest();
    let receipt = Receipt {
        record: request.record,
        phase: Phase::Queued,
        document_digest,
        system_owner: SystemOwner::NotSubmitted,
    };
    let dir = directory(marker);
    fs::create_dir(&dir).map_err(|e| Refusal::os("claiming native create request", e))?;
    // A partial directory is retained. No native effect precedes the complete queued request, and
    // a retry cannot reuse this identity while its marker still exists.
    let _owner = lock(&dir)?.ok_or_else(refused)?;
    let completion = win::named_event(&completion_name(marker))
        .map_err(|e| Refusal::os("making native completion event", e))?;
    win::reset(&completion).map_err(|e| Refusal::os("arming native completion event", e))?;
    write(&dir.join(INPUT), &input)?;
    write(&dir.join(RECEIPT), &receipt.encode()?)?;
    drop(_owner);
    let wake = win::named_event(&wake_name(state))
        .map_err(|e| Refusal::os("opening keeper native event", e))?;
    win::set(&wake).map_err(|e| Refusal::os("requesting keeper native create", e))?;
    let millis = u32::try_from(patience.as_millis()).map_err(|_| refused())?;
    if millis == u32::MAX {
        return Err(refused());
    }
    win::wait(&completion, millis).map_err(|e| Refusal::os("waiting on native completion", e))?;
    Ok(Pending {
        marker: marker.to_path_buf(),
        completion,
    })
}

/// The callback holds this request's native file lock until after terminal receipt publication.
/// The keeper retains the same context receipt through transient publication failures; physical
/// reservations remain in the existing records/Holding authority.
pub(crate) struct TerminalOwner {
    dir: PathBuf,
    receipt: Receipt,
    _owner: File,
    completion: win::Handle,
}
pub(crate) type TerminalReceipt = triomphe::UniqueArc<TerminalOwner>;
impl TerminalOwner {
    pub(crate) fn submitted(&mut self) {
        self.receipt.system_owner = SystemOwner::Unknown;
    }
    pub(crate) fn owns_system(&mut self) {
        self.receipt.system_owner = SystemOwner::Returned;
    }
    pub(crate) fn published(&self) -> bool {
        matches!(self.receipt.phase, Phase::Terminal { .. })
    }

    pub(crate) fn complete(&mut self, result: &Result<(), HcsFailure>) -> Result<(), Refusal> {
        if matches!(self.receipt.phase, Phase::Terminal { .. }) {
            return Ok(());
        }
        let outcome = match result {
            Ok(()) => Outcome::Created,
            Err(error) => Outcome::Failed { code: error.code },
        };
        // Retain Dispatched in memory on failed disk publication; a later query/drop may retry
        // exactly this terminal fact. The durable record never claims the write succeeded.
        let previous = self.receipt.phase;
        self.receipt.complete(outcome)?;
        let published = self
            .receipt
            .encode()
            .and_then(|bytes| write(&self.dir.join(RECEIPT), &bytes));
        if published.is_err() {
            self.receipt.phase = previous;
        }
        let notified =
            win::set(&self.completion).map_err(|e| Refusal::os("notifying native completion", e));
        published.and(notified)
    }
}

/// Native operation ownership inside the existing keeper, bounded by the existing box budget.
/// This retains PendingCreate/context/File locks; it never owns or selects physical reservations.
pub(crate) struct Service {
    attempts: Vec<Attempt>,
}
enum Attempt {
    Operation(sys::PendingCreate),
    Preparation {
        owner: TerminalReceipt,
        result: Result<(), HcsFailure>,
    },
}
impl Attempt {
    fn publish(&mut self) -> Result<bool, Refusal> {
        match self {
            Self::Operation(create) => create.publish_terminal(),
            Self::Preparation { owner, result } => {
                owner.complete(result)?;
                Ok(owner.published())
            }
        }
    }
}
impl Service {
    pub(crate) fn new() -> Self {
        Self {
            attempts: Vec::new(),
        }
    }
    /// Completion/query events retry the exact retained result. No timer, HCS redispatch or fresh
    /// catalog decision is involved; a failed write retains its existing native receipt.
    pub(crate) fn dispatch(&mut self, state: &Path, owner: hcs::InitRecord) -> Result<(), Refusal> {
        let mut publication_error = None;
        self.attempts.retain_mut(|attempt| match attempt.publish() {
            Ok(true) => false,
            Ok(false) => true,
            Err(error) => {
                if publication_error.is_none() {
                    publication_error = Some(error);
                }
                true
            }
        });
        let root = state.join("boxes");
        let entries = match fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Refusal::os("reading native create requests", e)),
        };
        // Per box: scratch folder, marker, its .next, and the native request directory. Refuse a
        // damaged/overfull root rather than walking an unbounded number of unrelated files.
        let entries_bound = crate::MAX_BOXES.checked_mul(4).ok_or_else(refused)?;
        for (count, entry) in entries.enumerate() {
            if count >= entries_bound {
                return Err(Refusal::Full(crate::MAX_BOXES));
            }
            let entry = entry.map_err(|e| Refusal::os("reading native create requests", e))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(marker) = name.strip_suffix(SUFFIX) else {
                continue;
            };
            if !marker.ends_with(super::MARKER_SUFFIX) {
                continue;
            }
            let marker = root.join(marker);
            if let Err(error) = self.dispatch_one(&marker, owner) {
                // A malformed sibling request cannot stop the keeper and drop other tenants' jobs.
                // One latest diagnostic per retained request is bounded by that request's lifetime.
                let bytes = error.to_string().into_bytes();
                if write(&directory(&marker).join("refusal.txt"), &bytes).is_err() {
                    // A diagnostic is not authority to stop this keeper or another request's dispatch.
                    // The caller still receives typed uncertainty from its nonterminal receipt.
                    let _ = writeln!(
                        std::io::stderr(),
                        "native request diagnostic could not be stored; claims remain held"
                    );
                }
            }
        }
        match publication_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn dispatch_one(&mut self, marker: &Path, owner: hcs::InitRecord) -> Result<(), Refusal> {
        let dir = directory(marker);
        let Some(lock) = lock(&dir)? else {
            return Ok(());
        };
        let request = Request::decode(&read(&dir.join(INPUT))?)?;
        let mut receipt = Receipt::decode(&read(&dir.join(RECEIPT))?)?;
        let record = super::read_marker(marker)?;
        receipt.outcome_for(&record)?;
        receipt.check_request(&request)?;
        let expected = record.system.as_ref().ok_or_else(refused)?;
        let expected_name = format!("{expected}{}", super::MARKER_SUFFIX);
        if marker.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
            return Err(refused());
        }
        if receipt.phase != Phase::Queued {
            return Ok(());
        }
        if self.attempts.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        self.attempts
            .try_reserve(1)
            .map_err(|_| Refusal::Exhausted {
                what: "keeper native creates",
                bound: crate::MAX_BOXES,
            })?;
        receipt.dispatch(owner)?;
        let completion = win::named_event(&completion_name(marker))
            .map_err(|e| Refusal::os("opening native completion event", e))?;
        let terminal = triomphe::UniqueArc::try_new(TerminalOwner {
            dir,
            receipt,
            _owner: lock,
            completion,
        })
        .map_err(|_| Refusal::Exhausted {
            what: "native terminal owner",
            bound: std::mem::size_of::<TerminalOwner>(),
        })?;
        write(&terminal.dir.join(RECEIPT), &terminal.receipt.encode()?)?;
        let mut durable = Some(terminal);
        let mut attempt =
            match sys::create_system_durable(expected.as_str(), &request.document, &mut durable) {
                Ok(create) => Attempt::Operation(create),
                Err(failure) => Attempt::Preparation {
                    owner: durable.ok_or_else(refused)?,
                    result: Err(failure),
                },
            };
        // The vector slot was reserved before native dispatch. No fallible allocation can lose this
        // physical receipt after HCS starts, and the request's lock excludes redispatch on every wake.
        if !matches!(attempt.publish(), Ok(true)) {
            self.attempts.push(attempt);
        }
        Ok(())
    }
}

fn notify(marker: &Path) -> Result<(), Refusal> {
    let state = marker.parent().and_then(Path::parent).ok_or_else(refused)?;
    let event = wake(state)?;
    win::set(&event).map_err(|error| Refusal::os("waking native result publication", error))
}

pub(crate) fn wake(state: &Path) -> Result<win::Handle, Refusal> {
    win::named_event(&wake_name(state)).map_err(|e| Refusal::os("making keeper native event", e))
}

/// Reconcile only a durable terminal receipt for this marker. Missing, dispatched or malformed
/// receipts remain uncertainty, regardless of whether HCS currently lists the system.
pub(crate) fn resolve(marker: &Path, record: &mut hcs::BoxRecord) -> Result<(), Refusal> {
    if record.creation != hcs::NativeCreate::Pending {
        return Ok(());
    }
    let path = directory(marker).join(RECEIPT);
    if !path.exists() {
        return Err(refused());
    }
    let receipt = Receipt::decode(&read(&path)?)?;
    let outcome = receipt.outcome_for(record)?.ok_or_else(refused)?;
    record.creation = match outcome {
        Outcome::Created => hcs::NativeCreate::Created,
        Outcome::Failed { code } => hcs::NativeCreate::Failed {
            code,
            owner: receipt.system_owner,
        },
    };
    super::write_marker(marker, record)
}

/// Inspect only the existing committed request/receipt. No lock, event, resolve, notification,
/// native operation or write is allowed before the whole retained-owner barrier.
pub(crate) fn preflight(marker: &Path, record: &hcs::BoxRecord) -> Result<(), Refusal> {
    let dir = directory(marker);
    match fs::metadata(&dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(refused()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if record.creation == hcs::NativeCreate::Pending {
                Err(refused())
            } else {
                Ok(())
            };
        }
        Err(error) => {
            return Err(Refusal::os(
                "reading retained native request metadata",
                error,
            ));
        }
    }
    let request = Request::decode(&read(&dir.join(INPUT))?)?;
    let receipt = Receipt::decode(&read(&dir.join(RECEIPT))?)?;
    receipt.outcome_for(record)?;
    receipt.check_request(&request)?;
    if marker
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(super::MARKER_SUFFIX))
        != record.system.as_ref().map(hex_api::BoxId::as_str)
    {
        return Err(refused());
    }
    Ok(())
}

pub(crate) fn reconcile(marker: &Path, cancel: bool) -> Result<(), Refusal> {
    notify(marker)?;
    let mut record = super::read_marker(marker)?;
    if record.creation != hcs::NativeCreate::Pending || !directory(marker).exists() {
        return Ok(());
    }
    if cancel {
        let completion = win::named_event(&completion_name(marker))
            .map_err(|e| Refusal::os("opening native completion event", e))?;
        let pending = Pending {
            marker: marker.to_path_buf(),
            completion,
        };
        let _ = pending.cancel();
    }
    resolve(marker, &mut record)
}

/// Called before removing a marker. A still-active callback prevents identity recycling.
pub(crate) fn remove(marker: &Path) -> Result<(), Refusal> {
    let dir = directory(marker);
    if !dir.exists() {
        return Ok(());
    }
    let Some(owner) = lock(&dir)? else {
        return Err(refused());
    };
    let record = super::read_marker(marker)?;
    let receipt_path = dir.join(RECEIPT);
    if receipt_path.exists() {
        let receipt = Receipt::decode(&read(&receipt_path)?)?;
        if receipt.outcome_for(&record)?.is_none() {
            return Err(refused());
        }
    } else if !record.creation.terminal() {
        return Err(refused());
    }
    for name in [
        INPUT,
        RECEIPT,
        "input.next",
        "receipt.next",
        "refusal.txt",
        "refusal.next",
    ] {
        match fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Refusal::os("removing terminal native request", error)),
        }
    }
    // No further native owner can start: the marker is terminal, and remains until this returns.
    drop(owner);
    fs::remove_file(dir.join(OWNER))
        .and_then(|()| fs::remove_dir(&dir))
        .map_err(|e| Refusal::os("removing terminal native ownership", e))
}
