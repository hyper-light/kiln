//! Conversations, kept per event, replicated and searchable (`docs/conversations.md`): what each
//! agent session said and did, on disk as it happens, so it is resumed exactly after a restart,
//! resumed on another machine, and found again.
//!
//! - [`rows`]: what a session's updates become in the store (no engine in sight).
//! - [`store`]: a machine's store, its writer thread, and the tee that feeds it from the sessions.
//! - [`turso`]: the engine: Turso, embedded.
//! - [`record`]: the harness's own record (transcripts, rollouts), followed into the store.
//! - [`feed`]: a runner's store sent to the server's, over the runner's link.

pub mod feed;
pub mod record;
pub mod rows;
pub mod sql;
pub mod store;
pub mod turso;

/// The store file in a machine's state folder.
pub const FILE: &str = "conversations.db";

/// This machine's conversation store in `dir`, its writer started: None when it is turned off
/// (`KILN_CONVERSATIONS=off`) or can't be opened, and the machine runs as it did without one.
pub fn open(dir: &std::path::Path) -> Option<store::Store> {
    if std::env::var("KILN_CONVERSATIONS").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no")) {
        tracing::info!("the conversation store is off (KILN_CONVERSATIONS)");
        return None;
    }
    let path = dir.join(FILE);
    let opened = turso::Turso::open(&path).and_then(|engine| store::Store::start(Box::new(engine)));
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
