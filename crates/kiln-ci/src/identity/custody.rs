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
//!   A relay's pass, or a receipt, may follow its originator's own signed statement (an agent's
//!   act), of that very statement, where its thing began; a relay's pass of what its sender signs
//!   nothing of (a frame) follows nothing, its chain beginning there ([`super::relay`]). No
//!   custodian's numbering of its streams has a gap. The first hop that doesn't hold is named.
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
    pub fn gives(self) -> bool {
        matches!(self, Self::HandedOn | Self::PassedOn | Self::Taken)
    }

    /// It takes its things from someone, following a giver's record.
    pub fn takes(self) -> bool {
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
    /// one hop are two records. An agent session's receipt, signed as it asks for its epoch's
    /// certificate, names it at that epoch without the process holding its key
    /// (`…/agent/<s>/epoch/<n>`): only one process holds an epoch's key, which its certificate
    /// names.
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
    /// Whom its signer acts for here, nearest first, back to the person: a plan's step kiln hands
    /// on as the run's driver, for the agent that ran the plan, for its person (W27-R08's nested
    /// actors). Said, as its signer knows them; who each acts for is its certificate's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acts_for: Vec<String>,
    /// What for: "leg 812/1's checkout", "deploy kiln-production".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// The part of its signer that holds them: kiln's registry, its cache, a runner's mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// A take's: the giver that couldn't sign, whose custody kiln's server hands on. Or kiln's
    /// receipt's: the taker that couldn't (a person with no key on the device that asked), for whom
    /// kiln's server takes them. A walk reads either link as kiln's for them, never as theirs.
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
        Self { v: VERSION, kind, by: by.to_owned(), to: None, from: None, held_for: None, acts_for: Vec::new(), why: String::new(), component: None, taken_for: None, epoch: None, said_at: None }
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
            (Kind::Received, who @ Some(_)) if !named(who) => Err("it's kiln's receipt for someone but names no one".into()),
            (Kind::Taken | Kind::Received, _) | (_, None) => Ok(()),
            (kind, Some(_)) => Err(format!("it says they were {kind}, and only a take, or kiln's receipt for someone, names whom kiln acts for")),
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
    if !names(&b.by, &c.custodian) {
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
    /// A take's or kiln's receipt's: whom kiln's server signed it for ("kiln for sam"), who
    /// didn't sign it.
    pub taken_for: Option<String>,
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
    /// A record that holds its things for someone (its body's `for`) without a delegation from
    /// them that entitles its signer to (docs/ci.md W27-R08: a machine taking custody for someone
    /// else does it under a delegation that names them and only narrows). Appended last.
    #[error("{record} holds them for {held_for}, but not under a delegation from them that entitles it: {why}")]
    NotEntitled { record: String, held_for: String, why: String },
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
            Self::NotEntitled { .. } => "not-entitled",
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

/// Whether `named`, a record's word for a custodian, names `signer`: the same identity, or an agent
/// session at an epoch named without the runner process holding its key (`…/agent/<s>/epoch/<n>`),
/// which whichever process holds that epoch's key signs for (`…/epoch/<n>/machine/<m>/instance/<i>`).
/// A giver hands a session on to its next epoch before knowing the process that will hold it.
pub fn names(named: &str, signer: &str) -> bool {
    if named == signer {
        return true;
    }
    let agent_epoch = named.split('/').collect::<Vec<_>>().windows(2).any(|w| matches!(w, ["epoch", n] if n.parse::<u64>().is_ok())) && named.contains("/agent/");
    agent_epoch && signer.strip_prefix(named).is_some_and(|rest| rest.starts_with("/machine/"))
}

/// Whose custody record `c` (body `b`) is: its signer's; a take's giver's, for whom kiln's server
/// hands on (a take signed by another breaks as [`Break::TakenNotByServer`] where it's taken); or,
/// kiln's receipt for a taker who couldn't sign, theirs ("kiln for sam").
fn custodian_for(c: &Checked, b: &Body, server: &str) -> String {
    match (&b.taken_for, b.kind) {
        (Some(who), Kind::Taken) => who.clone(),
        (Some(who), _) if c.custodian == server => who.clone(),
        _ => c.custodian.clone(),
    }
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
        // kiln's receipt for a taker that couldn't sign: only kiln's server signs one.
        if rb.taken_for.is_some() && r.custodian != server {
            return Err(Break::TakenNotByServer { record, signer: r.custodian.clone(), server: server.to_owned() });
        }
        let to = pb.to.clone().unwrap_or_default();
        let taker = custodian_for(r, rb, server);
        if !names(&to, &taker) {
            return Err(Break::NotTheTaker { record, from, to, signer: taker });
        }
        let giver = custodian_for(p, pb, server);
        let says = rb.from.clone().unwrap_or_default();
        if !names(&says, &giver) && !names(&giver, &says) {
            return Err(Break::NotTheGiver { record, from, says, giver });
        }
        return Ok(Edge::Transfer);
    }
    // A hand-on or a take: of what its holder received, or made from it.
    if !pb.kind.takes() {
        return Err(out_of_order());
    }
    let (holder, held) = (custodian_for(r, rb, server), custodian_for(p, pb, server));
    if !names(&holder, &held) {
        return Err(Break::NotTheHolder { record, from, holder: held, signer: holder });
    }
    Ok(if same.0 > 0 { Edge::Holding } else { Edge::MadeFrom })
}

/// Whether `c` is an originator's own signed statement (an agent session's act, kiln's server's
/// word), not a custody record or a delegation: what a relay passes on, which a relay's record or a
/// receipt may follow ([`crate::identity::relay`]).
fn originator(c: &Checked) -> bool {
    c.payload_type != PAYLOAD_CUSTODY && c.payload_type != PAYLOAD_DELEGATION
}

/// Whether record `r` is about statement `p` itself: one of its things is `p`, by its digest.
fn about(r: &Checked, p: &Checked) -> bool {
    let hex = p.digest.strip_prefix("sha256:").unwrap_or(&p.digest);
    r.statement.subject.iter().any(|t| t.digest.get("sha256").is_some_and(|d| d.eq_ignore_ascii_case(hex)))
}

/// Whether record `r` (body `rb`) takes `p`, its originator's own signed statement: only a relay's
/// pass or a receipt may, of that very statement (one of its things, by the statement's digest),
/// saying it came from the statement's signer.
fn from_originator(r: &Checked, rb: &Body, p: &Checked) -> Result<(), Break> {
    if !rb.kind.takes() {
        return Err(Break::NotCustody { record: p.digest.clone(), payload_type: p.payload_type.clone() });
    }
    if !about(r, p) {
        let thing = r.statement.subject.first().map(|t| t.name.clone()).unwrap_or_default();
        return Err(Break::NotHandedOn { record: r.digest.clone(), from: p.digest.clone(), thing });
    }
    let says = rb.from.clone().unwrap_or_default();
    if !names(&says, &p.custodian) && !names(&p.custodian, &says) {
        return Err(Break::NotTheGiver { record: r.digest.clone(), from: p.digest.clone(), says, giver: p.custodian.clone() });
    }
    Ok(())
}

/// Checks record `record`'s own hop: each record it follows, which `records` must hold, followed as
/// [`walk`] says it may be, and a receipt or a pass following a hand-on. Not the records before
/// those, nor its custodian's numbering: that's [`walk`]'s. What kiln checks as it takes a record.
pub fn hop(record: &str, records: &BTreeMap<String, Checked>, server: &str) -> Result<Hop, Break> {
    let r = records.get(record).ok_or_else(|| Break::NoRecord { record: record.to_owned(), followed_by: None })?;
    let rb = body(r)?;
    held_under(r, &rb, records)?;
    let (mut gave, mut held, mut made_from, mut originated) = (false, false, Vec::new(), false);
    for f in &r.statement.follows {
        let p = records.get(f).ok_or_else(|| Break::NoRecord { record: f.clone(), followed_by: Some(record.to_owned()) })?;
        // Its originator's own signed statement (an agent's act, kiln's word, or a custody record
        // of another's, which this record is about by its digest), which a relay passes on: the
        // thing began there, signed by its originator (docs/ci.md W27-R08's relays).
        if originator(p) || (about(r, p) && rb.kind.takes()) {
            from_originator(r, &rb, p)?;
            (gave, originated) = (true, true);
            continue;
        }
        let pb = body(p)?;
        match edge(r, &rb, p, &pb, server)? {
            Edge::Transfer => gave = true,
            Edge::Holding => held = true,
            Edge::MadeFrom => made_from.push(f.clone()),
        }
    }
    // A receipt of what came from where kiln has no identity (a registry outside it,
    // `docker://alpine:3.20`), following nothing: its chain begins there, as its taker received it.
    let from_outside = rb.kind == Kind::Received && r.statement.follows.is_empty() && rb.from.as_deref().is_some_and(|f| !f.is_empty() && !f.starts_with("kiln://"));
    // A relay's pass of what its sender signs nothing of (a frame of kiln's server, a reach link's
    // ciphertext), following nothing: its chain begins at the relay, as the relay records it.
    let unsigned = rb.kind == Kind::PassedOn && r.statement.follows.is_empty();
    if rb.kind.takes() && !gave && !from_outside && !unsigned {
        return Err(Break::NoHandOn { record: record.to_owned(), kind: rb.kind });
    }
    let began = (!rb.kind.takes() && !held) || from_outside || originated || unsigned;
    let taken_for = rb.taken_for.clone().filter(|_| r.custodian == server);
    Ok(Hop { record: record.to_owned(), kind: rb.kind, custodian: r.custodian.clone(), taken_for, at: r.at, subject: r.statement.subject.clone(), made_from, began })
}

/// Whether record `r` (body `b`), when it takes its things for someone (its `for`: an agent
/// session whose run another machine does), does so under a delegation from them among `records`,
/// the one its `under` names: one naming its signer as delegate and entitling it to receive each of
/// its things ([`receive_scope`]), as of when kiln took it.
fn held_under(r: &Checked, b: &Body, records: &BTreeMap<String, Checked>) -> Result<(), Break> {
    let Some(whom) = b.held_for.as_deref().filter(|w| !w.is_empty() && b.kind.takes()) else { return Ok(()) };
    let refuse = |why: String| Break::NotEntitled { record: r.digest.clone(), held_for: whom.to_owned(), why };
    let Some(under) = r.statement.under.as_deref() else { return Err(refuse("it names no delegation it acts under".into())) };
    let d = records.get(under).ok_or_else(|| Break::NoRecord { record: under.to_owned(), followed_by: Some(r.digest.clone()) })?;
    let g = grant(d).map_err(|why| refuse(format!("{under} isn't a delegation kiln reads: {why}")))?;
    if !names(&g.delegate, &r.custodian) {
        return Err(refuse(format!("{under} delegates to {}, not to {}, who signed it", g.delegate, r.custodian)));
    }
    if !names(whom, &d.custodian) && !names(&d.custodian, whom) {
        return Err(refuse(format!("{under} is signed by {}", d.custodian)));
    }
    let u = Under { statement: r.clone(), through: vec![d.clone()] };
    for t in &r.statement.subject {
        entitled(&u, &receive_scope(t), r.at, &[]).map_err(|e| refuse(e.to_string()))?;
    }
    Ok(())
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
            // An originator's own statement is where its thing began, not a hop of its own.
            next.extend(r.statement.follows.iter().filter(|f| records.get(*f).is_none_or(|p| !originator(p))).cloned());
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
    #[error("it was signed at epoch {signed} of session {session}, which went on at epoch {current}{to} at {since}{}: it's from a copy left behind", transfer_of(.transfer))]
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

/// " (transfer 3f…)", kiln's record of the transfer, or "" for none (a hand-over from before).
fn transfer_of(t: &str) -> String {
    if t.is_empty() { String::new() } else { format!(" (transfer {t})") }
}

impl Held {
    /// "went on at epoch 1, on runner adacachy (instance 01a14794) at 2026-10-25T22:58:15Z
    /// (transfer 3f…)": where the session is now, said to what comes from an epoch it left.
    pub fn went_on(&self) -> String {
        format!("went on at epoch {}{} at {}{}", self.epoch, on(self.by.as_ref()), chain::when(self.since), transfer_of(&self.transfer))
    }
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

/// A peer runner's hand-on of an object of its cache to the runner that asked for it (docs/ci.md
/// W24-R27; the Transfers plan's step 12): `by` the holder, `to` the taker, the object of `repo` by
/// its SHA-256. Both sides build it alike, and a statement's digest leaves its signer out, so the
/// taker's receipt follows its digest without its bytes ever crossing to the taker.
pub fn peer_object_hand_on(by: &str, to: &str, repo: &str, sha256: &str) -> statement::Statement {
    let body = Body { to: Some(to.to_owned()), why: "an object of its cache, to the peer that asked for it".into(), ..Body::new(Kind::HandedOn, by) };
    statement::Statement { subject: vec![peer_object(repo, sha256)], ..statement::Statement::new(to_value(&body)) }
}

/// The taker's receipt of [`peer_object_hand_on`] (its digest `hand_on`), of the object by the
/// SHA-256 it hashed to (`got`): a copy that isn't what its name says breaks at that hop, naming
/// the peer (`from`).
pub fn peer_object_receipt(by: &str, from: &str, repo: &str, got: &str, hand_on: &str) -> statement::Statement {
    let body = Body { from: Some(from.to_owned()), why: "an object of a peer's cache".into(), ..Body::new(Kind::Received, by) };
    statement::Statement { subject: vec![peer_object(repo, got)], follows: vec![hand_on.to_owned()], ..statement::Statement::new(to_value(&body)) }
}

/// An object of `repo`'s cache by its SHA-256 (`sha256:<hex>` or the hex).
fn peer_object(repo: &str, sha256: &str) -> Subject {
    by_sha256(&format!("object of {repo}"), sha256)
}

/// `name`, by its SHA-256 (`sha256:<hex>` or the hex).
fn by_sha256(name: &str, sha256: &str) -> Subject {
    let hex = sha256.strip_prefix("sha256:").unwrap_or(sha256).to_ascii_lowercase();
    Subject { name: name.to_owned(), digest: BTreeMap::from([("sha256".to_owned(), hex)]), ..Subject::default() }
}

/// What a delegation entitles the runner that runs an agent session's run on another machine to:
/// its workspace, as the sync left it (docs/ci.md W27-R08; the Transfers plan's step 12).
pub const RECEIVE_WORKSPACE: &str = "receive:workspace";
/// ... and the run itself.
pub const RECEIVE_RUN: &str = "receive:run";

/// What receiving `thing` for someone takes: `receive:` and its kind, its name's first word
/// (`receive:workspace` for "workspace kiln-…", `receive:run` for "run 3").
pub fn receive_scope(thing: &Subject) -> String {
    format!("receive:{}", thing.name.split_whitespace().next().unwrap_or_default())
}

/// What one runner hands another over a reach link (docs/ci.md W27-R08; the Transfers plan's step
/// 12), by its SHA-256: the workspace a sync left as copy `key` (its tree's digest, as `kiln run
/// --reuse` reads one), ...
pub fn reach_workspace(key: &str, sha256: &str) -> Subject {
    by_sha256(&format!("workspace {key}"), sha256)
}

/// ... run `id` of the asker's reach session (the digest of what it runs, where, with what and on
/// which workspace), ...
pub fn reach_run(id: u64, sha256: &str) -> Subject {
    by_sha256(&format!("run {id}"), sha256)
}

/// ... and run `id`'s output (the SHA-256 of its bytes, in the order the run said them).
pub fn reach_output(id: u64, sha256: &str) -> Subject {
    by_sha256(&format!("output of run {id}"), sha256)
}

/// `by`'s hand-on of `thing` to `to` across a reach link, held for `held_for` (the agent session
/// whose it is, when `by` isn't it), following `follows` (the receipt of what it was made from), for
/// `why`. Both sides build it alike: a statement's digest leaves its signer out, and it says no
/// time, so the taker's receipt follows it by a digest the taker works out too.
pub fn reach_hand_on(by: &str, to: &str, held_for: Option<&str>, thing: Subject, follows: Vec<String>, why: &str) -> statement::Statement {
    let body = Body { to: Some(to.to_owned()), held_for: held_for.map(str::to_owned), why: why.to_owned(), ..Body::new(Kind::HandedOn, by) };
    statement::Statement { subject: vec![thing], follows, ..statement::Statement::new(to_value(&body)) }
}

/// `by`'s receipt across a reach link of `hand_on` (its digest) from `from`, of `thing` by what it
/// got, held for `held_for` under delegation `under` (by its digest) when it takes custody for
/// someone else. A digest that changed on the way breaks the chain at this hop.
pub fn reach_receipt(by: &str, from: &str, held_for: Option<&str>, thing: Subject, hand_on: &str, under: Option<&str>, why: &str) -> statement::Statement {
    let body = Body { from: Some(from.to_owned()), held_for: held_for.map(str::to_owned), why: why.to_owned(), ..Body::new(Kind::Received, by) };
    statement::Statement { subject: vec![thing], follows: vec![hand_on.to_owned()], under: under.map(str::to_owned), ..statement::Statement::new(to_value(&body)) }
}

/// The delegation agent session `by` (its identity at its epoch) gives runner process `delegate`,
/// which runs a run of the session's on another machine ([`PAYLOAD_DELEGATION`]): to receive the
/// run and its workspace for the session, nothing more ([`RECEIVE_WORKSPACE`], [`RECEIVE_RUN`]).
/// It says no end and no time: it lasts as long as the session's epoch (its signer's certificate),
/// and the same session, epoch and runner give the same delegation, which kiln keeps once.
pub fn reach_delegation(by: &str, delegate: &str) -> statement::Statement {
    let grant = Grant { v: VERSION, by: by.to_owned(), delegate: delegate.to_owned(), may: vec![RECEIVE_WORKSPACE.to_owned(), RECEIVE_RUN.to_owned()], until: None };
    statement::Statement::new(serde_json::to_value(&grant).unwrap_or(Value::Null))
}

/// An act's payload type ([`Act`]).
pub const PAYLOAD_ACT: &str = "application/vnd.kiln.act+json";

/// An act an agent session makes through kiln (W27-R08: the twin), signed by the runner process
/// holding it with the session's key at its epoch, as [`PAYLOAD_ACT`]: what it is (`op`: a kiln
/// operation's name, a machine run), the statement's subject being the act itself, by its digest.
/// kiln applies it only if [`fence`] takes its signer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Act {
    /// [`VERSION`].
    pub v: u32,
    /// Its signer, as its certificate names it (as [`Body::by`]).
    #[serde(default)]
    pub by: String,
    pub session: String,
    pub epoch: u64,
    pub op: String,
}

/// Why an act doesn't count as its session's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unacted {
    #[error("it isn't signed as an act: {0}")]
    Unsigned(String),
    #[error("it's an act of session {said}, not {session}")]
    AnotherSession { said: String, session: String },
    #[error("it's not the act its session signed: the act itself changed")]
    Changed,
    #[error("{0}")]
    Fenced(#[from] Fenced),
}

/// Whether act `act` (checked: [`statement::verify`]) is session `held.session`'s to make now:
/// signed as of `what` (its bytes as sent, without the act), at the epoch kiln has the session at,
/// by the runner process holding it then ([`fence`]). The act, or why not.
pub fn acted(act: &Checked, held: &Held, what: &[u8]) -> Result<Act, Unacted> {
    if act.payload_type != PAYLOAD_ACT {
        return Err(Unacted::Unsigned(format!("it's a {} statement", act.payload_type)));
    }
    let a: Act = serde_json::from_value(act.statement.body.clone()).map_err(|e| Unacted::Unsigned(e.to_string()))?;
    if a.v != VERSION || a.by != act.custodian {
        return Err(Unacted::Unsigned(format!("its body (version {}, by {}) isn't one kiln reads", a.v, a.by)));
    }
    if a.session != held.session {
        return Err(Unacted::AnotherSession { said: a.session, session: held.session.clone() });
    }
    let digest = chain::hex(&<sha2::Sha256 as sha2::Digest>::digest(what));
    if !act.statement.subject.iter().any(|t| t.digest.get("sha256").is_some_and(|d| d.eq_ignore_ascii_case(&digest))) {
        return Err(Unacted::Changed);
    }
    let signer = Identity::parse(&act.custodian).map_err(|e| Unacted::Unsigned(e.to_string()))?;
    fence(held, &signer)?;
    Ok(a)
}

/// A hand-over's word's payload type ([`Word`]).
pub const PAYLOAD_WORD: &str = "application/vnd.kiln.word+json";

/// A hand-over's word kiln's server sends one runner process about an agent session (W27-R08: hand
/// it to the machine's newest process; the server's word on such a hand-over), signed by the
/// server as [`PAYLOAD_WORD`]: the process it's for, and the epoch that process holds the session
/// at as kiln knows it. The statement's subject is the word itself, by its digest. Any other
/// process refuses it (the 14:18 drop: a word for the runner being upgraded, passed on to its new
/// build, let go of six sessions there).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Word {
    /// [`VERSION`].
    pub v: u32,
    /// Its signer, kiln's server, as its certificate names it (as [`Body::by`]).
    #[serde(default)]
    pub by: String,
    pub session: String,
    pub machine: String,
    pub instance: String,
    pub epoch: u64,
}

/// Why a hand-over's word isn't this process's to apply.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Misaddressed {
    #[error("it isn't signed as kiln's server's word: {0}")]
    Unsigned(String),
    #[error("it's a word about session {said}, not {session}")]
    AnotherSession { said: String, session: String },
    #[error("it's not the word kiln signed: the word itself changed")]
    Changed,
    #[error("it's for {machine} (instance {instance}) at epoch {epoch}, not this process, {here}")]
    AnotherProcess { machine: String, instance: String, epoch: u64, here: String },
}

/// Whether hand-over's word `command` (as sent), signed as `word` (checked: [`statement::verify`]),
/// is for this process: runner process `instance` of `machine`, which holds session `session` at
/// epoch `epoch` (none: at none, as once it let go, when only the process is checked). kiln's
/// server, `server`, alone signs words. The word, or why not.
pub fn addressed(word: &Checked, server: &str, session: &str, command: &[u8], machine: &str, instance: &str, epoch: Option<u64>) -> Result<Word, Misaddressed> {
    let unsigned = |why: String| Misaddressed::Unsigned(why);
    if word.payload_type != PAYLOAD_WORD {
        return Err(unsigned(format!("it's a {} statement", word.payload_type)));
    }
    if word.custodian != server {
        return Err(unsigned(format!("{} signed it, not kiln's server", word.custodian)));
    }
    let w: Word = serde_json::from_value(word.statement.body.clone()).map_err(|e| unsigned(e.to_string()))?;
    if w.v != VERSION || w.by != word.custodian {
        return Err(unsigned(format!("its body (version {}, by {}) isn't one kiln reads", w.v, w.by)));
    }
    if w.session != session {
        return Err(Misaddressed::AnotherSession { said: w.session, session: session.to_owned() });
    }
    let digest = chain::hex(&<sha2::Sha256 as sha2::Digest>::digest(command));
    if !word.statement.subject.iter().any(|t| t.digest.get("sha256").is_some_and(|d| d.eq_ignore_ascii_case(&digest))) {
        return Err(Misaddressed::Changed);
    }
    if w.machine != machine || w.instance != instance || epoch.is_some_and(|e| e != w.epoch) {
        let here = match epoch {
            Some(e) => format!("{machine} (instance {instance}) at epoch {e}"),
            None => format!("{machine} (instance {instance}), which holds it at no epoch"),
        };
        return Err(Misaddressed::AnotherProcess { machine: w.machine, instance: w.instance, epoch: w.epoch, here });
    }
    Ok(w)
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
            // A leg's names the facts its kind needs: its commit and its workflow.
            if uri.contains("/leg/") {
                use crate::identity::chain::fulcio;
                for (member, value) in [(fulcio::SOURCE_REPOSITORY_DIGEST, "4c1e2a90d1f3b5a7c9e1f3a5b7c9d1e3f5a7b9c1"), (fulcio::BUILD_SIGNER_URI, "https://kiln.home/hyper-light/kiln/.kiln/ci.yml@refs/heads/main")] {
                    let mut oid = fulcio::ARC.to_vec();
                    oid.push(member);
                    p.custom_extensions.push(rcgen::CustomExtension::from_oid_content(&oid, crate::identity::chain::der_utf8_string(value).ok_or_else(|| anyhow::anyhow!("too long"))?));
                }
            }
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
            self.take_as(ca, by, PAYLOAD_CUSTODY, s, at)
        }

        /// [`Kept::take`], as `payload_type` (a delegation).
        fn take_as(&mut self, ca: &Ca, by: &Signer, payload_type: &str, s: &Statement, at: i64) -> anyhow::Result<String> {
            let c = verify(&by.sign(payload_type, s)?, &ca.der, &[], at)?;
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
        assert!(refused(Kind::PassedOn, &|b| b.taken_for = Some(FEDORA.into()), &ws)?.contains("only a take, or kiln's receipt for someone"));
        assert!(refused(Kind::Received, &|b| b.taken_for = Some(String::new()), &ws)?.contains("names no one"));
        assert!(refused(Kind::Received, &|_| {}, &[])?.contains("about nothing"));
        // A record saying it's by another than its signer: two custodians' records are never one.
        assert!(refused(Kind::Received, &|b| b.by = JUPITER.into(), &ws)?.contains(&format!("it says it's by {JUPITER}, but {FEDORA} signed it")));
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 6): sam takes a plan's step over from a device with no
    /// key. kiln's server signs the receipt for them (`taken_for`), and the walk reads that link as
    /// kiln's for sam, never as sam's; a receipt for someone that another signs, or for another
    /// than the hand-on went to, breaks the chain. Once sam has a key, what they hand on follows
    /// kiln's receipt for them as theirs. Whom a giver acts for goes with what it hands on, nearest
    /// first.
    #[test]
    fn w27_r08_kiln_takes_for_a_person_with_no_key() -> anyhow::Result<()> {
        let ca = ca()?;
        let (server, sam, fedora, agent) = (Signer::new(&ca, SERVER)?, Signer::new(&ca, SAM)?, Signer::new(&ca, FEDORA)?, Signer::new(&ca, AGENT)?);
        let mut kept = Kept::default();
        let step = thing("plan run 01a17 step 3", 'd');
        let planner = "kiln://hyper-light/person/sam/agent/planner/epoch/0";
        let mut handed = record(Kind::HandedOn, std::slice::from_ref(&step), Some(SAM), None, &[], "plans", 1);
        if let Some(o) = handed.body.as_object_mut() {
            o.insert("acts_for".into(), json!([planner, SAM]));
        }
        let hand_on = kept.take(&ca, &server, &handed, AT - 30)?;
        let b = body(kept.records.get(&hand_on).ok_or_else(|| anyhow::anyhow!("the hand-on isn't kept"))?)?;
        assert_eq!(b.acts_for, vec![planner.to_owned(), SAM.to_owned()]);
        let for_whom = |who: &str, s: Statement| -> Statement {
            let mut s = s;
            if let Some(o) = s.body.as_object_mut() {
                o.insert("taken_for".into(), json!(who));
            }
            s
        };
        let receipt = |stream: &str, seq: u64| record(Kind::Received, std::slice::from_ref(&step), None, Some(SERVER), &[&hand_on], stream, seq);
        let taken = kept.take(&ca, &server, &for_whom(SAM, receipt("plans", 2)), AT - 20)?;
        let h = hop(&taken, &kept.records, SERVER)?;
        assert_eq!((h.custodian.as_str(), h.taken_for.as_deref()), (SERVER, Some(SAM)), "kiln for sam, never sam");
        let forged = kept.take(&ca, &fedora, &for_whom(SAM, receipt("legs", 1)), AT - 19)?;
        assert!(matches!(hop(&forged, &kept.records, SERVER), Err(Break::TakenNotByServer { .. })));
        let elsewhere = kept.take(&ca, &server, &for_whom(JUPITER, receipt("plans-2", 1)), AT - 18)?;
        assert!(matches!(hop(&elsewhere, &kept.records, SERVER), Err(Break::NotTheTaker { .. })));
        // sam, with a key now, hands it on: theirs, following kiln's receipt for them.
        let to_agent = kept.take(&ca, &sam, &record(Kind::HandedOn, std::slice::from_ref(&step), Some(AGENT), None, &[&taken], "work", 1), AT - 10)?;
        let got = kept.take(&ca, &agent, &record(Kind::Received, std::slice::from_ref(&step), None, Some(SAM), &[&to_agent], "work", 1), AT)?;
        let w = walk(&got, &kept.records, SERVER)?;
        let hops: Vec<(&str, Option<&str>)> = w.hops.iter().map(|h| (h.custodian.as_str(), h.taken_for.as_deref())).collect();
        assert_eq!(hops, vec![(AGENT, None), (SAM, None), (SERVER, Some(SAM)), (SERVER, None)]);
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

    /// W27-R08 (the Transfers plan's step 11): a leg's receipt of an image from a registry kiln has
    /// no identity of begins its chain there, as the leg received it; a receipt from one of kiln's
    /// identities with no hand-on before it is still a break.
    #[test]
    fn w27_r08_a_receipt_from_outside_kiln_begins_its_chain() -> anyhow::Result<()> {
        let ca = ca()?;
        let fedora = Signer::new(&ca, FEDORA)?;
        let image = thing("image docker.io/library/alpine:3.20", 'c');
        let mut kept = Kept::default();
        let got = kept.take(&ca, &fedora, &record(Kind::Received, std::slice::from_ref(&image), None, Some("docker://alpine:3.20"), &[], "custody", 1), AT - 1)?;
        let w = walk(&got, &kept.records, SERVER)?;
        assert_eq!(w.began, vec![got.clone()]);
        assert!(w.hops.iter().all(|h| h.kind == Kind::Received && h.began));
        let mut kept = Kept::default();
        let from_kiln = kept.take(&ca, &fedora, &record(Kind::Received, std::slice::from_ref(&image), None, Some(SERVER), &[], "custody", 1), AT - 1)?;
        let r = walk(&from_kiln, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::NoHandOn { record, .. }) if record == &from_kiln), "{r:?}");
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 11): a leg's inputs, from the land to what it made. kiln
    /// hands fedora-2's runner the leg's assignment, its commit and the tarball it served it; the
    /// runner receives each and hands each the leg's executor needs to it ([`crate::attest::leg::
    /// runner_custody`]); the leg receives them, builds, and hands the image it made to kiln, which
    /// receives it. Walked back from kiln's receipt: every hop a hand-on and its receipt, begun only
    /// where kiln handed the leg's inputs on, none left unanswered. The tarball changed in transit:
    /// the runner's receipt of what it got breaks at that hop.
    #[test]
    fn w27_r08_a_legs_inputs_walk_from_kiln_to_what_it_made() -> anyhow::Result<()> {
        use crate::attest::leg::{self, Handed, Receipt};
        let leg_uri = format!("{FEDORA}/run/812/attempt/1/leg/build");
        assert_eq!(leg::runner_of(&leg_uri).map_err(anyhow::Error::msg)?, FEDORA);
        let ca = ca()?;
        let (server, runner, built) = (Signer::new(&ca, SERVER)?, Signer::new(&ca, FEDORA)?, Signer::new(&ca, &leg_uri)?);
        let commit = Subject { name: "commit".into(), digest: BTreeMap::from([("gitCommit".to_owned(), "4c1e2a".repeat(7).get(..40).unwrap_or_default().to_owned())]), ..Subject::default() };
        let (assignment, tarball) = (thing("assignment", 'a'), thing("checkout", 'b'));
        let mut kept = Kept::default();
        let mut handed = Vec::new();
        for (n, t) in [&assignment, &commit, &tarball].into_iter().enumerate() {
            let d = kept.take(&ca, &server, &record(Kind::HandedOn, std::slice::from_ref(t), Some(FEDORA), None, &[], "inputs", n as u64 + 1), AT - 10)?;
            handed.push(Handed { subject: t.clone(), hand_on: d, from: SERVER.into() });
        }
        let to_leg = vec![assignment.clone(), tarball.clone()];
        let (records, hand_ons) = leg::runner_custody(FEDORA, &leg_uri, &handed, &to_leg, "for the leg")?;
        assert_eq!(hand_ons.len(), to_leg.len());
        for (st, _) in &records {
            kept.take(&ca, &runner, st, AT - 8)?;
        }
        let receipts: Vec<Receipt> = to_leg.iter().zip(&hand_ons).map(|(t, h)| Receipt { subject: t.clone(), from: FEDORA.into(), why: "from its runner".into(), follows: vec![h.clone()] }).collect();
        let image = thing("image registry/app", 'e');
        let mut made = String::new();
        for (st, d) in leg::custody(&leg_uri, &receipts, vec![image.clone()], SERVER, None)? {
            kept.take(&ca, &built, &st, AT - 5)?;
            made = d;
        }
        let kilns = kept.take(&ca, &server, &record(Kind::Received, std::slice::from_ref(&image), None, Some(&leg_uri), &[made.as_str()], "leg", 1), AT - 1)?;
        let w = walk(&kilns, &kept.records, SERVER)?;
        assert_eq!(w.hops.len(), 3 + records.len() + receipts.len() + 2, "{:#?}", w.hops);
        // Begun where kiln handed the leg's inputs on, and where the leg made the image from them.
        let began: Vec<&str> = w.began.iter().filter_map(|b| kept.records.get(b)).map(|c| c.custodian.as_str()).collect();
        assert_eq!(began.iter().filter(|c| **c == SERVER).count(), 3, "{began:?}");
        assert_eq!(began.iter().filter(|c| **c == leg_uri).count(), 1, "{began:?}");
        assert!(w.hops.iter().any(|h| h.custodian == leg_uri && h.kind == Kind::HandedOn && h.began && h.made_from.len() == receipts.len()), "{:#?}", w.hops);
        assert!(unanswered(&kept.records).is_empty(), "{:?}", unanswered(&kept.records));
        // The tarball changed in transit: what the runner got isn't what kiln handed on.
        let got = record(Kind::Received, &[thing("checkout", 'f')], None, Some(SERVER), &[handed.get(2).map(|h| h.hand_on.as_str()).unwrap_or_default()], "refused", 1);
        let refused = kept.take(&ca, &runner, &got, AT)?;
        let r = hop(&refused, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::Changed { .. })), "{r:?}");
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 12, W24-R27): an object a peer hands on. The holder signs
    /// its hand-on and sends it to kiln; the taker builds the same hand-on itself, whose digest is
    /// the same whoever builds it, and its receipt follows it: unbroken. A copy that hashed to
    /// something else breaks at that hop, naming the peer.
    #[test]
    fn w27_r08_a_peers_object_changes_hands_signed_by_both() -> anyhow::Result<()> {
        let ca = ca()?;
        let (fedora, jupiter) = (Signer::new(&ca, FEDORA)?, Signer::new(&ca, JUPITER)?);
        let want = format!("sha256:{}", sha('a'));
        let mut kept = Kept::default();
        let handed = kept.take(&ca, &fedora, &peer_object_hand_on(FEDORA, JUPITER, "acme/app", &want), AT - 2)?;
        let built = digest(PAYLOAD_CUSTODY, &payload(&peer_object_hand_on(FEDORA, JUPITER, "acme/app", &want))?);
        assert_eq!(built, handed, "the taker's build of the hand-on has the holder's digest");
        let got = kept.take(&ca, &jupiter, &peer_object_receipt(JUPITER, FEDORA, "acme/app", &want, &built), AT - 1)?;
        let w = walk(&got, &kept.records, SERVER)?;
        assert_eq!(w.hops.iter().map(|h| (h.kind, h.custodian.as_str())).collect::<Vec<_>>(), vec![(Kind::Received, JUPITER), (Kind::HandedOn, FEDORA)]);
        let wrong = kept.take(&ca, &jupiter, &peer_object_receipt(JUPITER, FEDORA, "acme/app", &sha('f'), &built), AT)?;
        let r = hop(&wrong, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::Changed { from, .. }) if from == &handed), "{r:?}");
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 12, its example): kiln run on jupiter for an agent session
    /// at epoch 1, held by fedora-2's runner. The session hands its workspace and its run to
    /// jupiter's runner, with a delegation to receive both for it; jupiter receives each for the
    /// session under that delegation and hands the run's output back, which the session receives:
    /// unbroken, every hop signed by both sides. A workspace changed on its way breaks at that hop;
    /// a receipt for the session under no delegation of the session's to that runner entitling it
    /// to receive the thing doesn't hold.
    #[test]
    fn w27_r08_a_reach_run_changes_hands_under_the_sessions_delegation() -> anyhow::Result<()> {
        const SESSION: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1";
        const HELD: &str = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1/machine/fedora-2/instance/01a174ed";
        let ca = ca()?;
        let (session, jupiter, fedora) = (Signer::new(&ca, HELD)?, Signer::new(&ca, JUPITER)?, Signer::new(&ca, FEDORA)?);
        let mut kept = Kept::default();
        let g = kept.take_as(&ca, &session, PAYLOAD_DELEGATION, &reach_delegation(HELD, JUPITER), AT - 20)?;
        assert_eq!(digest(PAYLOAD_DELEGATION, &payload(&session.says(&reach_delegation(HELD, JUPITER)))?), g, "the same session, epoch and runner: the same delegation");
        let ws = reach_workspace("kiln-1b2c3d4e5f", &sha('1'));
        let h_ws = kept.take(&ca, &session, &reach_hand_on(HELD, JUPITER, None, ws.clone(), Vec::new(), "the workspace of a run"), AT - 19)?;
        let r_ws = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), ws.clone(), &h_ws, Some(&g), "a workspace to run in"), AT - 18)?;
        let run = reach_run(1, &sha('2'));
        let h_run = kept.take(&ca, &session, &reach_hand_on(HELD, JUPITER, None, run.clone(), Vec::new(), "a run"), AT - 17)?;
        let r_run = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), run.clone(), &h_run, Some(&g), "a run to do"), AT - 16)?;
        let out = reach_output(1, &sha('3'));
        let h_out = kept.take(&ca, &jupiter, &reach_hand_on(JUPITER, SESSION, None, out.clone(), vec![r_run.clone()], "the run's output"), AT - 15)?;
        let r_out = kept.take(&ca, &session, &reach_receipt(HELD, JUPITER, None, out.clone(), &h_out, None, "its run's output"), AT - 14)?;
        let hops = |from: &str| -> anyhow::Result<Vec<(Kind, String)>> { Ok(walk(from, &kept.records, SERVER)?.hops.into_iter().map(|h| (h.kind, h.custodian)).collect()) };
        assert_eq!(hops(&r_out)?, vec![(Kind::Received, HELD.to_owned()), (Kind::HandedOn, JUPITER.to_owned()), (Kind::Received, JUPITER.to_owned()), (Kind::HandedOn, HELD.to_owned())]);
        assert_eq!(hops(&r_ws)?, vec![(Kind::Received, JUPITER.to_owned()), (Kind::HandedOn, HELD.to_owned())]);
        assert_eq!((receive_scope(&ws), receive_scope(&run)), (RECEIVE_WORKSPACE.to_owned(), RECEIVE_RUN.to_owned()));
        // Changed on its way: jupiter's receipt of what it got breaks at that hop.
        let changed = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), reach_workspace("kiln-1b2c3d4e5f", &sha('f')), &h_ws, Some(&g), "a workspace to run in"), AT - 13)?;
        let r = hop(&changed, &kept.records, SERVER);
        assert!(matches!(&r, Err(Break::Changed { from, .. }) if from == &h_ws), "{r:?}");
        // Taken for the session: under no delegation, one to another runner, one another signed,
        // and one that doesn't entitle receiving the thing.
        let not_entitled = |kept: &Kept, d: &str, why: &str| {
            let r = hop(d, &kept.records, SERVER);
            assert!(matches!(&r, Err(Break::NotEntitled { held_for, why: w, .. }) if held_for == SESSION && w.contains(why)), "{why}: {r:?}");
        };
        let none = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), run.clone(), &h_run, None, "a run to do"), AT - 12)?;
        not_entitled(&kept, &none, "no delegation");
        let to_fedora = kept.take_as(&ca, &session, PAYLOAD_DELEGATION, &reach_delegation(HELD, FEDORA), AT - 12)?;
        let other_runner = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), run.clone(), &h_run, Some(&to_fedora), "a run to do"), AT - 11)?;
        not_entitled(&kept, &other_runner, &format!("delegates to {FEDORA}"));
        let by_fedora = kept.take_as(&ca, &fedora, PAYLOAD_DELEGATION, &reach_delegation(FEDORA, JUPITER), AT - 11)?;
        let others = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), run.clone(), &h_run, Some(&by_fedora), "a run to do"), AT - 10)?;
        not_entitled(&kept, &others, &format!("signed by {FEDORA}"));
        let object = peer_object("acme/app", &sha('4'));
        let h_obj = kept.take(&ca, &session, &reach_hand_on(HELD, JUPITER, None, object.clone(), Vec::new(), "an object"), AT - 9)?;
        let wider = kept.take(&ca, &jupiter, &reach_receipt(JUPITER, HELD, Some(SESSION), object, &h_obj, Some(&g), "an object"), AT - 8)?;
        not_entitled(&kept, &wider, "receive:object");
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

    /// A session handed on to its next epoch, before the giver knows which process will hold it:
    /// whichever holds that epoch's key takes it; another epoch's doesn't.
    #[test]
    fn w27_r08_a_session_goes_to_its_next_epoch_whoever_holds_it() -> anyhow::Result<()> {
        let ca = ca()?;
        let (left, moved) = (Signer::new(&ca, LEFT)?, Signer::new(&ca, MOVED)?);
        let state = thing("session 01a1751c", '5');
        let next = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1";
        let mut kept = Kept::default();
        let handed = kept.take(&ca, &left, &record(Kind::HandedOn, std::slice::from_ref(&state), Some(next), None, &[], "s", 1), AT - 5)?;
        let from = "kiln://hyper-light/person/ada/agent/01a1751c/epoch/0";
        let got = kept.take(&ca, &moved, &record(Kind::Received, std::slice::from_ref(&state), None, Some(from), &[&handed], "s", 1), AT)?;
        assert_eq!(walk(&got, &kept.records, SERVER)?.hops.len(), 2);
        // Epoch 2's holder isn't epoch 1's.
        let two = Signer::new(&ca, "kiln://hyper-light/person/ada/agent/01a1751c/epoch/2/machine/adacachy/instance/01a14794")?;
        let wrong = kept.take(&ca, &two, &record(Kind::Received, std::slice::from_ref(&state), None, Some(from), &[&handed], "s", 1), AT)?;
        assert!(matches!(walk(&wrong, &kept.records, SERVER), Err(Break::NotTheTaker { .. })));
        assert!(names(next, MOVED) && !names(next, "kiln://hyper-light/person/ada/agent/01a1751c/epoch/10/machine/a/instance/b") && !names(FEDORA, &format!("{FEDORA}/machine/x")));
        // Its receipt, signed as it asks for epoch 1's certificate, says it's by the session at
        // epoch 1, without the process: read as its signer's. At another epoch, or another
        // session's, it isn't.
        let receipt = record(Kind::Received, std::slice::from_ref(&state), None, Some(from), &[&handed], "s", 1);
        let by = |by: &str| -> anyhow::Result<Result<Body, Break>> {
            let mut s = receipt.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(by.to_owned()));
            }
            Ok(body(&verify(&moved.sign(PAYLOAD_CUSTODY, &s)?, &ca.der, &[], AT)?))
        };
        assert!(matches!(by(next)?, Ok(b) if b.by == next));
        assert!(matches!(by("kiln://hyper-light/person/ada/agent/01a1751c/epoch/2")?, Err(Break::Unreadable { .. })));
        assert!(matches!(by("kiln://hyper-light/person/ada/agent/01a1751d/epoch/1")?, Err(Break::Unreadable { .. })));
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 4, the 14:18 drop): a hand-over's word is applied only
    /// by the process it's for, at the epoch kiln says it holds the session at: not by another
    /// instance (a new build it was passed on to), not at an epoch the process has left, not when
    /// the word changed or is about another session, and not when kiln's server didn't sign it.
    #[test]
    fn w27_r08_a_hand_overs_word_is_only_its_processs() -> anyhow::Result<()> {
        let ca = ca()?;
        let (server, cd) = (Signer::new(&ca, SERVER)?, Signer::new(&ca, CD)?);
        let command = br#"{"type":"handed_over"}"#;
        let word = |by: &Signer, epoch: u64, said: &[u8]| -> anyhow::Result<Checked> {
            let body = serde_json::to_value(Word { v: VERSION, by: String::new(), session: "s1".into(), machine: "fedora".into(), instance: "main".into(), epoch })?;
            let subject = Subject { name: "a hand-over's word about session s1".into(), digest: BTreeMap::from([("sha256".to_owned(), chain::hex(&Sha256::digest(said)))]), ..Subject::default() };
            Ok(verify(&by.sign(PAYLOAD_WORD, &Statement { subject: vec![subject], ..Statement::new(body) })?, &ca.der, &[], AT)?)
        };
        let for_main = word(&server, 3, command)?;
        assert!(addressed(&for_main, SERVER, "s1", command, "fedora", "main", Some(3)).is_ok_and(|w| w.instance == "main" && w.epoch == 3));
        assert!(addressed(&for_main, SERVER, "s1", command, "fedora", "main", None).is_ok(), "once it let go, the process alone is checked");
        let passed_on = addressed(&for_main, SERVER, "s1", command, "fedora", "01a17502", Some(4));
        assert!(matches!(&passed_on, Err(Misaddressed::AnotherProcess { instance, epoch: 3, .. }) if instance == "main"), "{passed_on:?}");
        assert_eq!(passed_on.map_err(|e| e.to_string()).err().as_deref(), Some("it's for fedora (instance main) at epoch 3, not this process, fedora (instance 01a17502) at epoch 4"));
        assert!(matches!(addressed(&for_main, SERVER, "s1", command, "fedora", "main", Some(4)), Err(Misaddressed::AnotherProcess { .. })), "an epoch it has left");
        assert!(matches!(addressed(&for_main, SERVER, "s1", br#"{"type":"move_to"}"#, "fedora", "main", Some(3)), Err(Misaddressed::Changed)));
        assert!(matches!(addressed(&for_main, SERVER, "s2", command, "fedora", "main", Some(3)), Err(Misaddressed::AnotherSession { .. })));
        assert!(matches!(addressed(&word(&cd, 3, command)?, SERVER, "s1", command, "fedora", "main", Some(3)), Err(Misaddressed::Unsigned(_))), "CD doesn't speak for the server");
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 5, the twin): an act is its session's only from the
    /// process holding it at the epoch kiln has it at, on every channel it comes by (a kiln
    /// operation, a machine run, a message to an agent, a change put up for review, a CI run):
    /// not from a copy left behind at the epoch it left, not from a former holder of the current
    /// epoch, not for another session, not changed on the way, and not when it isn't an act.
    #[test]
    fn w27_r08_an_act_is_only_its_holders_at_its_epoch() -> anyhow::Result<()> {
        let ca = ca()?;
        let (copy, holder) = (Signer::new(&ca, LEFT)?, Signer::new(&ca, MOVED)?);
        let former = Signer::new(&ca, "kiln://hyper-light/person/ada/agent/01a1751c/epoch/1/machine/adacachy/instance/01a14700")?;
        let held = Held { session: "01a1751c".into(), epoch: 1, by: Some(Holder { machine: "adacachy".into(), instance: "01a14794".into() }), since: AT - 60, transfer: "sha256:5e".into() };
        let act = |by: &Signer, epoch: u64, op: &str, what: &[u8], payload_type: &str| -> anyhow::Result<Checked> {
            let body = serde_json::to_value(Act { v: VERSION, by: String::new(), session: "01a1751c".into(), epoch, op: op.into() })?;
            let subject = Subject { name: format!("an act of session 01a1751c: {op}"), digest: BTreeMap::from([("sha256".to_owned(), chain::hex(&Sha256::digest(what)))]), ..Subject::default() };
            Ok(verify(&by.sign(payload_type, &Statement { subject: vec![subject], ..Statement::new(body) })?, &ca.der, &[], AT)?)
        };
        for op in ["goals.done", "a machine run", "agents.send", "ci.runs.start", "agents.land"] {
            let what = format!(r#"{{"op":"call","name":"{op}"}}"#);
            assert!(acted(&act(&holder, 1, op, what.as_bytes(), PAYLOAD_ACT)?, &held, what.as_bytes()).is_ok_and(|a| a.op == op), "{op}");
            let stale = acted(&act(&copy, 0, op, what.as_bytes(), PAYLOAD_ACT)?, &held, what.as_bytes());
            assert!(matches!(stale, Err(Unacted::Fenced(Fenced::Left { signed: 0, current: 1, .. }))), "{op}: {stale:?}");
            let gone = acted(&act(&former, 1, op, what.as_bytes(), PAYLOAD_ACT)?, &held, what.as_bytes());
            assert!(matches!(gone, Err(Unacted::Fenced(Fenced::NotTheHolder { epoch: 1, .. }))), "{op}: {gone:?}");
            assert!(matches!(acted(&act(&holder, 1, op, what.as_bytes(), PAYLOAD_ACT)?, &held, b"another act"), Err(Unacted::Changed)), "{op}");
            assert!(matches!(acted(&act(&holder, 1, op, what.as_bytes(), PAYLOAD_CUSTODY)?, &held, what.as_bytes()), Err(Unacted::Unsigned(_))), "{op}");
        }
        let other = Held { session: "01a1751d".into(), ..held.clone() };
        assert!(matches!(acted(&act(&holder, 1, "goals.done", b"x", PAYLOAD_ACT)?, &other, b"x"), Err(Unacted::AnotherSession { .. })));
        let said = acted(&act(&copy, 0, "goals.done", b"x", PAYLOAD_ACT)?, &held, b"x").map_err(|e| e.to_string()).err().unwrap_or_default();
        assert!(said.contains("signed at epoch 0 of session 01a1751c, which went on at epoch 1, on runner adacachy (instance 01a14794)") && said.contains("sha256:5e"), "{said}");
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
