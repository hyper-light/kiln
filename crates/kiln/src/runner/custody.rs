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
//! The thing handed on is the session's timeline at the epoch it leaves ([`timeline`]): the next
//! hand-on is of a timeline made from the one received, and follows that receipt, so the chain
//! runs on from hand-over to hand-over.

use std::collections::{BTreeMap, VecDeque};
use std::sync::LazyLock;

use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{self, PAYLOAD_CUSTODY, Statement, Subject};
use parking_lot::Mutex;
use serde_json::Value;

use crate::custody::{at_epoch, timeline};
use crate::harness::model::Event;
use crate::proto::{IdentityAsk, Unbundled};
use crate::runner::identity;

/// How many records sent are kept, to send again after a reconnect.
const KEEP: usize = 512;

/// A session taken over here, waiting for its identity at the epoch it goes on at.
struct Waiting {
    /// What the receipt follows, when the process it came from signed a hand-on (none: one from
    /// before signed hand-overs, and it goes on unsigned, kiln saying so).
    signed: Option<Receipt>,
    /// Taken over blue/green: it goes on at the epoch it had until this process is promoted, its
    /// ask held till then, and it goes on at the next once that's certified ([`certified`]).
    held: bool,
    blue_green: bool,
}

/// A receipt to sign, of the hand-on it follows.
struct Receipt {
    subject: Subject,
    /// The hand-on (its bundle, as JSON), and its digest.
    hand_on: String,
    follows: String,
    from: String,
    /// The session at the epoch it was taken at, without the process: its signer, as it says
    /// before kiln has certified the process's key.
    by: String,
}

impl Receipt {
    /// Of session `session`'s `hand_on`, taken at `epoch` with `events`: none if it isn't an agent
    /// session's.
    fn of(session: &str, epoch: u64, events: &[Event], hand_on: &Value) -> Option<Self> {
        let from = giver_of(hand_on).unwrap_or_default();
        let Some(by) = at_epoch(&from, epoch) else {
            tracing::warn!(target: "kiln.custody", %session, epoch, from = %from, "a hand-on from no agent session: no receipt signed");
            return None;
        };
        Some(Self { subject: timeline(session, epoch.saturating_sub(1), events), hand_on: hand_on.to_string(), follows: digest_of(hand_on), from, by })
    }

    /// The receipt, at `epoch`.
    fn statement(&self, epoch: u64) -> Statement {
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
    /// What's sent once, not again after a reconnect: a hand-over's word refused here.
    once: VecDeque<String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

/// `s` signed with slot `slot`'s key, its body naming that identity: its bundle.
fn sign(slot: &str, by: &str, s: &Statement) -> anyhow::Result<Value> {
    sign_as(slot, by, PAYLOAD_CUSTODY, s)
}

/// [`sign`], as `payload_type`.
fn sign_as(slot: &str, by: &str, payload_type: &str, s: &Statement) -> anyhow::Result<Value> {
    let mut s = s.clone();
    if let Some(o) = s.body.as_object_mut() {
        o.insert("by".into(), Value::String(by.to_owned()));
    }
    let payload = statement::payload(&s)?;
    let (signature, cert) = identity::sign(slot, &statement::to_sign(payload_type, &payload))?;
    Ok(statement::bundle(payload_type, &payload, &signature, &crate::pki::der_of(&cert)?))
}

/// Ask `json`, a session's act ([`signed_ask`]), went unanswered (`why`: kiln couldn't be asked,
/// as from a machine cut off), so it wasn't done: kiln is told once it's back, and records it
/// with the act (docs/ci.md W27-R08). Nothing for another ask.
pub fn unanswered(json: &str, why: &str) {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return };
    let (Some(session), Some(op)) = (v.get("session").and_then(Value::as_str), v.get("op").and_then(Value::as_str)) else { return };
    if op == "registry" {
        return;
    }
    let name = v.get("name").and_then(Value::as_str).unwrap_or(op);
    tracing::info!(target: "kiln.custody", %session, %name, %why, "an act went unanswered by kiln: not done");
    let record = serde_json::json!({ "refused_act": { "session": session, "op": op, "name": name, "why": format!("kiln couldn't be asked whether it's the session's to do, so it wasn't done: {why}"), "act": v.get("act") } });
    STATE.lock().once.push_back(record.to_string());
}

/// This runner's agent sessions and their harness processes, as its harness says.
type Sessions = Box<dyn Fn() -> Vec<(String, Option<u32>)> + Send + Sync>;
static SESSIONS: std::sync::OnceLock<Sessions> = std::sync::OnceLock::new();

/// Where this runner's agent sessions' harness processes are (`Host::harness_pids`): what tells an
/// agent session's own processes from the machine's ([`session_of_pid`]).
pub fn set_sessions(f: impl Fn() -> Vec<(String, Option<u32>)> + Send + Sync + 'static) {
    let _ = SESSIONS.set(Box::new(f));
}

/// The agent session process `pid` belongs to (its harness, or what it started), if any. Blocking.
pub fn session_of_pid(pid: u32) -> Option<String> {
    let sessions = SESSIONS.get().map(|f| f()).unwrap_or_default();
    crate::harness::survivors::session_of(pid, &sessions)
}

/// Whether this runner's server takes custody records and checks its sessions' acts
/// (`CAP_CUSTODY`), as it last said.
static SERVER_CHECKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The server says what it can do: whether it checks this runner's sessions' acts.
pub fn server_checks(yes: bool) {
    SERVER_CHECKS.store(yes, std::sync::atomic::Ordering::Relaxed);
}

/// Whether kiln takes `what` (a run on another machine) as agent session `session`'s act now
/// (docs/ci.md W27-R08): asked with the act signed by the session this process holds, through
/// `handle` ([`signed_ask`]); why not (kiln refused it, or can't be asked) if it doesn't. A server
/// from before, which doesn't check acts, takes them as before.
pub async fn act_taken(handle: &crate::reach::node::Handle, session: &str, what: &str) -> Result<(), String> {
    if !signs() || !SERVER_CHECKS.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let ask = serde_json::json!({ "op": "act", "session": session, "name": what });
    let answer = handle.query(ask.to_string()).await.map_err(|e| format!("kiln couldn't be asked whether it's session {session}'s to do: {e:#}"))?;
    let v: Value = serde_json::from_str(&answer).map_err(|e| format!("kiln's answer was unreadable: {e}"))?;
    match v.get("error").and_then(Value::as_str) {
        Some(why) => Err(why.trim_start_matches("refused: ").to_owned()),
        None => Ok(()),
    }
}

/// Ask `json` this runner sends kiln for one of its agent sessions (a kiln operation it calls, a
/// page it shows: `crate::mcp::runner_ask`), with the act it is beside it as `act` (docs/ci.md
/// W27-R08; the Transfers plan's step 5): signed with the session's key at the epoch this process
/// holds it at, so kiln applies it only from the session's holder now. As it was when this process
/// holds no certified key for the session (it just took it over), or it isn't an act (what kiln's
/// operations are).
pub fn signed_ask(json: String) -> String {
    use kiln_ci::identity::custody::{Act, PAYLOAD_ACT};
    use sha2::Digest as _;
    if !signs() {
        return json;
    }
    let Ok(mut v) = serde_json::from_str::<Value>(&json) else { return json };
    let (Some(session), Some(op)) = (v.get("session").and_then(Value::as_str).map(str::to_owned), v.get("op").and_then(Value::as_str).map(str::to_owned)) else { return json };
    if op == "registry" {
        return json;
    }
    let Some(epoch) = identity::epoch_held(&session) else { return json };
    let slot = identity::slot("agent", Some(&session), Some(epoch));
    let Some(by) = identity::uri(&slot) else { return json };
    let what = crate::custody::without(&v, "act");
    let named = v.get("name").and_then(Value::as_str).map_or_else(|| op.clone(), |n| format!("{op} {n}"));
    let hex: String = sha2::Sha256::digest(what.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let subject = Subject { name: format!("an act of session {session}: {named}"), digest: BTreeMap::from([("sha256".to_owned(), hex)]), ..Subject::default() };
    let body = serde_json::to_value(Act { v: statement::VERSION, by: by.clone(), session: session.clone(), epoch, op: named }).unwrap_or(Value::Null);
    match sign_as(&slot, &by, PAYLOAD_ACT, &Statement { subject: vec![subject], ..Statement::new(body) }) {
        Ok(bundle) => {
            if let Some(o) = v.as_object_mut() {
                o.insert("act".into(), Value::String(bundle.to_string()));
            }
            v.to_string()
        }
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %session, epoch, error = %format!("{e:#}"), "an act couldn't be signed: it goes unsigned");
            json
        }
    }
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

/// Whether this process signs its sessions' hand-overs: not a test build's `unsigned` fault, which
/// stands in for a runner from before them.
pub fn signs() -> bool {
    !crate::runner::chaos::get().is_some_and(|c| c.unsigned)
}

/// The hand-on of session `session` as it leaves epoch `epoch` with `events`, for `why`: signed by
/// its identity at that epoch, if this process holds it, and sent to the server; none if not (it
/// goes unsigned, as before).
pub fn hand_on(session: &str, epoch: u64, events: &[Event], why: &str) -> Option<Value> {
    if !signs() {
        return None;
    }
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
    if !signs() {
        return;
    }
    let Some(signed) = Receipt::of(session, epoch, events, hand_on) else { return };
    STATE.lock().waiting.insert((session.to_owned(), epoch), Waiting { signed: Some(signed), held: false, blue_green: false });
}

/// Session `session` taken over blue/green from this machine's runner process being upgraded, at
/// epoch `epoch` with `events`, its `hand_on` to the next if that process signed one (none: it's
/// from before signed hand-overs): it goes on at `epoch` as before, and its ask for the next waits
/// for this process to be promoted ([`promoted`]), with the receipt of a hand-on; a rollback before
/// then leaves it to the process it came from, the hand-on lapsing.
pub fn took_blue_green(session: &str, epoch: u64, events: &[Event], hand_on: Option<&Value>) {
    if !signs() {
        return;
    }
    let next = epoch.saturating_add(1);
    let signed = hand_on.and_then(|h| Receipt::of(session, next, events, h));
    STATE.lock().waiting.insert((session.to_owned(), next), Waiting { signed, held: true, blue_green: true });
}

/// This process was promoted (blue/green): the sessions it took over, each at the epoch it goes
/// on at, whose asks for the next epoch go now.
pub fn promoted() -> Vec<(String, u64)> {
    let mut st = STATE.lock();
    st.waiting.iter_mut().filter(|(_, w)| w.held).map(|((s, e), w)| {
        w.held = false;
        (s.clone(), e.saturating_sub(1))
    }).collect()
}

/// The epoch to ask for session `session`'s identity at, which goes on at `epoch`: the next, once
/// its blue/green take-over's ask may go; none while it's held (until this process is promoted).
pub fn to_ask(session: &str, epoch: u64) -> Option<u64> {
    match STATE.lock().waiting.get(&(session.to_owned(), epoch.saturating_add(1))) {
        Some(w) if w.held => None,
        Some(_) => Some(epoch.saturating_add(1)),
        None => Some(epoch),
    }
}

/// The ask for session `session`'s identity at `epoch`, carrying the hand-on it was taken over
/// with and this process's receipt for it, signed with the key the ask is for: kiln records both
/// before the epoch rises. As it was when no receipt waits (it wasn't taken over here at `epoch`,
/// or its giver signed nothing) or the receipt can't be signed.
pub fn carrying(session: &str, epoch: u64, mut ask: IdentityAsk) -> IdentityAsk {
    let key = (session.to_owned(), epoch);
    let Some((hand_on, receipt)) = STATE.lock().waiting.get(&key).filter(|w| !w.held).and_then(|w| w.signed.as_ref()).map(|r| (r.hand_on.clone(), r.statement(epoch))) else { return ask };
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
/// the ask, and kiln recorded it. Whether the session goes on at `epoch` now (a blue/green
/// take-over's: it went on at the one before until then).
pub fn certified(session: &str, epoch: u64) -> bool {
    STATE.lock().waiting.remove(&(session.to_owned(), epoch)).is_some_and(|w| w.blue_green)
}

/// Whether session command `json` the server sent is a hand-over's word (a local `MoveTo`: hand
/// it to this machine's newest process; `HandedOver`: the server's word on such a hand-over): about
/// the hold of the process it's sent to, never another's.
pub fn hand_overs_word(json: &str) -> bool {
    use crate::harness::model::Command;
    matches!(serde_json::from_str::<Command>(json), Ok(Command::MoveTo { local: true, .. } | Command::HandedOver { .. }))
}

/// Whether hand-over's word `json`, about session `session`, is this process's to apply (docs/ci.md
/// W27-R08; the 14:18 drop): signed by kiln's server (its chain to kiln's root, `root`), for this
/// process (runner process `instance` of `machine`), at the epoch it holds the session at here
/// (`epoch`; none: at none, as once it let go, when only the process is checked). Any other command
/// is this process's as before; so is a hand-over's word from a server that doesn't sign them
/// (`signs`: it doesn't announce `CAP_CUSTODY`), or to this process when it doesn't say it checks
/// them (a test build's `unsigned`, as a runner from before: kiln sends it words unsigned). Why
/// not, when it isn't: kiln is told, and records it with whom the word was for.
pub fn word_for_me(session: &str, json: &str, signs: bool, root: &[u8], machine: &str, instance: &str, epoch: Option<u64>) -> Result<(), String> {
    if !hand_overs_word(json) || !self::signs() {
        return Ok(());
    }
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let word = v.get("word").and_then(Value::as_str);
    let checked = match word {
        None if !signs => return Ok(()),
        None => Err("it isn't signed: kiln signs every word it says of a hand-over".to_owned()),
        Some(w) => addressed(session, &v, w, root, machine, instance, epoch),
    };
    if let Err(why) = &checked {
        refused_word(session, word, why);
    }
    checked
}

fn addressed(session: &str, v: &Value, word: &str, root: &[u8], machine: &str, instance: &str, epoch: Option<u64>) -> Result<(), String> {
    use kiln_ci::identity::chain::{Actor, Identity};
    let bundle: Value = serde_json::from_str(word).map_err(|e| format!("its signature can't be read: {e}"))?;
    let c = statement::verify(&bundle, root, &[], crate::util::now_unix()).map_err(|e| e.to_string())?;
    // kiln's server alone signs words, under kiln's root.
    let server = if Identity::parse(&c.custodian).is_ok_and(|i| matches!(i.actor, Actor::Server)) { c.custodian.clone() } else { String::new() };
    kiln_ci::identity::custody::addressed(&c, &server, session, crate::custody::unworded(v).as_bytes(), machine, instance, epoch).map(|_| ()).map_err(|e| e.to_string())
}

/// A hand-over's word refused here, because `why`: kiln is told, and records it.
pub fn refused_word(session: &str, word: Option<&str>, why: &str) {
    tracing::warn!(target: "kiln.custody", %session, %why, "a hand-over's word refused: it isn't this process's");
    STATE.lock().once.push_back(serde_json::json!({ "refused_word": { "session": session, "why": why, "word": word } }).to_string());
}

/// The records to send now, oldest first: each kept, to send again after a reconnect, but a
/// refusal of a hand-over's word, sent once.
pub fn drain() -> Vec<String> {
    let mut st = STATE.lock();
    let out: Vec<String> = st.outbox.drain(..).collect();
    for r in &out {
        st.sent.push_back(r.clone());
    }
    while st.sent.len() > KEEP {
        st.sent.pop_front();
    }
    let once: Vec<String> = st.once.drain(..).collect();
    out.into_iter().chain(once).collect()
}

/// What was sent, to send again after a reconnect (the server takes each once).
pub fn sent() -> Vec<String> {
    STATE.lock().sent.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W27-R08: a session taken over blue/green from a runner that signed nothing (one from before
    /// signed hand-overs) asks for its next epoch only once this runner is promoted, carrying
    /// nothing, and goes on at it once that's certified.
    #[test]
    fn w27_r08_a_blue_green_take_over_asks_once_promoted() {
        took_blue_green("bg-unsigned", 3, &[], None);
        assert_eq!(to_ask("bg-unsigned", 3), None, "held until this runner is promoted");
        assert!(promoted().contains(&("bg-unsigned".to_owned(), 3)));
        assert_eq!(to_ask("bg-unsigned", 3), Some(4), "then its next epoch's");
        let ask = carrying("bg-unsigned", 4, IdentityAsk::default());
        assert!(ask.hand_on.is_none() && ask.receipt.is_none(), "nothing signed to carry: kiln records it unsigned");
        assert!(certified("bg-unsigned", 4), "certified, it goes on at epoch 4");
        assert_eq!(to_ask("bg-unsigned", 4), Some(4));
        assert!(!certified("bg-unsigned", 4), "once");
    }
}

