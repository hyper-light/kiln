//! Conversations, kept per event, replicated and searchable (`docs/conversations.md`): what each
//! agent session said and did, on disk as it happens, so it is resumed exactly after a restart,
//! resumed on another machine, and found again.
//!
//! - [`rows`]: what a session's updates become in the store (no engine in sight).
//! - [`store`]: a machine's store, its writer thread, and the tee that feeds it from the sessions.
//! - [`amphora`]: SQL and segmented transcripts in the shared host Amphora database.
//! - [`record`]: the harness's own record (transcripts, rollouts), followed into the store.
//! - [`feed`]: append-only wire compatibility; active replication belongs to Amphora.
//! - [`resume`]: a session packed from the server's store, to go on on another machine.
//! - [`telemetry`]: what each store and feed is doing, for the System page and its dashboards.
//! - [`board`]: the server's board of every store: their state, series and what happened lately.

pub mod amphora;
pub mod batch;
mod amphora_sql;
pub mod board;
// U15: production and tests share the bounded compatibility reader. Replication belongs to Amphora.
#[path = "feed_compat.rs"]
pub mod feed;
pub mod record;
pub mod resume;
pub mod rows;
mod segments;
pub(crate) const TRANSCRIPT_LINE_MAX: usize = segments::LINE_MAX;
// Historical unprefixed SQL is used only by the legacy fixture engine.
#[cfg(test)]
pub mod sql;
pub mod store;
pub(crate) mod retention;
pub mod telemetry;
// U15: retained only to construct/read compatibility fixtures. Production never opens it.
#[cfg(test)]
pub mod turso;

/// The store file in a machine's state folder.
pub const FILE: &str = "conversations.db";
/// Retired mirror file name, retained only to identify/refuse the old feature store.
pub const MIRROR_FILE: &str = "mirror.db";

/// U15: the existing process policy applies to canonical and compatibility boot alike.
/// Session control Records and Host restoration remain enabled when conversations are off.
pub fn enabled() -> bool {
    enabled_value(std::env::var("KILN_CONVERSATIONS").ok().as_deref())
}

fn enabled_value(value: Option<&str>) -> bool {
    !value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

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
    if !enabled() {
        tracing::info!("the conversation store is off (KILN_CONVERSATIONS)");
        return None;
    }
    if file != FILE {
        tracing::warn!(%file, "U15: conversation mirror stores are retired; use Amphora replication");
        return None;
    }
    let path = dir.join(file);
    let opened = crate::store::Store::open(&dir.join("kiln.db")).and_then(|host| from_database(host.database(), &path, upkeep));
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

/// U06/U15: singleton compatibility constructor. Replicated callers use `from_host_sources`
/// so independent authenticated origins with the same path never share an import receipt.
pub fn from_database(database: ::amphora::database::Database, legacy: &std::path::Path, upkeep: store::Upkeep) -> anyhow::Result<store::Store> {
    from_legacy_sources(database, &[legacy.to_path_buf()], upkeep)
}

/// U06/U15/U19: singleton compatibility import of a complete local source inventory.
/// Sources share canonical IDs but retain independent metadata, revisions and feed history.
pub fn from_legacy_sources(database: ::amphora::database::Database, legacy: &[std::path::PathBuf], upkeep: store::Upkeep) -> anyhow::Result<store::Store> {
    open_legacy_sources(database,None,legacy,upkeep,i64::MAX,None)
}

/// U19: replicated startup binds each old file to its certificate-authenticated origin.
/// Host/path identity is stable; the separately pinned fingerprint fences source replacement.
pub fn from_host_sources(database: ::amphora::database::Database, host:&str, legacy:&[std::path::PathBuf], upkeep:store::Upkeep)->anyhow::Result<store::Store> {
    anyhow::ensure!(!host.is_empty() && host.len()<=1024,"legacy source requires an authenticated host name of 1 to 1024 bytes");
    open_legacy_sources(database,Some(host),legacy,upkeep,i64::MAX,None)
}

pub(crate) fn from_host_sources_for_archive(database: ::amphora::database::Database, host:&str, legacy:&[std::path::PathBuf], upkeep:store::Upkeep, policy:retention::Policy)->anyhow::Result<store::Store> {
    anyhow::ensure!(!host.is_empty() && host.len()<=1024,"legacy source requires an authenticated host name of 1 to 1024 bytes");
    open_legacy_sources(database,Some(host),legacy,upkeep,i64::MAX,Some(policy))
}

fn open_legacy_sources(database: ::amphora::database::Database, host:Option<&str>, legacy:&[std::path::PathBuf], mut upkeep:store::Upkeep, initial_hold:i64, archive:Option<retention::Policy>)->anyhow::Result<store::Store> {
    anyhow::ensure!(legacy.len()<=4096,"too many legacy conversation sources");
    let mut engine = amphora::Amphora::from_database(database.clone())?;
    // Resume available inputs before checking missing ones: a missing optional path must not
    // prevent completion of another registered source that is still present.
    for path in legacy.iter().filter(|p|p.exists()).chain(legacy.iter().filter(|p|!p.exists())) {
        match host {Some(host)=>engine.import_legacy_from(host,path)?,None=>engine.import_legacy(path)?}
    }
    // This is deliberately a global barrier: partial projections are in one shared timeline.
    // A host must not expose another origin's unfinished projection as complete history.
    engine.finish_imports()?;
    let authority=match archive {
        Some(policy)=>retention::register(engine.host(),host.ok_or_else(||anyhow::anyhow!("archive startup requires its trusted host identity"))?,policy)?,
        None=>None,
    };
    engine.maintenance(authority.clone())?;
    if authority.is_none() { upkeep.keep_closed=None; }
    store::Store::start_with_maintenance(Box::new(engine), upkeep, initial_hold, authority)
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

/// The store in file `file` of `dir`, as [`open_file`] opens it, tried `tries` times `between`
/// apart (a file another process hasn't quite let go of, a slow disk). One that still won't open is
/// set aside ([`set_aside`]) once a new store opens in its place: what it holds is kept, beside it,
/// and the machine goes on keeping its conversations. When not even a new one opens, it stays where
/// it was, for a later start to try. The store (None: none opened, or the store is off), and where
/// the one set aside went. Blocking.
pub fn open_or_set_aside(dir: &std::path::Path, file: &str, upkeep: store::Upkeep, tries: usize, between: std::time::Duration) -> (Option<store::Store>, Option<std::path::PathBuf>) {
    for i in 0..tries.max(1) {
        if i > 0 {
            std::thread::sleep(between);
        }
        if let Some(st) = open_file(dir, file, upkeep) {
            return (Some(st), None);
        }
        if off() {
            return (None, None);
        }
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

#[cfg(test)]
mod tests {
    #[test]
    fn u15_conversation_process_policy_preserves_off_aliases_and_default() {
        for value in ["off", "0", "false", "no", " OFF ", " FaLsE\t"] {
            assert!(!super::enabled_value(Some(value)), "{value:?}");
        }
        for value in [None, Some(""), Some("on"), Some("1"), Some("true"), Some("yes"), Some("other")] {
            assert!(super::enabled_value(value), "{value:?}");
        }
    }
}
