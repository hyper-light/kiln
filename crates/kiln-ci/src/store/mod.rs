//! The rules of the runtime store (docs/ci.md W08): how objects are cut into chunks, and GitHub's
//! cache semantics. The server's store and services in `crates/kiln/src/ci/services` carry them
//! out.
//!
//! - [`cdc`]: content-defined chunking (W08-R01).
//! - [`cache`]: cache modes, lookup, immutability and eviction (W08-R06, W08-R14).

pub mod cache;
pub mod cdc;
