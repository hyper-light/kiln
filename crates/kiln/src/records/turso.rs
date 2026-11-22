//! Records in a file of kiln's embedded Turso (vendor/turso): the records kept in SQL ([`imp`],
//! `sql.rs`) on `turso_core`. The engine [`super::open`] opens unless told otherwise
//! ([`super::Engine`]).

use turso_core as engine;

#[path = "sql.rs"]
mod imp;

pub use imp::SqlRecords as TursoRecords;
pub use imp::*;
