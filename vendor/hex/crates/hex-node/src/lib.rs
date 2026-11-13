//! Hex's node agent ("hexlet", CP-T7.1): it drives a runtime class on one node, keeps a record of
//! every box that its own crash leaves whole ([`store`]), and after a restart takes each box back or
//! cleans it up so none is leaked ([`recover`], T-T7.1.2). It serves the seam in process, and over a
//! local socket to callers that don't link Hex ([`serve`], decision 3). The scheduling and control
//! plane that drive many nodes come in CP-T7.3; this is the node by itself.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

mod abandoned;
pub mod census;
pub mod disk_probe;
mod emptying;
pub mod facts;
pub mod placement;
pub mod quic;
pub mod recorded;
pub mod records;
pub mod store;
mod turns;

#[cfg(target_os = "linux")]
pub mod cgroups;
#[cfg(target_os = "linux")]
mod createpool;
#[cfg(target_os = "linux")]
mod devfs;
#[cfg(target_os = "linux")]
mod ipcuts;
#[cfg(target_os = "linux")]
pub mod netns;
#[cfg(target_os = "linux")]
mod node;
#[cfg(target_os = "linux")]
pub mod relay;
#[cfg(target_os = "linux")]
mod remove;
#[cfg(target_os = "linux")]
mod seed;
pub mod serve;
#[cfg(target_os = "linux")]
pub mod sizes;
#[cfg(target_os = "linux")]
pub mod supervise;

#[cfg(all(target_os = "linux", debug_assertions))]
pub use node::CrashPoint;
#[cfg(target_os = "linux")]
pub use node::{Node, OCI_BOXES, Runs, records_dir};
pub use recorded::RecoverySummary;

/// The live file-backed scheduler socket and authenticated peer shell.
pub mod scheduler;
