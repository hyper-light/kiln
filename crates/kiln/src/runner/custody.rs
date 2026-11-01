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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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

/// Work kiln's server handed an agent session this process holds (a plan's step, a subagent's
/// task, what a person handed off), its receipt to sign as the session takes it up: at its next
/// turn (docs/ci.md W27-R08; the Transfers plan's step 6).
struct Work {
    subject: Vec<Subject>,
    /// The hand-on's digest, which the receipt follows.
    follows: String,
    /// Whom it came from: the hand-on's signer, or the giver kiln took it for.
    from: String,
    why: String,
}

#[derive(Default)]
struct State {
    waiting: BTreeMap<(String, u64), Waiting>,
    /// Work handed to each session, not yet taken up.
    handed: BTreeMap<String, Vec<Work>>,
    /// The sessions whose turn is under way, as their harness last said.
    running: BTreeSet<String>,
    /// Sessions that took work up before this process held their identity: signed for once it
    /// does ([`take_up_due`]).
    due: BTreeSet<String>,
    /// The receipt this process signed for each session at each epoch: what its hand-on follows.
    receipts: BTreeMap<(String, u64), String>,
    outbox: VecDeque<String>,
    sent: VecDeque<String>,
    /// The machine's runner role, handed to this process (a blue/green upgrade's new build) by the
    /// one it took over from: its hand-on, to answer with this process's receipt once its own
    /// identity is certified ([`role_due`]).
    role: Option<Value>,
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

/// Whether this runner's server checks its records of the acts it passes on (`CAP_RELAYS`), as it
/// last said (docs/ci.md W27-R08's relays).
static SERVER_RELAYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The server says what it can do: whether it checks the records of the acts this runner passes
/// on to it.
pub fn server_relays(yes: bool) {
    SERVER_RELAYS.store(yes, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process sends kiln its custody records: it signs them, and its server takes them
/// (not a pod's ephemeral runner, nor one whose server is from before).
pub(crate) fn sends() -> bool {
    signs() && SERVER_CHECKS.load(std::sync::atomic::Ordering::Relaxed)
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
    // A subagent started for a task: the task is handed on, signed by this session, to kiln's
    // server, which starts the subagent and passes the task on to it unchanged (docs/ci.md W27-R08;
    // the Transfers plan's step 6). The act signs it too.
    if op == "call"
        && v.get("name").and_then(Value::as_str) == Some("agents.start")
        && let Some(task) = v.pointer("/input/prompt").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map(str::to_owned)
        && let Some(hand_on) = task_hand_on(&slot, &by, &session, &task)
        && let Some(o) = v.as_object_mut()
    {
        o.insert("hand_on".into(), Value::String(hand_on.to_string()));
    }
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

/// A custody record this process signed (its receipts of a leg's inputs, `runner::signed`), sent
/// to kiln with the others, oldest first (`AgentMsg::Custody`).
pub(crate) fn send(bundle: &Value) {
    queue(bundle);
}

/// What kiln is told once, not again after a reconnect: a refusal of something another runner
/// handed this one over a reach link (`crate::reach::custody::refused`).
pub(crate) fn send_once(record: &Value) {
    STATE.lock().once.push_back(record.to_string());
}

/// `s` signed as `payload_type` with slot `slot`'s key, its body naming that identity (`by`): its
/// bundle. What a reach link's custody records are signed with (`crate::reach::custody`).
pub(crate) fn sign_record(slot: &str, by: &str, payload_type: &str, s: &Statement) -> anyhow::Result<Value> {
    sign_as(slot, by, payload_type, s)
}

/// This process handed a peer runner an object of its cache (docs/ci.md W24-R27; the Transfers
/// plan's step 12): its hand-on to that runner process (`to`, as kiln names it), signed as this
/// process and sent to kiln. The peer builds the same hand-on, and its receipt follows it.
pub(crate) fn peer_object_handed(to: &str, repo: &str, digest: &str) {
    if !sends() {
        return;
    }
    let Some(by) = identity::uri("runner") else { return };
    match sign("runner", &by, &kiln_ci::identity::custody::peer_object_hand_on(&by, to, repo, digest)) {
        Ok(b) => queue(&b),
        Err(e) => tracing::warn!(target: "kiln.custody", %repo, object = %digest, error = %format!("{e:#}"), "this runner's hand-on of an object to a peer couldn't be signed"),
    }
}

/// This process took an object of `repo` from the peer runner process `from` (as kiln names it):
/// its receipt of what the copy hashed to (`got`), following the peer's hand-on of the object it
/// asked for (`want`), built here as the peer builds it, signed as this process and sent to kiln.
/// A copy that isn't what its name says is the break kiln records at that hop, naming the peer.
pub(crate) fn peer_object_taken(from: &str, repo: &str, want: &str, got: &str) {
    if !sends() {
        return;
    }
    let Some(by) = identity::uri("runner") else { return };
    let Ok(payload) = statement::payload(&kiln_ci::identity::custody::peer_object_hand_on(from, &by, repo, want)) else { return };
    let hand_on = statement::digest(PAYLOAD_CUSTODY, &payload);
    match sign("runner", &by, &kiln_ci::identity::custody::peer_object_receipt(&by, from, repo, got, &hand_on)) {
        Ok(b) => queue(&b),
        Err(e) => tracing::warn!(target: "kiln.custody", %repo, object = %want, error = %format!("{e:#}"), "this runner's receipt of a peer's object couldn't be signed"),
    }
}

/// This runner process's ends of conversation store feeds (docs/ci.md W27-R08; the Transfers plan's
/// step 12; `crate::convo::feed::Custodian`): its hand-on of each batch it sends kiln's server (of
/// its own store, or of its copy of the server's sent back to restore it), and its receipt of each
/// batch of the server's store it takes into its copy. Signed as this process, once kiln takes
/// custody from it; the server keeps the hand-ons that come with the batches, and this process's
/// receipts go to kiln with its other records.
pub struct Feed;

impl crate::convo::feed::Custodian for Feed {
    fn hand_on(&self, thing: Subject) -> futures::future::BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            if !sends() {
                return None;
            }
            let by = identity::uri("runner")?;
            // kiln's server, of the organization this process is certified in.
            let org = by.strip_prefix("kiln://")?.split('/').next()?.to_owned();
            let body = Body { to: Some(crate::custody::server_uri(&org)), why: "a batch of this runner's conversation store".into(), ..Body::new(Kind::HandedOn, &by) };
            match sign("runner", &by, &Statement { subject: vec![thing], ..Statement::new(to_value(&body)) }) {
                Ok(b) => Some(b.to_string()),
                Err(e) => {
                    tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "this runner's hand-on of a conversation store's batch couldn't be signed");
                    None
                }
            }
        })
    }

    fn received(&self, _machine: &str, hand_on: &str, thing: Subject) {
        if !sends() {
            return;
        }
        let (Some(by), Ok(given)) = (identity::uri("runner"), serde_json::from_str::<Value>(hand_on)) else { return };
        let Some(from) = giver_of(&given) else { return };
        let body = Body { from: Some(from), why: "a batch of kiln's conversation store, for the copy this runner keeps".into(), ..Body::new(Kind::Received, &by) };
        match sign("runner", &by, &Statement { subject: vec![thing], follows: vec![digest_of(&given)], ..Statement::new(to_value(&body)) }) {
            Ok(b) => queue(&b),
            Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "this runner's receipt of a batch of kiln's conversation store couldn't be signed"),
        }
    }
}

/// The digest of a bundle's statement ("" when it can't be read).
pub(crate) fn digest_of(bundle: &Value) -> String {
    use base64::Engine as _;
    let env = bundle.get("dsseEnvelope");
    let ty = env.and_then(|e| e.get("payloadType")).and_then(Value::as_str);
    let payload = env.and_then(|e| e.get("payload")).and_then(Value::as_str).and_then(|p| base64::engine::general_purpose::STANDARD.decode(p).ok());
    match (ty, payload) {
        (Some(t), Some(p)) => statement::digest(t, &p),
        _ => String::new(),
    }
}

/// A bundle's statement, unchecked: the server checks it.
pub(crate) fn statement_of(bundle: &Value) -> Option<Statement> {
    use base64::Engine as _;
    let payload = bundle.get("dsseEnvelope")?.get("payload")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A bundle's statement's body, unchecked: the server checks it.
fn body_of(bundle: &Value) -> Option<Value> {
    statement_of(bundle).map(|s| s.body)
}

/// Whom a hand-on says its thing comes from: the giver a take hands on for, else its signer.
pub(crate) fn giver_of(hand_on: &Value) -> Option<String> {
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
        Some(w) => addressed(session, &v, w, root, machine, instance, epoch, crate::util::now_unix()),
    };
    if let Err(why) = &checked {
        refused_word(session, word, why);
    }
    checked
}

/// Whether `word`, the signature beside hand-over's word `v` about session `session`, makes it this
/// process's as of `at` ([`word_for_me`]'s check; the transfers simulation, `crate::sim::transfers`,
/// runs it too). Why not, when it doesn't.
#[allow(clippy::too_many_arguments)]
pub(crate) fn addressed(session: &str, v: &Value, word: &str, root: &[u8], machine: &str, instance: &str, epoch: Option<u64>, at: i64) -> Result<(), String> {
    use kiln_ci::identity::chain::{Actor, Identity};
    let bundle: Value = serde_json::from_str(word).map_err(|e| format!("its signature can't be read: {e}"))?;
    let c = statement::verify(&bundle, root, &[], at).map_err(|e| e.to_string())?;
    // kiln's server alone signs words, under kiln's root.
    let server = if Identity::parse(&c.custodian).is_ok_and(|i| matches!(i.actor, Actor::Server)) { c.custodian.clone() } else { String::new() };
    kiln_ci::identity::custody::addressed(&c, &server, session, crate::custody::unworded(v).as_bytes(), machine, instance, epoch).map(|_| ()).map_err(|e| e.to_string())
}

/// A hand-over's word refused here, because `why`: kiln is told, and records it.
pub fn refused_word(session: &str, word: Option<&str>, why: &str) {
    tracing::warn!(target: "kiln.custody", %session, %why, "a hand-over's word refused: it isn't this process's");
    STATE.lock().once.push_back(serde_json::json!({ "refused_word": { "session": session, "why": why, "word": word } }).to_string());
}

/// Agent session `session`'s hand-on of `task`, a subagent's, to kiln's server, which starts the
/// subagent and passes the task on to it unchanged: signed with slot `slot`'s key, as `by`.
fn task_hand_on(slot: &str, by: &str, session: &str, task: &str) -> Option<Value> {
    let org = kiln_ci::identity::chain::Identity::parse(by).ok()?.org;
    let body = Body { to: Some(crate::custody::server_uri(&org)), why: "a subagent's task".into(), ..Body::new(Kind::HandedOn, by) };
    let s = Statement { subject: vec![crate::custody::work(&format!("a subagent's task from agent {session}"), &[session, task])], ..Statement::new(to_value(&body)) };
    match sign(slot, by, &s) {
        Ok(bundle) => Some(bundle),
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %session, error = %format!("{e:#}"), "a subagent's task couldn't be signed for: it goes unsigned");
            None
        }
    }
}

/// kiln's server handed one of this process's agent sessions work (`json`: `{session, hand_on}`,
/// `ServerMsg::Custody`): the session's receipt is signed as its next turn starts ([`status`]).
/// Asked again for the same hand-on (after a reconnect), it's kept once. Or it handed this process
/// a leg's inputs (`{leg, hand_on: [..]}`, the Transfers plan's step 11): kept for the leg, whose
/// end this process signs its receipts of them at (`runner::signed`).
pub fn handed(json: &str) {
    if !signs() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(json) else { return };
    // kiln's record of a run of a reach link's frames its relay passed on to this process
    // (docs/ci.md W27-R08's relays).
    if let Some(r) = v.get("reach") {
        return reach_record(r);
    }
    let bundle = |b: &Value| match b {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        b => b.clone(),
    };
    if let Some(job) = v.get("leg").and_then(Value::as_str) {
        for b in v.get("hand_on").and_then(Value::as_array).into_iter().flatten() {
            crate::runner::signed::kiln_handed(job, bundle(b));
        }
        return;
    }
    let Some(session) = v.get("session").and_then(Value::as_str) else { return };
    let hand_on = match v.get("hand_on") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        Some(b) => b.clone(),
        None => Value::Null,
    };
    let Some(statement) = statement_of(&hand_on) else {
        return tracing::warn!(target: "kiln.custody", %session, "work handed with a hand-on that can't be read: no receipt signed");
    };
    let follows = digest_of(&hand_on);
    let from = giver_of(&hand_on).unwrap_or_default();
    let why = statement.body.get("why").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut st = STATE.lock();
    let work = st.handed.entry(session.to_owned()).or_default();
    if !work.iter().any(|w| w.follows == follows) {
        tracing::info!(target: "kiln.custody", %session, %why, "work handed: its receipt is signed as the session takes it up");
        work.push(Work { subject: statement.subject, follows, from, why });
    }
}

/// Agent session `session`'s harness says it's `status` now. As a turn starts, the work handed to
/// it is taken up: its receipts signed with its identity at the epoch this process holds it at
/// (until that's certified, they wait: [`take_up_due`]). Whether any was signed.
pub fn status(session: &str, status: &crate::harness::model::Status) -> bool {
    let started = {
        let mut st = STATE.lock();
        let started = if matches!(status, crate::harness::model::Status::Running) {
            st.running.insert(session.to_owned())
        } else {
            st.running.remove(session);
            false
        };
        started && st.handed.contains_key(session)
    };
    started && take_up(session)
}

/// Signs session `session`'s receipts of the work handed to it, which it took up: with its
/// identity at the newest epoch this process holds a certificate for. None held yet: it's due,
/// signed once one is. Whether any was signed.
fn take_up(session: &str) -> bool {
    let held = identity::epoch_held(session).map(|e| identity::slot("agent", Some(session), Some(e))).and_then(|slot| identity::uri(&slot).map(|by| (slot, by)));
    let Some((slot, by)) = held else {
        STATE.lock().due.insert(session.to_owned());
        return false;
    };
    let work = {
        let mut st = STATE.lock();
        st.due.remove(session);
        st.handed.remove(session).unwrap_or_default()
    };
    let mut signed = false;
    for w in work {
        let body = Body { from: Some(w.from.clone()), why: w.why.clone(), ..Body::new(Kind::Received, &by) };
        let s = Statement { subject: w.subject.clone(), follows: vec![w.follows.clone()], ..Statement::new(to_value(&body)) };
        match sign(&slot, &by, &s) {
            Ok(bundle) => {
                tracing::info!(target: "kiln.custody", %session, why = %w.why, "took up work handed to it: signed its receipt");
                queue(&bundle);
                signed = true;
            }
            Err(e) => {
                tracing::warn!(target: "kiln.custody", %session, error = %format!("{e:#}"), "couldn't sign a receipt of work handed: it waits for the next turn");
                STATE.lock().handed.entry(session.to_owned()).or_default().push(w);
            }
        }
    }
    signed
}

/// The sessions that took work up before this process held their identity, signed for now that
/// it may (each heartbeat). Whether any was.
pub fn take_up_due() -> bool {
    let due: Vec<String> = STATE.lock().due.iter().cloned().collect();
    let mut signed = false;
    for s in &due {
        signed |= take_up(s);
    }
    signed
}

/// Session `session` closed here: the work handed to it that it never took up goes with it.
pub fn closed(session: &str) {
    let mut st = STATE.lock();
    st.handed.remove(session);
    st.running.remove(session);
    st.due.remove(session);
}

/// A machine's runner role, the thing a hand-over of it is about (docs/ci.md W27-R08; the
/// Transfers plan's step 8): "runner role of <machine>", by a digest of its organization and
/// machine, which the process handing it on and the one taking it work out alike.
pub fn role(org: &str, machine: &str) -> Subject {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(format!("kiln-runner-role/{org}/{machine}").as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    Subject { name: format!("runner role of {machine}"), digest: BTreeMap::from([("sha256".to_owned(), digest)]), ..Subject::default() }
}

/// (A blue/green upgrade's runner being upgraded.) Its hand-on of the machine's runner role to the
/// new build, runner process `to` of this machine (its instance), signed as this process's runner
/// identity: kept to send to the server, and given to the new build, which answers it with its
/// receipt ([`role_taken`]). None when this process holds no certified identity, or (logged) its
/// signature failed.
#[cfg(unix)]
pub fn role_hand_on(to: &str) -> Option<String> {
    use kiln_ci::identity::chain::{Actor, Identity};
    let by = identity::uri("runner")?;
    let me = Identity::parse(&by).ok()?;
    let Actor::Runner { enrolled_by, machine, .. } = &me.actor else { return None };
    let green = Identity::new(me.org.clone(), Actor::Runner { enrolled_by: enrolled_by.clone(), machine: machine.clone(), instance: to.to_owned() }).uri().ok()?;
    let b = Body { to: Some(green), why: format!("a blue/green upgrade of {machine}: its new build holds its runner role"), ..Body::new(Kind::HandedOn, "") };
    match sign("runner", &by, &Statement { subject: vec![role(&me.org, machine)], ..Statement::new(to_value(&b)) }) {
        Ok(bundle) => {
            queue(&bundle);
            Some(bundle.to_string())
        }
        Err(e) => {
            tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "the runner role's hand-on to the new build couldn't be signed");
            None
        }
    }
}

/// (A blue/green upgrade's new build.) The machine's runner role, handed to this process by the one
/// it took over from (`hand_on`, its bundle as JSON): answered with this process's receipt, signed
/// as its runner identity once that's certified ([`role_due`]), both sent to the server.
#[cfg(unix)]
pub fn role_taken(hand_on: &str) {
    let Ok(h) = serde_json::from_str::<Value>(hand_on) else {
        tracing::warn!(target: "kiln.custody", "the runner role's hand-on from the runner taken over from can't be read: no receipt signed");
        return;
    };
    STATE.lock().role = Some(h);
    role_due();
}

/// The runner role's receipt, signed once this process's runner identity is certified (as it
/// connects, and again on each connection): called as an identity is given, and on each beat.
pub fn role_due() {
    let Some(h) = STATE.lock().role.clone() else { return };
    let Some(by) = identity::uri("runner") else { return };
    let (Some(s), Some(from)) = (statement_of(&h), giver_of(&h)) else {
        STATE.lock().role = None;
        return;
    };
    let b = Body { from: Some(from), why: "this process holds the machine's runner role".into(), ..Body::new(Kind::Received, "") };
    match sign("runner", &by, &Statement { subject: s.subject, follows: vec![digest_of(&h)], ..Statement::new(to_value(&b)) }) {
        Ok(receipt) => {
            let mut st = STATE.lock();
            st.role = None;
            st.outbox.push_back(h.to_string());
            st.outbox.push_back(receipt.to_string());
            tracing::info!(target: "kiln.custody", "signed for the machine's runner role, handed on by the runner taken over from");
        }
        Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "the runner role's receipt couldn't be signed: tried again at the next beat"),
    }
}

// ---------------------------------------------------------------- relays (docs/ci.md W27-R08)

/// When this process started (unix ms): its relays' streams are named for it.
static STARTED: LazyLock<i64> = LazyLock::new(crate::util::now_ms);

/// The most records of relayed words a process remembers seeing, to refuse one sent again.
const SEEN_MAX: usize = 1024;

/// What this process relays, and is relayed (docs/ci.md W27-R08's relays).
#[derive(Default)]
struct Relaying {
    /// Its stream of records of the calls it passes on to kiln, one per connection to kiln, and the
    /// last number on it.
    calls: String,
    called: u64,
    connections: u64,
    /// The act it last passed on: what a test build's faulty relay swaps in (`relay_calls=swap`).
    last_act: Option<String>,
    /// The streams relays number to this process (blue/green's old build's), where each stands.
    numbering: kiln_ci::identity::relay::Numbering,
    /// The runner process this one took over from recorded what it forwarded: each frame since
    /// comes with its record.
    forwards_recorded: bool,
    /// Records of relayed words seen here, by digest, newest last: one again is refused.
    seen: VecDeque<String>,
    /// Records of the runner process this one took over from of passing an agent's words on to
    /// this one (blue/green), by the act's digest: the last relay's of those words.
    forwarded_words: BTreeMap<String, Value>,
    /// Receipts to sign once this process's own identity is certified, of what relays passed on to
    /// it, each with the records it follows, to send kiln with it.
    due: Vec<(Statement, Vec<Value>)>,
}

static RELAYING: LazyLock<Mutex<Relaying>> = LazyLock::new(Default::default);

/// kiln's server as the identity `uri` (of this organization's) names it: `kiln://<org>/server`.
fn server_of(uri: &str) -> Option<String> {
    let org = uri.strip_prefix("kiln://")?.split('/').next().filter(|o| !o.is_empty())?;
    Some(crate::custody::server_uri(org))
}

/// The runner process `instance` of the machine whose process `uri` is: the same identity, another
/// instance.
fn sibling(uri: &str, instance: &str) -> Option<String> {
    let (head, _) = uri.rsplit_once("/instance/")?;
    Some(format!("{head}/instance/{instance}"))
}

/// Whether identity `uri` names runner process `instance` of `machine`.
fn names_process(uri: &str, machine: &str, instance: &str) -> bool {
    uri.ends_with(&format!("/machine/{machine}/instance/{instance}"))
}

/// This process connected to kiln (again): the calls it passes on from now on are numbered on a
/// stream of their own, from 1, as kiln takes them, in the order they come over that connection.
pub fn calls_connected() {
    let mut r = RELAYING.lock();
    r.connections = r.connections.saturating_add(1);
    r.calls = format!("calls to kiln, from this process started {} over its connection {}", *STARTED, r.connections);
    r.called = 0;
}

/// Ask `json` (an agent session's act beside it: [`signed_ask`]) as this process passes it on to
/// kiln (docs/ci.md W27-R08's relays): with its record of doing so (`passed_on`), a passed-on of
/// the act by its digest, from the session that signed it, to kiln's server, numbered next on its
/// stream for this connection. Called as the ask goes, in the order asks go. As it was with no act
/// beside it, while this process's own identity isn't certified, or to a server from before
/// (`CAP_RELAYS`: it would take the record for a part of what the act signs). None when a test
/// build's faulty relay drops it (`relay_calls=drop`, every second one): numbered, recorded, and
/// never sent.
pub fn passed_on_ask(json: String) -> Option<String> {
    use crate::runner::chaos::RelayFault;
    use kiln_ci::identity::relay::{Pass, passed_on};
    if !signs() || !SERVER_RELAYS.load(std::sync::atomic::Ordering::Relaxed) {
        return Some(json);
    }
    let Ok(mut v) = serde_json::from_str::<Value>(&json) else { return Some(json) };
    let Some(act_json) = v.get("act").and_then(Value::as_str).map(str::to_owned) else { return Some(json) };
    let (Ok(act), Some(by)) = (serde_json::from_str::<Value>(&act_json), identity::uri("runner")) else {
        tracing::info!(target: "kiln.custody", "an act passed on to kiln unrecorded: this runner's own identity isn't certified yet");
        return Some(json);
    };
    let (Some(s), Some(server)) = (statement_of(&act), server_of(&by)) else { return Some(json) };
    let text = |k: &str| s.body.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let (from, op, session) = (text("by"), text("op"), text("session"));
    let digest = digest_of(&act);
    let (stream, seq, before) = {
        let mut r = RELAYING.lock();
        if r.calls.is_empty() {
            r.connections = r.connections.saturating_add(1);
            r.calls = format!("calls to kiln, from this process started {} over its connection {}", *STARTED, r.connections);
        }
        r.called = r.called.saturating_add(1);
        (r.calls.clone(), r.called, r.last_act.replace(act_json))
    };
    let pass = Pass { by: &by, from: &from, to: &server, stream: &stream, seq, why: "an agent's call, passed on to kiln" };
    let record = match sign("runner", &by, &passed_on(&pass, vec![crate::relays::act_thing(&session, &op, &digest)], vec![digest])) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: "kiln.custody", %session, error = %format!("{e:#}"), "this runner's record of passing an act on couldn't be signed: it goes without");
            return Some(json);
        }
    };
    let Some(o) = v.as_object_mut() else { return Some(json) };
    match crate::runner::chaos::get().and_then(|c| c.relay_calls).filter(|_| RelayFault::strikes(seq)) {
        Some(RelayFault::Drop) => {
            tracing::warn!(target: "kiln.custody", %session, seq, "chaos: an act this runner numbered and recorded passing on, dropped");
            return None;
        }
        Some(RelayFault::Swap) => {
            if let Some(before) = before {
                o.insert("act".into(), Value::String(before));
            }
        }
        Some(RelayFault::Alter) => {
            o.insert("altered".into(), Value::Bool(true));
        }
        None => {}
    }
    o.insert("passed_on".into(), Value::String(record.to_string()));
    Some(v.to_string())
}

/// What a receiver reports to kiln of a relay's record that doesn't hold, and what it refused
/// because of it: sent once (docs/ci.md W27-R08's relays).
fn relay_break(record: Option<&Value>, relay: &str, u: &kiln_ci::identity::relay::Unrelayed, refused: &str) {
    let r = serde_json::json!({ "relay_break": { "record": record.map(Value::to_string), "relay": relay, "code": u.code(), "detail": u.to_string(), "refused": refused } });
    tracing::warn!(target: "kiln.custody", %relay, detail = %u, %refused, "a relay's record doesn't hold");
    STATE.lock().once.push_back(r.to_string());
}

/// A receipt of what a relay passed on to this process, `things` following its record `record`
/// (`relay`'s), signed as this process's runner identity, now or once it's certified
/// ([`relay_due`]), and sent to kiln with `records` (the relays' records it follows, which kiln may
/// not have yet).
fn relay_receipt(relay: &str, things: Vec<Subject>, record: &str, why: &str, records: Vec<Value>) {
    let s = kiln_ci::identity::relay::receipt("", relay, things, record, why);
    RELAYING.lock().due.push((s, records));
    relay_due();
}

/// The receipts of what relays passed on to this process, signed once its own identity is
/// certified: called as one is made, as an identity is given, and on each beat.
pub fn relay_due() {
    let Some(by) = identity::uri("runner") else { return };
    let due = std::mem::take(&mut RELAYING.lock().due);
    for (s, records) in due {
        match sign("runner", &by, &s) {
            Ok(receipt) => {
                let mut st = STATE.lock();
                for r in records {
                    st.outbox.push_back(r.to_string());
                }
                st.outbox.push_back(receipt.to_string());
            }
            Err(e) => tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a receipt of what a relay passed on couldn't be signed"),
        }
    }
}

/// Words of another agent kiln passed on to session `session`, which this process (runner process
/// `instance` of `machine`) holds: `relayed` in its message's JSON (`json`, as sent), checked end to
/// end before the session takes them (docs/ci.md W27-R08's relays). The sender's act, signed under
/// kiln's root (`root`), over the ask it signs, an `agents.send` of these very words to this
/// session; each relay's record in turn, the first following the act, each the one before it, of
/// the act by its digest, from the relay before it, to the next; the last to this process (or to
/// the one this process took over from, whose forwarder passed it on: its record of that, last); and
/// none seen here before. Then this process's receipt, following the last relay's record, goes to
/// kiln. Why not: kiln is told (a break naming the relay), and the session isn't given them. A
/// message with nothing relayed beside it (a person's, a server's from before) goes as before.
pub fn relayed_words(session: &str, json: &str, root: &[u8], machine: &str, instance: &str) -> Result<(), String> {
    use kiln_ci::identity::custody::names;
    use kiln_ci::identity::relay::{Unrelayed, relayed};
    let Ok(v) = serde_json::from_str::<Value>(json) else { return Ok(()) };
    let Some(r) = v.get("relayed") else { return Ok(()) };
    let at = crate::util::now_unix();
    let message = v.get("id").and_then(Value::as_str).unwrap_or_default();
    let refused = format!("message {message} to session {session}");
    let bundle = |x: Option<&Value>| x.and_then(Value::as_str).and_then(|s| serde_json::from_str::<Value>(s).ok());
    let mut records: Vec<Value> = r.get("passed_on").and_then(Value::as_array).into_iter().flatten().filter_map(|x| bundle(Some(x))).collect();
    let (act, ask) = (bundle(r.get("act")), r.get("ask").and_then(Value::as_str).unwrap_or_default());
    let act_digest = act.as_ref().map(digest_of).unwrap_or_default();
    // The last relay's record of passing it on to this process: the forwarder's, when the process
    // this one took over from passed it on.
    if let Some(forwarded) = RELAYING.lock().forwarded_words.remove(&act_digest) {
        records.push(forwarded);
    }
    let last = records.last().cloned();
    let checked: Vec<statement::Checked> = records.iter().filter_map(|b| statement::verify(b, root, &[], at).ok()).collect();
    let relay = checked.last().map(|c| c.custodian.clone()).unwrap_or_else(|| "kiln's server".to_owned());
    let fail_at = |record: Option<&Value>, u: Unrelayed| {
        relay_break(record, u.relay(), &u, &refused);
        Err(format!("the words relayed to it don't hold: {u}"))
    };
    let fail = |u: Unrelayed| fail_at(last.as_ref(), u);
    let unreadable = |why: String| Unrelayed::Unreadable { relay: relay.clone(), why };
    if checked.len() != records.len() || checked.is_empty() {
        return fail(unreadable("a relay's record of passing it on isn't signed under kiln's root".into()));
    }
    // The sender's act, end to end: signed by its originator, over the ask that came, an
    // agents.send of these very words to this session.
    let Some(act) = act else { return fail(unreadable("no act came with it".into())) };
    let Ok(signed) = statement::verify(&act, root, &[], at) else {
        return fail(Unrelayed::Changed { relay: relay.clone(), thing: "the sender's act".into(), algorithm: "signature".into(), was: "its signer's".into(), now: "no one's".into() });
    };
    let ask_digest: String = <sha2::Sha256 as sha2::Digest>::digest(ask.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let covers = signed.statement.subject.iter().any(|t| t.digest.get("sha256").is_some_and(|d| d.eq_ignore_ascii_case(&ask_digest)));
    let asked: Value = serde_json::from_str(ask).unwrap_or_default();
    let said = |p: &str| asked.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    let words = v.get("text").and_then(Value::as_str).map(crate::harness::ops::body_of).unwrap_or_default();
    // For this session: as its sender named it (its id, or the start of it), or, named otherwise (by
    // its name), as kiln's record of passing them on says it found it.
    let to = said("/input/agent");
    let kilns_for = checked.iter().filter_map(|c| kiln_ci::identity::custody::body(c).ok()).filter_map(|b| b.held_for).any(|f| f == session || f.contains(&format!("/agent/{session}/")));
    let for_here = (!to.is_empty() && session.starts_with(&to)) || kilns_for;
    if !covers || signed.payload_type != kiln_ci::identity::custody::PAYLOAD_ACT || said("/name") != "agents.send" || said("/input/text") != words || !for_here {
        let now = if said("/input/text") == words { format!("an ask its act doesn't cover, to {to}") } else { format!("words other than those its sender signed: {words:?}") };
        return fail(Unrelayed::Changed { relay: relay.clone(), thing: "an agent's words".into(), algorithm: "sha256".into(), was: ask_digest, now });
    }
    // Each relay's record, the nearest first: of the act that came, to whom it went next (the
    // next relay, or, the last, this process, named by its machine and instance), following the
    // record of the relay before it, from that relay. The first that doesn't hold names its relay:
    // one that passed on another than it recorded is found before those it took from.
    let thing = kiln_ci::identity::relay::statement_thing(WORDS, &act_digest);
    for (i, c) in checked.iter().enumerate().rev() {
        let here = match checked.get(i + 1) {
            Some(n) => n.custodian.clone(),
            None => kiln_ci::identity::custody::body(c).ok().and_then(|b| b.to).filter(|t| names_process(t, machine, instance)).unwrap_or_else(|| format!("runner {machine} (instance {instance})")),
        };
        let b = match relayed(c, &c.custodian, &here, &thing, true) {
            Ok(b) => b,
            Err(u) => return fail_at(records.get(i), u),
        };
        let before = match i.checked_sub(1).and_then(|j| checked.get(j)) {
            Some(p) => (p.digest.clone(), p.custodian.clone()),
            None => (act_digest.clone(), signed.custodian.clone()),
        };
        let from = b.from.unwrap_or_default();
        if c.statement.follows != [before.0.clone()] || !(names(&from, &before.1) || names(&before.1, &from)) {
            return fail_at(records.get(i), Unrelayed::Unreadable { relay: c.custodian.clone(), why: format!("its record doesn't follow the record of the relay before it ({})", before.1) });
        }
    }
    let Some(nearest) = checked.last() else { return fail(unreadable("no relay's record came with it".into())) };
    let before = (nearest.digest.clone(), nearest.custodian.clone());
    // Sent again: refused.
    let again = {
        let mut st = RELAYING.lock();
        let again = st.seen.contains(&before.0);
        if !again {
            st.seen.push_back(before.0.clone());
            while st.seen.len() > SEEN_MAX {
                st.seen.pop_front();
            }
        }
        again
    };
    if again {
        let last = checked.last();
        return fail(Unrelayed::Again { relay: before.1.clone(), stream: last.and_then(|c| c.statement.stream.clone()).unwrap_or_default(), seq: last.and_then(|c| c.statement.seq).unwrap_or_default() });
    }
    relay_receipt(&before.1, vec![thing], &before.0, "an agent's words, taken by the session they're for", records);
    Ok(())
}

/// (A blue/green upgrade's runner being upgraded.) Where this process forwards kiln's frames to:
/// its new build, runner process `instance` of this machine, as kiln names it, and its stream to it.
/// None while its own identity isn't certified (it records nothing then).
pub fn forwarding_to(instance: &str) -> Option<(String, String)> {
    let by = identity::uri("runner")?;
    let to = sibling(&by, instance)?;
    let stream = format!("frames forwarded to {to}, from this process started {}", *STARTED);
    Some((to, stream))
}

/// (A blue/green upgrade's runner being upgraded.) Its record of passing frame `frame` of kiln's on
/// to its new build (`to`, its stream there, number `seq`: [`forwarding_to`]), signed as this
/// process (docs/ci.md W27-R08's relays): of the signed statements the frame carries
/// ([`statements_in`]), from their signer, following each; or, one carrying an agent's words kiln
/// passed on to this process, of the sender's act, following kiln's record of that. Sent to kiln
/// with its others. None for a frame that carries no signed statement (it goes on unrecorded), or
/// when its record can't be signed.
pub fn forward_record(frame: &[u8], to: &str, stream: &str, seq: u64) -> Option<String> {
    use kiln_ci::identity::relay::{Pass, passed_on, statement_thing};
    let by = identity::uri("runner")?;
    let server = server_of(&by)?;
    let (things, follows, from) = match words_in(frame).filter(|(_, last)| last.to.as_deref().is_some_and(|t| kiln_ci::identity::custody::names(t, &by))) {
        Some((relayed, _)) => {
            let last = relayed.get("passed_on").and_then(Value::as_array).and_then(|l| l.last()).and_then(Value::as_str).and_then(|b| serde_json::from_str::<Value>(b).ok())?;
            let act = relayed.get("act").and_then(Value::as_str).and_then(|b| serde_json::from_str::<Value>(b).ok())?;
            (vec![statement_thing(WORDS, &digest_of(&act))], vec![digest_of(&last)], server)
        }
        None => {
            let signed = statements_in(frame);
            let from = signed.first().and_then(statement_of).and_then(|s| s.body.get("by").and_then(Value::as_str).map(str::to_owned))?;
            (signed.iter().map(|b| statement_thing(FORWARDED, &digest_of(b))).collect(), signed.iter().map(digest_of).collect(), from)
        }
    };
    let pass = Pass { by: &by, from: &from, to, stream, seq, why: "a frame of kiln's, passed on by the runner being upgraded to its new build" };
    match sign("runner", &by, &passed_on(&pass, things, follows)) {
        Ok(b) => {
            queue(&b);
            Some(b.to_string())
        }
        Err(e) => {
            tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "this runner's record of forwarding a frame couldn't be signed");
            None
        }
    }
}

/// What a relay's record names an agent's words by: the sender's act.
const WORDS: &str = "an agent's words";

/// What blue/green's old build's record names a signed statement of kiln's it forwards by.
const FORWARDED: &str = "a statement of kiln's, forwarded";

/// The signed statements server frame `frame` carries for the process it's for (docs/ci.md
/// W27-R08's relays): a hand-over's word (`word`), and the custody records kiln hands it (`hand_on`:
/// work for a session, a leg's inputs; `reach.record`: a run of a reach link's frames). Not an
/// agent's words passed on with their act ([`words_in`]). None for a frame that carries none.
fn statements_in(frame: &[u8]) -> Vec<Value> {
    let bundle = |v: &Value| match v {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        Value::Object(_) => Some(v.clone()),
        _ => None,
    };
    let json = match crate::proto::decode::<crate::proto::ServerMsg>(frame) {
        Ok(crate::proto::ServerMsg::HarnessSend { json, .. } | crate::proto::ServerMsg::HarnessCmd { json, .. } | crate::proto::ServerMsg::Custody { json }) => json,
        _ => return Vec::new(),
    };
    let v: Value = serde_json::from_str(&json).unwrap_or_default();
    let mut out: Vec<Value> = v.get("word").and_then(bundle).into_iter().collect();
    match v.get("hand_on") {
        Some(Value::Array(l)) => out.extend(l.iter().filter_map(bundle)),
        Some(h) => out.extend(bundle(h)),
        None => {}
    }
    out.extend(v.pointer("/reach/record").and_then(bundle));
    out
}

/// An agent's words relayed in server frame `frame` (`relayed` beside a session's message), and
/// the body of the last relay's record of them.
fn words_in(frame: &[u8]) -> Option<(Value, kiln_ci::identity::custody::Body)> {
    let json = match crate::proto::decode::<crate::proto::ServerMsg>(frame).ok()? {
        crate::proto::ServerMsg::HarnessSend { json, .. } | crate::proto::ServerMsg::HarnessCmd { json, .. } => json,
        _ => return None,
    };
    let relayed = serde_json::from_str::<Value>(&json).ok()?.get("relayed")?.clone();
    let last = relayed.get("passed_on")?.as_array()?.last()?.as_str()?.to_owned();
    let body = serde_json::from_value(statement_of(&serde_json::from_str::<Value>(&last).ok()?)?.body).ok()?;
    Some((relayed, body))
}

/// (A blue/green upgrade's new build: runner process `instance` of `machine`.) Frame `frame` of
/// kiln's that the process this one took over from passed on, with its record of that
/// (`passed_on`), checked before this process takes it (docs/ci.md W27-R08's relays). A frame
/// carrying signed statements ([`statements_in`]) or an agent's words comes with that process's
/// record: signed by another runner process of this machine under kiln's root (`root`), to this
/// one, numbered next on its stream here, of those very statements by their digests (the words'
/// own check takes a record of their act as the last relay's: [`relayed_words`]). Then this
/// process's receipt goes to kiln with the record. Once that process has recorded what it forwards,
/// a frame of statements with no record is refused too; a frame with none goes on unrecorded. Why
/// not, kiln told (a break naming that process); a number skipped is told, and the frame taken.
pub fn forwarded(frame: &[u8], passed_on: Option<&str>, root: &[u8], machine: &str, instance: &str) -> Result<(), String> {
    use kiln_ci::identity::relay::{Place, Unrelayed, relayed, statement_thing};
    let words = words_in(frame).map(|(r, _)| r);
    let signed = statements_in(frame);
    let carries = words.is_some() || !signed.is_empty();
    let Some(record) = passed_on.and_then(|b| serde_json::from_str::<Value>(b).ok()) else {
        if carries && RELAYING.lock().forwards_recorded {
            let u = Unrelayed::Unrecorded { relay: "the runner process this one took over from".into(), what: "a frame of kiln's carrying signed statements".into() };
            relay_break(None, u.relay(), &u, "a frame of kiln's, forwarded to the new build");
            return Err(u.to_string());
        }
        return Ok(());
    };
    RELAYING.lock().forwards_recorded = true;
    let refused = "a frame of kiln's, forwarded to the new build";
    let fail = |u: Unrelayed| {
        relay_break(Some(&record), u.relay(), &u, refused);
        Err(u.to_string())
    };
    let at = crate::util::now_unix();
    let c = match statement::verify(&record, root, &[], at) {
        Ok(c) => c,
        Err(e) => return fail(Unrelayed::Unreadable { relay: "the runner process this one took over from".into(), why: e.to_string() }),
    };
    let relay = c.custodian.clone();
    if !relay.contains(&format!("/machine/{machine}/instance/")) || names_process(&relay, machine, instance) {
        return fail(Unrelayed::Forged { relay: format!("a runner process of {machine} other than this one"), signer: relay });
    }
    let here = sibling(&relay, instance).unwrap_or_default();
    // What came: the words' act, when the record is of it; else the statements the frame carries.
    let of_words = words.is_some() && c.statement.subject.iter().any(|t| t.name == WORDS);
    let came: Vec<(Subject, bool)> = match words.as_ref().filter(|_| of_words) {
        Some(r) => {
            let act = r.get("act").and_then(Value::as_str).and_then(|b| serde_json::from_str::<Value>(b).ok()).unwrap_or_default();
            vec![(statement_thing(WORDS, &digest_of(&act)), statement::verify(&act, root, &[], at).is_ok())]
        }
        None => signed.iter().map(|b| (statement_thing(FORWARDED, &digest_of(b)), statement::verify(b, root, &[], at).is_ok())).collect(),
    };
    if came.is_empty() || came.len() != c.statement.subject.len() {
        let named = c.statement.subject.iter().map(|t| t.name.clone()).collect::<Vec<_>>().join(", ");
        return fail(Unrelayed::Changed { relay, thing: named, algorithm: "sha256".into(), was: format!("{} statements", c.statement.subject.len()), now: format!("{} as it came", came.len()) });
    }
    for (thing, genuine) in &came {
        if let Err(u) = relayed(&c, &relay, &here, thing, *genuine) {
            return fail(u);
        }
    }
    let (stream, seq) = (c.statement.stream.clone().unwrap_or_default(), c.statement.seq.unwrap_or_default());
    let place = RELAYING.lock().numbering.take(&relay, &stream, seq);
    match place {
        Ok(Place::Next) => {}
        Ok(Place::AfterGap(gap)) => relay_break(Some(&record), &relay, &gap, ""),
        Err(u) => return fail(u),
    }
    if of_words {
        let digest = came.first().and_then(|(t, _)| t.digest.get("sha256")).map(|h| format!("sha256:{h}")).unwrap_or_default();
        RELAYING.lock().forwarded_words.insert(digest, record);
    } else {
        relay_receipt(&relay, came.into_iter().map(|(t, _)| t).collect(), &c.digest, "a frame of kiln's, taken by the new build", vec![record]);
    }
    Ok(())
}

/// The most reach links whose frames this process keeps the digests of, for kiln's records of them.
const LINKS_KEPT: usize = 256;

/// The most frames' digests a link keeps waiting for kiln's record of them (a run is at most 1,024,
/// recorded within 10 s as frames flow): past it, the oldest go.
const DIGESTS_KEPT: usize = 8192;

/// A reach link's frames that came here from kiln's relay (docs/ci.md W27-R08's relays).
#[derive(Default)]
struct LinkFrames {
    /// The number the next frame to come takes (the first is 1).
    next: u64,
    /// Each frame's SHA-256 by its number, until kiln's record of the run it's in is answered.
    digests: BTreeMap<u64, [u8; 32]>,
    /// When the link ended here: no more of its frames come but those already on their way.
    ended_at: Option<std::time::Instant>,
    /// kiln's records of runs of these frames, each since it came, waiting for the frames it's of.
    waiting: Vec<(std::time::Instant, Value)>,
}

static REACHED: LazyLock<Mutex<BTreeMap<String, LinkFrames>>> = LazyLock::new(Default::default);

/// Link `link`'s frames as they came (new, [`LINKS_KEPT`] at most: past it, one that ended goes,
/// or the first).
fn link_frames<'a>(r: &'a mut BTreeMap<String, LinkFrames>, link: &str) -> &'a mut LinkFrames {
    if !r.contains_key(link) && r.len() >= LINKS_KEPT {
        let old = r.iter().find(|(_, f)| f.ended_at.is_some()).or_else(|| r.iter().next()).map(|(k, _)| k.clone());
        if let Some(k) = old {
            r.remove(&k);
        }
    }
    r.entry(link.to_owned()).or_insert_with(|| LinkFrames { next: 1, ..LinkFrames::default() })
}

/// A frame of reach link `link`, as it came from kiln's relay: its digest kept until
/// kiln's record of the run it's in comes, to answer that with this process's receipt of the frames
/// as they came (docs/ci.md W27-R08's relays). A goodbye (no bytes) carries nothing.
pub fn reach_frame(link: &str, data: &[u8]) {
    if data.is_empty() || !signs() || !SERVER_RELAYS.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let due = {
        let mut r = REACHED.lock();
        let f = link_frames(&mut r, link);
        f.digests.insert(f.next, kiln_ci::identity::relay::digest_of(data));
        f.next = f.next.saturating_add(1);
        if f.digests.len() > DIGESTS_KEPT {
            f.digests.pop_first();
        }
        runs_answered(link, f)
    };
    for (relay, thing, record, changed) in due {
        reach_receipt(link, &relay, thing, &record, changed);
    }
}

/// Reach link `link` ended here: no more of its frames come but those on their way.
pub fn reach_closed(link: &str) {
    if let Some(f) = REACHED.lock().get_mut(link) {
        f.ended_at = Some(std::time::Instant::now());
    }
}

/// kiln's record of a run of reach link frames its relay passed on to this process (`reach` in a
/// `ServerMsg::Custody`): answered with this process's receipt of the frames as they came, once
/// they all have, or [`FRAMES_WAIT`] after it came.
fn reach_record(r: &Value) {
    let (Some(link), Some(record)) = (r.get("link").and_then(Value::as_str), r.get("record").and_then(Value::as_str).and_then(|b| serde_json::from_str::<Value>(b).ok())) else { return };
    let due = {
        let mut reached = REACHED.lock();
        let f = link_frames(&mut reached, link);
        f.waiting.push((std::time::Instant::now(), record));
        runs_answered(link, f)
    };
    for (relay, thing, record, changed) in due {
        reach_receipt(link, &relay, thing, &record, changed);
    }
}

/// How long kiln's record of a run waits for the frames it's of. Its relay passed them on before it
/// sent the record, but they come another way (their lane, then this process's reach node), so a
/// record can come first, even after the link ended here; past this, it's answered with what came.
const FRAMES_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// How long after a reach link ends here its frames wait for kiln's record of them.
const RECORD_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// kiln's records that waited [`FRAMES_WAIT`] for their frames, answered with what came; then reach
/// links that ended a while ago ([`RECORD_WAIT`]) with no record waiting: frames of theirs that came
/// with no record of kiln's relay passing them on (a run it never recorded) are told to kiln, a break
/// naming kiln's server, and the link is forgotten. Called on each beat.
pub fn reach_due() {
    use kiln_ci::identity::relay::Unrelayed;
    let (due, gone) = {
        let mut r = REACHED.lock();
        let mut due = Vec::new();
        for (link, f) in r.iter_mut() {
            due.extend(runs_answered(link, f).into_iter().map(|d| (link.clone(), d)));
        }
        let old: Vec<String> = r.iter().filter(|(_, f)| f.waiting.is_empty() && f.ended_at.is_some_and(|t| t.elapsed() >= RECORD_WAIT)).map(|(k, _)| k.clone()).collect();
        let gone: Vec<(String, Vec<u64>)> = old.into_iter().filter_map(|k| r.remove(&k).map(|f| (k, f.digests.into_keys().collect::<Vec<u64>>()))).filter(|(_, n)| !n.is_empty()).collect();
        (due, gone)
    };
    for (link, (relay, thing, record, changed)) in due {
        reach_receipt(&link, &relay, thing, &record, changed);
    }
    if gone.is_empty() {
        return;
    }
    let Some(server) = identity::uri("runner").and_then(|u| server_of(&u)) else { return };
    for (link, frames) in gone {
        let (first, last) = (frames.first().copied().unwrap_or_default(), frames.last().copied().unwrap_or_default());
        let u = Unrelayed::Unrecorded { relay: server.clone(), what: format!("{} of frames {first} to {last} of reach link {link}", frames.len()) };
        relay_break(None, &server, &u, "");
    }
}

/// (Tests.) Kiln's records waiting for link `link`'s frames as if they had come `by` earlier.
#[cfg(test)]
fn age_reached(link: &str, by: std::time::Duration) {
    for (since, _) in REACHED.lock().get_mut(link).into_iter().flat_map(|f| f.waiting.iter_mut()) {
        *since = since.checked_sub(by).unwrap_or(*since);
    }
}

/// The runs of link `link`'s frames kiln's records of which can be answered now, in any order: each
/// whose frames all came (numbered to its last), or that waited [`FRAMES_WAIT`] for them. Each with
/// the relay (the record's signer), the run as it came here (a thing named as the record names it),
/// the record, and whether what came differs.
fn runs_answered(link: &str, f: &mut LinkFrames) -> Vec<(String, Subject, Value, bool)> {
    use kiln_ci::identity::relay::{FRAMES, Frames, frames_of};
    let run = |record: &Value| statement_of(record).and_then(|s| s.subject.first().cloned()).and_then(|t| frames_of(&t).map(|n| (n, t)));
    let mut out = Vec::new();
    for (since, record) in std::mem::take(&mut f.waiting) {
        let Some(((first, count), named)) = run(&record) else { continue };
        let end = first.saturating_add(count);
        if f.next < end && since.elapsed() < FRAMES_WAIT {
            f.waiting.push((since, record));
            continue;
        }
        let mut got = Frames::new(first);
        let came: Vec<u64> = f.digests.range(first..end).map(|(n, _)| *n).collect();
        for d in came.iter().filter_map(|n| f.digests.remove(n)) {
            got.push_digest(d);
        }
        let thing = got.thing(&named.name);
        let changed = thing.digest.get(FRAMES) != named.digest.get(FRAMES);
        let relay = statement_of(&record).and_then(|s| s.body.get("by").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        tracing::debug!(target: "kiln.custody", %link, first, count, changed, "a run of a reach link's frames answered");
        out.push((relay, thing, record, changed));
    }
    out
}

/// This process's receipt of a run of reach link `link`'s frames kiln's relay passed on (`record`),
/// as they came (`thing`); one that differs from the record (a frame dropped, swapped or changed on
/// the way, which the link's end couldn't read) told to kiln too, as a break naming the relay.
fn reach_receipt(link: &str, relay: &str, thing: Subject, record: &Value, changed: bool) {
    use kiln_ci::identity::relay::{FRAMES, Unrelayed};
    if changed {
        let was = statement_of(record).and_then(|s| s.subject.first().and_then(|t| t.digest.get(FRAMES).cloned())).unwrap_or_default();
        let now = thing.digest.get(FRAMES).cloned().unwrap_or_default();
        let u = Unrelayed::Changed { relay: relay.to_owned(), thing: thing.name.clone(), algorithm: FRAMES.to_owned(), was, now };
        relay_break(Some(record), relay, &u, &format!("reach link {link}: its frames as they came, which its end couldn't read"));
    }
    relay_receipt(relay, vec![thing], &digest_of(record), "a reach link's frames, as they came", Vec::new());
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

    /// What the tests of this process's custody records share: its outbox, and its identities.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// W27-R08's relays, on a runner process (`relay-box`'s "blue"): it records each act of its
    /// agents' it passes on to kiln, numbered on a stream per connection; it checks the words kiln
    /// passes on to it end to end before its session takes them, and signs its receipt (words
    /// changed on the way, or sent again, refused and told to kiln naming the relay); as the old
    /// build of a blue/green upgrade it records each frame it forwards, which the new build checks
    /// (a frame changed, a number skipped, a number again); and it answers kiln's records of the
    /// reach frames its relay passed on with the frames as they came.
    #[test]
    fn w27_r08_relays_a_runner_records_what_it_passes_on_and_checks_what_is_passed_to_it() -> anyhow::Result<()> {
        use kiln_ci::identity::chain::{Actor, Holder, Identity};
        use kiln_ci::identity::relay::{Frames, Pass, passed_on, relayed, statement_thing};
        use rcgen::SigningKey as _;
        let _one = ONE_AT_A_TIME.lock();
        server_relays(true);
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let (org, machine) = (pki.organization(), "relay-box");
        let (a, b) = ("w27-r08-relays-a", "w27-r08-relays-b");
        let certify = |kind: &str, of: Option<&str>, epoch: Option<u64>, who: &Identity| -> anyhow::Result<String> {
            let asked: IdentityAsk = serde_json::from_str(&identity::ask(kind, of, epoch)?)?;
            let pem = pki.issue_identity(&asked.csr, who, &[], time::Duration::hours(1))?;
            identity::given(&serde_json::to_string(&crate::proto::IdentityGiven { kind: asked.kind, of: asked.of, epoch: asked.epoch, uri: who.uri()?, cert_pem: Some(pem), ..crate::proto::IdentityGiven::default() })?, dir.path(), "blue")?;
            Ok(who.uri()?)
        };
        let runner = certify("runner", None, None, &Identity::new(&org, Actor::Runner { enrolled_by: "ada".into(), machine: machine.into(), instance: "blue".into() }))?;
        let held = || Some(Holder { machine: machine.into(), instance: "blue".into() });
        let agent_a = certify("agent", Some(a), Some(0), &Identity::new(&org, Actor::Agent { person: "ada".into(), agent: a.into(), epoch: 0, held_by: held() }))?;
        certify("agent", Some(b), Some(0), &Identity::new(&org, Actor::Agent { person: "ada".into(), agent: b.into(), epoch: 0, held_by: held() }))?;
        let server = Identity::new(&org, Actor::Server).uri()?;
        let key = crate::pki::new_key_pem()?;
        let server_pem = pki.issue_identity(&crate::pki::make_csr_with_key("kiln-identity", &key)?, &Identity::new(&org, Actor::Server), &[], time::Duration::hours(1))?;
        let signing = rcgen::KeyPair::from_pem(&key)?;
        let server_signs = |s: &Statement| -> anyhow::Result<Value> {
            let p = statement::payload(s)?;
            Ok(statement::bundle(PAYLOAD_CUSTODY, &p, &signing.sign(&statement::to_sign(PAYLOAD_CUSTODY, &p))?, &crate::pki::der_of(&server_pem)?))
        };
        let json = |s: &str| serde_json::from_str::<Value>(s).unwrap_or_default();
        let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(json).unwrap_or_default();
        let mut drained: Vec<Value> = Vec::new();
        let drain_all = |into: &mut Vec<Value>| into.extend(drain().iter().map(|r| json(r)));
        let at = crate::util::now_unix();

        // Its agents' calls: each act with its record of passing it on, numbered on its stream for
        // this connection; the next connection numbers on another, from 1.
        calls_connected();
        let call = serde_json::json!({ "op": "call", "session": a, "name": "goals.done", "input": { "evidence": "a test" } }).to_string();
        let first = json(&passed_on_ask(signed_ask(call.clone())).ok_or_else(|| anyhow::anyhow!("not passed on"))?);
        let (act, record) = (field(&first, "act"), field(&first, "passed_on"));
        let s = statement_of(&record).ok_or_else(|| anyhow::anyhow!("no record"))?;
        let body: kiln_ci::identity::custody::Body = serde_json::from_value(s.body.clone())?;
        assert_eq!((body.kind, body.by.as_str(), body.from.as_deref(), body.to.as_deref()), (Kind::PassedOn, runner.as_str(), Some(agent_a.as_str()), Some(server.as_str())));
        assert_eq!((s.follows.clone(), s.seq), (vec![digest_of(&act)], Some(1)));
        let checked = statement::verify(&record, &pki.ca_der, &[], at)?;
        relayed(&checked, &runner, &server, &crate::relays::act_thing(a, "call goals.done", &digest_of(&act)), true)?;
        let second = json(&passed_on_ask(signed_ask(call.clone())).ok_or_else(|| anyhow::anyhow!("not passed on"))?);
        let s2 = statement_of(&field(&second, "passed_on")).ok_or_else(|| anyhow::anyhow!("no record"))?;
        assert_eq!((s2.stream.clone(), s2.seq), (s.stream.clone(), Some(2)));
        calls_connected();
        let third = json(&passed_on_ask(signed_ask(call)).ok_or_else(|| anyhow::anyhow!("not passed on"))?);
        let s3 = statement_of(&field(&third, "passed_on")).ok_or_else(|| anyhow::anyhow!("no record"))?;
        assert!(s3.stream != s.stream && s3.seq == Some(1), "another connection, another stream: {:?}", s3.stream);
        // An ask with no act beside it goes as it was.
        let registry = serde_json::json!({ "op": "registry", "session": a }).to_string();
        assert_eq!(passed_on_ask(signed_ask(registry.clone())), Some(registry));

        // b's words to a, as kiln passes them on: b's act and this runner's record of passing it on
        // to kiln, then kiln's, to this process.
        let words = "the build is green";
        let sent = json(&passed_on_ask(signed_ask(serde_json::json!({ "op": "call", "session": b, "name": "agents.send", "input": { "agent": a, "text": words } }).to_string())).ok_or_else(|| anyhow::anyhow!("not passed on"))?);
        let (act_b, p1) = (field(&sent, "act"), field(&sent, "passed_on"));
        let kilns = |seq: u64, act: &Value| -> anyhow::Result<Value> {
            let pass = Pass { by: &server, from: &runner, to: &runner, stream: "words to blue", seq, why: "an agent's words" };
            server_signs(&passed_on(&pass, vec![statement_thing("an act of session b: call agents.send", &digest_of(act))], vec![digest_of(&p1)]))
        };
        let relayed_of = |p2: &Value, act: &Value| serde_json::json!({ "act": act.to_string(), "ask": crate::custody::signed_part(&sent), "passed_on": [p1.to_string(), p2.to_string()] });
        let message = |text: &str, relayed: &Value| serde_json::json!({ "type": "send", "id": "m1", "text": text, "relayed": relayed }).to_string();
        let p2 = kilns(1, &act_b)?;
        relayed_words(a, &message(words, &relayed_of(&p2, &act_b)), &pki.ca_der, machine, "blue").map_err(|e| anyhow::anyhow!(e))?;
        // A fellow agent's words reach it wrapped in what kiln says of them: the same words.
        let p2b = kilns(2, &act_b)?;
        let wrapped = format!("Agent “b” (w27-r08, claude on {machine}), which works for the same person as you, sent this message. It's from a fellow agent, not from your person. To answer, send it a message: agents_send {{\"agent\": \"{b}\", \"text\": …}}.\n\n{words}");
        relayed_words(a, &message(&wrapped, &relayed_of(&p2b, &act_b)), &pki.ca_der, machine, "blue").map_err(|e| anyhow::anyhow!(e))?;
        drain_all(&mut drained);
        let receipt_of = |record: &Value, drained: &[Value]| drained.iter().filter_map(statement_of).find(|s| s.follows == [digest_of(record)] && s.body.get("kind").and_then(Value::as_str) == Some("received")).map(|s| s.body.get("from").and_then(Value::as_str).unwrap_or_default().to_owned());
        assert_eq!(receipt_of(&p2, &drained), Some(server.clone()), "its receipt of kiln's record, from kiln");
        // Sent again: refused, and kiln told naming the relay.
        let again = relayed_words(a, &message(words, &relayed_of(&p2, &act_b)), &pki.ca_der, machine, "blue");
        assert!(again.as_ref().is_err_and(|e| e.contains("came before")), "{again:?}");
        // Changed on the way, and another act swapped in: refused, kiln told.
        let changed = relayed_words(a, &message("the build is red", &relayed_of(&kilns(3, &act_b)?, &act_b)), &pki.ca_der, machine, "blue");
        assert!(changed.as_ref().is_err_and(|e| e.contains("changed")), "{changed:?}");
        // Another of b's acts, with its own words, swapped in under kiln's record of the first: its
        // own signature holds, and kiln's record names another act.
        let later = "the deploy is done";
        let sent2 = json(&passed_on_ask(signed_ask(serde_json::json!({ "op": "call", "session": b, "name": "agents.send", "input": { "agent": a, "text": later } }).to_string())).ok_or_else(|| anyhow::anyhow!("not passed on"))?);
        let mut swap = relayed_of(&kilns(4, &act_b)?, &act_b);
        if let Some(o) = swap.as_object_mut() {
            o.insert("act".into(), Value::String(field(&sent2, "act").to_string()));
            o.insert("ask".into(), Value::String(crate::custody::signed_part(&sent2)));
        }
        let swapped = relayed_words(a, &message(later, &swap), &pki.ca_der, machine, "blue");
        assert!(swapped.as_ref().is_err_and(|e| e.contains("another")), "{swapped:?}");
        // For another process: refused.
        let elsewhere = relayed_words(a, &message(words, &relayed_of(&kilns(5, &act_b)?, &act_b)), &pki.ca_der, machine, "green");
        assert!(elsewhere.as_ref().is_err_and(|e| e.contains("not to runner relay-box (instance green)")), "{elsewhere:?}");
        drain_all(&mut drained);
        let breaks: Vec<Value> = drained.iter().filter_map(|v| v.get("relay_break").cloned()).collect();
        let codes: Vec<(&str, &str)> = breaks.iter().map(|b| (b.get("code").and_then(Value::as_str).unwrap_or_default(), b.get("relay").and_then(Value::as_str).unwrap_or_default())).collect();
        for code in ["again", "changed", "swapped", "not-here"] {
            assert!(codes.contains(&(code, server.as_str())), "{code} told naming kiln's server: {codes:?}");
        }

        // As blue/green's old build: a record of each frame it forwards that carries a signed
        // statement (here kiln's hand-on of work to session a), which the new build checks; a frame
        // with none goes on unrecorded.
        let (to, stream) = forwarding_to("green").ok_or_else(|| anyhow::anyhow!("no stream to green"))?;
        let work = server_signs(&Statement { subject: vec![crate::custody::work("plan run r1 step 1", &["r1", "1"])], ..Statement::new(to_value(&Body { to: Some(agent_a.clone()), ..Body::new(Kind::HandedOn, &server) })) })?;
        let frame = crate::proto::encode(&crate::proto::ServerMsg::Custody { json: serde_json::json!({ "session": a, "hand_on": work.to_string() }).to_string() })?;
        let plain = crate::proto::encode(&crate::proto::ServerMsg::HarnessClose { session: a.into() })?;
        assert!(forward_record(&plain, &to, &stream, 1).is_none(), "a frame with no signed statement goes unrecorded");
        let record_of = |seq: u64| forward_record(&frame, &to, &stream, seq).ok_or_else(|| anyhow::anyhow!("no record"));
        let (r1, r2, r4) = (record_of(1)?, record_of(2)?, record_of(4)?);
        forwarded(&plain, None, &pki.ca_der, machine, "green").map_err(|e| anyhow::anyhow!(e))?;
        forwarded(&frame, Some(&r1), &pki.ca_der, machine, "green").map_err(|e| anyhow::anyhow!(e))?;
        let mut altered = frame.clone();
        if let Some(x) = altered.last_mut() {
            *x ^= 1;
        }
        assert!(forwarded(&altered, Some(&r2), &pki.ca_der, machine, "green").is_err_and(|e| e.contains("changed")));
        forwarded(&frame, Some(&r4), &pki.ca_der, machine, "green").map_err(|e| anyhow::anyhow!(e))?;
        assert!(forwarded(&frame, Some(&r2), &pki.ca_der, machine, "green").is_err_and(|e| e.contains("came before")));
        assert!(forwarded(&frame, None, &pki.ca_der, machine, "green").is_err(), "a frame of statements with no record, after records");
        forwarded(&plain, None, &pki.ca_der, machine, "green").map_err(|e| anyhow::anyhow!(e))?;
        assert!(forwarded(&frame, Some(&r1), &pki.ca_der, machine, "blue").is_err(), "a record of its own is no other process's");
        drain_all(&mut drained);
        let told: Vec<String> = drained.iter().filter_map(|v| v.get("relay_break")).filter(|b| b.get("relay").and_then(Value::as_str) == Some(runner.as_str())).filter_map(|b| b.get("code").and_then(Value::as_str).map(str::to_owned)).collect();
        for code in ["changed", "dropped", "again"] {
            assert!(told.iter().any(|c| c == code), "{code} told naming the old build: {told:?}");
        }
        assert!(receipt_of(&json(&r1), &drained).is_some(), "the new build's receipt of the first frame's statement");

        // A reach link's frames kiln's relay passed on: answered with what came.
        let rec = |link: &str, first: u64, frames: &[&[u8]], seq: u64| -> anyhow::Result<Value> {
            let mut run = Frames::new(first);
            for f in frames {
                run.push(f);
            }
            let name = format!("frames {first} to {} of reach link {link} from box-b", run.next().saturating_sub(1));
            let stream = format!("reach link {link} to blue");
            let pass = Pass { by: &server, from: "kiln://x/person/ada/machine/box-b/instance/main", to: &runner, stream: &stream, seq, why: "a reach link's frames" };
            server_signs(&passed_on(&pass, vec![run.thing(&name)], Vec::new()))
        };
        let hand = |link: &str, record: &Value| handed(&serde_json::json!({ "reach": { "link": link, "from": "box-b", "record": record.to_string() } }).to_string());
        for f in [b"one".as_slice(), b"two", b"three"] {
            reach_frame("L-w27-relays", f);
        }
        let whole = rec("L-w27-relays", 1, &[b"one", b"two", b"three"], 1)?;
        hand("L-w27-relays", &whole);
        // Frame 5 never came: the run kiln recorded waits for it, even once the link ends here (it
        // may still be on its way), then is answered with what came.
        reach_frame("L-w27-relays", b"four");
        let short = rec("L-w27-relays", 4, &[b"four", b"five"], 2)?;
        hand("L-w27-relays", &short);
        reach_closed("L-w27-relays");
        reach_due();
        drain_all(&mut drained);
        assert!(receipt_of(&short, &drained).is_none(), "it waits for frame 5");
        age_reached("L-w27-relays", FRAMES_WAIT);
        reach_due();
        // kiln signs each run on its own, so its records may come out of order: each is answered
        // with its own frames.
        for f in [b"a".as_slice(), b"b", b"c", b"d"] {
            reach_frame("L-w27-later", f);
        }
        let (early, late) = (rec("L-w27-later", 1, &[b"a", b"b"], 1)?, rec("L-w27-later", 3, &[b"c", b"d"], 2)?);
        hand("L-w27-later", &late);
        hand("L-w27-later", &early);
        drain_all(&mut drained);
        let frames_of = |record: &Value| drained.iter().filter_map(statement_of).find(|s| s.follows == [digest_of(record)]).and_then(|s| s.subject.first().and_then(|t| t.digest.get(kiln_ci::identity::relay::FRAMES).cloned()));
        let named = |record: &Value| statement_of(record).and_then(|s| s.subject.first().and_then(|t| t.digest.get(kiln_ci::identity::relay::FRAMES).cloned()));
        for r in [&whole, &early, &late] {
            assert!(frames_of(r).is_some() && frames_of(r) == named(r), "the frames as they came, as kiln's relay passed them on");
        }
        assert!(frames_of(&short).is_some() && frames_of(&short) != named(&short), "the frames as they came: one short");
        let changed = |link: &str| drained.iter().filter_map(|v| v.get("relay_break")).any(|b| b.get("code").and_then(Value::as_str) == Some("changed") && b.get("refused").and_then(Value::as_str).is_some_and(|r| r.contains(&format!("reach link {link}:"))));
        assert!(changed("L-w27-relays") && !changed("L-w27-later"));

        for s in [a, b] {
            identity::forget_prefix(&format!("agent/{s}/"));
        }
        identity::forget("runner");
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 6): work kiln handed one of this process's sessions is
    /// signed for as the session takes it up, at its next turn, with its identity at its epoch.
    /// Before its identity is certified it waits, and is signed once it is; the same hand-on asked
    /// again is one receipt; work handed while a turn is under way waits for the next turn.
    #[test]
    fn w27_r08_work_handed_is_received_at_the_sessions_next_turn() -> anyhow::Result<()> {
        use crate::harness::model::Status;
        use kiln_ci::identity::chain::{Actor, Identity};
        let _one = ONE_AT_A_TIME.lock();
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let (session, org) = ("w27-r08-work", pki.organization());
        let server = Identity::new(&org, Actor::Server);
        let key = crate::pki::new_key_pem()?;
        let server_pem = pki.issue_identity(&crate::pki::make_csr_with_key("kiln-identity", &key)?, &server, &[], time::Duration::hours(1))?;
        let signing = rcgen::KeyPair::from_pem(&key)?;
        let hand_on = |name: &str| -> anyhow::Result<Value> {
            use rcgen::SigningKey as _;
            let body = Body { to: Some(format!("kiln://{org}/person/ada/agent/{session}/epoch/0")), why: format!("{name}: a test"), ..Body::new(Kind::HandedOn, &server.uri()?) };
            let p = statement::payload(&Statement { subject: vec![crate::custody::work(name, &[name])], ..Statement::new(to_value(&body)) })?;
            Ok(statement::bundle(PAYLOAD_CUSTODY, &p, &signing.sign(&statement::to_sign(PAYLOAD_CUSTODY, &p))?, &crate::pki::der_of(&server_pem)?))
        };
        let ask = |b: &Value| serde_json::json!({ "session": session, "hand_on": b.to_string() }).to_string();
        let receipts = |name: &str| -> Vec<Statement> { drain().iter().filter_map(|r| serde_json::from_str::<Value>(r).ok()).filter_map(|b| statement_of(&b)).filter(|s| s.subject.first().is_some_and(|t| t.name == name)).collect() };
        let first = hand_on("plan run r1 step 1")?;
        handed(&ask(&first));
        assert!(!status(session, &Status::Idle), "nothing before a turn");
        assert!(!status(session, &Status::Running), "its turn starts before its identity is certified: it waits");
        // Its identity at epoch 0, certified.
        let asked: IdentityAsk = serde_json::from_str(&identity::ask("agent", Some(session), Some(0))?)?;
        let me = Identity::new(&org, Actor::Agent { person: "ada".into(), agent: session.into(), epoch: 0, held_by: Some(kiln_ci::identity::chain::Holder { machine: "adacachy".into(), instance: "01a14794".into() }) });
        let pem = pki.issue_identity(&asked.csr, &me, &[], time::Duration::hours(1))?;
        identity::given(&serde_json::to_string(&crate::proto::IdentityGiven { kind: asked.kind, of: asked.of, epoch: asked.epoch, uri: me.uri()?, cert_pem: Some(pem), ..crate::proto::IdentityGiven::default() })?, dir.path(), "main")?;
        assert!(take_up_due(), "signed once its identity is");
        let got = receipts("plan run r1 step 1");
        let body = got.first().map(|s| s.body.clone()).unwrap_or_default();
        assert_eq!(got.len(), 1);
        assert_eq!((body.get("kind"), body.get("by"), body.get("from")), (Some(&serde_json::json!("received")), Some(&serde_json::json!(me.uri()?)), Some(&serde_json::json!(server.uri()?))));
        assert_eq!(got.first().map(|s| s.follows.clone()), Some(vec![digest_of(&first)]));
        // Asked again (a reconnect), and more work while the turn is under way: the next turn.
        handed(&ask(&first));
        let second = hand_on("plan run r1 step 2")?;
        handed(&ask(&second));
        assert!(!status(session, &Status::Running), "a turn under way takes up nothing new");
        assert!(!status(session, &Status::Idle));
        assert!(status(session, &Status::Running), "the next turn takes it up");
        assert_eq!(receipts("plan run r1 step 2").len(), 1);
        assert!(receipts("plan run r1 step 1").is_empty(), "the first, signed for once");
        closed(session);
        identity::forget_prefix(&format!("agent/{session}/"));
        Ok(())
    }

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

