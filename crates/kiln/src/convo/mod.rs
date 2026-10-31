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
// Historical unprefixed SQL is used only by the legacy fixture engine.
#[cfg(test)]
pub mod sql;
pub mod store;
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
    open_legacy_sources(database,None,legacy,upkeep,i64::MAX)
}

/// U19: replicated startup binds each old file to its certificate-authenticated origin.
/// Host/path identity is stable; the separately pinned fingerprint fences source replacement.
pub fn from_host_sources(database: ::amphora::database::Database, host:&str, legacy:&[std::path::PathBuf], upkeep:store::Upkeep)->anyhow::Result<store::Store> {
    from_host_sources_with_initial_hold(database,host,legacy,upkeep,i64::MAX)
}

pub(crate) fn from_host_sources_with_initial_hold(database: ::amphora::database::Database, host:&str, legacy:&[std::path::PathBuf], upkeep:store::Upkeep, initial_hold:i64)->anyhow::Result<store::Store> {
    anyhow::ensure!(!host.is_empty() && host.len()<=1024,"legacy source requires an authenticated host name of 1 to 1024 bytes");
    open_legacy_sources(database,Some(host),legacy,upkeep,initial_hold)
}

fn open_legacy_sources(database: ::amphora::database::Database, host:Option<&str>, legacy:&[std::path::PathBuf], upkeep:store::Upkeep, initial_hold:i64)->anyhow::Result<store::Store> {
    anyhow::ensure!(legacy.len()<=4096,"too many legacy conversation sources");
    let mut engine = amphora::Amphora::from_database(database)?;
    // Resume available inputs before checking missing ones: a missing optional path must not
    // prevent completion of another registered source that is still present.
    for path in legacy.iter().filter(|p|p.exists()).chain(legacy.iter().filter(|p|!p.exists())) {
        match host {Some(host)=>engine.import_legacy_from(host,path)?,None=>engine.import_legacy(path)?}
    }
    // This is deliberately a global barrier: partial projections are in one shared timeline.
    // A host must not expose another origin's unfinished projection as complete history.
    engine.finish_imports()?;
    store::Store::start_with_initial_hold(Box::new(engine), upkeep, initial_hold)
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
