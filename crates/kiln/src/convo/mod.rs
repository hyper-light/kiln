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
pub mod mirror;
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

/// Move a store's file at `from`, once let go of ([`store::Store::close`]), to `to`, with its log
/// beside it: for the process that goes on with it there (a new build taking over from the runner
/// being upgraded keeps that runner's store, `crate::bluegreen`). Never over a file at `to`.
pub fn move_file(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    let side = |p: &std::path::Path, s: &str| {
        let mut name = p.as_os_str().to_owned();
        name.push(s);
        std::path::PathBuf::from(name)
    };
    for s in ["", "-wal", "-shm"] {
        anyhow::ensure!(!side(to, s).exists(), "{} is there already", side(to, s).display());
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(from, to).map_err(|e| anyhow::Error::from(e).context(format!("move {} to {}", from.display(), to.display())))?;
    for s in ["-wal", "-shm"] {
        match std::fs::rename(side(from, s), side(to, s)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::from(e).context(format!("move {}", side(from, s).display()))),
        }
    }
    // The renames themselves on disk (not on Windows, which opens no folder).
    if let Some(dir) = to.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}
