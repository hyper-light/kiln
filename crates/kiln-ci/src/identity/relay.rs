//! Relays, checked purely (docs/ci.md W27-R08; the Transfers plan's step 13, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): what an intermediary records as it carries something from
//! one party to another, and what the party it reaches checks, as ARC's handlers each add their
//! record of a message they pass on, and any of them can check the whole set.
//!
//! - **What's relayed** is its originator's own signed statement (an agent session's act), named
//!   by that statement's digest ([`statement_thing`]); bytes whose sender signs nothing of them
//!   (a frame of kiln's server) by their SHA-256 ([`bytes_thing`]); or a run of frames a relay
//!   can't read (a reach link's ciphertext) by the chain of theirs ([`Frames`]).
//! - **A relay's record** ([`passed_on`]) is a custody record of kind passed-on: what, from whom it
//!   took it, to whom it passes it, numbered on one of its streams. A receiver sees one stream
//!   whole: each is the relay's to one receiver, for one run of the relay's process or connection.
//!   It follows the originator's statement, or the record of the relay before it; what its sender
//!   signs nothing of follows nothing, its chain beginning at the relay.
//! - **The receiver's check** ([`relayed`], [`Numbering`]): the record is a passed-on, signed by
//!   the relay the thing came through, to this receiver, about what arrived, and its number comes
//!   next on its stream. An altered thing's digest differs; a thing swapped for another, itself
//!   signed by its originator, is another; a number skipped is a record dropped on the way, and one
//!   seen before is one sent again. Each is refused where it's used and recorded as a break naming
//!   the relay. The originator's own signature, checked end to end, is the caller's to check (an
//!   act's [`super::custody::acted`]).
//! - **The receiver's receipt** ([`receipt`]) follows the relay's record, about the thing as it
//!   arrived, so a walk ([`super::custody::walk`]) goes from where it's used back through each relay
//!   to its originator.
//!
//! No clock, disk or network: the records and what arrived are arguments.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::chain;
use super::custody::{self, Body, Kind};
use super::statement::{Checked, PAYLOAD_CUSTODY, Statement, Subject};

/// The digest algorithm a run of frames is named by ([`Frames`]).
pub const FRAMES: &str = "kiln-frames-v1";

/// The most streams a [`Numbering`] keeps: past it, the oldest it saw is forgotten (a stream is a
/// run of a relay's process or connection, and one that ended is never seen again).
pub const STREAMS_MAX: usize = 4096;

/// An originator's signed statement (its digest, `sha256:<hex>`) as a thing a relay passes on.
pub fn statement_thing(name: &str, digest: &str) -> Subject {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest).to_ascii_lowercase();
    Subject { name: name.to_owned(), digest: BTreeMap::from([("sha256".to_owned(), hex)]), ..Subject::default() }
}

/// Bytes whose sender signs nothing of them (a frame of kiln's server) as a thing a relay passes
/// on, by their SHA-256.
pub fn bytes_thing(name: &str, bytes: &[u8]) -> Subject {
    Subject { name: name.to_owned(), digest: BTreeMap::from([("sha256".to_owned(), chain::hex(&Sha256::digest(bytes)))]), size: u64::try_from(bytes.len()).ok(), ..Subject::default() }
}

/// A run of frames as a relay that can't read them passes them on: each frame's SHA-256, chained
/// (`sha256(previous ‖ sha256(frame))`, from 32 zero bytes), numbered from `first` in the order
/// they went. Relay and receiver each chain what they saw: a frame dropped, swapped, added or
/// changed on the way makes another chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frames {
    first: u64,
    count: u64,
    head: [u8; 32],
}

impl Frames {
    /// A run starting at frame `first`, holding none yet.
    pub fn new(first: u64) -> Self {
        Self { first, count: 0, head: [0; 32] }
    }

    /// The next frame of the run.
    pub fn push(&mut self, frame: &[u8]) {
        self.push_digest(digest_of(frame));
    }

    /// The next frame of the run, by its SHA-256 ([`digest_of`]): what a receiver keeps of a frame
    /// it got until the relay's record of its run comes.
    pub fn push_digest(&mut self, digest: [u8; 32]) {
        let mut h = Sha256::new();
        h.update(self.head);
        h.update(digest);
        self.head = h.finalize().into();
        self.count = self.count.saturating_add(1);
    }

    /// The number of its first frame.
    pub fn first(&self) -> u64 {
        self.first
    }

    /// How many frames it holds.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The number of the frame after its last.
    pub fn next(&self) -> u64 {
        self.first.saturating_add(self.count)
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The run as a thing named `name`: its chain's head, its first frame's number and how many.
    pub fn thing(&self, name: &str) -> Subject {
        let annotations = BTreeMap::from([("first".to_owned(), Value::from(self.first)), ("frames".to_owned(), Value::from(self.count))]);
        Subject { name: name.to_owned(), digest: BTreeMap::from([(FRAMES.to_owned(), chain::hex(&self.head))]), size: None, annotations }
    }
}

/// A frame's SHA-256, as a run chains it ([`Frames::push_digest`]).
pub fn digest_of(frame: &[u8]) -> [u8; 32] {
    Sha256::digest(frame).into()
}

/// A run's first frame and how many it holds, as its thing says ([`Frames::thing`]).
pub fn frames_of(thing: &Subject) -> Option<(u64, u64)> {
    let n = |k: &str| thing.annotations.get(k).and_then(Value::as_u64);
    Some((n("first")?, n("frames")?))
}

/// Who passes what on, numbered where: a relay's record's words ([`passed_on`]).
#[derive(Clone, Copy, Debug)]
pub struct Pass<'a> {
    /// The relay, as its certificate names it.
    pub by: &'a str,
    /// Whom it took the thing from: its originator, or the relay before it.
    pub from: &'a str,
    /// Whom it passes it to.
    pub to: &'a str,
    /// Its stream to `to`, and the record's number on it (from 1).
    pub stream: &'a str,
    pub seq: u64,
    pub why: &'a str,
}

/// A relay's record ([`Kind::PassedOn`]) that it passed `things` on unchanged, as `p` says,
/// following `follows`: the originator's statement, or the record of the relay before it; none for
/// what its sender signs nothing of.
pub fn passed_on(p: &Pass<'_>, things: Vec<Subject>, follows: Vec<String>) -> Statement {
    let body = Body { from: Some(p.from.to_owned()), to: Some(p.to.to_owned()), why: p.why.to_owned(), ..Body::new(Kind::PassedOn, p.by) };
    Statement { subject: things, follows, stream: Some(p.stream.to_owned()), seq: Some(p.seq), ..Statement::new(custody::to_value(&body)) }
}

/// The receiver's receipt (`by`) of what relay `relay` passed on to it (its record's digest
/// `record`): `things` as they arrived, by the digests the receiver worked out itself, so one
/// changed on the way breaks the chain at that hop.
pub fn receipt(by: &str, relay: &str, things: Vec<Subject>, record: &str, why: &str) -> Statement {
    let body = Body { from: Some(relay.to_owned()), why: why.to_owned(), ..Body::new(Kind::Received, by) };
    Statement { subject: things, follows: vec![record.to_owned()], ..Statement::new(custody::to_value(&body)) }
}

/// Why what a relay passed on isn't taken as it says. Each names the relay.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unrelayed {
    #[error("{relay} passed it on with a record kiln doesn't read: {why}")]
    Unreadable { relay: String, why: String },
    #[error("it came through {relay}, but its record of passing it on is signed by {signer}")]
    Forged { relay: String, signer: String },
    #[error("{relay} passed it on to {to}, not to {here}")]
    NotHere { relay: String, to: String, here: String },
    #[error("{relay} changed {thing} on the way: its {algorithm} was {was} as {relay} passed it on, and is {now} as it arrived")]
    Changed { relay: String, thing: String, algorithm: String, was: String, now: String },
    #[error("{relay} passed on {named}, but what arrived is {got}, another")]
    Swapped { relay: String, named: String, got: String },
    #[error("{} on {stream} never came: dropped on the way", numbers(relay, *from, *to))]
    Dropped { relay: String, stream: String, from: u64, to: u64 },
    #[error("{relay}'s record {seq} on {stream} came before: sent again, or out of its order")]
    Again { relay: String, stream: String, seq: u64 },
    #[error("{relay}'s record of passing it on is numbered on no stream")]
    Unnumbered { relay: String },
    #[error("{relay} passed on {what} with no record of doing so")]
    Unrecorded { relay: String, what: String },
}

/// "fedora's record 4", "fedora's records 4 to 6".
fn numbers(relay: &str, from: u64, to: u64) -> String {
    if from == to { format!("{relay}'s record {from}") } else { format!("{relay}'s records {from} to {to}") }
}

impl Unrelayed {
    /// Its kind, as kiln's records name it.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreadable { .. } => "unreadable",
            Self::Forged { .. } => "forged",
            Self::NotHere { .. } => "not-here",
            Self::Changed { .. } => "changed",
            Self::Swapped { .. } => "swapped",
            Self::Dropped { .. } => "dropped",
            Self::Again { .. } => "again",
            Self::Unnumbered { .. } => "unnumbered",
            Self::Unrecorded { .. } => "unrecorded",
        }
    }

    /// The relay it names.
    pub fn relay(&self) -> &str {
        match self {
            Self::Unreadable { relay, .. }
            | Self::Forged { relay, .. }
            | Self::NotHere { relay, .. }
            | Self::Changed { relay, .. }
            | Self::Swapped { relay, .. }
            | Self::Dropped { relay, .. }
            | Self::Again { relay, .. }
            | Self::Unrecorded { relay, .. }
            | Self::Unnumbered { relay } => relay,
        }
    }
}

/// Whether `record` (checked: [`super::statement::verify`]) is relay `relay`'s record of passing
/// `arrived` on to `here`, the receiver: a passed-on, signed by `relay` (the one it came through),
/// to `here` (as [`custody::names`] says: a session at an epoch names whichever process holds it),
/// about what arrived by every digest both name. `genuine`: what arrived is itself its
/// originator's signed statement, so one other than the record names was swapped for it, not
/// changed. The record's body, or why not.
pub fn relayed(record: &Checked, relay: &str, here: &str, arrived: &Subject, genuine: bool) -> Result<Body, Unrelayed> {
    let unreadable = |why: String| Unrelayed::Unreadable { relay: relay.to_owned(), why };
    if record.payload_type != PAYLOAD_CUSTODY {
        return Err(unreadable(format!("it's a {} statement, not a custody record", record.payload_type)));
    }
    let b = custody::body(record).map_err(|e| unreadable(e.to_string()))?;
    if b.kind != Kind::PassedOn {
        return Err(unreadable(format!("it says they were {}, not passed on", b.kind)));
    }
    if !custody::names(relay, &record.custodian) && record.custodian != relay {
        return Err(Unrelayed::Forged { relay: relay.to_owned(), signer: record.custodian.clone() });
    }
    let to = b.to.clone().unwrap_or_default();
    if !custody::names(&to, here) {
        return Err(Unrelayed::NotHere { relay: relay.to_owned(), to, here: here.to_owned() });
    }
    if record.statement.stream.is_none() || record.statement.seq.is_none() {
        return Err(Unrelayed::Unnumbered { relay: relay.to_owned() });
    }
    let shared = |t: &Subject| t.digest.iter().filter_map(|(alg, hex)| arrived.digest.get(alg).map(|now| (alg.clone(), hex.clone(), now.clone()))).collect::<Vec<_>>();
    // The thing it names that arrived: one that agrees with what arrived on every digest both
    // name, and shares one.
    if record.statement.subject.iter().any(|t| {
        let both = shared(t);
        !both.is_empty() && both.iter().all(|(_, was, now)| was.eq_ignore_ascii_case(now))
    }) {
        return Ok(b);
    }
    let named = record.statement.subject.iter().map(|t| t.name.clone()).collect::<Vec<_>>().join(", ");
    if genuine {
        return Err(Unrelayed::Swapped { relay: relay.to_owned(), named, got: arrived.name.clone() });
    }
    let differs = record.statement.subject.iter().find_map(|t| shared(t).into_iter().find(|(_, was, now)| !was.eq_ignore_ascii_case(now)).map(|(algorithm, was, now)| (t.name.clone(), algorithm, was, now)));
    match differs {
        Some((thing, algorithm, was, now)) => Err(Unrelayed::Changed { relay: relay.to_owned(), thing, algorithm, was, now }),
        None => Err(Unrelayed::Swapped { relay: relay.to_owned(), named, got: arrived.name.clone() }),
    }
}

/// Where a record stands on its relay's stream, as a receiver takes it ([`Numbering::take`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// Next on its stream.
    Next,
    /// After a gap: the records numbered in it never came (dropped on the way). It's taken; the
    /// gap is a break, recorded naming the relay.
    AfterGap(Unrelayed),
}

/// Where each relay's streams to a receiver stand, as the receiver saw them: the next number it
/// expects of each. A stream is seen from its start, at 1: a relay numbers on a stream of its own
/// for each receiver and each run of its process or connection.
#[derive(Clone, Debug, Default)]
pub struct Numbering {
    next: BTreeMap<(String, String), u64>,
}

impl Numbering {
    /// Takes record `seq` of `relay`'s stream `stream`: next ([`Place::Next`]) or after a gap
    /// ([`Place::AfterGap`]), or refused as one that came before ([`Unrelayed::Again`]).
    pub fn take(&mut self, relay: &str, stream: &str, seq: u64) -> Result<Place, Unrelayed> {
        let key = (relay.to_owned(), stream.to_owned());
        if !self.next.contains_key(&key) && self.next.len() >= STREAMS_MAX
            && let Some(oldest) = self.next.keys().next().cloned()
        {
            self.next.remove(&oldest);
        }
        let next = self.next.entry(key).or_insert(1);
        if seq < *next {
            return Err(Unrelayed::Again { relay: relay.to_owned(), stream: stream.to_owned(), seq });
        }
        let gap = (seq > *next).then(|| Unrelayed::Dropped { relay: relay.to_owned(), stream: stream.to_owned(), from: *next, to: seq - 1 });
        *next = seq.saturating_add(1);
        Ok(gap.map_or(Place::Next, Place::AfterGap))
    }

    /// The next number expected on `relay`'s stream `stream`, once one of it was taken.
    pub fn next(&self, relay: &str, stream: &str) -> Option<u64> {
        self.next.get(&(relay.to_owned(), stream.to_owned())).copied()
    }

    /// Forgets `relay`'s stream `stream`: its run ended.
    pub fn forget(&mut self, relay: &str, stream: &str) {
        self.next.remove(&(relay.to_owned(), stream.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::custody::{Act, PAYLOAD_ACT, walk};
    use crate::identity::statement::{VERSION, bundle, payload, to_sign, verify};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData as _, SanType, SerialNumber, SigningKey as _, date_time_ymd};

    /// 2026-09-21T14:13:20Z: inside every test leaf's validity.
    const AT: i64 = 1_790_000_000;
    const SERVER: &str = "kiln://hyper-light/server";
    /// Kiln PRs at epoch 1, held by adacachy's runner process 01a14794 (the plan's example).
    const AGENT: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1/machine/adacachy/instance/01a14794";
    const ADACACHY: &str = "kiln://hyper-light/person/ada/machine/adacachy/instance/01a14794";
    const FEDORA: &str = "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed";
    const GREEN: &str = "kiln://hyper-light/person/ada/machine/adacachy/instance/01a19000";

    struct Ca {
        der: Vec<u8>,
        issuer: Issuer<'static, KeyPair>,
    }

    fn ca() -> anyhow::Result<Ca> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2025, 1, 1);
        p.not_after = date_time_ymd(2036, 1, 1);
        let cert = p.self_signed(&key)?;
        Ok(Ca { der: cert.der().to_vec(), issuer: Issuer::new(p, key) })
    }

    struct Signer {
        uri: String,
        der: Vec<u8>,
        key: KeyPair,
    }

    impl Signer {
        fn new(ca: &Ca, uri: &str) -> anyhow::Result<Self> {
            let mut p = CertificateParams::new(Vec::<String>::new())?;
            p.subject_alt_names = vec![SanType::URI(uri.try_into()?)];
            p.is_ca = IsCa::ExplicitNoCa;
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.not_before = date_time_ymd(2026, 1, 1);
            p.not_after = date_time_ymd(2027, 1, 1);
            let key = KeyPair::generate()?;
            let serial: Vec<u8> = std::iter::once(0x42).chain(Sha256::digest(key.der_bytes()).iter().take(8).copied()).collect();
            p.serial_number = Some(SerialNumber::from_slice(&serial));
            Ok(Self { uri: uri.to_owned(), der: p.signed_by(&key, &ca.issuer)?.der().to_vec(), key })
        }

        /// `s` signed as `payload_type`, its body naming this signer, checked as kiln took it.
        fn signed(&self, ca: &Ca, payload_type: &str, s: &Statement) -> anyhow::Result<Checked> {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            let p = payload(&s)?;
            Ok(verify(&bundle(payload_type, &p, &self.key.sign(&to_sign(payload_type, &p))?, &self.der), &ca.der, &[], AT)?)
        }
    }

    /// Kiln PRs' act, `goals.done`, over the ask it signs.
    fn act(ca: &Ca, agent: &Signer, ask: &[u8]) -> anyhow::Result<Checked> {
        let body = serde_json::to_value(Act { v: VERSION, by: String::new(), session: "01a1751c".into(), epoch: 1, op: "call goals.done".into() })?;
        let subject = Subject { name: "an act of session 01a1751c: call goals.done".into(), digest: BTreeMap::from([("sha256".to_owned(), chain::hex(&Sha256::digest(ask)))]), ..Subject::default() };
        agent.signed(ca, PAYLOAD_ACT, &Statement { subject: vec![subject], ..Statement::new(body) })
    }

    fn the_act(c: &Checked) -> Subject {
        statement_thing("an act of session 01a1751c: call goals.done", &c.digest)
    }

    /// The plan's example: Kiln PRs calls goals.done, its runner adacachy passes the signed ask on
    /// to kiln, recording "passed-on #5521", and kiln's receipt follows it. A walk from kiln's
    /// receipt goes back through the runner's record to the act itself, signed by the agent (on a
    /// stream kiln has whole: from its first record).
    #[test]
    fn w27_r08_relays_a_runner_passes_its_agents_call_on_and_kilns_receipt_follows() -> anyhow::Result<()> {
        let ca = ca()?;
        let (agent, runner, server) = (Signer::new(&ca, AGENT)?, Signer::new(&ca, ADACACHY)?, Signer::new(&ca, SERVER)?);
        let ask = br#"{"op":"call","session":"01a1751c","name":"goals.done"}"#;
        let a = act(&ca, &agent, ask)?;
        let example = Pass { by: ADACACHY, from: AGENT, to: SERVER, stream: "calls to kiln", seq: 5521, why: "an agent's call, passed on to kiln" };
        let p = runner.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&example, vec![the_act(&a)], vec![a.digest.clone()]))?;
        let b = relayed(&p, ADACACHY, SERVER, &the_act(&a), true)?;
        assert_eq!((b.kind, b.from.as_deref(), b.to.as_deref()), (Kind::PassedOn, Some(AGENT), Some(SERVER)));
        assert_eq!((p.statement.stream.as_deref(), p.statement.seq), (Some("calls to kiln"), Some(5521)));
        // Walked, a relay's stream is checked whole: kiln has record 5521 alone, so 1 to 5520 are a gap.
        let alone: BTreeMap<String, Checked> = [a.clone(), p.clone()].into_iter().map(|c| (c.digest.clone(), c)).collect();
        assert!(matches!(walk(&p.digest, &alone, SERVER), Err(custody::Break::Gap(_))));
        let pass = Pass { seq: 1, ..example };
        let p = runner.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&pass, vec![the_act(&a)], vec![a.digest.clone()]))?;
        let got = server.signed(&ca, PAYLOAD_CUSTODY, &receipt(SERVER, ADACACHY, vec![the_act(&a)], &p.digest, "an agent's call"))?;
        let records: BTreeMap<String, Checked> = [a.clone(), p.clone(), got.clone()].into_iter().map(|c| (c.digest.clone(), c)).collect();
        let w = walk(&got.digest, &records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| (h.kind, h.custodian.as_str())).collect::<Vec<_>>(), vec![(Kind::Received, SERVER), (Kind::PassedOn, ADACACHY)]);
        // The act began it: the runner's record follows the agent's own statement.
        assert_eq!(w.began, vec![p.digest.clone()]);
        // A record that says the act came from another than its signer breaks there.
        let liar = Pass { from: FEDORA, seq: 2, ..pass };
        let p2 = runner.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&liar, vec![the_act(&a)], vec![a.digest.clone()]))?;
        let mut records2 = records.clone();
        records2.insert(p2.digest.clone(), p2.clone());
        assert!(matches!(custody::hop(&p2.digest, &records2, SERVER), Err(custody::Break::NotTheGiver { .. })));
        // One about another thing than the statement it follows breaks there too.
        let other = statement_thing("an act of session 01a1751c: call goals.done", &format!("sha256:{}", "e".repeat(64)));
        let p3 = runner.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { seq: 3, ..pass }, vec![other], vec![a.digest.clone()]))?;
        records2.insert(p3.digest.clone(), p3.clone());
        assert!(matches!(custody::hop(&p3.digest, &records2, SERVER), Err(custody::Break::NotHandedOn { .. })));
        Ok(())
    }

    /// The server passes an agent's act on to another agent's runner, after the sender's runner
    /// passed it on to the server: two relays, each a record numbered on its stream, and the
    /// receiver's receipt follows the last. The walk goes back through both.
    #[test]
    fn w27_r08_relays_two_relays_carry_one_statement_end_to_end() -> anyhow::Result<()> {
        let ca = ca()?;
        let (agent, runner, server, fedora) = (Signer::new(&ca, AGENT)?, Signer::new(&ca, ADACACHY)?, Signer::new(&ca, SERVER)?, Signer::new(&ca, FEDORA)?);
        let a = act(&ca, &agent, br#"{"op":"call","name":"agents.send","input":{"agent":"01a18aaa","text":"the build is green"}}"#)?;
        let p1 = runner.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { by: ADACACHY, from: AGENT, to: SERVER, stream: "calls", seq: 1, why: "a call" }, vec![the_act(&a)], vec![a.digest.clone()]))?;
        let p2 = server.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { by: SERVER, from: ADACACHY, to: FEDORA, stream: "words to fedora-2", seq: 1, why: "an agent's words" }, vec![the_act(&a)], vec![p1.digest.clone()]))?;
        relayed(&p2, SERVER, FEDORA, &the_act(&a), true)?;
        let got = fedora.signed(&ca, PAYLOAD_CUSTODY, &receipt(FEDORA, SERVER, vec![the_act(&a)], &p2.digest, "an agent's words"))?;
        let records: BTreeMap<String, Checked> = [a, p1.clone(), p2.clone(), got.clone()].into_iter().map(|c| (c.digest.clone(), c)).collect();
        let w = walk(&got.digest, &records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| (h.kind, h.custodian.as_str())).collect::<Vec<_>>(), vec![(Kind::Received, FEDORA), (Kind::PassedOn, SERVER), (Kind::PassedOn, ADACACHY)]);
        assert_eq!(w.began, vec![p1.digest.clone()]);
        assert!(custody::unanswered(&records).is_empty(), "every record answered");
        // Without the receipt, the server's record is a hand-on with no receipt downstream.
        let mut open = records.clone();
        open.remove(&got.digest);
        assert_eq!(custody::unanswered(&open), vec![p2.digest]);
        Ok(())
    }

    /// Each way a relay can fail what it carries is caught where it arrives, naming the relay: a
    /// record someone else signed, one for another receiver, an altered thing, another thing
    /// swapped in, a number skipped and a number again.
    #[test]
    fn w27_r08_relays_that_forge_alter_swap_or_drop_are_caught_naming_the_relay() -> anyhow::Result<()> {
        let ca = ca()?;
        let (agent, runner, fedora) = (Signer::new(&ca, AGENT)?, Signer::new(&ca, ADACACHY)?, Signer::new(&ca, FEDORA)?);
        let a = act(&ca, &agent, b"ask one")?;
        let b = act(&ca, &agent, b"ask two")?;
        let pass = Pass { by: ADACACHY, from: AGENT, to: SERVER, stream: "calls", seq: 1, why: "a call" };
        let p = runner.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&pass, vec![the_act(&a)], vec![a.digest.clone()]))?;
        // Signed by another than the relay it came through.
        let forged = fedora.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { by: FEDORA, ..pass }, vec![the_act(&a)], vec![a.digest.clone()]))?;
        let r = relayed(&forged, ADACACHY, SERVER, &the_act(&a), true);
        assert!(matches!(&r, Err(Unrelayed::Forged { signer, .. }) if signer == FEDORA), "{r:?}");
        // Passed on to another.
        let r = relayed(&p, ADACACHY, FEDORA, &the_act(&a), true);
        assert!(matches!(&r, Err(Unrelayed::NotHere { to, .. }) if to == SERVER), "{r:?}");
        // Another genuine act swapped in under its record.
        let r = relayed(&p, ADACACHY, SERVER, &the_act(&b), true);
        assert!(matches!(&r, Err(Unrelayed::Swapped { .. })), "{r:?}");
        assert_eq!(r.as_ref().map_err(Unrelayed::relay).err(), Some(ADACACHY));
        // Altered on the way: what arrived isn't its originator's any more.
        let r = relayed(&p, ADACACHY, SERVER, &the_act(&b), false);
        assert!(matches!(&r, Err(Unrelayed::Changed { algorithm, was, .. }) if algorithm == "sha256" && was == a.digest.trim_start_matches("sha256:")), "{r:?}");
        assert!(r.is_err_and(|e| e.code() == "changed" && e.to_string().starts_with(&format!("{ADACACHY} changed"))));
        // Not a passed-on at all, and numbered on no stream.
        let handed = runner.signed(&ca, PAYLOAD_CUSTODY, &Statement { subject: vec![the_act(&a)], ..Statement::new(custody::to_value(&Body { to: Some(SERVER.into()), ..Body::new(Kind::HandedOn, "") })) })?;
        assert!(matches!(relayed(&handed, ADACACHY, SERVER, &the_act(&a), true), Err(Unrelayed::Unreadable { .. })));
        let mut unnumbered = passed_on(&pass, vec![the_act(&a)], vec![a.digest.clone()]);
        (unnumbered.stream, unnumbered.seq) = (None, None);
        let unnumbered = runner.signed(&ca, PAYLOAD_CUSTODY, &unnumbered)?;
        assert!(matches!(relayed(&unnumbered, ADACACHY, SERVER, &the_act(&a), true), Err(Unrelayed::Unnumbered { .. })));
        // Numbers: 1, 2, then 4 (3 dropped), then 2 again (sent again).
        let mut n = Numbering::default();
        assert_eq!(n.take(ADACACHY, "calls", 1)?, Place::Next);
        assert_eq!(n.take(ADACACHY, "calls", 2)?, Place::Next);
        let gap = n.take(ADACACHY, "calls", 4)?;
        assert!(matches!(&gap, Place::AfterGap(Unrelayed::Dropped { from: 3, to: 3, .. })), "{gap:?}");
        if let Place::AfterGap(d) = gap {
            assert_eq!(d.to_string(), format!("{ADACACHY}'s record 3 on calls never came: dropped on the way"));
        }
        assert!(matches!(n.take(ADACACHY, "calls", 2), Err(Unrelayed::Again { seq: 2, .. })));
        assert_eq!(n.next(ADACACHY, "calls"), Some(5));
        // Another stream of the same relay starts at 1; a stream that ended is forgotten.
        assert!(matches!(n.take(ADACACHY, "calls again", 3)?, Place::AfterGap(Unrelayed::Dropped { from: 1, to: 2, .. })));
        n.forget(ADACACHY, "calls");
        assert_eq!(n.next(ADACACHY, "calls"), None);
        Ok(())
    }

    /// A run of frames a relay can't read (a reach link's ciphertext): the relay's chain and the
    /// receiver's agree only on the same frames, in the same order.
    #[test]
    fn w27_r08_relays_a_run_of_frames_is_named_by_its_chain() -> anyhow::Result<()> {
        let ca = ca()?;
        let server = Signer::new(&ca, SERVER)?;
        let frames: Vec<Vec<u8>> = (0u8..5).map(|i| vec![i; 40]).collect();
        let chain_of = |fs: &[Vec<u8>]| {
            let mut f = Frames::new(1);
            for x in fs {
                f.push(x);
            }
            f
        };
        let sent = chain_of(&frames);
        assert_eq!((sent.first(), sent.count(), sent.next()), (1, 5, 6));
        let name = "frames 1 to 5 of reach link L1 from mac";
        let p = server.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { by: SERVER, from: "kiln://hyper-light/person/ada/machine/mac/instance/main", to: FEDORA, stream: "reach link L1 to fedora-2", seq: 1, why: "a reach link's frames" }, vec![sent.thing(name)], Vec::new()))?;
        assert_eq!(frames_of(p.statement.subject.first().ok_or_else(|| anyhow::anyhow!("no thing"))?), Some((1, 5)));
        relayed(&p, SERVER, FEDORA, &chain_of(&frames).thing(name), false)?;
        let mut swapped = frames.clone();
        swapped.swap(1, 2);
        let mut changed = frames.clone();
        if let Some(b) = changed.get_mut(3).and_then(|f| f.first_mut()) {
            *b ^= 1;
        }
        let dropped: Vec<Vec<u8>> = frames.iter().enumerate().filter(|(i, _)| *i != 2).map(|(_, f)| f.clone()).collect();
        for (what, got) in [("swapped", swapped), ("changed", changed), ("dropped", dropped)] {
            let r = relayed(&p, SERVER, FEDORA, &chain_of(&got).thing(name), false);
            assert!(matches!(&r, Err(Unrelayed::Changed { algorithm, .. }) if algorithm == FRAMES), "{what}: {r:?}");
        }
        // A passed-on of what its sender signs nothing of begins its chain at the relay.
        let records: BTreeMap<String, Checked> = [(p.digest.clone(), p.clone())].into();
        let w = walk(&p.digest, &records, SERVER)?;
        assert_eq!(w.began, vec![p.digest.clone()]);
        Ok(())
    }

    /// Blue/green's forwarder: the old build passes a frame of the server's on to the new one,
    /// numbered on its stream to it; the new build's receipt follows. A frame altered on the way
    /// differs by its digest; the green it wasn't for refuses it.
    #[test]
    fn w27_r08_relays_blue_passes_a_servers_frame_on_to_green() -> anyhow::Result<()> {
        let ca = ca()?;
        let (blue, green) = (Signer::new(&ca, ADACACHY)?, Signer::new(&ca, GREEN)?);
        let frame = b"a server's frame about session 01a1751c";
        let thing = bytes_thing("a frame of kiln's server", frame);
        let p = blue.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { by: ADACACHY, from: SERVER, to: GREEN, stream: "forwarded to 01a19000", seq: 1, why: "a frame of kiln's server, forwarded to the new build" }, vec![thing.clone()], Vec::new()))?;
        relayed(&p, ADACACHY, GREEN, &thing, false)?;
        let mut altered = frame.to_vec();
        if let Some(b) = altered.last_mut() {
            *b ^= 1;
        }
        let r = relayed(&p, ADACACHY, GREEN, &bytes_thing("a frame of kiln's server", &altered), false);
        assert!(matches!(&r, Err(Unrelayed::Changed { .. })), "{r:?}");
        assert!(matches!(relayed(&p, ADACACHY, FEDORA, &thing, false), Err(Unrelayed::NotHere { .. })));
        let got = green.signed(&ca, PAYLOAD_CUSTODY, &receipt(GREEN, ADACACHY, vec![thing], &p.digest, "a frame of kiln's server"))?;
        let records: BTreeMap<String, Checked> = [p.clone(), got.clone()].into_iter().map(|c| (c.digest.clone(), c)).collect();
        let w = walk(&got.digest, &records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| (h.kind, h.custodian.as_str())).collect::<Vec<_>>(), vec![(Kind::Received, GREEN), (Kind::PassedOn, ADACACHY)]);
        // A frame carrying a custody record of kiln's (work it hands a session): blue's record is of
        // that record itself, by its digest, following it; the walk goes back through it.
        let server = Signer::new(&ca, SERVER)?;
        let work = Subject { name: "plan run r1 step 2".into(), digest: BTreeMap::from([("sha256".to_owned(), "d".repeat(64))]), ..Subject::default() };
        let hand_on = server.signed(&ca, PAYLOAD_CUSTODY, &Statement { subject: vec![work], ..Statement::new(custody::to_value(&Body { to: Some(AGENT.into()), ..Body::new(Kind::HandedOn, "") })) })?;
        let it = statement_thing("a statement of kiln's, forwarded", &hand_on.digest);
        let p = blue.signed(&ca, PAYLOAD_CUSTODY, &passed_on(&Pass { by: ADACACHY, from: SERVER, to: GREEN, stream: "statements forwarded to 01a19000", seq: 1, why: "a statement of kiln's, forwarded" }, vec![it.clone()], vec![hand_on.digest.clone()]))?;
        relayed(&p, ADACACHY, GREEN, &it, true)?;
        let got = green.signed(&ca, PAYLOAD_CUSTODY, &receipt(GREEN, ADACACHY, vec![it], &p.digest, "a statement of kiln's, forwarded"))?;
        let records: BTreeMap<String, Checked> = [hand_on.clone(), p, got.clone()].into_iter().map(|c| (c.digest.clone(), c)).collect();
        let w = walk(&got.digest, &records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| (h.kind, h.custodian.as_str())).collect::<Vec<_>>(), vec![(Kind::Received, GREEN), (Kind::PassedOn, ADACACHY), (Kind::HandedOn, SERVER)]);
        let unrecorded = Unrelayed::Unrecorded { relay: ADACACHY.into(), what: "a statement of kiln's".into() };
        assert_eq!((unrecorded.code(), unrecorded.relay()), ("unrecorded", ADACACHY));
        Ok(())
    }
}
