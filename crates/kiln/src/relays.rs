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
    // The relay's record checked, and the act as it came, signed by its originator or not: another of its
    // signer's, swapped in, is genuine; one altered on the way isn't. Both on the compute pool, off the
    // shards (goal 9, plan 01a1a87b2cec step 3): with kiln's receipt below, three public-key checks and a
    // signature held the shard of the MCP call that relayed it.
    let (b, a, root) = (bundle.clone(), act.clone(), hub.pki.ca_der.clone());
    let (record, signed) = crate::rt::compute(move || (statement::verify(&b, &root, &revocations, at), statement::verify(&a, &root, &[], at).ok())).await?;
    let record = match record {
        Ok(c) => c,
        Err(e) => {
            let why = found.record(&Unrelayed::Unreadable { relay: relay.clone(), why: e.to_string() }, true).await?;
            return Ok(Err(why));
        }
    };
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
    // The act, kept as checked above (a call that got this far has it signed: one that isn't was refused).
    if let Some(c) = &signed {
        crate::custody::keep_originator_checked(&hub.store, c, &act, &came_from).await?;
    }
    match crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &bundle, &came_from, at).await? {
        Ok(t) if t.broke.is_none() => {}
        Ok(t) => return Ok(Err(format!("{relay}'s record of passing it on doesn't hold: {}", t.broke.unwrap_or_default()))),
        Err(why) => return Ok(Err(format!("{relay}'s record of passing it on was refused: {why}"))),
    }
    let receipt = relay::receipt(&server, &relay, vec![thing], &record.digest, "an agent's call, taken by kiln");
    let signed_receipt = crate::custody::server_signed_off(&hub.pki, receipt).await?;
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
        // Signed on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
        let bundle = crate::custody::server_signed_off(&hub.pki, s).await?;
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
            // Signed on the compute pool, off the shards (goal 9, plan 01a1a87b2cec step 3).
            let bundle = crate::custody::server_signed_off(&hub.pki, relay::passed_on(&pass, vec![thing], Vec::new())).await?;
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

/// kiln's records of relaying that no receipt answered, taken at `?1` or before, newest first, at most `?2`: the passes
/// still open (`custody_open_passes`, migration `dbq_relays`, which the writes that keep a pass, answer it or break it
/// change in their own transactions), where every pass kiln ever took was walked, and its followers looked for, each
/// minute (the DB query audit, 2026-11-06: 2.5 ms at production's size and 44 ms at ten times its passes, 6 and 56 us).
/// `LIMIT +?2`: SQLite plans by a bare `?2` there, preparing the statement again each time it's bound.
const OPEN_PASSES: &str = "SELECT c.digest, c.custodian, c.stream, c.seq FROM custody_open_passes o CROSS JOIN custody c ON c.digest = o.record WHERE c.taken_at <= ?1 ORDER BY c.taken_at DESC LIMIT +?2";

/// kiln's records of what reached it from a relay (or of kiln's own relaying) that no receipt
/// answered `lapse` seconds after kiln took them: each a hand-on with no receipt downstream
/// (dropped on the way, or lost), and, when `record`, recorded as a break naming its relay, once.
/// Each, newest first. Not where its receiver is from before relays were checked, whose streams say
/// so ([`UNANSWERED`]): it never answers; nor one already recorded as a break (refused where it
/// came, or never delivered). Read beside the writer ([`OPEN_PASSES`]): no caller looks for a write
/// of its own it hasn't waited for.
pub async fn unanswered(store: &Store, lapse: i64, at: i64, record: bool) -> anyhow::Result<Vec<Value>> {
    let before = at.saturating_sub(lapse);
    let open: Vec<(String, String, String, Option<i64>)> = store
        .read(move |c| {
            let mut q = c.prepare_cached(OPEN_PASSES)?;
            let rows = q.query_map(rusqlite::params![before, i64::try_from(crate::custody::LIST_MAX).unwrap_or(i64::MAX)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
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
#[allow(clippy::disallowed_methods)] // (e) relays looked at every 5 s; their state changes could tell
pub async fn relays_loop(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    let mut ticks = 0u32;
    while hub.shutdown.sleep(std::time::Duration::from_secs(5)).await {
        // The relay's owner records them in its turn (goal 5, plan 01a1a59c2aae step 2).
        hub.relay().send(crate::reach::relay::Ask::Due);
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(12)
            && let Err(e) = unanswered(&hub.store, crate::custody::LAPSE_AFTER, crate::util::now_unix(), true).await
        {
            tracing::warn!(target: "kiln.custody", error = %format!("{e:#}"), "relays' records with no receipt couldn't be looked for");
        }
    }
    Ok(())
}

/// Each relay's streams, the 500 it passed a record on last first: how many of their records kiln has, and the first
/// and last numbers among them, read as kept with each pass (`custody_relay_streams`, migration `dbq_relays`), where
/// every pass kiln ever took was read and grouped for each look (the DB query audit, 2026-11-06: 1.6 ms at
/// production's size and 19.5 ms at ten times its passes, 0.12 and 0.16 ms).
const STREAMS: &str = "SELECT custodian, stream, records, first, last FROM custody_relay_streams ORDER BY newest DESC LIMIT 500";

/// The relays' streams as kiln has them (`GET /api/custody/relays`): each relay's numbered records
/// on each stream, how many, and any numbers missing between them ([`STREAMS`]); and the records no
/// receipt answered `lapse` seconds on ([`unanswered`], only listed: kiln records them as breaks once
/// they lapse, or when asked to, `POST /api/custody/relays`). Read beside the writer, as [`unanswered`] is.
pub async fn view(store: &Store, lapse: i64, at: i64) -> anyhow::Result<Value> {
    let streams: Vec<(String, String, i64, i64, i64)> = store
        .read(|c| {
            let mut q = c.prepare_cached(STREAMS)?;
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


#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use rusqlite::types::Value as Sql;

    use super::*;

    /// The relays' migration: applied again where its tables and triggers are gone, it's what a database kept before
    /// it meets.
    const MIGRATION: &str = include_str!("store/migrations/20261106-2106_dbq_relays.sql");

    /// How many records' digests the steps choose from: few enough that records follow, break and replace each other.
    const POOL: u64 = 300;
    const STEPS: usize = 1200;

    /// A record as the model keeps it.
    #[derive(Clone, Debug, PartialEq)]
    struct Kept {
        kind: Option<&'static str>,
        custodian: String,
        stream: Option<String>,
        seq: i64,
        taken_at: i64,
    }

    /// What the steps wrote, as the specification reads it.
    #[derive(Default)]
    struct Model {
        records: BTreeMap<String, Kept>,
        /// Each record following another: (the record, the one it follows).
        follows: BTreeSet<(String, String)>,
        /// Each break, in the order recorded: the record it's at, its kind and its words.
        breaks: Vec<(String, String, String)>,
    }

    impl Model {
        /// The passes open: a relay's record on a stream that waits for a receipt (not one saying its receiver is
        /// from before relays were checked), which no record follows and no break is recorded at.
        fn open(&self) -> BTreeSet<String> {
            let waits = |k: &Kept| k.kind == Some("passed-on") && k.stream.as_deref().is_some_and(|s| !s.contains(UNANSWERED));
            self.records.iter().filter(|(d, k)| waits(k) && !self.follows.iter().any(|(_, f)| f == *d) && !self.breaks.iter().any(|(r, ..)| r == *d)).map(|(d, _)| d.clone()).collect()
        }

        /// Each relay's stream, as the relays' page lists it: (its relay, the stream) to how many of its passes kiln
        /// has, their first and last numbers, and when kiln took the newest.
        fn streams(&self) -> BTreeMap<(String, String), (i64, i64, i64, i64)> {
            let mut out: BTreeMap<(String, String), (i64, i64, i64, i64)> = BTreeMap::new();
            for k in self.records.values().filter(|k| k.kind == Some("passed-on")) {
                let Some(stream) = &k.stream else { continue };
                let e = out.entry((k.custodian.clone(), stream.clone())).or_insert((0, k.seq, k.seq, k.taken_at));
                *e = (e.0 + 1, e.1.min(k.seq), e.2.max(k.seq), e.3.max(k.taken_at));
            }
            out
        }

        /// Whether `listed` (as [`STREAMS`] gives them: relay, stream, records, first, last) is what the specification
        /// says: every stream (they're fewer than 500), each as kept, the one passed on last first.
        fn lists(&self, listed: &[(String, String, i64, i64, i64)]) -> Result<(), String> {
            let streams = self.streams();
            if listed.len() != streams.len() {
                return Err(format!("{} streams listed, of {}", listed.len(), streams.len()));
            }
            let mut at = i64::MAX;
            for (relay, stream, records, first, last) in listed {
                let Some(&(n, lo, hi, newest)) = streams.get(&(relay.clone(), stream.clone())) else { return Err(format!("{relay} {stream:?} isn't a stream")) };
                if (*records, *first, *last) != (n, lo, hi) || newest > at {
                    return Err(format!("{relay} {stream:?}: {records} records, {first} to {last}; kept, {n}, {lo} to {hi}; or out of order"));
                }
                at = newest;
            }
            Ok(())
        }

        /// Records a break as `crate::custody::relay_break` does: once for its record, kind and words.
        fn broke(&mut self, record: &str, kind: &str, detail: &str) {
            if !self.breaks.iter().any(|(r, k, w)| r == record && k == kind && w == detail) {
                self.breaks.push((record.to_owned(), kind.to_owned(), detail.to_owned()));
            }
        }

        /// Whether `found` (as [`OPEN_PASSES`] gives them) is what the specification says for `before` and `limit`:
        /// the open passes taken then or before, each as kept, newest first; the newest of them when there are more.
        fn holds(&self, found: &[(String, String, String, Option<i64>)], before: i64, limit: usize) -> Result<(), String> {
            let due: BTreeMap<String, &Kept> = self.open().into_iter().filter_map(|d| self.records.get(&d).filter(|k| k.taken_at <= before).map(|k| (d, k))).collect();
            if found.len() != limit.min(due.len()) {
                return Err(format!("{} found, of {} due, at most {limit}", found.len(), due.len()));
            }
            let mut at = i64::MAX;
            let mut seen = BTreeSet::new();
            for (digest, custodian, stream, seq) in found {
                let Some(k) = due.get(digest) else { return Err(format!("{digest} isn't due")) };
                if !seen.insert(digest) || *custodian != k.custodian || Some(stream) != k.stream.as_ref() || *seq != Some(k.seq) || k.taken_at > at {
                    return Err(format!("{digest} found again, not as kept, or out of order"));
                }
                at = k.taken_at;
            }
            match due.iter().filter(|(d, _)| !seen.contains(d)).map(|(_, k)| k.taken_at).max() {
                Some(left) if left > at => Err(format!("one taken at {left} was left for older ones")),
                _ => Ok(()),
            }
        }
    }

    /// SplitMix64: the same steps from the same seed, every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }

        /// From `lo`, one of the `n` numbers after it.
        fn int(&mut self, lo: i64, n: u64) -> i64 {
            lo.saturating_add(i64::try_from(self.below(n)).unwrap_or(0))
        }

        /// One of `xs`, if it has any.
        fn pick<T: Clone>(&mut self, xs: &[T]) -> Option<T> {
            xs.get(usize::try_from(self.below(xs.len() as u64)).unwrap_or(0)).cloned()
        }

        fn digest(&mut self) -> String {
            format!("sha256:{:03}", self.below(POOL))
        }

        /// A record's kind: most often a relay's record.
        fn kind(&mut self) -> Option<&'static str> {
            self.pick(&[Some("passed-on"), Some("passed-on"), Some("passed-on"), Some("handed-on"), Some("received"), None]).flatten()
        }

        /// A record's stream: one that waits for a receipt, one a runner from before relays were checked says won't
        /// see one, or none.
        fn stream(&mut self) -> Option<String> {
            self.pick(&[Some("words to r1, from kiln started 1".to_owned()), Some("reach link 7 to r2, from kiln started 1".to_owned()), Some(format!("words to r3, from kiln started 1{UNANSWERED}")), None]).flatten()
        }

        /// A follower, or a break, the model has: its two parts (none when it has none).
        fn of<'a>(&mut self, xs: impl Iterator<Item = (&'a String, &'a String)>) -> Option<(String, String)> {
            let all: Vec<(String, String)> = xs.map(|(a, b)| (a.clone(), b.clone())).collect();
            self.pick(&all)
        }

        /// One write: its statements, each with its values, and what it does to `model`.
        fn write(&mut self, model: &mut Model) -> Vec<(&'static str, Vec<Sql>)> {
            let text = |s: &str| Sql::Text(s.to_owned());
            let maybe = |s: Option<&str>| s.map_or(Sql::Null, |s| Sql::Text(s.to_owned()));
            match self.below(100) {
                0..40 => {
                    let (digest, kind, stream) = (self.digest(), self.kind(), self.stream());
                    let k = Kept { kind, custodian: format!("kiln://acme/person/ada/machine/m{}/instance/i", self.below(3)), stream, seq: self.int(1, 9), taken_at: self.int(1000, 30) };
                    let w = vec![(
                        "INSERT OR IGNORE INTO custody (digest, payload_type, kind, custodian, stream, seq, envelope, came_from, taken_at) VALUES (?1, 'application/vnd.kiln.custody+json', ?2, ?3, ?4, ?5, '{}', 'a test', ?6)",
                        vec![text(&digest), maybe(k.kind), text(&k.custodian), maybe(k.stream.as_deref()), Sql::Integer(k.seq), Sql::Integer(k.taken_at)],
                    )];
                    model.records.entry(digest).or_insert(k);
                    w
                }
                40..65 => {
                    let (record, follows) = (self.digest(), self.digest());
                    model.follows.insert((record.clone(), follows.clone()));
                    vec![("INSERT OR IGNORE INTO custody_follows (record, follows) VALUES (?1, ?2)", vec![text(&record), text(&follows)])]
                }
                65..72 => {
                    let (record, detail) = (self.digest(), format!("a gap {}", self.next()));
                    model.broke(&record, "gap", &detail);
                    vec![("INSERT INTO custody_breaks (record, kind, detail, at) VALUES (?1, 'gap', ?2, 1)", vec![text(&record), text(&detail)])]
                }
                72..76 => {
                    let Some((record, follows)) = self.of(model.follows.iter().map(|(a, b)| (a, b))) else { return Vec::new() };
                    model.follows.remove(&(record.clone(), follows.clone()));
                    vec![("DELETE FROM custody_follows WHERE record = ?1 AND follows = ?2", vec![text(&record), text(&follows)])]
                }
                76..80 => {
                    let record = self.digest();
                    if let Some(i) = model.breaks.iter().position(|(r, ..)| *r == record) {
                        model.breaks.remove(i);
                    }
                    vec![("DELETE FROM custody_breaks WHERE id = (SELECT MIN(id) FROM custody_breaks WHERE record = ?1)", vec![text(&record)])]
                }
                80..83 => {
                    let digest = self.digest();
                    model.records.remove(&digest);
                    vec![("DELETE FROM custody WHERE digest = ?1", vec![text(&digest)])]
                }
                83..87 => {
                    let (digest, kind) = (self.digest(), self.kind());
                    if let Some(k) = model.records.get_mut(&digest) {
                        k.kind = kind;
                    }
                    vec![("UPDATE custody SET kind = ?2 WHERE digest = ?1", vec![text(&digest), maybe(kind)])]
                }
                87..91 => {
                    let (digest, stream) = (self.digest(), self.stream());
                    let w = vec![("UPDATE custody SET stream = ?2 WHERE digest = ?1", vec![text(&digest), maybe(stream.as_deref())])];
                    if let Some(k) = model.records.get_mut(&digest) {
                        k.stream = stream;
                    }
                    w
                }
                91..94 => {
                    let (from, to) = (self.digest(), self.digest());
                    if from != to && !model.records.contains_key(&to)
                        && let Some(k) = model.records.remove(&from)
                    {
                        model.records.insert(to.clone(), k);
                    }
                    vec![("UPDATE OR IGNORE custody SET digest = ?2 WHERE digest = ?1", vec![text(&from), text(&to)])]
                }
                94..97 => {
                    let Some((record, from)) = self.of(model.follows.iter().map(|(a, b)| (a, b))) else { return Vec::new() };
                    let to = self.digest();
                    if !model.follows.contains(&(record.clone(), to.clone())) {
                        model.follows.remove(&(record.clone(), from.clone()));
                        model.follows.insert((record.clone(), to.clone()));
                    }
                    vec![("UPDATE OR IGNORE custody_follows SET follows = ?3 WHERE record = ?1 AND follows = ?2", vec![text(&record), text(&from), text(&to)])]
                }
                _ => {
                    let (from, to) = (self.digest(), self.digest());
                    if let Some(b) = model.breaks.iter_mut().find(|(r, ..)| *r == from) {
                        b.0.clone_from(&to);
                    }
                    vec![("UPDATE custody_breaks SET record = ?2 WHERE id = (SELECT MIN(id) FROM custody_breaks WHERE record = ?1)", vec![text(&from), text(&to)])]
                }
            }
        }
    }

    /// Runs `w` in one transaction on the writer; `cut_short`, the process stops before it commits.
    async fn run(store: &Store, w: Vec<(&'static str, Vec<Sql>)>, cut_short: bool) -> anyhow::Result<()> {
        store
            .call(move |c| {
                let tx = crate::store::tx(c)?;
                for (sql, args) in &w {
                    tx.execute(sql, rusqlite::params_from_iter(args.iter()))?;
                }
                anyhow::ensure!(!cut_short, "the process stopped before this write committed");
                tx.commit()?;
                Ok(())
            })
            .await
    }

    /// What the relays' tables hold and their queries find, read as [`unanswered`] and [`view`] read.
    struct Read {
        /// What [`OPEN_PASSES`] finds for each ask (by when, and at most how many).
        found: Vec<Vec<(String, String, String, Option<i64>)>>,
        /// What `custody_open_passes` holds.
        open: BTreeSet<String>,
        /// What `custody_relay_streams` holds, as [`Model::streams`] says it.
        streams: BTreeMap<(String, String), (i64, i64, i64, i64)>,
        /// What [`STREAMS`] lists.
        listed: Vec<(String, String, i64, i64, i64)>,
    }

    async fn read(store: &Store, asks: Vec<(i64, i64)>) -> anyhow::Result<Read> {
        store
            .read(move |c| {
                let mut found = Vec::new();
                for (before, limit) in asks {
                    found.push(c.prepare_cached(OPEN_PASSES)?.query_map(rusqlite::params![before, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?);
                }
                let open = c.prepare_cached("SELECT record FROM custody_open_passes")?.query_map([], |r| r.get(0))?.collect::<Result<BTreeSet<String>, _>>()?;
                let streams = c
                    .prepare_cached("SELECT custodian, stream, records, first, last, newest FROM custody_relay_streams")?
                    .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))))?
                    .collect::<Result<BTreeMap<_, _>, _>>()?;
                let listed = c.prepare_cached(STREAMS)?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(Read { found, open, streams, listed })
            })
            .await
    }

    /// The DB query audit (2026-11-06): at every step, the open passes are exactly the relays' records the
    /// specification calls open (on a stream that waits for a receipt, no record following them, no break at them),
    /// and [`unanswered`] finds those taken by when it's asked, as kept, newest first, the newest when there are more
    /// than it gives; and each relay's stream is its passes kiln has, counted, their first and last numbers, which
    /// [`view`] lists, the one passed on last first. The steps keep passes (on streams that wait, on one a runner
    /// from before says won't, on none) and records of other kinds, records following them (some before them), breaks
    /// (the loop's own, a relay's, any), and take followers, breaks and records away or change them; some writes stop
    /// before they commit, some write several in one transaction, the store is opened again from its file, and once
    /// the relays' tables and triggers are gone for a while, as in a database kept before them, and the migration
    /// finds them again.
    #[test]
    fn dbq_relays_passes_and_streams_are_as_kept_at_every_step() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("kiln.db");
            let mut store = Store::open(&path)?;
            let (mut rng, mut model) = (Rng(20_261_106), Model::default());
            let (mut found, mut open_steps, mut recorded, mut streamed_steps) = (0, 0, 0, 0);
            for step in 0..STEPS {
                if step == STEPS / 2 {
                    let dropped = store
                        .call(|c| {
                            let mut dropped = Vec::new();
                            for table in ["custody_open_passes", "custody_relay_streams"] {
                                let triggers: Vec<String> = c.prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name GLOB ?1 || '_*'")?.query_map([table], |r| r.get(0))?.collect::<Result<_, _>>()?;
                                for t in &triggers {
                                    c.execute_batch(&format!("DROP TRIGGER {t}"))?;
                                }
                                c.execute_batch(&format!("DROP TABLE {table}"))?;
                                dropped.push(triggers.len());
                            }
                            Ok(dropped)
                        })
                        .await?;
                    assert_eq!(dropped, [9, 3], "a pass kept, changed or gone, a follower and a break each kept, changed or gone; a pass kept, gone or changed");
                    for _ in 0..100 {
                        let w = rng.write(&mut model);
                        run(&store, w, false).await?;
                    }
                    store.call(|c| Ok(c.execute_batch(MIGRATION)?)).await?;
                }
                match rng.below(100) {
                    0..3 => {
                        drop(store);
                        store = Store::open(&path)?;
                    }
                    3..8 => {
                        // Written and stopped before it committed: nothing of it stays, so the model has none of it.
                        let w = rng.write(&mut Model::default());
                        assert!(run(&store, w, true).await.is_err(), "a write cut short fails");
                    }
                    8..13 => {
                        let mut w = rng.write(&mut model);
                        w.extend(rng.write(&mut model));
                        w.extend(rng.write(&mut model));
                        run(&store, w, false).await?;
                    }
                    13..18 => {
                        // The relays' loop: it finds the passes due and records a break at each.
                        let (lapse, at) = (rng.int(-5, 40), 1030);
                        let listed = unanswered(&store, lapse, at, true).await?;
                        let rows: Vec<(String, String, String, Option<i64>)> = listed
                            .iter()
                            .map(|v| {
                                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                                (s("record"), s("relay"), s("stream"), v.get("seq").and_then(Value::as_i64))
                            })
                            .collect();
                        model.holds(&rows, at - lapse, crate::custody::LIST_MAX).map_err(|why| anyhow::anyhow!("step {step}, the loop by {}: {why}", at - lapse))?;
                        // The relays' page, as the loop finds them: its streams, each with the numbers it misses.
                        let page = view(&store, lapse, at).await?;
                        let paged: Vec<(String, String, i64, i64, i64)> = page
                            .get("streams")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .map(|v| {
                                let (s, n) = (|k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned(), |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(-1));
                                anyhow::ensure!(n("missing") == n("last").saturating_sub(n("records")), "step {step}: {v} misses its last number less those it has");
                                Ok((s("relay"), s("stream"), n("records"), n("first"), n("last")))
                            })
                            .collect::<anyhow::Result<_>>()?;
                        model.lists(&paged).map_err(|why| anyhow::anyhow!("step {step}, the relays' page: {why}"))?;
                        for v in &listed {
                            let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                            model.broke(&s("record"), "no-receipt", &s("detail"));
                        }
                        recorded += listed.len();
                    }
                    18..23 => {
                        let (record, detail) = (rng.digest(), format!("relay r1 dropped it at step {step}"));
                        crate::custody::relay_break(&store, &record, "dropped", &detail, std::slice::from_ref(&record), "", "kiln://acme/server", 1).await?;
                        model.broke(&record, "dropped", &detail);
                    }
                    _ => {
                        let w = rng.write(&mut model);
                        run(&store, w, false).await?;
                    }
                }
                let mut asks = Vec::new();
                for _ in 0..4 {
                    asks.push((rng.int(995, 40), rng.pick(&[1, 2, 3, 500]).unwrap_or(500)));
                }
                let Read { found: lists, open, streams, listed } = read(&store, asks.clone()).await?;
                assert_eq!(open, model.open(), "step {step}: the open passes");
                assert_eq!(streams, model.streams(), "step {step}: the relays' streams");
                model.lists(&listed).map_err(|why| anyhow::anyhow!("step {step}, the streams listed: {why}"))?;
                streamed_steps += usize::from(!streams.is_empty());
                for ((before, limit), list) in asks.iter().zip(&lists) {
                    model.holds(list, *before, usize::try_from(*limit).unwrap_or(usize::MAX)).map_err(|why| anyhow::anyhow!("step {step}, by {before}, at most {limit}: {why}"))?;
                    found += list.len();
                }
                open_steps += usize::from(!open.is_empty());
            }
            assert!(found > 200 && open_steps > STEPS / 4 && recorded > 0, "the steps met open passes: {found} found, open at {open_steps} steps, {recorded} recorded by the loop");
            assert!(streamed_steps > STEPS / 2, "the steps met streams: at {streamed_steps} steps");
            Ok(())
        })
    }

    /// The DB query audit (2026-11-06): the open passes are read off their table, each pass by its digest, never by
    /// walking every pass kiln took by its kind, and the streams off theirs, in the order they're listed; the
    /// statement is prepared once, whatever is bound next; and the migration's triggers know a stream that says its
    /// receiver won't answer by [`UNANSWERED`] itself, in each place.
    #[test]
    fn dbq_relays_reads_are_planned_on_their_tables() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = Store::in_memory()?;
            let (plan, again, streams) = store
                .call(|c| {
                    let plan: Vec<String> = c.prepare(&format!("EXPLAIN QUERY PLAN {OPEN_PASSES}"))?.query_map(rusqlite::params![1000, 500], |r| r.get(3))?.collect::<Result<_, _>>()?;
                    let mut q = c.prepare(OPEN_PASSES)?;
                    for (before, limit) in [(1000, 500), (2000, 10), (0, 1)] {
                        let mut rows = q.query(rusqlite::params![before, limit])?;
                        while rows.next()?.is_some() {}
                    }
                    let streams: Vec<String> = c.prepare(&format!("EXPLAIN QUERY PLAN {STREAMS}"))?.query_map([], |r| r.get(3))?.collect::<Result<_, _>>()?;
                    Ok((plan, q.get_status(rusqlite::StatementStatus::RePrepare), streams))
                })
                .await?;
            assert!(plan.iter().any(|p| p == "SCAN o") && plan.iter().any(|p| p.contains("sqlite_autoindex_custody_1")), "{plan:?}");
            assert!(!plan.iter().any(|p| p.contains("custody_kind_taken")), "{plan:?}");
            assert_eq!(again, 0, "prepared once");
            assert!(streams.iter().any(|p| p.contains("custody_relay_streams_newest")) && !streams.iter().any(|p| p.contains("TEMP B-TREE") || p.contains("custody_kind_taken")), "{streams:?}");
            assert_eq!(MIGRATION.matches(&format!("'{UNANSWERED}'")).count(), 7, "each trigger that opens a pass, and the backfill");
            Ok(())
        })
    }
}
