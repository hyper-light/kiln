//! Custody records kept (docs/ci.md W27-R08; the Transfers plan's step 2, kiln plan
//! `01a17856b1b477eda14a574ae312a123`; migration `w27_custody`). Every signed custody record and
//! delegation kiln takes ([`kiln_ci::identity::custody`]) is kept by its statement's digest, its
//! envelope as it was signed, checked as of when kiln took it. Its own hop is checked as it's
//! taken: a receipt that comes before the hand-on it answers waits for it, and is checked when it
//! comes. A break found then, or by walking a thing's chain, is recorded with the records around
//! it; a statement kiln can't take (its signer's identity revoked, as a copy left behind's is once
//! its session went on) is refused and recorded with its envelope. Breaks and refusals go to the
//! audit log too, and to the trail once it's built (the Review plan's step 9).

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use kiln_ci::identity::chain::{Actor, Identity, Revocation};
use kiln_ci::identity::custody::{self, Break, Hop, Kind};
use kiln_ci::identity::statement::{self, Checked, PAYLOAD_CUSTODY, PAYLOAD_DELEGATION};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use crate::store::Store;

pub mod blobs;
pub mod feeds;

/// The most records a walk loads: a thing's chain, and its custodians' streams.
pub const WALK_MAX: usize = 10_000;
/// The most breaks or refusals a list gives, newest first.
pub const LIST_MAX: usize = 500;
/// How long an offer (a hand-on) waits for its receipt before it lapses, its giver still holding
/// what it offered (seconds).
pub const LAPSE_AFTER: i64 = 600;

/// The digest algorithm a session's timeline is named by ([`timeline`]).
pub const TIMELINE: &str = "kiln-timeline-v1";

/// Session `session`'s timeline as it leaves epoch `epoch` with `events`: the thing a hand-over of
/// it is about ("session <id> at epoch <n>"), its digest over its id, that epoch and each event's
/// digest (`crate::wire::sums::event`), which both sides work out alike from the events its
/// package carries. A timeline grows while it's held, so each epoch's is a thing of its own.
pub fn timeline(session: &str, epoch: u64, events: &[crate::harness::model::Event]) -> statement::Subject {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(session.as_bytes());
    h.update([0]);
    h.update(epoch.to_be_bytes());
    h.update(u64::try_from(events.len()).unwrap_or(u64::MAX).to_be_bytes());
    for e in events {
        h.update(serde_json::to_value(e).map(|v| crate::wire::sums::event(&v)).unwrap_or_default().to_be_bytes());
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let annotations = BTreeMap::from([("epoch".to_owned(), Value::from(epoch)), ("events".to_owned(), Value::from(events.len()))]);
    statement::Subject { name: format!("session {session} at epoch {epoch}"), digest: BTreeMap::from([(TIMELINE.to_owned(), hex)]), size: None, annotations }
}

/// The agent identity at `epoch` of the session whose identity at some epoch is `uri`, without the
/// runner process holding its key: whom a hand-on goes to, or a receipt comes from.
pub fn at_epoch(uri: &str, epoch: u64) -> Option<String> {
    let (head, _) = uri.split_once("/epoch/")?;
    head.contains("/agent/").then(|| format!("{head}/epoch/{epoch}"))
}

/// kiln's server's identity in organization `org`: the only signer of takes.
pub fn server_uri(org: &str) -> String {
    Identity::new(org, Actor::Server).uri().unwrap_or_default()
}

/// A kind's name, as records say it (`handed-on`).
fn kind_name(k: Kind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// The digest of a bundle's statement, read without checking it ("" when it can't be read).
pub(crate) fn digest_of(bundle: &Value) -> String {
    let env = bundle.get("dsseEnvelope");
    let ty = env.and_then(|e| e.get("payloadType")).and_then(Value::as_str);
    let payload = env.and_then(|e| e.get("payload")).and_then(Value::as_str).and_then(|p| base64::engine::general_purpose::STANDARD.decode(p).ok());
    match (ty, payload) {
        (Some(t), Some(p)) => statement::digest(t, &p),
        _ => String::new(),
    }
}

/// What became of a statement kiln took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken {
    pub digest: String,
    /// kiln had it already: the same statement, by its digest, is kept once.
    pub again: bool,
    /// Its own hop doesn't hold: the break, recorded.
    pub broke: Option<String>,
}

/// Why kiln didn't take a statement. Each is recorded ([`refused`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// Not a signed statement kiln takes: its signer's chain, signature or form, as of now.
    #[error("{0}")]
    Unsigned(String),
    /// Signed, but not a custody record or a delegation kiln reads.
    #[error("{0}")]
    NotCustody(String),
}

/// A refusal, as kiln records it.
#[derive(Default)]
struct Refusal {
    digest: String,
    act: String,
    signer: String,
    why: String,
    envelope: String,
    came_from: String,
    at: i64,
    /// An agent session's: the epoch it's from (signed at, or, unsigned, the one its sender held
    /// the session at).
    epoch: Option<u64>,
    /// The transfer the session went on with, when that epoch is one it left.
    superseded_by: String,
}

/// Takes the signed custody record or delegation `bundle`, which came over `came_from` (a runner
/// process, an API caller), at `at`: checked as of then against kiln's root `root` and the
/// revocations kiln has; kept by its digest; its own hop checked, and the hops of records that
/// waited for it. Refused, and recorded, when kiln can't take it. One kiln has already (sent again
/// after a reconnect, or carried by another) is answered as such, as it was judged when taken: its
/// signer may have moved on since.
pub async fn take(store: &Store, root: &[u8], org: &str, bundle: &Value, came_from: &str, at: i64) -> anyhow::Result<Result<Taken, Refused>> {
    let digest = digest_of(bundle);
    if !digest.is_empty() && kept(store, &digest).await? {
        return Ok(Ok(Taken { digest, again: true, broke: None }));
    }
    let revocations = crate::identities::revocations(store).await?;
    let refusal = |act: &str, signer: String, why: String| Refusal { digest: digest_of(bundle), act: act.to_owned(), signer, why, envelope: bundle.to_string(), came_from: came_from.to_owned(), at, ..Refusal::default() };
    let c = match statement::verify(bundle, root, &revocations, at) {
        Ok(c) => c,
        Err(e) => {
            // Who signed it, as its certificate says, revocations aside: a copy left behind signs
            // as the epoch it left.
            let signer = statement::verify(bundle, root, &[], at).map(|c| c.custodian).unwrap_or_default();
            let (epoch, superseded_by) = left(store, &signer).await?;
            let why = e.to_string();
            refuse(store, Refusal { epoch, superseded_by, ..refusal("a signed statement", signer, why.clone()) }).await?;
            return Ok(Err(Refused::Unsigned(why)));
        }
    };
    let (kind, counterpart) = match c.payload_type.as_str() {
        PAYLOAD_CUSTODY => match custody::body(&c) {
            Ok(b) => (Some(b.kind), if b.kind == Kind::Received { b.from } else { b.to }),
            Err(e) => {
                refuse(store, refusal("a custody record", c.custodian.clone(), e.to_string())).await?;
                return Ok(Err(Refused::NotCustody(e.to_string())));
            }
        },
        PAYLOAD_DELEGATION => match custody::grant(&c) {
            Ok(g) => (None, Some(g.delegate)),
            Err(why) => {
                refuse(store, refusal("a delegation", c.custodian.clone(), why.clone())).await?;
                return Ok(Err(Refused::NotCustody(why)));
            }
        },
        other => {
            let why = format!("it's a {other} statement, not a custody record or a delegation");
            refuse(store, refusal("a signed statement", c.custodian.clone(), why.clone())).await?;
            return Ok(Err(Refused::NotCustody(why)));
        }
    };
    let again = !keep(store, &c, kind, counterpart, bundle, came_from).await?;
    let mut broke = None;
    if kind.is_some() && !again {
        broke = check(store, root, org, &c.digest, &revocations, at).await?;
        for waiting in followers(store, &c.digest).await? {
            check(store, root, org, &waiting, &revocations, at).await?;
        }
    }
    // A delegation: the records taken for someone under it that waited for it (they come from
    // other machines, in any order) are checked now.
    if kind.is_none() && !again {
        for waiting in under_it(store, &c.digest).await? {
            check(store, root, org, &waiting, &revocations, at).await?;
        }
    }
    Ok(Ok(Taken { digest: c.digest, again, broke }))
}

/// The records that act under delegation `digest`.
async fn under_it(store: &Store, digest: &str) -> anyhow::Result<Vec<String>> {
    let d = digest.to_owned();
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest FROM custody WHERE under = ?1")?;
            let rows = q.query_map([d], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// A custody record a runner process sent ([`crate::proto::AgentMsg::Custody`]): taken as having
/// come over that process's connection, and logged either way.
pub async fn on_runner(hub: &crate::hub::Hub, peer: crate::instances::PeerId, machine: &str, json: &str) {
    let instance = hub.instances.lock().get(peer).map(|i| i.label()).unwrap_or_else(|| "legacy".to_owned());
    let Ok(bundle) = serde_json::from_str::<Value>(json) else {
        return tracing::debug!(%machine, "an unreadable custody record");
    };
    let came_from = format!("runner {machine} (instance {instance})");
    // Something another runner handed it over a reach link that it refused where it would have
    // used it (W27-R08: a sync's workspace, a run, a run's output not what was handed on, or with
    // no hand-on or receipt of it).
    if let Some(r) = bundle.get("refused_hop") {
        let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let why = format!("{} from {}: {}", text("what"), text("from"), text("why"));
        let refusal = Refusal { act: "a hand-over between machines".into(), why: why.clone(), came_from, at: crate::util::now_unix(), ..Refusal::default() };
        if let Err(e) = refuse(&hub.store, refusal).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a refused hand-over between machines couldn't be recorded");
        }
        return tracing::info!(target: "kiln.custody", %machine, %instance, %why, "a runner process refused what another handed it");
    }
    // An act of one of its sessions it didn't do, kiln being out of reach (W27-R08: a machine cut
    // off, as the twin's was).
    if let Some(r) = bundle.get("refused_act") {
        let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let op = r.get("op").and_then(Value::as_str).unwrap_or("call");
        let ask = json!({ "op": op, "name": text("name"), "act": r.get("act") });
        if let Err(e) = refused_act(hub, machine, &instance, &text("session"), &ask, &text("why"), crate::util::now_unix()).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "an act a runner didn't do couldn't be recorded");
        }
        return tracing::info!(target: "kiln.custody", %machine, %instance, session = %text("session"), why = %text("why"), "a runner process didn't do an act: kiln was out of its reach");
    }
    // A break it found in what a relay passed on to it, and what it refused because of it (W27-R08's
    // relays): kiln's words, a fellow agent's, blue/green's frames, a reach link's.
    if let Some(r) = bundle.get("relay_break") {
        if let Err(e) = crate::relays::reported(hub, machine, &instance, r).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a break a runner found in what a relay passed on couldn't be recorded");
        }
        let said = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (relay, detail) = (said("relay"), said("detail"));
        return tracing::info!(target: "kiln.custody", %machine, %instance, %relay, %detail, "a runner process found a break in what a relay passed on");
    }
    // A hand-over's word it refused, not being the process it was for (W27-R08; the 14:18 drop).
    if let Some(r) = bundle.get("refused_word") {
        let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let word: Option<Value> = r.get("word").and_then(Value::as_str).and_then(|w| serde_json::from_str(w).ok());
        let signer = word.as_ref().and_then(|w| statement::verify(w, &hub.pki.ca_der, &[], crate::util::now_unix()).ok()).map(|c| c.custodian).unwrap_or_default();
        let why = format!("session {}: {}", text("session"), text("why"));
        let refusal = Refusal { digest: word.as_ref().map(digest_of).unwrap_or_default(), act: "a hand-over's word".into(), signer, why: why.clone(), envelope: word.map(|w| w.to_string()).unwrap_or_default(), came_from, at: crate::util::now_unix(), ..Refusal::default() };
        if let Err(e) = refuse(&hub.store, refusal).await {
            tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "a refused hand-over's word couldn't be recorded");
        }
        return tracing::info!(target: "kiln.custody", %machine, %instance, %why, "a runner process refused a hand-over's word not for it");
    }
    match take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), &bundle, &came_from, crate::util::now_unix()).await {
        Ok(Ok(t)) => tracing::info!(target: "kiln.custody", %machine, %instance, digest = %t.digest, again = t.again, broke = t.broke.as_deref().unwrap_or(""), "took a custody record"),
        Ok(Err(why)) => tracing::info!(target: "kiln.custody", %machine, %instance, %why, "refused a custody record"),
        Err(e) => tracing::warn!(target: "kiln.custody", %machine, error = %format!("{e:#}"), "couldn't keep a custody record"),
    }
}

/// `s` signed by kiln's server with its own signing key (`<data>/pki/server-signing.key`, which
/// `identities::own_identities` keeps certified), its body naming it: its bundle. Only kiln's
/// server signs takes.
pub fn server_signed(pki: &crate::pki::Pki, s: &statement::Statement) -> anyhow::Result<Value> {
    server_signed_as(pki, PAYLOAD_CUSTODY, s)
}

/// [`server_signed`], as `payload_type`.
fn server_signed_as(pki: &crate::pki::Pki, payload_type: &str, s: &statement::Statement) -> anyhow::Result<Value> {
    own_signed_as(pki, "server", &server_uri(&pki.organization()), payload_type, s)
}

/// kiln's CD's identity in organization `org`.
pub fn cd_uri(org: &str) -> String {
    Identity::new(org, Actor::Cd).uri().unwrap_or_default()
}

/// `s` signed by kiln's CD with its own signing key (`<data>/pki/cd-signing.key`, kept certified
/// beside the server's), its body naming it: its bundle. CD signs a promotion's records (docs/ci.md
/// W27-R07, R08: `crate::deploy::promotion`).
pub fn cd_signed(pki: &crate::pki::Pki, s: &statement::Statement) -> anyhow::Result<Value> {
    own_signed_as(pki, "cd", &cd_uri(&pki.organization()), PAYLOAD_CUSTODY, s)
}

/// `s` signed as `payload_type` with the key of kiln's own identity `kind` (`server`, `cd`), whose
/// URI is `by`, its body naming it.
fn own_signed_as(pki: &crate::pki::Pki, kind: &str, by: &str, payload_type: &str, s: &statement::Statement) -> anyhow::Result<Value> {
    use anyhow::Context as _;
    use rcgen::SigningKey as _;
    let read = |f: &str| std::fs::read_to_string(pki.dir().join(f)).with_context(|| format!("kiln's {kind}'s {f}"));
    let key = rcgen::KeyPair::from_pem(&read(&format!("{kind}-signing.key"))?)?;
    let cert = read(&format!("{kind}-signing.pem"))?;
    let mut s = s.clone();
    if let Some(o) = s.body.as_object_mut() {
        o.insert("by".into(), Value::String(by.to_owned()));
    }
    let payload = statement::payload(&s)?;
    let signature = key.sign(&statement::to_sign(payload_type, &payload))?;
    Ok(statement::bundle(payload_type, &payload, &signature, &crate::pki::der_of(&cert)?))
}

/// A hand-over's word (`command`, a session command's JSON) as kiln's server sends it to runner
/// process `instance` of `machine`, which holds session `session` at `epoch` as kiln knows it
/// (docs/ci.md W27-R08; the Transfers plan's step 4): the command with the server's signature over
/// it and whom it's for (`kiln_ci::identity::custody::Word`) beside it, as `word`. A runner from
/// before reads the command alone. What's signed is the command as [`unworded`] writes it.
pub fn word(pki: &crate::pki::Pki, session: &str, command: &str, machine: &str, instance: &str, epoch: u64) -> anyhow::Result<String> {
    word_signed(&|payload_type, s| server_signed_as(pki, payload_type, s), session, command, machine, instance, epoch)
}

/// [`word`], signed by `sign` (kiln's server's key: a payload type and a statement to its bundle),
/// as the transfers simulation (`crate::sim::transfers`) signs one with its own server's.
pub fn word_signed(sign: &dyn Fn(&str, &statement::Statement) -> anyhow::Result<Value>, session: &str, command: &str, machine: &str, instance: &str, epoch: u64) -> anyhow::Result<String> {
    use sha2::Digest as _;
    let mut v: Value = serde_json::from_str(command)?;
    let digest: String = sha2::Sha256::digest(unworded(&v).as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let body = serde_json::to_value(custody::Word { v: statement::VERSION, by: String::new(), session: session.to_owned(), machine: machine.to_owned(), instance: instance.to_owned(), epoch })?;
    let subject = statement::Subject { name: format!("a hand-over's word about session {session}"), digest: BTreeMap::from([("sha256".to_owned(), digest)]), ..statement::Subject::default() };
    let bundle = sign(custody::PAYLOAD_WORD, &statement::Statement { subject: vec![subject], ..statement::Statement::new(body) })?;
    if let Some(o) = v.as_object_mut() {
        o.insert("word".into(), Value::String(bundle.to_string()));
    }
    Ok(v.to_string())
}

/// Keeps the word kiln's server signed beside command `json` ([`word`]), so a walk through a relay
/// that passes it on (blue/green's old build: docs/ci.md W27-R08's relays) reaches it.
pub(crate) async fn keep_word(store: &Store, root: &[u8], json: &str) {
    let word = serde_json::from_str::<Value>(json).ok().and_then(|v| v.get("word").and_then(Value::as_str).and_then(|w| serde_json::from_str::<Value>(w).ok()));
    let Some(bundle) = word else { return };
    if let Err(e) = keep_originator(store, root, &bundle, "kiln's server", crate::util::now_unix()).await {
        tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "a hand-over's word kiln signed wasn't kept");
    }
}

/// The command a hand-over's word signs (`v`, as sent): its JSON without the word, as `serde_json`
/// writes it again, which the server and the runner write alike.
pub fn unworded(v: &Value) -> String {
    without(v, "word")
}

/// `v` without its `key` (a signature beside it), as `serde_json` writes it again: what that
/// signature signs, which both sides write alike.
pub fn without(v: &Value, key: &str) -> String {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove(key);
    }
    v.to_string()
}

/// What an agent session's act signs of ask `v`, as sent: the ask without the act beside it, nor
/// the runner's record of passing it on (`passed_on`, added after the act was signed: docs/ci.md
/// W27-R08's relays), as `serde_json` writes it again.
pub fn signed_part(v: &Value) -> String {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("act");
        o.remove("passed_on");
    }
    v.to_string()
}

/// Whether ask `ask`, which runner process `instance` of `machine` sends kiln for agent session
/// `session` (a kiln operation, a page it shows, a run it asks before doing:
/// `crate::mcp::runner_ask`), is the session's act now (docs/ci.md W27-R08; the Transfers plan's
/// step 5, the twin). Once kiln has certified a holder at the session's epoch, it's the session's
/// only if it's signed beside it (`act`) by that holder at that epoch
/// (`kiln_ci::identity::custody::acted`): a copy left behind signs at an epoch whose key kiln
/// revoked as the session went on, and a runner process that signs acts (`signs`: `CAP_CUSTODY`)
/// and that kiln certified for the session can't send one unsigned. One that isn't certified for it
/// yet (it just took it over), or a runner from before, goes by its machine, as before.
/// Why not, each refusal recorded with the act's envelope, the epoch it's from and the transfer
/// that superseded it.
pub async fn act_counts(hub: &crate::hub::Hub, machine: &str, instance: &str, signs: bool, session: &str, ask: &Value, at: i64) -> anyhow::Result<Result<(), String>> {
    // The session as its transfers say (the newest epoch kiln certified it at, and its holder
    // then), not as an Open says: what a process announces doesn't fence out the holder.
    let Some(held) = held_now(&hub.store, session).await? else { return Ok(Ok(())) };
    let came_from = format!("runner {machine} (instance {instance})");
    let record = |signer: String, epoch: Option<u64>, why: String, act: Option<&Value>| Refusal {
        digest: act.map(digest_of).unwrap_or_default(),
        act: format!("an act of session {session}: {}", op_of(ask)),
        signer,
        why,
        envelope: act.map(Value::to_string).unwrap_or_default(),
        came_from: came_from.clone(),
        at,
        superseded_by: if epoch.is_some_and(|e| e < held.epoch) { held.transfer.clone() } else { String::new() },
        epoch,
    };
    let Some(act) = ask.get("act").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok()) else {
        // Unsigned: from a process that signs acts and that kiln certified for the session, it's
        // refused (at the session's epoch it holds the key; at one the session left, it's a copy
        // left behind); from one kiln hasn't certified for it (it just took it over, its
        // certificate on its way) or a runner from before, it goes by its machine, as before.
        let held_at = if signs { crate::identities::epoch_held(&hub.store, session, machine, instance).await? } else { None };
        let Some(e) = held_at else { return Ok(Ok(())) };
        let why = if e < held.epoch {
            format!("it isn't signed, from a runner process that held session {session} at epoch {e}, which {}: it's from a copy left behind", held.went_on())
        } else {
            format!("it isn't signed, from a runner process that signs its sessions' acts and holds session {session}'s key at epoch {e}")
        };
        refuse(&hub.store, record(String::new(), Some(e), why.clone(), None)).await?;
        return Ok(Err(why));
    };
    let what = signed_part(ask);
    let revocations = crate::identities::revocations(&hub.store).await?;
    match act_check(&act, &hub.pki.ca_der, &revocations, &held, what.as_bytes(), at) {
        Ok(()) => Ok(Ok(())),
        Err((signer, why)) => {
            refuse(&hub.store, record(signer.clone(), epoch_of(&signer), why.clone(), Some(&act))).await?;
            Ok(Err(why))
        }
    }
}

/// The server's check of a signed act (`act`, a bundle) of agent session `held.session` (docs/ci.md
/// W27-R08): signed under kiln's root (`root`) by an identity not revoked as of `at`
/// (`revocations`), and the act of its holder now ([`custody::acted`], as of `what`: the ask it
/// was signed beside, without it). [`act_counts`] runs it, and so does the transfers simulation
/// (`crate::sim::transfers`). Who signed it (revocations aside) and why not, when it doesn't count.
pub fn act_check(act: &Value, root: &[u8], revocations: &[Revocation], held: &custody::Held, what: &[u8], at: i64) -> Result<(), (String, String)> {
    let checked = match statement::verify(act, root, revocations, at) {
        Ok(c) => c,
        Err(e) => {
            // Its signer's identity revoked: a copy left behind signs at the epoch it left, which
            // kiln revoked as the session went on. Who signed it, revocations aside, and why the
            // fence says it doesn't count.
            let signed = statement::verify(act, root, &[], at).ok();
            let why = match signed.as_ref().map(|c| custody::acted(c, held, what)) {
                Some(Err(fenced @ custody::Unacted::Fenced(_))) => fenced.to_string(),
                _ => format!("session {}'s act isn't signed as its holder now: {e}", held.session),
            };
            return Err((signed.map(|c| c.custodian).unwrap_or_default(), why));
        }
    };
    custody::acted(&checked, held, what).map(|_| ()).map_err(|e| (checked.custodian.clone(), e.to_string()))
}

/// Act `ask` of agent session `session`, from runner process `instance` of `machine`, refused
/// before it was checked as an act (`why`: the session went on elsewhere, so this is a copy left
/// behind; its runner, out of kiln's reach, didn't do it): recorded with its envelope, who signed it
/// and at which epoch (revocations aside), and the transfer the session went on with if that's an
/// epoch it left (docs/ci.md W27-R08). Why, saying where the session went on.
pub async fn refused_act(hub: &crate::hub::Hub, machine: &str, instance: &str, session: &str, ask: &Value, why: &str, at: i64) -> anyhow::Result<String> {
    refused_act_of(hub, machine, instance, session, ask, why, None, at).await
}

/// [`refused_act`], of work agent session `session` held that changed hands since (a plan's step
/// tried again with another agent, or taken over by a person): recorded as superseded by
/// `transfer`, that work's hand-over (docs/ci.md W27-R08; the Transfers plan's step 6).
#[allow(clippy::too_many_arguments)]
pub async fn refused_work_act(hub: &crate::hub::Hub, machine: &str, instance: &str, session: &str, ask: &Value, why: &str, transfer: &str, at: i64) -> anyhow::Result<String> {
    refused_act_of(hub, machine, instance, session, ask, why, Some(transfer.to_owned()), at).await
}

#[allow(clippy::too_many_arguments)]
async fn refused_act_of(hub: &crate::hub::Hub, machine: &str, instance: &str, session: &str, ask: &Value, why: &str, transfer: Option<String>, at: i64) -> anyhow::Result<String> {
    let act = ask.get("act").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok());
    let signer = act.as_ref().and_then(|a| statement::verify(a, &hub.pki.ca_der, &[], at).ok()).map(|c| c.custodian).unwrap_or_default();
    // The epoch it's from: the one it's signed at, or, unsigned, the newest kiln certified this
    // process for the session at.
    let epoch = match epoch_of(&signer) {
        Some(e) => Some(e),
        None => crate::identities::epoch_held(&hub.store, session, machine, instance).await?,
    };
    let (why, superseded_by) = match (transfer, held_now(&hub.store, session).await?, epoch) {
        (Some(t), _, _) => (why.to_owned(), t),
        (None, Some(h), Some(e)) if e < h.epoch => (format!("{why}: it's from epoch {e} of session {session}, which {}", h.went_on()), h.transfer),
        _ => (why.to_owned(), String::new()),
    };
    let refusal = Refusal {
        digest: act.as_ref().map(digest_of).unwrap_or_default(),
        act: format!("an act of session {session}: {}", op_of(ask)),
        signer,
        why: why.clone(),
        envelope: act.map(|a| a.to_string()).unwrap_or_default(),
        came_from: format!("runner {machine} (instance {instance})"),
        at,
        epoch,
        superseded_by,
    };
    refuse(&hub.store, refusal).await?;
    Ok(why)
}

/// What ask `ask` is: "call goals.done", "act run", "page".
fn op_of(ask: &Value) -> String {
    match (ask.get("op").and_then(Value::as_str), ask.get("name").and_then(Value::as_str)) {
        (Some(op), Some(name)) => format!("{op} {name}"),
        (Some(op), None) => op.to_owned(),
        _ => "an ask".to_owned(),
    }
}

/// The epoch agent identity `signer` signs at; None for another identity.
fn epoch_of(signer: &str) -> Option<u64> {
    Identity::parse(signer).ok().and_then(|i| match i.actor {
        Actor::Agent { epoch, .. } => Some(epoch),
        _ => None,
    })
}

/// Who holds agent session `session` now, as its transfers say: the newest epoch kiln certified it
/// at, the runner process it certified then and when, and kiln's record of the transfer that
/// process took it with (its receipt). None before kiln certified any (W27-R08).
async fn held_now(store: &Store, session: &str) -> anyhow::Result<Option<custody::Held>> {
    let Some((epoch, holder)) = crate::identities::newest_holder(store, session).await? else { return Ok(None) };
    let by = Identity::parse(&holder).ok().and_then(|i| match i.actor {
        Actor::Agent { held_by, .. } => held_by,
        _ => None,
    });
    Ok(Some(custody::Held { session: session.to_owned(), epoch, by, since: issued_at(store, &holder).await?.unwrap_or_default(), transfer: receipt_of(store, &holder).await?.unwrap_or_default() }))
}

/// The epoch agent identity `signer` signs at, and the transfer its session went on with if that's
/// an epoch the session left (a copy left behind's): what a refusal of what it signed records.
async fn left(store: &Store, signer: &str) -> anyhow::Result<(Option<u64>, String)> {
    let Some((session, epoch)) = Identity::parse(signer).ok().and_then(|i| match i.actor {
        Actor::Agent { agent, epoch, .. } => Some((agent, epoch)),
        _ => None,
    }) else {
        return Ok((None, String::new()));
    };
    let held = held_now(store, &session).await?;
    Ok((Some(epoch), held.filter(|h| epoch < h.epoch).map(|h| h.transfer).unwrap_or_default()))
}

/// When kiln issued its newest certificate for identity `subject`.
async fn issued_at(store: &Store, subject: &str) -> anyhow::Result<Option<i64>> {
    let s = subject.to_owned();
    store.call(move |c| Ok(c.query_row("SELECT MAX(issued_at) FROM identities WHERE subject = ?1", [&s], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten())).await
}

/// kiln's take of agent session `session` as it leaves epoch `epoch` (its timeline `events`), for
/// the process that held it there, which can't hand it on (`why`: its machine is away, and since
/// when): to the session at the next epoch, whichever process takes it, following that process's
/// receipt of it if kiln has one. Kept here at once, and carried in the session's package: its
/// receipt follows it as a hand-on's does (docs/ci.md W27-R08). None if kiln never certified the
/// session at that epoch: it goes on unsigned, as before.
pub async fn take_for(store: &Store, pki: &crate::pki::Pki, session: &str, epoch: u64, events: &[crate::harness::model::Event], why: &str) -> anyhow::Result<Option<Value>> {
    let Some(holder) = crate::identities::holder_at(store, session, epoch).await? else { return Ok(None) };
    let org = pki.organization();
    let to = at_epoch(&holder, epoch.saturating_add(1)).ok_or_else(|| anyhow::anyhow!("{holder} isn't an agent session's identity"))?;
    let body = custody::Body { taken_for: Some(holder.clone()), to: Some(to), why: why.to_owned(), epoch: Some(epoch.saturating_add(1)), ..custody::Body::new(Kind::Taken, &server_uri(&org)) };
    let follows = receipt_of(store, &holder).await?.into_iter().collect();
    let s = statement::Statement { subject: vec![timeline(session, epoch, events)], follows, ..statement::Statement::new(custody::to_value(&body)) };
    let bundle = server_signed(pki, &s)?;
    match take(store, &pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(_) => Ok(Some(bundle)),
        Err(why) => anyhow::bail!("kiln refused its own take: {why}"),
    }
}

/// What a hand-over of work is about, by its digest (docs/ci.md W27-R08; the Transfers plan's step
/// 6): a plan's step, a subagent's task, what a person handed off.
pub const WORK: &str = "kiln-work-v1";

/// Work named `name` ("plan run <run> step <n>", "the task of agent <id>"), its digest over what it
/// is (`parts`: a step's run, number and goal; a task's words), which its giver and its taker work
/// out alike: the same work across every hand it goes through.
pub fn work(name: &str, parts: &[&str]) -> statement::Subject {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(u64::try_from(p.len()).unwrap_or(u64::MAX).to_be_bytes());
        h.update(p.as_bytes());
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    statement::Subject { name: name.to_owned(), digest: BTreeMap::from([(WORK.to_owned(), hex)]), ..statement::Subject::default() }
}

/// Person `name`'s identity in organization `org`.
pub fn person_uri(org: &str, name: &str) -> String {
    Identity::new(org, Actor::Person { name: name.to_owned() }).uri().unwrap_or_default()
}

/// How kiln's server hands work on: as itself (a plan run's driver), acting for those it names,
/// nearest first; or as a take, for a giver who can't sign (a retried step's earlier agent, a
/// step's agent as a person takes it over, a person with no key on the device that asked).
pub enum Giver {
    Kiln { acts_for: Vec<String> },
    For(String),
}

/// kiln's server hands `thing` on to `to` (an identity's URI: an agent session's at its epoch,
/// without the process holding its key, or a person's), for `why`, as `giver` says; a take follows
/// how its giver came to hold it, when kiln has that. Kept at once: its digest and bundle.
pub async fn hand_work(hub: &crate::hub::Hub, thing: statement::Subject, to: &str, why: &str, giver: Giver) -> anyhow::Result<(String, Value)> {
    let org = hub.pki.organization();
    let (kind, acts_for, taken_for) = match giver {
        Giver::Kiln { acts_for } => (Kind::HandedOn, acts_for, None),
        Giver::For(g) => (Kind::Taken, Vec::new(), Some(g)),
    };
    let follows = match &taken_for {
        Some(g) => held_with(&hub.store, &org, g, &thing).await?.into_iter().collect(),
        None => Vec::new(),
    };
    let body = custody::Body { to: Some(to.to_owned()), why: why.to_owned(), acts_for, taken_for, ..custody::Body::new(kind, &server_uri(&org)) };
    let s = statement::Statement { subject: vec![thing], follows, ..statement::Statement::new(custody::to_value(&body)) };
    let bundle = server_signed(&hub.pki, &s)?;
    match take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(t) => Ok((t.digest, bundle)),
        Err(why) => anyhow::bail!("kiln refused its own hand-on: {why}"),
    }
}

/// kiln's server takes `thing` for `person` (their identity's URI), who has no key on the device
/// that asked: its receipt for them of hand-on `hand_on` (from `giver`), on the strength of their
/// signed-in session, saying their own signature is missing and why. A walk reads the link as
/// kiln's for them, never as theirs. Kept at once: its digest.
pub async fn receive_for(hub: &crate::hub::Hub, person: &str, thing: statement::Subject, hand_on: &str, giver: &str, why: &str) -> anyhow::Result<String> {
    receive_for_because(hub, person, thing, hand_on, giver, why, "no key on the device that asked").await
}

/// [`receive_for`], their own signature missing for the reason `missing` (a browser that can't
/// sign: "this page isn't secure (https), so this browser has no key").
pub async fn receive_for_because(hub: &crate::hub::Hub, person: &str, thing: statement::Subject, hand_on: &str, giver: &str, why: &str, missing: &str) -> anyhow::Result<String> {
    let org = hub.pki.organization();
    let why = format!("{why}: kiln's receipt for them, on their signed-in session; their own signature is missing: {missing}");
    let body = custody::Body { from: Some(giver.to_owned()), taken_for: Some(person.to_owned()), why, ..custody::Body::new(Kind::Received, &server_uri(&org)) };
    let s = statement::Statement { subject: vec![thing], follows: vec![hand_on.to_owned()], ..statement::Statement::new(custody::to_value(&body)) };
    let bundle = server_signed(&hub.pki, &s)?;
    match take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(t) => Ok(t.digest),
        Err(why) => anyhow::bail!("kiln refused its own receipt for {person}: {why}"),
    }
}

/// Person `person`'s own receipt (`bundle`, signed on their device with the key `kiln login` made
/// them) of hand-on `hand_on`, which came over `came_from`: kept if it's theirs, and a receipt of
/// that hand-on; why not, if it isn't.
pub async fn person_receipt(hub: &crate::hub::Hub, person: &str, hand_on: &str, bundle: &Value, came_from: &str) -> anyhow::Result<Result<String, String>> {
    let at = crate::util::now_unix();
    let revocations = crate::identities::revocations(&hub.store).await?;
    let c = match statement::verify(bundle, &hub.pki.ca_der, &revocations, at) {
        Ok(c) => c,
        Err(e) => return Ok(Err(format!("the receipt doesn't verify: {e}"))),
    };
    if !custody::names(person, &c.custodian) {
        return Ok(Err(format!("the receipt is signed by {}, not by {person}", c.custodian)));
    }
    let is_receipt = custody::body(&c).is_ok_and(|b| b.kind == Kind::Received);
    if !is_receipt || !c.statement.follows.iter().any(|f| f == hand_on) {
        return Ok(Err(format!("it isn't a receipt of {hand_on}")));
    }
    Ok(take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, came_from, at).await?.map(|t| t.digest).map_err(|r| r.to_string()))
}

/// Person `person`'s own hand-on (`bundle`, signed on their device with the key `kiln login` made
/// them) of `thing` to `to`, which came over `came_from`: kept if it's theirs, of that work, to
/// that taker; why not, if it isn't.
pub async fn person_hand_on(hub: &crate::hub::Hub, person: &str, to: &str, thing: &statement::Subject, bundle: &Value, came_from: &str) -> anyhow::Result<Result<String, String>> {
    let at = crate::util::now_unix();
    let revocations = crate::identities::revocations(&hub.store).await?;
    let c = match statement::verify(bundle, &hub.pki.ca_der, &revocations, at) {
        Ok(c) => c,
        Err(e) => return Ok(Err(format!("the hand-on doesn't verify: {e}"))),
    };
    if !custody::names(person, &c.custodian) {
        return Ok(Err(format!("the hand-on is signed by {}, not by {person}", c.custodian)));
    }
    let of_it = custody::body(&c).is_ok_and(|b| b.kind == Kind::HandedOn && b.to.as_deref().is_some_and(|t| custody::names(t, to) || custody::names(to, t)));
    if !of_it || !c.statement.subject.iter().any(|t| t.name == thing.name && t.digest == thing.digest) {
        return Ok(Err(format!("it isn't a hand-on of {} to {to}", thing.name)));
    }
    Ok(take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, came_from, at).await?.map(|t| t.digest).map_err(|r| r.to_string()))
}

/// What a person signs on their device for custody statement `s` (a browser, with its own key:
/// `ui/src/lib/signing.ts`), said as a verdict's is (`ci::verdicts::to_sign`): its payload type, the
/// statement, its bytes and the bytes signed (DSSE's PAE), those two base64; never required.
pub fn to_sign(s: &statement::Statement) -> anyhow::Result<Value> {
    let payload = statement::payload(s).map_err(|e| anyhow::anyhow!("{e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(json!({ "payload_type": PAYLOAD_CUSTODY, "statement": s, "payload": b64.encode(&payload), "pae": b64.encode(statement::to_sign(PAYLOAD_CUSTODY, &payload)), "required": false }))
}

/// Whom a hand-on (`bundle`, read without checking it) says it hands its things to.
pub fn hand_on_to(bundle: &Value) -> Option<String> {
    body_in(&bundle.to_string())?.to
}

/// kiln's server passes `things`, handed on to it by `from` as `hand_on` (its digest), on unchanged
/// to `to`, for `why`: a relay's record, its server's (docs/ci.md W27-R08). Kept at once: its
/// digest and bundle, which `to`'s receipt follows.
pub async fn pass_on(hub: &crate::hub::Hub, things: Vec<statement::Subject>, hand_on: &str, from: &str, to: &str, why: &str) -> anyhow::Result<(String, Value)> {
    let org = hub.pki.organization();
    let body = custody::Body { from: Some(from.to_owned()), to: Some(to.to_owned()), why: why.to_owned(), ..custody::Body::new(Kind::PassedOn, &server_uri(&org)) };
    let s = statement::Statement { subject: things, follows: vec![hand_on.to_owned()], ..statement::Statement::new(custody::to_value(&body)) };
    let bundle = server_signed(&hub.pki, &s)?;
    match take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
        Ok(t) => Ok((t.digest, bundle)),
        Err(why) => anyhow::bail!("kiln refused its own pass of {hand_on}: {why}"),
    }
}

/// Whom a hand-on (`bundle`, read without checking: kiln checked it as it took it) says its things
/// come from: the giver a take hands on for, else its signer.
pub fn giver_of(bundle: &Value) -> Option<String> {
    let b: custody::Body = serde_json::from_value(statement_in(&bundle.to_string())?.body).ok()?;
    Some(match b.kind {
        Kind::Taken => b.taken_for?,
        _ => b.by,
    })
}

/// The receipt by which `holder` came to hold `thing`, its newest of it: one it signed, or kiln's
/// for it (a person with no key on the device that asked). None if kiln has none.
async fn held_with(store: &Store, org: &str, holder: &str, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let rows: Vec<(String, String, String)> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT c.digest, c.custodian, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind = 'received' AND t.algorithm = ?1 AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC")?;
            let rows = q.query_map(params![algorithm, hex], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let server = server_uri(org);
    Ok(rows
        .into_iter()
        .find(|(_, custodian, envelope)| {
            let for_whom = if *custodian == server { body_in(envelope).and_then(|b| b.taken_for) } else { None };
            let whose = for_whom.unwrap_or_else(|| custodian.clone());
            custody::names(holder, &whose) || custody::names(&whose, holder)
        })
        .map(|(d, _, _)| d))
}

/// The newest hand-on (or take) of `thing` to `to` kiln has: its digest.
pub async fn handed_to(store: &Store, to: &str, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let to = to.to_owned();
    store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT c.digest FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND c.counterpart = ?1 AND t.algorithm = ?2 AND t.hex = ?3 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![to, algorithm, hex],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
}

/// A kept record's statement, read from its envelope as kiln took it.
pub(crate) fn statement_in(envelope: &str) -> Option<statement::Statement> {
    let bundle: Value = serde_json::from_str(envelope).ok()?;
    let payload = bundle.get("dsseEnvelope")?.get("payload")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A kept record's body, read from its envelope as kiln took it.
fn body_in(envelope: &str) -> Option<custody::Body> {
    serde_json::from_value(statement_in(envelope)?.body).ok()
}

/// The newest hand-on (or take) of `thing` to `to` that no receipt answers yet: its digest and
/// bundle (docs/ci.md W27-R08; the Transfers plan's step 7: a change given, waiting to be taken).
pub async fn waiting_for(store: &Store, to: &str, thing: &statement::Subject) -> anyhow::Result<Option<(String, Value)>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let to = to.to_owned();
    let row: Option<(String, String)> = store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT c.digest, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND c.counterpart = ?1 AND t.algorithm = ?2 AND t.hex = ?3
                 AND NOT EXISTS (SELECT 1 FROM custody_follows f JOIN custody r ON r.digest = f.record WHERE f.follows = c.digest AND r.kind = 'received')
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![to, algorithm, hex],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await?;
    Ok(row.and_then(|(d, e)| serde_json::from_str(&e).ok().map(|b| (d, b))))
}

/// The newest hand-on (or take) of `thing` away from `giver` (its signer, or whom kiln's server
/// took it for): how `giver` came to no longer hold it. Its digest (docs/ci.md W27-R08: a change's
/// former author).
pub async fn handed_from(store: &Store, org: &str, giver: &str, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    let rows: Vec<(String, String, String)> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT c.digest, c.custodian, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND t.algorithm = ?1 AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC")?;
            let rows = q.query_map(params![algorithm, hex], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let server = server_uri(org);
    Ok(rows
        .into_iter()
        .find(|(_, custodian, envelope)| {
            let for_whom = if *custodian == server { body_in(envelope).and_then(|b| b.taken_for) } else { None };
            let whose = for_whom.unwrap_or_else(|| custodian.clone());
            custody::names(giver, &whose) || custody::names(&whose, giver)
        })
        .map(|(d, _, _)| d))
}

/// The newest hand-on (or take) of `thing` kiln has, to whomever: its digest.
pub async fn newest_hand_on(store: &Store, thing: &statement::Subject) -> anyhow::Result<Option<String>> {
    let Some((algorithm, hex)) = thing.digest.iter().next().map(|(a, h)| (a.clone(), h.clone())) else { return Ok(None) };
    store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT c.digest FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind IN ('handed-on', 'taken') AND t.algorithm = ?1 AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![algorithm, hex],
                |r| r.get(0),
            )
            .optional()?)
        })
        .await
}

/// The newest work kiln took for agent session `session` (at any epoch) and handed to another: a
/// plan's step tried again with another agent, or taken over by a person. The take's digest, the
/// work's name and whom it went to.
pub async fn work_taken_from(store: &Store, session: &str) -> anyhow::Result<Option<(String, String, String)>> {
    let rows: Vec<(String, String)> = store
        .call(|c| {
            let mut q = c.prepare(
                "SELECT c.digest, c.envelope FROM custody c WHERE c.kind = 'taken' AND EXISTS (SELECT 1 FROM custody_things t WHERE t.record = c.digest AND t.algorithm = ?1)
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 256",
            )?;
            let rows = q.query_map(params![WORK], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mark = format!("/agent/{session}/epoch/");
    Ok(rows.into_iter().find_map(|(digest, envelope)| {
        let s = statement_in(&envelope)?;
        let b: custody::Body = serde_json::from_value(s.body).ok()?;
        b.taken_for.as_deref().is_some_and(|g| g.contains(&mark)).then(|| (digest, s.subject.first().map(|t| t.name.clone()).unwrap_or_default(), b.to.unwrap_or_default()))
    }))
}

/// Asks the runner process holding agent session `session` for its receipt of work `hand_on`,
/// which it signs as the session takes the work up, at its next turn (`ServerMsg::Custody`): the
/// process kiln counts as its holder, else its machine's newest. Only one that signs custody
/// (`CAP_CUSTODY`) is asked: another's hand-on stays unanswered, as a walk shows it.
pub async fn ask_receipt(hub: &crate::hub::Hub, session: &str, hand_on: &Value) {
    let Some(crate::harness::registry::Location::Machine(machine)) = hub.harness.location(session) else { return };
    let msg = crate::proto::ServerMsg::Custody { json: json!({ "session": session, "hand_on": hand_on.to_string() }).to_string() };
    let (owner, signs) = {
        let inst = hub.instances.lock();
        let owner = inst.owner(&crate::instances::Owned::Agent(session.to_owned()));
        let signs = owner.and_then(|p| inst.get(p)).is_some_and(|i| (u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0);
        (owner, signs)
    };
    match owner {
        Some(p) if signs => hub.send_peer(p, &msg).await,
        Some(_) => {}
        None if hub.runner_wide_caps(&machine) & crate::proto::CAP_CUSTODY != 0 => {
            hub.send_to_agent(&machine, crate::proto::LANE_CONTROL, &msg).await;
        }
        None => {}
    }
}

/// Agent session `parent` (over `came_from`, its runner) started subagents `children` for a task,
/// handing it on signed with its own key at its epoch (`hand_on`, beside its `agents.start` ask) to
/// kiln's server, which starts them: kiln keeps the hand-on, passes the task on to each child
/// unchanged (a relay's record, its server's), and asks each for its receipt as it takes the task
/// up, at its first turn (docs/ci.md W27-R08; the Transfers plan's step 6).
pub async fn subagents_handed(hub: &crate::hub::Hub, came_from: &str, hand_on: &Value, children: &[String]) {
    let (org, at) = (hub.pki.organization(), crate::util::now_unix());
    let taken = match take(&hub.store, &hub.pki.ca_der, &org, hand_on, came_from, at).await {
        Ok(Ok(t)) => t,
        Ok(Err(why)) => return tracing::warn!(target: "kiln.custody", %came_from, %why, "a subagent's task: its hand-on refused"),
        Err(e) => return tracing::warn!(target: "kiln.custody", %came_from, error = %format!("{e:#}"), "a subagent's task: its hand-on wasn't kept"),
    };
    let Some(s) = statement_in(&hand_on.to_string()) else { return };
    let parent = s.body.get("by").and_then(Value::as_str).unwrap_or_default().to_owned();
    for child in children {
        let to = match crate::identities::agent_at(hub, child, 0).await {
            Ok(to) => to,
            Err(e) => {
                tracing::warn!(target: "kiln.custody", %child, error = %format!("{e:#}"), "a subagent's task passed on unsigned: kiln can't name the subagent");
                continue;
            }
        };
        match pass_on(hub, s.subject.clone(), &taken.digest, &parent, &to, "a subagent's task, passed on unchanged as kiln started it").await {
            Ok((_, bundle)) => ask_receipt(hub, child, &bundle).await,
            Err(e) => tracing::warn!(target: "kiln.custody", %child, error = %format!("{e:#}"), "a subagent's task passed on wasn't kept"),
        }
    }
}

/// Runner process `peer` says it holds `sessions` (its inventory): each one's work kiln handed it
/// that no receipt answers yet is asked for again (its ask may have gone while the runner was
/// away).
pub async fn ask_again(hub: &crate::hub::Hub, peer: crate::instances::PeerId, sessions: &[String]) {
    let signs = hub.instances.lock().get(peer).is_some_and(|i| (u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0);
    if !signs {
        return;
    }
    for session in sessions {
        for envelope in unreceived(&hub.store, session).await.unwrap_or_default() {
            let msg = crate::proto::ServerMsg::Custody { json: json!({ "session": session, "hand_on": envelope }).to_string() };
            hub.send_peer(peer, &msg).await;
        }
    }
}

/// Agent session `session` opened on its runner: the work kiln handed it before (a plan's step
/// given as it started) that no receipt answers yet is asked for ([`ask_receipt`]).
pub async fn ask_open(hub: &crate::hub::Hub, session: &str) {
    for envelope in unreceived(&hub.store, session).await.unwrap_or_default() {
        if let Ok(bundle) = serde_json::from_str::<Value>(&envelope) {
            ask_receipt(hub, session, &bundle).await;
        }
    }
}

/// The work kiln handed agent session `session` (to it at any epoch) that no receipt answers yet:
/// the hand-ons' envelopes, oldest first.
async fn unreceived(store: &Store, session: &str) -> anyhow::Result<Vec<String>> {
    let pattern = format!("%/agent/{session}/epoch/%");
    store
        .call(move |c| {
            let mut q = c.prepare(
                "SELECT c.envelope FROM custody c WHERE c.kind IN ('handed-on', 'taken', 'passed-on') AND c.counterpart LIKE ?1
                 AND EXISTS (SELECT 1 FROM custody_things t WHERE t.record = c.digest AND t.algorithm = ?2)
                 AND NOT EXISTS (SELECT 1 FROM custody_follows f JOIN custody r ON r.digest = f.record WHERE f.follows = c.digest AND r.kind = 'received')
                 ORDER BY c.taken_at, c.rowid",
            )?;
            let rows = q.query_map(params![pattern, WORK], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// The newest receipt custodian `custodian` signed: how it came to hold what it holds.
async fn receipt_of(store: &Store, custodian: &str) -> anyhow::Result<Option<String>> {
    let c = custodian.to_owned();
    store.call(move |conn| Ok(conn.query_row("SELECT digest FROM custody WHERE custodian = ?1 AND kind = 'received' ORDER BY taken_at DESC, rowid DESC LIMIT 1", [&c], |r| r.get(0)).optional()?)).await
}

/// Agent session `session`'s hand-over to epoch `epoch` went unsigned (docs/ci.md W27-R08): from
/// `gave` to `took`, as kiln knows them, because `why`. Kept once per session and epoch, and
/// audited; whether it's new.
pub async fn unsigned(store: &Store, session: &str, epoch: u64, gave: &str, took: &str, why: &str, at: i64) -> anyhow::Result<bool> {
    let (s, g, t, w) = (session.to_owned(), gave.to_owned(), took.to_owned(), why.to_owned());
    let e = i64::try_from(epoch).unwrap_or(i64::MAX);
    let new = store
        .call(move |c| Ok(c.execute("INSERT OR IGNORE INTO transfers_unsigned (session, epoch, gave, took, why, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![s, e, g, t, w, at])? > 0))
        .await?;
    if new {
        tracing::info!(target: "kiln.custody", %session, epoch, %gave, %took, %why, "an agent session's hand-over went unsigned");
        store.audit("kiln".into(), "custody.unsigned".into(), format!("session {session} to epoch {epoch}, from {} to {}: {why}", or_unknown(gave), or_unknown(took))).await;
    }
    Ok(new)
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() { "a process kiln never certified" } else { s }
}

/// The hand-overs that went unsigned, newest first, at most `limit`.
pub async fn unsigned_list(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT session, epoch, gave, took, why, at FROM transfers_unsigned ORDER BY id DESC LIMIT ?1")?;
            let rows = q
                .query_map([n], |r| {
                    Ok(json!({ "session": r.get::<_, String>(0)?, "epoch": r.get::<_, i64>(1)?, "gave": r.get::<_, String>(2)?, "took": r.get::<_, String>(3)?, "why": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// Whether kiln keeps record `digest`.
pub(crate) async fn kept(store: &Store, digest: &str) -> anyhow::Result<bool> {
    let d = digest.to_owned();
    store.call(move |c| Ok(c.query_row("SELECT 1 FROM custody WHERE digest = ?1", [&d], |_| Ok(())).optional()?.is_some())).await
}

/// Whether hand-on `b` (a custody record's body) hands an agent session on to `issued`, the identity
/// kiln certifies it at at `epoch` ([`transfer`]; the transfers simulation, `crate::sim::transfers`,
/// checks one so too). Why not, when it doesn't.
pub fn hands_on_to(b: &custody::Body, issued: &str, epoch: u64) -> Result<(), String> {
    if !b.kind.gives() || b.epoch != Some(epoch) || !b.to.as_deref().is_some_and(|to| custody::names(to, issued)) {
        let to = b.to.as_deref().unwrap_or("no one");
        return Err(format!("its hand-on ({}) is to {to} at epoch {}, not to {issued}", kind_name(b.kind), b.epoch.map_or_else(|| "none".to_owned(), |e| e.to_string())));
    }
    Ok(())
}

/// A hand-over's records, taken as the session's process at its new epoch `epoch` asks for its
/// certificate (docs/ci.md W27-R08; the Transfers plan's step 3), at `at`: the hand-on its
/// package carried (`hand_on`, a bundle as JSON), then its receipt, signed with the key being
/// certified and bundled here with the certificate `pem` kiln issued it for identity `issued`.
/// Both are kept before the epoch rises: the caller revokes the epoch left, and records the
/// certificate, only then. The receipt's digest, or why not (each refusal recorded as [`take`]
/// records them): the hand-on refused (a copy left behind's, its epoch revoked), or not to
/// `issued` at `epoch`; the receipt missing, refused, or not following it. Then kiln doesn't
/// certify the key. A receipt of other than what was handed on is kept, its break recorded.
#[allow(clippy::too_many_arguments)]
pub async fn transfer(store: &Store, root: &[u8], org: &str, came_from: &str, issued: &str, epoch: u64, hand_on: &str, receipt: Option<&crate::proto::Unbundled>, pem: &str, at: i64) -> anyhow::Result<Result<String, String>> {
    let Ok(given) = serde_json::from_str::<Value>(hand_on) else { return Ok(Err("its hand-on can't be read".into())) };
    let handed = match take(store, root, org, &given, came_from, at).await? {
        Ok(t) => t.digest,
        Err(why) => return Ok(Err(format!("its hand-on was refused: {why}"))),
    };
    // Kept, so checked when kiln took it: read again, revocations aside.
    let b = statement::verify(&given, root, &[], at).map_err(|e| anyhow::anyhow!("{e}")).and_then(|c| custody::body(&c).map_err(|e| anyhow::anyhow!("{e}")))?;
    if let Err(why) = hands_on_to(&b, issued, epoch) {
        return Ok(Err(why));
    }
    let Some(r) = receipt else { return Ok(Err("no receipt came with its hand-on".into())) };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(payload), Ok(signature)) = (b64.decode(&r.payload), b64.decode(&r.signature)) else { return Ok(Err("its receipt can't be read".into())) };
    let bundle = statement::bundle(&r.payload_type, &payload, &signature, &crate::pki::der_of(pem)?);
    let got = match take(store, root, org, &bundle, came_from, at).await? {
        Ok(t) => t,
        Err(why) => return Ok(Err(format!("its receipt was refused: {why}"))),
    };
    // A receipt of other than what was handed on breaks the chain there, recorded (`take`): the
    // session still goes on where it's held, as the chain says.
    if let Some(broke) = &got.broke {
        tracing::warn!(target: "kiln.custody", record = %got.digest, %broke, "a hand-over's receipt breaks its chain");
    }
    let follows = statement::verify(&bundle, root, &[], at).map(|c| c.statement.follows).unwrap_or_default();
    if !follows.contains(&handed) {
        return Ok(Err(format!("its receipt doesn't follow its hand-on {handed}")));
    }
    Ok(Ok(got.digest))
}

/// Keeps `c`: whether it's new (false: kiln had it).
async fn keep(store: &Store, c: &Checked, kind: Option<Kind>, counterpart: Option<String>, bundle: &Value, came_from: &str) -> anyhow::Result<bool> {
    let (digest, payload_type, kind, custodian) = (c.digest.clone(), c.payload_type.clone(), kind.map(kind_name), c.custodian.clone());
    let (stream, seq, under, envelope, came_from, taken_at) = (c.statement.stream.clone(), c.statement.seq.and_then(|s| i64::try_from(s).ok()), c.statement.under.clone(), bundle.to_string(), came_from.to_owned(), c.at);
    let follows = c.statement.follows.clone();
    let things: Vec<(String, String, String)> = c.statement.subject.iter().flat_map(|s| s.digest.iter().map(|(alg, hex)| (s.name.clone(), alg.clone(), hex.clone()))).collect();
    store
        .call(move |conn| {
            let tx = crate::store::tx(conn)?;
            let n = tx.execute(
                "INSERT OR IGNORE INTO custody (digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![digest, payload_type, kind, custodian, counterpart, stream, seq, under, envelope, came_from, taken_at],
            )?;
            if n == 0 {
                return Ok(false);
            }
            for f in &follows {
                tx.execute("INSERT OR IGNORE INTO custody_follows (record, follows) VALUES (?1, ?2)", params![digest, f])?;
            }
            for (name, alg, hex) in &things {
                tx.execute("INSERT INTO custody_things (record, name, algorithm, hex) VALUES (?1, ?2, ?3, ?4)", params![digest, name, alg, hex])?;
            }
            crate::trail::take(&tx, &envelope, taken_at)?;
            tx.commit()?;
            Ok(true)
        })
        .await
}

/// The records that follow record `digest`.
async fn followers(store: &Store, digest: &str) -> anyhow::Result<Vec<String>> {
    let d = digest.to_owned();
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT record FROM custody_follows WHERE follows = ?1")?;
            let rows = q.query_map([d], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// The records kiln has among `digests`, each checked as of when kiln took it (a revocation
/// since doesn't undo it).
async fn load(store: &Store, root: &[u8], digests: &[String], revocations: &[Revocation]) -> anyhow::Result<BTreeMap<String, Checked>> {
    let wanted = digests.to_vec();
    let rows: Vec<(String, String, i64)> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest, envelope, taken_at FROM custody WHERE digest = ?1")?;
            let mut out = Vec::new();
            for d in &wanted {
                if let Some(row) = q.query_row([d], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()? {
                    out.push(row);
                }
            }
            Ok(out)
        })
        .await?;
    let mut map = BTreeMap::new();
    for (d, envelope, at) in rows {
        let Ok(bundle) = serde_json::from_str::<Value>(&envelope) else { continue };
        // Only what was revoked before kiln took it: kiln checked it then, and a revocation since
        // (one in the same second included) doesn't undo it.
        let before: Vec<Revocation> = revocations.iter().filter(|r| r.at < at).cloned().collect();
        match statement::verify(&bundle, root, &before, at) {
            Ok(c) => {
                map.insert(d, c);
            }
            Err(e) => tracing::warn!(record = %d, error = %e, "a custody record kiln kept no longer checks as of when it took it"),
        }
    }
    Ok(map)
}

/// Checks record `digest`'s own hop, at `at`: a break, recorded, if it doesn't hold; none if it
/// holds or waits for a record it follows that kiln doesn't have yet, or, taken for someone, the
/// delegation it acts under (W27-R08: another machine's, which comes in any order).
async fn check(store: &Store, root: &[u8], org: &str, digest: &str, revocations: &[Revocation], at: i64) -> anyhow::Result<Option<String>> {
    let mine = load(store, root, &[digest.to_owned()], revocations).await?;
    let Some(r) = mine.get(digest) else { return Ok(None) };
    let follows = r.statement.follows.clone();
    let held_for = custody::body(r).is_ok_and(|b| b.kind.takes() && b.held_for.as_deref().is_some_and(|w| !w.is_empty()));
    let wanted: Vec<String> = follows.iter().cloned().chain(r.statement.under.clone().filter(|_| held_for)).collect();
    let mut map = load(store, root, &wanted, revocations).await?;
    if wanted.iter().any(|f| !map.contains_key(f)) {
        return Ok(None);
    }
    map.extend(mine);
    match custody::hop(digest, &map, &server_uri(org)) {
        Ok(_) => Ok(None),
        Err(b) => {
            let mut around = follows;
            around.insert(0, digest.to_owned());
            record_break(store, digest, &b, &around, &relay_named(&b, digest, &map), at).await?;
            Ok(Some(b.to_string()))
        }
    }
}

/// Records break `b` at record `record`, with the records around it, once; naming `relay` when
/// it's in what a relay passed on ([`relay_named`]).
async fn record_break(store: &Store, record: &str, b: &Break, around: &[String], relay: &str, at: i64) -> anyhow::Result<()> {
    insert_break(store, record, b.code(), &b.to_string(), around, "", relay, at).await
}

/// The relay break `b` at record `record` names, when it's in what a relay passed on (docs/ci.md
/// W27-R08's relays): the broken record's signer when it's a relay's record, else the signer of the
/// relay's record it follows; "" for none.
fn relay_named(b: &Break, record: &str, map: &BTreeMap<String, Checked>) -> String {
    let passed = |d: &str| map.get(d).filter(|c| custody::body(c).is_ok_and(|b| b.kind == Kind::PassedOn)).map(|c| c.custodian.clone());
    let from = match b {
        Break::Changed { from, .. }
        | Break::NoCommonDigest { from, .. }
        | Break::NotHandedOn { from, .. }
        | Break::NotAll { from, .. }
        | Break::NotTheTaker { from, .. }
        | Break::NotTheGiver { from, .. }
        | Break::NotTheHolder { from, .. } => Some(from.as_str()),
        _ => None,
    };
    passed(record).or_else(|| from.and_then(passed)).unwrap_or_default()
}

/// Records a break found in what relay `relay` (its identity's URI) passed on (docs/ci.md
/// W27-R08's relays): at record `record`, the relay's record by its digest; its kind (`code`, as
/// `kiln_ci::identity::relay::Unrelayed::code` names it) and words; the records around it; and
/// what was refused because of it, if anything. Once.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn relay_break(store: &Store, record: &str, code: &str, detail: &str, around: &[String], refused: &str, relay: &str, at: i64) -> anyhow::Result<()> {
    insert_break(store, record, code, detail, around, refused, relay, at).await
}

#[allow(clippy::too_many_arguments)]
async fn insert_break(store: &Store, record: &str, kind: &str, detail: &str, around: &[String], refused: &str, relay: &str, at: i64) -> anyhow::Result<()> {
    let (r, kind, detail, around_json, refused, relay) = (record.to_owned(), kind.to_owned(), detail.to_owned(), serde_json::to_string(around)?, refused.to_owned(), relay.to_owned());
    let (rr, dd) = (r.clone(), detail.clone());
    let new = store
        .call(move |c| {
            let seen: Option<i64> = c.query_row("SELECT id FROM custody_breaks WHERE record = ?1 AND kind = ?2 AND detail = ?3", params![r, kind, detail], |x| x.get(0)).optional()?;
            if seen.is_some() {
                return Ok(false);
            }
            c.execute("INSERT INTO custody_breaks (record, kind, detail, around, refused, relay, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![r, kind, detail, around_json, refused, relay, at])?;
            Ok(true)
        })
        .await?;
    if new {
        tracing::warn!(target: "kiln.custody", record = %rr, detail = %dd, "a chain of custody doesn't hold");
        let entry = crate::audit::Entry { principal: "kiln".into(), action: "custody.break".into(), target: Some(rr), result: "broken".into(), detail: dd, ..crate::audit::Entry::default() };
        crate::audit::write(store, entry).await;
    }
    Ok(())
}

/// Records that kiln, or a runner process, refused `what`, which relay `relay` passed on, for
/// `why` (docs/ci.md W27-R08's relays): the relay's record by its digest and envelope, and what it
/// came over.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refused_relayed(store: &Store, digest: &str, what: &str, relay: &str, why: &str, envelope: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    let r = Refusal { digest: digest.to_owned(), act: what.to_owned(), signer: relay.to_owned(), why: why.to_owned(), envelope: envelope.to_owned(), came_from: came_from.to_owned(), at, ..Refusal::default() };
    refuse(store, r).await
}

/// Keeps `bundle`, an originator's own signed statement that a relay passed on (an agent session's
/// act: docs/ci.md W27-R08's relays), by its digest, so a walk from a relay's record finds it: not
/// a custody record, so of no kind; checked as of `at`, revocations aside (whether the act counts
/// is its own check's). Whether it's new.
pub(crate) async fn keep_originator(store: &Store, root: &[u8], bundle: &Value, came_from: &str, at: i64) -> anyhow::Result<bool> {
    let c = statement::verify(bundle, root, &[], at).map_err(|e| anyhow::anyhow!("{e}"))?;
    keep(store, &c, None, None, bundle, came_from).await
}

/// Records refusal `r`, and audits it.
async fn refuse(store: &Store, r: Refusal) -> anyhow::Result<()> {
    let entry = crate::audit::Entry {
        principal: if r.signer.is_empty() { "unknown".into() } else { r.signer.clone() },
        action: "custody.refused".into(),
        target: Some(if r.digest.is_empty() { r.act.clone() } else { r.digest.clone() }),
        result: "refused".into(),
        detail: r.why.clone(),
        ..crate::audit::Entry::default()
    };
    tracing::info!(target: "kiln.custody", digest = %r.digest, signer = %r.signer, why = %r.why, "refused {}", r.act);
    // What an API caller sent is audited as their request already, one row for it (W19-AC04);
    // what a runner sent, which no request audits, is audited here.
    let audited = !r.came_from.starts_with("api:");
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO transfer_refusals (digest, act, signer, why, envelope, came_from, at, epoch, superseded_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![r.digest, r.act, r.signer, r.why, r.envelope, r.came_from, r.at, r.epoch.map(|e| i64::try_from(e).unwrap_or(i64::MAX)), r.superseded_by],
            )?;
            Ok(())
        })
        .await?;
    if audited {
        crate::audit::write(store, entry).await;
    }
    Ok(())
}

/// Records that kiln refused `act` by `signer`, who no longer holds what it acts on: superseded by
/// `transfer`, the hand-over that moved it (docs/ci.md W27-R08: a change's former author).
pub async fn refused_former(store: &Store, act: &str, signer: &str, why: &str, transfer: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    refuse(store, Refusal { act: act.to_owned(), signer: signer.to_owned(), why: why.to_owned(), superseded_by: transfer.to_owned(), came_from: came_from.to_owned(), at, ..Refusal::default() }).await
}

/// Records that kiln refused act `act` (a statement's digest `digest`, signed by `signer`) for
/// `why`: what the fence and the act checks call (W27-R08).
pub async fn refused(store: &Store, digest: &str, act: &str, signer: &str, why: &str, came_from: &str, at: i64) -> anyhow::Result<()> {
    refuse(store, Refusal { digest: digest.to_owned(), act: act.to_owned(), signer: signer.to_owned(), why: why.to_owned(), came_from: came_from.to_owned(), at, ..Refusal::default() }).await
}

/// The record a walk starts from: `digest` if it's a record kiln has; else, as a thing's digest
/// (`sha256:…`, `gitCommit:…`), the head of that thing's chain: a record about it that no other
/// record about it follows (records arrive in any order: a receipt may come before its hand-on),
/// the newest such when its chain forks.
async fn start(store: &Store, digest: &str) -> anyhow::Result<Option<String>> {
    let d = digest.to_owned();
    store
        .call(move |c| {
            if let Some(r) = c.query_row("SELECT digest FROM custody WHERE digest = ?1", [&d], |x| x.get::<_, String>(0)).optional()? {
                return Ok(Some(r));
            }
            let Some((alg, hex)) = d.split_once(':') else { return Ok(None) };
            Ok(c.query_row(
                "SELECT t.record FROM custody_things t JOIN custody c ON c.digest = t.record
                 WHERE t.algorithm = ?1 AND t.hex = ?2 AND c.kind IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM custody_follows f JOIN custody_things n ON n.record = f.record WHERE f.follows = t.record AND n.algorithm = t.algorithm AND n.hex = t.hex)
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                params![alg, hex.to_ascii_lowercase()],
                |x| x.get(0),
            )
            .optional()?)
        })
        .await
}

/// The head of the chain of the newest thing named `name` ("session 01a1751c at epoch 3"; a name
/// ending in `*` matches every name it begins: "session 01a1751c at epoch *"): the record a walk of
/// it starts from, none if kiln has no record of it.
pub async fn head_named(store: &Store, name: &str) -> anyhow::Result<Option<String>> {
    let escape = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = name.strip_suffix('*').map_or_else(|| escape(name), |p| format!("{}%", escape(p)));
    store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT t.record FROM custody_things t JOIN custody c ON c.digest = t.record
                 WHERE t.name LIKE ?1 ESCAPE '\\' AND c.kind IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM custody_follows f WHERE f.follows = t.record)
                 ORDER BY c.taken_at DESC, c.rowid DESC LIMIT 1",
                [pattern],
                |x| x.get(0),
            )
            .optional()?)
        })
        .await
}

/// One side of an approval carried across a rebase-only version ([`carry`]): the version, its
/// commit and the base it's on.
pub struct Side<'a> {
    pub n: u32,
    pub commit: &'a str,
    pub base: &'a str,
}

/// An approval carried across a rebase-only version, as a transfer (docs/ci.md W27-R08, with
/// W27-R04; the Transfers plan's step 7): verdict `verdict` (its statement's digest), given on
/// version `from` of `diff` (`acme/review D1`), holds on `to`, a later one with the same verbatim
/// patch-id `patch_id`. kiln's server hands it on for the version it was given on and receives it
/// for the one it reached, each record naming its version's commit and base (`component`), and
/// why: "carried from v2: rebase only, patch-id 9f3c41a…, base a1b2c3d to c3d4e5f". Both are kept;
/// what comes back is the receipt's digest and that why. Its bodies are what the versions say, no
/// time of their own, so carrying it again (the verdicts read again) gives the same records, kept
/// once.
pub async fn carry(hub: &crate::hub::Hub, diff: &str, verdict: &str, from: &Side<'_>, to: &Side<'_>, patch_id: &str) -> anyhow::Result<Result<(String, String), String>> {
    let short = |s: &str| s.get(..7).unwrap_or(s).to_owned();
    let why = format!("carried from v{}: rebase only, patch-id {}…, base {} to {}", from.n, short(patch_id), short(from.base), short(to.base));
    let hex = verdict.strip_prefix("sha256:").unwrap_or(verdict);
    let things = vec![
        statement::Subject { name: format!("verdict {} on {diff}", short(hex)), digest: BTreeMap::from([("sha256".to_owned(), hex.to_owned())]), ..statement::Subject::default() },
        statement::Subject { name: format!("{diff}'s patch"), digest: BTreeMap::from([(kiln_ci::identity::verdict::PATCH_ID.to_owned(), patch_id.to_owned())]), ..statement::Subject::default() },
    ];
    let (org, at) = (hub.pki.organization(), crate::util::now_unix());
    let (server, came_from) = (server_uri(&org), format!("kiln's review of {diff}"));
    let version = |v: &Side<'_>| format!("v{} at {} on base {}", v.n, short(v.commit), short(v.base));
    let b = custody::Body { to: Some(server.clone()), component: Some(version(from)), why: why.clone(), ..custody::Body::new(Kind::HandedOn, "") };
    let hand_on = server_signed(&hub.pki, &statement::Statement { subject: things.clone(), ..statement::Statement::new(custody::to_value(&b)) })?;
    let handed = match take(&hub.store, &hub.pki.ca_der, &org, &hand_on, &came_from, at).await? {
        Ok(t) => t.digest,
        Err(why) => return Ok(Err(format!("its hand-on wasn't kept: {why}"))),
    };
    let b = custody::Body { from: Some(server), component: Some(version(to)), why: why.clone(), ..custody::Body::new(Kind::Received, "") };
    let receipt = server_signed(&hub.pki, &statement::Statement { subject: things, follows: vec![handed], ..statement::Statement::new(custody::to_value(&b)) })?;
    Ok(match take(&hub.store, &hub.pki.ca_der, &org, &receipt, &came_from, at).await? {
        Ok(t) => Ok((t.digest, why)),
        Err(why) => Err(format!("its receipt wasn't kept: {why}")),
    })
}

/// What a runner's blue/green upgrade on `machine`, under way since `since` (unix seconds), handed
/// on, as a replant's record of it lists them (docs/ci.md W27-R08; the Transfers plan's step 8):
/// the machine's runner role, from the process it replaced to its new build, and each of
/// `sessions` (the agent sessions it held as the upgrade began), each a transfer kiln took since,
/// signed by both sides and walking unbroken; and what moved without one.
pub async fn replanted(store: &Store, root: &[u8], org: &str, machine: &str, since: i64, sessions: &[String]) -> anyhow::Result<Value> {
    let (mut transfers, mut without) = (Vec::new(), Vec::new());
    let things = std::iter::once((format!("runner role of {machine}"), format!("runner role of {machine}"))).chain(sessions.iter().map(|s| (format!("session {s}"), format!("session {s} at epoch *"))));
    for (what, name) in things {
        match transfer_since(store, root, org, &name, since).await? {
            Some(mut t) => {
                if let Some(o) = t.as_object_mut() {
                    o.insert("what".into(), json!(what));
                }
                transfers.push(t);
            }
            None => without.push(what),
        }
    }
    Ok(json!({ "transfers": transfers, "moved_without_a_transfer": without }))
}

/// The newest transfer of the thing named `name` (a trailing `*` matching any end) that kiln took
/// since `since`: its receipt and the hand-on or take it follows, both signed, its chain walking
/// unbroken. None otherwise.
async fn transfer_since(store: &Store, root: &[u8], org: &str, name: &str, since: i64) -> anyhow::Result<Option<Value>> {
    let Some(head) = head_named(store, name).await? else { return Ok(None) };
    let Some(v) = chain(store, root, org, &head, crate::util::now_unix()).await? else { return Ok(None) };
    if v.get("unbroken") != Some(&json!(true)) {
        return Ok(None);
    }
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let s = |h: &Value, k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(match (hops.first(), hops.get(1)) {
        (Some(got), Some(gave))
            if s(got, "kind") == "received"
                && matches!(s(gave, "kind").as_str(), "handed-on" | "taken")
                && !s(got, "signer").is_empty()
                && !s(gave, "signer").is_empty()
                && got.get("at").and_then(Value::as_i64).is_some_and(|at| at >= since) =>
        {
            Some(json!({ "record": s(got, "record"), "from": s(gave, "custodian"), "to": s(got, "custodian"), "at": got.get("at") }))
        }
        _ => None,
    })
}

/// The digests of every record kiln has on custodian `custodian`'s stream `stream`.
async fn on_stream(store: &Store, custodian: &str, stream: &str) -> anyhow::Result<Vec<String>> {
    let (who, s) = (custodian.to_owned(), stream.to_owned());
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest FROM custody WHERE custodian = ?1 AND stream = ?2 ORDER BY seq LIMIT ?3")?;
            let rows = q.query_map(params![who, s, i64::try_from(WALK_MAX).unwrap_or(i64::MAX)], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await
}

/// A thing's chain of custody, walked now from `digest` (a record's, or a thing's): each hop with
/// both sides and who signed it, or the first break, recorded. None when kiln has no record of it.
pub async fn chain(store: &Store, root: &[u8], org: &str, digest: &str, at: i64) -> anyhow::Result<Option<Value>> {
    let Some(from) = start(store, digest).await? else { return Ok(None) };
    let revocations = crate::identities::revocations(store).await?;
    let mut map: BTreeMap<String, Checked> = BTreeMap::new();
    let mut frontier = vec![from.clone()];
    while !frontier.is_empty() && map.len() < WALK_MAX {
        let got = load(store, root, &frontier, &revocations).await?;
        let mut next: BTreeSet<String> = BTreeSet::new();
        for c in got.values() {
            // What it follows, and the delegation it acts under (a record taken for someone is
            // judged by it: W27-R08).
            next.extend(c.statement.follows.iter().chain(c.statement.under.iter()).filter(|f| !map.contains_key(*f) && !got.contains_key(*f)).cloned());
        }
        map.extend(got);
        frontier = next.into_iter().filter(|d| !map.contains_key(d)).collect();
    }
    let streams: BTreeSet<(String, String)> = map.values().filter_map(|c| c.statement.stream.clone().map(|s| (c.custodian.clone(), s))).collect();
    for (who, s) in streams {
        let more: Vec<String> = on_stream(store, &who, &s).await?.into_iter().filter(|d| !map.contains_key(d)).collect();
        let got = load(store, root, &more, &revocations).await?;
        map.extend(got);
    }
    let view = |h: &Hop| {
        let c = map.get(&h.record);
        let b = c.and_then(|c| custody::body(c).ok());
        json!({
            "record": h.record,
            "kind": kind_name(h.kind),
            "custodian": h.custodian,
            // kiln's take, or its receipt, for someone who couldn't sign: "kiln for sam".
            "taken_for": h.taken_for,
            "signer": c.map(|c| c.signer.to_string()),
            "to": b.as_ref().and_then(|b| b.to.clone()),
            "from": b.as_ref().and_then(|b| b.from.clone()),
            // The part of its custodian that holds them: a promotion's environment.
            "component": b.as_ref().and_then(|b| b.component.clone()),
            // Whom it holds them for, and the delegation it takes them under (W27-R08).
            "for": b.as_ref().and_then(|b| b.held_for.clone()),
            "under": c.and_then(|c| c.statement.under.clone()),
            "why": b.as_ref().map(|b| b.why.clone()),
            "at": h.at,
            "subject": h.subject,
            "made_from": h.made_from,
            "began": h.began,
        })
    };
    // Who holds it as of that record: its taker; or, for an offer no record answers yet, its giver,
    // until it's taken (and after it lapses).
    let (mut open, mut lapsed, mut held_by) = (false, false, None);
    if let Some((c, b)) = map.get(&from).and_then(|c| custody::body(c).ok().map(|b| (c, b))) {
        if b.kind == Kind::Received {
            // kiln's receipt for a person with no key on the device that asked: theirs to hold.
            held_by = Some(b.taken_for.clone().filter(|_| c.custodian == server_uri(org)).unwrap_or_else(|| c.custodian.clone()));
        } else if followers(store, &from).await?.is_empty() {
            open = true;
            lapsed = at - c.at > LAPSE_AFTER;
            held_by = Some(if b.kind == Kind::Taken { b.taken_for.clone().unwrap_or_default() } else { c.custodian.clone() });
        }
    }
    Ok(Some(match custody::walk(&from, &map, &server_uri(org)) {
        Ok(w) => json!({ "from": from, "unbroken": true, "open": open, "lapsed": lapsed, "held_by": held_by, "hops": w.hops.iter().map(view).collect::<Vec<_>>(), "began": w.began }),
        Err(b) => {
            let at_record = break_record(&b).unwrap_or(&from).to_owned();
            // A gap in a relay's stream names the relay: records it passed on never came.
            let relay = match &b {
                Break::Gap(statement::Gap::Missing { custodian, stream, .. } | statement::Gap::Twice { custodian, stream, .. })
                    if map.values().any(|c| c.custodian == *custodian && c.statement.stream.as_deref() == Some(stream) && custody::body(c).is_ok_and(|b| b.kind == Kind::PassedOn)) =>
                {
                    custodian.clone()
                }
                _ => relay_named(&b, &at_record, &map),
            };
            record_break(store, &at_record, &b, std::slice::from_ref(&from), &relay, at).await?;
            json!({ "from": from, "unbroken": false, "break": { "kind": b.code(), "record": at_record, "detail": b.to_string(), "relay": relay } })
        }
    }))
}

/// The record a break is at.
fn break_record(b: &Break) -> Option<&str> {
    Some(match b {
        Break::NoRecord { followed_by: Some(r), .. } => r,
        Break::NoRecord { record, .. }
        | Break::NotCustody { record, .. }
        | Break::Unreadable { record, .. }
        | Break::Changed { record, .. }
        | Break::NoCommonDigest { record, .. }
        | Break::NotHandedOn { record, .. }
        | Break::NotAll { record, .. }
        | Break::NotTheTaker { record, .. }
        | Break::NotTheGiver { record, .. }
        | Break::NotTheHolder { record, .. }
        | Break::NoHandOn { record, .. }
        | Break::OutOfOrder { record, .. }
        | Break::TakenNotByServer { record, .. }
        | Break::NotEntitled { record, .. } => record,
        Break::Gap(_) => return None,
    })
}

/// The breaks kiln found, newest first.
pub async fn breaks(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT record, kind, detail, around, refused, at, relay FROM custody_breaks ORDER BY at DESC, id DESC LIMIT ?1")?;
            let rows = q
                .query_map([n], |r| {
                    let around: String = r.get(3)?;
                    // The relay, when it's in what one passed on (docs/ci.md W27-R08's relays).
                    let relay: String = r.get(6)?;
                    Ok(json!({ "record": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "detail": r.get::<_, String>(2)?, "around": serde_json::from_str::<Value>(&around).unwrap_or(Value::Null), "refused": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)?, "relay": relay }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// What kiln refused, newest first.
pub async fn refusals(store: &Store, limit: usize) -> anyhow::Result<Vec<Value>> {
    let n = i64::try_from(limit).unwrap_or(i64::MAX);
    store
        .call(move |c| {
            let mut q = c.prepare("SELECT digest, act, signer, why, came_from, at, epoch, superseded_by FROM transfer_refusals ORDER BY at DESC, id DESC LIMIT ?1")?;
            let rows = q
                .query_map([n], |r| {
                    Ok(json!({ "digest": r.get::<_, String>(0)?, "act": r.get::<_, String>(1)?, "signer": r.get::<_, String>(2)?, "why": r.get::<_, String>(3)?, "came_from": r.get::<_, String>(4)?, "at": r.get::<_, i64>(5)?, "epoch": r.get::<_, Option<i64>>(6)?, "superseded_by": r.get::<_, String>(7)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// `kiln custody`: a thing's chain of custody, and the breaks found, over the API (W27-R08).
#[derive(clap::Args)]
pub struct CustodyArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `audit.read`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: CustodyCmd,
}

#[derive(clap::Subcommand)]
enum CustodyCmd {
    /// A thing's chain of custody, walked now: from a record's digest, or a thing's
    /// (`sha256:…`, `gitCommit:…`), each hop with who signed it, or where it breaks.
    Show { digest: String },
    /// The breaks kiln found in chains of custody, newest first.
    Breaks,
}

/// `kiln transfers`: what kiln refused because its signer no longer held what it acted on, and the
/// hand-overs that went unsigned.
#[derive(clap::Args)]
pub struct TransfersArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `audit.read`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: TransfersCmd,
}

#[derive(clap::Subcommand)]
enum TransfersCmd {
    /// What kiln refused, newest first: when, what, who signed it, and why.
    Refused,
    /// Agent sessions' hand-overs that went unsigned, newest first: when, the session and the
    /// epoch it went on at, from whom to whom, and why.
    Unsigned,
}

/// `kiln custody`.
pub async fn cli(args: CustodyArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    use crate::ci::cli::{Api, enc, when};
    use crate::http::client::Method;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        CustodyCmd::Show { digest } => {
            let v = api.call_api(Method::Get, &format!("/api/custody?digest={}", enc(&digest)), None).await?;
            if v.get("unbroken").and_then(Value::as_bool) == Some(true) {
                let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
                println!("unbroken: {} hop(s) from {}", hops.len(), s(&v, "from"));
                for h in hops {
                    let whom = if s(&h, "kind") == "received" { format!("from {}", s(&h, "from")) } else { format!("to {}", s(&h, "to")) };
                    println!("{}  {:<10} {}  {whom}  {}", when(h.get("at").and_then(Value::as_i64)), s(&h, "kind"), s(&h, "signer"), s(&h, "record"));
                }
            } else {
                let b = v.get("break").cloned().unwrap_or(Value::Null);
                println!("broken at {}: {}", s(&b, "record"), s(&b, "detail"));
            }
        }
        CustodyCmd::Breaks => {
            let v = api.call_api(Method::Get, "/api/custody/breaks", None).await?;
            for b in v.get("breaks").and_then(Value::as_array).cloned().unwrap_or_default() {
                println!("{}  {:<16} {}  {}", when(b.get("at").and_then(Value::as_i64)), s(&b, "kind"), s(&b, "record"), s(&b, "detail"));
            }
        }
    }
    Ok(())
}

/// `kiln transfers`.
pub async fn transfers_cli(args: TransfersArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    use crate::ci::cli::{Api, when};
    use crate::http::client::Method;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        TransfersCmd::Refused => {
            let v = api.call_api(Method::Get, "/api/transfers/refused", None).await?;
            for r in v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default() {
                let signer = s(&r, "signer");
                println!("{}  {}  {}  {}", when(r.get("at").and_then(Value::as_i64)), s(&r, "act"), if signer.is_empty() { "unknown signer".to_owned() } else { signer }, s(&r, "why"));
            }
        }
        TransfersCmd::Unsigned => {
            let v = api.call_api(Method::Get, "/api/transfers/unsigned", None).await?;
            for u in v.get("unsigned").and_then(Value::as_array).cloned().unwrap_or_default() {
                let epoch = u.get("epoch").and_then(Value::as_i64).unwrap_or_default();
                println!("{}  session {} to epoch {epoch}  from {} to {}  {}", when(u.get("at").and_then(Value::as_i64)), s(&u, "session"), or_unknown(&s(&u, "gave")), or_unknown(&s(&u, "took")), s(&u, "why"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::identity::custody::{Body, to_value};
    use kiln_ci::identity::statement::{Statement, Subject, bundle, payload, to_sign};
    use rcgen::SigningKey as _;

    const ORG: &str = "hyper-light";
    const FEDORA: &str = "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed";
    const JUPITER: &str = "kiln://hyper-light/person/ada/machine/jupiter/instance/01a18b00";
    const LEFT: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/0/machine/fedora/instance/01a14c4f";

    /// A custodian with a key, certified by `pki` as `uri`.
    struct Signer {
        uri: String,
        key: rcgen::KeyPair,
        der: Vec<u8>,
        /// Its certificate, as kiln records what it issues.
        pem: String,
    }

    impl Signer {
        fn new(pki: &crate::pki::Pki, uri: &str) -> anyhow::Result<Self> {
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
            let pem = pki.issue_identity(&csr, &Identity::parse(uri)?, &[], time::Duration::hours(1))?;
            Ok(Self { uri: uri.to_owned(), key: rcgen::KeyPair::from_pem(&key)?, der: crate::pki::der_of(&pem)?, pem })
        }

        /// `s`'s payload as this custodian signs it: its body naming it.
        fn payload(&self, s: &Statement) -> anyhow::Result<Vec<u8>> {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            Ok(payload(&s)?)
        }

        /// The digest `s` has once this custodian signs it.
        fn digest(&self, s: &Statement) -> anyhow::Result<String> {
            Ok(statement::digest(PAYLOAD_CUSTODY, &self.payload(s)?))
        }

        fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
            let p = self.payload(s)?;
            Ok(bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
        }
    }

    fn thing(name: &str, c: char) -> Subject {
        Subject { name: name.into(), digest: BTreeMap::from([("sha256".to_owned(), c.to_string().repeat(64))]), ..Subject::default() }
    }

    fn record(kind: Kind, what: &Subject, to: Option<&str>, from: Option<&str>, follows: &[&str], stream: &str, seq: u64) -> Statement {
        let b = Body { to: to.map(str::to_owned), from: from.map(str::to_owned), why: "a test".into(), ..Body::new(kind, "") };
        Statement { subject: vec![what.clone()], follows: follows.iter().map(|d| (*d).to_owned()).collect(), stream: Some(stream.into()), seq: Some(seq), ..Statement::new(to_value(&b)) }
    }

    async fn count(store: &Store, table: &'static str) -> anyhow::Result<i64> {
        store.call(move |c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)).await
    }

    /// W27-R08 (the Transfers plan's step 2): records kept once by digest, each hop checked as
    /// it's taken (a receipt before its hand-on waits for it), a chain walked by a record's digest
    /// or a thing's, a break recorded with the records around it, and a statement signed at an
    /// epoch its session left refused and recorded with its signer and envelope.
    #[test]
    fn w27_r08_records_kept_breaks_and_refusals_recorded() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let server = server_uri(&org);
            let (kiln, fedora, jupiter) = (Signer::new(&pki, &server)?, Signer::new(&pki, &FEDORA.replace(ORG, &org))?, Signer::new(&pki, &JUPITER.replace(ORG, &org))?);
            let (fedora_uri, jupiter_uri) = (FEDORA.replace(ORG, &org), JUPITER.replace(ORG, &org));
            let commit = thing("commit 4c1e2a", 'c');
            // The receipt comes first: it waits for the hand-on, and nothing breaks.
            let hand_on = record(Kind::HandedOn, &commit, Some(&fedora_uri), None, &[], "land", 1);
            let hand_on_digest = kiln.digest(&hand_on)?;
            let receipt = kiln_or(take(&store, &root, &org, &fedora.sign(&record(Kind::Received, &commit, None, Some(&server), &[&hand_on_digest], "legs", 1))?, "runner fedora-2", at).await?)?;
            assert_eq!(receipt.broke, None);
            let given = kiln_or(take(&store, &root, &org, &kiln.sign(&hand_on)?, "kiln", at).await?)?;
            assert_eq!((given.digest.as_str(), given.broke.as_deref(), given.again), (hand_on_digest.as_str(), None, false));
            assert_eq!(count(&store, "custody_breaks").await?, 0);
            // Taken again, it's kept once.
            assert!(kiln_or(take(&store, &root, &org, &kiln.sign(&hand_on)?, "kiln", at).await?)?.again);
            assert_eq!(count(&store, "custody").await?, 2);
            // The chain, walked from the receipt or from the commit's digest.
            for from in [receipt.digest.clone(), format!("sha256:{}", "c".repeat(64))] {
                let v = chain(&store, &root, &org, &from, at).await?.ok_or_else(|| anyhow::anyhow!("no chain from {from}"))?;
                assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
                let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
                assert_eq!(hops.iter().map(|h| h.get("custodian").cloned().unwrap_or_default()).collect::<Vec<_>>(), vec![json!(fedora_uri), json!(server)]);
                assert!(hops.iter().all(|h| h.get("signer").and_then(Value::as_str).is_some_and(|s| !s.is_empty())), "each hop names who signed it");
            }
            assert_eq!(chain(&store, &root, &org, "sha256:feed", at).await?, None);
            // An offer no one takes: open, the giver holding it; lapsed once its time has passed.
            let offered = kiln_or(take(&store, &root, &org, &fedora.sign(&record(Kind::HandedOn, &thing("image kiln:4c1e2a", 'e'), Some(&jupiter_uri), None, &[&receipt.digest], "legs", 2))?, "runner fedora-2", at).await?)?;
            let v = chain(&store, &root, &org, &offered.digest, at).await?.unwrap_or_default();
            assert_eq!((v.get("open"), v.get("lapsed"), v.get("held_by")), (Some(&json!(true)), Some(&json!(false)), Some(&json!(fedora_uri))), "{v}");
            let v = chain(&store, &root, &org, &offered.digest, at + LAPSE_AFTER + 1).await?.unwrap_or_default();
            assert_eq!((v.get("lapsed"), v.get("held_by")), (Some(&json!(true)), Some(&json!(fedora_uri))), "{v}");
            // jupiter takes what was handed to fedora-2: kept, the break recorded with the records
            // around it, and audited.
            let wrong = kiln_or(take(&store, &root, &org, &jupiter.sign(&record(Kind::Received, &commit, None, Some(&server), &[&hand_on_digest], "legs", 1))?, "runner jupiter", at).await?)?;
            assert!(wrong.broke.as_deref().is_some_and(|b| b.contains("not to") && b.contains(&jupiter_uri)), "{wrong:?}");
            let listed = breaks(&store, LIST_MAX).await?;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed.first().and_then(|b| b.get("kind")), Some(&json!("not-the-taker")));
            assert_eq!(listed.first().and_then(|b| b.get("around")), Some(&json!([wrong.digest, hand_on_digest])));
            let v = chain(&store, &root, &org, &wrong.digest, at).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(false)));
            assert_eq!(breaks(&store, LIST_MAX).await?.len(), 1, "the same break is recorded once");
            // The session's epoch 0 left (its certificate revoked as it went on): what its copy
            // signs after is refused, and recorded naming it.
            let left_uri = LEFT.replace(ORG, &org);
            let copy = Signer::new(&pki, &left_uri)?;
            crate::identities::record(&store, "agent", &left_uri, &copy.pem).await?;
            crate::identities::revoke(&store, crate::identities::Target::Subject(left_uri.clone()), "epoch 0 left: moved on to epoch 1, on runner adacachy (instance 01a14794)", "kiln", at - 10).await?;
            let stale = take(&store, &root, &org, &copy.sign(&record(Kind::HandedOn, &thing("goal met", 'd'), Some(&server), None, &[], "acts", 1))?, "runner fedora", at).await?;
            assert!(matches!(&stale, Err(Refused::Unsigned(why)) if why.contains("epoch 0 left")), "{stale:?}");
            let r = refusals(&store, LIST_MAX).await?;
            assert_eq!(r.len(), 1);
            let first = r.first().cloned().unwrap_or_default();
            assert_eq!((first.get("signer"), first.get("came_from")), (Some(&json!(left_uri)), Some(&json!("runner fedora"))));
            assert!(first.get("digest").and_then(Value::as_str).is_some_and(|d| d.starts_with("sha256:")));
            assert_eq!(count(&store, "custody").await?, 4, "a refused statement isn't kept as custody");
            let audited: i64 = store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM audit2 WHERE action IN ('custody.break', 'custody.refused')", [], |r| r.get(0))?)).await?;
            assert_eq!(audited, 2);
            // fedora-2's certificate revoked in the second kiln took its receipt: what kiln took
            // before stays taken, and its chain still holds.
            crate::identities::record(&store, "runner", &fedora_uri, &fedora.pem).await?;
            crate::identities::revoke(&store, crate::identities::Target::Subject(fedora_uri.clone()), "runner forgotten", "ada", at).await?;
            let v = chain(&store, &root, &org, &receipt.digest, at + 1).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            Ok(())
        })
    }

    /// W27-R08: both sides of a hand-over name a session's timeline alike from the same events,
    /// and differently once an event differs or the epoch does.
    #[test]
    fn w27_r08_a_timeline_is_named_alike_by_both_sides() -> anyhow::Result<()> {
        let events: Vec<crate::harness::model::Event> = serde_json::from_value(json!([]))?;
        let a = timeline("s1", 3, &events);
        assert_eq!(a, timeline("s1", 3, &events));
        assert_ne!(a.digest, timeline("s1", 4, &events).digest);
        assert_ne!(a.digest, timeline("s2", 3, &events).digest);
        assert_eq!(a.digest.get(TIMELINE).map(String::len), Some(64));
        assert_eq!(at_epoch("kiln://o/person/p/agent/s1/epoch/3/machine/m/instance/i", 4).as_deref(), Some("kiln://o/person/p/agent/s1/epoch/4"));
        assert_eq!(at_epoch("kiln://o/person/p/machine/m/instance/i", 4), None);
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 4, the 14:18 drop): a hand-over's word as kiln's server
    /// sends it is applied only by the process it's for, at the epoch it holds the session at: not
    /// by another instance (a new build it was passed on to), not at an epoch the process left,
    /// not changed on the way, and not unsigned from a server that signs words. From a server that
    /// doesn't, it goes as before; and a command that isn't a hand-over's word isn't checked.
    #[test]
    fn w27_r08_a_hand_overs_word_goes_only_to_its_process() -> anyhow::Result<()> {
        use crate::harness::model::Command;
        use crate::runner::custody::word_for_me;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            crate::identities::own_identities(&store, &pki).await?;
            let root = pki.ca_der.clone();
            let command = serde_json::to_string(&Command::HandedOver { error: None, unsure: false, by_newest: false, none_newer: false })?;
            let worded = word(&pki, "s1", &command, "fedora", "main", 3)?;
            let check = |json: &str, instance: &str, epoch: Option<u64>| word_for_me("s1", json, true, &root, "fedora", instance, epoch);
            assert_eq!(check(&worded, "main", Some(3)), Ok(()), "for this process at its epoch");
            assert_eq!(check(&worded, "main", None), Ok(()), "for this process, which let go of it");
            let passed = check(&worded, "01a17502", Some(4));
            assert!(passed.as_ref().is_err_and(|w| w.contains("for fedora (instance main) at epoch 3, not this process, fedora (instance 01a17502) at epoch 4")), "{passed:?}");
            assert!(check(&worded, "main", Some(4)).is_err(), "an epoch it left");
            let mut changed: Value = serde_json::from_str(&worded)?;
            if let Some(o) = changed.as_object_mut() {
                o.insert("error".into(), json!("let go of it"));
            }
            assert!(check(&changed.to_string(), "main", Some(3)).is_err_and(|w| w.contains("changed")));
            assert!(check(&command, "main", Some(3)).is_err_and(|w| w.contains("isn't signed")), "unsigned, from a server that signs words");
            assert_eq!(word_for_me("s1", &command, false, &root, "fedora", "main", Some(3)), Ok(()), "unsigned, from a server from before");
            assert_eq!(check(&serde_json::to_string(&Command::Interrupt)?, "01a17502", None), Ok(()), "not a hand-over's word");
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 5, the twin): once kiln has certified a session's holder
    /// at its epoch, an act counts only from that holder, signed at that epoch, whatever it is (a
    /// goal met, a message to an agent, a CI run, a page's answer, a run on another machine asked
    /// before it's done). Unsigned, it's refused from a runner process that signs and that kiln
    /// certified for the session (its holder, or a copy left behind), and goes by its machine from
    /// one kiln hasn't certified for it (it just took it over) or a runner from before; signed by
    /// the holder, it counts; signed by the copy left behind at the epoch it left, or by a former
    /// holder of the current epoch, or beside another act than its own, it's refused, saying why.
    /// Each refusal is recorded with who signed it, the epoch it's from and the transfer that
    /// superseded that epoch.
    #[test]
    fn w27_r08_an_act_counts_only_from_its_holder_now() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (org, at) = (hub.pki.organization(), crate::util::now_unix());
            let uri = |epoch: u64, machine: &str, instance: &str| format!("kiln://{org}/person/ada/agent/s1/epoch/{epoch}/machine/{machine}/instance/{instance}");
            let (left_uri, former_uri, holder_uri) = (uri(0, "fedora", "main"), uri(1, "jupiter", "01a14700"), uri(1, "adacachy", "01a14794"));
            let (left, former, holder) = (Signer::new(&hub.pki, &left_uri)?, Signer::new(&hub.pki, &former_uri)?, Signer::new(&hub.pki, &holder_uri)?);
            // Certified in turn: the copy left behind at epoch 0, then at epoch 1 a process that
            // held it before its holder now.
            for (u, s) in [(&left_uri, &left), (&former_uri, &former), (&holder_uri, &holder)] {
                crate::identities::record(&hub.store, "agent", u, &s.pem).await?;
            }
            crate::identities::revoke(&hub.store, crate::identities::Target::Subject(left_uri.clone()), "epoch 0 left: moved on to epoch 1, on runner adacachy (instance 01a14794)", "kiln", at - 10).await?;
            // kiln's record of the receipt its holder took it with.
            let h = holder_uri.clone();
            hub.store.call(move |c| Ok(c.execute("INSERT INTO custody (digest, payload_type, kind, custodian, envelope, taken_at) VALUES ('sha256:5e55', ?1, 'received', ?2, '{}', ?3)", params![PAYLOAD_CUSTODY, h, at])?)).await?;
            // An Open that says epoch 7 doesn't move the epoch acts are held to.
            hub.harness.raise_epoch("s1", 7);
            let asks = [
                json!({"op": "call", "session": "s1", "name": "goals.done", "input": {"id": "g1", "evidence": "built"}}),
                json!({"op": "call", "session": "s1", "name": "agents.send", "input": {"agent": "a2", "text": "the build is green"}}),
                json!({"op": "call", "session": "s1", "name": "ci.runs.trigger", "input": {"repo": "kiln"}}),
                json!({"op": "page", "session": "s1", "page": "p1", "do": "approve"}),
                json!({"op": "act", "session": "s1", "name": "run on jupiter: cargo build"}),
            ];
            let signed = |by: &Signer, epoch: u64, ask: &Value| -> anyhow::Result<Value> {
                use sha2::Digest as _;
                let hex: String = sha2::Sha256::digest(without(ask, "act").as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
                let body = serde_json::to_value(custody::Act { v: statement::VERSION, by: String::new(), session: "s1".into(), epoch, op: op_of(ask) })?;
                let subject = Subject { name: "an act of session s1".into(), digest: BTreeMap::from([("sha256".to_owned(), hex)]), ..Subject::default() };
                let p = by.payload(&Statement { subject: vec![subject], ..Statement::new(body) })?;
                let act = bundle(custody::PAYLOAD_ACT, &p, &by.key.sign(&to_sign(custody::PAYLOAD_ACT, &p))?, &by.der);
                let mut a = ask.clone();
                if let Some(o) = a.as_object_mut() {
                    o.insert("act".into(), Value::String(act.to_string()));
                }
                Ok(a)
            };
            let counts = |machine: &'static str, instance: &'static str, signs: bool, a: Value| {
                let hub = std::sync::Arc::clone(&hub);
                async move { act_counts(&hub, machine, instance, signs, "s1", &a, at).await }
            };
            for ask in &asks {
                let op = op_of(ask);
                assert!(counts("adacachy", "01a14794", true, ask.clone()).await?.is_err_and(|w| w.contains("isn't signed")), "{op}: unsigned, from the process that holds the key");
                assert_eq!(counts("adacachy", "z9", true, ask.clone()).await?, Ok(()), "{op}: unsigned, from a process kiln hasn't certified for it");
                assert_eq!(counts("adacachy", "01a14794", false, ask.clone()).await?, Ok(()), "{op}: unsigned, from a runner from before");
                assert_eq!(counts("adacachy", "01a14794", true, signed(&holder, 1, ask)?).await?, Ok(()), "{op}: signed by its holder at its epoch");
                let stale = counts("fedora", "main", true, signed(&left, 0, ask)?).await?;
                assert!(stale.as_ref().is_err_and(|w| w.contains("signed at epoch 0 of session s1, which went on at epoch 1, on runner adacachy (instance 01a14794)") && w.contains("(transfer sha256:5e55): it's from a copy left behind")), "{op}: {stale:?}");
                let unsigned_left = counts("fedora", "main", true, ask.clone()).await?;
                assert!(unsigned_left.as_ref().is_err_and(|w| w.contains("held session s1 at epoch 0, which went on at epoch 1") && w.ends_with("it's from a copy left behind")), "{op}: {unsigned_left:?}");
                let former_now = counts("jupiter", "01a14700", true, signed(&former, 1, ask)?).await?;
                assert!(former_now.as_ref().is_err_and(|w| w.contains("on runner jupiter (instance 01a14700), but the session is held, on runner adacachy (instance 01a14794)")), "{op}: {former_now:?}");
            }
            // Its holder's signature beside another act (its goal changed): refused.
            let mut changed = signed(&holder, 1, &json!({"op": "call", "session": "s1", "name": "goals.done", "input": {"id": "g1", "evidence": "built"}}))?;
            if let Some(id) = changed.pointer_mut("/input/id") {
                *id = json!("g2");
            }
            assert!(counts("adacachy", "01a14794", true, changed).await?.is_err_and(|w| w.contains("the act itself changed")));
            let r = refusals(&hub.store, LIST_MAX).await?;
            assert_eq!(r.len(), 4 * asks.len() + 1, "{r:?}");
            let from = |who: &str| -> Vec<&Value> { r.iter().filter(|x| x.get("signer") == Some(&json!(who))).collect() };
            let (stale, former_now) = (from(&left_uri), from(&former_uri));
            assert!(stale.len() == asks.len() && stale.iter().all(|x| x.get("epoch") == Some(&json!(0)) && x.get("superseded_by") == Some(&json!("sha256:5e55")) && x.get("came_from") == Some(&json!("runner fedora (instance main)"))), "{stale:?}");
            assert!(former_now.len() == asks.len() && former_now.iter().all(|x| x.get("epoch") == Some(&json!(1)) && x.get("superseded_by") == Some(&json!(""))), "{former_now:?}");
            let unsigned_left: Vec<&Value> = r.iter().filter(|x| x.get("signer") == Some(&json!("")) && x.get("epoch") == Some(&json!(0))).collect();
            assert!(unsigned_left.len() == asks.len() && unsigned_left.iter().all(|x| x.get("superseded_by") == Some(&json!("sha256:5e55"))), "{unsigned_left:?}");
            Ok(())
        })
    }

    /// W27-R08 (the Transfers plan's step 6): work changes hands with both sides signed. kiln, a
    /// plan run's driver, hands a step to its agent, acting for the person; the agent signs its
    /// receipt. Tried again, kiln takes it for the earlier agent (following that one's receipt) and
    /// hands it to the next, which signs; kiln knows the earlier agent no longer holds it. Taken over
    /// by a person with no key on the device that asked, kiln signs the receipt for them, and the
    /// walk reads that link as kiln's for them; with a key, the receipt is theirs, and one signed by
    /// another is refused.
    #[test]
    fn w27_r08_work_changes_hands_signed_by_both_sides() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            // The server's own signing key, as a server's start certifies it.
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let (org, at) = (hub.pki.organization(), crate::util::now_unix());
            let (root, server) = (hub.pki.ca_der.clone(), server_uri(&org));
            let (ada, sam) = (person_uri(&org, "ada"), person_uri(&org, "sam"));
            let agent = |id: &str| format!("kiln://{org}/person/ada/agent/{id}/epoch/0");
            let held = |id: &str| format!("{}/machine/adacachy/instance/01a14794", agent(id));
            let (first, second) = (Signer::new(&hub.pki, &held("a1"))?, Signer::new(&hub.pki, &held("a2"))?);
            let step = work("plan run r1 step 2", &["r1", "2", "the build is green"]);
            let receipt = |by: &Signer, from: &str, hand_on: &str| -> anyhow::Result<Value> {
                let b = Body { from: Some(from.to_owned()), why: "step 2 of plan run r1".into(), ..Body::new(Kind::Received, "") };
                by.sign(&Statement { subject: vec![step.clone()], follows: vec![hand_on.to_owned()], ..Statement::new(to_value(&b)) })
            };
            let walked = |digest: String| {
                let (store, root, org) = (hub.store.clone(), root.clone(), org.clone());
                async move { chain(&store, &root, &org, &digest, at).await?.ok_or_else(|| anyhow::anyhow!("no chain")) }
            };
            // The first try: kiln's, for ada; the agent's receipt.
            let (handed, _) = hand_work(&hub, step.clone(), &agent("a1"), "step 2 of plan run r1", Giver::Kiln { acts_for: vec![ada.clone()] }).await?;
            let got = kiln_or(take(&hub.store, &root, &org, &receipt(&first, &server, &handed)?, "runner adacachy", at).await?)?;
            assert_eq!(walked(got.digest.clone()).await?.get("unbroken"), Some(&json!(true)));
            // Tried again: kiln's take for the first agent, following its receipt; the next's receipt.
            let (taken, _) = hand_work(&hub, step.clone(), &agent("a2"), "step 2 of plan run r1, tried again", Giver::For(held("a1"))).await?;
            let again = kiln_or(take(&hub.store, &root, &org, &receipt(&second, &held("a1"), &taken)?, "runner adacachy", at).await?)?;
            let v = walked(again.digest.clone()).await?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let custodians: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("custodian").and_then(Value::as_str)).collect();
            assert_eq!(custodians, vec![held("a2").as_str(), server.as_str(), held("a1").as_str(), server.as_str()], "{v}");
            assert_eq!(work_taken_from(&hub.store, "a1").await?, Some((taken.clone(), step.name.clone(), agent("a2"))));
            assert_eq!(work_taken_from(&hub.store, "a2").await?, None);
            // Taken over by ada, with no key on the device that asked: kiln's receipt for her.
            let (to_ada, _) = hand_work(&hub, step.clone(), &ada, "step 2 of plan run r1, taken over", Giver::For(held("a2"))).await?;
            let for_ada = receive_for(&hub, &ada, step.clone(), &to_ada, &held("a2"), "step 2 of plan run r1, taken over").await?;
            let v = walked(for_ada).await?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let first_hop = v.get("hops").and_then(Value::as_array).and_then(|h| h.first()).cloned().unwrap_or_default();
            assert_eq!((first_hop.get("custodian"), first_hop.get("taken_for")), (Some(&json!(server)), Some(&json!(ada))), "kiln for ada, never ada: {first_hop}");
            assert!(first_hop.get("why").and_then(Value::as_str).is_some_and(|w| w.contains("no key on the device that asked")), "{first_hop}");
            // Taken over again, by ada with a key: her own receipt; one signed by sam isn't hers.
            let (to_ada_again, _) = hand_work(&hub, step.clone(), &ada, "step 2 of plan run r1, taken over again", Giver::For(held("a2"))).await?;
            let (ada_key, sam_key) = (Signer::new(&hub.pki, &ada)?, Signer::new(&hub.pki, &sam)?);
            let not_hers = person_receipt(&hub, &ada, &to_ada_again, &receipt(&sam_key, &held("a2"), &to_ada_again)?, "api:sam").await?;
            assert!(not_hers.as_ref().is_err_and(|w| w.contains("not by")), "{not_hers:?}");
            let hers = person_receipt(&hub, &ada, &to_ada_again, &receipt(&ada_key, &held("a2"), &to_ada_again)?, "api:ada").await?.map_err(|w| anyhow::anyhow!("{w}"))?;
            assert_eq!(walked(hers).await?.get("unbroken"), Some(&json!(true)));
            assert_eq!(handed_to(&hub.store, &ada, &step).await?, Some(to_ada_again));
            Ok(())
        })
    }

    /// A taken statement, or the refusal as an error.
    fn kiln_or(r: Result<Taken, Refused>) -> anyhow::Result<Taken> {
        r.map_err(|e| anyhow::anyhow!("refused: {e}"))
    }

    /// W27-R08 (the Transfers plan's step 3): a session's hand-over, as its process at the next
    /// epoch asks for its certificate. Its hand-on and its receipt, signed with the key being
    /// certified before kiln certified it, are kept, and walk as one transfer; asked again, the
    /// same. Not certified: a hand-on to another epoch, one without its receipt, and a copy left
    /// behind's (its epoch revoked), which is refused and recorded. A receipt of other than what
    /// was handed on is kept, its break recorded: the session goes on where it's held.
    #[test]
    fn w27_r08_a_transfer_is_kept_before_its_epoch_rises() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (root, org) = (pki.ca_der.clone(), pki.organization());
            let at = crate::util::now_unix();
            let left_uri = LEFT.replace(ORG, &org);
            let left = Signer::new(&pki, &left_uri)?;
            let (next, issued) = (format!("kiln://{org}/person/ada/agent/01a1751c/epoch/1"), format!("kiln://{org}/person/ada/agent/01a1751c/epoch/1/machine/adacachy/instance/01a14794"));
            // The receiver's key, its receipt signed with it before kiln certifies it.
            let key = crate::pki::new_key_pem()?;
            let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
            let pem = pki.issue_identity(&csr, &Identity::parse(&issued)?, &[], time::Duration::hours(1))?;
            let signing = rcgen::KeyPair::from_pem(&key)?;
            let timeline = thing("session 01a1751c at epoch 0", 'e');
            let hand_on = |to: &str, epoch: u64, what: &Subject| -> anyhow::Result<Value> {
                let b = Body { to: Some(to.to_owned()), why: "taken over".into(), epoch: Some(epoch), ..Body::new(Kind::HandedOn, "") };
                left.sign(&Statement { subject: vec![what.clone()], ..Statement::new(to_value(&b)) })
            };
            let receipt = |follows: &str, what: &Subject| -> anyhow::Result<crate::proto::Unbundled> {
                let b = Body { from: Some(left_uri.clone()), why: "taken over".into(), epoch: Some(1), ..Body::new(Kind::Received, &next) };
                let p = payload(&Statement { subject: vec![what.clone()], follows: vec![follows.to_owned()], ..Statement::new(to_value(&b)) })?;
                let b64 = base64::engine::general_purpose::STANDARD;
                Ok(crate::proto::Unbundled { payload_type: PAYLOAD_CUSTODY.into(), payload: b64.encode(&p), signature: b64.encode(signing.sign(&to_sign(PAYLOAD_CUSTODY, &p))?) })
            };
            let transfer = |h: Value, r: Option<crate::proto::Unbundled>| {
                let (store, root, org, issued, pem) = (store.clone(), root.clone(), org.clone(), issued.clone(), pem.clone());
                async move { super::transfer(&store, &root, &org, "runner adacachy (instance 01a14794)", &issued, 1, &h.to_string(), r.as_ref(), &pem, at).await }
            };
            let handed = hand_on(&next, 1, &timeline)?;
            let handed_digest = digest_of(&handed);
            let got = transfer(handed.clone(), Some(receipt(&handed_digest, &timeline)?)).await?.map_err(|why| anyhow::anyhow!("{why}"))?;
            let v = chain(&store, &root, &org, &got, at).await?.ok_or_else(|| anyhow::anyhow!("no chain"))?;
            assert_eq!(v.get("unbroken"), Some(&json!(true)), "{v}");
            let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
            let custodians: Vec<&str> = hops.iter().filter_map(|h| h.get("custodian").and_then(Value::as_str)).collect();
            assert_eq!(custodians, vec![issued.as_str(), left_uri.as_str()], "{v}");
            // Asked again (its answer lost): the same transfer, kept once.
            assert_eq!(transfer(handed.clone(), Some(receipt(&handed_digest, &timeline)?)).await?, Ok(got.clone()));
            assert_eq!(count(&store, "custody").await?, 2);
            // A hand-on to another epoch, or without its receipt: no certificate.
            let other = hand_on(&format!("kiln://{org}/person/ada/agent/01a1751c/epoch/2"), 2, &thing("session 01a1751c at epoch 0", 'f'))?;
            assert!(transfer(other, None).await?.is_err_and(|why| why.contains("not to")));
            let lone = hand_on(&next, 1, &thing("session 01a1751c at epoch 0", '7'))?;
            assert!(transfer(lone, None).await?.is_err_and(|why| why.contains("no receipt")));
            // A receipt of other than what was handed on: kept, its break recorded.
            let more = hand_on(&next, 1, &thing("session 01a1751c at epoch 0", '8'))?;
            let more_digest = digest_of(&more);
            assert!(transfer(more, Some(receipt(&more_digest, &thing("session 01a1751c at epoch 0", '9'))?)).await?.is_ok());
            assert_eq!(breaks(&store, LIST_MAX).await?.len(), 1);
            // The copy left behind, its epoch revoked as it moved on, hands it on again: refused,
            // recorded, never certified.
            crate::identities::record(&store, "agent", &left_uri, &left.pem).await?;
            crate::identities::revoke(&store, crate::identities::Target::Subject(left_uri.clone()), "epoch 0 left: moved on to epoch 1, on runner adacachy (instance 01a14794)", "kiln", at - 10).await?;
            let stale = hand_on(&next, 1, &thing("session 01a1751c at epoch 0", 'a'))?;
            let stale_digest = digest_of(&stale);
            assert!(transfer(stale, Some(receipt(&stale_digest, &thing("session 01a1751c at epoch 0", 'a'))?)).await?.is_err_and(|why| why.contains("refused") && why.contains("epoch 0 left")));
            let r = refusals(&store, LIST_MAX).await?;
            assert!(r.iter().any(|x| x.get("signer") == Some(&json!(left_uri))), "{r:?}");
            Ok(())
        })
    }
}
