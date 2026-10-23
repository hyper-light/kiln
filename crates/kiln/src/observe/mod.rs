//! Observe: kiln's own metrics, logs and events (`docs/observe.md`). The engine is `kiln-obs`; this
//! is kiln's side of it: the group's owner on every node ([`owner`]), what nodes say on the
//! observe lane ([`wire`]), the writes a node gathers ([`ingest`]), and the API ([`api`]).

pub mod api;
pub mod apm;
pub mod archives;
pub mod catalog;
pub mod cloud;
pub mod collect;
pub mod dashboards;
pub mod ddash;
pub mod ddagent;
pub mod ddtrace;
pub mod dora;
pub mod downtimes;
pub mod errors;
pub mod github;
pub mod host;
pub mod hub;
pub mod incidents;
pub mod infra;
pub mod kafka;
pub mod kiln;
pub mod logmetrics;
pub mod monitors;
pub mod notebooks;
pub mod notify;
pub mod openapi;
pub mod ingest;
pub mod otlp;
pub mod owner;
pub mod pipeline;
pub mod rehydrate;
pub mod remote;
pub mod scanner;
pub mod scrape;
pub mod slos;
pub mod smtp;
pub mod spans;
pub mod statsd;
pub mod synthetics;
pub mod wire;

pub use owner::{Observe, Out, Role, SERVER, Settings, View};

/// What a node's aggregators owe as it stops (the tracers' concentrator, each DogStatsD listener):
/// each registered as it starts and told done once its last offer is made; then one wait for all of
/// it to be written.
static LAST: std::sync::Mutex<(Vec<ingest::Ack>, Option<ingest::Ack>)> = std::sync::Mutex::new((Vec::new(), None));

/// An aggregator's debt to a node stopping: set once its last offer is made.
pub(crate) fn owes_last() -> ingest::Ack {
    let ack = ingest::Ack::new();
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    last.0.retain(|a| !a.done());
    last.0.push(ack.clone());
    ack
}

/// Whether what the node's aggregators owed as it stops is written: each one's last offer made, and
/// then everything taken before them written (asked once, the first time they all are).
pub fn last_written() -> bool {
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !last.0.iter().all(ingest::Ack::done) {
        return false;
    }
    if let Some(written) = &last.1 {
        return written.done();
    }
    let Some(o) = api::get() else { return true };
    let written = ingest::Ack::new();
    o.settle(written.clone());
    last.1 = Some(written);
    false
}

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
