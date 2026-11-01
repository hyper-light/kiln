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

    /// W27-R08 (the Transfers plan's step 6): work kiln handed one of this process's sessions is
    /// signed for as the session takes it up, at its next turn, with its identity at its epoch.
    /// Before its identity is certified it waits, and is signed once it is; the same hand-on asked
    /// again is one receipt; work handed while a turn is under way waits for the next turn.
    #[test]
    fn w27_r08_work_handed_is_received_at_the_sessions_next_turn() -> anyhow::Result<()> {
        use crate::harness::model::Status;
        use kiln_ci::identity::chain::{Actor, Identity};
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

