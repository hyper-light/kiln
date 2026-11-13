//! The Linux runtime: cgroup v2 boxes (`cgroup`), processes watched through pidfds (`proc`), the
//! disks under a path (`disks`), and the few system calls that need `unsafe` (`sys`, named in
//! CLAUDE.md §12).

pub mod cgroup;
pub mod disks;
pub mod oci;
pub mod proc;
pub mod process;
pub mod sys;

use hex_api::Refusal;

/// An I/O error from `op` on `path`, as a typed refusal.
pub(crate) fn os_err(
    op: &'static str,
    path: &std::path::Path,
    e: impl std::fmt::Display,
) -> Refusal {
    Refusal::Os {
        op,
        why: format!("{}: {e}", path.display()),
    }
}

/// The descriptor limit a box's program gets where its spec names none: Docker 29's, capped at this
/// node's own hard limit, which an unprivileged process can't raise ([`hex_core::limits`]).
pub(crate) fn default_nofile() -> rustix::process::Rlimit {
    let node = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let d = hex_core::limits::default_nofile(node.maximum);
    rustix::process::Rlimit {
        current: Some(d.soft),
        maximum: Some(d.hard),
    }
}
