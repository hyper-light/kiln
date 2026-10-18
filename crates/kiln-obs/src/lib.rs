#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic))]
//! kiln's observability engine (`docs/observe.md`): metrics, logs and events in a columnar store,
//! the query language over them, and monitors' rules.
//!
//! Like hyper-raft's crates it does no I/O of its own beyond the files a store and its group's log
//! own: it is fed the group's entries and asked queries, and kiln drives it and carries its
//! messages (`crates/kiln/src/observe`).

pub mod batch;
pub mod bits;
pub mod block;
pub mod command;
pub mod crc32c;
pub mod gorilla;
pub mod group;
pub mod hash;
pub mod head;
pub mod image;
pub mod index;
pub mod logs;
pub mod lz4;
pub mod machine;
pub mod manifest;
pub mod monitor;
pub mod otlp;
pub mod prom;
pub mod query;
pub mod store;
pub mod symbols;
pub mod testkit;
pub mod varint;
