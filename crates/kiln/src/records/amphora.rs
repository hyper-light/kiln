//! Records in a file of Amphora's engine (vendor/amphora, `amphora-engine`; CP-T1.5): the same
//! records kept in SQL as on Turso ([`imp`], `sql.rs`), one code path, compiled on Amphora's engine,
//! Turso's lineage with its pages checksummed. Opened when `KILN_RECORDS_ENGINE=amphora`
//! ([`super::Engine`]).

use amphora_engine as engine;

#[path = "sql.rs"]
mod imp;

pub use imp::SqlRecords as AmphoraRecords;
pub use imp::*;
