#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic))]
//! kiln's observability engine (`docs/observe.md`): metrics, logs and events in a columnar store,
//! the query language over them, and monitors' rules.
//!
//! Like hyper-raft's crates it does no I/O of its own beyond the files a store owns: it is fed
//! write batches and asked queries, and kiln drives it (`crates/kiln/src/observe`).

pub mod batch;
pub mod bits;
pub mod hash;
pub mod head;
pub mod index;
pub mod store;
pub mod symbols;
pub mod gorilla;
pub mod testkit;
pub mod varint;
