//! Observe: kiln's own metrics, logs and events (`docs/observe.md`). The engine is `kiln-obs`; this
//! is kiln's side of it: the group's owner on every node ([`owner`]), what nodes say on the
//! observe lane ([`wire`]), the writes a node gathers ([`ingest`]), and the API ([`api`]).

pub mod api;
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
