//! The keeper: a passive process a node starts to hold its boxes' job handles, so a box outlives the node
//! (native plan step 8, the decision "the Windows node, on one completion port", its recovery part; the
//! lead's (A), 2026-11-08). Each box's job stays kill-on-close; the node and the keeper each hold a handle
//! to it, so:
//! - the node crashes: the keeper's handle keeps the job and its name, and the next node takes the box
//!   back by name;
//! - the keeper dies while the node lives: the node sees its end and starts another, duplicating the
//!   handles again;
//! - both are gone: the last handle closes, and kill-on-close ends the boxes, so nothing is left running
//!   in a job no one can open.
//!
//! The keeper does nothing itself: the node duplicates job handles into it (`sys::duplicate_into`) and
//! closes them there on delete (`sys::close_in`). It holds them, with no timer, until its stop is asked. It
//! is `hex keep`, started by the node ([`super::WinRuntime`]); this module is the keeper's body and the
//! node's handle on it.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use hex_api::Refusal;

use super::sys::{self, Handle};

/// What `hex keep` is given: the folder its node keeps its state in, and the path it writes its own
/// identity to once ready.
pub struct Args {
    pub state: PathBuf,
    pub ready: PathBuf,
}

impl Args {
    /// Parses `hex keep`'s arguments: `--state <dir> --ready <path>`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let (mut state, mut ready) = (None, None);
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--state" => state = Some(PathBuf::from(value)),
                "--ready" => ready = Some(PathBuf::from(value)),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        let env_path = |flag: &str, var: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{flag} is needed"))
        };
        Ok(Self {
            state: state.map_or_else(|| env_path("--state", "HEX_KEEP_STATE"), Ok)?,
            ready: ready.map_or_else(|| env_path("--ready", "HEX_KEEP_READY"), Ok)?,
        })
    }
}

/// Runs the keeper: it holds whatever the node duplicates into it and waits, without a timer, until its
/// stop is asked (a node going down cleanly, or being replaced). It never touches the handles itself, and
/// never exits on its own: a node that never returns leaves its boxes running, as a box on Linux waits for
/// its node with no timer. When the keeper is gone (stopped, or killed with the node gone too), each box's
/// job has no handle left and kill-on-close ends it.
pub fn run(args: &Args) -> Result<(), Refusal> {
    let stop = sys::named_event(&stop_name(&args.state))
        .map_err(|e| Refusal::os("making the keeper's stop", e))?;
    let me = sys::open_process(std::process::id())
        .map_err(|e| Refusal::os("reading the keeper's own identity", e))?
        .ok_or_else(|| Refusal::Broken("the keeper can't open itself".to_owned()))?;
    let created =
        sys::created(&me).map_err(|e| Refusal::os("reading the keeper's start time", e))?;
    write_ready(&args.ready, std::process::id(), created)
        .map_err(|e| Refusal::os("saying the keeper is ready", e))?;
    let wake = super::oci::service::wake(&args.state)?;
    let owner = hex_core::hcs::InitRecord {
        pid: std::process::id(),
        created,
    };
    let mut native = super::oci::service::Service::new();
    loop {
        // Reset before the scan: a request arriving during the scan retains its signal. Startup
        // also drains requests a node committed before it could signal the keeper.
        sys::reset(&wake).map_err(|e| Refusal::os("arming the keeper's native wake", e))?;
        if let Err(error) = native.dispatch(&args.state, owner) {
            // Preserve the keeper's existing job ownership on a service/storage refusal. This is
            // a latest-error diagnostic, not a retry queue or a claim; the next request wakes us.
            if std::fs::write(args.state.join("native-create.refusal"), error.to_string()).is_err()
            {
                // The requester also gets typed uncertainty when its receipt cannot be read.
                // Diagnostic storage loss must not close unrelated boxes' keeper handles.
                let _ = writeln!(
                    std::io::stderr(),
                    "native create failed and its diagnostic could not be stored; claims remain held"
                );
            }
        }
        if sys::wait_either(&stop, &wake)
            .map_err(|e| Refusal::os("waiting on keeper work or stop", e))?
        {
            return Ok(());
        }
    }
}

/// The keeper's stop event's name, one per state folder (`Local\hex-keep-<hash of path>-stop`): the node
/// and its keeper reach the same event by the folder they share.
fn stop_name(state: &Path) -> Vec<u16> {
    format!("Local\\hex-keep-{:016x}-stop\0", path_hash(state))
        .encode_utf16()
        .collect()
}

/// A name for the node keeping its state in `dir`, apart from another node's in the session: a hash of the
/// folder, so the same node (the same state folder) names the same box jobs across its restarts, and hex,
/// so it is safe in a job object's name (`Local\hex-<node>-<box>`, [`super::WinRuntime::job_name`]).
pub fn node_name(dir: &Path) -> String {
    format!("{:016x}", path_hash(dir))
}

/// FNV-1a of a path's bytes, for the names a node and its keeper derive from the folder they share.
fn path_hash(p: &Path) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for b in p.to_string_lossy().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(1099511628211);
    }
    h
}

fn write_ready(path: &Path, pid: u32, created: u64) -> io::Result<()> {
    std::fs::write(path, format!("{pid} {created}"))
}

/// The node's handle on the keeper it started: its process, for watching and duplicating handles into, and
/// the stop it signals.
pub struct NodeKeeper {
    process: Handle,
    pid: u32,
    created: u64,
    stop: Handle,
}

impl NodeKeeper {
    /// The keeper process `pid` (its creation time `created`), opened for watching and duplicating into;
    /// its stop is the one named for `state`.
    pub fn open(pid: u32, created: u64, state: &Path) -> Result<Self, Refusal> {
        let process = sys::open_process(pid)
            .map_err(|e| Refusal::os("opening the node's keeper", e))?
            .filter(|p| sys::created(p).ok() == Some(created))
            .ok_or_else(|| Refusal::Invalid {
                what: "keeper",
                why: format!("keeper pid {pid} is gone or reused"),
            })?;
        let stop = sys::named_event(&stop_name(state))
            .map_err(|e| Refusal::os("opening the keeper's stop", e))?;
        Ok(Self {
            process,
            pid,
            created,
            stop,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn created(&self) -> u64 {
        self.created
    }

    /// Whether the keeper still runs (its handle unsignalled).
    pub fn alive(&self) -> bool {
        sys::wait(&self.process, 0)
            .map(|ended| !ended)
            .unwrap_or(false)
    }

    /// The keeper's process handle, to watch on the node's port for its end.
    pub fn process(&self) -> &Handle {
        &self.process
    }

    /// Duplicates the handle value `job` (a box's job, this process's) into the keeper, returning its
    /// value there.
    pub fn hold(&self, job: usize) -> Result<usize, Refusal> {
        sys::duplicate_into(job, &self.process)
            .map_err(|e| Refusal::os("handing a box's job to the keeper", e))
    }

    /// Closes the handle `value` in the keeper (a deleted box's job).
    pub fn drop_held(&self, value: usize) -> Result<(), Refusal> {
        sys::close_in(&self.process, value)
            .map_err(|e| Refusal::os("closing a box's job in the keeper", e))
    }

    /// Tells the keeper to stop and end (the node is going down cleanly, or replacing it).
    pub fn stop(&self) {
        let _ = sys::set(&self.stop);
    }
}

/// The file naming the keeper holding a node's boxes, under the state folder: its pid and creation time, so
/// a node after a restart finds the same keeper (or starts a new one where it is gone).
pub fn keeper_file(state: &Path) -> PathBuf {
    state.join("keeper.owner")
}

/// The file holding box `id`'s job inside the keeper: which keeper holds it (its pid and creation time) and
/// the handle's value there, so a delete or a restart can close or re-duplicate it. The keeper is named
/// because a handle value means nothing in another keeper: a node that finds a value recorded against a
/// keeper that isn't its current one must re-duplicate the job into the one it has ([`super::WinRuntime`]).
pub fn held_file(state: &Path, id: &str) -> PathBuf {
    state.join(format!("held-{id}"))
}

/// Box `id`'s job in the keeper: which keeper holds it, and its handle value there.
pub struct Held {
    pub keeper_pid: u32,
    pub keeper_created: u64,
    pub value: usize,
}

impl Held {
    /// Whether the keeper of pid `pid` and creation time `created` is the one holding this value.
    pub fn held_by(&self, pid: u32, created: u64) -> bool {
        self.keeper_pid == pid && self.keeper_created == created
    }
}

/// Records box `id`'s job held by the keeper `keeper_pid` (creation time `keeper_created`) at handle
/// `value` (`held_file`).
pub fn write_held(
    state: &Path,
    id: &str,
    keeper_pid: u32,
    keeper_created: u64,
    value: usize,
) -> io::Result<()> {
    std::fs::write(
        held_file(state, id),
        format!("{keeper_pid} {keeper_created} {value}"),
    )
}

/// Box `id`'s recorded keeper handle, if any: which keeper holds it and its value there.
pub fn read_held(state: &Path, id: &str) -> Option<Held> {
    let text = std::fs::read_to_string(held_file(state, id)).ok()?;
    parse_held(&text, false)
}

fn parse_held(text: &str, strict: bool) -> Option<Held> {
    let mut p = text.split_whitespace();
    let held = Held {
        keeper_pid: p.next()?.parse().ok()?,
        keeper_created: p.next()?.parse().ok()?,
        value: p.next()?.parse().ok()?,
    };
    if strict
        && (held.keeper_pid == 0
            || held.keeper_created == 0
            || held.value == 0
            || p.next().is_some())
    {
        return None;
    }
    Some(held)
}

/// Same receipt parser, retaining missing/read/malformed as distinct outcomes for startup.
pub(super) fn read_held_strict(state: &Path, id: &hex_api::BoxId) -> Result<Option<Held>, Refusal> {
    // A decimal unsigned integer has no more digits than its bit width, plus two separators.
    let bound = u32::BITS
        .checked_add(u64::BITS)
        .and_then(|bits| bits.checked_add(usize::BITS))
        .and_then(|bits| bits.checked_add(2))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| super::allocation(usize::MAX))?;
    let Some(bytes) = read_native_bytes(
        &vm::path(state, hex_core::record::OwnerRef::Box(id))?,
        bound,
    )?
    else {
        return Ok(None);
    };
    let held = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| parse_held(text, true))
        .ok_or_else(|| receipt_invalid("a legacy keeper receipt is malformed"))?;
    Ok(Some(held))
}

/// Bounded fallible storage shared by legacy startup validation and the committed VM receipt.
pub(super) fn read_native_bytes(path: &Path, bound: usize) -> Result<Option<Vec<u8>>, Refusal> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Refusal::os("opening a native keeper receipt", error)),
    };
    let room = bound
        .checked_add(1)
        .ok_or_else(|| super::allocation(bound))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| super::allocation(room))?;
    bytes.resize(room, 0);
    let mut file = file.take(u64::try_from(room).map_err(|_| super::allocation(room))?);
    let mut used = 0usize;
    while used < room {
        let tail = bytes
            .get_mut(used..)
            .ok_or_else(|| super::allocation(room))?;
        let count = file
            .read(tail)
            .map_err(|error| Refusal::os("reading a native keeper receipt", error))?;
        if count == 0 {
            break;
        }
        used = used
            .checked_add(count)
            .ok_or_else(|| super::allocation(room))?;
    }
    if used > bound {
        return Err(receipt_invalid(
            "a native keeper receipt exceeds its field bound",
        ));
    }
    bytes.truncate(used);
    Ok(Some(bytes))
}

fn receipt_invalid(message: &'static str) -> Refusal {
    match super::copy_text(message) {
        Ok(why) => Refusal::Invalid {
            what: "native keeper receipt",
            why,
        },
        Err(error) => error,
    }
}

/// Forgets box `id`'s keeper handle value (its job closed in the keeper).
pub fn forget_held(state: &Path, id: &str) {
    let _ = std::fs::remove_file(held_file(state, id));
}
pub(super) mod vm;
