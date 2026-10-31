//! Amphora: the storage under the SQL layer that lets many writers change one database at once
//! (Amphora's `docs/design/engine.md`, CP-T1.3): rows changed in place in one shared buffer pool,
//! under page latches, their earlier versions in undo records, reads from the B-tree alone.
//!
//! Built in stages beside the engine it is to replace, each measured (the design's last section);
//! the current engine stays the default until the comprehensive suite says otherwise. This stage:
//! the page format ([`page`]), the buffer pool ([`pool`]), B-link trees on it ([`blink`]) and row
//! versions on them ([`version`]: snapshots, first-writer-wins, rollback, collection); then their
//! files ([`disk`]), the redo log ([`redo`]), and checkpoints and recovery ([`durable`]).

pub mod blink;
pub mod disk;
pub mod durable;
pub mod page;
pub mod pool;
pub mod redo;
pub mod version;
