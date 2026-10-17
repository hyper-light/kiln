//! kiln's deployment engine: what it takes to render, compare and sync applications, written
//! as a state machine with no clock, no disk and no network, the way hyper-raft's core is.
//!
//! The engine ([`engine`]) is told what happened (an app's render inputs moved, a render
//! finished, a live object changed, an apply answered, time passed) and says what to do
//! (render this, watch that kind, apply this object, delete that one, record this deployment).
//! Its owner, the kiln server, does the I/O. Because nothing here waits on anything, the
//! benchmarks drive the same engine production runs, against simulated clusters, at full speed.
//!
//! - [`obj`]: object identity, parsing, structural hashes, compressed storage.
//! - [`model`]: app configuration, sync options and policies (Argo CD's spellings).
//! - [`health`]: resource health, rule for rule as Argo CD assesses it.
//! - [`diff`]: comparing desired and live objects by field ownership.
//! - [`sync`]: planning a sync: phases, waves, hooks and prunes, in Argo CD's order.
//! - [`engine`]: the controller.
//! - [`sim`] and [`harness`]: simulated clusters, and the engine driven against them on a
//!   simulated clock (tests and `kiln cd bench`).

pub mod bench;
pub mod diff;
pub mod engine;
pub mod harness;
pub mod health;
pub mod model;
pub mod obj;
pub mod sim;
pub mod sync;

#[cfg(test)]
mod tests;
