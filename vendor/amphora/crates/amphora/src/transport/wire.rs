//! Embedded owner protocol. Variant order is append-only.
use amphora_api::{
    Changes, Consistency, Migration, Page, Refusal, Rev, Row, Scan, SqlResult, Statement, Tx, Value,
};
use serde::{Deserialize, Serialize};

pub(super) const FRAME_MAX: usize = amphora_api::RESULT_MAX + (1 << 20);
pub(super) const COMMAND_MAX: usize = 4096;
pub(super) const CONNECTION_MAX: usize = 32;
pub(super) const WAIT_MAX: usize = 8;
pub(super) const WATCH_BATCH_MAX: usize = 64;
pub(super) const WATCH_REPLY_MAX: usize = 64 << 10;
pub(super) const SESSION_MAX: std::time::Duration = std::time::Duration::from_secs(30);
pub(super) const CAP_BOUNDED_COMPACTION: u64 = 1;

#[derive(Serialize, Deserialize)]
pub(super) struct Hello {
    pub version: u32,
    pub nonce: [u8; 32],
    pub millis: u64,
}
#[derive(Serialize, Deserialize)]
pub(super) struct Challenge {
    pub nonce: [u8; 32],
    pub proof: [u8; 32],
}
#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Session,
    Execute(#[serde(deserialize_with = "super::bounded::statements")] Vec<Statement>),
    Migrate(Migration),
    Records {
        namespace: String,
        op: Record,
    },
    Probe,
    Identity,
    RestoreProgress,
    WatchBatch(
        #[serde(deserialize_with = "super::bounded::watch_requests")] Vec<(String, String, Rev)>,
    ),
    ReplicaStatus,
    Capabilities,
}
#[derive(Serialize, Deserialize)]
pub(super) enum Record {
    Get(String, Vec<u8>, Consistency),
    Scan(String, Scan),
    Commit(#[serde(deserialize_with = "super::bounded::transaction")] Tx),
    Changes(String, Rev, usize),
    Wait(String, Rev),
    Head,
    Compact(Rev),
    TryWait(String, Rev),
    ImportPage(
        #[serde(deserialize_with = "super::bounded::import_page")] amphora_api::import::ImportPage,
    ),
    ImportState,
    CompactStep(Rev),
}
#[derive(Serialize, Deserialize)]
pub(super) enum Command {
    Batch(String),
    Query(
        String,
        #[serde(deserialize_with = "super::bounded::params")] Vec<Value>,
        bool,
    ),
    Columns(String),
    Commit,
    Abort,
    RecordsGet(String, String, Vec<u8>),
    RecordsCommit(
        String,
        #[serde(deserialize_with = "super::bounded::transaction")] Tx,
    ),
}
#[derive(Serialize, Deserialize)]
pub(super) enum Answer {
    Ready,
    Rows {
        #[serde(deserialize_with = "super::bounded::rows")]
        rows: Vec<Vec<Value>>,
        changes: i64,
        rowid: i64,
    },
    Columns(usize),
    Done,
    Sql(#[serde(deserialize_with = "super::bounded::sql_results")] Vec<SqlResult>),
    Migrated(bool),
    Row(Option<Row>),
    Page(#[serde(deserialize_with = "super::bounded::page")] Page),
    Rev(Rev),
    Changes(#[serde(deserialize_with = "super::bounded::changes")] Changes),
    OptionalRev(Option<Rev>),
    ImportState(Option<amphora_api::import::ImportProgress>),
    SessionRow {
        row: Option<Row>,
        changes: i64,
        rowid: i64,
    },
    SessionRev {
        rev: Rev,
        changes: i64,
        rowid: i64,
    },
    Identity(crate::database::remote::Identity),
    WatchBatch(
        #[serde(deserialize_with = "super::bounded::watch_answers")]
        Vec<Result<Option<Rev>, Refusal>>,
    ),
    ReplicaStatus(Option<amphora_api::replication::ReplicaStatus>),
    // Untyped SQL failures retain caller context; typed refusals use Response::Err.
    SqlError(String),
    Capabilities(u64),
    Compaction(amphora_api::CompactionProgress),
}
pub(super) type Response = Result<Answer, Refusal>;
