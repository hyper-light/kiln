//! The relays' records, kept and checked (docs/ci.md W27-R08; the Transfers plan's step 13, kiln
//! plan `01a17856b1b477eda14a574ae312a123`). Every intermediary records what it passes on, by
//! digest, numbered on its stream, and whoever it reaches checks that record as it takes the thing
//! (`kiln_ci::identity::relay`):
//!
//! - **A runner passing its agents' calls on to kiln.** Each act goes with the runner's record of
//!   passing it on (`passed_on`, beside the ask), numbered on the runner's stream for that
//!   connection. kiln takes the numbers in the order the runner's messages come
//!   ([`Relays::placed`]), then checks the record against the act ([`call`]). The act, the record
//!   and kiln's receipt following it are kept. A record that's forged, of another act or of an
//!   altered one, or numbered before, refuses the call, and the break is recorded naming the
//!   runner. A number skipped is recorded the same way, though the call itself is taken.
//! - **kiln passing an agent's words on to another agent's runner.** The sender's act and each
//!   relay's record of it so far go with the message, kiln's own record numbered on its stream to
//!   that runner process ([`words`]). The recipient's runner checks the act end to end and every
//!   record, and signs its receipt (`crate::runner::custody`).
//! - **kiln passing reach links' frames on.** It can't read them, so it records each run of a
//!   link's frames by the chain of their ciphertext's digests (`crate::reach::relay`, [`frames`]).
//!   The runner at the link's other end chains what it got and signs its receipt of that.
//! - **Blue/green's old build passing kiln's frames on to the new one.** The old build's records
//!   come with its others (`AgentMsg::Custody`); the new build checks each and signs its receipt.
//! - **Breaks.** What a receiver reports finding ([`reported`]), and a relay's record no receipt
//!   answered after [`crate::custody::LAPSE_AFTER`] ([`unanswered`]: dropped, or lost on the way),
//!   are recorded naming the relay.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use kiln_ci::identity::relay::{self, Numbering, Pass, Place, Unrelayed};
use kiln_ci::identity::statement;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::instances::PeerId;
use crate::store::Store;

/// When this server started (unix ms): its own streams are named for it, so a server started
/// again numbers new ones from 1.
static STARTED: LazyLock<i64> = LazyLock::new(crate::util::now_ms);

/// The most calls carried at once from a call to the words it sends ([`Relays::carry`]).
const CARRIED_MAX: usize = 1024;

/// What a stream of kiln's records says, when the runner it passes things on to is from before
/// relays were checked (no `CAP_RELAYS`): it signs no receipt, so none is waited for ([`unanswered`]).
const UNANSWERED: &str = " (unanswered: a runner from before)";

/// Whether runner process `peer` checks what relays pass on to it, and signs its receipts
/// (`CAP_RELAYS`).
fn answers(hub: &Hub, peer: PeerId) -> bool {
    hub.instances.load().get(peer).is_some_and(|i| (u64::from(i.caps) | i.wide) & crate::proto::CAP_RELAYS != 0)
}

/// kiln's own state of the relays it takes from and is.
#[derive(Default)]
pub struct Relays {
    /// Each runner connection's streams of records of the calls it passes on, as kiln read them, in
    /// the order they came, keyed by the connection.
    calls: Mutex<Numbering>,
    /// The connections whose runner recorded a call it passed on: from then on, each act it passes
    /// on goes with its record.
    recording: Mutex<HashSet<PeerId>>,
    /// Agents' calls to send words, taken and checked, carried from the call to the words it sends:
    /// by a number of its own.
    carried: Mutex<HashMap<u64, Carried>>,
    next_carried: Mutex<u64>,
    /// kiln's own streams, the next number on each.
    streams: Mutex<HashMap<String, u64>>,
    /// The two acts kiln passed on as words last (each with the ask it signs and the words): what a
    /// test build's faulty relay swaps in (`relay_words=swap`).
    passed_words: Mutex<std::collections::VecDeque<(String, String, String)>>,
}

/// A call's record of being passed on, as kiln read it from the runner's connection.
pub enum Placed {
    /// The record (a bundle), and where its number stands on its stream, taken in the order the
    /// runner's messages came.
    Record { bundle: Value, place: Result<Place, Unrelayed> },
    /// An act with no record, from a runner that records the calls it passes on.
    Unrecorded,
}

/// An agent's call to send words, checked and kept: its act, the ask it signs, and each relay's
/// record of it so far, carried to the words it sends ([`words`]).
#[derive(Clone, Debug)]
pub struct Carried {
    pub session: String,
    pub text: String,
    pub act: Value,
    pub ask: String,
    pub records: Vec<Value>,
}

impl Relays {
    /// Where call `ask`'s record stands, read as runner connection `peer`'s messages are read, in
    /// their order. None for an ask with no record from a runner that never recorded one (a runner
    /// from before relays recorded, an ask that isn't an act).
    pub fn placed(&self, peer: PeerId, ask: &Value) -> Option<Placed> {
        let Some(bundle) = ask.get("passed_on").and_then(Value::as_str).and_then(|b| serde_json::from_str::<Value>(b).ok()) else {
            let recording = self.recording.lock().contains(&peer);
            return (recording && ask.get("act").is_some()).then_some(Placed::Unrecorded);
        };
        self.recording.lock().insert(peer);
        let numbered = crate::custody::statement_in(&bundle.to_string()).and_then(|s| Some((s.stream?, s.seq?)));
        let place = match numbered {
            Some((stream, seq)) => self.calls.lock().take(&format!("connection {peer}"), &stream, seq),
            None => Err(Unrelayed::Unnumbered { relay: format!("connection {peer}") }),
        };
        Some(Placed::Record { bundle, place })
    }

    /// Runner connection `peer` went: it records again only once it says so.
    pub fn lost(&self, peer: PeerId) {
        self.recording.lock().remove(&peer);
    }

    /// Carries `c` to the words its call sends: the number to take it by.
    pub fn carry(&self, c: Carried) -> u64 {
        let n = {
            let mut next = self.next_carried.lock();
            *next = next.wrapping_add(1);
            *next
        };
        let mut carried = self.carried.lock();
        if carried.len() >= CARRIED_MAX
            && let Some(oldest) = carried.keys().min().copied()
        {
            carried.remove(&oldest);
        }
        carried.insert(n, c);
        n
    }

    /// What session `session` carries for words `text` (its call's, taken once), if any.
    pub fn carried(&self, session: &str, text: &str) -> Option<Carried> {
        let mut carried = self.carried.lock();
        let n = carried.iter().filter(|(_, c)| c.session == session && c.text == text).map(|(n, _)| *n).min()?;
        carried.remove(&n)
    }

    /// Done with what [`Relays::carry`] carried as `n`, taken or not.
    pub fn uncarry(&self, n: u64) {
        self.carried.lock().remove(&n);
    }

    /// `relayed` (what goes with words kiln passes on: [`words`]) with the act and ask of the words
    /// it passed on before in place of its own, its records unchanged, and those words: what a test
    /// build's faulty relay sends (`relay_words=swap`). None when there were none before.
    pub fn swapped(&self, relayed: &Value) -> Option<(Value, String)> {
        let own = relayed.get("act").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (act, ask, words) = self.passed_words.lock().iter().find(|(act, ..)| *act != own).cloned()?;
        let mut v = relayed.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert("act".into(), Value::String(act));
            o.insert("ask".into(), Value::String(ask));
        }
        Some((v, words))
    }

    /// The next number on kiln's own stream `stream`.
    fn next(&self, stream: &str) -> u64 {
        let mut streams = self.streams.lock();
        let n = streams.entry(stream.to_owned()).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }
}

/// What ask `ask` is: "call goals.done", "act run", "page".
fn op_of(ask: &Value) -> String {
    match (ask.get("op").and_then(Value::as_str), ask.get("name").and_then(Value::as_str)) {
        (Some(op), Some(name)) => format!("{op} {name}"),
        (Some(op), None) => op.to_owned(),
        _ => "an ask".to_owned(),
    }
}

/// What a relay's record is about: an agent session's act, by its statement's digest.
pub fn act_thing(session: &str, op: &str, act_digest: &str) -> statement::Subject {
    relay::statement_thing(&format!("an act of session {session}: {op}"), act_digest)
}

/// A relay's record found not to hold: recorded as a break naming the relay, with what was refused
/// because of it, and the refusal itself when something was.
struct Found<'a> {
    hub: &'a Hub,
    /// The relay's record (its digest; its bundle, kept as the refusal's envelope).
    record: String,
    envelope: String,
    relay: String,
    /// What it was about: what's refused, or taken despite a gap.
    what: String,
    came_from: String,
    around: Vec<String>,
    at: i64,
}

impl Found<'_> {
    /// Records `u`: a break, and the refusal when `refused`. Its words.
    async fn record(&self, u: &Unrelayed, refused: bool) -> anyhow::Result<String> {
        let why = u.to_string();
        let what = if refused { self.what.clone() } else { String::new() };
        crate::custody::relay_break(&self.hub.store, &self.record, u.code(), &why, &self.around, &what, &self.relay, self.at).await?;
        if refused {
            crate::custody::refused_relayed(&self.hub.store, &self.record, &self.what, &self.relay, &why, &self.envelope, &self.came_from, self.at).await?;
        }
        Ok(why)
    }
}

/// Checks call `ask` of agent session `session`, from runner `machine`'s process `instance`, by its
/// record as [`Relays::placed`] placed it (docs/ci.md W27-R08's relays): the runner's record of
/// passing the act on, signed by that process, to kiln, of the very act that came (by its digest:
/// one altered on the way differs, another of the session's, swapped in, is another), from the
/// act's signer, numbered next on its stream. Then the act and the record are kept and kiln's
/// receipt follows. A record that doesn't hold, or a number seen before, refuses the call, and the
/// break is recorded naming the runner; a number skipped is recorded so, the call taken. Whether
/// the call goes on, or why not; for words an agent sends, what's carried to them.
pub async fn call(hub: &Hub, machine: &str, instance: &str, session: &str, ask: &Value, placed: Placed, at: i64) -> anyhow::Result<Result<Option<Carried>, String>> {
    let org = hub.pki.organization();
    let server = crate::custody::server_uri(&org);
    let relay = crate::identities::runner_identity(hub, machine, instance).await?.uri()?;
    let came_from = format!("runner {machine} (instance {instance})");
    let what = format!("an act of session {session}: {}", op_of(ask));
    let act = ask.get("act").and_then(Value::as_str).and_then(|a| serde_json::from_str::<Value>(a).ok());
    let act_digest = act.as_ref().map(crate::custody::digest_of).unwrap_or_default();
    let mut found = Found { hub, record: act_digest.clone(), envelope: String::new(), relay: relay.clone(), what: what.clone(), came_from: came_from.clone(), around: vec![act_digest.clone()], at };
    let (bundle, place) = match placed {
        Placed::Unrecorded => {
            let why = found.record(&Unrelayed::Unreadable { relay: relay.clone(), why: "it records the calls it passes on, and passed this act on with no record of it".into() }, true).await?;
            return Ok(Err(why));
        }
        Placed::Record { bundle, place } => (bundle, place),
    };
    (found.record, found.envelope) = (crate::custody::digest_of(&bundle), bundle.to_string());
    found.around = vec![found.record.clone(), act_digest.clone()];
    let Some(act) = act else {
        let why = found.record(&Unrelayed::Unreadable { relay: relay.clone(), why: "it records passing an act on, and no act came".into() }, true).await?;
        return Ok(Err(why));
    };
    let revocations = crate::identities::revocations(&hub.store).await?;
    let record = match statement::verify(&bundle, &hub.pki.ca_der, &revocations, at) {
        Ok(c) => c,
        Err(e) => {
            let why = found.record(&Unrelayed::Unreadable { relay: relay.clone(), why: e.to_string() }, true).await?;
            return Ok(Err(why));
        }
    };
    // The act as it came, signed by its originator or not: another of its signer's, swapped in, is
    // genuine; one altered on the way isn't.
    let signed = statement::verify(&act, &hub.pki.ca_der, &[], at).ok();
    let thing = act_thing(session, &op_of(ask), &act_digest);
    // What the act signs: the ask that came, unchanged (an ask changed on the way isn't the one its
    // agent signed, though the act itself holds).
    let asked: String = <sha2::Sha256 as sha2::Digest>::digest(crate::custody::signed_part(ask).as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let checked = relay::relayed(&record, &relay, &server, &thing, signed.is_some()).and_then(|b| {
        let from = b.from.unwrap_or_default();
        match &signed {
            Some(s) if !s.statement.subject.iter().any(|t| t.digest.get("sha256").is_some_and(|d| d.eq_ignore_ascii_case(&asked))) => {
                let was = s.statement.subject.first().and_then(|t| t.digest.get("sha256").cloned()).unwrap_or_default();
                Err(Unrelayed::Changed { relay: relay.clone(), thing: format!("the ask its act signs ({what})"), algorithm: "sha256".into(), was, now: asked.clone() })
            }
            Some(s) if kiln_ci::identity::custody::names(&from, &s.custodian) => Ok(()),
            Some(s) => Err(Unrelayed::Unreadable { relay: relay.clone(), why: format!("it says the act came from {from}, but {} signed it", s.custodian) }),
            None => Err(Unrelayed::Changed { relay: relay.clone(), thing: thing.name.clone(), algorithm: "signature".into(), was: "its signer's".into(), now: "no one's".into() }),
        }
    });
    if let Err(u) = checked {
        let why = found.record(&u, true).await?;
        return Ok(Err(why));
    }
    match place {
        Ok(Place::Next) => {}
        Ok(Place::AfterGap(gap)) => {
            found.record(&named(gap, &relay), false).await?;
        }
        Err(u) => {
            let why = found.record(&named(u, &relay), true).await?;
            return Ok(Err(why));
        }
    }
    crate::custody::keep_originator(&hub.store, &hub.pki.ca_der, &act, &came_from, at).await?;
    match crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &bundle, &came_from, at).await? {
        Ok(t) if t.broke.is_none() => {}
        Ok(t) => return Ok(Err(format!("{relay}'s record of passing it on doesn't hold: {}", t.broke.unwrap_or_default()))),
        Err(why) => return Ok(Err(format!("{relay}'s record of passing it on was refused: {why}"))),
    }
    let receipt = relay::receipt(&server, &relay, vec![thing], &record.digest, "an agent's call, taken by kiln");
    let signed_receipt = crate::custody::server_signed(&hub.pki, &receipt)?;
    if let Ok(Err(why)) = crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &signed_receipt, "kiln's server", at).await {
        tracing::warn!(target: "kiln.custody", %session, %why, "kiln's receipt of a relayed call wasn't kept");
    }
    let carried = (ask.get("name").and_then(Value::as_str) == Some("agents.send")).then(|| Carried {
        session: session.to_owned(),
        text: ask.pointer("/input/text").and_then(Value::as_str).unwrap_or_default().to_owned(),
        ask: crate::custody::signed_part(ask),
        act: act.clone(),
        records: vec![bundle.clone()],
    });
    Ok(Ok(carried))
}

/// `u`, a numbering's, naming the relay by its identity rather than its connection.
fn named(u: Unrelayed, relay: &str) -> Unrelayed {
    match u {
        Unrelayed::Dropped { stream, from, to, .. } => Unrelayed::Dropped { relay: relay.to_owned(), stream, from, to },
        Unrelayed::Again { stream, seq, .. } => Unrelayed::Again { relay: relay.to_owned(), stream, seq },
        Unrelayed::Unnumbered { .. } => Unrelayed::Unnumbered { relay: relay.to_owned() },
        other => other,
    }
}

/// kiln passes agent `carried.session`'s words on to the runner process holding session `to`, the
/// agent they're for (docs/ci.md W27-R08's relays): its record (a passed-on of the sender's act,
/// following the record of the relay before it, from that relay, to that process), numbered on
/// kiln's stream to it, kept; and what goes with the message, for the recipient to check end to
/// end (`relayed`: the act, the ask it signs, each relay's record), its number on that stream and
/// its digest. A process from before relays were checked (no `CAP_RELAYS`) is passed them as
/// before, kiln's record on a stream saying it's unanswered: None then, and when that process isn't
/// connected.
pub async fn words(hub: &Hub, to: &str, carried: Carried) -> Option<(Value, u64, String)> {
    let Some(crate::harness::registry::Location::Machine(machine)) = hub.harness.location(to) else { return None };
    let held = {
        let inst = hub.instances.load();
        inst.reaches(&machine, &crate::instances::Owned::Agent(to.to_owned())).and_then(|p| inst.get(p).map(|i| (i.label(), p)))
    };
    let (instance, peer) = held?;
    let answers = answers(hub, peer);
    let run = || async {
        let org = hub.pki.organization();
        let server = crate::custody::server_uri(&org);
        let to_uri = crate::identities::runner_identity(hub, &machine, &instance).await?.uri()?;
        let last = carried.records.last().ok_or_else(|| anyhow::anyhow!("no relay's record came with it"))?;
        let from = crate::custody::statement_in(&last.to_string()).and_then(|s| s.body.get("by").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
        let op = "call agents.send";
        let mut thing = act_thing(&carried.session, op, &crate::custody::digest_of(&carried.act));
        // For whom: the session kiln found the words are for, as its identity names it. An
        // annotation, not the record's `for`: kiln passes them on, it doesn't hold them for it.
        let recipient = crate::identities::agent_uri(&hub.store, to).await?.unwrap_or_else(|| to.to_owned());
        thing.annotations.insert("for".into(), Value::String(recipient));
        let stream = format!("words to {to_uri}, from kiln started {}{}", *STARTED, if answers { "" } else { UNANSWERED });
        let pass = Pass { by: &server, from: &from, to: &to_uri, stream: &stream, seq: hub.relays.next(&stream), why: "an agent's words, passed on to the runner process holding the agent they're for" };
        let s = relay::passed_on(&pass, vec![thing], vec![crate::custody::digest_of(last)]);
        let bundle = crate::custody::server_signed(&hub.pki, &s)?;
        match crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
            Ok(_) => {}
            Err(why) => anyhow::bail!("kiln refused its own record of passing words on: {why}"),
        }
        let mut records: Vec<String> = carried.records.iter().map(Value::to_string).collect();
        records.push(bundle.to_string());
        {
            let mut passed = hub.relays.passed_words.lock();
            passed.push_back((carried.act.to_string(), carried.ask.clone(), carried.text.clone()));
            while passed.len() > 2 {
                passed.pop_front();
            }
        }
        anyhow::Ok((json!({ "act": carried.act.to_string(), "ask": carried.ask, "passed_on": records }), pass.seq, crate::custody::digest_of(&bundle)))
    };
    match run().await {
        Ok(v) if answers => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(target: "kiln.custody", session = %carried.session, %to, error = %format!("{e:#}"), "an agent's words went on with no record of kiln passing them on");
            None
        }
    }
}

/// kiln's records of the runs of reach links' frames its relay passed on (docs/ci.md W27-R08's
/// relays; `crate::reach::relay`): each a passed-on of the run, by the chain of its frames'
/// ciphertext digests (kiln can't read them), from the runner process that sent them to the one it
/// passed them to, numbered on kiln's stream to that process for that link; kept, and sent to that
/// process, which answers with its receipt of the frames as they came. One from before relays were
/// checked (no `CAP_RELAYS`) is sent none, and kiln's record says no receipt is waited for.
pub async fn frames(hub: &Hub, runs: Vec<crate::reach::relay::Passed>) {
    let org = hub.pki.organization();
    let server = crate::custody::server_uri(&org);
    for p in runs {
        let of = |peer: PeerId| hub.instances.load().get(peer).map(|i| i.label());
        let (Some(from_instance), Some(to_instance)) = (of(p.from_peer), of(p.to_peer)) else { continue };
        let answers = answers(hub, p.to_peer);
        let run = || async {
            let from = crate::identities::runner_identity(hub, &p.from, &from_instance).await?.uri()?;
            let to = crate::identities::runner_identity(hub, &p.to, &to_instance).await?.uri()?;
            let last = p.frames.next().saturating_sub(1);
            let thing = p.frames.thing(&format!("frames {} to {last} of reach link {} from {}", p.frames.first(), p.link, p.from));
            let stream = format!("reach link {} to {to}, from kiln started {}{}", p.link, *STARTED, if answers { "" } else { UNANSWERED });
            let pass = Pass { by: &server, from: &from, to: &to, stream: &stream, seq: p.seq, why: "a reach link's frames, passed on unread by kiln's relay" };
            let bundle = crate::custody::server_signed(&hub.pki, &relay::passed_on(&pass, vec![thing], Vec::new()))?;
            if let Err(why) = crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", crate::util::now_unix()).await? {
                anyhow::bail!("kiln refused its own record of passing a reach link's frames on: {why}");
            }
            anyhow::Ok(bundle)
        };
        match run().await {
            Ok(bundle) if answers => {
                let json = json!({ "reach": { "link": p.link, "from": p.from, "record": bundle.to_string() } });
                hub.send_peer(p.to_peer, &crate::proto::ServerMsg::Custody { json: json.to_string() }).await;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(target: "kiln.custody", link = %p.link, error = %format!("{e:#}"), "a run of a reach link's frames went unrecorded"),
        }
    }
}

/// kiln couldn't deliver the words it recorded passing on (`record`, its record's digest), for
/// `why` (the runner away, the session gone): a hand-on no receipt will follow, recorded at once as a
/// break naming kiln's server, with why.
pub async fn undelivered(hub: &Hub, record: &str, why: &str) {
    let server = crate::custody::server_uri(&hub.pki.organization());
    let detail = format!("kiln recorded passing an agent's words on, and couldn't deliver them: {why}");
    if let Err(e) = crate::custody::relay_break(&hub.store, record, "undelivered", &detail, &[record.to_owned()], "", &server, crate::util::now_unix()).await {
        tracing::warn!(target: "kiln.custody", %record, error = %format!("{e:#}"), "words kiln couldn't deliver weren't recorded so");
    }
}

/// A break a runner process found in what a relay passed on to it, and what it refused because of
/// it (`relay_break`, `AgentMsg::Custody`): recorded naming the relay, the relay's record kept as
/// it came (its envelope), as runner `machine`'s process `instance` reports it.
pub async fn reported(hub: &Hub, machine: &str, instance: &str, r: &Value) -> anyhow::Result<()> {
    let text = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let record = r.get("record").and_then(Value::as_str).and_then(|b| serde_json::from_str::<Value>(b).ok());
    let digest = record.as_ref().map(crate::custody::digest_of).unwrap_or_default();
    let came_from = format!("runner {machine} (instance {instance})");
    let (code, detail, refused, relay) = (text("code"), format!("{} (as {came_from} found it)", text("detail")), text("refused"), text("relay"));
    let at = crate::util::now_unix();
    crate::custody::relay_break(&hub.store, &digest, &code, &detail, std::slice::from_ref(&digest), &refused, &relay, at).await?;
    if !refused.is_empty() {
        crate::custody::refused_relayed(&hub.store, &digest, &refused, &relay, &detail, &record.map(|b| b.to_string()).unwrap_or_default(), &came_from, at).await?;
    }
    Ok(())
}

/// kiln's records of what reached it from a relay (or of kiln's own relaying) that no receipt
/// answered `lapse` seconds after kiln took them: each a hand-on with no receipt downstream
/// (dropped on the way, or lost), and, when `record`, recorded as a break naming its relay, once.
/// Each, newest first. Not where its receiver is from before relays were checked, whose streams say
/// so ([`UNANSWERED`]): it never answers; nor one already recorded as a break (refused where it
/// came, or never delivered).
pub async fn unanswered(store: &Store, lapse: i64, at: i64, record: bool) -> anyhow::Result<Vec<Value>> {
    let before = at.saturating_sub(lapse);
    let open: Vec<(String, String, String, Option<i64>)> = store
        .call(move |c| {
            let mut q = c.prepare(
                "SELECT digest, custodian, stream, seq FROM custody WHERE kind = 'passed-on' AND stream IS NOT NULL AND taken_at <= ?1
                 AND NOT EXISTS (SELECT 1 FROM custody_follows f WHERE f.follows = custody.digest)
                 AND instr(stream, ?3) = 0 AND NOT EXISTS (SELECT 1 FROM custody_breaks b WHERE b.record = custody.digest) ORDER BY taken_at DESC LIMIT ?2",
            )?;
            let rows = q.query_map(rusqlite::params![before, i64::try_from(crate::custody::LIST_MAX).unwrap_or(i64::MAX), UNANSWERED], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut out = Vec::new();
    for (digest, relay, stream, seq) in open {
        let detail = format!("{relay} passed it on (record {} on {stream}), and no receipt follows it {lapse} s later: dropped on the way, or lost", seq.unwrap_or_default());
        if record {
            crate::custody::relay_break(store, &digest, "no-receipt", &detail, std::slice::from_ref(&digest), "", &relay, at).await?;
        }
        out.push(json!({ "record": digest, "relay": relay, "stream": stream, "seq": seq, "detail": detail }));
    }
    Ok(out)
}

/// Until the server stops: every five seconds, the runs of reach links' frames whose frames stopped
/// coming, recorded (`crate::reach::relay::Relay::due`); every minute, kiln's records of relaying
/// that no receipt answered once they lapsed ([`unanswered`]).
pub async fn relays_loop(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    let mut ticks = 0u32;
    while hub.shutdown.sleep(std::time::Duration::from_secs(5)).await {
        let due = hub.reach.lock().due();
        hub.reach_passed(due);
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(12)
            && let Err(e) = unanswered(&hub.store, crate::custody::LAPSE_AFTER, crate::util::now_unix(), true).await
        {
            tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "relays' records with no receipt couldn't be looked for");
        }
    }
    Ok(())
}

/// The relays' streams as kiln has them (`GET /api/custody/relays`): each relay's numbered records
/// on each stream, how many, and any numbers missing between them; and the records no receipt
/// answered `lapse` seconds on ([`unanswered`], only listed: kiln records them as breaks once they
/// lapse, or when asked to, `POST /api/custody/relays`).
pub async fn view(store: &Store, lapse: i64, at: i64) -> anyhow::Result<Value> {
    let streams: Vec<(String, String, i64, i64, i64)> = store
        .call(|c| {
            let mut q = c.prepare("SELECT custodian, stream, COUNT(*), MIN(seq), MAX(seq) FROM custody WHERE kind = 'passed-on' AND stream IS NOT NULL GROUP BY custodian, stream ORDER BY MAX(taken_at) DESC LIMIT 500")?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let streams: Vec<Value> = streams
        .into_iter()
        .map(|(relay, stream, n, first, last)| {
            // A stream is whole from 1: each number below its last that kiln doesn't have is missing.
            let missing = last.saturating_sub(n);
            json!({ "relay": relay, "stream": stream, "records": n, "first": first, "last": last, "missing": missing })
        })
        .collect();
    Ok(json!({ "streams": streams, "unanswered": unanswered(store, lapse, at, false).await? }))
}
