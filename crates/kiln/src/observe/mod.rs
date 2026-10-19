//! Observe: kiln's own metrics, logs and events (`docs/observe.md`). The engine is `kiln-obs`; this
//! is kiln's side of it: the group's owner on every node ([`owner`]), what nodes say on the
//! observe lane ([`wire`]), the writes a node gathers ([`ingest`]), and the API ([`api`]).

pub mod api;
pub mod cloud;
pub mod collect;
pub mod dashboards;
pub mod downtimes;
pub mod github;
pub mod host;
pub mod hub;
pub mod kafka;
pub mod kiln;
pub mod logmetrics;
pub mod monitors;
pub mod notify;
pub mod openapi;
pub mod ingest;
pub mod otlp;
pub mod owner;
pub mod pipeline;
pub mod scrape;
pub mod slos;
pub mod smtp;
pub mod spans;
pub mod wire;

pub use owner::{Observe, Out, Role, SERVER, Settings, View};

/// An id for something kept by name (`m…` a monitor, `d…` a dashboard): 64 random bits, none of
/// those `taken`. (A UUIDv7's first digits are its millisecond: two made in one would be the same.)
pub(crate) fn fresh_id(prefix: &str, taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("{prefix}{:016x}", rand::random::<u64>());
        if !taken(&id) {
            return id;
        }
    }
}

/// The time observe gives what it writes and judges (ms since the epoch): this machine's clock.
/// `KILN_OBSERVE_CLOCK_SKEW_MS` moves it, for a test of a machine whose clock is off (changing the
/// real one takes root): what observe of that machine sees is what a skewed machine's would be.
pub fn clock_ms() -> i64 {
    static SKEW: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    crate::util::now_ms().saturating_add(*SKEW.get_or_init(|| std::env::var("KILN_OBSERVE_CLOCK_SKEW_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)))
}

/// Bytes free to this process on the volume `path` is on, where the system says.
#[cfg(unix)]
#[allow(clippy::useless_conversion)] // u64 on Linux, narrower elsewhere (macOS's block counts)
pub fn disk_free(path: &std::path::Path) -> Option<u64> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    Some(u64::from(s.blocks_available()).saturating_mul(u64::from(s.fragment_size())))
}

/// Bytes free on the volume `path` is on: the disk whose mount point holds it, nearest first.
#[cfg(not(unix))]
pub fn disk_free(path: &std::path::Path) -> Option<u64> {
    let path = std::fs::canonicalize(path).ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks.list().iter().filter(|d| path.starts_with(d.mount_point())).max_by_key(|d| d.mount_point().as_os_str().len()).map(sysinfo::Disk::available_space)
}
