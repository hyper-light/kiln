//! This runner process's custody records (docs/ci.md W27-R08; the Transfers plan's step 3). An
//! agent session it hands on is signed for as it goes: a hand-on, by the session's identity at the
//! epoch it leaves, to its next epoch, carried in its package and sent to the server
//! (`AgentMsg::Custody`, kept to send again after a reconnect: the server takes each once, by its
//! digest). One it takes is signed for as it asks for the session's identity at the new epoch: a
//! receipt, following the hand-on its package carried, signed with the key the ask is for. Both go
//! in that ask, and kiln records both before the epoch rises, and certifies the key only then. A
//! session whose epoch's key this process doesn't hold (a server from before identities) goes as
//! before, unsigned.
//!
//! The thing handed on is the session's timeline at the epoch it leaves ("session <id> at epoch
//! <n>"): its id, that epoch and each of its events' digests (`crate::wire::sums::event`), which
//! both sides work out alike from the events the package carries. A timeline grows while it's
//! held, so each epoch's is a thing of its own: the next hand-on is of a timeline made from the one
//! received, and follows that receipt, and the chain runs on from hand-over to hand-over.

use std::collections::{BTreeMap, VecDeque};
use std::sync::LazyLock;

use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{self, PAYLOAD_CUSTODY, Statement, Subject};
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::harness::model::Event;
use crate::proto::{IdentityAsk, Unbundled};
use crate::runner::identity;

/// The digest algorithm a session's timeline is named by.
pub const TIMELINE: &str = "kiln-timeline-v1";
/// How many records sent are kept, to send again after a reconnect.
const KEEP: usize = 512;

/// A receipt waiting for its epoch's certificate.
struct Waiting {
    subject: Subject,
    /// The hand-on it follows (its bundle, as JSON), and that hand-on's digest.
    hand_on: String,
    follows: String,
    from: String,
    /// The session at the epoch it was taken at, without the process: its signer, as it says
    /// before kiln has certified the process's key.
    by: String,
}

impl Waiting {
    /// The receipt, at `epoch`.
    fn receipt(&self, epoch: u64) -> Statement {
        let body = Body { from: Some(self.from.clone()), why: "taken over".into(), epoch: Some(epoch), ..Body::new(Kind::Received, &self.by) };
        Statement { subject: vec![self.subject.clone()], follows: vec![self.follows.clone()], ..Statement::new(to_value(&body)) }
    }
}

#[derive(Default)]
struct State {
    waiting: BTreeMap<(String, u64), Waiting>,
    /// The receipt this process signed for each session at each epoch: what its hand-on follows.
    receipts: BTreeMap<(String, u64), String>,
    outbox: VecDeque<String>,
    sent: VecDeque<String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

/// Session `session`'s timeline as it leaves epoch `epoch` with `events`: the thing its hand-over
/// is about, named alike by both sides.
pub fn timeline(session: &str, epoch: u64, events: &[Event]) -> Subject {
    let mut h = Sha256::new();
    h.update(session.as_bytes());
    h.update([0]);
    h.update(epoch.to_be_bytes());
    h.update(u64::try_from(events.len()).unwrap_or(u64::MAX).to_be_bytes());
    for e in events {
        h.update(serde_json::to_value(e).map(|v| crate::wire::sums::event(&v)).unwrap_or_default().to_be_bytes());
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let annotations = BTreeMap::from([("epoch".to_owned(), Value::from(epoch)), ("events".to_owned(), Value::from(events.len()))]);
    Subject { name: format!("session {session} at epoch {epoch}"), digest: BTreeMap::from([(TIMELINE.to_owned(), hex)]), size: None, annotations }
}

/// The agent identity at `epoch` of the session whose identity at some epoch is `uri`, without the
/// runner process holding its key: whom a hand-on goes to, or a receipt comes from.
pub fn at_epoch(uri: &str, epoch: u64) -> Option<String> {
    let (head, _) = uri.split_once("/epoch/")?;
    head.contains("/agent/").then(|| format!("{head}/epoch/{epoch}"))
}

/// `s` signed with slot `slot`'s key, its body naming that identity: its bundle.
fn sign(slot: &str, by: &str, s: &Statement) -> anyhow::Result<Value> {
    let mut s = s.clone();
    if let Some(o) = s.body.as_object_mut() {
        o.insert("by".into(), Value::String(by.to_owned()));
    }
    let payload = statement::payload(&s)?;
    let (signature, cert) = identity::sign(slot, &statement::to_sign(PAYLOAD_CUSTODY, &payload))?;
    Ok(statement::bundle(PAYLOAD_CUSTODY, &payload, &signature, &crate::pki::der_of(&cert)?))
}

fn queue(bundle: &Value) {
    STATE.lock().outbox.push_back(bundle.to_string());
}

/// The digest of a bundle's statement ("" when it can't be read).
fn digest_of(bundle: &Value) -> String {
    use base64::Engine as _;
    let env = bundle.get("dsseEnvelope");
    let ty = env.and_then(|e| e.get("payloadType")).and_then(Value::as_str);
    let payload = env.and_then(|e| e.get("payload")).and_then(Value::as_str).and_then(|p| base64::engine::general_purpose::STANDARD.decode(p).ok());
    match (ty, payload) {
        (Some(t), Some(p)) => statement::digest(t, &p),
        _ => String::new(),
    }
}

/// A bundle's statement's body, unchecked: the server checks it.
fn body_of(bundle: &Value) -> Option<Value> {
    use base64::Engine as _;
    let payload = bundle.get("dsseEnvelope")?.get("payload")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    let s: Statement = serde_json::from_slice(&bytes).ok()?;
    Some(s.body)
}

/// Whom a hand-on says its thing comes from: the giver a take hands on for, else its signer.
fn giver_of(hand_on: &Value) -> Option<String> {
    let body = body_of(hand_on)?;
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
    if s("kind").as_deref() == Some("taken") { s("taken_for") } else { s("by") }
}

/// The hand-on of session `session` as it leaves epoch `epoch` with `events`, for `why`: signed by
/// its identity at that epoch, if this process holds it, and sent to the server; none if not (it
/// goes unsigned, as before).
pub fn hand_on(session: &str, epoch: u64, events: &[Event], why: &str) -> Option<Value> {
    let slot = identity::slot("agent", Some(session), Some(epoch));
    let by = identity::uri(&slot)?;
    let to = at_epoch(&by, epoch.saturating_add(1))?;
    let body = Body { to: Some(to), why: why.to_owned(), epoch: Some(epoch.saturating_add(1)), ..Body::new(Kind::HandedOn, &by) };
    // Made from the timeline this process received at that epoch, when it took the session over.
    let follows = STATE.lock().receipts.get(&(session.to_owned(), epoch)).cloned().into_iter().collect();
    let s = Statement { subject: vec![timeline(session, epoch, events)], follows, ..Statement::new(to_value(&body)) };
    match sign(&slot, &by, &s) {
        Ok(bundle) => {
            tracing::info!(target: "kiln.custody", %session, epoch, "signed its hand-on");
            queue(&bundle);
            Some(bundle)
        }
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %session, epoch, error = %format!("{e:#}"), "couldn't sign its hand-on: it goes unsigned");
            None
        }
    }
}

/// Session `session` taken over here at `epoch` (an epoch up from the one it left), its package
/// carrying `events` and the giver's `hand_on`: this process's receipt waits to go with the ask
/// for its identity at `epoch` ([`carrying`]).
pub fn took(session: &str, epoch: u64, events: &[Event], hand_on: &Value) {
    let from = giver_of(hand_on).unwrap_or_default();
    let Some(by) = at_epoch(&from, epoch) else {
        return tracing::warn!(target: "kiln.custody", %session, epoch, from = %from, "a hand-on from no agent session: no receipt signed");
    };
    let waiting = Waiting { subject: timeline(session, epoch.saturating_sub(1), events), hand_on: hand_on.to_string(), follows: digest_of(hand_on), from, by };
    STATE.lock().waiting.insert((session.to_owned(), epoch), waiting);
}

/// The ask for session `session`'s identity at `epoch`, carrying the hand-on it was taken over
/// with and this process's receipt for it, signed with the key the ask is for: kiln records both
/// before the epoch rises. As it was when no receipt waits (it wasn't taken over here at `epoch`,
/// or its giver signed nothing) or the receipt can't be signed.
pub fn carrying(session: &str, epoch: u64, mut ask: IdentityAsk) -> IdentityAsk {
    let key = (session.to_owned(), epoch);
    let Some((hand_on, receipt)) = STATE.lock().waiting.get(&key).map(|w| (w.hand_on.clone(), w.receipt(epoch))) else { return ask };
    let slot = identity::slot("agent", Some(session), Some(epoch));
    match sign_uncertified(&slot, &receipt) {
        Ok((payload, signature)) => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD;
            STATE.lock().receipts.insert(key, statement::digest(PAYLOAD_CUSTODY, &payload));
            ask.hand_on = Some(hand_on);
            ask.receipt = Some(Unbundled { payload_type: PAYLOAD_CUSTODY.to_owned(), payload: b64.encode(&payload), signature: b64.encode(&signature) });
            tracing::info!(target: "kiln.custody", %session, epoch, "signed its receipt");
        }
        Err(e) => tracing::warn!(target: "kiln.custody", %session, epoch, error = %format!("{e:#}"), "couldn't sign its receipt"),
    }
    ask
}

/// `s`'s payload, and its signature with slot `slot`'s key, before kiln has certified it.
fn sign_uncertified(slot: &str, s: &Statement) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let payload = statement::payload(s)?;
    let signature = identity::sign_uncertified(slot, &statement::to_sign(PAYLOAD_CUSTODY, &payload))?;
    Ok((payload, signature))
}

/// This process's identity at `epoch` of session `session` was certified: its receipt went with
/// the ask, and kiln recorded it.
pub fn certified(session: &str, epoch: u64) {
    STATE.lock().waiting.remove(&(session.to_owned(), epoch));
}

/// The records to send now, oldest first: each kept, to send again after a reconnect.
pub fn drain() -> Vec<String> {
    let mut st = STATE.lock();
    let out: Vec<String> = st.outbox.drain(..).collect();
    for r in &out {
        st.sent.push_back(r.clone());
    }
    while st.sent.len() > KEEP {
        st.sent.pop_front();
    }
    out
}

/// What was sent, to send again after a reconnect (the server takes each once).
pub fn sent() -> Vec<String> {
    STATE.lock().sent.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W27-R08: both sides of a hand-over name a session's timeline alike from the same events,
    /// and differently once an event differs or the epoch does.
    #[test]
    fn w27_r08_a_timeline_is_named_alike_by_both_sides() -> anyhow::Result<()> {
        let events: Vec<Event> = serde_json::from_value(serde_json::json!([]))?;
        let a = timeline("s1", 3, &events);
        assert_eq!(a, timeline("s1", 3, &events));
        assert_ne!(a.digest, timeline("s1", 4, &events).digest);
        assert_ne!(a.digest, timeline("s2", 3, &events).digest);
        assert_eq!(a.digest.get(TIMELINE).map(String::len), Some(64));
        assert_eq!(at_epoch("kiln://o/person/p/agent/s1/epoch/3/machine/m/instance/i", 4).as_deref(), Some("kiln://o/person/p/agent/s1/epoch/4"));
        assert_eq!(at_epoch("kiln://o/person/p/machine/m/instance/i", 4), None);
        Ok(())
    }
}
