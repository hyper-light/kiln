//! Custody, checked purely (docs/ci.md W27-R08; the Transfers plan's step 1, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): what a custodian's record of a thing says, whether a thing's
//! chain of custody holds hop by hop, whether a signer acting under delegations may do what it
//! does, and whether an act of an agent session comes from the process that holds it at its epoch.
//!
//! - **A custody record** is a signed statement ([`statement`]) of payload type
//!   [`PAYLOAD_CUSTODY`]. Its subjects are the things it's about, by their digests; its body
//!   ([`Body`]) says what happened to them. Handed on: its giver names whom to. Received: its taker
//!   names whom from, following the record of the hand-on it answers. Passed on: a relay held it
//!   and passed it on unchanged, naming both. Taken: kiln's server hands it on for a giver that
//!   can't sign (its machine away, a change commandeered), naming that giver and saying why.
//! - **The walk** ([`walk`]) goes back from a record, where a thing is used, along what each
//!   record follows, to where each thing began. Each hop must hold. A receipt answers a hand-on to
//!   its signer, from whom it says, about the same things with the same digests (in-toto's digest
//!   sets: two records agree on every algorithm they share, and share one). A hand-on hands on
//!   what its signer received, or what it made from what it received. A take is kiln's server's.
//!   No custodian's numbering of its streams has a gap. The first hop that doesn't hold is named.
//! - **Entitlement** ([`entitled`]): a signer acting under delegations may do what the nearest one
//!   gives it, if each gives no more and lasts no longer than the one it acts under, and none has
//!   ended or been revoked.
//! - **The fence** ([`fence`]): an agent session's act counts only if it's signed at the session's
//!   epoch by the runner process holding it then; one signed at an epoch it has left is a copy left
//!   behind, refused saying when the session went on and where.
//!
//! No clock, disk or network: the times, the records and the revocations are arguments.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::chain::{self, Actor, Holder, Identity};
use super::statement::{self, Checked, Gap, PAYLOAD_CUSTODY, PAYLOAD_DELEGATION, Subject, Under};

/// The version of a custody record's body, and of a delegation's, kiln writes and reads.
pub const VERSION: u32 = 1;

/// What a custody record says happened to its things.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Its signer handed them on, to [`Body::to`].
    HandedOn,
    /// Its signer received them, from [`Body::from`].
    Received,
    /// Its signer, a relay, held them and passed them on unchanged, from [`Body::from`] to
    /// [`Body::to`].
    PassedOn,
    /// kiln's server handed them on, to [`Body::to`], for a giver that can't sign
    /// ([`Body::taken_for`]).
    Taken,
}

impl Kind {
    /// It hands its things to someone.
    fn gives(self) -> bool {
        matches!(self, Self::HandedOn | Self::PassedOn | Self::Taken)
    }

    /// It takes its things from someone, following a giver's record.
    fn takes(self) -> bool {
        matches!(self, Self::Received | Self::PassedOn)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HandedOn => "handed on",
            Self::Received => "received",
            Self::PassedOn => "passed on",
            Self::Taken => "taken",
        })
    }
}

/// A custody record's body ([`PAYLOAD_CUSTODY`]): what happened, between whom, and why. The things
/// themselves are the statement's subjects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    /// [`VERSION`].
    pub v: u32,
    pub kind: Kind,
    /// Its signer (an identity's URI), as its certificate names it. A statement's digest is the
    /// same whoever signs it, so a record names its signer itself: two custodians' records of
    /// one hop are two records.
    #[serde(default)]
    pub by: String,
    /// Whom they go to (an identity's URI): handed on, passed on, taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Whom they came from: received, passed on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Whom its signer holds them for, when not itself: a session, a person.
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub held_for: Option<String>,
    /// What for: "leg 812/1's checkout", "deploy kiln-production".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// The part of its signer that holds them: kiln's registry, its cache, a runner's mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// A take's: the giver that couldn't sign, whose custody kiln's server hands on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_for: Option<String>,
    /// An agent session's transfer: the epoch it goes on at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    /// When its signer says it happened (unix seconds). Said, never judged by: kiln judges a record
    /// as of when it took it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub said_at: Option<i64>,
}

impl Body {
    /// A record of `kind` signed `by` (an identity's URI), between no one yet.
    pub fn new(kind: Kind, by: &str) -> Self {
        Self { v: VERSION, kind, by: by.to_owned(), to: None, from: None, held_for: None, why: String::new(), component: None, taken_for: None, epoch: None, said_at: None }
    }

    /// Whether it's well formed, or why not.
    fn check(&self) -> Result<(), String> {
        if self.v != VERSION {
            return Err(format!("it's version {} of kiln's custody records; this kiln reads version {VERSION}", self.v));
        }
        let named = |who: &Option<String>| who.as_deref().is_some_and(|w| !w.is_empty());
        if self.kind.gives() && !named(&self.to) {
            return Err(format!("it says they were {} but not to whom", self.kind));
        }
        if self.kind.takes() && !named(&self.from) {
            return Err(format!("it says they were {} but not from whom", self.kind));
        }
        match (self.kind, &self.taken_for) {
            (Kind::Taken, who) if !named(who) => Err("it's a take but names no giver it takes for".into()),
            (Kind::Taken, _) | (_, None) => Ok(()),
            (kind, Some(_)) => Err(format!("it says they were {kind}, and only a take names a giver it takes for")),
        }
    }
}

/// A custody record's body, from a statement kiln took: refused if it isn't a custody record, or
/// isn't one kiln reads.
pub fn body(c: &Checked) -> Result<Body, Break> {
    if c.payload_type != PAYLOAD_CUSTODY {
        return Err(Break::NotCustody { record: c.digest.clone(), payload_type: c.payload_type.clone() });
    }
    let unreadable = |why: String| Break::Unreadable { record: c.digest.clone(), why };
    let b: Body = serde_json::from_value(c.statement.body.clone()).map_err(|e| unreadable(e.to_string()))?;
    b.check().map_err(unreadable)?;
    if b.by != c.custodian {
        return Err(unreadable(format!("it says it's by {}, but {} signed it", if b.by.is_empty() { "no one" } else { &b.by }, c.custodian)));
    }
    if c.statement.subject.is_empty() {
        return Err(unreadable("it's about nothing: a custody record names what it's about".into()));
    }
    Ok(b)
}

/// A delegation's body ([`PAYLOAD_DELEGATION`]): who may do what, until when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// [`VERSION`].
    pub v: u32,
    /// Its signer, the delegator, as its certificate names it (as [`Body::by`]).
    #[serde(default)]
    pub by: String,
    /// Whom it entitles (an identity's URI): the signer of what acts under it.
    pub delegate: String,
    /// What it entitles them to: scopes such as `receive:workspace` or `act:goals.done`;
    /// `receive:*` is every scope of its kind, `*` every scope.
    pub may: Vec<String>,
    /// When it ends (unix seconds); none, when what it acts under ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

impl Grant {
    /// Whether it's well formed, or why not.
    fn check(&self) -> Result<(), String> {
        if self.v != VERSION {
            return Err(format!("it's version {} of kiln's delegations; this kiln reads version {VERSION}", self.v));
        }
        if self.delegate.is_empty() {
            return Err("it names no delegate".into());
        }
        if self.may.is_empty() || self.may.iter().any(String::is_empty) {
            return Err("it entitles its delegate to nothing it names".into());
        }
        Ok(())
    }
}

/// A delegation's body, from a statement kiln took: refused if it isn't a delegation, or isn't one
/// kiln reads.
pub fn grant(c: &Checked) -> Result<Grant, String> {
    if c.payload_type != PAYLOAD_DELEGATION {
        return Err(format!("it's a {} statement, not a delegation", c.payload_type));
    }
    let g: Grant = serde_json::from_value(c.statement.body.clone()).map_err(|e| e.to_string())?;
    g.check()?;
    if g.by != c.custodian {
        return Err(format!("it says it's by {}, but {} signed it", if g.by.is_empty() { "no one" } else { &g.by }, c.custodian));
    }
    Ok(g)
}

/// Whether scope `granted` covers `wanted`: the same scope, `kind:*` for every scope of its kind,
/// or `*` for every scope.
fn covers(granted: &str, wanted: &str) -> bool {
    granted == "*" || granted == wanted || granted.strip_suffix(":*").is_some_and(|kind| wanted.split_once(':').is_some_and(|(k, _)| k == kind))
}

/// A delegation revoked by its digest, from `at` (unix seconds) on, and why. kiln revokes the
/// delegations an agent session signed at an epoch when it goes on at the next: a certificate
/// revoked later doesn't undo a delegation by itself ([`statement::verify_under`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revoked {
    pub digest: String,
    pub at: i64,
    pub why: String,
}

/// In whose right a signer does what it does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entitled {
    /// Its own: it acts under no delegation.
    Own,
    /// Under delegations, the furthest signed by `by`, through `links` of them.
    Delegated { by: String, links: usize },
}

/// Whether the signer of `under`'s statement may do `wanted` as of `at`: in its own right when it
/// acts under no delegation; else if the nearest delegation gives it `wanted`, each gives no
/// scope the one it acts under doesn't and lasts no longer, and none has ended or been revoked as
/// of `at`.
pub fn entitled(under: &Under, wanted: &str, at: i64, revoked: &[Revoked]) -> Result<Entitled, Refusal> {
    let mut grants: Vec<(&Checked, Grant)> = Vec::new();
    for (i, link) in under.through.iter().enumerate() {
        let n = i + 1;
        let g = grant(link).map_err(|why| Refusal::Unreadable { n, link: link.digest.clone(), why })?;
        if let Some(r) = revoked.iter().find(|r| r.digest == link.digest && r.at <= at) {
            return Err(Refusal::Revoked { n, link: link.digest.clone(), at: chain::when(r.at), why: r.why.clone() });
        }
        if let Some(until) = g.until.filter(|u| *u < at) {
            return Err(Refusal::Expired { n, link: link.digest.clone(), until: chain::when(until) });
        }
        grants.push((link, g));
    }
    let Some((nearest, first)) = grants.first() else { return Ok(Entitled::Own) };
    if !first.may.iter().any(|g| covers(g, wanted)) {
        return Err(Refusal::NotEntitled { n: 1, link: nearest.digest.clone(), wanted: wanted.to_owned() });
    }
    for (i, pair) in grants.windows(2).enumerate() {
        let [(link, g), (_, above)] = pair else { continue };
        let n = i + 1;
        if let Some(scope) = g.may.iter().find(|s| !above.may.iter().any(|a| covers(a, s))) {
            return Err(Refusal::Widened { n, link: link.digest.clone(), scope: scope.clone() });
        }
        if let Some(ends) = above.until
            && g.until.is_none_or(|u| u > ends)
        {
            return Err(Refusal::Lengthened { n, link: link.digest.clone(), until: chain::when(ends) });
        }
    }
    let by = grants.last().map(|(c, _)| c.custodian.clone()).unwrap_or_default();
    Ok(Entitled::Delegated { by, links: grants.len() })
}

/// Why a signer acting under delegations may not do what it does.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("delegation {n} ({link}) isn't one kiln reads: {why}")]
    Unreadable { n: usize, link: String, why: String },
    #[error("delegation {n} ({link}) doesn't entitle its delegate to {wanted}")]
    NotEntitled { n: usize, link: String, wanted: String },
    #[error("delegation {n} ({link}) gives {scope}, which the delegation it acts under doesn't")]
    Widened { n: usize, link: String, scope: String },
    #[error("delegation {n} ({link}) lasts past {until}, when the delegation it acts under ends")]
    Lengthened { n: usize, link: String, until: String },
    #[error("delegation {n} ({link}) ended at {until}")]
    Expired { n: usize, link: String, until: String },
    #[error("delegation {n} ({link}) was revoked at {at}: {why}")]
    Revoked { n: usize, link: String, at: String, why: String },
}

/// One record on a thing's chain of custody, as the walk found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    /// The record's digest.
    pub record: String,
    pub kind: Kind,
    /// Its signer.
    pub custodian: String,
    /// When kiln took it.
    pub at: i64,
    pub subject: Vec<Subject>,
    /// For things made here: the receipts, by the same custodian, of what they were made from.
    pub made_from: Vec<String>,
    /// A thing began here: a hand-on or a take that follows no record of its things.
    pub began: bool,
}

/// A chain of custody that holds: each hop from where the walk began back to where each thing
/// began, each record once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Walked {
    pub hops: Vec<Hop>,
    /// The records where things began: hand-ons of what their signers made, or first held, which
    /// follow no record of the same things.
    pub began: Vec<String>,
}

/// Where a chain of custody doesn't hold.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Break {
    #[error("kiln has no record {record}{}", followed_by.as_ref().map(|r| format!(", which {r} follows")).unwrap_or_default())]
    NoRecord { record: String, followed_by: Option<String> },
    #[error("{record} is a {payload_type} statement, not a custody record")]
    NotCustody { record: String, payload_type: String },
    #[error("{record} isn't a custody record kiln reads: {why}")]
    Unreadable { record: String, why: String },
    #[error("{thing} changed between {from} and {record}: its {} was {}, and is {}", digest.algorithm, digest.was, digest.now)]
    Changed { record: String, from: String, thing: String, digest: Box<Changed> },
    #[error("{record} and {from} name {thing} by no digest they share")]
    NoCommonDigest { record: String, from: String, thing: String },
    #[error("{record} received {thing}, which {from} didn't hand on")]
    NotHandedOn { record: String, from: String, thing: String },
    #[error("{record} received less than {from} handed on: {} didn't arrive", missing.join(", "))]
    NotAll { record: String, from: String, missing: Vec<String> },
    #[error("{from} handed them on to {to}, not to {signer}, who signed {record}")]
    NotTheTaker { record: String, from: String, to: String, signer: String },
    #[error("{record} says they came from {says}, but {giver} handed them on ({from})")]
    NotTheGiver { record: String, from: String, says: String, giver: String },
    #[error("{record} hands on what {holder} held ({from}), but {signer} signed it")]
    NotTheHolder { record: String, from: String, holder: String, signer: String },
    #[error("{record} says they were {kind}, but follows no record of them being handed on")]
    NoHandOn { record: String, kind: Kind },
    #[error("{record} ({kind}) can't follow {from} ({was})")]
    OutOfOrder { record: String, kind: Kind, from: String, was: Kind },
    #[error("{record} is a take, signed by {signer}, not kiln's server ({server})")]
    TakenNotByServer { record: String, signer: String, server: String },
    #[error("{0}")]
    Gap(Gap),
}

impl Break {
    /// Its kind, as kiln's records name it.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoRecord { .. } => "no-record",
            Self::NotCustody { .. } => "not-custody",
            Self::Unreadable { .. } => "unreadable",
            Self::Changed { .. } => "changed",
            Self::NoCommonDigest { .. } => "no-common-digest",
            Self::NotHandedOn { .. } => "not-handed-on",
            Self::NotAll { .. } => "not-all",
            Self::NotTheTaker { .. } => "not-the-taker",
            Self::NotTheGiver { .. } => "not-the-giver",
            Self::NotTheHolder { .. } => "not-the-holder",
            Self::NoHandOn { .. } => "no-hand-on",
            Self::OutOfOrder { .. } => "out-of-order",
            Self::TakenNotByServer { .. } => "taken-not-by-server",
            Self::Gap(_) => "gap",
        }
    }
}

/// A digest that changed across a hop: its algorithm, what it was, and what it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed {
    pub algorithm: String,
    pub was: String,
    pub now: String,
}

/// How two records' things compare.
enum Compared {
    /// Each thing of one, the same thing (by name, else by a digest) in the other, or none.
    Matched { same: usize, only_here: Vec<String>, only_there: Vec<String> },
    /// The same thing, by name, with a digest that differs.
    Changed { thing: String, algorithm: String, was: String, now: String },
    /// The same thing, by name, with no digest both name.
    NoCommon { thing: String },
}

/// The digests two things both name: each algorithm, with its value in `a` and in `b`.
fn shared<'a>(a: &'a Subject, b: &'a Subject) -> Vec<(&'a String, &'a String, &'a String)> {
    a.digest.iter().filter_map(|(alg, hex)| b.digest.get(alg).map(|other| (alg, hex, other))).collect()
}

/// `here`'s things against `there`'s: a thing is the same thing by its name, else by a digest the
/// two agree on; the same thing must agree on every algorithm both name, and both must name one.
fn compare(here: &[Subject], there: &[Subject]) -> Compared {
    let mut matched: BTreeSet<usize> = BTreeSet::new();
    let mut only_here = Vec::new();
    for h in here {
        let by_name = there.iter().position(|t| t.name == h.name);
        let found = by_name.or_else(|| there.iter().position(|t| shared(h, t).iter().any(|(_, a, b)| a == b)));
        let Some(i) = found else {
            only_here.push(h.name.clone());
            continue;
        };
        let Some(t) = there.get(i) else { continue };
        let both = shared(h, t);
        if let Some((alg, now, was)) = both.iter().find(|(_, a, b)| a != b) {
            return Compared::Changed { thing: h.name.clone(), algorithm: (*alg).clone(), was: (*was).clone(), now: (*now).clone() };
        }
        if both.is_empty() {
            return Compared::NoCommon { thing: h.name.clone() };
        }
        matched.insert(i);
    }
    let only_there = there.iter().enumerate().filter(|(i, _)| !matched.contains(i)).map(|(_, t)| t.name.clone()).collect();
    Compared::Matched { same: matched.len(), only_here, only_there }
}

/// How a record follows another.
enum Edge {
    /// It takes what the other gave it.
    Transfer,
    /// It hands on what its signer received.
    Holding,
    /// It hands on what its signer made from what it received.
    MadeFrom,
}

/// How record `r` (body `rb`) follows record `p` (body `pb`), or the break between them.
fn edge(r: &Checked, rb: &Body, p: &Checked, pb: &Body, server: &str) -> Result<Edge, Break> {
    let (record, from) = (r.digest.clone(), p.digest.clone());
    let compared = compare(&r.statement.subject, &p.statement.subject);
    let same = match compared {
        Compared::Changed { thing, algorithm, was, now } => return Err(Break::Changed { record, from, thing, digest: Box::new(Changed { algorithm, was, now }) }),
        Compared::NoCommon { thing } => return Err(Break::NoCommonDigest { record, from, thing }),
        Compared::Matched { same, only_here, only_there } => (same, only_here, only_there),
    };
    let out_of_order = || Break::OutOfOrder { record: r.digest.clone(), kind: rb.kind, from: p.digest.clone(), was: pb.kind };
    if rb.kind.takes() {
        if !pb.kind.gives() {
            return Err(out_of_order());
        }
        let (_, only_here, only_there) = same;
        if let Some(thing) = only_here.into_iter().next() {
            return Err(Break::NotHandedOn { record, from, thing });
        }
        if !only_there.is_empty() {
            return Err(Break::NotAll { record, from, missing: only_there });
        }
        if pb.kind == Kind::Taken && p.custodian != server {
            return Err(Break::TakenNotByServer { record: from, signer: p.custodian.clone(), server: server.to_owned() });
        }
        let to = pb.to.clone().unwrap_or_default();
        if to != r.custodian {
            return Err(Break::NotTheTaker { record, from, to, signer: r.custodian.clone() });
        }
        let giver = if pb.kind == Kind::Taken { pb.taken_for.clone().unwrap_or_default() } else { p.custodian.clone() };
        let says = rb.from.clone().unwrap_or_default();
        if says != giver {
            return Err(Break::NotTheGiver { record, from, says, giver });
        }
        return Ok(Edge::Transfer);
    }
    // A hand-on or a take: of what its holder received, or made from it.
    if !pb.kind.takes() {
        return Err(out_of_order());
    }
    let holder = if rb.kind == Kind::Taken { rb.taken_for.clone().unwrap_or_default() } else { r.custodian.clone() };
    if p.custodian != holder {
        return Err(Break::NotTheHolder { record, from, holder: p.custodian.clone(), signer: holder });
    }
    Ok(if same.0 > 0 { Edge::Holding } else { Edge::MadeFrom })
}

/// Checks record `record`'s own hop: each record it follows, which `records` must hold, followed as
/// [`walk`] says it may be, and a receipt or a pass following a hand-on. Not the records before
/// those, nor its custodian's numbering: that's [`walk`]'s. What kiln checks as it takes a record.
pub fn hop(record: &str, records: &BTreeMap<String, Checked>, server: &str) -> Result<Hop, Break> {
    let r = records.get(record).ok_or_else(|| Break::NoRecord { record: record.to_owned(), followed_by: None })?;
    let rb = body(r)?;
    let (mut gave, mut held, mut made_from) = (false, false, Vec::new());
    for f in &r.statement.follows {
        let p = records.get(f).ok_or_else(|| Break::NoRecord { record: f.clone(), followed_by: Some(record.to_owned()) })?;
        let pb = body(p)?;
        match edge(r, &rb, p, &pb, server)? {
            Edge::Transfer => gave = true,
            Edge::Holding => held = true,
            Edge::MadeFrom => made_from.push(f.clone()),
        }
    }
    if rb.kind.takes() && !gave {
        return Err(Break::NoHandOn { record: record.to_owned(), kind: rb.kind });
    }
    let began = !rb.kind.takes() && !held;
    Ok(Hop { record: record.to_owned(), kind: rb.kind, custodian: r.custodian.clone(), at: r.at, subject: r.statement.subject.clone(), made_from, began })
}

/// Walks a thing's chain of custody back from record `from` (by its digest), where it's used,
/// through what each record follows, to where each thing began. `records` are the custody records
/// kiln took, by digest: every record the chain follows, and every record kiln has on the streams
/// of the chain's custodians, whose numbering must have no gap. `server` is kiln's server's
/// identity (its URI), which alone signs takes. The chain, or its first break.
pub fn walk(from: &str, records: &BTreeMap<String, Checked>, server: &str) -> Result<Walked, Break> {
    let mut walked = Walked::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut next = vec![from.to_owned()];
    while let Some(d) = next.pop() {
        if !seen.insert(d.clone()) {
            continue;
        }
        let h = hop(&d, records, server)?;
        if let Some(r) = records.get(&d) {
            next.extend(r.statement.follows.iter().cloned());
        }
        if h.began {
            walked.began.push(d);
        }
        walked.hops.push(h);
    }
    let streams: BTreeSet<(&str, &str)> = walked
        .hops
        .iter()
        .filter_map(|h| records.get(&h.record))
        .filter_map(|c| c.statement.stream.as_deref().map(|s| (c.custodian.as_str(), s)))
        .collect();
    let numbered = records.values().filter(|c| c.statement.stream.as_deref().is_some_and(|s| streams.contains(&(c.custodian.as_str(), s))));
    if let Some(gap) = statement::gaps(numbered).into_iter().next() {
        return Err(Break::Gap(gap));
    }
    Ok(walked)
}

/// The records among `records` that hand things on with no record answering them: in transit, or
/// dropped on the way.
pub fn unanswered(records: &BTreeMap<String, Checked>) -> Vec<String> {
    let answered: BTreeSet<&str> = records
        .values()
        .filter(|c| body(c).is_ok_and(|b| b.kind.takes()))
        .flat_map(|c| c.statement.follows.iter().map(String::as_str))
        .collect();
    records.values().filter(|c| body(c).is_ok_and(|b| b.kind.gives()) && !answered.contains(c.digest.as_str())).map(|c| c.digest.clone()).collect()
}

/// Who holds an agent session now: its epoch, the runner process holding it then, and the transfer
/// that gave it to them (kiln's record of it, and when kiln took it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub session: String,
    pub epoch: u64,
    pub by: Option<Holder>,
    pub since: i64,
    pub transfer: String,
}

/// Why an act signed as an agent session doesn't count.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fenced {
    #[error("it's signed by {signer}, not an agent session: only the session's holder acts as it")]
    NotAnAgent { signer: String },
    #[error("it's signed as agent {agent}, not session {session}")]
    AnotherSession { session: String, agent: String },
    #[error("it was signed at epoch {signed} of session {session}, which went on at epoch {current}{to} at {since} ({transfer}): it's from a copy left behind")]
    Left { session: String, signed: u64, current: u64, to: String, since: String, transfer: String },
    #[error("it's signed at epoch {signed} of session {session}, which kiln has at epoch {current}: no transfer to epoch {signed} is recorded")]
    Ahead { session: String, signed: u64, current: u64 },
    #[error("it's signed at epoch {epoch} of session {session}{signed_by}, but the session is held{holder}")]
    NotTheHolder { session: String, epoch: u64, signed_by: String, holder: String },
}

/// ", on runner fedora-2 (instance 01a174ed)", or "" for no holder.
fn on(h: Option<&Holder>) -> String {
    h.map(|h| format!(", on runner {} (instance {})", h.machine, h.instance)).unwrap_or_default()
}

/// Whether an act signed by `signer` counts as agent session `held.session`'s: signed at the epoch
/// it's at, by the runner process holding it then. An act signed at an epoch it has left comes
/// from a copy left behind; one at an epoch kiln has no transfer to, or by another process of its
/// epoch (a certificate naming no holder among them), doesn't count either.
pub fn fence(held: &Held, signer: &Identity) -> Result<(), Fenced> {
    let Actor::Agent { agent, epoch, held_by, .. } = &signer.actor else {
        return Err(Fenced::NotAnAgent { signer: signer.to_string() });
    };
    if *agent != held.session {
        return Err(Fenced::AnotherSession { session: held.session.clone(), agent: agent.clone() });
    }
    if *epoch < held.epoch {
        return Err(Fenced::Left { session: held.session.clone(), signed: *epoch, current: held.epoch, to: on(held.by.as_ref()), since: chain::when(held.since), transfer: held.transfer.clone() });
    }
    if *epoch > held.epoch {
        return Err(Fenced::Ahead { session: held.session.clone(), signed: *epoch, current: held.epoch });
    }
    if held.by.is_some() && *held_by != held.by {
        return Err(Fenced::NotTheHolder { session: held.session.clone(), epoch: *epoch, signed_by: on(held_by.as_ref()), holder: on(held.by.as_ref()) });
    }
    Ok(())
}

/// A custody body as JSON, for a [`statement::Statement`]'s body.
pub fn to_value(b: &Body) -> Value {
    serde_json::to_value(b).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::statement::{PAYLOAD_CUSTODY, PAYLOAD_DELEGATION, Statement, bundle, digest, payload, to_sign, verify, verify_under};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData as _, SanType, SerialNumber, SigningKey as _, date_time_ymd};
    use serde_json::json;
    use sha2::{Digest as _, Sha256};

    /// 2026-09-21T14:13:20Z: inside every test leaf's validity (2026-01-01 to 2027-01-01).
    const AT: i64 = 1_790_000_000;

    const SERVER: &str = "kiln://hyper-light/server";
    const CD: &str = "kiln://hyper-light/server/cd";
    const FEDORA: &str = "kiln://hyper-light/person/ada/machine/fedora-2/instance/01a174ed";
    const JUPITER: &str = "kiln://hyper-light/person/ada/machine/jupiter/instance/01a18b00";
    const SAM: &str = "kiln://hyper-light/person/sam";
    const AGENT: &str = "kiln://hyper-light/person/sam/agent/matrix/epoch/3/machine/adacachy/instance/01a14794";
    const LEFT: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/0/machine/fedora/instance/01a14c4f";
    const MOVED: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1/machine/adacachy/instance/01a14794";

    struct Ca {
        der: Vec<u8>,
        issuer: Issuer<'static, KeyPair>,
    }

    fn ca() -> anyhow::Result<Ca> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, "kiln CA");
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2025, 1, 1);
        p.not_after = date_time_ymd(2036, 1, 1);
        let cert = p.self_signed(&key)?;
        Ok(Ca { der: cert.der().to_vec(), issuer: Issuer::new(p, key) })
    }

    /// A custodian: its certificate and key.
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

        /// `s` as its signer says it: a body naming no signer names this one.
        fn says(&self, s: &Statement) -> Statement {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut()
                && o.get("by").and_then(Value::as_str).is_none_or(str::is_empty)
            {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            s
        }

        /// `s` as `payload_type`, signed: its bundle and its digest.
        fn signed(&self, payload_type: &str, s: &Statement) -> anyhow::Result<(Value, String)> {
            let p = payload(&self.says(s))?;
            Ok((bundle(payload_type, &p, &self.key.sign(&to_sign(payload_type, &p))?, &self.der), digest(payload_type, &p)))
        }

        /// `s` as `payload_type`, signed: its bundle.
        fn sign(&self, payload_type: &str, s: &Statement) -> anyhow::Result<Value> {
            Ok(self.signed(payload_type, s)?.0)
        }
    }

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn thing(name: &str, c: char) -> Subject {
        Subject { name: name.into(), digest: BTreeMap::from([("sha256".to_owned(), sha(c))]), ..Subject::default() }
    }

    /// A custody record of `kind` about `things`, `to` or `from` someone, following `follows`,
    /// numbered `seq` on its signer's stream `stream`.
    fn record(kind: Kind, things: &[Subject], to: Option<&str>, from: Option<&str>, follows: &[&str], stream: &str, seq: u64) -> Statement {
        let b = Body { to: to.map(str::to_owned), from: from.map(str::to_owned), why: "a test".into(), ..Body::new(kind, "") };
        Statement {
            subject: things.to_vec(),
            follows: follows.iter().map(|d| (*d).to_owned()).collect(),
            stream: Some(stream.into()),
            seq: Some(seq),
            ..Statement::new(to_value(&b))
        }
    }

    /// kiln's records: each statement checked as kiln took it, by digest.
    #[derive(Default)]
    struct Kept {
        records: BTreeMap<String, Checked>,
    }

    impl Kept {
        /// `s`, signed by `by`, taken by kiln at `at`: its digest.
        fn take(&mut self, ca: &Ca, by: &Signer, s: &Statement, at: i64) -> anyhow::Result<String> {
            let c = verify(&by.sign(PAYLOAD_CUSTODY, s)?, &ca.der, &[], at)?;
            let d = c.digest.clone();
            self.records.insert(d.clone(), c);
            Ok(d)
        }
    }

    /// The chain the plan's example walks: the land hands commit c to fedora-2's runner, which
    /// builds image i from it and hands it to kiln's registry, which hands it to CD for the deploy.
    struct Land {
        ca: Ca,
        kept: Kept,
        landed: String,
        checkout: String,
        built: String,
        stored: String,
        sent: String,
        deployed: String,
    }

    fn land() -> anyhow::Result<Land> {
        let ca = ca()?;
        let (server, fedora, cd) = (Signer::new(&ca, SERVER)?, Signer::new(&ca, FEDORA)?, Signer::new(&ca, CD)?);
        let mut kept = Kept::default();
        let (c, i) = (thing("commit 4c1e2a", 'c'), thing("image kiln:4c1e2a", 'e'));
        let landed = kept.take(&ca, &server, &record(Kind::HandedOn, std::slice::from_ref(&c), Some(FEDORA), None, &[], "land", 1), AT - 50)?;
        let checkout = kept.take(&ca, &fedora, &record(Kind::Received, std::slice::from_ref(&c), None, Some(SERVER), &[&landed], "legs", 1), AT - 40)?;
        let built = kept.take(&ca, &fedora, &record(Kind::HandedOn, std::slice::from_ref(&i), Some(SERVER), None, &[&checkout], "legs", 2), AT - 30)?;
        let stored = kept.take(&ca, &server, &record(Kind::Received, std::slice::from_ref(&i), None, Some(FEDORA), &[&built], "registry", 1), AT - 20)?;
        let sent = kept.take(&ca, &server, &record(Kind::HandedOn, std::slice::from_ref(&i), Some(CD), None, &[&stored], "registry", 2), AT - 10)?;
        let deployed = kept.take(&ca, &cd, &record(Kind::Received, std::slice::from_ref(&i), None, Some(SERVER), &[&sent], "deploys", 1), AT)?;
        Ok(Land { ca, kept, landed, checkout, built, stored, sent, deployed })
    }

    #[test]
    fn w27_r08_custody_bodies_are_read_and_checked() -> anyhow::Result<()> {
        let ca = ca()?;
        let fedora = Signer::new(&ca, FEDORA)?;
        let take = |s: &Statement, payload_type: &str| -> anyhow::Result<Result<Body, Break>> { Ok(body(&verify(&fedora.sign(payload_type, s)?, &ca.der, &[], AT)?)) };
        let good = record(Kind::Received, &[thing("workspace", 'a')], None, Some(JUPITER), &[], "sync", 1);
        let b = take(&good, PAYLOAD_CUSTODY)??;
        assert_eq!((b.kind, b.from.as_deref(), b.why.as_str()), (Kind::Received, Some(JUPITER), "a test"));
        assert_eq!(to_value(&b).get("kind"), Some(&json!("received")));
        assert!(matches!(take(&good, PAYLOAD_DELEGATION)?, Err(Break::NotCustody { .. })));
        let refused = |kind: Kind, edit: &dyn Fn(&mut Body), things: &[Subject]| -> anyhow::Result<String> {
            let mut b = Body { to: Some(JUPITER.into()), from: Some(JUPITER.into()), ..Body::new(kind, "") };
            edit(&mut b);
            let s = Statement { subject: things.to_vec(), ..Statement::new(to_value(&b)) };
            match take(&s, PAYLOAD_CUSTODY)? {
                Err(Break::Unreadable { why, .. }) => Ok(why),
                other => anyhow::bail!("{kind}: not refused as unreadable: {other:?}"),
            }
        };
        let ws = [thing("workspace", 'a')];
        assert!(refused(Kind::Received, &|b| b.v = 2, &ws)?.contains("version 2"));
        assert!(refused(Kind::HandedOn, &|b| b.to = None, &ws)?.contains("not to whom"));
        assert!(refused(Kind::PassedOn, &|b| b.from = Some(String::new()), &ws)?.contains("not from whom"));
        assert!(refused(Kind::Taken, &|_| {}, &ws)?.contains("names no giver"));
        assert!(refused(Kind::Received, &|b| b.taken_for = Some(FEDORA.into()), &ws)?.contains("only a take"));
        assert!(refused(Kind::Received, &|_| {}, &[])?.contains("about nothing"));
        // A record saying it's by another than its signer: two custodians' records are never one.
        assert!(refused(Kind::Received, &|b| b.by = JUPITER.into(), &ws)?.contains(&format!("it says it's by {JUPITER}, but {FEDORA} signed it")));
        Ok(())
    }

    #[test]
    fn w27_r08_a_chain_from_the_land_to_the_deploy_walks_unbroken() -> anyhow::Result<()> {
        let l = land()?;
        let w = walk(&l.deployed, &l.kept.records, SERVER)?;
        let hops: Vec<(&str, Kind, &str)> = w.hops.iter().map(|h| (h.record.as_str(), h.kind, h.custodian.as_str())).collect();
        assert_eq!(
            hops,
            vec![
                (l.deployed.as_str(), Kind::Received, CD),
                (l.sent.as_str(), Kind::HandedOn, SERVER),
                (l.stored.as_str(), Kind::Received, SERVER),
                (l.built.as_str(), Kind::HandedOn, FEDORA),
                (l.checkout.as_str(), Kind::Received, FEDORA),
                (l.landed.as_str(), Kind::HandedOn, SERVER),
            ]
        );
        // Two things began: the image, made from the checkout's commit, and the commit, landed.
        assert_eq!(w.began, vec![l.built.clone(), l.landed.clone()]);
        assert_eq!(w.hops.iter().find(|h| h.record == l.built).map(|h| h.made_from.clone()), Some(vec![l.checkout.clone()]));
        // Walked from any hop, it holds as far back as it goes.
        assert_eq!(walk(&l.checkout, &l.kept.records, SERVER)?.hops.len(), 2);
        // Every hand-on here was answered.
        assert_eq!(unanswered(&l.kept.records), Vec::<String>::new());
        // One hop alone, as kiln checks a record it takes: only the records it follows need be there.
        let alone: BTreeMap<String, Checked> = [&l.stored, &l.built].into_iter().filter_map(|d| l.kept.records.get(d).map(|c| (d.clone(), c.clone()))).collect();
        let h = hop(&l.stored, &alone, SERVER)?;
        assert_eq!((h.kind, h.began, h.custodian.as_str()), (Kind::Received, false, SERVER));
        assert!(matches!(walk(&l.stored, &alone, SERVER), Err(Break::NoRecord { .. })), "the walk needs every record before them");
        assert!(hop(&l.built, &alone, SERVER).is_err_and(|e| matches!(e, Break::NoRecord { ref record, .. } if record == &l.checkout)));
        Ok(())
    }

    #[test]
    fn w27_r08_each_break_in_a_chain_is_named() -> anyhow::Result<()> {
        let l = land()?;
        let (fedora, jupiter, server) = (Signer::new(&l.ca, FEDORA)?, Signer::new(&l.ca, JUPITER)?, Signer::new(&l.ca, SERVER)?);
        let (c, i) = (thing("commit 4c1e2a", 'c'), thing("image kiln:4c1e2a", 'e'));
        // Each case: kiln's records with one record changed, and the receipt the walk starts from.
        let broken = |s: &Statement, by: &Signer| -> anyhow::Result<Result<Walked, Break>> {
            let mut kept = Kept { records: l.kept.records.clone() };
            let d = kept.take(&l.ca, by, s, AT + 5)?;
            Ok(walk(&d, &kept.records, SERVER))
        };
        // The registry's receipt of an image other than the one built.
        let other = thing("image kiln:4c1e2a", 'f');
        let r = broken(&record(Kind::Received, &[other], None, Some(FEDORA), &[&l.built], "registry", 3), &server)?;
        assert!(matches!(&r, Err(Break::Changed { thing, digest, .. }) if thing == "image kiln:4c1e2a" && digest.algorithm == "sha256" && digest.was == sha('e')), "{r:?}");
        assert!(r.is_err_and(|e| e.to_string().contains(&format!("its sha256 was {}, and is {}", sha('e'), sha('f')))));
        // A receipt naming it by a digest the hand-on doesn't name.
        let git = Subject { name: "commit 4c1e2a".into(), digest: BTreeMap::from([("gitCommit".to_owned(), "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1".to_owned())]), ..Subject::default() };
        let r = broken(&record(Kind::Received, &[git], None, Some(SERVER), &[&l.landed], "legs", 3), &fedora)?;
        assert!(matches!(r, Err(Break::NoCommonDigest { .. })), "{r:?}");
        // jupiter takes what the land handed to fedora-2.
        let r = broken(&record(Kind::Received, std::slice::from_ref(&c), None, Some(SERVER), &[&l.landed], "legs", 1), &jupiter)?;
        assert!(matches!(&r, Err(Break::NotTheTaker { to, signer, .. }) if to == FEDORA && signer == JUPITER), "{r:?}");
        // fedora-2 says the commit came from jupiter.
        let r = broken(&record(Kind::Received, std::slice::from_ref(&c), None, Some(JUPITER), &[&l.landed], "legs", 3), &fedora)?;
        assert!(matches!(&r, Err(Break::NotTheGiver { says, giver, .. }) if says == JUPITER && giver == SERVER), "{r:?}");
        // jupiter hands on the image the registry holds.
        let r = broken(&record(Kind::HandedOn, std::slice::from_ref(&i), Some(CD), None, &[&l.stored], "registry", 1), &jupiter)?;
        assert!(matches!(&r, Err(Break::NotTheHolder { holder, signer, .. }) if holder == SERVER && signer == JUPITER), "{r:?}");
        // A receipt of something nothing handed on.
        let r = broken(&record(Kind::Received, std::slice::from_ref(&i), None, Some(SERVER), &[], "deploys", 9), &fedora)?;
        assert!(matches!(r, Err(Break::NoHandOn { kind: Kind::Received, .. })), "{r:?}");
        // A receipt following a receipt, and a hand-on following a hand-on.
        let r = broken(&record(Kind::Received, std::slice::from_ref(&i), None, Some(SERVER), &[&l.stored], "deploys", 9), &fedora)?;
        assert!(matches!(r, Err(Break::OutOfOrder { kind: Kind::Received, was: Kind::Received, .. })), "{r:?}");
        let r = broken(&record(Kind::HandedOn, std::slice::from_ref(&i), Some(CD), None, &[&l.sent], "registry", 3), &server)?;
        assert!(matches!(r, Err(Break::OutOfOrder { kind: Kind::HandedOn, was: Kind::HandedOn, .. })), "{r:?}");
        // A hand-on of two things, received as one: the other didn't arrive.
        let report = thing("report junit.xml", '7');
        let mut kept = Kept { records: l.kept.records.clone() };
        let both = kept.take(&l.ca, &fedora, &record(Kind::HandedOn, &[i.clone(), report], Some(SERVER), None, &[&l.checkout], "legs", 3), AT + 1)?;
        let one = kept.take(&l.ca, &server, &record(Kind::Received, std::slice::from_ref(&i), None, Some(FEDORA), &[&both], "registry", 3), AT + 2)?;
        let r = walk(&one, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::NotAll { missing, .. }) if missing == &vec!["report junit.xml".to_owned()]), "{r:?}");
        assert_eq!(unanswered(&kept.records), Vec::<String>::new());
        // A record the chain follows that kiln doesn't have.
        let mut kept = Kept { records: l.kept.records.clone() };
        kept.records.remove(&l.built);
        let r = walk(&l.deployed, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::NoRecord { record, followed_by: Some(by) }) if record == &l.built && by == &l.stored), "{r:?}");
        assert!(r.is_err_and(|e| e.to_string().contains("which")));
        Ok(())
    }

    #[test]
    fn w27_r08_a_relay_passes_a_thing_on_unchanged_or_breaks_its_chain() -> anyhow::Result<()> {
        let ca = ca()?;
        let (fedora, server, jupiter) = (Signer::new(&ca, FEDORA)?, Signer::new(&ca, SERVER)?, Signer::new(&ca, JUPITER)?);
        let ws = thing("workspace tree", 'a');
        let mut kept = Kept::default();
        let sent = kept.take(&ca, &fedora, &record(Kind::HandedOn, std::slice::from_ref(&ws), Some(SERVER), None, &[], "sync", 1), AT - 3)?;
        let relayed = kept.take(&ca, &server, &record(Kind::PassedOn, std::slice::from_ref(&ws), Some(JUPITER), Some(FEDORA), &[&sent], "relay", 1), AT - 2)?;
        let got = kept.take(&ca, &jupiter, &record(Kind::Received, std::slice::from_ref(&ws), None, Some(SERVER), &[&relayed], "sync", 1), AT - 1)?;
        let w = walk(&got, &kept.records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| h.kind).collect::<Vec<_>>(), vec![Kind::Received, Kind::PassedOn, Kind::HandedOn]);
        assert_eq!(w.began, vec![sent.clone()]);
        // A relay that passed on something else: the digest changed at its hop.
        let mut kept2 = Kept { records: kept.records.clone() };
        let swapped = kept2.take(&ca, &server, &record(Kind::PassedOn, &[thing("workspace tree", 'b')], Some(JUPITER), Some(FEDORA), &[&sent], "relay", 2), AT)?;
        let got2 = kept2.take(&ca, &jupiter, &record(Kind::Received, &[thing("workspace tree", 'b')], None, Some(SERVER), &[&swapped], "sync", 2), AT + 1)?;
        let r = walk(&got2, &kept2.records, SERVER);
        assert!(matches!(&r, Err(Break::Changed { record, from, .. }) if record == &swapped && from == &sent), "{r:?}");
        // A relay's pass jupiter never answered: in transit, or dropped.
        let mut kept3 = Kept { records: kept.records.clone() };
        kept3.records.remove(&got);
        assert_eq!(unanswered(&kept3.records), vec![relayed]);
        Ok(())
    }

    #[test]
    fn w27_r08_a_take_hands_on_for_a_giver_that_cant_sign() -> anyhow::Result<()> {
        let ca = ca()?;
        let (left, moved, server, fedora) = (Signer::new(&ca, LEFT)?, Signer::new(&ca, MOVED)?, Signer::new(&ca, SERVER)?, Signer::new(&ca, FEDORA)?);
        let state = thing("session 01a1751c's state", '5');
        let mut kept = Kept::default();
        let held = kept.take(&ca, &left, &record(Kind::Received, std::slice::from_ref(&state), None, Some(SERVER), &[], "sessions", 1), AT - 100)?;
        // The session began with kiln: its first receipt follows kiln's hand-on.
        let began = kept.take(&ca, &server, &record(Kind::HandedOn, std::slice::from_ref(&state), Some(LEFT), None, &[], "sessions", 1), AT - 110)?;
        kept.records.remove(&held);
        let held = kept.take(&ca, &left, &record(Kind::Received, std::slice::from_ref(&state), None, Some(SERVER), &[&began], "sessions", 1), AT - 100)?;
        let take = |by: &Signer, kept: &mut Kept, seq: u64| -> anyhow::Result<String> {
            let b = Body { to: Some(MOVED.into()), taken_for: Some(LEFT.into()), why: "fedora away since 22:52:19Z".into(), epoch: Some(1), ..Body::new(Kind::Taken, "") };
            let s = Statement { subject: vec![state.clone()], follows: vec![held.clone()], stream: Some("sessions".into()), seq: Some(seq), ..Statement::new(to_value(&b)) };
            kept.take(&ca, by, &s, AT - 10)
        };
        let taken = take(&server, &mut kept, 2)?;
        let got = kept.take(&ca, &moved, &record(Kind::Received, std::slice::from_ref(&state), None, Some(LEFT), &[&taken], "sessions", 1), AT)?;
        let w = walk(&got, &kept.records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| (h.kind, h.custodian.as_str())).collect::<Vec<_>>(), vec![(Kind::Received, MOVED), (Kind::Taken, SERVER), (Kind::Received, LEFT), (Kind::HandedOn, SERVER)]);
        // A take signed by anyone but kiln's server is refused.
        let mut kept2 = Kept { records: kept.records.clone() };
        let forged = take(&fedora, &mut kept2, 1)?;
        let got2 = kept2.take(&ca, &moved, &record(Kind::Received, std::slice::from_ref(&state), None, Some(LEFT), &[&forged], "sessions", 2), AT + 1)?;
        let r = walk(&got2, &kept2.records, SERVER);
        assert!(matches!(&r, Err(Break::TakenNotByServer { signer, .. }) if signer == FEDORA), "{r:?}");
        Ok(())
    }

    #[test]
    fn w27_r08_a_gap_in_a_custodians_stream_breaks_the_chain() -> anyhow::Result<()> {
        let l = land()?;
        let fedora = Signer::new(&l.ca, FEDORA)?;
        let mut kept = Kept { records: l.kept.records.clone() };
        // fedora-2's record 4 on its stream, with no record 3 kiln has: one was dropped.
        kept.take(&l.ca, &fedora, &record(Kind::HandedOn, &[thing("log", 'd')], Some(SERVER), None, &[&l.checkout], "legs", 4), AT + 1)?;
        let r = walk(&l.deployed, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::Gap(Gap::Missing { custodian, stream, from: 3, to: 3 })) if custodian == FEDORA && stream == "legs"), "{r:?}");
        assert!(r.is_err_and(|e| e.to_string() == format!("{FEDORA}'s record 3 on legs is missing")));
        Ok(())
    }

    #[test]
    fn w27_r08_delegations_only_narrow() -> anyhow::Result<()> {
        let ca = ca()?;
        let (sam, agent, jupiter) = (Signer::new(&ca, SAM)?, Signer::new(&ca, AGENT)?, Signer::new(&ca, JUPITER)?);
        let grant = |to: &str, may: &[&str], until: Option<i64>, under: Option<&str>| Statement {
            under: under.map(str::to_owned),
            ..Statement::new(json!({ "v": VERSION, "delegate": to, "may": may, "until": until }))
        };
        let mut kept: BTreeMap<String, (Value, i64)> = BTreeMap::new();
        let mut keep = |by: &Signer, s: &Statement, at: i64| -> anyhow::Result<String> {
            let (b, d) = by.signed(PAYLOAD_DELEGATION, s)?;
            kept.insert(d.clone(), (b, at));
            Ok(d)
        };
        let to_agent = keep(&sam, &grant(AGENT, &["receive:*", "act:run"], Some(AT + 3600), None), AT - 60)?;
        let to_jupiter = keep(&agent, &grant(JUPITER, &["receive:workspace"], Some(AT + 600), Some(&to_agent)), AT - 30)?;
        let wide = keep(&agent, &grant(JUPITER, &["*"], Some(AT + 600), Some(&to_agent)), AT - 30)?;
        let long = keep(&agent, &grant(JUPITER, &["receive:workspace"], None, Some(&to_agent)), AT - 30)?;
        let short = keep(&agent, &grant(JUPITER, &["receive:workspace"], Some(AT - 1), Some(&to_agent)), AT - 30)?;
        let receipt = |under: &str| -> anyhow::Result<Under> {
            let s = Statement { under: Some(under.to_owned()), ..record(Kind::Received, &[thing("workspace", 'a')], None, Some(AGENT), &[], "sync", 1) };
            Ok(verify_under(&jupiter.sign(PAYLOAD_CUSTODY, &s)?, &ca.der, &[], AT, |d| kept.get(d).cloned())?)
        };
        let u = receipt(&to_jupiter)?;
        assert_eq!(entitled(&u, "receive:workspace", AT, &[]), Ok(Entitled::Delegated { by: SAM.into(), links: 2 }));
        assert!(matches!(entitled(&u, "act:run", AT, &[]), Err(Refusal::NotEntitled { n: 1, .. })));
        assert!(matches!(entitled(&receipt(&wide)?, "receive:workspace", AT, &[]), Err(Refusal::Widened { n: 1, ref scope, .. }) if scope == "*"));
        assert!(matches!(entitled(&receipt(&long)?, "receive:workspace", AT, &[]), Err(Refusal::Lengthened { n: 1, .. })));
        assert!(matches!(entitled(&receipt(&short)?, "receive:workspace", AT, &[]), Err(Refusal::Expired { n: 1, .. })));
        // The agent's session went on to its next epoch at AT - 5: kiln revoked what it delegated.
        let moved = [Revoked { digest: to_jupiter.clone(), at: AT - 5, why: "agent matrix went on at epoch 4".into() }];
        let r = entitled(&u, "receive:workspace", AT, &moved);
        assert!(matches!(&r, Err(Refusal::Revoked { n: 1, why, .. }) if why.contains("epoch 4")), "{r:?}");
        // Before the revocation it held.
        assert!(entitled(&u, "receive:workspace", AT - 6, &moved).is_ok());
        // Under no delegation, a signer acts in its own right.
        let own = verify_under(&jupiter.sign(PAYLOAD_CUSTODY, &record(Kind::Received, &[thing("workspace", 'a')], None, Some(AGENT), &[], "sync", 1))?, &ca.der, &[], AT, |_| None)?;
        assert_eq!(entitled(&own, "anything", AT, &[]), Ok(Entitled::Own));
        assert!(covers("receive:*", "receive:blob") && !covers("receive:*", "act:run") && !covers("receive:workspace", "receive:*"));
        Ok(())
    }

    #[test]
    fn w27_r08_the_fence_takes_only_the_holder_at_its_epoch() -> anyhow::Result<()> {
        let held = Held {
            session: "01a1751c".into(),
            epoch: 1,
            by: Some(Holder { machine: "adacachy".into(), instance: "01a14794".into() }),
            since: AT - 3600,
            transfer: format!("sha256:{}", sha('t')),
        };
        let id = |uri: &str| Identity::parse(uri);
        assert_eq!(fence(&held, &id(MOVED)?), Ok(()));
        let r = fence(&held, &id(LEFT)?);
        assert!(matches!(&r, Err(Fenced::Left { signed: 0, current: 1, .. })), "{r:?}");
        assert!(r.is_err_and(|e| e.to_string().contains("went on at epoch 1, on runner adacachy (instance 01a14794)") && e.to_string().ends_with("it's from a copy left behind")));
        // Its epoch, from another process of its machine (the 14:18 drop's old build).
        let blue = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1/machine/adacachy/instance/01a17502";
        assert!(matches!(fence(&held, &id(blue)?), Err(Fenced::NotTheHolder { epoch: 1, .. })));
        // An epoch no transfer reached, another session, and a runner signing as no agent.
        assert!(matches!(fence(&held, &id("kiln://hyper-light/person/ada/agent/01a1751c/epoch/2/machine/adacachy/instance/01a14794")?), Err(Fenced::Ahead { signed: 2, .. })));
        assert!(matches!(fence(&held, &id(AGENT)?), Err(Fenced::AnotherSession { .. })));
        assert!(matches!(fence(&held, &id(FEDORA)?), Err(Fenced::NotAnAgent { .. })));
        Ok(())
    }
}
