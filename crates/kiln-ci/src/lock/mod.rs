//! Workflow dependency locks (docs/ci.md W29-R23 to R26). Format and pin decisions are pure;
//! the server supplies source identity, resolution and reachability facts.

pub mod github;
pub mod kiln;
pub mod regenerate;
pub mod verify;
