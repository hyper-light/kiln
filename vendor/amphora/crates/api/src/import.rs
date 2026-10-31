//! Revision-preserving, restartable import of one Records namespace from a fixed snapshot.
use crate::Rev;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportVersion {
    pub family: String,
    pub key: Vec<u8>,
    pub rev: Rev,
    pub seq: u64,
    pub value: Option<Vec<u8>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportState {
    pub head: Rev,
    pub horizon: Rev,
    pub feed: Rev,
    /// A bounded chunk of source family heads, accumulated across pages. These may include
    /// families whose versions were entirely compacted. Repeated entries must agree.
    pub newest: Vec<(String, Rev)>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPage {
    /// Stable identity of the consistent source snapshot, never a mutable file path alone.
    pub source: String,
    pub expected_cursor: Vec<u8>,
    pub next_cursor: Vec<u8>,
    pub versions: Vec<ImportVersion>,
    /// Scalar head/horizon/feed are fixed for this source snapshot; newest is a paged chunk.
    pub state: ImportState,
    pub done: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportProgress {
    pub source: String,
    pub cursor: Vec<u8>,
    pub done: bool,
}
