//! The `oci` class (CP-T7.1): an OCI Runtime Specification container, run by Hex's own runtime (the
//! decision, with its evidence, is docs/design/runtime.md). The lifecycle is the runtime-spec's —
//! `create`, `start`, `state`, `kill`, `delete` — so the opencontainers/runtime-tools validation
//! suite drives it unchanged (AC-T7.1.4).
//!
//! Layering, sans-IO core first (CLAUDE.md §9): [`spec`] parses `config.json`, [`state`] is the
//! on-disk container state, [`mount`] turns a spec's mounts and paths into an ordered plan of
//! operations, [`resources`] turns `linux.resources` into cgroup v2 writes and a device program,
//! [`seccomp`] compiles `linux.seccomp` into a BPF program the init loads before `execve`, [`lsm`]
//! applies `linux.mountLabel` and `process.apparmorProfile` or refuses them by name, and
//! [`devshare`] lets containers with one device policy share one loaded device program. The
//! imperative shell that makes the cgroup and namespaces, pivots the root and runs the init is in
//! [`run`] and [`init`], which resolves every path in the container's root inside it ([`inroot`])
//! and brings up the loopback of a network namespace it makes ([`net`]); a node agent's device
//! programs, held open one per policy, are [`programs`]; the system calls that need `unsafe` are in
//! [`sys`].

pub mod caps;
pub mod checked;
pub mod devmount;
pub mod devshare;
pub mod escape;
pub mod exec;
pub mod hooks;
pub mod init;
pub mod inroot;
pub mod ipcuts;
pub mod lsm;
pub mod mntns;
pub mod mount;
pub mod net;
pub mod netcheck;
pub mod procmount;
pub mod programs;
pub mod resources;
pub mod run;
pub mod seccomp;
pub mod seed;
pub mod spec;
pub mod state;
pub mod sys;

use hex_api::Refusal;

/// Why an OCI runtime operation was refused. OCI runtimes report failures on stderr with a non-zero
/// exit; this is Hex's typed form, mapped to that at the CLI edge.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OciError {
    #[error("config: {0}")]
    Config(String),
    /// A program its box's exec bound refuses (HEX-RT-14).
    #[error("program: {0}")]
    Program(String),
    #[error("no container {0}")]
    NoSuchContainer(String),
    #[error("container {0} exists already")]
    Exists(String),
    #[error("container {0} is {1}, not {2}")]
    WrongState(String, &'static str, &'static str),
    #[error("{op} failed: {why}")]
    Os { op: &'static str, why: String },
    #[error("the runtime's state is unusable: {0}")]
    Broken(String),
    /// No create seed came free for the create, its line at the seeds' `bound` (full, or its wait ended), and
    /// nothing of it was made: refused, never sent another way (one implementation per job, the lead's ruling of
    /// 2026-11-06). `why` is the line's own account.
    #[error("every create seed is taken: the node holds its most ({bound}); {why}")]
    SeedsExhausted { bound: usize, why: String },
}

impl OciError {
    pub fn os(op: &'static str, err: impl std::fmt::Display) -> Self {
        Self::Os {
            op,
            why: err.to_string(),
        }
    }
}

/// A system call that failed while a process became the container's: the step and its errno, as plain data.
/// The child of a vfork-style clone shares its parent's memory and may allocate nothing, not even to say
/// why it failed, so the steps a clone's child takes return this, and a caller that may allocate turns it
/// into an [`OciError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub what: &'static str,
    pub errno: rustix::io::Errno,
}

impl Failure {
    /// A closure that names `what` failed with the errno it is given.
    pub fn of(what: &'static str) -> impl Fn(rustix::io::Errno) -> Self {
        move |errno| Self { what, errno }
    }
}

impl From<Failure> for OciError {
    fn from(f: Failure) -> Self {
        OciError::os(f.what, f.errno)
    }
}

impl From<OciError> for Refusal {
    fn from(e: OciError) -> Refusal {
        match e {
            OciError::Config(why) => Refusal::Invalid {
                what: "oci config",
                why,
            },
            OciError::Program(why) => Refusal::Invalid {
                what: "program",
                why,
            },
            OciError::NoSuchContainer(id) => Refusal::Os {
                op: "oci",
                why: format!("no container {id}"),
            },
            OciError::Exists(id) => Refusal::Os {
                op: "oci",
                why: format!("container {id} exists"),
            },
            OciError::WrongState(id, is, want) => Refusal::Os {
                op: "oci",
                why: format!("container {id} is {is}, not {want}"),
            },
            OciError::Os { op, why } => Refusal::Os { op, why },
            OciError::Broken(why) => Refusal::Broken(why),
            OciError::SeedsExhausted { bound, .. } => Refusal::Exhausted {
                what: "create seeds",
                bound,
            },
        }
    }
}
