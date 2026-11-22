//! The conversation store on Turso (vendor/turso): the store kept in SQL ([`imp`], `sqlstore.rs`) on
//! `turso_core`. The engine kiln opens unless told otherwise ([`super::Kind`]).

use turso_core as engine;

#[path = "sqlstore.rs"]
mod imp;

pub use imp::Sql as Turso;
