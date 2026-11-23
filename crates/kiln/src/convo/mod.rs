//! Conversations, kept per event, replicated and searchable (`docs/conversations.md`): what each
//! agent session said and did, on disk as it happens, so it is resumed exactly after a restart,
//! resumed on another machine, and found again.
//!
//! - [`rows`]: what a session's updates become in the store (no engine in sight).
//! - [`store`]: a machine's store, its writer thread, and the tee that feeds it from the sessions.
//! - [`turso`]: the engine: Turso, embedded.
//! - [`record`]: the harness's own record (transcripts, rollouts), followed into the store.
//! - [`feed`]: a runner's store sent to the server's, over the runner's link.
//! - [`resume`]: a session packed from the server's store, to go on on another machine.
//! - [`mirror`]: runners that keep a copy of the server's store.
//! - [`telemetry`]: what each store and feed is doing, for the System page and its dashboards.
//! - [`board`]: the server's board of every store: their state, series and what happened lately.

pub mod board;
pub mod feed;
mod file_move;
pub mod inbox;
pub mod mirror;
mod custody;
pub(crate) mod liveness;
pub mod raft;
pub mod record;
pub mod resume;
pub mod rows;
pub mod sql;
pub mod store;
pub mod telemetry;
pub mod turso;

/// The store file in a machine's state folder.
pub const FILE: &str = "conversations.db";
/// A mirror's copy of the server's store, beside a runner's own (`mirror`).
pub const MIRROR_FILE: &str = "mirror.db";

/// This machine's conversation store in `dir`, its writer started: None when it is turned off
/// (`KILN_CONVERSATIONS=off`) or can't be opened, and the machine runs as it did without one.
/// `keep_closed`: how long the records of sessions that ended are kept (None: for good).
pub fn open(dir: &std::path::Path, upkeep: store::Upkeep) -> Option<store::Store> {
    open_file(dir, FILE, upkeep)
}

/// Whether conversation stores are turned off on this machine (`KILN_CONVERSATIONS=off`).
pub fn off() -> bool {
    std::env::var("KILN_CONVERSATIONS").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

/// The store kept in file `file` of `dir` (see [`open`]).
pub fn open_file(dir: &std::path::Path, file: &str, upkeep: store::Upkeep) -> Option<store::Store> {
    if off() {
        tracing::info!("the conversation store is off (KILN_CONVERSATIONS)");
        return None;
    }
    let path = dir.join(file);
    let opened = turso::Turso::open(&path).and_then(|engine| store::Store::start(Box::new(engine), upkeep));
    match opened {
        Ok(s) => {
            tracing::info!(file = %path.display(), "conversations are kept in the store");
            Some(s)
        }
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "the conversation store couldn't be opened; conversations aren't kept");
            None
        }
    }
}

pub use file_move::{move_file, recover_file_move};

/// What a store's file set aside is called, after its own name ([`set_aside`]): the first such.
pub const SET_ASIDE: &str = ".unopened";

/// A file beside `path`: its name with `suffix` added (a store's log is its name and `-wal`).
fn beside(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Set the store's file at `path` aside, with its log, kept whole for a person to look at: moved
/// beside it as `<file>.unopened` (`.unopened.2` and on, if that's taken), its log by the same name
/// and `-wal`, so that it still opens as it is there; never over a file. Where it went.
pub fn set_aside(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let mut n = 1u64;
    loop {
        let suffix = if n == 1 { SET_ASIDE.to_owned() } else { format!("{SET_ASIDE}.{n}") };
        let to = beside(path, &suffix);
        let mut occupied = false;
        for side in ["", "-wal", "-shm", ".handoff.json"] {
            match std::fs::symlink_metadata(beside(&to, side)) {
                Ok(_) => {
                    occupied = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !occupied {
            file_move::move_aside(path, &to)?;
            return Ok(to);
        }
        n = n.checked_add(1).ok_or_else(|| anyhow::anyhow!("no number for another store set aside"))?;
    }
}

/// The store in file `file` of `dir`, as [`open_file`] opens it. One that won't open is set aside
/// ([`set_aside`]) once a new store opens in its place: what it holds is kept, beside it, and the
/// machine goes on keeping its conversations. When not even a new one opens, it stays where it was,
/// for a later start to try. The store (None: none opened, or the store is off), and where the one
/// set aside went. Blocking. Tried once: a store handed over is let go of before it's handed
/// ([`store::Store::close`] answers once its files are closed), so no other process holds it as it
/// opens; it was tried three times, a second apart, for one not quite let go of, and a file that
/// doesn't open as a store opens no better later.
pub fn open_or_set_aside(dir: &std::path::Path, file: &str, upkeep: store::Upkeep) -> (Option<store::Store>, Option<std::path::PathBuf>) {
    if let Some(st) = open_file(dir, file, upkeep) {
        return (Some(st), None);
    }
    if off() {
        return (None, None);
    }
    let path = dir.join(file);
    if !path.exists() {
        return (None, None);
    }
    let aside = match set_aside(&path) {
        Ok(aside) => aside,
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "a conversation store that wouldn't open couldn't be set aside");
            return (None, None);
        }
    };
    if let Some(st) = open_file(dir, file, upkeep) {
        tracing::warn!(file = %path.display(), aside = %aside.display(), "a conversation store that wouldn't open is set aside, kept whole; a new one takes its place");
        return (Some(st), Some(aside));
    }
    // A failed replacement can still have created a file. Preserve it before restoring
    // the original, and let the move's exclusive custody check protect any concurrent open.
    if path.exists()
        && let Err(e) = set_aside(&path)
    {
        tracing::warn!(file=%path.display(), error=%e, "could not preserve the failed replacement; the original stays set aside");
        return (None, Some(aside));
    }
    if let Err(e) = move_file(&aside, &path) {
        tracing::warn!(file = %path.display(), aside = %aside.display(), error = %format!("{e:#}"), "a conversation store set aside couldn't be put back where it was; it stays set aside");
        return (None, Some(aside));
    }
    (None, None)
}
