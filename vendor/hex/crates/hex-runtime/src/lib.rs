//! Hex's runtime classes (docs/design/runtime.md): the `process` class, a resource box holding host
//! processes (cgroup v2 on Linux), and the `oci` class, an OCI runtime-spec container run by Hex's
//! own runtime (Linux). `hex-node` holds the records and drives these; they keep no state of their
//! own beyond what the kernel holds.

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

/// Most boxes one node holds; past it `create` is refused with [`hex_api::Refusal::Full`]. The kernel's own
/// default bound on descendants is unlimited; this is Hex's, well above a node's useful load. Here, for every
/// platform, so a bound that is one a box (the node's record turns) names it wherever it compiles.
pub const MAX_BOXES: usize = 65_536;

pub mod devices;
pub mod firecracker;
pub mod grants;
pub mod guest;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod run;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub mod elsewhere;
