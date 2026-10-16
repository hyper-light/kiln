//! Host and process statistics: the server's and agents' host samples, the server's own process,
//! and a job's process tree. Linux reads procfs directly (no daemons, microseconds per sample);
//! macOS and Windows ask the OS through `sysinfo` (see [`crate::os`]).

pub use crate::os::metrics::{Sampler, process_group, process_self};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ProcStats {
    pub rss_bytes: u64,
    pub cpu_seconds: f64,
    pub open_fds: u64,
    pub threads: u64,
}
