//! Acceptance borrows its original body and owns one bounded encoded transaction. The charge
//! moves into Records with that transaction, including cancellation while its worker is queued.

use std::io;
use kiln_seams::records::{self, Cond, Tx, Write};
use serde::Serialize;
use super::{Cached, Current, Head, Origin, Pending, Refusal, HEADS, MESSAGES, PENDING, unavailable};
use crate::harness::{admission::{BufferCharge, Input, Lease, Disposition}, model::Command};

#[derive(Clone, Copy, Serialize)]
pub(super) struct PendingWrite<'a> {
    lease: &'a Lease,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a Command>,
    #[serde(skip_serializing_if = "empty_writes")]
    opaque_writes: &'a [String],
    origin: Origin,
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<&'a Current>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disposition: Option<&'a Disposition>,
    accepted_at: i64,
    digest: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted_seq: Option<records::Rev>,
}

fn empty_writes(writes: &&[String]) -> bool { writes.is_empty() }

impl<'a> PendingWrite<'a> {
    pub(super) fn original(lease: &'a Lease, command: &'a Command, body_bytes: usize) -> Self {
        Self { lease, command: Some(command), opaque_writes: &[], origin: Origin::Accepted, current: None, body_bytes: Some(body_bytes), disposition: None, accepted_at: 0, digest: "", accepted_seq: None }
    }
    pub(super) fn borrowed(pending: &'a Pending) -> Self {
        Self { lease: &pending.lease, command: pending.command.as_ref(), opaque_writes: &pending.opaque_writes, origin: pending.origin, current: pending.current.as_ref(), body_bytes: pending.body_bytes, disposition: pending.disposition.as_ref(), accepted_at: pending.accepted_at, digest: &pending.digest, accepted_seq: pending.accepted_seq }
    }
    fn receipt(self, at: i64, digest: &'a str) -> Self { Self { accepted_at: at, digest, ..self } }
}

#[derive(Serialize)]
struct KeptWrite<'a> { epoch: u64, at: i64, digest: &'a str }

struct Count(usize);
impl io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).filter(|bytes| *bytes <= records::VALUE_MAX).ok_or_else(|| io::Error::other("inbox value exceeds record capacity"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn count(session: &str, value: &impl Serialize) -> Result<usize, Refusal> {
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_err(|_| unavailable(session, "the accepted original exceeds record capacity"))?;
    Ok(count.0)
}

struct Fixed { bytes: Vec<u8>, limit: usize }
impl io::Write for Fixed {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().checked_add(bytes.len()).is_none_or(|after| after > self.limit) { return Err(io::Error::other("inbox encoding changed after its count")); }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn encode(session: &str, value: &impl Serialize, len: usize) -> Result<Vec<u8>, Refusal> {
    let mut out = Fixed { bytes: Vec::new(), limit: len };
    out.bytes.try_reserve_exact(len).map_err(|_| unavailable(session, "the accepted original cannot allocate its counted encoding"))?;
    serde_json::to_writer(&mut out, value).map_err(|_| unavailable(session, "the accepted original cannot be encoded within its reservation"))?;
    if out.bytes.len() != len { return Err(unavailable(session, "the accepted original changed its encoded size")); }
    Ok(out.bytes)
}

fn string(session: &str, value: &str) -> Result<String, Refusal> {
    let mut out = String::new();
    out.try_reserve_exact(value.len()).map_err(|_| unavailable(session, "inbox metadata allocation is unavailable"))?;
    out.push_str(value);
    Ok(out)
}

fn key(session: &str, message: Option<&str>) -> Result<Vec<u8>, Refusal> {
    let len = match message { Some(message) => session.len().checked_add(1).and_then(|bytes| bytes.checked_add(message.len())), None => Some(session.len()) }.ok_or_else(|| unavailable(session, "inbox key size overflowed"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| unavailable(session, "inbox key allocation is unavailable"))?;
    out.extend_from_slice(session.as_bytes());
    if let Some(message) = message { out.push(b'/'); out.extend_from_slice(message.as_bytes()); }
    Ok(out)
}

pub(super) struct Built {
    pub tx: Tx,
    pub owner: Option<Box<dyn Send + Sync>>,
    pub head: Option<Head>,
}

pub(super) fn build<'a>(session: &'a str, message: &str, digest: &'a str, at: i64, cached: &Cached, pending: Option<PendingWrite<'a>>, budget: Option<&Input>) -> Result<Built, Refusal> {
    let kept = KeptWrite { epoch: cached.head.epoch, at, digest };
    let pending = pending.map(|pending| pending.receipt(at, digest));
    let head = cached.rev.is_none().then(|| cached.head.clone());
    let kept_len = count(session, &kept)?;
    let pending_len = pending.as_ref().map(|pending| count(session, pending)).transpose()?;
    let head_len = head.as_ref().map(|head| count(session, head)).transpose()?;
    let key_len = session.len().checked_add(1).and_then(|bytes| bytes.checked_add(message.len())).ok_or_else(|| unavailable(session, "inbox key size overflowed"))?;
    let conditions = 2usize.checked_add(usize::from(pending.is_some())).ok_or_else(|| unavailable(session, "inbox condition count overflowed"))?;
    let writes = 1usize.checked_add(usize::from(pending.is_some())).and_then(|n| n.checked_add(usize::from(head.is_some()))).ok_or_else(|| unavailable(session, "inbox write count overflowed"))?;
    let mut bytes = conditions.checked_mul(size_of::<Cond>()).and_then(|n| writes.checked_mul(size_of::<Write>()).and_then(|w| n.checked_add(w))).ok_or_else(|| unavailable(session, "inbox transaction size overflowed"))?;
    let mut add = |n: usize| { bytes = bytes.checked_add(n).ok_or_else(|| unavailable(session, "inbox transaction size overflowed"))?; Ok::<_, Refusal>(()) };
    // One family/key allocation per actual condition/write; one encoded value per put.
    for (family, len) in [(HEADS, session.len()), (MESSAGES, key_len)] { add(family.len())?; add(len)?; }
    if pending.is_some() { add(PENDING.len())?; add(key_len)?; }
    let mut driver = 0usize;
    for (family, len, value) in [(MESSAGES, key_len, Some(kept_len)), (PENDING, key_len, pending_len), (HEADS, session.len(), head_len)] {
        if let Some(value) = value {
            add(family.len())?; add(len)?; add(value)?;
            let bound = family.len().checked_add(len).and_then(|n| n.checked_add(value)).ok_or_else(|| unavailable(session, "inbox binding size overflowed"))?;
            driver = driver.max(bound);
        }
    }
    // Both production backends bind at most one row at once. Turso moves that one ValueBlob
    // into its statement; SQLite's SQLITE_TRANSIENT copy dies with execute's statement.
    add(driver)?;
    add(size_of::<BufferCharge>())?;
    let owner = budget.map(|budget| budget.reserve_buffer(bytes).map_err(|_| unavailable(session, "inbox transaction reservation overflowed"))?.ok_or_else(|| unavailable(session, "inbox transaction has no physical byte authority"))).transpose()?;
    let mut tx = Tx::default();
    tx.conds.try_reserve_exact(conditions).map_err(|_| unavailable(session, "inbox condition allocation is unavailable"))?;
    tx.writes.try_reserve_exact(writes).map_err(|_| unavailable(session, "inbox write allocation is unavailable"))?;
    let head_key = key(session, None)?;
    let family = string(session, HEADS)?;
    tx.conds.push(match cached.rev { Some(rev) => Cond::At { family, key: head_key, rev }, None => Cond::Absent { family, key: head_key } });
    tx.conds.push(Cond::Absent { family: string(session, MESSAGES)?, key: key(session, Some(message))? });
    tx.writes.push(Write::Put { family: string(session, MESSAGES)?, key: key(session, Some(message))?, value: encode(session, &kept, kept_len)? });
    if let (Some(pending), Some(len)) = (pending, pending_len) {
        tx.conds.push(Cond::Absent { family: string(session, PENDING)?, key: key(session, Some(message))? });
        tx.writes.push(Write::Put { family: string(session, PENDING)?, key: key(session, Some(message))?, value: encode(session, &pending, len)? });
    }
    if let (Some(head), Some(len)) = (&head, head_len) { tx.writes.push(Write::Put { family: string(session, HEADS)?, key: key(session, None)?, value: encode(session, head, len)? }); }
    Ok(Built { tx, owner: owner.map(|owner| Box::new(owner) as Box<dyn Send + Sync>), head })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::harness::{admission::{PhysicalBudget, Stage}, model::{SendMode, Attachment}};

    fn original(text: &str) -> Pending {
        Pending::new(Lease { message: "original".into(), peer: "peer".into(), epoch: 0, bytes: 100, stage: Stage::InboxAccepted, token: None, handoff: None }, Command::Send { id: "original".into(), text: text.into(), mode: SendMode::Queue, attachments: vec![Attachment { name: "é\".txt".into(), mime: "text/plain".into(), data_base64: "eA==".into(), file_id: Some("fid".into()), path: None }], overrides: None, reply_to: None, task: None })
    }

    #[test]
    fn borrowed_original_encodes_the_same_wire_with_exact_utf8_and_escape_count() -> Result<(), Refusal> {
        let mut pending = original("line\n\"\\🦀");
        pending.accepted_at = 12;
        pending.digest = "immutable-digest".into();
        let borrowed = PendingWrite::borrowed(&pending);
        let len = count("s", &borrowed)?;
        let encoded = encode("s", &borrowed, len)?;
        assert_eq!(encoded.len(), len);
        let ordinary = serde_json::to_vec(&pending).map_err(|_| unavailable("s", "fixture encoding failed"))?;
        assert_eq!(encoded, ordinary, "borrowed serialization must preserve the existing body schema");
        assert!(encode("s", &borrowed, len.saturating_sub(1)).is_err(), "the writer never grows beyond its counted buffer");
        Ok(())
    }

    #[test]
    fn counted_transaction_refuses_short_authority_and_releases_on_drop() -> Result<(), Refusal> {
        let pool = PhysicalBudget::new(records::VALUE_MAX.saturating_mul(2));
        let inbox = Arc::new(super::super::Inbox::new(Arc::new(records::SimRecords::new()), super::super::Limits::default()));
        let input = Input::new(0, Arc::clone(&pool), inbox);
        let cached = Cached { head: Head::default(), rev: None, kept: None };
        let pending = original("actual retained words");
        let built = build("s", "original", "digest", 12, &cached, Some(PendingWrite::borrowed(&pending)), Some(&input))?;
        let (required, _) = pool.counts();
        assert!(required > 0);
        drop(built);
        assert_eq!(pool.counts().0, 0);
        pool.resize(required.saturating_sub(1));
        assert!(build("s", "original", "digest", 12, &cached, Some(PendingWrite::borrowed(&pending)), Some(&input)).is_err());
        assert_eq!(pool.counts().0, 0, "a refused count cannot leak input authority");
        let oversized = original("x".repeat(records::VALUE_MAX).as_str());
        assert!(count("s", &PendingWrite::borrowed(&oversized)).is_err(), "record capacity is checked before encoding allocation");
        Ok(())
    }
}
