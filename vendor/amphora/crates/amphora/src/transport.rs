//! Narrow embedded transport for one canonical database owner (CP-T1.5, U05/U06).
//! No retry replays application closures or submitted writes. Every platform uses a protected
//! owner record and a local listener; Windows additionally requires an exclusive address bind.
mod auth;
mod bounded;
mod common;
mod endpoint;
mod watch;
mod wire;
pub use common::{Client, Namespace, Owner};
pub(crate) use endpoint::try_lease;
pub use watch::Watch;

/// The transport tests' hang guard, measured: twenty times the slowest whole run of any transport
/// test, 5,856 ms over 200 runs of each at loads 19 to 39 (adacachy, 2026-10-31). No test's verdict
/// rests on it (local-owner-transport.md, "The tests' budgets").
#[cfg(test)]
const HANG: std::time::Duration = std::time::Duration::from_secs(120);

/// The peer disappeared after submission: durability may already have completed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Refused(#[from] amphora_api::Refusal),
    #[error("owner connection lost after submission; transaction outcome is unknown")]
    UnknownOutcome,
}

impl From<Error> for amphora_api::Refusal {
    fn from(error: Error) -> Self {
        match error {
            Error::Refused(refusal) => refusal,
            Error::UnknownOutcome => Self::UnknownOutcome,
        }
    }
}
