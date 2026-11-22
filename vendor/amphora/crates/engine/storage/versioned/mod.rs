//! Amphora: the storage under the SQL layer that lets many writers change one database at once
//! (Amphora's `docs/design/engine.md`, CP-T1.3): rows changed in place in one shared buffer pool,
//! under page latches, their earlier versions in undo records, reads from the B-tree alone.
//!
//! Built in stages beside the engine it is to replace, each measured (the design's last section);
//! the current engine stays the default until the comprehensive suite says otherwise. This stage:
//! the page format ([`page`]), the buffer pool ([`pool`]), B-link trees on it ([`blink`]) and row
//! versions on them ([`version`]: snapshots, first-writer-wins, rollback, collection); then their
//! files ([`disk`]; on Linux, `uring`: the engine's `io_uring` feature), the redo log ([`redo`]),
//! and checkpoints and recovery ([`durable`]); the commit validations' Elle-style isolation checks
//! (`elle`, G8 step 1); and a table's column copy ([`columns`], G11). Stage 3 begins with the
//! executor's cursor on the trees ([`cursor`]), then transactions' savepoints ([`version`]),
//! checked against the current engine by the shared model test of transactions (`txn_model`); and
//! the switch that puts a connection's B-trees on this storage ([`switch`]); then many writers
//! through SQL against a serial replay (`many_writers`) and Elle's checks through SQL (`elle_sql`).

pub mod blink;
pub mod columns;
pub(crate) mod columns_path;
#[cfg(test)]
mod conformance;
pub mod cursor;
pub mod disk;
pub mod durable;
#[cfg(test)]
mod elle;
#[cfg(test)]
mod elle_sql;
#[cfg(test)]
mod evict;
#[cfg(test)]
mod many_writers;
pub mod memory;
pub mod overflow;
pub mod page;
pub mod pool;
pub mod redo;
#[cfg(test)]
mod sql;
pub mod switch;
#[cfg(test)]
mod txn_model;
#[cfg(all(target_os = "linux", feature = "io_uring"))]
pub mod uring;
pub mod version;
