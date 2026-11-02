//! Workflow dependency locks (docs/ci.md W29-R23 to R26). Format and pin decisions are pure;
//! the server supplies source identity, resolution and reachability facts.

pub mod github;
pub mod execution;
pub mod dockerfile;
pub mod kiln;
pub mod regenerate;
pub mod replay;
pub mod verify;
pub mod verify_kiln;
