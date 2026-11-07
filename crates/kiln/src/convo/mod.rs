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
    for s in ["", "-wal", "-shm"] {
        anyhow::ensure!(!beside(to, s).exists(), "{} is there already", beside(to, s).display());
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(from, to).map_err(|e| anyhow::Error::from(e).context(format!("move {} to {}", from.display(), to.display())))?;
    for s in ["-wal", "-shm"] {
        match std::fs::rename(beside(from, s), beside(to, s)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::Error::from(e).context(format!("move {}", beside(from, s).display()))),
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
    let to = (1..=100u32)
        .map(|i| beside(path, &if i == 1 { SET_ASIDE.to_owned() } else { format!("{SET_ASIDE}.{i}") }))
        .find(|to| ["", "-wal", "-shm"].iter().all(|s| !beside(to, s).exists()))
        .ok_or_else(|| anyhow::anyhow!("no name beside {} is free to set it aside under", path.display()))?;
    move_file(path, &to)?;
    Ok(to)
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
    // Not even a new one: what that began goes, and the one set aside goes back where it was.
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(beside(&path, s));
    }
    if let Err(e) = move_file(&aside, &path) {
        tracing::warn!(file = %path.display(), aside = %aside.display(), error = %format!("{e:#}"), "a conversation store set aside couldn't be put back where it was; it stays set aside");
        return (None, Some(aside));
    }
    (None, None)
}
