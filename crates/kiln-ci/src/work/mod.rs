//! Doing the least work a change needs (docs/ci.md W24): every unit of work keyed by what it
//! depends on, so a recorded success can be reused and a rebuild can say why.
//!
//! - [`key`]: weak and strong fingerprints (W24-R01, R03 to R05).
//! - [`graph`]: package graphs from manifests, and the packages a change affects (W24-R16, R17).
//! - [`ops`]: container operations as a DAG of W24 units, and the Dockerfile frontend that lowers
//!   a Dockerfile to them (W24-R29).

pub mod graph;
pub mod key;
pub mod ops;
