//! Observe: kiln's own metrics, logs and events (`docs/observe.md`). The engine is `kiln-obs`; this
//! is kiln's side of it: the group's owner on every node ([`owner`]), what nodes say on the
//! observe lane ([`wire`]), the writes a node gathers ([`ingest`]), and the API ([`api`]).

pub mod api;
pub mod dashboards;
pub mod github;
pub mod host;
pub mod hub;
pub mod kiln;
pub mod monitors;
pub mod notify;
pub mod ingest;
pub mod otlp;
pub mod owner;
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
