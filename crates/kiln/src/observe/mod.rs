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
