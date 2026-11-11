//! Bounded detail reads through the session's existing command and live-feed paths. A failed read
//! leaves presence unknown; only a response at the requested queue revision establishes absence.

use std::io::{self, Write};

use futures::future::{Either, select};
use kiln_seams::bus::Bus as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{Command, QueueItem};
use super::queued::Queued;
use super::registry::ANSWER_WAIT;
use super::session::Info;
use super::state_view::{Error, QueueEntry, Read, Reply, Step, FRAME};
use crate::hub::Hub;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "code", content = "cause", rename_all = "snake_case")]
pub enum Failure {
    NotReady,
    Unreachable,
    TimedOut,
    FeedLost,
    Malformed,
    Source(Error),
}

pub struct Observed<T> {
    pub value: T,
    /// Older runners have no conditional-action protocol.
    pub revision: Option<String>,
}

pub enum Removal {
    Applied { revision: Option<String> },
    Unchanged,
}

struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).filter(|n| *n <= FRAME).ok_or_else(|| io::Error::other("state read frame budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

#[derive(Deserialize)]
struct Refusal { code: Error }

fn answer(text: &str, client: &str) -> Option<Result<Value, Failure>> {
    if text.len() > FRAME { return None; }
    #[derive(Deserialize)]
    struct Envelope {
        t: String,
        #[serde(default)]
        client: String,
        value: Option<Value>,
    }
    let frame: Envelope = serde_json::from_str(text).ok()?;
    if frame.t != "reply" || frame.client != client { return None; }
    let value = frame.value?;
    if let Some(error) = value.get("error") {
        return Some(Err(serde_json::from_value::<Refusal>(error.clone()).map_or(Failure::Malformed, |r| Failure::Source(r.code))));
    }
    Some(value.get("result").cloned().ok_or(Failure::Malformed))
}

/// Attach before dispatch, so an immediate reply cannot race its listener. The existing inbox
/// deadline bounds the entire operation, including command dispatch and feed availability.
async fn request(hub: &Hub, session: &str, client: String, command: Command) -> Result<Value, Failure> {
    if serde_json::to_writer(Count(0), &command).is_err() { return Err(Failure::Source(Error::Budget)); }
    let super::registry::Attachment { snapshot, topic, at, viewer, refusals, .. } = hub.harness.attach(session).ok_or(Failure::Unreachable)?;
    // This request needs the live feed, not a retained copy of the session's history.
    drop(snapshot);
    let mut at = at.ok_or(Failure::FeedLost)?;
    let operation = async {
        let sent = match command {
            command @ Command::MailHandoff { .. } => super::mail_delivery::send(hub, session, command, None).await.map_err(|_| Failure::Unreachable)?,
            command => hub.harness.command(hub, session, command).await.went(),
        };
        if !sent { return Err(Failure::Unreachable); }
        loop {
            if viewer.refusals() != refusals { return Err(Failure::FeedLost); }
            // One live frame at a time; no queue or whole-snapshot assembly for this reader.
            let batch = hub.bus.read(&topic, at, 1).await.map_err(|_| Failure::FeedLost)?;
            let empty = batch.messages.is_empty();
            at = batch.next;
            for message in batch.messages {
                let Some(frame) = super::live::decode_ref(&message.payload) else { return Err(Failure::Malformed) };
                if let Some(answer) = answer(frame.text, &client) { return answer; }
            }
            if empty { hub.bus.wait(&topic, at).await.map_err(|_| Failure::FeedLost)?; }
        }
    };
    let deadline = hub.clock.sleep(ANSWER_WAIT);
    futures::pin_mut!(operation, deadline);
    match select(operation, deadline).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(Failure::TimedOut),
    }
}

pub async fn read(hub: &Hub, session: &str, read: Read) -> Result<Reply, Failure> {
    let expected = read.revision.clone();
    let client = crate::util::new_id();
    let value = request(hub, session, client.clone(), Command::StateRead { client, read }).await?;
    let reply: Reply = serde_json::from_value(value).map_err(|_| Failure::Malformed)?;
    let revision = match &reply {
        Reply::Page { revision, .. } | Reply::Text { revision, .. } | Reply::Value { revision, .. } | Reply::Node { revision, .. } => revision,
    };
    if revision != &expected { return Err(Failure::Malformed); }
    Ok(reply)
}

fn registered(value: &Value, expected: &str) -> Result<(), Failure> {
    if value.get("type").and_then(Value::as_str) != Some("registered") || value.get("revision").and_then(Value::as_str) != Some(expected) { return Err(Failure::Malformed); }
    Ok(())
}

/// Register one bounded response at its current source owner. This records metadata only:
/// queue contents and input custody remain until native provider delivery is proven.
pub(super) async fn register_handoff(hub: &Hub, session: &str, revision: String, handoff: String, turn: Option<i64>, ids: Vec<String>, response_digest: String) -> Result<(), Failure> {
    let client = crate::util::new_id();
    let expected = revision.clone();
    let value = request(hub, session, client.clone(), Command::MailHandoff { client, revision, handoff, turn, ids, response_digest }).await?;
    registered(&value, &expected)
}

fn revision(info: &Info) -> Result<String, Failure> {
    info.queue_revision.clone().ok_or(Failure::NotReady)
}

fn queued(entry: QueueEntry) -> Result<Queued, Failure> {
    let digest = u64::from_str_radix(&entry.digest, 16).map_err(|_| Failure::Malformed)?;
    Ok(Queued { id: entry.id, digest, len: entry.len })
}

/// A positive entry in the current bounded index is useful without another read. A negative
/// answer requires a complete index or a targeted response at the owner's queue revision.
pub async fn lookup(hub: &Hub, session: &str, info: &Info, message: &str) -> Result<Option<Queued>, Failure> {
    if let Some(q) = info.queue.iter().find(|q| q.id == message) { return Ok(Some(q.clone())); }
    if !info.queue_partial { return Ok(None); }
    let requested = Read { revision: revision(info)?, field: "$queue_index".into(), path: vec![Step::Id { id: message.into() }], offset: 0 };
    match read(hub, session, requested).await {
        Ok(Reply::Value { value, .. }) => {
            let entry: QueueEntry = serde_json::from_value(value).map_err(|_| Failure::Malformed)?;
            if entry.id != message { return Err(Failure::Malformed); }
            queued(entry).map(Some)
        }
        Err(Failure::Source(Error::InvalidPath)) => Ok(None),
        Ok(_) => Err(Failure::Source(Error::Budget)),
        Err(error) => Err(error),
    }
}

/// Visit index pages without retaining them together. Each continuation must advance; a stale
/// revision or a lost feed is an error rather than an authoritative end of the queue.
async fn index_pages(hub: &Hub, session: &str, info: &Info, mut visit: impl FnMut(Queued) -> bool) -> Result<(), Failure> {
    let operation = async {
        for q in &info.queue { if visit(q.clone()) { return Ok(()); } }
        if !info.queue_partial { return Ok(()); }
        let revision = revision(info)?;
        let mut offset = info.queue_next.unwrap_or(0);
        loop {
        let requested = Read { revision: revision.clone(), field: "$queue_index".into(), path: Vec::new(), offset };
        let Reply::Page { entries, next, .. } = read(hub, session, requested).await? else { return Err(Failure::Malformed) };
        let mut expected = offset;
        for entry in entries {
            if entry.key != (Step::Index { index: expected }) { return Err(Failure::Malformed); }
            expected = expected.checked_add(1).ok_or(Failure::Malformed)?;
            let value = entry.value.ok_or(Failure::Source(Error::Budget))?;
            let entry: QueueEntry = serde_json::from_value(value).map_err(|_| Failure::Malformed)?;
            if visit(queued(entry)?) { return Ok(()); }
        }
        match next {
            Some(next) if next > offset && next == expected => offset = next,
            Some(_) => return Err(Failure::Malformed),
            None if expected == info.queued => return Ok(()),
            None => return Err(Failure::Malformed),
        }
        }
    };
    let deadline = hub.clock.sleep(ANSWER_WAIT);
    futures::pin_mut!(operation, deadline);
    match select(operation, deadline).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(Failure::TimedOut),
    }
}

pub async fn first(hub: &Hub, session: &str, info: &Info, matches: impl Fn(&Queued) -> bool) -> Result<Option<Queued>, Failure> {
    let mut found = None;
    index_pages(hub, session, info, |q| {
        if matches(&q) { found = Some(q); true } else { false }
    }).await?;
    Ok(found)
}

pub async fn mail(hub: &Hub, waiter: &super::wait::Waiter, info: &Info) -> Result<Vec<String>, Failure> {
    let (mut out, mut bytes) = (Vec::new(), 0);
    if super::wait::mail_page(waiter, info, &[], &mut out, &mut bytes) { return Ok(out); }
    index_pages(hub, &waiter.id, info, |q| super::wait::mail_page(waiter, info, std::slice::from_ref(&q), &mut out, &mut bytes)).await?;
    Ok(out)
}

/// A wait can wake on its first eligible message; it need not read the rest of the index before
/// the existing handoff path selects and claims what fits in one answer.
pub(super) async fn has_mail(hub: &Hub, waiter: &super::wait::Waiter, info: &Info) -> Result<bool, Failure> {
    let (mut out, mut bytes) = (Vec::new(), 0);
    if super::wait::mail_page(waiter, info, &[], &mut out, &mut bytes) { return Ok(false); }
    index_pages(hub, &waiter.id, info, |q| {
        super::wait::mail_page(waiter, info, std::slice::from_ref(&q), &mut out, &mut bytes);
        !out.is_empty()
    }).await?;
    Ok(!out.is_empty())
}

/// Read only the selected item's body. A body too large for one response remains on its owner;
/// callers must not remove it after a failed or incomplete read.
pub async fn item(hub: &Hub, session: &str, info: &Info, message: &str) -> Result<Option<Observed<QueueItem>>, Failure> {
    let Some(revision) = info.queue_revision.clone() else {
        return Ok(hub.harness.queue(session).into_iter().find(|q| q.id == message).map(|value| Observed { value, revision: None }));
    };
    let requested = Read { revision: revision.clone(), field: "queue".into(), path: vec![Step::Id { id: message.into() }], offset: 0 };
    match read(hub, session, requested).await {
        Ok(reply) => {
            let value = item_value(reply)?;
            let value: QueueItem = serde_json::from_value(value).map_err(|_| Failure::Malformed)?;
            if value.id != message { return Err(Failure::Malformed); }
            Ok(Some(Observed { value, revision: Some(revision) }))
        }
        Err(Failure::Source(Error::InvalidPath)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn item_value(reply: Reply) -> Result<Value, Failure> {
    match reply {
        Reply::Value { value, .. } => Ok(value),
        Reply::Page { entries, next: None, .. } => {
            let mut object = serde_json::Map::new();
            for entry in entries {
                if entry.node.is_some() { return Err(Failure::Source(Error::Budget)); }
                let Step::Key { key } = entry.key else { return Err(Failure::Malformed) };
                let value = entry.value.ok_or(Failure::Source(Error::Budget))?;
                if object.insert(key, value).is_some() { return Err(Failure::Malformed); }
            }
            Ok(Value::Object(object))
        }
        _ => Err(Failure::Source(Error::Budget)),
    }
}

pub async fn remove(hub: &Hub, session: &str, item: &Observed<QueueItem>) -> Result<Removal, Failure> {
    let command = Command::QueueRemove { id: item.value.id.clone() };
    let Some(revision) = &item.revision else {
        return Ok(if hub.harness.command(hub, session, command).await.went() { Removal::Applied { revision: None } } else { Removal::Unchanged });
    };
    let client = crate::util::new_id();
    let value = request(hub, session, client.clone(), Command::AtState { client, revision: revision.clone(), command: Box::new(command) }).await?;
    if value.get("type").and_then(Value::as_str) != Some("applied") { return Err(Failure::Malformed); }
    Ok(Removal::Applied { revision: value.get("revision").and_then(Value::as_str).map(str::to_owned) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_expose_only_typed_codes() {
        assert_eq!(serde_json::to_value(Failure::TimedOut).ok(), Some(serde_json::json!({"code": "timed_out"})));
        assert_eq!(serde_json::to_value(Failure::Source(Error::Stale)).ok(), Some(serde_json::json!({"code": "source", "cause": "stale"})));
    }

    #[test]
    fn only_the_matching_reply_establishes_a_result() {
        assert_eq!(answer(r#"{"t":"reply","client":"other","value":{"result":{"type":"value","revision":"r","value":null}}}"#, "mine"), None);
        assert_eq!(answer(r#"{"t":"reply","client":"mine","value":{"error":{"code":"stale"}}}"#, "mine"), Some(Err(Failure::Source(Error::Stale))));
        assert!(matches!(answer(r#"{"t":"reply","client":"mine","value":{"result":{"type":"value","revision":"r","value":null}}}"#, "mine"), Some(Ok(_))));
    }

    #[test]
    fn only_exact_owner_registration_acknowledges_a_handoff() {
        assert_eq!(registered(&serde_json::json!({"type":"registered","revision":"queue-r"}),"queue-r"),Ok(()));
        for reply in [serde_json::json!({"type":"applied","revision":"queue-r"}),serde_json::json!({"type":"registered","revision":"stale-r"}),serde_json::json!({"type":"registered","revision":null}),serde_json::json!({"type":"registered"}),serde_json::json!({"retained":true})] {
            assert_eq!(registered(&reply,"queue-r"),Err(Failure::Malformed));
        }
    }

    #[test]
    fn unreadable_or_oversized_replies_do_not_establish_absence() {
        assert_eq!(answer("not json", "mine"), None);
        let over = format!(r#"{{"t":"reply","client":"mine","value":{{"result":"{}"}}}}"#, "x".repeat(FRAME));
        assert_eq!(answer(&over, "mine"), None);
        assert_eq!(answer(r#"{"t":"reply","client":"mine","value":{}}"#, "mine"), Some(Err(Failure::Malformed)));
    }
}
