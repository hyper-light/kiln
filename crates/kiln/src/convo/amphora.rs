//! The conversation store on Amphora's engine (vendor/amphora, `amphora-engine`; CP-T1.1): the same store kept in
//! SQL as on Turso ([`imp`], `sqlstore.rs`), one code path, compiled on Amphora's engine, Turso's lineage with its
//! pages checksummed. Opened when `KILN_CONVERSATION_ENGINE=amphora` ([`super::Kind`]).

use amphora_engine as engine;

#[path = "sqlstore.rs"]
mod imp;

pub use imp::Sql as Amphora;
