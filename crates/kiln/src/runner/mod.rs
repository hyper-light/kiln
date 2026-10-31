//! The runner side of CI (docs/ci.md §2.2): the GitHub-exact step runtime (W06), actions (W07),
//! executors and facts (W13), job terminals (W17). Both assignment wire formats use the shared
//! runtime; `exec.rs` only adapts the original server's endpoints and events.

pub mod artifacts;
pub mod attest;
pub mod auth;
pub mod builtins;
pub mod cache;
pub mod chaos;
pub mod cpus;
pub mod credential;
pub mod docker;
pub mod docker_sock;
pub mod pool;
pub mod egress;
#[cfg(target_os = "linux")]
pub mod firecracker;
#[cfg(target_os = "linux")]
pub mod guest;
pub mod hashfiles;
pub mod identity;
#[cfg(target_os = "linux")]
pub mod jail;
pub mod kept;
pub mod observe;
pub mod probe;
pub mod processes;
#[cfg(target_os = "linux")]
pub mod rootfs;
pub mod rustc;
pub mod runtime;
pub mod setup;
pub mod warm;
pub mod work;
pub mod workspaces;
