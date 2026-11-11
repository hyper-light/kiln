//! Durable communication custody and explicit status replacement before source admission.
//! A dispatched revision stays immutable and frozen, including across backpressure. Only its
//! never-dispatched successor can be superseded; every original remains in the ledger.

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::Store;

pub mod handoffs;

pub const TABLES: &[&str] = &["agent_mail", "agent_mail_transfers"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailStatusKey"))]
pub struct StatusKey {
    /// Absent means session scope; a task identity is distinct from every other task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailPublish"))]
pub struct Publish {
    pub owner: String,
    pub sender: String,
    pub recipient: String,
    pub id: String,
    pub command: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusKey>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailDisposition"))]
pub enum Disposition { Waiting, Dispatched, Admitted, Consumed, Superseded, Cancelled, Transferred }

impl Disposition {
    fn name(self) -> &'static str {
        match self { Self::Waiting => "waiting", Self::Dispatched => "dispatched", Self::Admitted => "admitted", Self::Consumed => "consumed", Self::Superseded => "superseded", Self::Cancelled => "cancelled", Self::Transferred => "transferred" }
    }
    fn parse(value: &str) -> Result<Self, Error> {
        match value { "waiting" => Ok(Self::Waiting), "dispatched" => Ok(Self::Dispatched), "admitted" => Ok(Self::Admitted), "consumed" => Ok(Self::Consumed), "superseded" => Ok(Self::Superseded), "cancelled" => Ok(Self::Cancelled), "transferred" => Ok(Self::Transferred), _ => Err(Error::Unavailable { reason: "invalid saved communication disposition".into() }) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailReceipt"))]
pub struct Receipt {
    pub id: String,
    pub revision: u64,
    pub generation: u64,
    pub disposition: Disposition,
    pub repeat: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailProjection"))]
pub struct Projection { pub publication: Receipt, pub mail_scope: String }

/// The same opaque sender namespace exposed by /api/me. This is a label, never authority;
/// callers obtain its input from authenticated context or the immutable committed publication.
pub fn scope(sender: &str) -> String { crate::util::sha256_hex(format!("mail-scope-v1:{sender}").as_bytes()) }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailToken"))]
pub struct Token { pub revision: u64, pub generation: u64 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailDelivery"))]
pub struct Delivery { pub token: Token, pub epoch: u64, pub publication: Publish }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailPublication"))]
pub struct Publication { pub receipt: Receipt, pub publication: Publish, #[serde(default, skip_serializing_if = "Option::is_none")] pub terminal: Option<Value> }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailPage"))]
pub struct Page<T> { pub items: Vec<T>, pub next: Option<u64> }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailHead"))]
pub struct Head { pub receipt: Receipt, pub sender: String, pub bytes: u64, pub epoch: Option<u64>, pub frozen: bool }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailTransfer"))]
pub struct Transfer { pub generation: u64, pub from_epoch: u64, pub to_epoch: u64, pub disposition: Value }

/// Exact bytes exported under the old owner's fence. Both peers prove this same manifest;
/// neither a new capacity picture nor a naked epoch can authorize custody transfer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransferManifest {
    pub attempt: String,
    pub session: String,
    pub source_epoch: u64,
    pub destination_epoch: u64,
    pub count: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferUp {
    Offer { manifest: TransferManifest, target: String, instance: Option<String> },
    Ready { manifest: TransferManifest },
    Adopt { manifest: TransferManifest },
    Abort { manifest: TransferManifest },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferRefusal { Unavailable, Stale, Reused, Capacity, Unsupported }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferDown {
    Offered { manifest: TransferManifest, disk_floor: Option<u64> },
    Committed { manifest: TransferManifest },
    Aborted { manifest: TransferManifest },
    Refused { attempt: String, code: TransferRefusal },
}

fn push_page<T: Serialize>(page: &mut Page<T>, bytes: &mut usize, item: T, cursor: u64, budget: usize) -> Result<bool, Error> {
    let size = match encoded(&item, budget, cursor) {
        Ok(value) => value.len(),
        Err(Error::TooLarge { .. }) if !page.items.is_empty() => return Ok(false),
        Err(error) => return Err(error),
    };
    #[derive(Serialize)]
    struct EmptyPage { items: [(); 0], next: Option<u64> }
    let overhead = encoded(&EmptyPage { items: [], next: Some(cursor) }, budget, cursor)?.len().max(encoded(&EmptyPage { items: [], next: None }, budget, cursor)?.len());
    let new_bytes = bytes.checked_add(size).and_then(|n| n.checked_add(usize::from(!page.items.is_empty()))).ok_or(Error::TooLarge { revision: cursor, budget })?;
    if new_bytes.checked_add(overhead).is_none_or(|n| n > budget) {
        if page.items.is_empty() { return Err(Error::TooLarge { revision: cursor, budget }); }
        return Ok(false);
    }
    page.items.push(item);
    page.next = Some(cursor);
    *bytes = new_bytes;
    Ok(true)
}

/// Durable unresolved recipients, paged by their oldest unresolved revision. A caller begins a
/// fresh scan after a ledger event; head movement may repeat a recipient, never erase its rows.
pub async fn recipients(store: &Store, after: u64, budget: usize) -> Result<Page<String>, Error> {
    store.read(move |c| {
        let mut page = Page { items: Vec::new(), next: None };
        let mut bytes = 0;
        let mut query = c.prepare("SELECT recipient,MIN(revision) AS first FROM agent_mail WHERE disposition IN ('waiting','dispatched','admitted') GROUP BY recipient HAVING first>?1 ORDER BY first")?;
        let mut rows = query.query([number(after)?])?;
        while let Some(row) = rows.next()? {
            let cursor = unsigned(row.get(1)?)?;
            if !push_page(&mut page, &mut bytes, row.get::<_, String>(0)?, cursor, budget)? { encoded(&page, budget, cursor)?; return Ok(Ok(page)); }
        }
        page.next = None; encoded(&page, budget, after)?; Ok(Ok(page))
    }).await.map_err(store_error)?
}

pub async fn peer_heads(store: &Store, recipient: String, after: u64, budget: usize) -> Result<Page<Head>, Error> {
    store.read(move |c| {
        let mut page = Page { items: Vec::new(), next: None }; let mut bytes = 0;
        let mut query = c.prepare("SELECT m.id,m.revision,m.generation,m.disposition,m.sender,m.bytes,m.epoch,m.frozen FROM agent_mail m WHERE m.recipient=?1 AND m.revision>?2 AND m.disposition IN ('waiting','dispatched','admitted') AND NOT EXISTS (SELECT 1 FROM agent_mail older WHERE older.recipient=m.recipient AND older.sender=m.sender AND older.disposition IN ('waiting','dispatched','admitted') AND older.revision<m.revision) ORDER BY m.revision")?;
        let mut rows = query.query(params![recipient, number(after)?])?;
        while let Some(row) = rows.next()? {
            let receipt = from_receipt(row_receipt(row)?, false)?; let cursor = receipt.revision;
            let item = Head { receipt, sender: row.get(4)?, bytes: unsigned(row.get(5)?)?, epoch: row.get::<_, Option<i64>>(6)?.map(unsigned).transpose()?, frozen: row.get(7)? };
            if !push_page(&mut page, &mut bytes, item, cursor, budget)? { encoded(&page, budget, cursor)?; return Ok(Ok(page)); }
        }
        page.next = None; encoded(&page, budget, after)?; Ok(Ok(page))
    }).await.map_err(store_error)?
}

/// Waiting demand excludes already admitted/in-flight-only peers. A frozen waiting original
/// still represents unaccepted demand; no body is loaded and no caller polls for this page.
pub async fn waiting_peers(store: &Store, recipient: String, after: u64, budget: usize) -> Result<Page<super::admission::Demand>, Error> {
    store.read(move |c| Ok(waiting_peers_on(c, &recipient, after, budget))).await.map_err(store_error)?
}

fn waiting_peers_on(c: &Connection, recipient: &str, after: u64, budget: usize) -> Result<Page<super::admission::Demand>, Error> {
    let mut page = Page { items: Vec::new(), next: None }; let mut bytes = 0;
    let mut q = c.prepare("SELECT sender,MIN(revision) AS first FROM agent_mail WHERE recipient=?1 AND disposition='waiting' GROUP BY sender HAVING first>?2 ORDER BY first").map_err(sql_error)?;
    let mut rows = q.query(params![recipient, number(after)?]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let cursor = unsigned(row.get(1).map_err(sql_error)?)?;
        let demand = super::admission::Demand { peer: row.get(0).map_err(sql_error)?, revision: cursor };
        if !push_page(&mut page, &mut bytes, demand, cursor, budget)? { encoded(&page, budget, cursor)?; return Ok(page); }
    }
    page.next = None; encoded(&page, budget, after)?; Ok(page)
}

/// Metadata-only authentication of an owner's event, including an idempotent consumed ACK.
pub async fn attempt(store: &Store, recipient: String, token: Token, epoch: u64) -> Result<Option<Receipt>, Error> {
    store.read(move |c| Ok(attempt_on(c, &recipient, token, epoch, false).map(|row| row.map(|(receipt, _)| receipt)))).await.map_err(store_error)?
}

fn attempt_on(c: &Connection, recipient: &str, token: Token, epoch: u64, include_waiting: bool) -> Result<Option<(Receipt, String)>, Error> {
    if token.generation == 0 { return Ok(None); }
    let row = c.query_row("SELECT id,revision,generation,disposition,sender FROM agent_mail WHERE recipient=?1 AND revision=?2 AND generation=?3 AND epoch=?4 AND (disposition IN ('dispatched','admitted','consumed','cancelled') OR (?5 AND disposition='waiting' AND frozen=1)) UNION ALL SELECT m.id,t.revision,t.generation,'transferred',m.sender FROM agent_mail_transfers t JOIN agent_mail m ON m.revision=t.revision WHERE m.recipient=?1 AND t.revision=?2 AND t.generation=?3 AND t.from_epoch=?4 LIMIT 1", params![recipient, number(token.revision)?, number(token.generation)?, number(epoch)?, include_waiting], |r| Ok((row_receipt(r)?, r.get::<_, String>(4)?))).optional().map_err(sql_error)?;
    row.map(|(row, sender)| Ok((from_receipt(row, false)?, sender))).transpose()
}

/// For an authenticated source event, bind its exact committed original and sender namespace.
/// This returns the saved custody disposition, including Waiting and old transfer receipts;
/// it never promotes publication retention to admission or consumption. The caller preserves
/// Receipt.repeat from its own committed transition when constructing the outgoing frame.
pub async fn projection(store: &Store, recipient: String, token: Token, epoch: u64) -> Result<Option<Projection>, Error> {
    store.read(move |c| Ok(projection_on(c, &recipient, token, epoch))).await.map_err(store_error)?
}

/// The caller authenticates the current owner and its current event epoch before using this
/// committed terminal receipt. This grants no new credit or state transition across a move.
pub async fn terminal_projection(store: &Store, recipient: String, token: Token) -> Result<Option<Projection>, Error> {
    store.read(move |c| Ok(terminal_projection_on(c, &recipient, token))).await.map_err(store_error)?
}

fn terminal_projection_on(c: &Connection, recipient: &str, token: Token) -> Result<Option<Projection>, Error> {
    if token.generation == 0 { return Ok(None); }
    let row=c.query_row("SELECT id,revision,generation,disposition,sender FROM agent_mail WHERE recipient=?1 AND revision=?2 AND generation=?3 AND disposition IN ('consumed','cancelled')",params![recipient,number(token.revision)?,number(token.generation)?],|row|Ok((row_receipt(row)?,row.get::<_,String>(4)?))).optional().map_err(sql_error)?;
    let Some((receipt,sender))=row else { return Ok(None) };
    let projection=Projection { publication:from_receipt(receipt,true)?,mail_scope:scope(&sender) };
    encoded(&projection,super::state_view::FRAME,token.revision)?;
    Ok(Some(projection))
}

fn projection_on(c: &Connection, recipient: &str, token: Token, epoch: u64) -> Result<Option<Projection>, Error> {
    let Some((publication, sender)) = attempt_on(c, recipient, token, epoch, true)? else { return Ok(None) };
    let projection = Projection { publication, mail_scope: scope(&sender) };
    encoded(&projection, super::state_view::FRAME, token.revision)?;
    Ok(Some(projection))
}

/// Recovery preserves the original attempt token: uncertain dispatch is not permission to
/// allocate a new generation or pretend that its source never accepted it.
pub async fn attempts(store: &Store, recipient: String, after: u64, budget: usize) -> Result<Page<Delivery>, Error> {
    store.read(move |c| {
        let mut page = Page { items: Vec::new(), next: None }; let mut bytes = 0;
        let mut q = c.prepare("SELECT revision,generation,epoch,body FROM agent_mail WHERE recipient=?1 AND revision>?2 AND disposition IN ('dispatched','admitted') ORDER BY revision")?;
        let mut rows = q.query(params![recipient, number(after)?])?;
        while let Some(row) = rows.next()? {
            let revision = unsigned(row.get(0)?)?;
            let token = Token { revision, generation: unsigned(row.get(1)?)? };
            let epoch = unsigned(row.get(2)?)?; let body: String = row.get(3)?;
            let publication = serde_json::from_str(&body)?;
            if !push_page(&mut page, &mut bytes, Delivery { token, epoch, publication }, revision, budget)? { encoded(&page, budget, revision)?; return Ok(Ok(page)); }
        }
        page.next = None; encoded(&page, budget, after)?; Ok(Ok(page))
    }).await.map_err(store_error)?
}

pub async fn admitted_count(store: &Store, recipient: String, epoch: u64) -> Result<u64, Error> {
    store.read(move |c| Ok(c.query_row("SELECT COUNT(*) FROM agent_mail WHERE recipient=?1 AND epoch=?2 AND disposition='admitted'", params![recipient, number(epoch)?], |r| r.get::<_, i64>(0))?)).await.map_err(store_error).and_then(unsigned)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailBodyRead"))]
pub struct BodyRead {
    pub revision: u64,
    /// JSON pointer into the original Publish envelope, for example /command/text.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailBodyNode"))]
pub enum BodyNode {
    Value { value: Value },
    Text { text: String },
    Keys { keys: Vec<String> },
    Array { len: usize },
    Transfers { entries: Vec<Transfer> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailBodyReply"))]
pub struct BodyReply { pub revision: u64, pub path: String, pub offset: usize, pub node: BodyNode, pub next: Option<usize> }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "MailError"))]
pub enum Error {
    Invalid { reason: String },
    Reused { id: String },
    Capacity { required: u64, authority: u64 },
    Backpressure { pending: u64, authority: u64 },
    StorageFull,
    Unavailable { reason: String },
    Stale { revision: u64, generation: u64 },
    TooLarge { revision: u64, budget: usize },
    NoSession,
}

impl Error {
    pub fn status_code(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::Invalid { .. } => StatusCode::BAD_REQUEST,
            Self::Reused { .. } | Self::Stale { .. } => StatusCode::CONFLICT,
            Self::Capacity { .. } | Self::StorageFull => StatusCode::INSUFFICIENT_STORAGE,
            Self::Backpressure { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::NoSession => StatusCode::NOT_FOUND,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{self:?}") }
}
impl std::error::Error for Error {}

fn sql_error(error: rusqlite::Error) -> Error {
    if matches!(&error, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DiskFull) {
        Error::StorageFull
    } else { Error::Unavailable { reason: error.to_string() } }
}
fn store_error(error: anyhow::Error) -> Error {
    if let Some(error) = error.downcast_ref::<Error>() { return error.clone(); }
    if let Some(error) = error.downcast_ref::<rusqlite::Error>() && matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DiskFull) { return Error::StorageFull; }
    Error::Unavailable { reason: error.to_string() }
}
fn number(value: u64) -> Result<i64, Error> { i64::try_from(value).map_err(|_| Error::Invalid { reason: "communication revision exceeds SQLite integer range".into() }) }
fn unsigned(value: i64) -> Result<u64, Error> { u64::try_from(value).map_err(|_| Error::Unavailable { reason: "negative saved communication revision".into() }) }
fn encoded<T: Serialize + ?Sized>(value: &T, budget: usize, revision: u64) -> Result<Vec<u8>, Error> {
    super::state_view::encode(value, budget).map_err(|e| match e { super::state_view::Error::Budget => Error::TooLarge { revision, budget }, _ => Error::Unavailable { reason: format!("communication serialization failed: {e:?}") } })
}
fn valid_identity(value: &str) -> bool { !value.is_empty() && !value.chars().any(char::is_control) }
fn handoff_active(connection: &Connection, recipient: &str) -> Result<bool, Error> {
    connection.query_row("SELECT EXISTS(SELECT 1 FROM agent_mail_handoffs WHERE session=?1 AND phase IN ('offered','prepared'))", [recipient], |row| row.get(0)).map_err(sql_error)
}
fn validate(p: &Publish) -> Result<(), Error> {
    if [&p.owner, &p.sender, &p.recipient, &p.id].iter().any(|s| !valid_identity(s)) { return Err(Error::Invalid { reason: "communication identities must be nonempty with no control characters".into() }); }
    if let Some(status) = &p.status && (!valid_identity(&status.key) || status.task.as_deref().is_some_and(|s| !valid_identity(s))) { return Err(Error::Invalid { reason: "status key and task must be nonempty with no control characters".into() }); }
    Ok(())
}

fn row_receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, i64, i64, String)> { Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)) }
fn from_receipt(row: (String, i64, i64, String), repeat: bool) -> Result<Receipt, Error> {
    Ok(Receipt { id: row.0, revision: unsigned(row.1)?, generation: unsigned(row.2)?, disposition: Disposition::parse(&row.3)?, repeat })
}

/// Bodyless reconciliation by the complete immutable request identity. Consumers call on a
/// lifecycle/commit event or startup; a retained receipt is not evidence of model delivery.
pub async fn receipt(store: &Store, owner: String, sender: String, recipient: String, id: String) -> Result<Option<Receipt>, Error> {
    store.read(move |c| Ok(receipt_on(c, &owner, &sender, &recipient, &id))).await.map_err(store_error)?
}

fn receipt_on(c: &Connection, owner: &str, sender: &str, recipient: &str, id: &str) -> Result<Option<Receipt>, Error> {
    if [owner, sender, recipient, id].iter().any(|s| !valid_identity(s)) { return Err(Error::Invalid { reason: "communication identities must be nonempty with no control characters".into() }); }
    let row = c.query_row("SELECT id,revision,generation,disposition FROM agent_mail WHERE owner=?1 AND sender=?2 AND recipient=?3 AND id=?4", params![owner, sender, recipient, id], row_receipt).optional().map_err(sql_error)?;
    let receipt = row.map(|r| from_receipt(r, false)).transpose()?;
    if let Some(receipt) = &receipt { encoded(receipt, super::state_view::FRAME, receipt.revision)?; }
    Ok(receipt)
}

/// `bytes_authority` is the room this one encoded original may take on disk: the caller's
/// physical authority, its SQLite/WAL overhead reserved separately. Nothing is summed over every
/// conversation's mail, so one conversation's backlog takes no other's room; each recipient's
/// own pending depth is its bound (`pending_authority`), which learning never enlarges. A failed
/// transaction does not transfer custody.
pub async fn publish(store: &Store, publication: Publish, bytes_authority: u64, pending_authority: u64) -> Result<Receipt, Error> {
    store.call(move |c| Ok(publish_on(c, &publication, bytes_authority, pending_authority))).await.map_err(store_error)?
}

fn publish_on(c: &mut Connection, publication: &Publish, authority: u64, pending_authority: u64) -> Result<Receipt, Error> {
    validate(publication)?;
    // SQLite stores this exact accounting in a signed integer; its representable range is a
    // storage invariant, not a learned capacity or a permission to enlarge caller authority.
    let authority = authority.min(i64::MAX as u64);
    // Store::call can batch calls; this savepoint also rolls back every write on typed refusal.
    let tx = c.savepoint().map_err(sql_error)?;
    let old: Option<(String, i64, i64, String, String)> = tx.query_row(
        "SELECT id,revision,generation,disposition,body FROM agent_mail WHERE owner=?1 AND sender=?2 AND recipient=?3 AND id=?4",
        params![publication.owner, publication.sender, publication.recipient, publication.id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().map_err(sql_error)?;
    if let Some((id, revision, generation, disposition, previous)) = old {
        let original: Publish = serde_json::from_str(&previous).map_err(|e| Error::Unavailable { reason: format!("invalid saved communication: {e}") })?;
        // A transport retry may carry a fresh authenticated relay record. Idempotency is about
        // the same command and explicit status scope; keep the first accepted proof unchanged.
        if original.command != publication.command || original.status != publication.status { return Err(Error::Reused { id }); }
        let receipt = from_receipt((id, revision, generation, disposition), true)?;
        tx.commit().map_err(sql_error)?;
        return Ok(receipt);
    }
    let body = encoded(publication, super::state_view::FRAME, 0)?;
    let bytes = u64::try_from(body.len()).map_err(|_| Error::Capacity { required: u64::MAX, authority })?;
    let body = String::from_utf8(body).map_err(|e| Error::Unavailable { reason: e.to_string() })?;
    if bytes > authority { return Err(Error::Capacity { required: bytes, authority }); }
    if handoff_active(&tx, &publication.recipient)? { return Err(Error::Unavailable { reason: "current owner handover retains all accepted communication; new custody is waiting".into() }); }
    let unresolved: i64 = tx.query_row("SELECT COUNT(*) FROM agent_mail WHERE recipient=?1 AND disposition IN ('waiting','dispatched','admitted')", [&publication.recipient], |r| r.get(0)).map_err(sql_error)?;
    let superseded = if let Some(status) = &publication.status {
        tx.query_row("SELECT COUNT(*) FROM agent_mail WHERE owner=?1 AND sender=?2 AND recipient=?3 AND status_task IS ?4 AND status_key=?5 AND disposition='waiting' AND frozen=0", params![publication.owner, publication.sender, publication.recipient, status.task, status.key], |r| r.get::<_, i64>(0)).map_err(sql_error)?
    } else { 0 };
    let pending = unsigned(unresolved)?.checked_sub(unsigned(superseded)?).and_then(|n| n.checked_add(1)).ok_or_else(|| Error::Unavailable { reason: "communication pending accounting overflow".into() })?;
    if pending > pending_authority { return Err(Error::Backpressure { pending, authority: pending_authority }); }
    if let Some(status) = &publication.status {
        tx.execute("UPDATE agent_mail SET disposition='superseded' WHERE owner=?1 AND sender=?2 AND recipient=?3 AND status_task IS ?4 AND status_key=?5 AND disposition='waiting' AND frozen=0",
            params![publication.owner, publication.sender, publication.recipient, status.task, status.key]).map_err(sql_error)?;
    }
    let status_task = publication.status.as_ref().and_then(|s| s.task.as_deref());
    let status_key = publication.status.as_ref().map(|s| s.key.as_str());
    tx.execute("INSERT INTO agent_mail(owner,sender,recipient,id,body,bytes,status_task,status_key,disposition) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'waiting')",
        params![publication.owner, publication.sender, publication.recipient, publication.id, body, number(bytes)?, status_task, status_key]).map_err(sql_error)?;
    let revision = unsigned(tx.last_insert_rowid())?;
    tx.commit().map_err(sql_error)?;
    Ok(Receipt { id: publication.id.clone(), revision, generation: 0, disposition: Disposition::Waiting, repeat: false })
}

// A frozen revision, even when waiting after a refusal, holds the stream until handoff. The
// candidate itself is excluded so that it can be retried without admitting its successor.
const ELIGIBLE: &str = "m.disposition='waiting' AND (m.status_key IS NULL OR NOT EXISTS (SELECT 1 FROM agent_mail active WHERE active.owner=m.owner AND active.sender=m.sender AND active.recipient=m.recipient AND active.status_task IS m.status_task AND active.status_key=m.status_key AND active.revision<>m.revision AND active.frozen=1 AND active.disposition IN ('waiting','dispatched','admitted')))";

pub async fn pending(store: &Store, recipient: String, after: u64, budget: usize) -> Result<Page<Receipt>, Error> {
    store.read(move |c| Ok(pending_on(c, &recipient, after, budget))).await.map_err(store_error)?
}

fn pending_on(c: &Connection, recipient: &str, after: u64, budget: usize) -> Result<Page<Receipt>, Error> {
    pending_filtered_on(c, recipient, None, after, budget)
}

pub async fn pending_for(store: &Store, recipient: String, sender: String, after: u64, budget: usize) -> Result<Page<Receipt>, Error> {
    store.read(move |c| Ok(pending_filtered_on(c, &recipient, Some(&sender), after, budget))).await.map_err(store_error)?
}

fn pending_filtered_on(c: &Connection, recipient: &str, sender: Option<&str>, after: u64, budget: usize) -> Result<Page<Receipt>, Error> {
    let mut page = Page { items: Vec::new(), next: None }; let mut bytes = 0;
    let mut query = c.prepare(&format!("SELECT m.id,m.revision,m.generation,m.disposition FROM agent_mail m WHERE m.recipient=?1 AND m.revision>?2 AND (?3 IS NULL OR m.sender=?3) AND {ELIGIBLE} ORDER BY m.revision")).map_err(sql_error)?;
    let mut rows = query.query(params![recipient, number(after)?, sender]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let entry = from_receipt(row_receipt(row).map_err(sql_error)?, false)?;
        let revision = entry.revision;
        if !push_page(&mut page, &mut bytes, entry, revision, budget)? { encoded(&page, budget, revision)?; return Ok(page); }
    }
    page.next = None;
    encoded(&page, budget, after)?;
    Ok(page)
}

/// Atomically freezes a still-eligible revision before any source command is sent.
pub async fn claim_at(store: &Store, revision: u64, epoch: u64, source_generation: u64, source_connection: String, budget: usize) -> Result<Option<Delivery>, Error> {
    if !valid_identity(&source_connection) { return Err(Error::Invalid { reason: "source connection identity is required".into() }); }
    store.call(move |c| Ok(claim_at_on(c, revision, epoch, source_generation, Some(&source_connection), budget))).await.map_err(store_error)?
}

#[cfg(test)]
fn claim_on(c: &mut Connection, revision: u64, epoch: u64, budget: usize) -> Result<Option<Delivery>, Error> {
    claim_at_on(c, revision, epoch, 0, None, budget)
}

fn claim_at_on(c: &mut Connection, revision: u64, epoch: u64, source_generation: u64, source_connection: Option<&str>, budget: usize) -> Result<Option<Delivery>, Error> {
    let tx = c.savepoint().map_err(sql_error)?;
    let recipient: Option<String> = tx.query_row("SELECT recipient FROM agent_mail WHERE revision=?1", [number(revision)?], |row| row.get(0)).optional().map_err(sql_error)?;
    if let Some(recipient) = &recipient && handoff_active(&tx, recipient)? { return Ok(None); }
    let saved: Option<(String, i64, Option<i64>, Option<i64>, Option<String>)> = tx.query_row(&format!("SELECT m.body,m.generation,m.epoch,m.source_generation,m.source_connection FROM agent_mail m WHERE m.revision=?1 AND {ELIGIBLE}"), [number(revision)?], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().map_err(sql_error)?;
    let Some((body, generation, minimum_epoch, previous_source_generation, previous_connection)) = saved else { return Ok(None) };
    if minimum_epoch.map(unsigned).transpose()?.is_some_and(|minimum| epoch < minimum) { return Err(Error::Stale { revision, generation: unsigned(generation)? }); }
    if source_connection.is_some() && previous_connection.as_deref() == source_connection && previous_source_generation == Some(number(source_generation)?) && minimum_epoch == Some(number(epoch)?) { return Ok(None); }
    let generation = unsigned(generation)?.checked_add(1).filter(|g| *g <= i64::MAX as u64).ok_or(Error::Unavailable { reason: "communication generation exhausted".into() })?;
    let publication = serde_json::from_str(&body).map_err(|e| Error::Unavailable { reason: format!("invalid saved communication: {e}") })?;
    let delivery = Delivery { token: Token { revision, generation }, epoch, publication };
    encoded(&delivery, budget, revision)?;
    tx.execute("UPDATE agent_mail SET disposition='dispatched',frozen=1,generation=?2,epoch=?3,terminal=NULL,source_generation=?4,source_connection=?5 WHERE revision=?1", params![number(revision)?, number(generation)?, number(epoch)?, number(source_generation)?, source_connection]).map_err(sql_error)?;
    tx.commit().map_err(sql_error)?;
    Ok(Some(delivery))
}

/// An uncertain attempt is replayed with the same token only after an actual connection change.
/// Store notifications and capacity generations alone never make this gate eligible.
pub async fn replay_at(store: &Store, revision: u64, epoch: u64, source_connection: String, budget: usize) -> Result<Option<Delivery>, Error> {
    if !valid_identity(&source_connection) { return Err(Error::Invalid { reason: "source connection identity is required".into() }); }
    store.call(move |c| Ok(replay_at_on(c, revision, epoch, &source_connection, budget))).await.map_err(store_error)?
}

fn replay_at_on(c: &mut Connection, revision: u64, epoch: u64, connection: &str, budget: usize) -> Result<Option<Delivery>, Error> {
    let tx = c.savepoint().map_err(sql_error)?;
    let saved: Option<(String, i64, Option<String>)> = tx.query_row("SELECT body,generation,source_connection FROM agent_mail WHERE revision=?1 AND epoch=?2 AND disposition IN ('dispatched','admitted')", params![number(revision)?, number(epoch)?], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().map_err(sql_error)?;
    let Some((body, generation, previous)) = saved else { return Ok(None) };
    if previous.as_deref() == Some(connection) { return Ok(None); }
    let publication = serde_json::from_str(&body).map_err(|e| Error::Unavailable { reason: format!("invalid saved communication: {e}") })?;
    let delivery = Delivery { token: Token { revision, generation: unsigned(generation)? }, epoch, publication };
    encoded(&delivery, budget, revision)?;
    tx.execute("UPDATE agent_mail SET source_connection=?2 WHERE revision=?1", params![number(revision)?, connection]).map_err(sql_error)?;
    tx.commit().map_err(sql_error)?;
    Ok(Some(delivery))
}

pub async fn mark_admitted(store: &Store, token: Token, epoch: u64) -> Result<Receipt, Error> {
    store.call(move |c| Ok(transition_on(c, token, Some(epoch), Disposition::Admitted, None))).await.map_err(store_error)?
}
pub async fn mark_waiting(store: &Store, token: Token) -> Result<Receipt, Error> {
    store.call(move |c| Ok(transition_on(c, token, None, Disposition::Waiting, None))).await.map_err(store_error)?
}
pub async fn mark_consumed(store: &Store, token: Token, epoch: u64) -> Result<Receipt, Error> {
    store.call(move |c| Ok(transition_on(c, token, Some(epoch), Disposition::Consumed, None))).await.map_err(store_error)?
}

pub async fn mark_disposed(store: &Store, token: Token, epoch: u64, disposition: super::admission::Disposition) -> Result<Receipt, Error> {
    if let super::admission::Disposition::Transferred { epoch: destination } = &disposition {
        let destination = *destination;
        let record = serde_json::to_string(&disposition).map_err(|e| Error::Unavailable { reason: e.to_string() })?;
        return store.call(move |c| Ok(transfer_on(c, token, epoch, destination, &record))).await.map_err(store_error)?;
    }
    let next = match &disposition {
        super::admission::Disposition::Cancelled { .. } => Disposition::Cancelled,
        super::admission::Disposition::Transferred { .. } => Disposition::Transferred,
        super::admission::Disposition::Consumed { .. } => return Err(Error::Invalid { reason: "consumption requires correlated provider evidence".into() }),
    };
    let terminal = serde_json::to_string(&disposition).map_err(|e| Error::Unavailable { reason: e.to_string() })?;
    store.call(move |c| Ok(transition_on(c, token, Some(epoch), next, Some(&terminal)))).await.map_err(store_error)?
}

/// The caller verifies destination body custody before this transition. A capacity event or an
/// epoch number alone is not that proof. The old receipt stays ACKable; the same frozen original
/// becomes eligible at its destination epoch without being called consumed or republished.
fn transfer_on(c: &mut Connection, token: Token, source_epoch: u64, destination_epoch: u64, record: &str) -> Result<Receipt, Error> {
    let stale = || Error::Stale { revision: token.revision, generation: token.generation };
    if destination_epoch <= source_epoch { return Err(Error::Invalid { reason: "destination custody epoch must advance".into() }); }
    let tx = c.savepoint().map_err(sql_error)?;
    let saved: Option<(String, i64, i64, String, Option<i64>)> = tx.query_row("SELECT id,revision,generation,disposition,epoch FROM agent_mail WHERE revision=?1", [number(token.revision)?], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().map_err(sql_error)?;
    let Some((id, revision, generation, state, epoch)) = saved else { return Err(stale()) };
    let prior: Option<(i64, i64, String)> = tx.query_row("SELECT from_epoch,to_epoch,terminal FROM agent_mail_transfers WHERE revision=?1 AND generation=?2 AND from_epoch=?3", params![revision, number(token.generation)?, number(source_epoch)?], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional().map_err(sql_error)?;
    if let Some((from, to, previous)) = prior {
        if from != number(source_epoch)? || to != number(destination_epoch)? || previous != record { return Err(stale()); }
        tx.commit().map_err(sql_error)?;
        return Ok(Receipt { id, revision: token.revision, generation: token.generation, disposition: Disposition::Transferred, repeat: true });
    }
    if unsigned(generation)? != token.generation || token.generation == 0 || epoch != Some(number(source_epoch)?) || !matches!(Disposition::parse(&state)?, Disposition::Dispatched | Disposition::Admitted) { return Err(stale()); }
    tx.execute("INSERT INTO agent_mail_transfers(revision,generation,from_epoch,to_epoch,terminal) VALUES(?1,?2,?3,?4,?5)", params![revision, generation, number(source_epoch)?, number(destination_epoch)?, record]).map_err(sql_error)?;
    tx.execute("UPDATE agent_mail SET disposition='waiting',frozen=1,epoch=?2,terminal=?3 WHERE revision=?1", params![revision, number(destination_epoch)?, record]).map_err(sql_error)?;
    tx.commit().map_err(sql_error)?;
    Ok(Receipt { id, revision: token.revision, generation: token.generation, disposition: Disposition::Transferred, repeat: false })
}

fn transition_on(c: &mut Connection, token: Token, epoch: Option<u64>, next: Disposition, terminal: Option<&str>) -> Result<Receipt, Error> {
    let tx = c.savepoint().map_err(sql_error)?;
    let saved: Option<(String, i64, i64, String, Option<i64>)> = tx.query_row("SELECT id,revision,generation,disposition,epoch FROM agent_mail WHERE revision=?1", [number(token.revision)?], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().map_err(sql_error)?;
    let stale = || Error::Stale { revision: token.revision, generation: token.generation };
    let Some((id, revision, generation, state, admitted_epoch)) = saved else { return Err(stale()) };
    if unsigned(generation)? != token.generation || token.generation == 0 { return Err(stale()); }
    let current = Disposition::parse(&state)?;
    let expected_epoch = epoch.map(number).transpose()?;
    if admitted_epoch.is_some() && expected_epoch.is_some() && admitted_epoch != expected_epoch { return Err(stale()); }
    if next == Disposition::Admitted && matches!(current, Disposition::Consumed | Disposition::Cancelled | Disposition::Transferred) {
        tx.commit().map_err(sql_error)?;
        return Ok(Receipt { id, revision: token.revision, generation: token.generation, disposition: current, repeat: true });
    }
    let allowed = match next {
        Disposition::Waiting => matches!(current, Disposition::Dispatched | Disposition::Waiting),
        Disposition::Admitted => matches!(current, Disposition::Dispatched | Disposition::Admitted),
        // A handoff event may beat its admission ACK; both name this exact frozen attempt.
        Disposition::Consumed => matches!(current, Disposition::Dispatched | Disposition::Admitted | Disposition::Consumed),
        Disposition::Cancelled | Disposition::Transferred => matches!(current, Disposition::Dispatched | Disposition::Admitted) || current == next,
        _ => false,
    };
    if !allowed { return Err(stale()); }
    let old_terminal: Option<String> = tx.query_row("SELECT terminal FROM agent_mail WHERE revision=?1", [revision], |r| r.get(0)).map_err(sql_error)?;
    if terminal.is_some() && old_terminal.is_some() && old_terminal.as_deref() != terminal { return Err(stale()); }
    if current == next {
        tx.commit().map_err(sql_error)?;
        return Ok(Receipt { id, revision: token.revision, generation: token.generation, disposition: current, repeat: true });
    }
    tx.execute("UPDATE agent_mail SET disposition=?2,epoch=COALESCE(epoch,?3),terminal=COALESCE(terminal,?5) WHERE revision=?1 AND generation=?4", params![revision, next.name(), expected_epoch, generation, terminal]).map_err(sql_error)?;
    tx.commit().map_err(sql_error)?;
    Ok(Receipt { id, revision: token.revision, generation: token.generation, disposition: next, repeat: current == next })
}

pub async fn history(store: &Store, owner: String, sender: String, recipient: String, status: Option<StatusKey>, after: u64, budget: usize) -> Result<Page<Publication>, Error> {
    store.read(move |c| Ok(history_on(c, &owner, &sender, &recipient, status.as_ref(), after, budget))).await.map_err(store_error)?
}

fn history_on(c: &Connection, owner: &str, sender: &str, recipient: &str, status: Option<&StatusKey>, after: u64, budget: usize) -> Result<Page<Publication>, Error> {
    let mut page = Page { items: Vec::new(), next: None };
    let mut bytes = 0;
    let mut q = c.prepare("SELECT id,revision,generation,disposition,body,terminal FROM agent_mail WHERE owner=?1 AND sender=?2 AND recipient=?3 AND revision>?4 AND (?5 IS NULL OR (status_key=?5 AND status_task IS ?6)) ORDER BY revision").map_err(sql_error)?;
    let mut rows = q.query(params![owner, sender, recipient, number(after)?, status.map(|s| s.key.as_str()), status.and_then(|s| s.task.as_deref())]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let receipt = from_receipt(row_receipt(row).map_err(sql_error)?, false)?;
        let revision = receipt.revision;
        let body: String = row.get(4).map_err(sql_error)?;
        let publication = serde_json::from_str(&body).map_err(|e| Error::Unavailable { reason: format!("invalid saved communication: {e}") })?;
        let terminal = row.get::<_, Option<String>>(5).map_err(sql_error)?.map(|v| serde_json::from_str(&v).map_err(|e| Error::Unavailable { reason: e.to_string() })).transpose()?;
        if !push_page(&mut page, &mut bytes, Publication { receipt, publication, terminal }, revision, budget)? { encoded(&page, budget, revision)?; return Ok(page); }
    }
    page.next = None;
    encoded(&page, budget, after)?;
    Ok(page)
}

/// Inspect one immutable envelope without requiring a near-frame body to fit beside history's
/// metadata. Objects expose bounded key pages, arrays their length, strings UTF-8 ranges; callers
/// address children by JSON pointer. This never emits chunks of an encoded JSON document.
pub async fn body(store: &Store, owner: String, sender: String, recipient: String, read: BodyRead, budget: usize) -> Result<BodyReply, Error> {
    store.read(move |c| Ok(body_on(c, &owner, &sender, &recipient, &read, budget))).await.map_err(store_error)?
}

fn body_on(c: &Connection, owner: &str, sender: &str, recipient: &str, read: &BodyRead, budget: usize) -> Result<BodyReply, Error> {
    let body: Option<String> = c.query_row("SELECT body FROM agent_mail WHERE owner=?1 AND sender=?2 AND recipient=?3 AND revision=?4", params![owner, sender, recipient, number(read.revision)?], |r| r.get(0)).optional().map_err(sql_error)?;
    let invalid = || Error::Invalid { reason: "no communication value at this path/cursor".into() };
    let Some(body) = body else { return Err(invalid()) };
    if read.path == "/$transfers" {
        let mut wrapper = BodyReply { revision: read.revision, path: read.path.clone(), offset: read.offset, node: BodyNode::Transfers { entries: Vec::new() }, next: None };
        encoded(&wrapper, budget, read.revision)?;
        let mut entries = Vec::new(); let mut bytes = 0usize; let mut next = None;
        let mut q = c.prepare("SELECT cursor,generation,from_epoch,to_epoch,terminal FROM agent_mail_transfers WHERE revision=?1 AND cursor>?2 ORDER BY cursor").map_err(sql_error)?;
        let mut rows = q.query(params![number(read.revision)?, number(u64::try_from(read.offset).map_err(|_| invalid())?)?]).map_err(sql_error)?;
        let transfer = |row: &rusqlite::Row<'_>| -> Result<(usize, Transfer), Error> {
            let cursor = usize::try_from(unsigned(row.get(0).map_err(sql_error)?)?).map_err(|_| invalid())?;
            let record: String = row.get(4).map_err(sql_error)?;
            let entry = Transfer { generation: unsigned(row.get(1).map_err(sql_error)?)?, from_epoch: unsigned(row.get(2).map_err(sql_error)?)?, to_epoch: unsigned(row.get(3).map_err(sql_error)?)?, disposition: serde_json::from_str(&record).map_err(|e| Error::Unavailable { reason: e.to_string() })? };
            Ok((cursor, entry))
        };
        let mut pending = rows.next().map_err(sql_error)?.map(transfer).transpose()?;
        while let Some((cursor, entry)) = pending.take() {
            pending = rows.next().map_err(sql_error)?.map(transfer).transpose()?;
            // The actual body envelope owns the cursor. In particular, `null` is longer
            // than a one-digit continuation, so a generic page's maximum reserve is not exact.
            wrapper.next = pending.as_ref().map(|_| cursor);
            let overhead = encoded(&wrapper, budget, read.revision)?.len();
            let size = match encoded(&entry, budget, read.revision) {
                Ok(value) => value.len(),
                Err(Error::TooLarge { .. }) if !entries.is_empty() => break,
                Err(error) => return Err(error),
            };
            let candidate = bytes.checked_add(size).and_then(|n| n.checked_add(usize::from(!entries.is_empty())));
            if candidate.and_then(|n| n.checked_add(overhead)).is_none_or(|n| n > budget) {
                if entries.is_empty() { return Err(Error::TooLarge { revision: read.revision, budget }); }
                break;
            }
            bytes = candidate.ok_or(Error::TooLarge { revision: read.revision, budget })?;
            entries.push(entry);
            next = wrapper.next;
        }
        let reply = BodyReply { revision: read.revision, path: read.path.clone(), offset: read.offset, node: BodyNode::Transfers { entries }, next };
        encoded(&reply, budget, read.revision)?;
        return Ok(reply);
    }
    let value: Value = serde_json::from_str(&body).map_err(|e| Error::Unavailable { reason: e.to_string() })?;
    let value = value.pointer(&read.path).ok_or_else(invalid)?;
    let reply = |node, next| BodyReply { revision: read.revision, path: read.path.clone(), offset: read.offset, node, next };
    let result = match value {
        Value::String(text) => {
            if !text.is_char_boundary(read.offset) { return Err(invalid()); }
            let tail = text.get(read.offset..).ok_or_else(invalid)?;
            #[derive(Serialize)]
            #[serde(tag = "type", rename_all = "snake_case")]
            enum TextNode<'a> { Text { text: &'a str } }
            #[derive(Serialize)]
            struct TextReply<'a> { revision: u64, path: &'a str, offset: usize, node: TextNode<'a>, next: Option<usize> }
            let fits = |end: usize| -> Result<bool, Error> {
                let Some(part) = tail.get(..end) else { return Err(invalid()) };
                let next = (end < tail.len()).then(|| read.offset + end);
                match encoded(&TextReply { revision: read.revision, path: &read.path, offset: read.offset, node: TextNode::Text { text: part }, next }, budget, read.revision) {
                    Ok(_) => Ok(true), Err(Error::TooLarge { .. }) => Ok(false), Err(error) => Err(error),
                }
            };
            // A final null cursor can be shorter than a numeric next cursor, so test the whole
            // remaining string before binary searching the monotone partial-range envelopes.
            let mut low = if fits(tail.len())? { tail.len() } else { 0 };
            let mut high = tail.len();
            while low < high {
                let mid = low + (high - low).div_ceil(2);
                let mut boundary = mid;
                while boundary > low && !tail.is_char_boundary(boundary) { boundary -= 1; }
                if boundary == low {
                    let advance = tail.get(low..).and_then(|s| s.chars().next()).map_or(0, char::len_utf8);
                    let boundary = low.checked_add(advance).ok_or_else(invalid)?;
                    if boundary > high || !fits(boundary)? { high = low; } else { low = boundary; }
                } else if fits(boundary)? { low = boundary; } else { high = boundary - 1; }
            }
            if !tail.is_empty() && low == 0 { return Err(Error::TooLarge { revision: read.revision, budget }); }
            let part = tail.get(..low).ok_or_else(invalid)?;
            reply(BodyNode::Text { text: part.to_owned() }, (low < tail.len()).then(|| read.offset + low))
        }
        Value::Object(map) => {
            if read.offset > map.len() { return Err(invalid()); }
            let mut keys = Vec::new();
            let mut next = None;
            for key in map.keys().skip(read.offset) {
                keys.push(key.clone());
                let end = read.offset.checked_add(keys.len()).ok_or_else(invalid)?;
                let candidate = reply(BodyNode::Keys { keys }, (end < map.len()).then_some(end));
                let BodyNode::Keys { keys: mut kept } = candidate.node else { return Err(invalid()) };
                // Only string keys are copied; full values stay in their bounded source envelope.
                #[derive(Serialize)]
                struct KeysReply<'a> { revision: u64, path: &'a str, offset: usize, node: KeysNode<'a>, next: Option<usize> }
                #[derive(Serialize)]
                #[serde(tag = "type", rename_all = "snake_case")]
                enum KeysNode<'a> { Keys { keys: &'a [String] } }
                if encoded(&KeysReply { revision: read.revision, path: &read.path, offset: read.offset, node: KeysNode::Keys { keys: &kept }, next: candidate.next }, budget, read.revision).is_err() {
                    kept.pop();
                    if kept.is_empty() { return Err(Error::TooLarge { revision: read.revision, budget }); }
                    next = Some(read.offset + kept.len()); keys = kept; break;
                }
                next = candidate.next; keys = kept;
            }
            reply(BodyNode::Keys { keys }, next)
        }
        Value::Array(values) => {
            if read.offset != 0 { return Err(invalid()); }
            reply(BodyNode::Array { len: values.len() }, None)
        }
        value => {
            if read.offset != 0 { return Err(invalid()); }
            reply(BodyNode::Value { value: value.clone() }, None)
        }
    };
    encoded(&result, budget, read.revision)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> anyhow::Result<Connection> { let c = Connection::open_in_memory()?; c.execute_batch(include_str!("../store/migrations/20261111-0100_agent_mail.sql"))?; Ok(c) }
    fn message(id: &str, status: Option<&str>, text: &str) -> Publish {
        Publish { owner: "owner".into(), sender: "sender".into(), recipient: "recipient".into(), id: id.into(), command: json!({"type":"send","id":id,"text":text}), provenance: Some(json!({"original":id})), status: status.map(|key| StatusKey { task: None, key: key.into() }) }
    }
    fn put(c: &mut Connection, p: &Publish) -> Result<Receipt, Error> { publish_on(c, p, u64::MAX, u64::MAX) }
    fn all(c: &Connection) -> Result<Page<Publication>, Error> { history_on(c, "owner", "sender", "recipient", None, 0, super::super::state_view::FRAME) }

    #[test]
    fn preaccept_failure_cannot_turn_its_own_reservation_release_into_new_retry_credit() -> anyhow::Result<()> {
        use std::sync::Arc;
        use crate::harness::{admission::{self, Capacity, FlowEvent}, core::Update, host::Host, model::Command, session::Spec};

        struct Close { host: Arc<Host>, id: String }
        impl Drop for Close { fn drop(&mut self) { let _ = self.host.end(&self.id); } }

        async fn flow(rx: &async_channel::Receiver<(String, Update)>, id: &str, matches: impl Fn(&FlowEvent) -> bool) -> anyhow::Result<(FlowEvent, bool)> {
            crate::rt::timeout(crate::harness::registry::ANSWER_WAIT, async {
                let mut accepted_ack = false;
                loop {
                    let (source, update) = rx.recv().await?;
                    if source != id { continue; }
                    match update {
                        Update::Inbox { said } if said.answer.is_ok() => accepted_ack = true,
                        Update::MailFlow { event } if matches(&event) => return Ok((*event, accepted_ack)),
                        _ => {}
                    }
                }
            }).await.map_err(|_| anyhow::anyhow!("the bounded source admission event did not arrive"))?
        }

        fn capacity(event: FlowEvent) -> Capacity {
            match event {
                FlowEvent::Accepted { capacity, .. } | FlowEvent::Consumed { capacity, .. } | FlowEvent::Waiting { capacity, .. }
                | FlowEvent::Capacity { capacity } | FlowEvent::Disposed { capacity, .. } | FlowEvent::Fault { capacity, .. } => capacity,
            }
        }

        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let id = crate::util::new_id();
            let mut original = message("original", None, "immutable words retained by the sender");
            original.recipient = id.clone();
            let command: Command = serde_json::from_value(original.command.clone())?;
            let body_bytes = admission::command_bytes(&command).map_err(|error| anyhow::anyhow!("fixture body size: {error:?}"))?;
            let (tx, rx) = async_channel::unbounded();
            let records = Arc::new(kiln_seams::records::SimRecords::new());
            let host = Host::with_records(tx, None, crate::harness::host::Limits::default(), Arc::new(kiln_seams::clock::SimClock::new()), Some(records));
            // The session's real authority admits this original representation, but cannot also
            // allocate its counted transaction/driver representation. No Capacity is forged.
            host.set_input_budget(body_bytes);
            let _close = Close { host: Arc::clone(&host), id: id.clone() };
            let source = host.open(Some(id.clone()), Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(dir.path().join("no-provider-process").to_string_lossy().into_owned()), ..Spec::default() })?;
            let epoch = source.epoch();
            let (ready, _) = flow(&rx, &id, |event| matches!(event, FlowEvent::Capacity { capacity } if capacity.epoch == epoch && capacity.target > 0)).await?;
            assert!(capacity(ready).admitted == 0);
            let mut ledger = db()?;
            let publication = put(&mut ledger, &original)?;
            assert!(host.command(&id, Command::MailPeers { epoch, peers: vec!["sender".into()], demand: vec![admission::Demand { peer: "sender".into(), revision: publication.revision }] }));
            let (offered, _) = flow(&rx, &id, |event| matches!(event, FlowEvent::Capacity { capacity } if capacity.eligible_peer.as_deref() == Some("sender"))).await?;
            let offered = capacity(offered);
            let first = claim_at_on(&mut ledger, publication.revision, epoch, offered.retry_permission(), Some("one-actual-connection"), crate::harness::state_view::FRAME)?.ok_or_else(|| anyhow::anyhow!("the first publication was not claimable"))?;
            assert!(host.command(&id, Command::Admit { peer: "sender".into(), token: first.token, epoch, generation: offered.feedback_generation.unwrap_or(offered.generation), peers: vec!["sender".into()], command: Box::new(command.clone()) }));
            // The owner publishes Flow through Core; its direct Inbox answer may arrive first.
            let (waiting, accepted_ack, refused) = crate::rt::timeout(crate::harness::registry::ANSWER_WAIT, async {
                let mut waiting = None;
                let mut refused = None;
                let mut accepted_ack = false;
                loop {
                    let (source, update) = rx.recv().await?;
                    if source != id { continue; }
                    match update {
                        Update::Inbox { said } => {
                            accepted_ack |= said.answer.is_ok();
                            if said.message == "original" && refused.is_none() { refused = Some(said.answer); }
                        }
                        Update::MailFlow { event } if matches!(&*event, FlowEvent::Waiting { token, .. } if *token == first.token) => {
                            if waiting.is_none() { waiting = Some(*event); }
                        }
                        _ => {}
                    }
                    match (waiting.take(), refused.take()) {
                        (Some(waiting), Some(refused)) => return Ok::<_, anyhow::Error>((waiting, accepted_ack, refused)),
                        (saved_waiting, saved_refused) => { waiting = saved_waiting; refused = saved_refused; }
                    }
                }
            }).await.map_err(|_| anyhow::anyhow!("the bounded source admission and refusal events did not arrive"))??;
            assert!(!accepted_ack, "a failed preaccept journal reservation cannot acknowledge custody");
            assert!(matches!(&waiting, FlowEvent::Waiting { cause: admission::WaitCause::SourceUnavailable, .. }), "the original must fit; this fixture must reach the actual journal reservation failure");
            assert!(matches!(&refused, Err(crate::harness::inbox::Refusal::Unavailable { why, .. }) if why == "inbox transaction has no physical byte authority"), "the actual source must refuse its counted journal allocation before any custody ACK");
            let waiting = capacity(waiting);
            let inbox = host.inbox().ok_or_else(|| anyhow::anyhow!("fixture source inbox missing"))?;
            assert!(inbox.pending(&id, "original").await?.is_none());
            let saved: String = ledger.query_row("SELECT body FROM agent_mail WHERE revision=?1", [number(publication.revision)?], |row| row.get(0))?;
            assert_eq!(serde_json::from_str::<Publish>(&saved)?, original, "preaccept failure cannot replace the durable sender original");
            // This is the same real transition used by mail_delivery::flow for preaccept Waiting.
            transition_on(&mut ledger, first.token, None, Disposition::Waiting, None)?;
            let input = source.core.lock().input.clone().ok_or_else(|| anyhow::anyhow!("fixture source input missing"))?;
            let own_buffer = input.reserve_buffer(1).map_err(|error| anyhow::anyhow!("fixture self buffer: {error:?}"))?.ok_or_else(|| anyhow::anyhow!("fixture self buffer had no room"))?;
            drop(own_buffer);
            host.set_input_budget(body_bytes); // unchanged real authority is not another grant
            assert!(host.command(&id, Command::MailPeers { epoch, peers: vec!["sender".into()], demand: Vec::new() }));
            let (withdrawn, _) = flow(&rx, &id, |event| matches!(event, FlowEvent::Capacity { capacity } if capacity.generation > waiting.generation && capacity.eligible_peer.is_none())).await?;
            let withdrawn = capacity(withdrawn);
            assert_eq!(withdrawn.retry_permission(), waiting.retry_permission());
            assert!(host.command(&id, Command::MailPeers { epoch, peers: vec!["sender".into()], demand: vec![admission::Demand { peer: "sender".into(), revision: publication.revision }] }));
            let (bookkeeping, _) = flow(&rx, &id, |event| matches!(event, FlowEvent::Capacity { capacity } if capacity.generation > withdrawn.generation && capacity.eligible_peer.as_deref() == Some("sender"))).await?;
            let bookkeeping = capacity(bookkeeping);
            assert_eq!(bookkeeping.retry_permission(), waiting.retry_permission(), "demand withdrawal/reappearance and own buffers cannot mint retry permission");
            let self_retry = claim_at_on(&mut ledger, publication.revision, epoch, bookkeeping.retry_permission(), Some("one-actual-connection"), crate::harness::state_view::FRAME)?;
            assert!(self_retry.is_none(), "a source's own failed reservation/release must not authorize the same publication again");

            // Positive control: an actual increase of its bound produces the owner's normal
            // capacity event and permits one retry. The test dispatches at most two attempts.
            let additional = kiln_seams::records::VALUE_MAX.checked_mul(2).and_then(|bytes| bytes.checked_add(body_bytes)).ok_or_else(|| anyhow::anyhow!("fixture budget overflowed"))?;
            host.set_input_budget(additional);
            let (external, _) = flow(&rx, &id, |event| matches!(event, FlowEvent::Capacity { capacity } if capacity.retry_permission() > waiting.retry_permission() && capacity.eligible_peer.as_deref() == Some("sender"))).await?;
            let external = capacity(external);
            let retry = claim_at_on(&mut ledger, publication.revision, epoch, external.retry_permission(), Some("one-actual-connection"), crate::harness::state_view::FRAME)?.ok_or_else(|| anyhow::anyhow!("actual external byte authority did not permit retry"))?;
            assert_eq!(retry.token.revision, first.token.revision);
            assert_eq!(retry.token.generation, first.token.generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("fixture token overflowed"))?);
            assert!(host.command(&id, Command::Admit { peer: "sender".into(), token: retry.token, epoch, generation: external.feedback_generation.unwrap_or(external.generation), peers: vec!["sender".into()], command: Box::new(command.clone()) }));
            let _ = flow(&rx, &id, |event| matches!(event, FlowEvent::Accepted { token, .. } if *token == retry.token)).await?;
            let pending = inbox.pending(&id, "original").await?.ok_or_else(|| anyhow::anyhow!("successful source acceptance did not retain its original"))?;
            assert_eq!(pending.command.as_ref(), Some(&command));
            Ok(())
        })
    }

    #[test]
    fn original_status_revisions_survive_pending_coalescing() -> anyhow::Result<()> {
        let mut c = db()?;
        let first = message("one", Some("build"), "first 日本語 \\\"");
        let latest = message("two", Some("build"), "second");
        let a = put(&mut c, &first)?;
        let b = put(&mut c, &latest)?;
        let history = all(&c)?;
        assert_eq!(history.items.len(), 2);
        assert_eq!(history.items.first().map(|i| &i.publication), Some(&first));
        assert_eq!(history.items.first().map(|i| i.receipt.disposition), Some(Disposition::Superseded));
        assert_eq!(pending_on(&c, "recipient", 0, 4096)?.items.iter().map(|r| r.revision).collect::<Vec<_>>(), vec![b.revision]);
        assert!(claim_on(&mut c, a.revision, 7, 4096)?.is_none());
        Ok(())
    }

    #[test]
    fn freeze_holds_one_latest_successor_until_consumed() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = put(&mut c, &message("one", Some("build"), "first"))?;
        let d = claim_on(&mut c, a.revision, 7, 4096)?.ok_or_else(|| anyhow::anyhow!("missing delivery"))?;
        let b = put(&mut c, &message("two", Some("build"), "second"))?;
        let z = put(&mut c, &message("three", Some("build"), "third"))?;
        assert!(claim_on(&mut c, b.revision, 7, 4096)?.is_none());
        assert!(claim_on(&mut c, z.revision, 7, 4096)?.is_none());
        transition_on(&mut c, d.token, Some(7), Disposition::Admitted, None)?;
        transition_on(&mut c, d.token, Some(7), Disposition::Consumed, None)?;
        assert!(claim_on(&mut c, z.revision, 7, 4096)?.is_some());
        assert_eq!(all(&c)?.items.len(), 3);
        Ok(())
    }

    #[test]
    fn ordinary_messages_and_distinct_stream_scopes_do_not_coalesce() -> anyhow::Result<()> {
        let mut c = db()?;
        put(&mut c, &message("one", None, "instruction"))?;
        put(&mut c, &message("two", None, "result"))?;
        put(&mut c, &message("three", Some("build"), "one task"))?;
        let mut other = message("four", Some("build"), "another task");
        if let Some(status) = other.status.as_mut() { status.task = Some("task".into()); }
        put(&mut c, &other)?;
        other.id = "five".into(); other.sender = "other".into();
        put(&mut c, &other)?;
        assert_eq!(pending_on(&c, "recipient", 0, 4096)?.items.len(), 5);
        Ok(())
    }

    #[test]
    fn immutable_retry_and_capacity_refusal_preserve_custody() -> anyhow::Result<()> {
        let mut c = db()?;
        let p = message("one", Some("build"), "first");
        let a = put(&mut c, &p)?;
        let repeat = publish_on(&mut c, &p, 0, 0)?;
        assert_eq!(repeat.revision, a.revision);
        assert!(repeat.repeat);
        assert!(matches!(put(&mut c, &message("one", Some("build"), "changed")), Err(Error::Reused { .. })));
        assert!(matches!(publish_on(&mut c, &message("two", Some("build"), "replacement"), 0, 0), Err(Error::Capacity { .. })));
        let history = all(&c)?;
        assert_eq!(history.items.len(), 1);
        assert_eq!(history.items.first().map(|r| r.receipt.disposition), Some(Disposition::Waiting));
        Ok(())
    }

    #[test]
    fn stale_attempt_and_epoch_cannot_release_newer_work() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = put(&mut c, &message("one", Some("build"), "first"))?;
        let first = claim_on(&mut c, a.revision, 7, 4096)?.ok_or_else(|| anyhow::anyhow!("missing delivery"))?;
        transition_on(&mut c, first.token, None, Disposition::Waiting, None)?;
        let second = claim_on(&mut c, a.revision, 7, 4096)?.ok_or_else(|| anyhow::anyhow!("missing retry"))?;
        assert!(matches!(transition_on(&mut c, first.token, Some(7), Disposition::Consumed, None), Err(Error::Stale { .. })));
        transition_on(&mut c, second.token, Some(7), Disposition::Admitted, None)?;
        assert!(matches!(transition_on(&mut c, second.token, Some(1), Disposition::Consumed, None), Err(Error::Stale { .. })));
        let b = put(&mut c, &message("two", Some("build"), "next"))?;
        transition_on(&mut c, second.token, Some(7), Disposition::Consumed, None)?;
        let next = claim_on(&mut c, b.revision, 7, 4096)?.ok_or_else(|| anyhow::anyhow!("missing successor"))?;
        transition_on(&mut c, second.token, Some(7), Disposition::Consumed, None)?;
        assert_eq!(next.publication.id, "two");
        assert!(claim_on(&mut c, a.revision, 7, 4096)?.is_none());
        Ok(())
    }

    #[test]
    fn history_and_pending_pages_fit_exact_encoded_budget() -> anyhow::Result<()> {
        let mut c = db()?;
        for n in 0..12 { put(&mut c, &message(&format!("m{n}"), None, "日本語\\\""))?; }
        let first = pending_on(&c, "recipient", 0, 240)?;
        assert!(serde_json::to_vec(&first)?.len() <= 240);
        let next = first.next.ok_or_else(|| anyhow::anyhow!("page did not continue"))?;
        let following = pending_on(&c, "recipient", next, 240)?;
        assert!(following.items.first().is_some_and(|r| r.revision > next));
        let history = history_on(&c, "owner", "sender", "recipient", None, 0, 700)?;
        assert!(serde_json::to_vec(&history)?.len() <= 700);
        assert!(history.next.is_some());
        assert!(matches!(history_on(&c, "owner", "sender", "recipient", None, 0, 1), Err(Error::TooLarge { .. })));
        Ok(())
    }

    #[test]
    fn consumption_before_admission_ack_never_regresses() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = put(&mut c, &message("one", Some("build"), "first"))?;
        let first = claim_on(&mut c, a.revision, 9, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        transition_on(&mut c, first.token, Some(9), Disposition::Consumed, None)?;
        let late = transition_on(&mut c, first.token, Some(9), Disposition::Admitted, None)?;
        assert_eq!(late.disposition, Disposition::Consumed);
        assert!(late.repeat);
        let b = put(&mut c, &message("two", Some("build"), "after consumed"))?;
        assert!(claim_on(&mut c, b.revision, 9, 4096)?.is_some());
        assert!(matches!(transition_on(&mut c, first.token, Some(8), Disposition::Consumed, None), Err(Error::Stale { .. })));
        Ok(())
    }

    #[test]
    fn logical_authority_includes_server_custody_and_refusal_does_not_supersede() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = publish_on(&mut c, &message("one", Some("build"), "first"), u64::MAX, 1)?;
        let b = publish_on(&mut c, &message("two", Some("build"), "latest pending"), u64::MAX, 1)?;
        assert_ne!(a.revision, b.revision);
        assert!(matches!(publish_on(&mut c, &message("ordinary", None, "distinct result"), u64::MAX, 1), Err(Error::Backpressure { pending: 2, authority: 1 })));
        let frozen = claim_on(&mut c, b.revision, 3, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        assert!(matches!(publish_on(&mut c, &message("three", Some("build"), "successor"), u64::MAX, 1), Err(Error::Backpressure { .. })));
        transition_on(&mut c, frozen.token, None, Disposition::Waiting, None)?;
        assert!(matches!(publish_on(&mut c, &message("four", Some("build"), "must not supersede frozen"), u64::MAX, 1), Err(Error::Backpressure { .. })));
        assert_eq!(all(&c)?.items.len(), 2);
        assert_eq!(pending_on(&c, "recipient", 0, 4096)?.items.first().map(|r| r.id.as_str()), Some("two"));
        Ok(())
    }

    /// Each conversation's queue is its own: what one recipient holds takes nothing of another's
    /// room, and no count is kept over every conversation's mail. The room is one original's
    /// on disk; each recipient's bound is its own pending depth.
    #[test]
    fn one_conversations_backlog_takes_no_other_conversations_room() -> anyhow::Result<()> {
        let mut c = db()?;
        let to = |recipient: &str, id: &str| Publish { recipient: recipient.into(), ..message(id, None, "words of one size") };
        let size = u64::try_from(encoded(&to("a", "a1"), super::super::state_view::FRAME, 0)?.len())?;
        publish_on(&mut c, &to("a", "a1"), size, 2)?;
        publish_on(&mut c, &to("a", "a2"), size, 2)?;
        assert!(matches!(publish_on(&mut c, &to("a", "a3"), size, 2), Err(Error::Backpressure { pending: 3, authority: 2 })), "a's own depth holds its third");
        publish_on(&mut c, &to("b", "b1"), size, 2)?;
        publish_on(&mut c, &to("b", "b2"), size, 2)?;
        assert!(matches!(publish_on(&mut c, &to("b", "b3"), size - 1, 2), Err(Error::Capacity { .. })), "an original the disk has no room for still waits");
        Ok(())
    }

    #[test]
    fn large_history_has_scoped_utf8_body_ranges() -> anyhow::Result<()> {
        let mut c = db()?;
        let text = "日本語 🦀 \\\"\n".repeat(3000);
        let a = put(&mut c, &message("one", Some("build"), &text))?;
        assert!(matches!(history_on(&c, "owner", "sender", "recipient", None, 0, 256), Err(Error::TooLarge { revision, .. }) if revision == a.revision));
        let mut offset = 0;
        let mut restored = String::new();
        loop {
            let read = BodyRead { revision: a.revision, path: "/command/text".into(), offset };
            let reply = body_on(&c, "owner", "sender", "recipient", &read, 256)?;
            assert!(serde_json::to_vec(&reply)?.len() <= 256);
            let BodyNode::Text { text } = reply.node else { anyhow::bail!("not a text range") };
            restored.push_str(&text);
            match reply.next { Some(next) => { assert!(next > offset); offset = next; }, None => break }
        }
        assert_eq!(restored, text);
        let wrong_owner = body_on(&c, "other", "sender", "recipient", &BodyRead { revision: a.revision, path: "/command/text".into(), offset: 0 }, 256);
        assert!(matches!(wrong_owner, Err(Error::Invalid { .. })));
        assert!(matches!(body_on(&c, "owner", "sender", "recipient", &BodyRead { revision: a.revision, path: "/command/text".into(), offset: 1 }, 256), Err(Error::Invalid { .. })));
        Ok(())
    }

    #[test]
    fn cancellation_is_retained_without_claiming_consumption() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = put(&mut c, &message("one", None, "original"))?;
        let delivery = claim_on(&mut c, a.revision, 4, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        let reason = serde_json::to_string(&super::super::admission::Disposition::Cancelled { reason: super::super::admission::CancelReason::User })?;
        let terminal = transition_on(&mut c, delivery.token, Some(4), Disposition::Cancelled, Some(&reason))?;
        assert_eq!(terminal.disposition, Disposition::Cancelled);
        assert!(pending_on(&c, "recipient", 0, 4096)?.items.is_empty());
        assert_eq!(all(&c)?.items.first().map(|i| &i.publication.command), Some(&json!({"type":"send","id":"one","text":"original"})));
        assert_eq!(all(&c)?.items.first().and_then(|i| i.terminal.as_ref()), Some(&serde_json::from_str::<Value>(&reason)?));
        assert!(matches!(transition_on(&mut c, delivery.token, Some(4), Disposition::Consumed, None), Err(Error::Stale { .. })));
        Ok(())
    }

    #[test]
    fn transfer_fences_old_attempt_and_requeues_same_original_at_destination() -> anyhow::Result<()> {
        let mut c = db()?;
        let p = message("one", Some("build"), "original");
        let a = put(&mut c, &p)?;
        let first = claim_on(&mut c, a.revision, 4, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        let record = serde_json::to_string(&super::super::admission::Disposition::Transferred { epoch: 5 })?;
        let receipt = transfer_on(&mut c, first.token, 4, 5, &record)?;
        assert_eq!(receipt.disposition, Disposition::Transferred);
        assert!(claim_on(&mut c, a.revision, 4, 4096).is_err());
        let next = claim_on(&mut c, a.revision, 5, 4096)?.ok_or_else(|| anyhow::anyhow!("missing destination claim"))?;
        assert_eq!(next.publication, p);
        assert!(next.token.generation > first.token.generation);
        assert!(transfer_on(&mut c, first.token, 4, 5, &record)?.repeat);
        assert!(matches!(transition_on(&mut c, first.token, Some(4), Disposition::Consumed, None), Err(Error::Stale { .. })));
        let history = body_on(&c, "owner", "sender", "recipient", &BodyRead { revision: a.revision, path: "/$transfers".into(), offset: 0 }, 512)?;
        assert!(matches!(history.node, BodyNode::Transfers { entries } if entries.len() == 1));
        Ok(())
    }

    #[test]
    fn fresh_retry_proof_does_not_replace_first_accepted_original() -> anyhow::Result<()> {
        let mut c = db()?;
        let p = message("one", Some("build"), "same command");
        let a = put(&mut c, &p)?;
        let mut retry = p.clone(); retry.provenance = Some(json!({"fresh_transport_proof": true}));
        let answer = publish_on(&mut c, &retry, 0, 0)?;
        assert_eq!(answer.revision, a.revision);
        assert!(answer.repeat);
        assert_eq!(all(&c)?.items.first().map(|r| &r.publication), Some(&p));
        Ok(())
    }

    #[test]
    fn retries_require_new_source_events_and_replay_preserves_uncertain_token() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = put(&mut c, &message("one", None, "original"))?;
        let first = claim_at_on(&mut c, a.revision, 3, 8, Some("connection-a"), 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        transition_on(&mut c, first.token, None, Disposition::Waiting, None)?;
        let changes = c.total_changes();
        assert!(transition_on(&mut c, first.token, None, Disposition::Waiting, None)?.repeat);
        assert_eq!(c.total_changes(), changes);
        assert!(claim_at_on(&mut c, a.revision, 3, 8, Some("connection-a"), 4096)?.is_none());
        assert_eq!(c.total_changes(), changes);
        let next = claim_at_on(&mut c, a.revision, 3, 9, Some("connection-a"), 4096)?.ok_or_else(|| anyhow::anyhow!("changed capacity event did not permit claim"))?;
        assert!(next.token.generation > first.token.generation);
        assert!(replay_at_on(&mut c, a.revision, 3, "connection-a", 4096)?.is_none());
        let replay = replay_at_on(&mut c, a.revision, 3, "connection-b", 4096)?.ok_or_else(|| anyhow::anyhow!("actual reconnect did not permit replay"))?;
        assert_eq!(replay.token, next.token);
        assert_eq!(replay.publication, next.publication);
        let changes = c.total_changes();
        assert!(replay_at_on(&mut c, a.revision, 3, "connection-b", 4096)?.is_none());
        assert_eq!(c.total_changes(), changes);
        Ok(())
    }

    #[test]
    fn source_projection_binds_original_sender_id_attempt_and_waiting_disposition() -> anyhow::Result<()> {
        let mut c = db()?;
        let mut p = message("original-id", None, "private original words");
        p.sender = super::super::ops::key_scope(&crate::authz::Caller::owner());
        let a = put(&mut c, &p)?;
        let claimed = claim_on(&mut c, a.revision, 7, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        let projection = projection_on(&c, "recipient", claimed.token, 7)?.ok_or_else(|| anyhow::anyhow!("missing committed projection"))?;
        assert_eq!(projection.publication.id, p.id);
        assert_eq!(projection.publication.revision, a.revision);
        assert_eq!(projection.publication.generation, claimed.token.generation);
        assert_eq!(projection.publication.disposition, Disposition::Dispatched);
        assert_eq!(projection.mail_scope, crate::util::sha256_hex(format!("mail-scope-v1:{}", super::super::ops::key_scope(&crate::authz::Caller::owner())).as_bytes()));
        let wire = serde_json::to_string(&projection)?;
        assert!(!wire.contains("private original words") && !wire.contains("\"sender\"") && !wire.contains("\"owner\""));
        assert!(projection_on(&c, "wrong-recipient", claimed.token, 7)?.is_none());
        assert!(projection_on(&c, "recipient", claimed.token, 8)?.is_none());
        assert!(projection_on(&c, "recipient", Token { revision: a.revision, generation: 0 }, 7)?.is_none());
        transition_on(&mut c, claimed.token, None, Disposition::Waiting, None)?;
        let waiting = projection_on(&c, "recipient", claimed.token, 7)?.ok_or_else(|| anyhow::anyhow!("retained Waiting projection missing"))?;
        assert_eq!(waiting.publication.disposition, Disposition::Waiting);
        Ok(())
    }

    #[test]
    fn transferred_projection_still_binds_old_token_after_destination_claim() -> anyhow::Result<()> {
        let mut c = db()?;
        let a = put(&mut c, &message("one", None, "unchanged original"))?;
        let old = claim_on(&mut c, a.revision, 4, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        let record = serde_json::to_string(&super::super::admission::Disposition::Transferred { epoch: 5 })?;
        transfer_on(&mut c, old.token, 4, 5, &record)?;
        let destination = claim_on(&mut c, a.revision, 5, 4096)?.ok_or_else(|| anyhow::anyhow!("missing destination claim"))?;
        let projection = projection_on(&c, "recipient", old.token, 4)?.ok_or_else(|| anyhow::anyhow!("old transfer receipt disappeared"))?;
        assert_eq!(projection.publication.disposition, Disposition::Transferred);
        assert_eq!(projection.publication.generation, old.token.generation);
        assert_ne!(projection.publication.generation, destination.token.generation);
        assert_eq!(projection.mail_scope, scope("sender"));
        Ok(())
    }

    #[test]
    fn bodyless_receipt_requires_complete_identity_and_keeps_exact_current_custody() -> anyhow::Result<()> {
        let mut c = db()?;
        let p = message("original", None, "original body remains private");
        let published = put(&mut c, &p)?;
        let waiting = receipt_on(&c, "owner", "sender", "recipient", "original")?.ok_or_else(|| anyhow::anyhow!("missing custody receipt"))?;
        assert_eq!(waiting.id, p.id);
        assert_eq!(waiting.revision, published.revision);
        assert_eq!(waiting.disposition, Disposition::Waiting);
        for (owner, sender, recipient, id) in [("wrong", "sender", "recipient", "original"), ("owner", "wrong", "recipient", "original"), ("owner", "sender", "wrong", "original"), ("owner", "sender", "recipient", "wrong")] {
            assert!(receipt_on(&c, owner, sender, recipient, id)?.is_none());
        }
        assert!(matches!(receipt_on(&c, "owner", "sender", "recipient", ""), Err(Error::Invalid { .. })));
        let delivery = claim_on(&mut c, published.revision, 4, 4096)?.ok_or_else(|| anyhow::anyhow!("missing claim"))?;
        transition_on(&mut c, delivery.token, Some(4), Disposition::Consumed, None)?;
        let consumed = receipt_on(&c, "owner", "sender", "recipient", "original")?.ok_or_else(|| anyhow::anyhow!("missing consumed receipt"))?;
        assert_eq!(consumed.generation, delivery.token.generation);
        assert_eq!(consumed.disposition, Disposition::Consumed);
        let wire = serde_json::to_string(&consumed)?;
        assert!(!wire.contains("original body remains private"));
        Ok(())
    }

    #[test]
    fn waiting_peer_demand_excludes_admitted_only_but_keeps_frozen_waiting() -> anyhow::Result<()> {
        let mut c=db()?;
        let mut a=message("one",None,"waiting");a.sender="sender-a".into();let a=put(&mut c,&a)?;
        let mut b=message("two",None,"already admitted");b.sender="sender-b".into();let b=put(&mut c,&b)?;
        let admitted=claim_on(&mut c,b.revision,7,4096)?.ok_or_else(||anyhow::anyhow!("missing claim"))?;
        transition_on(&mut c,admitted.token,Some(7),Disposition::Admitted,None)?;
        let mut d=message("three",None,"waiting after refusal");d.sender="sender-c".into();let d=put(&mut c,&d)?;
        let frozen=claim_on(&mut c,d.revision,7,4096)?.ok_or_else(||anyhow::anyhow!("missing frozen claim"))?;
        transition_on(&mut c,frozen.token,None,Disposition::Waiting,None)?;
        let page=waiting_peers_on(&c,"recipient",0,512)?;
        assert_eq!(page.items,vec![super::super::admission::Demand { peer:"sender-a".into(), revision:a.revision },super::super::admission::Demand { peer:"sender-c".into(), revision:d.revision }]);
        assert!(serde_json::to_vec(&page)?.len()<=512);
        assert!(matches!(waiting_peers_on(&c,"recipient",0,1),Err(Error::TooLarge { .. })));
        assert!(waiting_peers_on(&c,"other-recipient",0,512)?.items.is_empty());
        Ok(())
    }
}
