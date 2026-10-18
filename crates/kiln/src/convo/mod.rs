//! Conversations, kept per event, replicated and searchable (`docs/conversations.md`): what each
//! agent session said and did, on disk as it happens, so it is resumed exactly after a restart,
//! resumed on another machine, and found again.
//!
//! - [`rows`]: what a session's updates become in the store (no engine in sight).
//! - [`store`]: a machine's store, its writer thread, and the tee that feeds it from the sessions.

pub mod rows;
pub mod store;
