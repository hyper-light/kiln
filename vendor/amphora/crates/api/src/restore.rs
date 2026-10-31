//! Bounded pages for restoring SQL from one fixed source snapshot.
use crate::Statement;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestorePage {
    pub expected_cursor: Vec<u8>,
    pub next_cursor: Vec<u8>,
    /// Schema script for this page, applied atomically with its data and cursor.
    pub schema: Option<String>,
    pub statements: Vec<Statement>,
}
