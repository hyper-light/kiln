//! W24's work ledger (docs/ci.md W24-R02, R04, R30): what units of work produced, recorded once
//! under their strong keys ([`kiln_ci::work::key`]) and found again by two-phase lookup, and the
//! e-classes that hold the keys known to stand for the same outputs ([`kiln_ci::work::eclass`]).
//! Rows are shaped as the Records seam's (family, key, value) in `work_records`, so the ledger can
//! move onto Records (Amphora) unchanged. Test binaries' results (W24-R10) go through the same path
//! ([`record_test`], [`found_test`]).
//!
//! Every key is computed and looked up on one path, as in Dagger's cache: callers give [`record`]
//! and [`lookup`] a unit, never a key, and both key it with [`keyed`]. A lookup tries the unit's
//! strong key, then its class's canonical key: the unit with the upstream units it names by key
//! taken by class, and its definition in the normal form of the rewrite rules registered for its
//! scope ([`register`]). A record keeps the result under the strong key and puts that key in the
//! class of its outputs' digest ([`digest`]), so keys whose units produced byte-identical outputs
//! are one class, with the canonical key claimed into it. [`verified`] takes what verification saw
//! re-running a hit (W24-R23): a mismatch splits the class and quarantines its lineage. What
//! decides is `eclass`'s; this keeps its rows.
//!
//! `work.eclass`'s rows, each under its scope (`owner/repo|domain|`), so a class holds only its
//! own domain's entries (W24-R21):
//!
//! - `m|<key>`: a key's member ([`Member`]), and for one claimed into its class without outputs
//!   of its own, the key whose result a class-based hit through it serves;
//! - `c|<key>|<key>`: a claim, each way ([`Why`]);
//! - `l|<key>|<key>`: lineage, the second key's unit having taken the first by class;
//! - `r|<name>`: a rewrite rule, with its witness;
//! - `w|<canonical weak key>|<path set>`: the path sets of the units whose canonical form that
//!   key is and isn't their own, newest first, the newest [`PATH_SETS`] kept.
//!
//! Trust (W24-R21, R22) is enforced here, on the decisions of [`kiln_ci::work::trust`]: each scope
//! takes only its own domain's results, and only those built on what its domain reads ([`write`]);
//! an entry is never overwritten, a second runner's identical result is kept beside it as a
//! verification and a different one as evidence ([`Written`]); a lookup reads the domains its unit
//! may read, the most trusted first, and takes only what [`trust::admit`] does ([`lookup_as`]);
//! quotas evict each domain's entries alone ([`evict`]). What breaks the trust rules, a result
//! refused or a misplaced or forged entry passed over, is written to the audit log (AC06).
//!
//! Signatures (W24-R22) are [`kiln_ci::work::seal`]'s, made and checked here with a [`Seal`]: a
//! runner signs what it records with its identity key ([`RunnerKey`]), [`write`] checks that
//! against the key the server knows for the runner and countersigns it into the scope with the
//! server's own, and [`lookup_as`] checks both before it takes an entry.
//!
//! Verification (W24-R23) is [`kiln_ci::work::verify`]'s: whatever brings a key's result again (a
//! sampled hit run again, a cold rerun, another runner's ordinary run) is a reproduction or a
//! conflict, and a conflict has the entry judged again on all its runs ([`verdict`]): a unit whose
//! outputs differ is marked nondeterministic ([`nondeterministic`]), and an entry in doubt has its
//! lineage quarantined ([`quarantined`]), which lookups pass over.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, ECDSA_P384_SHA384_ASN1, ECDSA_P384_SHA384_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _, UnparsedPublicKey, VerificationAlgorithm};
use kiln_ci::work::eclass::{self, ClassId, Classes as _, Member, Refused, Rule, Rules, Split, Why, Witness};
use kiln_ci::work::key::{self, Key, PathSet, Unit};
use kiln_ci::work::seal::{self, Check, Checked};
use kiln_ci::work::tests as test_results;
use kiln_ci::work::tests::{Binary, Choice, Context, Kept, Marks, Ran};
use kiln_ci::work::trust::{self, Comparison, Domain, Found, Outcome, Passed, Quotas, Usage};
use kiln_ci::work::verify::{self, Verdict};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

/// The ledger's families (W24-R02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// A strong key's result.
    Result,
    /// A weak key's path sets.
    Weak,
    /// A path set, by its digest.
    PathSet,
    /// A warm workspace's snapshot (W24-R12).
    Snapshot,
    /// A lane's newest snapshot (W24-R11).
    Lane,
    /// Test keys' flaky marks, and each test binary's newest pass (W24-R10).
    Test,
    /// E-classes and their merges (W24-R30).
    EClass,
    /// An object of the content store an entry refers to (a step's tree and its files, W24-R07),
    /// keyed `<scope><sha256>`, its `at` when no result referring to it is reusable any more: the
    /// store keeps it while it's here (`services::cas::collect`), `services::memo::sweep` lets it go.
    Object,
    /// What's known of an entry beside it (W24-R22, R23): its reproductions, conflicts and
    /// quarantine, under `<scope><strong>|<kind>…`; and a unit's nondeterministic mark, under
    /// `<scope><weak>|nondeterministic`.
    Verify,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Result => "work.result",
            Family::Weak => "work.weak",
            Family::PathSet => "work.pathset",
            Family::Snapshot => "work.snapshot",
            Family::Lane => "work.lane",
            Family::Test => "work.test",
            Family::EClass => "work.eclass",
            Family::Object => "work.object",
            Family::Verify => "work.verify",
        }
    }
}

/// The most path sets a weak key keeps, newest first (W24-R04).
pub const PATH_SETS: usize = 8;

/// Where an entry belongs: its repository and trust domain (W24-R21).
pub fn scope(repo: &str, domain: &str) -> String {
    format!("{repo}|{domain}|")
}

/// The trust domain a scope's entries belong to: `<domain>` of `owner/repo|<domain>|`.
pub fn domain_of(scope: &str) -> &str {
    scope.strip_suffix('|').and_then(|s| s.rsplit_once('|')).map_or("", |(_, d)| d)
}

/// The domain a scope (`owner/repo|domain|`) is of, when it's one.
pub fn scope_domain(scope: &str) -> Option<Domain> {
    let (_, domain) = scope.strip_suffix('|')?.rsplit_once('|')?;
    Domain::parse(domain).ok()
}

/// What a unit produced (W24-R02).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recorded {
    pub outcome: String,
    pub exit: Option<i32>,
    /// Outputs by name, as content digests (files and trees in the content store) or values.
    pub outputs: BTreeMap<String, String>,
    /// Its log, by digest.
    pub logs: Option<String>,
    /// Its test reports, by digest.
    pub reports: Vec<String>,
    pub provenance: Provenance,
    /// The trust domain that wrote it, and those its inputs came from (W24-R21).
    pub domain: String,
    pub lineage: Vec<String>,
    /// The runners that reproduced it (W24-R23), and its signatures (W24-R22). A reproduction is a
    /// row of its own beside the entry, which is never rewritten: what a writer gives here isn't
    /// kept, and a lookup fills it in from those rows. Its signatures are its runner's and the
    /// server's ([`seal::Signatures`]); a writer gives its runner's, and [`write`] countersigns.
    pub verified_by: Vec<String>,
    pub signatures: Vec<String>,
    /// The bytes its outputs hold in the content store, for quotas (W24-R21).
    pub size: u64,
    /// The entries it reused, `<scope><strong>` each (W24-R22): its provenance lists them, each
    /// with its producer and verifiers a lookup away.
    pub reused: Vec<String>,
}

impl Recorded {
    fn outcome(&self) -> Outcome<'_> {
        Outcome { outcome: &self.outcome, exit: self.exit, outputs: &self.outputs }
    }
}

/// Where and how a result was made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Provenance {
    pub repo: String,
    pub run: String,
    pub leg: String,
    pub step: Option<u32>,
    pub attempt: u32,
    pub runner: String,
    pub commit: String,
    pub tree: String,
    pub ms: u64,
    pub cpu_ms: u64,
    /// When it finished, in seconds since the epoch.
    pub finished_at: i64,
}

/// What a unit produced, as one digest (W24-R30, [`eclass::output_digest`]): its outcome, exit
/// code, outputs and test reports, not its log or provenance. A class's id is it, and a unit after
/// it names its outputs by it (W24-R03), as one that names it by key takes it by class.
pub fn digest(r: &Recorded) -> String {
    eclass::output_digest(&r.outcome, r.exit, &r.outputs, &r.reports)
}

/// A row, written or replaced.
pub fn put(c: &Connection, family: Family, key: &str, value: &[u8], at: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO work_records (family, key, value, at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (family, key) DO UPDATE SET value = excluded.value, at = excluded.at",
        params![family.name(), key, value, at],
    )?;
    Ok(())
}

/// A row written only where its key holds none (W24-R22: an entry is never overwritten), and
/// whether it was: the database itself refuses the second write, however the writers race.
pub fn put_new(c: &Connection, family: Family, key: &str, value: &[u8], at: i64) -> rusqlite::Result<bool> {
    let n = c.execute("INSERT INTO work_records (family, key, value, at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (family, key) DO NOTHING", params![family.name(), key, value, at])?;
    Ok(n > 0)
}

/// A row's value.
pub fn get(c: &Connection, family: Family, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    c.query_row("SELECT value FROM work_records WHERE family = ?1 AND key = ?2", params![family.name(), key], |r| r.get(0)).optional()
}

/// A row gone.
pub fn delete(c: &Connection, family: Family, key: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM work_records WHERE family = ?1 AND key = ?2", params![family.name(), key])?;
    Ok(())
}

/// The rows whose keys start with `prefix`, highest `at` first.
pub fn scan(c: &Connection, family: Family, prefix: &str) -> rusqlite::Result<Vec<(String, Vec<u8>, i64)>> {
    let mut q = c.prepare_cached("SELECT key, value, at FROM work_records WHERE family = ?1 AND key >= ?2 AND key < ?3 ORDER BY at DESC, key")?;
    let rows = q.query_map(params![family.name(), prefix, format!("{prefix}\u{10FFFF}")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

fn weak_prefix(scope: &str, weak: &Key) -> String {
    format!("{scope}{}|", weak.hex())
}

/// A weak key's path sets, by digest, newest first.
pub fn path_sets(c: &Connection, scope: &str, weak: &Key) -> rusqlite::Result<Vec<String>> {
    listed(c, Family::Weak, &weak_prefix(scope, weak))
}

/// The path sets listed under `prefix`, newest first.
fn listed(c: &Connection, family: Family, prefix: &str) -> rusqlite::Result<Vec<String>> {
    Ok(scan(c, family, prefix)?.into_iter().map(|(_, id, _)| String::from_utf8_lossy(&id).into_owned()).collect())
}

/// Lists a path set under `prefix` as the newest, keeping the newest [`PATH_SETS`].
fn list(c: &Connection, family: Family, prefix: &str, id: &str) -> rusqlite::Result<()> {
    let next = scan(c, family, prefix)?.first().map_or(1, |(_, _, at)| at + 1);
    put(c, family, &format!("{prefix}{id}"), id.as_bytes(), next)?;
    for (old, _, _) in scan(c, family, prefix)?.iter().skip(PATH_SETS) {
        delete(c, family, old)?;
    }
    Ok(())
}

fn path_set(c: &Connection, id: &str) -> anyhow::Result<Option<PathSet>> {
    Ok(get(c, Family::PathSet, id)?.map(|set| serde_json::from_slice(&set)).transpose()?)
}

/// How the ledger keys a unit (W24-R30): the one path [`record`] and [`lookup`] share, as Dagger's
/// cache has one for key calculation and lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed {
    /// Its weak key: the unit as written (W24-R01).
    pub weak: Key,
    /// Its canonical form ([`eclass::canonical`]): the upstream units it names by key taken by
    /// class, its definition in the normal form of its scope's rules.
    pub canonical: eclass::Canonical,
    /// The upstream units it took by class, by key.
    pub took: Vec<Key>,
}

/// Keys a unit in `scope` (W24-R30): its weak key, and its canonical form's under the rules
/// registered for the scope and the classes its upstream units named by key are in now.
pub fn keyed(c: &Connection, scope: &str, unit: &Unit) -> anyhow::Result<Keyed> {
    let (rules, rows) = (rules(c, scope)?, Rows { c, scope });
    let mut held = BTreeMap::new();
    for k in unit.upstream.values().filter_map(|v| eclass::reference(v)) {
        if let Some(m) = rows.entry(&k)? {
            held.insert(k, m);
        }
    }
    let canonical = eclass::canonical(unit, &rules, &|k| held.get(k).and_then(Member::rep));
    let took = match &canonical.why {
        Why::Canonical { by_class, .. } => by_class.iter().filter_map(|name| unit.upstream.get(name)).filter_map(|v| eclass::reference(v)).collect(),
        Why::Claim(_) => Vec::new(),
    };
    Ok(Keyed { weak: key::weak(unit), canonical, took })
}

/// A row of [`Family::Verify`] beside an entry (W24-R22, R23).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Verification {
    /// `reproduced` (another runner gave the same result) or `conflict` (another result, refused).
    pub kind: String,
    pub runner: String,
    pub run: String,
    pub leg: String,
    pub attempt: u32,
    /// When, in seconds since the epoch.
    pub at: i64,
    /// How the result compared with the entry's.
    pub comparison: Option<Comparison>,
    /// The result it gave: a reproduction's, signed as the entry is (a protected unit counts its
    /// runners by their keys, W24-R22), or a conflicting one, kept as evidence.
    pub result: Option<Recorded>,
}

/// What recording did (W24-R21, R22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// A new entry under its strong key.
    New(Key),
    /// The entry the key holds has the same outcome and outputs: by another runner, a reproduction,
    /// kept beside it as a verification (W24-R23); by the same runner, nothing new.
    Reproduced { strong: Key, by_another: bool },
    /// The key holds another result: entries are never overwritten, so this one is refused and
    /// kept beside the entry as evidence, and the entry judged again on all its runs (W24-R23):
    /// `verdict` says what that did.
    Conflict { strong: Key, comparison: Comparison, verdict: Verdict },
    /// Not the scope's to take (W24-R21): another domain's result, or one built on what the
    /// scope's domain doesn't read. Nothing is written to the ledger, and the audit log says so.
    Refused(String),
}

fn verify_prefix(scope: &str, strong: &Key) -> String {
    format!("{scope}{}|", strong.hex())
}

/// The reproductions beside an entry, each runner once (W24-R23).
fn reproductions(c: &Connection, scope: &str, strong: &Key) -> anyhow::Result<Vec<Verification>> {
    let mut out: Vec<Verification> = Vec::new();
    for (_, v, _) in scan(c, Family::Verify, &format!("{}by|", verify_prefix(scope, strong)))? {
        let v: Verification = serde_json::from_slice(&v)?;
        if !out.iter().any(|o| o.runner == v.runner) {
            out.push(v);
        }
    }
    Ok(out)
}

/// The runners that produced an entry: the one that recorded it, then each that reproduced it.
fn runners(c: &Connection, scope: &str, strong: &Key, entry: &Recorded) -> anyhow::Result<Vec<String>> {
    let mut out = vec![entry.provenance.runner.clone()];
    for v in reproductions(c, scope, strong)? {
        if !out.contains(&v.runner) {
            out.push(v.runner);
        }
    }
    Ok(out)
}

/// A row of the audit log (W19-R05) for what the ledger refused or passed over as a breach of the
/// trust rules (W24-R21, AC06). It's no request's, so it's a row of its own.
fn audit(c: &Connection, principal: &str, action: &str, target: &str, detail: &str) -> rusqlite::Result<()> {
    audit_as(c, principal, action, target, "refused", detail)
}

/// A row of the audit log with how it ended: what the ledger did on its own (W24-R23's
/// quarantines) says so there, and why.
fn audit_as(c: &Connection, principal: &str, action: &str, target: &str, result: &str, detail: &str) -> rusqlite::Result<()> {
    let principal = if principal.is_empty() { "kiln" } else { principal };
    c.execute("INSERT INTO audit2 (ts, principal, action, target, result, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![crate::util::now_unix(), principal, action, target, result, detail])?;
    Ok(())
}

/// Why `result` isn't `scope`'s to take (W24-R21), when it isn't.
fn refusal(scope: &str, result: &Recorded) -> Option<String> {
    let Some(domain) = scope_domain(scope) else { return Some(format!("`{scope}` isn't a scope (owner/repo|domain|)")) };
    match Domain::parse(&result.domain) {
        Ok(writer) if writer.may_write(&domain) => trust::lineage_fits(&domain, &result.lineage).err(),
        Ok(writer) => Some(format!("a {writer} result can't be recorded among {domain}'s entries: promotion is by reproduction only (W24-R21)")),
        Err(e) => Some(format!("its domain: {e}")),
    }
}

/// Records what a unit produced (W24-R02, R21, R22, R30), keyed on the ledger's one path
/// ([`keyed`]): its path set kept by digest and listed under its weak key (the newest
/// [`PATH_SETS`] kept), its result under its strong key, once, and that key in the class of the
/// outputs it was recorded with, its canonical key claimed into it when that isn't its own. Only
/// successes are to be recorded (W24-R07).
///
/// The scope takes only its own domain's results, built on nothing its domain doesn't read
/// ([`trust::lineage_fits`]): a result of another domain gets there by reproduction, never by
/// copying. A refusal writes nothing to the ledger and a row to the audit log (AC06).
///
/// An entry is never overwritten: the result row is inserted only where its key holds none
/// ([`put_new`]). Recorded again with the same outcome and outputs, it gains a verification by the
/// new runner; with another result, the write is refused and kept as evidence, and the key's class
/// splits where it is, as verification's mismatch would split it (W24-R30). Refusing a different
/// result, rather than keeping both, leaves each strong key one result to serve: two would make
/// every lookup choose, and a forged second one could be chosen. Replacing it, as the ledger did
/// before, would let the last writer decide what everyone after reuses.
///
/// With a [`Seal`] (W24-R22), the result's runner signature is checked against the key the server
/// knows for its runner, a bad one refused (and audited), and the result countersigned into the
/// scope before it's kept, as an entry or as a reproduction beside one.
pub fn write(c: &Connection, seal: Option<&Seal>, scope: &str, unit: &Unit, read: &PathSet, result: &Recorded) -> anyhow::Result<Written> {
    let refuse = |why: String| -> anyhow::Result<Written> {
        let p = &result.provenance;
        audit(c, &p.runner, "work.record", scope, &format!("{why} (run {}, leg {}, attempt {})", p.run, p.leg, p.attempt))?;
        tracing::warn!(scope, why = %why, run = %p.run, leg = %p.leg, "a result refused by the work ledger (W24-R21, R22)");
        Ok(Written::Refused(why))
    };
    if let Some(why) = refusal(scope, result) {
        return refuse(why);
    }
    let tx = crate::store::tx(c)?;
    let keyed = keyed(&tx, scope, unit)?;
    let weak = &keyed.weak;
    let strong = key::strong(weak, read);
    let result = match seal {
        None => Cow::Borrowed(result),
        Some(seal) => match seal.countersign(scope, &strong, result)? {
            Ok(signatures) => Cow::Owned(Recorded { signatures, ..result.clone() }),
            Err(why) => {
                // Nothing of it is kept: the transaction is let go before the audit log says why.
                drop(tx);
                return refuse(why);
            }
        },
    };
    let result = result.as_ref();
    let now = crate::util::now_unix();
    let set = serde_json::to_vec(read)?;
    let id = crate::util::sha256_hex(&set);
    put(&tx, Family::PathSet, &id, &set, now)?;
    list(&tx, Family::Weak, &weak_prefix(scope, weak), &id)?;
    let at = format!("{scope}{}", strong.hex());
    let entry = Recorded { verified_by: Vec::new(), ..result.clone() };
    let written = if put_new(&tx, Family::Result, &at, &serde_json::to_vec(&entry)?, now)? {
        Written::New(strong)
    } else {
        let kept = get(&tx, Family::Result, &at)?.ok_or_else(|| anyhow::anyhow!("{strong}'s entry went while it was recorded again"))?;
        let kept: Recorded = serde_json::from_slice(&kept)?;
        let p = &result.provenance;
        let verification = |kind: &str, comparison: Option<Comparison>, evidence: Option<Recorded>| Verification { kind: kind.to_owned(), runner: p.runner.clone(), run: p.run.clone(), leg: p.leg.clone(), attempt: p.attempt, at: now, comparison, result: evidence };
        match trust::compare(&kept.outcome(), &result.outcome()) {
            Comparison::Same => {
                let known = runners(&tx, scope, &strong, &kept)?;
                let by_another = !p.runner.is_empty() && !known.contains(&p.runner);
                if by_another {
                    let reproduced = Recorded { verified_by: Vec::new(), ..result.clone() };
                    put(&tx, Family::Verify, &format!("{}by|{}", verify_prefix(scope, &strong), p.runner), &serde_json::to_vec(&verification("reproduced", Some(Comparison::Same), Some(reproduced)))?, now)?;
                    // A reproduction of an entry already in doubt tells nondeterminism from corruption.
                    if !conflicts(&tx, scope, &strong)?.is_empty() {
                        judge_again(&tx, scope, weak, &strong)?;
                    }
                }
                Written::Reproduced { strong, by_another }
            }
            comparison => {
                let evidence = Recorded { verified_by: Vec::new(), ..result.clone() };
                let row = format!("{}conflict|{}|{}|{}|{}", verify_prefix(scope, &strong), p.runner, p.run, p.leg, p.attempt);
                put(&tx, Family::Verify, &row, &serde_json::to_vec(&verification("conflict", Some(comparison.clone()), Some(evidence)))?, now)?;
                tracing::warn!(scope, strong = %strong, ?comparison, run = %p.run, leg = %p.leg, "a result differs from the entry its key holds; the entry stands (W24-R22)");
                let verdict = judge_again(&tx, scope, weak, &strong)?;
                Written::Conflict { strong, comparison, verdict }
            }
        }
    };
    join(&mut Rows { c: &tx, scope }, &keyed, &strong, read, &id, &result.domain, &digest(result))?;
    tx.commit()?;
    Ok(written)
}

/// [`write`], for callers that only need the strong key (W24-R22). A new entry and a reproduction
/// give it; a conflict (the key holds another result, which stands, this one kept as evidence) and
/// a refusal (not the scope's to take, audited) are errors saying so, so a caller that kept the
/// result elsewhere too (W08-R10's own table) learns it isn't the one reused here. It neither
/// signs nor checks signatures: it's W08-R10's leg reuse, which no runner signs yet.
pub fn record(c: &Connection, scope: &str, unit: &Unit, read: &PathSet, result: &Recorded) -> anyhow::Result<Key> {
    match write(c, None, scope, unit, read, result)? {
        Written::New(strong) | Written::Reproduced { strong, .. } => Ok(strong),
        Written::Conflict { strong, comparison, verdict } => anyhow::bail!("{strong} already holds another result ({comparison:?}); entries are never overwritten (W24-R22), and it's judged again: {}", verdict.why()),
        Written::Refused(why) => anyhow::bail!("{why}"),
    }
}

/// Puts a recorded key in the class of its outputs (W24-R30), notes what its unit took by class,
/// and claims its canonical key into its class when that isn't its own.
fn join(rows: &mut Rows, keyed: &Keyed, strong: &Key, read: &PathSet, set: &str, writer: &str, produced: &str) -> anyhow::Result<()> {
    match eclass::producing(strong, rows.entry(strong)?.as_ref(), writer, produced) {
        Ok(member) => rows.place(strong, &Placed { member, source: None })?,
        // Its unit produced other outputs before, or its class holds others: the class splits
        // where it is, as verification's mismatch would split it (W24-R23).
        Err(Refused::Conflict { .. }) => {
            eclass::split(rows, strong, Some(produced))?;
        }
        // Nondeterministic already: it joins no class.
        Err(Refused::Nondeterministic(_)) => {}
        Err(e) => return Err(e.into()),
    }
    for up in &keyed.took {
        rows.pair("l", up, strong, b"")?;
    }
    let canonical = key::strong(&keyed.canonical.key, read);
    if canonical == *strong {
        return Ok(());
    }
    let Some(own) = rows.entry(strong)?.filter(Member::claimable) else { return Ok(()) };
    list(rows.c, Family::EClass, &rows.key("w", &format!("{}|", keyed.canonical.key.hex())), set)?;
    match rows.placed(&canonical)? {
        None => {
            let member = Member { class: own.class, writer: writer.to_owned(), produced: None, quarantined: false, nondeterministic: false };
            rows.place(&canonical, &Placed { member, source: Some(strong.hex()) })?;
            rows.claim(&canonical, strong, &keyed.canonical.why)?;
        }
        // Quarantined or nondeterministic, it takes no claim.
        Some(p) if !p.member.claimable() => {}
        Some(p) => match eclass::joined(&p.member.class, &own.class) {
            Ok(_) => {
                if p.source.is_none() && !p.member.anchored() {
                    rows.place(&canonical, &Placed { member: p.member, source: Some(strong.hex()) })?;
                }
                rows.claim(&canonical, strong, &keyed.canonical.why)?;
            }
            // Its canonical form's class holds other outputs: the rules, or the classes it took
            // upstream units by, made one what isn't. That class splits, its lineage quarantined.
            Err(_) => {
                eclass::split(rows, &canonical, None)?;
            }
        },
    }
    Ok(())
}

/// A result found for a unit (W24-R04, R21, R30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The strong key it was found under: the unit's own, or its canonical form's. A unit after it
    /// that takes its outputs before they're materialized names it by this key ([`eclass::refer`]).
    pub key: Key,
    /// The domain whose entries held it.
    pub domain: Domain,
    /// The entry, its `verified_by` filled in.
    pub recorded: Recorded,
    /// How it was found by class, when it wasn't under the unit's own key: what verification
    /// re-checks (W24-R23).
    pub class: Option<ByClass>,
    /// The outputs its unit's runs disagree on, when verification marked the unit nondeterministic
    /// (W24-R23): shown with the hit, and early cutoff off for it, since its outputs' digests don't
    /// stand for what it does.
    pub nondeterministic: Option<Vec<String>>,
}

/// How a hit was found by class (W24-R30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByClass {
    /// The class it was found in: its outputs' digest.
    pub class: ClassId,
    /// The key whose unit produced the result.
    pub source: Key,
    /// What made the unit's canonical form: the rules that rewrote it, the upstream units it took
    /// by class.
    pub why: Why,
    /// The upstream units it took by class, by key.
    pub took: Vec<Key>,
}

/// What a lookup found (W24-R04, R21, AC06).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lookup {
    pub hit: Option<Hit>,
    /// Each entry it found and didn't take, and why, for the unit's report (W24-R28); those that
    /// break the trust rules are in the audit log too.
    pub passed_over: Vec<String>,
}

/// The entries a weak key's path sets lead to in `scope`, newest first, each with the strong key
/// that found it, until `take` says which to stop at.
fn candidates(c: &Connection, scope: &str, weak: &Key, now: &dyn Fn(&PathSet) -> PathSet, take: &mut dyn FnMut(Key, Recorded) -> anyhow::Result<Option<Hit>>) -> anyhow::Result<Option<Hit>> {
    for id in path_sets(c, scope, weak)? {
        let Some(set) = get(c, Family::PathSet, &id)? else { continue };
        let recorded: PathSet = serde_json::from_slice(&set)?;
        let strong = key::strong(weak, &now(&recorded));
        if let Some(found) = get(c, Family::Result, &format!("{scope}{}", strong.hex()))?
            && let Some(hit) = take(strong, serde_json::from_slice(&found)?)?
        {
            return Ok(Some(hit));
        }
    }
    Ok(None)
}

/// Whether a unit of `reader` takes `entry`, found under `strong` (for the unit of `weak`) among
/// `domain`'s entries: its hit, or why not. With a [`Seal`], its signatures are checked first
/// (W24-R22), and a protected unit counts the runners that produced it by their keys: the entry's
/// runner and each that reproduced it, whose signatures, its runner's and the server's, both
/// verify. A quarantined entry is passed over (W24-R23). A breach of the trust rules is audited
/// (AC06).
#[allow(clippy::too_many_arguments)]
fn judge(c: &Connection, seal: Option<&Seal>, scope: &str, domain: &Domain, reader: &Domain, weak: &Key, strong: Key, mut entry: Recorded) -> anyhow::Result<Result<Hit, Passed>> {
    let named = runners(c, scope, &strong, &entry)?;
    let mut keyed = Vec::new();
    let mut verdict = Ok(());
    if let Some(seal) = seal {
        let (checked, key) = seal.check(scope, &strong, &entry);
        verdict = seal::judge(reader, domain, &checked);
        keyed.extend(key);
        for v in reproductions(c, scope, &strong)? {
            let Some(again) = v.result.filter(|r| trust::compare(&entry.outcome(), &r.outcome()) == Comparison::Same) else { continue };
            if let (Checked { runner: Check::Valid, server: Check::Valid }, Some(key)) = seal.check(scope, &strong, &again) {
                keyed.push(key);
            }
        }
    }
    let runners = if seal.is_some() { &keyed } else { &named };
    let quarantine = quarantined(c, scope, &strong)?;
    let found = Found { scope: domain, writer: &entry.domain, lineage: &entry.lineage, runners: runners.iter().map(String::as_str).collect(), quarantined: quarantine.as_ref().map(|q| q.why.as_str()) };
    if let Err(why) = verdict.and_then(|()| trust::admit(reader, &found)) {
        if why.is_breach() {
            audit(c, "kiln", "work.lookup", &format!("{scope}{strong}"), &format!("passed over for a {reader} unit: {why}"))?;
            tracing::warn!(scope, strong = %strong, why = %why, "an entry that breaks the trust rules passed over (W24-R21, R22)");
        }
        return Ok(Err(why));
    }
    entry.verified_by = named.into_iter().skip(1).collect();
    let nondeterministic = nondeterministic(c, scope, weak)?.map(|m| m.outputs);
    Ok(Ok(Hit { key: strong, domain: domain.clone(), recorded: entry, class: None, nondeterministic }))
}

/// The ledger's one lookup path (W24-R04, R21, R30): a unit, keyed in `scope` ([`keyed`]), found
/// among `domain`'s entries as a unit of `reader` takes them ([`judge`], its signatures checked
/// with `seal` when there's one). First under its strong
/// key: its weak key's path sets, newest first, each made a strong key from what is at its paths
/// now (`now` reads them), and the first recorded result `reader` takes. Then by class, under its
/// canonical key: the path sets of the units whose canonical form it keys, each made the canonical
/// form's strong key the same way, and the first whose member serves a class-based hit, with its
/// class's result, if `reader` takes that. What it passes over goes to `passed`.
#[allow(clippy::too_many_arguments)]
fn find(c: &Connection, seal: Option<&Seal>, scope: &str, domain: &Domain, reader: &Domain, unit: &Unit, now: &dyn Fn(&PathSet) -> PathSet, passed: &mut dyn FnMut(Key, Passed)) -> anyhow::Result<Option<Hit>> {
    let keyed = keyed(c, scope, unit)?;
    let own = candidates(c, scope, &keyed.weak, now, &mut |strong, entry| {
        Ok(match judge(c, seal, scope, domain, reader, &keyed.weak, strong, entry)? {
            Ok(hit) => Some(hit),
            Err(why) => {
                passed(strong, why);
                None
            }
        })
    })?;
    if own.is_some() {
        return Ok(own);
    }
    let rows = Rows { c, scope };
    let canonical = &keyed.canonical.key;
    // The canonical form's own path sets, when it isn't the unit's (those were tried), then those
    // of the units claimed into its classes.
    let mut ids = if *canonical == keyed.weak { Vec::new() } else { path_sets(c, scope, canonical)? };
    ids.extend(listed(c, Family::EClass, &rows.key("w", &format!("{}|", canonical.hex())))?);
    let mut tried = BTreeSet::new();
    for id in ids {
        if !tried.insert(id.clone()) {
            continue;
        }
        let Some(read) = path_set(c, &id)? else { continue };
        let at = key::strong(canonical, &now(&read));
        let Some(p) = rows.placed(&at)?.filter(|p| p.member.serves()) else { continue };
        let source = if p.member.anchored() { Some(at) } else { p.source.as_deref().and_then(eclass::parse_key) };
        let Some(source) = source else { continue };
        let Some(found) = get(c, Family::Result, &format!("{scope}{}", source.hex()))? else { continue };
        let recorded: Recorded = serde_json::from_slice(&found)?;
        // What its source holds now must be its class's outputs.
        if p.member.class.output() != Some(digest(&recorded).as_str()) {
            continue;
        }
        let hit = match judge(c, seal, scope, domain, reader, &keyed.weak, source, recorded)? {
            Ok(hit) => hit,
            Err(why) => {
                passed(source, why);
                continue;
            }
        };
        let class = ByClass { class: p.member.class, source, why: keyed.canonical.why.clone(), took: keyed.took.clone() };
        return Ok(Some(Hit { key: at, class: Some(class), ..hit }));
    }
    Ok(None)
}

/// Finds a unit's result in `scope` (W24-R04, R30), as a unit of the scope's own domain takes it
/// ([`trust::admit`]: one its domain wrote, not built on what's below it; for a protected scope,
/// one two runners produced), on the ledger's one path ([`find`]): under its strong key, then by
/// class. The entry's `verified_by` is filled in. Signatures aren't checked: this is [`record`]'s
/// reader (W08-R10's leg reuse); [`lookup_as`] checks them.
pub fn lookup(c: &Connection, scope: &str, unit: &Unit, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Option<Hit>> {
    let Some(domain) = scope_domain(scope) else { return Ok(None) };
    find(c, None, scope, &domain, &domain, unit, now, &mut |strong, why| tracing::debug!(scope, strong = %strong, why = %why, "an entry passed over"))
}

/// Two-phase lookup for a unit of `reader`'s domain (W24-R04, R21): each domain it reads, the most
/// trusted first ([`Domain::lookups`]), keyed as that domain keys the unit (its trust component
/// that domain) on the ledger's one path ([`find`]), and the first entry [`trust::admit`] takes,
/// its signatures checked with `seal` (W24-R22). What it passes over, and why, it says.
pub fn lookup_as(c: &Connection, seal: Option<&Seal>, repo: &str, reader: &Domain, unit: &Unit, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Lookup> {
    let mut out = Lookup::default();
    for domain in reader.lookups() {
        let sc = scope(repo, &domain.to_string());
        let theirs = Unit { trust: domain.to_string(), ..unit.clone() };
        let passed = &mut out.passed_over;
        let hit = find(c, seal, &sc, &domain, reader, &theirs, now, &mut |strong, why| passed.push(format!("{domain} entry {strong}: {why}")))?;
        if hit.is_some() {
            out.hit = hit;
            break;
        }
    }
    Ok(out)
}

/// What quotas say must go (W24-R21, AC12): `repo`'s entries, each domain weighed alone by
/// [`trust::evict`] (a byte is the entry's row and its outputs, a hit saves its unit's time), its
/// results deleted with what's beside them. The keys of the entries gone.
pub fn evict(c: &Connection, repo: &str, quotas: &Quotas) -> anyhow::Result<Vec<String>> {
    let prefix = format!("{repo}|");
    let mut keys = Vec::new();
    let mut usage = Vec::new();
    for (k, v, at) in scan(c, Family::Result, &prefix)? {
        let Some(domain) = k.strip_prefix(&prefix).and_then(|rest| rest.split_once('|')).and_then(|(d, _)| Domain::parse(d).ok()) else { continue };
        let entry: Recorded = serde_json::from_slice(&v).unwrap_or_default();
        usage.push(Usage { domain, bytes: (v.len() as u64).saturating_add(entry.size), saved_ms: entry.provenance.ms, hits: 0, last_used: at });
        keys.push(k);
    }
    let mut gone = Vec::new();
    for i in trust::evict(&usage, quotas) {
        let Some(k) = keys.get(i) else { continue };
        delete(c, Family::Result, k)?;
        for (v, _, _) in scan(c, Family::Verify, &format!("{k}|"))? {
            delete(c, Family::Verify, &v)?;
        }
        gone.push(k.clone());
    }
    Ok(gone)
}

// ------------------------------------------------------------------ signatures (W24-R22)

/// The keys of W24-R22's signatures: the server's own (its attestation key, W18-R02), which
/// countersigns what's written and checks what's read, and the runners' identity keys as the
/// server knows them, by runner name ([`runner_keys`]).
pub struct Seal<'a> {
    server: &'a crate::ci::attest::Signer,
    /// The server's public key (SubjectPublicKeyInfo, DER).
    server_spki: Vec<u8>,
    runners: &'a BTreeMap<String, String>,
}

/// The runners' identity keys the server knows, by name: the fingerprints in reach's directory
/// (each the SHA-256 of a runner's reach identity key, which signs its results).
pub fn runner_keys(hub: &crate::hub::Hub) -> BTreeMap<String, String> {
    hub.reach_directory().into_iter().filter_map(|r| Some((r.name, r.fingerprint?))).collect()
}

/// A P-256 key's uncompressed point is the last 65 bytes of its SubjectPublicKeyInfo; a P-384
/// key's, the last 97.
const P256_POINT: usize = 65;
const P384_POINT: usize = 97;

/// Whether `sig` (DER) over `msg` verifies under the EC key `spki`, of `alg` and its point's size.
fn verifies(alg: &'static dyn VerificationAlgorithm, point: usize, spki: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let point = spki.get(spki.len().saturating_sub(point)..).unwrap_or_default();
    UnparsedPublicKey::new(alg, point).verify(msg, sig).is_ok()
}

/// What a result claims as recorded in `scope` under `strong` (W24-R22).
fn claim<'a>(scope: &'a str, strong: &'a Key, r: &'a Recorded) -> seal::Claim<'a> {
    let p = &r.provenance;
    seal::Claim {
        scope,
        strong,
        outcome: &r.outcome,
        exit: r.exit,
        outputs: &r.outputs,
        logs: r.logs.as_deref(),
        reports: &r.reports,
        domain: &r.domain,
        lineage: &r.lineage,
        reused: &r.reused,
        runner: &p.runner,
        run: &p.run,
        leg: &p.leg,
        step: p.step,
        attempt: p.attempt,
        commit: &p.commit,
        tree: &p.tree,
    }
}

impl<'a> Seal<'a> {
    /// The server's key and the runners' keys it knows (`runner_keys`).
    pub fn new(server: &'a crate::ci::attest::Signer, runners: &'a BTreeMap<String, String>) -> anyhow::Result<Seal<'a>> {
        let (_, server_spki) = crate::ci::signing::der_of(&server.public_pem()).ok_or_else(|| anyhow::anyhow!("the server's key isn't PEM"))?;
        Ok(Seal { server, server_spki, runners })
    }

    /// `result`'s signatures as kept in `scope` under `strong`: its runner's, checked against the
    /// key the server knows for that runner (dropped, with a warning, where it knows none: the
    /// countersignature then says the runner signed nothing), and the server's over both. Why not,
    /// when the runner's signature is malformed, by another key than its runner's, or bad.
    fn countersign(&self, scope: &str, strong: &Key, result: &Recorded) -> anyhow::Result<Result<Vec<String>, String>> {
        let sigs = match seal::Signatures::parse(&result.signatures) {
            Ok(s) => s,
            Err(e) => return Ok(Err(format!("its signatures: {e} (W24-R22)"))),
        };
        let digest = seal::digest(&claim(scope, strong, result));
        let name = &result.provenance.runner;
        let runner = match (sigs.runner, self.runners.get(name)) {
            (None, _) => None,
            (Some(r), None) => {
                tracing::warn!(runner = %name, key = %r.keyid, "a result signed with a key the server doesn't know for its runner: countersigned as its runner's unsigned one (W24-R22)");
                None
            }
            (Some(r), Some(known)) if *known != r.keyid => return Ok(Err(format!("signed with key {}, which isn't {name}'s ({known}) (W24-R22)", r.keyid))),
            (Some(r), Some(_)) if !verifies(&ECDSA_P384_SHA384_ASN1, P384_POINT, &r.spki, &seal::runner_message(&digest), &r.sig) => return Ok(Err(format!("{name}'s signature doesn't verify (W24-R22)"))),
            (Some(r), Some(_)) => Some(r),
        };
        let sig = self.server.sign(&seal::server_message(&digest, runner.as_ref()))?;
        Ok(Ok(seal::Signatures { runner, server: Some(seal::ServerSig { keyid: self.server.keyid.clone(), sig }) }.to_list()))
    }

    /// What checking `entry`'s signatures, as recorded in `scope` under `strong`, finds, with its
    /// runner's key id when both verify.
    fn check(&self, scope: &str, strong: &Key, entry: &Recorded) -> (Checked, Option<String>) {
        let sigs = match seal::Signatures::parse(&entry.signatures) {
            Ok(s) => s,
            Err(e) => return (Checked { runner: Check::Invalid(e.clone()), server: Check::Invalid(e) }, None),
        };
        let digest = seal::digest(&claim(scope, strong, entry));
        let server = match &sigs.server {
            None => Check::Missing,
            Some(s) if s.keyid != self.server.keyid => Check::Invalid(format!("countersigned by key {}, not this server's", s.keyid)),
            Some(s) if verifies(&ECDSA_P256_SHA256_ASN1, P256_POINT, &self.server_spki, &seal::server_message(&digest, sigs.runner.as_ref()), &s.sig) => Check::Valid,
            Some(_) => Check::Invalid("it's over another claim, scope or runner signature".into()),
        };
        let runner = match &sigs.runner {
            None => Check::Missing,
            Some(r) if verifies(&ECDSA_P384_SHA384_ASN1, P384_POINT, &r.spki, &seal::runner_message(&digest), &r.sig) => Check::Valid,
            Some(_) => Check::Invalid("it's over another claim".into()),
        };
        let key = sigs.runner.filter(|_| runner == Check::Valid && server == Check::Valid).map(|r| r.keyid);
        (Checked { runner, server }, key)
    }
}

/// A runner's identity key (W24-R22): its reach identity's P-384 key, made on the machine and never
/// leaving it (`<state>/reach/identity.key`, docs/reach.md), which signs the results it records.
pub struct RunnerKey {
    pair: EcdsaKeyPair,
    spki: Vec<u8>,
    /// Its id: the SHA-256 of its public key, the runner's fingerprint in reach's directory.
    pub keyid: String,
}

impl RunnerKey {
    /// The key reach keeps in `dir`, or `None` where there's none (reach off, or a job pod's
    /// runner): its results then stand on the server's countersignature alone.
    pub fn open(dir: &Path) -> anyhow::Result<Option<RunnerKey>> {
        let path = dir.join("identity.key");
        let pem = match std::fs::read_to_string(&path) {
            Ok(pem) => pem,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => anyhow::bail!("{}: {e}", path.display()),
        };
        let (_, der) = crate::ci::signing::der_of(&pem).ok_or_else(|| anyhow::anyhow!("{} isn't PEM", path.display()))?;
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_ASN1_SIGNING, &der).map_err(|_| anyhow::anyhow!("{} isn't a P-384 key", path.display()))?;
        let spki = pair.public_key().as_der().map_err(|_| anyhow::anyhow!("the identity key's public half"))?.as_ref().to_vec();
        Ok(Some(RunnerKey { keyid: seal::keyid(&spki), pair, spki }))
    }

    /// `result` as this runner records it in `scope` under `strong`: signed, its signature in place
    /// of any it had.
    pub fn sign(&self, scope: &str, strong: &Key, result: &Recorded) -> anyhow::Result<Recorded> {
        let digest = seal::digest(&claim(scope, strong, result));
        let sig = self.pair.sign(&SystemRandom::new(), &seal::runner_message(&digest)).map_err(|_| anyhow::anyhow!("signing failed"))?;
        let sigs = seal::Signatures { runner: Some(seal::RunnerSig::new(self.spki.clone(), sig.as_ref().to_vec())), server: None };
        Ok(Recorded { signatures: sigs.to_list(), ..result.clone() })
    }
}

// ------------------------------------------------------------------ verification (W24-R23)

/// What quarantined an entry, kept beside it (W24-R23): lookups pass it over, saying why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Quarantine {
    /// The verdict's words.
    pub why: String,
    /// The entry whose verdict it was (`<scope><strong>`): this one, or one it was built on.
    pub root: String,
    /// When, in seconds since the epoch.
    pub at: i64,
}

/// A unit verification marked nondeterministic (W24-R23), kept under its weak key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Nondeterministic {
    /// The outputs its runs disagree on.
    pub outputs: Vec<String>,
    /// The entry whose runs showed it (`<scope><strong>`).
    pub entry: String,
    pub at: i64,
}

/// An entry's row key: `<scope><strong>`.
fn entry_key(scope: &str, strong: &Key) -> String {
    format!("{scope}{}", strong.hex())
}

/// What quarantined the entry `scope` holds under `strong`, if anything did.
pub fn quarantined(c: &Connection, scope: &str, strong: &Key) -> anyhow::Result<Option<Quarantine>> {
    Ok(get(c, Family::Verify, &format!("{}quarantine", verify_prefix(scope, strong)))?.map(|v| serde_json::from_slice(&v)).transpose()?)
}

/// Whether verification marked the unit of `weak` nondeterministic in `scope`.
pub fn nondeterministic(c: &Connection, scope: &str, weak: &Key) -> anyhow::Result<Option<Nondeterministic>> {
    Ok(get(c, Family::Verify, &format!("{}nondeterministic", weak_prefix(scope, weak)))?.map(|v| serde_json::from_slice(&v)).transpose()?)
}

/// The results an entry's key refused, kept as evidence.
fn conflicts(c: &Connection, scope: &str, strong: &Key) -> anyhow::Result<Vec<Recorded>> {
    let mut out = Vec::new();
    for (_, v, _) in scan(c, Family::Verify, &format!("{}conflict|", verify_prefix(scope, strong)))? {
        let v: Verification = serde_json::from_slice(&v)?;
        out.extend(v.result);
    }
    Ok(out)
}

/// What every other run of an entry's key says of it (W24-R23, [`verify::judge`]): its
/// reproductions, which gave its outcome and outputs, and the results its key refused. `None` when
/// the key holds no entry.
pub fn verdict(c: &Connection, scope: &str, strong: &Key) -> anyhow::Result<Option<Verdict>> {
    let Some(entry) = get(c, Family::Result, &entry_key(scope, strong))? else { return Ok(None) };
    let entry: Recorded = serde_json::from_slice(&entry)?;
    let mut runs: Vec<Recorded> = reproductions(c, scope, strong)?.into_iter().map(|v| v.result.unwrap_or_else(|| entry.clone())).collect();
    runs.extend(conflicts(c, scope, strong)?);
    let outcomes: Vec<Outcome> = runs.iter().map(Recorded::outcome).collect();
    Ok(Some(verify::judge(&entry.outcome(), &outcomes)))
}

/// The entry `scope` holds under `strong` (of `weak`) judged again on all its runs, and what the
/// verdict does (W24-R23): a nondeterministic unit is marked; an entry in doubt has its lineage
/// quarantined, the entry and every entry of the repository built on it, through what each says
/// it reused, in whatever domain. Kiln decides this on its own, so it writes why to the audit log.
fn judge_again(c: &Connection, scope: &str, weak: &Key, strong: &Key) -> anyhow::Result<Verdict> {
    let Some(verdict) = verdict(c, scope, strong)? else { return Ok(Verdict::Verified) };
    let now = crate::util::now_unix();
    let root = entry_key(scope, strong);
    let marked = format!("{}nondeterministic", weak_prefix(scope, weak));
    if let Verdict::Nondeterministic { names } = &verdict {
        let mark = Nondeterministic { outputs: names.clone(), entry: root.clone(), at: now };
        put(c, Family::Verify, &marked, &serde_json::to_vec(&mark)?, now)?;
    } else if matches!(verdict, Verdict::Corrupted { .. }) && nondeterministic(c, scope, weak)?.is_some_and(|m| m.entry == root) {
        // Its first run again only said it differed; now runs agree that the entry is what's
        // wrong, so the unit loses the mark that run gave it.
        delete(c, Family::Verify, &marked)?;
    }
    if verdict.quarantines() {
        let repo = scope.strip_suffix('|').and_then(|s| s.rsplit_once('|')).map_or(scope, |(repo, _)| repo);
        let mut reused: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v, _) in scan(c, Family::Result, &format!("{repo}|"))? {
            let r: Recorded = serde_json::from_slice(&v).unwrap_or_default();
            if !r.reused.is_empty() {
                reused.insert(k, r.reused);
            }
        }
        let lineage = verify::lineage(&root, &reused);
        let why = verdict.why();
        let q = serde_json::to_vec(&Quarantine { why: why.clone(), root: root.clone(), at: now })?;
        for entry in &lineage {
            put(c, Family::Verify, &format!("{entry}|quarantine"), &q, now)?;
        }
        audit_as(c, "kiln", "work.verify", &root, "quarantined", &format!("{why}; its lineage quarantined: {}", lineage.join(", ")))?;
        tracing::warn!(entry = %root, why = %why, entries = lineage.len(), "an entry's lineage quarantined (W24-R23)");
    }
    Ok(verdict)
}

/// What verification saw re-running a hit's unit (W24-R23): `observed`, its outputs' digest
/// ([`digest`]). The hit's own: nothing changes. Otherwise the hit was wrong, and the class splits
/// where it was found (W24-R30): under the unit's own key, its unit is nondeterministic; by class,
/// the claims that made it are refuted, and the classes it took upstream units by split too. Each
/// split quarantines its lineage, so none of them serves a class-based hit again. Returns the keys
/// quarantined.
pub fn verified(c: &Connection, scope: &str, hit: &Hit, observed: &str) -> anyhow::Result<BTreeSet<Key>> {
    let mut quarantined = BTreeSet::new();
    if digest(&hit.recorded) == observed {
        return Ok(quarantined);
    }
    let tx = crate::store::tx(c)?;
    let mut rows = Rows { c: &tx, scope };
    let at: Vec<(Key, Option<&str>)> = match &hit.class {
        None => vec![(hit.key, Some(observed))],
        Some(by) => std::iter::once(hit.key).chain(by.took.iter().copied()).map(|k| (k, None)).collect(),
    };
    for (k, seen) in at {
        if rows.entry(&k)?.is_some() {
            quarantined.extend(eclass::split(&mut rows, &k, seen)?);
        }
    }
    tx.commit()?;
    Ok(quarantined)
}

/// A key's member in its scope's e-classes (W24-R30), if it has one.
pub fn member(c: &Connection, scope: &str, k: &Key) -> anyhow::Result<Option<Member>> {
    Rows { c, scope }.entry(k)
}

/// A rewrite rule as `work.eclass` keeps it, with the witness it was registered with.
#[derive(Serialize, Deserialize)]
struct Registered {
    rule: Rule,
    witness: Witness,
}

/// The rewrite rules registered for a scope's units (W24-R30).
pub fn rules(c: &Connection, scope: &str) -> anyhow::Result<Rules> {
    let mut held = Vec::new();
    for (_, value, _) in scan(c, Family::EClass, &format!("{scope}r|"))? {
        held.push(serde_json::from_slice::<Registered>(&value)?);
    }
    held.sort_by(|a, b| a.rule.name.cmp(&b.rule.name));
    let mut rules = Rules::default();
    for r in held {
        rules.register(r.rule, r.witness)?;
    }
    Ok(rules)
}

/// Registers a rewrite rule for a scope's units with its differential test's witness (W24-R30),
/// refused as [`Rules::register`] refuses one (a rule without a witness among others). From then on
/// the scope's units also key in its normal form, and a unit it rewrites finds its class's result.
pub fn register(c: &Connection, scope: &str, rule: &Rule, witness: &Witness) -> anyhow::Result<()> {
    rules(c, scope)?.register(rule.clone(), witness.clone())?;
    let row = Registered { rule: rule.clone(), witness: witness.clone() };
    put(c, Family::EClass, &format!("{scope}r|{}", rule.name), &serde_json::to_vec(&row)?, crate::util::now_unix())?;
    Ok(())
}

/// A key's member as `work.eclass` keeps it (W24-R30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Placed {
    member: Member,
    /// For a key claimed into its class without outputs of its own: the key whose result a
    /// class-based hit through it serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

/// A scope's e-classes in `work.eclass` (W24-R30), for the ledger and for [`eclass::split`].
struct Rows<'a> {
    c: &'a Connection,
    scope: &'a str,
}

impl Rows<'_> {
    fn key(&self, tag: &str, rest: &str) -> String {
        format!("{}{tag}|{rest}", self.scope)
    }

    fn placed(&self, k: &Key) -> anyhow::Result<Option<Placed>> {
        Ok(get(self.c, Family::EClass, &self.key("m", &k.hex()))?.map(|p| serde_json::from_slice(&p)).transpose()?)
    }

    fn place(&self, k: &Key, p: &Placed) -> anyhow::Result<()> {
        Ok(put(self.c, Family::EClass, &self.key("m", &k.hex()), &serde_json::to_vec(p)?, crate::util::now_unix())?)
    }

    fn pair(&self, tag: &str, a: &Key, b: &Key, value: &[u8]) -> rusqlite::Result<()> {
        put(self.c, Family::EClass, &self.key(tag, &format!("{}|{}", a.hex(), b.hex())), value, crate::util::now_unix())
    }

    /// The keys paired with `a` under `tag`.
    fn paired(&self, tag: &str, a: &Key) -> anyhow::Result<BTreeSet<Key>> {
        let prefix = self.key(tag, &format!("{}|", a.hex()));
        Ok(scan(self.c, Family::EClass, &prefix)?.into_iter().filter_map(|(k, _, _)| k.strip_prefix(prefix.as_str()).and_then(eclass::parse_key)).collect())
    }

    fn claim(&self, a: &Key, b: &Key, why: &Why) -> anyhow::Result<()> {
        let why = serde_json::to_vec(why)?;
        self.pair("c", a, b, &why)?;
        self.pair("c", b, a, &why)?;
        Ok(())
    }
}

impl eclass::Classes for Rows<'_> {
    type Error = anyhow::Error;

    fn entry(&self, k: &Key) -> anyhow::Result<Option<Member>> {
        Ok(self.placed(k)?.map(|p| p.member))
    }

    fn claimed(&self, k: &Key) -> anyhow::Result<BTreeSet<Key>> {
        self.paired("c", k)
    }

    fn lineage(&self, k: &Key) -> anyhow::Result<BTreeSet<Key>> {
        self.paired("l", k)
    }

    /// A split's members: what their own outputs show, so none serves through another's result.
    fn apply(&mut self, split: &Split) -> anyhow::Result<()> {
        for (a, b) in &split.dropped {
            delete(self.c, Family::EClass, &self.key("c", &format!("{}|{}", a.hex(), b.hex())))?;
            delete(self.c, Family::EClass, &self.key("c", &format!("{}|{}", b.hex(), a.hex())))?;
        }
        for (k, member) in &split.members {
            self.place(k, &Placed { member: member.clone(), source: None })?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ test results (W24-R10)

fn marks_at(scope: &str, key: &Key) -> String {
    format!("{scope}marks|{}", key.hex())
}

fn last_at(scope: &str, b: &Binary, cx: &Context) -> String {
    format!("{scope}last|{}", test_results::identity(b, cx).hex())
}

fn marks(c: &Connection, scope: &str, key: &Key) -> anyhow::Result<Marks> {
    Ok(get(c, Family::Test, &marks_at(scope, key))?.map(|v| serde_json::from_slice(&v)).transpose()?.unwrap_or_default())
}

/// Records a test binary's run (W24-R10) through the ledger's one path: a pass goes in by
/// [`record`], as the binary's unit ([`kiln_ci::work::tests::unit`]) with what it read (its test
/// key, [`kiln_ci::work::tests::key`]), so [`lookup`] finds it only while its digest, arguments,
/// environment, platform and every file it read are as they were; a failure, a crash or a timeout
/// never does. Every run counts towards its test key's flaky marks (W16-R04), and a pass is kept as
/// its binary's identity's newest, with what it read, for a run that doesn't rebuild it. Returns the
/// test key of a pass.
pub fn record_test(c: &Connection, scope: &str, b: &Binary, cx: &Context, read: &PathSet, ran: &Ran, provenance: &Provenance) -> anyhow::Result<Option<Key>> {
    let now = crate::util::now_unix();
    let at = test_results::key(b, cx, read);
    let mut counted = marks(c, scope, &at)?;
    counted.add(ran);
    put(c, Family::Test, &marks_at(scope, &at), &serde_json::to_vec(&counted)?, now)?;
    if !ran.passed() {
        return Ok(None);
    }
    let kept = Kept { ran: ran.clone(), digest: b.digest.clone().unwrap_or_default(), run: provenance.run.clone(), leg: provenance.leg.clone(), flaky: counted.flaky() };
    let result = Recorded {
        outcome: "success".into(),
        exit: ran.exit,
        outputs: BTreeMap::from([("test".to_owned(), serde_json::to_string(&kept)?)]),
        provenance: Provenance { ms: ran.ms, ..provenance.clone() },
        domain: cx.trust.clone(),
        ..Recorded::default()
    };
    // A pass already kept under its test key stands (W24-R22: entries are never overwritten). This
    // one differs from it only in its run, leg and time, not in what the binary did, so it isn't
    // recorded again, which the ledger would take for a conflict and judge (W24-R23); it's its
    // identity's newest all the same.
    let strong = match get(c, Family::Result, &format!("{scope}{}", at.hex()))? {
        Some(_) => at,
        None => record(c, scope, &test_results::unit(b, cx), read, &result)?,
    };
    put(c, Family::Test, &last_at(scope, b, cx), &serde_json::to_vec(&(&kept, read))?, now)?;
    Ok(Some(strong))
}

/// What the ledger has for a test binary (W24-R10), as [`kiln_ci::work::tests::select`] asks: a
/// pass found for its unit by [`lookup`] (only for a binary with its digest), with the flaky tests
/// of the key it was found under as they are now; and its identity's newest pass, with whether what
/// that read holds. `now` gives a path set as it is now.
pub fn found_test(c: &Connection, scope: &str, b: &Binary, cx: &Context, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<test_results::Found> {
    let mut by_key = None;
    if b.digest.is_some()
        && let Some(hit) = lookup(c, scope, &test_results::unit(b, cx), now)?
        && let Some(kept) = hit.recorded.outputs.get("test").map(|t| serde_json::from_str::<Kept>(t)).transpose()?
    {
        by_key = Some(Kept { flaky: marks(c, scope, &hit.key)?.flaky(), ..kept });
    }
    let last: Option<(Kept, PathSet)> = get(c, Family::Test, &last_at(scope, b, cx))?.map(|v| serde_json::from_slice(&v)).transpose()?;
    let last_holds = last.as_ref().is_some_and(|(_, read)| now(read) == *read);
    Ok(test_results::Found { by_key, last: last.map(|(k, _)| k), last_holds })
}

/// Which of a run's test binaries run and which replay their passes (W24-R10), each saying why:
/// [`kiln_ci::work::tests::select`] over what the ledger has for each ([`found_test`]).
pub fn select_tests(c: &Connection, scope: &str, binaries: &[Binary], cx: &Context, affected: &BTreeSet<String>, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Vec<(Binary, Choice)>> {
    let found: Vec<test_results::Found> = binaries.iter().map(|b| found_test(c, scope, b, cx, now)).collect::<anyhow::Result<_>>()?;
    let answer = |b: &Binary| binaries.iter().position(|x| x == b).and_then(|i| found.get(i)).cloned().unwrap_or_default();
    Ok(test_results::select(binaries, affected, &answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use kiln_ci::work::eclass::{Case, Pattern, Rewrite};
    use kiln_ci::work::key::{Kind, Platform, Read};

    fn unit(n: u64) -> Unit {
        Unit {
            kind: Kind::Step,
            definition: format!("cargo test -p crate{n}"),
            secrets: BTreeSet::new(),
            env: BTreeMap::new(),
            platform: Platform { os: "linux".into(), arch: "x86_64".into(), ..Platform::default() },
            inputs: BTreeMap::from([("crates/**".to_owned(), format!("tree{n}"))]),
            upstream: BTreeMap::new(),
            trust: "trusted".into(),
        }
    }

    fn reads(n: u64, variant: u64) -> PathSet {
        (0..6).map(|i| (format!("src/f{n}_{i}.rs"), Read::File(format!("d{n}_{i}_{variant}")))).collect()
    }

    fn ok(run: &str) -> Recorded {
        Recorded { outcome: "success".into(), exit: Some(0), provenance: Provenance { run: run.into(), ..Provenance::default() }, domain: "trusted".into(), ..Recorded::default() }
    }

    fn same(p: &PathSet) -> PathSet {
        p.clone()
    }

    /// A trusted step that runs `definition` over upstream units, each by its outputs' digest or by
    /// key.
    fn step(definition: &str, upstream: &[(&str, String)]) -> Unit {
        Unit { definition: definition.to_owned(), inputs: BTreeMap::new(), upstream: upstream.iter().map(|(n, v)| ((*n).to_owned(), v.clone())).collect(), platform: Platform::default(), ..unit(0) }
    }

    /// A success whose output is `out`.
    fn made_out(out: &str, run: &str) -> Recorded {
        Recorded { outputs: BTreeMap::from([("out".to_owned(), out.to_owned())]), ..ok(run) }
    }

    /// What the steps here read: a lockfile, unchanged.
    fn lock() -> PathSet {
        PathSet::from([("Cargo.lock".to_owned(), Read::File("l1".into()))])
    }

    /// A rule, and the witness a differential test gave it: `before` and `after` run alike.
    fn rule(name: &str, rewrite: Rewrite, before: &str, after: &str) -> (Rule, Witness) {
        let case = Case { before: before.into(), after: after.into(), before_output: "sha256:alike".into(), after_output: "sha256:alike".into() };
        (Rule { name: name.into(), kinds: BTreeSet::new(), rewrite }, Witness { test: format!("{name}, both ways"), cases: vec![case] })
    }

    #[test]
    fn w24_r02_a_result_is_found_while_what_it_read_is_unchanged() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let (trusted, untrusted) = (scope("acme/app", "trusted"), scope("acme/app", "untrusted"));
                let strong = record(c, &trusted, &unit(1), &reads(1, 0), &ok("r1"))?;
                let hit = lookup(c, &trusted, &unit(1), &same)?;
                anyhow::ensure!(hit.as_ref().map(|h| (h.key, h.recorded.provenance.run.as_str(), h.class.is_none())) == Some((strong, "r1", true)), "{hit:?}");
                // A file it read changed: no result for that.
                let changed = |p: &PathSet| -> PathSet { p.keys().map(|k| (k.clone(), Read::File("changed".into()))).collect() };
                anyhow::ensure!(lookup(c, &trusted, &unit(1), &changed)?.is_none());
                // Another domain's ledger holds nothing of it (W24-R21).
                anyhow::ensure!(lookup(c, &untrusted, &unit(1), &same)?.is_none());
                // Another unit: nothing.
                anyhow::ensure!(lookup(c, &trusted, &unit(2), &same)?.is_none());
                Ok(())
            })
            .await
        })
    }

    #[test]
    fn w24_r04_a_weak_key_keeps_its_newest_eight_path_sets() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                let weak = key::weak(&unit(1));
                let ids: Vec<String> = (0..10).map(|v| crate::util::sha256_hex(&serde_json::to_vec(&reads(1, v)).unwrap_or_default())).collect();
                for v in 0..10 {
                    record(c, &sc, &unit(1), &reads(1, v), &ok(&format!("r{v}")))?;
                }
                let kept = path_sets(c, &sc, &weak)?;
                anyhow::ensure!(kept.len() == PATH_SETS, "{kept:?}");
                let newest: Vec<String> = ids.iter().rev().take(PATH_SETS).cloned().collect();
                anyhow::ensure!(kept == newest, "newest first: {kept:?}");
                // Recorded again, an older path set is newest again, without a second listing.
                record(c, &sc, &unit(1), &reads(1, 3), &ok("again"))?;
                let again = path_sets(c, &sc, &weak)?;
                anyhow::ensure!(again.len() == PATH_SETS && again.first() == ids.get(3), "{again:?}");
                Ok(())
            })
            .await
        })
    }

    /// W24-AC10: ledger lookups take p99 under 5 ms on the server: 20,000 results under 5,000
    /// weak keys (4 path sets each) in a store on disk, looked up 5,000 times through the store's
    /// own calls, as dispatch would. Then, with a rule registered, 5,000 units it rewrites, each
    /// found by class (W24-R30), in the same time.
    #[test]
    fn w24_ac10_lookup_p99_under_5ms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let s = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        let units: u64 = 5_000;
        crate::rt::block_on(async move {
            s.call(move |c| {
                let tx = c.transaction()?;
                let sc = scope("acme/app", "trusted");
                for n in 0..units {
                    for v in 0..4 {
                        record(&tx, &sc, &unit(n), &reads(n, v), &ok(&format!("r{n}_{v}")))?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
            let mut took = Vec::new();
            for i in 0..units {
                let n = (i * 7_919) % units;
                let started = std::time::Instant::now();
                let hit = s.call(move |c| lookup(c, &scope("acme/app", "trusted"), &unit(n), &same)).await?;
                took.push(started.elapsed());
                anyhow::ensure!(hit.is_some_and(|h| h.class.is_none()), "unit {n} has results under its own key");
            }
            took.sort();
            let at = |q: f64| took.get(((took.len() as f64 * q).ceil() as usize).saturating_sub(1)).copied().unwrap_or_default();
            let (p50, p99) = (at(0.50), at(0.99));
            println!("w24_ac10: {} lookups, p50 {:?}, p99 {:?}", took.len(), p50, p99);
            anyhow::ensure!(p99 < std::time::Duration::from_millis(5), "p99 {p99:?} is 5 ms or more");
            let (no_op, witness) = rule("drop-true", Rewrite::Drop { op: Pattern::Is("true".into()) }, "make\ntrue", "make");
            s.call(move |c| register(c, &scope("acme/app", "trusted"), &no_op, &witness)).await?;
            let mut took = Vec::new();
            for i in 0..units {
                let n = (i * 7_919) % units;
                let started = std::time::Instant::now();
                let hit = s.call(move |c| lookup(c, &scope("acme/app", "trusted"), &Unit { definition: format!("{}\ntrue", unit(n).definition), ..unit(n) }, &same)).await?;
                took.push(started.elapsed());
                anyhow::ensure!(hit.is_some_and(|h| h.class.is_some()), "unit {n}, rewritten, is found by class");
            }
            took.sort();
            let at = |q: f64| took.get(((took.len() as f64 * q).ceil() as usize).saturating_sub(1)).copied().unwrap_or_default();
            let (p50, p99) = (at(0.50), at(0.99));
            println!("w24_ac10: {} lookups by class, p50 {:?}, p99 {:?}", took.len(), p50, p99);
            anyhow::ensure!(p99 < std::time::Duration::from_millis(5), "by class, p99 {p99:?} is 5 ms or more");
            Ok(())
        })
    }

    /// W24-R10 through the ledger's one path: a test binary's pass is found again under its test
    /// key while its digest and what it read hold, and replayed; a failure never is, though it
    /// counts towards the key's flaky marks; a binary its change didn't rebuild replays its newest
    /// pass while what that read holds; another domain's ledger has none of it.
    #[test]
    fn w24_r10_test_results_through_the_ledger() -> anyhow::Result<()> {
        use kiln_ci::work::tests::Case;
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                let cx = Context { env: BTreeMap::from([("RUST_TEST_THREADS".to_owned(), "4".to_owned())]), platform: Platform { os: "linux".into(), arch: "x86_64".into(), ..Platform::default() }, trust: "trusted".into() };
                let bin = |p: &str, d: Option<&str>| Binary { package: p.into(), name: "lib".into(), args: Vec::new(), digest: d.map(str::to_owned) };
                let read = |p: &str| PathSet::from([(format!("crates/{p}/tests/data.txt"), Read::File(format!("{p}-data")))]);
                let ran = |t2: Case, exit: i32| Ran { exit: Some(exit), cases: BTreeMap::from([("t1".to_owned(), Case::Passed), ("t2".to_owned(), t2)]), ms: 1_500 };
                let at = |run: &str| Provenance { repo: "acme/app".into(), run: run.into(), leg: format!("{run}-test"), ..Provenance::default() };
                let same = |p: &PathSet| p.clone();
                let (a, b) = (bin("a", Some("a1")), bin("b", Some("b1")));
                // A failure isn't kept, but counts: t2 failed, then passed with nothing changed.
                anyhow::ensure!(record_test(c, &sc, &a, &cx, &read("a"), &ran(Case::Failed, 101), &at("r1"))?.is_none());
                let key = record_test(c, &sc, &a, &cx, &read("a"), &ran(Case::Passed, 0), &at("r2"))?;
                anyhow::ensure!(key == Some(test_results::key(&a, &cx, &read("a"))), "kept under its test key: {key:?}");
                record_test(c, &sc, &b, &cx, &read("b"), &ran(Case::Passed, 0), &at("r2"))?;
                let found = found_test(c, &sc, &a, &cx, &same)?;
                let kept = found.by_key.clone().ok_or_else(|| anyhow::anyhow!("no pass under its key"))?;
                anyhow::ensure!(kept.run == "r2" && kept.leg == "r2-test" && kept.digest == "a1" && kept.ran.ms == 1_500, "{kept:?}");
                anyhow::ensure!(kept.flaky == BTreeSet::from(["t2".to_owned()]), "flaky on its key: {:?}", kept.flaky);
                anyhow::ensure!(found.last.is_some() && found.last_holds);
                // b's data changes, and a isn't rebuilt (its package isn't affected): b runs, a
                // replays its newest pass.
                let edited = |p: &PathSet| -> PathSet { p.iter().map(|(k, v)| (k.clone(), if k.starts_with("crates/b/") { Read::File("edited".into()) } else { v.clone() })).collect() };
                let affected = BTreeSet::from(["b".to_owned()]);
                let choices = select_tests(c, &sc, &[bin("a", None), b.clone()], &cx, &affected, &edited)?;
                let said: Vec<(String, bool, String)> = choices.iter().map(|(x, ch)| (x.package.clone(), ch.runs(), match ch { Choice::Run { why } | Choice::Replay { why, .. } => why.clone() })).collect();
                anyhow::ensure!(
                    said == [("a".to_owned(), false, "its package isn't affected: as in run r2".to_owned()), ("b".to_owned(), true, "what it read changed since run r2".to_owned())],
                    "{said:?}"
                );
                // A failed run of b is never kept: b runs again next time too.
                anyhow::ensure!(record_test(c, &sc, &b, &cx, &edited(&read("b")), &ran(Case::Failed, 101), &at("r3"))?.is_none());
                anyhow::ensure!(found_test(c, &sc, &b, &cx, &edited)?.by_key.is_none());
                // Another domain's ledger holds none of it (W24-R21).
                let theirs = found_test(c, &scope("acme/app", "untrusted"), &a, &cx, &same)?;
                anyhow::ensure!(theirs.by_key.is_none() && theirs.last.is_none(), "{theirs:?}");
                Ok(())
            })
            .await
        })
    }

    /// W24-R10 with W24-R22: a binary that passes again under the same test key (another run of
    /// the same tree) is no conflict: the first pass kept stands, nothing is quarantined, and the
    /// newest is its identity's.
    #[test]
    fn w24_r10_a_pass_again_under_its_test_key_stands() -> anyhow::Result<()> {
        use kiln_ci::work::tests::Case;
        in_store(|c| {
            let sc = scope("acme/app", "trusted");
            let cx = Context { trust: "trusted".into(), ..Context::default() };
            let b = Binary { package: "a".into(), name: "lib".into(), args: Vec::new(), digest: Some("a1".into()) };
            let read = PathSet::from([("crates/a/tests/data.txt".to_owned(), Read::File("a-data".into()))]);
            let ran = Ran { exit: Some(0), cases: BTreeMap::from([("t1".to_owned(), Case::Passed)]), ms: 1_000 };
            let at = |run: &str| Provenance { repo: "acme/app".into(), run: run.into(), leg: format!("{run}-test"), runner: run.into(), ..Provenance::default() };
            let first = record_test(c, &sc, &b, &cx, &read, &ran, &at("r1"))?;
            let again = record_test(c, &sc, &b, &cx, &read, &Ran { ms: 2_000, ..ran.clone() }, &at("r2"))?;
            anyhow::ensure!(first.is_some() && again == first, "{first:?} {again:?}");
            let key = first.ok_or_else(|| anyhow::anyhow!("no key"))?;
            anyhow::ensure!(quarantined(c, &sc, &key)?.is_none() && scan(c, Family::Verify, &sc)?.is_empty(), "nothing in doubt");
            let same = |p: &PathSet| p.clone();
            let found = found_test(c, &sc, &b, &cx, &same)?;
            anyhow::ensure!(found.by_key.as_ref().is_some_and(|k| k.run == "r1") && found.last.as_ref().is_some_and(|k| k.run == "r2"), "{found:?}");
            Ok(())
        })
    }

    /// W24-R30: keys whose units produced byte-identical outputs are one class, and congruent units
    /// share a result: a unit that names an upstream unit by key, before its outputs are
    /// materialized, finds what ran over any key of its class.
    #[test]
    fn w24_r30_record_merges_classes_by_output_digest() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                let fetch = |docs: &str, src: &str| Unit { inputs: BTreeMap::from([("docs".to_owned(), docs.to_owned()), ("src".to_owned(), src.to_owned())]), ..step("fetch", &[]) };
                let a = record(c, &sc, &fetch("d1", "s1"), &lock(), &made_out("sha256:src1", "r1"))?;
                let b = record(c, &sc, &fetch("d2", "s1"), &lock(), &made_out("sha256:src1", "r2"))?;
                let other = record(c, &sc, &fetch("d1", "s2"), &lock(), &made_out("sha256:src2", "r3"))?;
                let class = |k: &Key| member(c, &sc, k).map(|m| m.map(|m| m.class));
                let d = digest(&made_out("sha256:src1", "r1"));
                anyhow::ensure!(a != b && class(&a)? == Some(ClassId::Output(d.clone())) && class(&b)? == class(&a)?, "one class: {:?} {:?}", class(&a)?, class(&b)?);
                anyhow::ensure!(class(&other)? != class(&a)?, "other outputs, another class");
                // A build over a's outputs ran; one over b, named by key, takes b by class.
                record(c, &sc, &step("make", &[("src", d.clone())]), &lock(), &made_out("sha256:bin1", "r1"))?;
                let hit = lookup(c, &sc, &step("make", &[("src", eclass::refer(&b))]), &same)?.context("a congruent result")?;
                let by = hit.class.as_ref().context("found by class")?;
                let why = Why::Canonical { rules: Vec::new(), by_class: vec!["src".into()] };
                anyhow::ensure!(by.took == vec![b] && by.why == why, "{by:?}");
                anyhow::ensure!(hit.recorded.outputs.get("out").map(String::as_str) == Some("sha256:bin1"));
                // Over a key no class holds, or one whose class has other outputs: nothing.
                anyhow::ensure!(lookup(c, &sc, &step("make", &[("src", eclass::refer(&Key([7; 32])))]), &same)?.is_none());
                anyhow::ensure!(lookup(c, &sc, &step("make", &[("src", eclass::refer(&other))]), &same)?.is_none());
                Ok(())
            })
            .await
        })
    }

    /// W24-R30: a unit the scope's rules rewrite finds its class's result, once the rules are
    /// registered, each with its differential test's witness; a rule without one is refused.
    #[test]
    fn w24_r30_a_rewritten_unit_finds_its_class_by_the_rules_registered() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                let (make, variant) = (step("add a\nadd b\nmake", &[]), step("add b\nadd a\ntrue\nmake", &[]));
                let k = record(c, &sc, &make, &lock(), &made_out("sha256:bin", "r1"))?;
                anyhow::ensure!(lookup(c, &sc, &variant, &same)?.is_none(), "no rules: plain keys");
                let (no_op, witness) = rule("drop-true", Rewrite::Drop { op: Pattern::Is("true".into()) }, "make\ntrue", "make");
                anyhow::ensure!(register(c, &sc, &no_op, &Witness { cases: Vec::new(), ..witness.clone() }).is_err(), "a witness with no case");
                anyhow::ensure!(register(c, &sc, &no_op, &Witness { test: String::new(), ..witness.clone() }).is_err(), "a witness naming no test");
                anyhow::ensure!(rules(c, &sc)?.rules().count() == 0);
                register(c, &sc, &no_op, &witness)?;
                anyhow::ensure!(register(c, &sc, &no_op, &witness).is_err(), "a name held");
                let (adds, witness) = rule("adds-commute", Rewrite::Commute { a: Pattern::Starts("add ".into()), b: Pattern::Starts("add ".into()) }, "add a\nadd b", "add b\nadd a");
                register(c, &sc, &adds, &witness)?;
                let hit = lookup(c, &sc, &variant, &same)?.context("found by class")?;
                let by = hit.class.as_ref().context("by class")?;
                let why = Why::Canonical { rules: vec!["adds-commute".into(), "drop-true".into()], by_class: Vec::new() };
                anyhow::ensure!(hit.key == k && by.source == k && by.why == why, "{hit:?}");
                anyhow::ensure!(lookup(c, &sc, &make, &same)?.is_some_and(|h| h.key == k && h.class.is_none()), "the unit itself, under its own key");
                // Recorded, the variant's own key joins the class, claimed to the normal form's; a
                // third way of writing it finds the class too.
                let v = record(c, &sc, &variant, &lock(), &made_out("sha256:bin", "r2"))?;
                anyhow::ensure!(member(c, &sc, &v)?.map(|m| m.class) == member(c, &sc, &k)?.map(|m| m.class));
                let third = step("true\nadd b\nadd a\nmake", &[]);
                anyhow::ensure!(lookup(c, &sc, &third, &same)?.is_some_and(|h| h.class.is_some()));
                // Another domain's scope has no rules of this one's.
                anyhow::ensure!(lookup(c, &scope("acme/app", "untrusted"), &variant, &same)?.is_none());
                Ok(())
            })
            .await
        })
    }

    /// W24-R30, R23: a wrong class-based hit, found by verification, splits the class it was found
    /// in and quarantines its lineage: they serve no class-based hit again, while hits under units'
    /// own keys stand. An exact hit seen to differ marks its unit nondeterministic.
    #[test]
    fn w24_r30_a_wrong_class_hit_splits_its_class_and_quarantines_its_lineage() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let sc = scope("acme/app", "trusted");
                // An unsound rule: `echo` is taken to do nothing, its differential test having run
                // where echo wrote nowhere that counts.
                let (echo, witness) = rule("drop-echo", Rewrite::Drop { op: Pattern::Starts("echo ".into()) }, "make\necho done", "make");
                register(c, &sc, &echo, &witness)?;
                let make = step("make", &[]);
                let k = record(c, &sc, &make, &lock(), &made_out("sha256:bin", "r1"))?;
                let d = digest(&made_out("sha256:bin", "r1"));
                record(c, &sc, &step("test", &[("bin", d.clone())]), &lock(), &made_out("sha256:passed", "r1"))?;
                // A unit that echoes into its outputs is taken for `make`, and a test over it, named
                // by key, for the test over make's outputs.
                let echoes = step("make\necho stamp >> bin", &[]);
                let hit = lookup(c, &sc, &echoes, &same)?.context("a class-based hit")?;
                anyhow::ensure!(hit.key == k && hit.class.is_some(), "{hit:?}");
                let test_over = step("test", &[("bin", eclass::refer(&hit.key))]);
                anyhow::ensure!(lookup(c, &sc, &test_over, &same)?.is_some_and(|h| h.class.is_some()), "found before the echoing unit's outputs exist");
                // A lint recorded over it by key is its lineage, and the lint over make's outputs is
                // claimed into the lint's class.
                let lint_over = step("lint", &[("bin", eclass::refer(&hit.key))]);
                let l = record(c, &sc, &lint_over, &lock(), &made_out("sha256:clean", "r1"))?;
                let lint = step("lint", &[("bin", d.clone())]);
                anyhow::ensure!(lookup(c, &sc, &lint, &same)?.is_some_and(|h| h.class.is_some()));
                // Verification runs the echoing unit: other outputs.
                let observed = digest(&made_out("sha256:bin+stamp", "verify"));
                let quarantined = verified(c, &sc, &hit, &observed)?;
                anyhow::ensure!(quarantined.contains(&k) && quarantined.contains(&l), "{quarantined:?}");
                anyhow::ensure!(lookup(c, &sc, &echoes, &same)?.is_none(), "the class serves no class-based hit");
                anyhow::ensure!(lookup(c, &sc, &test_over, &same)?.is_none(), "nor does what takes it by class");
                anyhow::ensure!(lookup(c, &sc, &lint, &same)?.is_none(), "its lineage's claims are dropped");
                anyhow::ensure!(lookup(c, &sc, &make, &same)?.is_some_and(|h| h.key == k && h.class.is_none()), "make's own result stands");
                anyhow::ensure!(lookup(c, &sc, &lint_over, &same)?.is_some_and(|h| h.key == l && h.class.is_none()), "as does the lint's");
                anyhow::ensure!(member(c, &sc, &k)?.is_some_and(|m| m.quarantined && m.anchored()));
                // The verification's run, recorded, is the echoing unit's own class, claimed into none.
                let e = record(c, &sc, &echoes, &lock(), &made_out("sha256:bin+stamp", "verify"))?;
                anyhow::ensure!(lookup(c, &sc, &echoes, &same)?.is_some_and(|h| h.key == e && h.class.is_none()));
                anyhow::ensure!(member(c, &sc, &e)?.is_some_and(|m| m.class == ClassId::Output(observed.clone()) && !m.quarantined));
                // Verification sees make's own result differ: make is nondeterministic.
                let again = lookup(c, &sc, &make, &same)?.context("make's own result")?;
                let quarantined = verified(c, &sc, &again, &digest(&made_out("sha256:other", "v2")))?;
                anyhow::ensure!(quarantined.contains(&k) && member(c, &sc, &k)?.is_some_and(|m| m.nondeterministic));
                anyhow::ensure!(verified(c, &sc, &again, &digest(&again.recorded))?.is_empty(), "what agrees changes nothing");
                Ok(())
            })
            .await
        })
    }

    /// W24-R30, R21: merges stay within a trust domain. A result its scope's lookups may not read
    /// is refused whole; each domain's classes hold its own entries; a trusted lookup takes no
    /// untrusted key by class and no untrusted scope's rules.
    #[test]
    fn w24_r30_classes_stay_within_a_trust_domain() -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            s.call(|c| {
                let (trusted, untrusted) = (scope("acme/app", "trusted"), scope("acme/app", "untrusted"));
                anyhow::ensure!(domain_of(&trusted) == "trusted" && domain_of(&scope("acme/app", "person:ada")) == "person:ada");
                let forked = |out: &str| Recorded { domain: "untrusted".into(), ..made_out(out, "fork") };
                anyhow::ensure!(record(c, &trusted, &step("make", &[]), &lock(), &forked("sha256:bin")).is_err());
                anyhow::ensure!(scan(c, Family::Result, &trusted)?.is_empty() && scan(c, Family::EClass, &trusted)?.is_empty(), "nothing of it written");
                let t = record(c, &trusted, &step("make", &[]), &lock(), &made_out("sha256:bin", "main"))?;
                let u = record(c, &untrusted, &Unit { trust: "untrusted".into(), ..step("make --fast", &[]) }, &lock(), &forked("sha256:bin"))?;
                anyhow::ensure!(member(c, &untrusted, &t)?.is_none() && member(c, &trusted, &u)?.is_none(), "each domain's classes hold its own");
                anyhow::ensure!(lookup(c, &trusted, &step("test", &[("bin", eclass::refer(&u))]), &same)?.is_none());
                let (no_op, witness) = rule("drop-true", Rewrite::Drop { op: Pattern::Is("true".into()) }, "make\ntrue", "make");
                register(c, &untrusted, &no_op, &witness)?;
                anyhow::ensure!(rules(c, &trusted)?.rules().count() == 0);
                anyhow::ensure!(lookup(c, &trusted, &step("make\ntrue", &[]), &same)?.is_none());
                // Untrusted lookups read trusted entries (R21), where they are: writes stay home, so a
                // trusted result isn't written among untrusted entries and joins no class there;
                // `lookup_as` reads it in the trusted entries instead.
                anyhow::ensure!(record(c, &untrusted, &step("make", &[]), &lock(), &made_out("sha256:bin", "main")).is_err());
                let read_down = lookup_as(c, None, "acme/app", &Domain::Untrusted, &step("make", &[]), &same)?.hit;
                anyhow::ensure!(read_down.as_ref().is_some_and(|h| h.domain == Domain::Trusted && h.key == t), "{read_down:?}");
                Ok(())
            })
            .await
        })
    }

    /// The replay's history: runs of a five-unit pipeline (each unit's name, what it runs over, and
    /// its script: features added, then its command).
    const PIPELINE: [(&str, &[&str], &[&str]); 5] = [
        ("fetch", &[], &["fetch"]),
        ("build", &["fetch"], &["add a", "add b", "make"]),
        ("test", &["build"], &["add x", "add y", "test"]),
        ("lint", &["fetch"], &["lint"]),
        ("pack", &["build", "test"], &["add a", "add z", "pack"]),
    ];

    const RUNS: usize = 80;

    /// A run: its sources' and docs' trees, and each unit's script as it was written that time.
    #[derive(Debug, Clone)]
    struct Run {
        src: u64,
        docs: u64,
        scripts: Vec<String>,
    }

    /// A deterministic stream (xorshift64*), so the replay is the same every time.
    struct Seeded(u64);

    impl Seeded {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n.max(1)
        }
    }

    /// The history: sources recur, docs change on their own (fetching alike), and scripts are
    /// written with their added features in either order and a no-op now and then, and once in a
    /// while a build adds another feature.
    fn history() -> Vec<Run> {
        let mut rng = Seeded(0x9E37_79B9_7F4A_7C15);
        (0..RUNS)
            .map(|_| {
                let (src, docs) = (rng.below(3), rng.below(3));
                let scripts = PIPELINE
                    .iter()
                    .map(|(name, _, ops)| {
                        let mut ops: Vec<String> = ops.iter().map(|op| (*op).to_owned()).collect();
                        if rng.below(2) == 0
                            && let [a, b, ..] = ops.as_mut_slice()
                            && a.starts_with("add ")
                            && b.starts_with("add ")
                        {
                            std::mem::swap(a, b);
                        }
                        if *name == "build" && rng.below(8) == 0 {
                            ops.insert(0, "add c".into());
                        }
                        if rng.below(3) == 0 {
                            let at = usize::try_from(rng.below(ops.len() as u64 + 1)).unwrap_or_default().min(ops.len());
                            ops.insert(at, "true".into());
                        }
                        ops.join("\n")
                    })
                    .collect();
                Run { src, docs, scripts }
            })
            .collect()
    }

    /// What a script does, by the replay's semantics: the features it adds, as a set, and each
    /// other command with the features it saw. `true` does nothing; adding features commutes.
    fn effect(script: &str) -> String {
        let (mut features, mut steps) = (BTreeSet::new(), Vec::new());
        for op in script.split('\n').map(str::trim).filter(|op| !op.is_empty() && *op != "true") {
            match op.strip_prefix("add ") {
                Some(f) => {
                    features.insert(f);
                }
                None => steps.push(format!("{op} {features:?}")),
            }
        }
        format!("{features:?} {steps:?}")
    }

    /// What a unit of the replay really produces: its script's effect over what it's given.
    fn truth(name: &str, script: &str, given: &[String]) -> String {
        format!("sha256:{}", crate::util::sha256_hex(format!("{name}|{}|{}", effect(script), given.join("|")).as_bytes()))
    }

    /// The replay's rewrite rules, each with its differential test's witness: both scripts run by
    /// the replay's semantics, their outputs compared.
    fn rewrites() -> Vec<(Rule, Witness)> {
        let witness = |name: &str, cases: &[(&str, &str)]| Witness {
            test: format!("{name}: both ways, outputs compared"),
            cases: cases.iter().map(|(before, after)| Case { before: (*before).into(), after: (*after).into(), before_output: truth("witness", before, &[]), after_output: truth("witness", after, &[]) }).collect(),
        };
        vec![
            (Rule { name: "drop-true".into(), kinds: BTreeSet::new(), rewrite: Rewrite::Drop { op: Pattern::Is("true".into()) } }, witness("drop-true", &[("make\ntrue", "make"), ("true\ntest", "test")])),
            (
                Rule { name: "adds-commute".into(), kinds: BTreeSet::new(), rewrite: Rewrite::Commute { a: Pattern::Starts("add ".into()), b: Pattern::Starts("add ".into()) } },
                witness("adds-commute", &[("add a\nadd b\nmake", "add b\nadd a\nmake")]),
            ),
        ]
    }

    /// What a replay saw.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct Tally {
        lookups: u64,
        hits: u64,
        by_class: u64,
        verified: u64,
        wrong: u64,
    }

    /// A unit done in a run of the replay: how the units after it name it (its outputs' digest, or
    /// after a class-based hit its key), the digest of what they'd be given, and of what it really
    /// produces.
    struct Done {
        named: String,
        given: String,
        truly: String,
    }

    /// Replays the history through a ledger of its own, with e-classes (the rules registered, and a
    /// unit after a class-based hit naming it by key) or with plain keys (neither). Each unit is
    /// looked up; a hit is taken; a class-based one is re-checked by verification, which runs it over
    /// what its inputs really are, compares and is recorded; a miss runs over what it's given and
    /// is recorded.
    fn replay(history: &[Run], classes: bool) -> anyhow::Result<Tally> {
        let (s, history) = (crate::store::Store::in_memory()?, history.to_vec());
        crate::rt::block_on(async move {
            s.call(move |c| {
                let sc = scope("acme/app", "trusted");
                if classes {
                    for (rule, witness) in rewrites() {
                        register(c, &sc, &rule, &witness)?;
                    }
                }
                let mut t = Tally::default();
                for (n, run) in history.iter().enumerate() {
                    let mut done: BTreeMap<&str, Done> = BTreeMap::new();
                    for ((name, ups, _), script) in PIPELINE.iter().zip(&run.scripts) {
                        let over = |pick: &dyn Fn(&Done) -> String| -> Vec<(&'static str, String)> { ups.iter().map(|u| (*u, done.get(u).map(pick).unwrap_or_default())).collect() };
                        let (named, given, truly) = (over(&|d| d.named.clone()), over(&|d| d.given.clone()), over(&|d| d.truly.clone()));
                        let unit = |upstream: &[(&str, String)]| Unit {
                            inputs: if *name == "fetch" { BTreeMap::from([("docs".to_owned(), format!("docs{}", run.docs)), ("src".to_owned(), format!("src{}", run.src))]) } else { BTreeMap::new() },
                            ..step(script, upstream)
                        };
                        // A fetch makes what its sources hold, whatever its docs; the rest make what
                        // their scripts do over what they're given.
                        let runs_over = |upstream: &[(&str, String)]| -> Recorded {
                            let inputs: Vec<String> = if *name == "fetch" { vec![format!("src{}", run.src)] } else { upstream.iter().map(|(_, d)| d.clone()).collect() };
                            made_out(&truth(name, script, &inputs), &format!("r{n}"))
                        };
                        let real = runs_over(&truly);
                        t.lookups += 1;
                        let now = match lookup(c, &sc, &unit(&named), &same)? {
                            Some(hit) => {
                                t.hits += 1;
                                let gave = digest(&hit.recorded);
                                if gave != digest(&real) {
                                    t.wrong += 1;
                                }
                                if hit.class.is_some() {
                                    t.by_class += 1;
                                    t.verified += 1;
                                    verified(c, &sc, &hit, &digest(&real))?;
                                    record(c, &sc, &unit(&truly), &lock(), &real)?;
                                    Done { named: eclass::refer(&hit.key), given: gave, truly: digest(&real) }
                                } else {
                                    Done { named: gave.clone(), given: gave, truly: digest(&real) }
                                }
                            }
                            None => {
                                let ran = runs_over(&given);
                                record(c, &sc, &unit(&given), &lock(), &ran)?;
                                Done { named: digest(&ran), given: digest(&ran), truly: digest(&real) }
                            }
                        };
                        done.insert(*name, now);
                    }
                }
                Ok(t)
            })
            .await
        })
    }

    /// W24-AC14: equivalence loses no hits. A deterministic replay of a synthetic history (80 runs
    /// of a five-unit pipeline whose sources recur, whose docs change without changing what's
    /// fetched, and whose scripts are written with commuting operations in either order and no-ops
    /// put in) through the ledger with e-classes and with plain keys. With e-classes the hits are
    /// at least as many (here more, by the rules and by what follows a class-based hit), and when
    /// verification re-checks every class-based hit, none is wrong.
    #[test]
    fn w24_ac14_equivalence_loses_no_hits() -> anyhow::Result<()> {
        let history = history();
        let (with, plain) = (replay(&history, true)?, replay(&history, false)?);
        println!(
            "w24_ac14: {} lookups each; plain keys {} hits; e-classes {} hits, {} of them by class, {} verified, {} wrong",
            with.lookups, plain.hits, with.hits, with.by_class, with.verified, with.wrong
        );
        anyhow::ensure!(plain.by_class == 0 && plain.wrong == 0, "plain keys find only what a unit's own key holds: {plain:?}");
        anyhow::ensure!(with.lookups == plain.lookups && with.hits >= plain.hits, "e-classes lose no hits: {with:?} against {plain:?}");
        anyhow::ensure!(with.by_class > 0 && with.hits > plain.hits, "and the rules add some: {with:?} against {plain:?}");
        anyhow::ensure!(with.verified == with.by_class && with.wrong == 0, "verification re-checked every class-based hit, and none was wrong: {with:?}");
        anyhow::ensure!(replay(&history, true)? == with, "the replay is deterministic");
        Ok(())
    }

    /// A result of `domain`, made on `runner`, with `outputs`.
    fn made(domain: &str, runner: &str, outputs: &[(&str, &str)]) -> Recorded {
        Recorded {
            outcome: "success".into(),
            exit: Some(0),
            outputs: outputs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            provenance: Provenance { runner: runner.into(), run: format!("run-{runner}"), leg: "build".into(), attempt: 1, ms: 60_000, ..Provenance::default() },
            domain: domain.into(),
            ..Recorded::default()
        }
    }

    /// `unit` as `domain` keys it: its trust component that domain.
    fn unit_as(u: &Unit, domain: &str) -> Unit {
        Unit { trust: domain.into(), ..u.clone() }
    }

    /// `unit` keyed as `domain` keys it.
    fn weak_as(u: &Unit, domain: &str) -> Key {
        key::weak(&unit_as(u, domain))
    }

    fn in_store(f: impl FnOnce(&Connection) -> anyhow::Result<()> + Send + 'static) -> anyhow::Result<()> {
        let s = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move { s.call(move |c| f(c)).await })
    }

    /// W24-R22: an entry is never overwritten. The same result again is a reproduction, another
    /// runner's kept beside it as a verification; another result is refused and kept as evidence.
    #[test]
    fn w24_r22_entries_are_never_overwritten() -> anyhow::Result<()> {
        in_store(|c| {
            let sc = scope("acme/app", "trusted");
            let u = unit(1);
            let read = reads(1, 0);
            let first = made("trusted", "r1", &[("bin", "sha256:aa")]);
            let strong = match write(c, None, &sc, &u, &read, &first)? {
                Written::New(k) => k,
                other => anyhow::bail!("{other:?}"),
            };
            // A writer's own say on who verified it isn't kept.
            let claimed = Recorded { verified_by: vec!["r9".into()], ..made("trusted", "r2", &[("bin", "sha256:aa")]) };
            anyhow::ensure!(write(c, None, &sc, &u, &read, &claimed)? == Written::Reproduced { strong, by_another: true });
            anyhow::ensure!(write(c, None, &sc, &u, &read, &made("trusted", "r1", &[("bin", "sha256:aa")]))? == Written::Reproduced { strong, by_another: false });
            let same = |p: &PathSet| p.clone();
            let found = lookup(c, &sc, &u, &same)?.map(|h| h.recorded).ok_or_else(|| anyhow::anyhow!("no hit"))?;
            anyhow::ensure!(found.provenance.runner == "r1" && found.verified_by == ["r2"], "{found:?}");
            // Another output, then another outcome: refused, the entry as it was, and judged again
            // on all its runs (W24-R23): quarantined.
            let poisoned = made("trusted", "r3", &[("bin", "sha256:bad")]);
            let names = vec!["bin".to_owned()];
            anyhow::ensure!(write(c, None, &sc, &u, &read, &poisoned)? == Written::Conflict { strong, comparison: Comparison::Outputs { names: names.clone() }, verdict: Verdict::Nondeterministic { names } });
            let failed = Recorded { outcome: "failure".into(), exit: Some(1), ..made("trusted", "r4", &[("bin", "sha256:aa")]) };
            anyhow::ensure!(matches!(write(c, None, &sc, &u, &read, &failed)?, Written::Conflict { comparison: Comparison::Outcome { .. }, verdict: Verdict::Outcome { .. }, .. }));
            anyhow::ensure!(record(c, &sc, &u, &read, &poisoned).is_err_and(|e| e.to_string().contains("never overwritten")));
            let kept: Recorded = serde_json::from_slice(&get(c, Family::Result, &entry_key(&sc, &strong))?.unwrap_or_default())?;
            anyhow::ensure!(kept.outputs.get("bin").map(String::as_str) == Some("sha256:aa") && kept.provenance.runner == "r1", "{kept:?}");
            anyhow::ensure!(lookup(c, &sc, &u, &same)?.is_none(), "quarantined, it's passed over");
            let evidence = scan(c, Family::Verify, &format!("{}conflict|", verify_prefix(&sc, &strong)))?;
            anyhow::ensure!(evidence.len() == 2, "both conflicts kept: {}", evidence.len());
            let first_conflict: Verification = serde_json::from_slice(&evidence.iter().find(|(k, _, _)| k.contains("|r3|")).ok_or_else(|| anyhow::anyhow!("r3's conflict"))?.1)?;
            anyhow::ensure!(first_conflict.result.as_ref().and_then(|r| r.outputs.get("bin")).map(String::as_str) == Some("sha256:bad"));
            Ok(())
        })
    }

    /// W24-R21: each scope takes only its own domain's results, built on what its domain reads.
    #[test]
    fn w24_r21_scopes_take_only_their_own_domains_results() -> anyhow::Result<()> {
        in_store(|c| {
            let (u, read) = (unit(2), reads(2, 0));
            let trusted = scope("acme/app", "trusted");
            for (result, why) in [
                (made("untrusted", "fork", &[]), "promotion is by reproduction"),
                (made("person:ada", "laptop", &[]), "promotion is by reproduction"),
                (Recorded { lineage: vec!["untrusted".into()], ..made("trusted", "r1", &[]) }, "reads go down only"),
                (made("nobody", "r1", &[]), "trust domain"),
            ] {
                match write(c, None, &trusted, &u, &read, &result)? {
                    Written::Refused(w) => anyhow::ensure!(w.contains(why), "{w}"),
                    other => anyhow::bail!("{result:?} was {other:?}"),
                }
            }
            anyhow::ensure!(scan(c, Family::Result, "acme/app|")?.is_empty() && scan(c, Family::Weak, "acme/app|")?.is_empty(), "nothing written");
            anyhow::ensure!(matches!(write(c, None, "acme/app", &u, &read, &made("trusted", "r1", &[]))?, Written::Refused(_)), "not a scope");
            let ada = scope("acme/app", "person:ada");
            let theirs = Recorded { lineage: vec!["trusted".into(), "person:ada".into()], ..made("person:ada", "laptop", &[]) };
            anyhow::ensure!(matches!(write(c, None, &ada, &unit_as(&u, "person:ada"), &read, &theirs)?, Written::New(_)));
            Ok(())
        })
    }

    /// W24-R21: a lookup reads the domains its unit may read, the most trusted first, each keyed
    /// as that domain keys the unit; a protected unit takes only what two runners produced.
    #[test]
    fn w24_r21_lookups_read_down_only() -> anyhow::Result<()> {
        in_store(|c| {
            let (u, read) = (unit(3), reads(3, 0));
            let same = |p: &PathSet| p.clone();
            let put_as = |domain: &str, runner: &str, out: &str| write(c, None, &scope("acme/app", domain), &unit_as(&u, domain), &read, &made(domain, runner, &[("bin", out)]));
            put_as("untrusted", "fork", "sha256:fork")?;
            put_as("person:ada", "laptop", "sha256:ada")?;
            let found = |reader: &Domain| -> anyhow::Result<Option<(Domain, String)>> {
                Ok(lookup_as(c, None, "acme/app", reader, &u, &same)?.hit.map(|h| (h.domain, h.recorded.outputs.get("bin").cloned().unwrap_or_default())))
            };
            let (t, p, unt, ada, bo) = (Domain::Trusted, Domain::Protected, Domain::Untrusted, Domain::Person("ada".into()), Domain::Person("bo".into()));
            anyhow::ensure!(found(&t)?.is_none(), "trusted never reads untrusted or person entries");
            anyhow::ensure!(found(&unt)? == Some((unt.clone(), "sha256:fork".into())));
            anyhow::ensure!(found(&ada)? == Some((ada.clone(), "sha256:ada".into())));
            anyhow::ensure!(found(&bo)?.is_none(), "people don't read each other's");
            put_as("trusted", "r1", "sha256:main")?;
            for reader in [&t, &unt, &ada, &bo] {
                anyhow::ensure!(found(reader)? == Some((t.clone(), "sha256:main".into())), "{reader} takes the trusted entry first");
            }
            // Protected: one runner isn't enough; a second one's reproduction is.
            let once = lookup_as(c, None, "acme/app", &p, &u, &same)?;
            anyhow::ensure!(once.hit.is_none() && once.passed_over.iter().any(|w| w.contains("two independent")), "{once:?}");
            put_as("trusted", "r2", "sha256:main")?;
            let twice = lookup_as(c, None, "acme/app", &p, &u, &same)?.hit.ok_or_else(|| anyhow::anyhow!("no hit"))?;
            anyhow::ensure!(twice.domain == t && twice.recorded.verified_by == ["r2"], "{twice:?}");
            Ok(())
        })
    }

    /// The audit log's rows of `action`: principal, target and detail, oldest first.
    fn audited(c: &Connection, action: &str) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut q = c.prepare("SELECT principal, COALESCE(target, ''), detail FROM audit2 WHERE action = ?1 AND result = 'refused' ORDER BY id")?;
        let rows = q.query_map([action], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// W24-AC06, through the ledger: a fork's build plants a poisoned rlib. Its entry never
    /// satisfies a trusted lookup, however it gets placed, and every attempt says why and is
    /// audited.
    #[test]
    fn w24_ac06_a_forks_poisoned_rlib_never_reaches_a_trusted_leg() -> anyhow::Result<()> {
        in_store(|c| {
            let (u, read) = (unit(4), reads(4, 0));
            let same = |p: &PathSet| p.clone();
            let poison = [("target/debug/deps/libfoo.rlib", "sha256:poisoned")];
            let fork = scope("acme/app", "untrusted");
            anyhow::ensure!(matches!(write(c, None, &fork, &unit_as(&u, "untrusted"), &read, &made("untrusted", "fork-runner", &poison))?, Written::New(_)));
            for reader in [Domain::Trusted, Domain::Protected] {
                anyhow::ensure!(lookup_as(c, None, "acme/app", &reader, &u, &same)?.hit.is_none(), "{reader} took the fork's entry");
            }
            anyhow::ensure!(audited(c, "work.record")?.is_empty() && audited(c, "work.lookup")?.is_empty(), "nothing amiss yet");
            // Recorded straight into trusted entries: refused, and audited as the fork runner's.
            let trusted = scope("acme/app", "trusted");
            let (tu, tw) = (unit_as(&u, "trusted"), weak_as(&u, "trusted"));
            anyhow::ensure!(matches!(write(c, None, &trusted, &tu, &read, &made("untrusted", "fork-runner", &poison))?, Written::Refused(_)));
            let refused = audited(c, "work.record")?;
            anyhow::ensure!(refused.len() == 1 && refused.iter().all(|(who, target, why)| who == "fork-runner" && *target == trusted && why.contains("promotion is by reproduction only") && why.contains("run run-fork-runner")), "{refused:?}");
            // Copied there as it is, past write's checks (a bug, a restored backup): passed over.
            let strong = key::strong(&tw, &read);
            let row = get(c, Family::Result, &format!("{fork}{}", key::strong(&weak_as(&u, "untrusted"), &read).hex()))?.ok_or_else(|| anyhow::anyhow!("the fork's row"))?;
            let id = crate::util::sha256_hex(&serde_json::to_vec(&read)?);
            put(c, Family::Weak, &format!("{}{id}", weak_prefix(&trusted, &tw)), id.as_bytes(), 1)?;
            put(c, Family::Result, &format!("{trusted}{}", strong.hex()), &row, 1)?;
            let copied = lookup_as(c, None, "acme/app", &Domain::Trusted, &u, &same)?;
            anyhow::ensure!(copied.hit.is_none() && copied.passed_over.iter().any(|w| w.contains("written by untrusted")), "{copied:?}");
            anyhow::ensure!(lookup(c, &trusted, &tu, &same)?.is_none());
            let passed = audited(c, "work.lookup")?;
            anyhow::ensure!(passed.len() == 2 && passed.iter().all(|(who, target, why)| who == "kiln" && *target == format!("{trusted}{strong}") && why.contains("written by untrusted")), "both lookups audited: {passed:?}");
            // Rewritten to claim it's trusted, its lineage still the fork's: passed over.
            let forged = Recorded { lineage: vec!["untrusted".into()], ..made("trusted", "fork-runner", &poison) };
            put(c, Family::Result, &format!("{trusted}{}", strong.hex()), &serde_json::to_vec(&forged)?, 1)?;
            anyhow::ensure!(lookup_as(c, None, "acme/app", &Domain::Trusted, &u, &same)?.passed_over.iter().any(|w| w.contains("reads go down only")));
            anyhow::ensure!(audited(c, "work.lookup")?.last().is_some_and(|(_, _, why)| why.contains("reads go down only")));
            // A trusted entry of its own stands: the poison written over it is refused and kept as
            // evidence, and the entry, in doubt now, is quarantined (W24-R23): nothing is reused
            // until runs again settle which is right.
            delete(c, Family::Result, &format!("{trusted}{}", strong.hex()))?;
            write(c, None, &trusted, &tu, &read, &made("trusted", "r1", &[("target/debug/deps/libfoo.rlib", "sha256:honest")]))?;
            anyhow::ensure!(matches!(write(c, None, &trusted, &tu, &read, &made("trusted", "r1", &poison))?, Written::Conflict { verdict: Verdict::Nondeterministic { .. }, .. }));
            let kept: Recorded = serde_json::from_slice(&get(c, Family::Result, &format!("{trusted}{}", strong.hex()))?.unwrap_or_default())?;
            anyhow::ensure!(kept.outputs.get("target/debug/deps/libfoo.rlib").map(String::as_str) == Some("sha256:honest"), "the entry as it was");
            let l = lookup_as(c, None, "acme/app", &Domain::Trusted, &u, &same)?;
            anyhow::ensure!(l.hit.is_none() && l.passed_over.iter().any(|w| w.contains("quarantined")), "{l:?}");
            Ok(())
        })
    }

    /// W24-R21: quotas evict each domain's entries alone: untrusted entries over their quota go,
    /// the least time saved per byte first, and trusted ones stay.
    #[test]
    fn w24_r21_quotas_evict_only_the_domain_over_quota() -> anyhow::Result<()> {
        in_store(|c| {
            for n in 0..6u64 {
                let r = Recorded { size: 1_000, provenance: Provenance { ms: 1_000 * (n + 1), runner: "r".into(), ..Provenance::default() }, ..made("untrusted", "r", &[]) };
                write(c, None, &scope("acme/app", "untrusted"), &unit_as(&unit(10 + n), "untrusted"), &reads(10 + n, 0), &r)?;
                let t = Recorded { size: 1_000, ..made("trusted", "r", &[]) };
                write(c, None, &scope("acme/app", "trusted"), &unit_as(&unit(20 + n), "trusted"), &reads(20 + n, 0), &t)?;
            }
            let quotas = Quotas { trusted: 1 << 30, protected: 1 << 30, untrusted: 3_500, person: 1 << 30 };
            let gone = evict(c, "acme/app", &quotas)?;
            anyhow::ensure!(!gone.is_empty() && gone.iter().all(|k| k.starts_with("acme/app|untrusted|")), "{gone:?}");
            anyhow::ensure!(scan(c, Family::Result, "acme/app|trusted|")?.len() == 6, "trusted entries all stay");
            let left = scan(c, Family::Result, "acme/app|untrusted|")?;
            let bytes: u64 = left.iter().map(|(_, v, _)| v.len() as u64 + 1_000).sum();
            anyhow::ensure!(bytes <= 3_500, "untrusted within its quota: {bytes}");
            let slowest: Vec<u64> = left.iter().filter_map(|(_, v, _)| serde_json::from_slice::<Recorded>(v).ok()).map(|r| r.provenance.ms).collect();
            anyhow::ensure!(slowest.iter().all(|ms| *ms >= 4_000), "the ones that save the most stay: {slowest:?}");
            Ok(())
        })
    }

    /// A server's key and two runners' identity keys, made as kiln makes them (its attestation key,
    /// their reach identities), each in a folder of its own under `dir`, and the runners' keys as the
    /// server knows them.
    fn keys(dir: &Path) -> anyhow::Result<(crate::ci::attest::Signer, RunnerKey, RunnerKey, BTreeMap<String, String>)> {
        let server = crate::ci::attest::Signer::open(&dir.join("keys"))?;
        let mut runners = BTreeMap::new();
        let mut made = Vec::new();
        for name in ["r1", "r2"] {
            let identity = crate::reach::e2e::Identity::load_or_create(&dir.join(name), name)?;
            let key = RunnerKey::open(&dir.join(name))?.ok_or_else(|| anyhow::anyhow!("{name}'s key"))?;
            anyhow::ensure!(key.keyid == identity.fingerprint, "a runner signs with its reach identity");
            runners.insert(name.to_owned(), key.keyid.clone());
            made.push(key);
        }
        let mut made = made.into_iter();
        let (Some(r1), Some(r2)) = (made.next(), made.next()) else { anyhow::bail!("two runners' keys") };
        anyhow::ensure!(RunnerKey::open(&dir.join("nowhere"))?.is_none(), "no identity, no key");
        Ok((server, r1, r2, runners))
    }

    /// W24-R22: the runner signs, the server countersigns into the scope, and readers check both. An
    /// entry altered, stripped of its runner's signature or moved to another key is a breach,
    /// audited; a result signed with a key that isn't its runner's, or over something else, is
    /// refused; one whose runner has no identity key stands on the countersignature alone for a
    /// trusted unit, not for a protected one, which counts its two runners by their keys.
    #[test]
    fn w24_r22_runners_sign_the_server_countersigns_readers_check_both() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (server, r1, r2, runners) = keys(dir.path())?;
        in_store(move |c| {
            let seal = Seal::new(&server, &runners)?;
            let sc = scope("acme/app", "trusted");
            let read = reads(30, 0);
            let same = |p: &PathSet| p.clone();
            let keyed = |n: u64| {
                let u = unit(n);
                let weak = weak_as(&u, "trusted");
                let strong = key::strong(&weak, &read);
                (u, weak, strong)
            };
            let take = |u: &Unit, reader: &Domain| lookup_as(c, Some(&seal), "acme/app", reader, u, &same);
            let (u, _, strong) = keyed(30);
            // r1 signs and the server countersigns: both kept, both checked.
            let signed = r1.sign(&sc, &strong, &made("trusted", "r1", &[("bin", "sha256:aa")]))?;
            anyhow::ensure!(write(c, Some(&seal), &sc, &u, &read, &signed)? == Written::New(strong));
            let hit = take(&u, &Domain::Trusted)?.hit.ok_or_else(|| anyhow::anyhow!("no hit"))?;
            let sigs = seal::Signatures::parse(&hit.recorded.signatures).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(sigs.runner.map(|r| r.keyid) == Some(r1.keyid.clone()) && sigs.server.map(|s| s.keyid) == Some(server.keyid.clone()));
            // A protected unit wants a second runner's key, which r2's reproduction gives.
            anyhow::ensure!(take(&u, &Domain::Protected)?.passed_over.iter().any(|w| w.contains("has 1")));
            let again = r2.sign(&sc, &strong, &made("trusted", "r2", &[("bin", "sha256:aa")]))?;
            anyhow::ensure!(write(c, Some(&seal), &sc, &u, &read, &again)? == Written::Reproduced { strong, by_another: true });
            let protected = take(&u, &Domain::Protected)?.hit.ok_or_else(|| anyhow::anyhow!("no protected hit"))?;
            anyhow::ensure!(protected.recorded.verified_by == ["r2"], "{protected:?}");
            // Altered where it's kept: breaches, each audited.
            let row = format!("{sc}{}", strong.hex());
            let stored: Recorded = serde_json::from_slice(&get(c, Family::Result, &row)?.unwrap_or_default())?;
            for (altered, why) in [
                (Recorded { outputs: BTreeMap::from([("bin".to_owned(), "sha256:evil".to_owned())]), ..stored.clone() }, "its countersignature doesn't verify"),
                (Recorded { signatures: stored.signatures.iter().filter(|s| s.starts_with("server:")).cloned().collect(), ..stored.clone() }, "its countersignature doesn't verify"),
                (Recorded { signatures: Vec::new(), ..stored.clone() }, "not countersigned into trusted's entries"),
            ] {
                put(c, Family::Result, &row, &serde_json::to_vec(&altered)?, 1)?;
                let l = take(&u, &Domain::Trusted)?;
                anyhow::ensure!(l.hit.is_none() && l.passed_over.iter().any(|w| w.contains(why)), "{why}: {l:?}");
                anyhow::ensure!(audited(c, "work.lookup")?.last().is_some_and(|(_, _, d)| d.contains(why)));
            }
            put(c, Family::Result, &row, &serde_json::to_vec(&stored)?, 1)?;
            anyhow::ensure!(take(&u, &Domain::Trusted)?.hit.is_some(), "as it was, it's taken again");
            // Moved, as it is, to another unit's key: the countersignature is for another claim.
            let (u2, weak2, strong2) = keyed(31);
            let id = crate::util::sha256_hex(&serde_json::to_vec(&read)?);
            put(c, Family::Weak, &format!("{}{id}", weak_prefix(&sc, &weak2)), id.as_bytes(), 1)?;
            put(c, Family::Result, &format!("{sc}{}", strong2.hex()), &serde_json::to_vec(&stored)?, 1)?;
            let moved = take(&u2, &Domain::Trusted)?;
            anyhow::ensure!(moved.hit.is_none() && moved.passed_over.iter().any(|w| w.contains("countersignature doesn't verify")), "{moved:?}");
            // Refused as written: r2's key on a result of r1's, and r1's signature over other outputs.
            let (u3, _, strong3) = keyed(32);
            let impostor = r2.sign(&sc, &strong3, &made("trusted", "r1", &[("bin", "sha256:aa")]))?;
            let swapped = Recorded { outputs: BTreeMap::from([("bin".to_owned(), "sha256:swapped".to_owned())]), ..r1.sign(&sc, &strong3, &made("trusted", "r1", &[("bin", "sha256:aa")]))? };
            for (result, why) in [(impostor, "which isn't r1's"), (swapped, "r1's signature doesn't verify")] {
                match write(c, Some(&seal), &sc, &u3, &read, &result)? {
                    Written::Refused(w) => anyhow::ensure!(w.contains(why), "{w}"),
                    other => anyhow::bail!("{why}: {other:?}"),
                }
                anyhow::ensure!(audited(c, "work.record")?.last().is_some_and(|(who, _, d)| who == "r1" && d.contains(why)));
            }
            anyhow::ensure!(get(c, Family::Result, &format!("{sc}{}", strong3.hex()))?.is_none(), "nothing kept");
            // A runner with no identity key (a job pod's), or a key the server doesn't know: the
            // countersignature alone, which a trusted unit takes and a protected one doesn't.
            for (n, result) in [(33, made("trusted", "pod-7", &[("bin", "sha256:cc")])), (34, r2.sign(&sc, &keyed(34).2, &made("trusted", "r9", &[("bin", "sha256:dd")]))?)] {
                let (u, _, _) = keyed(n);
                anyhow::ensure!(matches!(write(c, Some(&seal), &sc, &u, &read, &result)?, Written::New(_)));
                let hit = take(&u, &Domain::Trusted)?.hit.ok_or_else(|| anyhow::anyhow!("no hit for {n}"))?;
                anyhow::ensure!(hit.recorded.signatures.len() == 1 && hit.recorded.signatures.iter().all(|s| s.starts_with("server:")), "{:?}", hit.recorded.signatures);
                let p = take(&u, &Domain::Protected)?;
                anyhow::ensure!(p.hit.is_none() && p.passed_over.iter().any(|w| w.contains("their runners signed")), "{p:?}");
            }
            Ok(())
        })
    }

    /// W24-AC07's fault injector, deterministic: a unit's run gives the outputs that follow from the
    /// unit alone, unless nondeterminism is injected into it, when each runner's run gives its own.
    fn run_of(n: u64, runner: &str, nondeterministic: bool, reused: &[String]) -> Recorded {
        let out = if nondeterministic { format!("sha256:{n}-{runner}") } else { format!("sha256:{n}") };
        Recorded { reused: reused.to_vec(), ..made("trusted", runner, &[("out", &out)]) }
    }

    /// W24-R23, AC07, by deterministic fault injection: hits sampled at the host's 10 % are run
    /// again on other runners. A result corrupted after it was recorded (its output's digest
    /// flipped where it's kept) and a unit with nondeterminism injected are each detected, and their
    /// lineage quarantined: what was built on them, through what each reused, is passed over with
    /// them, and the rest of the ledger isn't. Runs that agree with each other tell the corrupted
    /// entry from the nondeterministic unit, which alone stays marked, and the mark shows on its
    /// other entries. Kiln quarantines on its own, so the audit log says why.
    #[test]
    fn w24_ac07_corruption_and_nondeterminism_are_detected_and_quarantined() -> anyhow::Result<()> {
        in_store(|c| {
            let sc = scope("acme/app", "trusted");
            let same = |p: &PathSet| p.clone();
            let at = |n: u64| {
                let weak = weak_as(&unit(n), "trusted");
                let strong = key::strong(&weak, &reads(n, 0));
                (weak, strong)
            };
            let row = |n: u64| entry_key(&sc, &at(n).1);
            // a <- b <- c, each reusing the one before; d alone; nd, nondeterministic, <- e.
            let (a, b, cc, d, nd, e) = (40, 41, 42, 43, 44, 45);
            let uses = BTreeMap::from([(b, vec![a]), (cc, vec![b]), (e, vec![nd])]);
            let reused = |n: u64| uses.get(&n).map(|u| u.iter().map(|x| row(*x)).collect::<Vec<_>>()).unwrap_or_default();
            for n in [a, b, cc, d, nd, e] {
                anyhow::ensure!(matches!(write(c, None, &sc, &unit(n), &reads(n, 0), &run_of(n, "r1", n == nd, &reused(n)))?, Written::New(_)));
            }
            // Corrupted after it was recorded: a's output's digest flipped where it's kept.
            let stored: Recorded = serde_json::from_slice(&get(c, Family::Result, &row(a))?.unwrap_or_default())?;
            let flipped = Recorded { outputs: BTreeMap::from([("out".to_owned(), "sha256:4O".to_owned())]), ..stored };
            put(c, Family::Result, &row(a), &serde_json::to_vec(&flipped)?, 1)?;
            // 1,200 hits of a, d and nd, sampled at 10 %; each unit's first two sampled run again,
            // on r2 then r3.
            let (mut drawn, mut checked) = (0, BTreeMap::<u64, usize>::new());
            for hit in 0..400u64 {
                for n in [a, d, nd] {
                    if !verify::sampled(b"the server's seed", &at(n).1, &format!("run{hit}/build/1"), verify::Hermeticity::Host.rate()) {
                        continue;
                    }
                    drawn += 1;
                    let k = checked.entry(n).or_default();
                    if *k < 2 {
                        *k += 1;
                        write(c, None, &sc, &unit(n), &reads(n, 0), &run_of(n, &format!("r{}", *k + 1), n == nd, &reused(n)))?;
                    }
                }
            }
            anyhow::ensure!((60..=180).contains(&drawn), "about 10 % of 1,200 hits drawn: {drawn}");
            anyhow::ensure!(checked.values().all(|k| *k == 2) && checked.len() == 3, "{checked:?}");
            // The corrupted result: two runs agree against it; a, b and c quarantined; a unmarked.
            let va = verdict(c, &sc, &at(a).1)?;
            anyhow::ensure!(matches!(va, Some(Verdict::Corrupted { runs: 2, .. })), "{va:?}");
            anyhow::ensure!(nondeterministic(c, &sc, &at(a).0)?.is_none(), "corrupted, not nondeterministic");
            // The nondeterministic unit: marked; nd and e quarantined.
            anyhow::ensure!(matches!(verdict(c, &sc, &at(nd).1)?, Some(Verdict::Nondeterministic { .. })));
            anyhow::ensure!(nondeterministic(c, &sc, &at(nd).0)?.is_some_and(|m| m.outputs == ["out"] && m.entry == row(nd)));
            for n in [a, b, cc, nd, e] {
                let q = quarantined(c, &sc, &at(n).1)?.ok_or_else(|| anyhow::anyhow!("{n} isn't quarantined"))?;
                anyhow::ensure!(q.root == row(if n <= cc { a } else { nd }), "{n}: {q:?}");
                let l = lookup_as(c, None, "acme/app", &Domain::Trusted, &unit(n), &same)?;
                anyhow::ensure!(l.hit.is_none() && l.passed_over.iter().any(|w| w.contains("quarantined (W24-R23)")), "{n}: {l:?}");
            }
            // d, run again alike twice: verified, and taken.
            anyhow::ensure!(verdict(c, &sc, &at(d).1)? == Some(Verdict::Verified) && quarantined(c, &sc, &at(d).1)?.is_none());
            let hit = lookup_as(c, None, "acme/app", &Domain::Trusted, &unit(d), &same)?.hit.ok_or_else(|| anyhow::anyhow!("d's hit"))?;
            anyhow::ensure!(hit.recorded.verified_by.iter().map(String::as_str).collect::<BTreeSet<_>>() == BTreeSet::from(["r2", "r3"]) && hit.nondeterministic.is_none());
            // What kiln did on its own, in the audit log.
            let mut q = c.prepare("SELECT COALESCE(target, ''), detail FROM audit2 WHERE action = 'work.verify' AND result = 'quarantined' ORDER BY id")?;
            let said = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            anyhow::ensure!(said.iter().any(|(t, why)| *t == row(a) && why.contains("corrupted") && why.contains(&row(cc))), "{said:?}");
            anyhow::ensure!(said.iter().any(|(t, why)| *t == row(nd) && why.contains("nondeterministic") && why.contains(&row(e))), "{said:?}");
            // The mark shows on the nondeterministic unit's other entries.
            write(c, None, &sc, &unit(nd), &reads(nd, 1), &run_of(nd, "r1", true, &[]))?;
            let other = lookup_as(c, None, "acme/app", &Domain::Trusted, &unit(nd), &same)?.hit.ok_or_else(|| anyhow::anyhow!("nd's other entry"))?;
            anyhow::ensure!(other.nondeterministic == Some(vec!["out".to_owned()]), "{other:?}");
            Ok(())
        })
    }

    /// A file's or a tree's digest as a lookup reads it now (W24-R04): a file's content, every file
    /// under a folder for a path ending in `/`, or that it's absent.
    fn read_now(path: &Path) -> Read {
        fn walk(p: &Path, h: &mut Vec<u8>) {
            if let Ok(bytes) = std::fs::read(p) {
                h.extend(crate::util::sha256_hex(&bytes).into_bytes());
                return;
            }
            let mut names: Vec<_> = std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect();
            names.sort();
            for n in names {
                h.extend(n.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default().into_bytes());
                walk(&n, h);
            }
        }
        if !path.exists() {
            return Read::Absent;
        }
        let mut h = Vec::new();
        walk(path, &mut h);
        Read::File(crate::util::sha256_hex(&h))
    }

    /// W24-R04, R05, end to end with a real path set: rustc builds a crate and writes its
    /// dependency file, which [`kiln_ci::work::pathset`] reads into the unit's path set (its
    /// sources, the file `include_str!` read, the variable `option_env!` read); the result is
    /// recorded under it, and a two-phase lookup that reads the files as they are now finds it
    /// while what the crate read is unchanged, from another workspace too, and misses once a file
    /// it read or a variable it read changes, never for a file it didn't read.
    #[test]
    fn w24_r04_two_phase_lookup_end_to_end_on_rustcs_own_record() -> anyhow::Result<()> {
        use kiln_ci::work::pathset::{self, Observed, Roots};
        use std::path::PathBuf;
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
        if !std::process::Command::new(&rustc).arg("--version").output().is_ok_and(|o| o.status.success()) {
            println!("w24_r04: no rustc to make a dependency file with; nothing checked");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let (one, two) = (dir.path().join("ws"), dir.path().join("elsewhere/ws"));
        for ws in [&one, &two] {
            std::fs::create_dir_all(ws.join("src"))?;
            std::fs::write(ws.join("src/lib.rs"), "pub const DATA: &str = include_str!(\"data.txt\");\npub const NOTE: Option<&str> = option_env!(\"KILN_W24_NOTE\");\n")?;
            std::fs::write(ws.join("src/data.txt"), "what the crate reads\n")?;
            std::fs::write(ws.join("README.md"), "what it doesn't\n")?;
        }
        let roots = |ws: &Path| Roots::workspace(&ws.to_string_lossy()).skip(&ws.join("target").to_string_lossy());
        // rustc's own record of what it read, as cargo has it write one.
        let observe = |ws: &Path, note: &str| -> anyhow::Result<Observed> {
            let out = ws.join("target");
            std::fs::create_dir_all(&out)?;
            let o = std::process::Command::new(&rustc)
                .current_dir(ws)
                .env("KILN_W24_NOTE", note)
                .args(["--crate-name", "probe", "--crate-type", "lib", "--edition", "2021", "--emit", "dep-info,metadata", "--out-dir"])
                .arg(&out)
                .arg("src/lib.rs")
                .output()?;
            anyhow::ensure!(o.status.success(), "rustc: {}", String::from_utf8_lossy(&o.stderr));
            let d = pathset::dep_info(&std::fs::read_to_string(out.join("probe.d"))?).map_err(anyhow::Error::msg)?;
            let mut observed = Observed::default();
            observed.dep_info(&d, &ws.to_string_lossy(), &roots(ws));
            Ok(observed)
        };
        let observed = observe(&one, "first")?;
        anyhow::ensure!(observed.paths == BTreeSet::from(["src/data.txt".to_owned(), "src/lib.rs".to_owned()]) && observed.outside.is_empty(), "{observed:?}");
        anyhow::ensure!(observed.env.get("KILN_W24_NOTE") == Some(&Some("first".to_owned())), "{:?}", observed.env);
        // The unit, keyed with the variables it was seen to read; its result recorded under the
        // path set its record gave, each path's digest as it is.
        let names = observed.read_env();
        let unit_with = move |note: &str| {
            let all = BTreeMap::from([("KILN_W24_NOTE".to_owned(), note.to_owned()), ("UNREAD".to_owned(), "x".to_owned())]);
            let env = key::relevant_env(&all, &BTreeSet::new(), &[], "rustc probe", &names);
            Unit { env, definition: "rustc --crate-name probe src/lib.rs".into(), ..unit(50) }
        };
        let now_in = move |ws: PathBuf| move |recorded: &PathSet| -> PathSet { recorded.keys().map(|p| (p.clone(), roots(&ws).resolve(p).map_or(Read::Absent, |at| read_now(Path::new(&at))))).collect() };
        let read = observed.path_set(&|p: &str| roots(&one).resolve(p).map_or(Read::Absent, |at| read_now(Path::new(&at))));
        let (one2, two2) = (one.clone(), two.clone());
        in_store(move |c| {
            let sc = scope("acme/app", "trusted");
            let u = unit_with("first");
            anyhow::ensure!(matches!(write(c, None, &sc, &u, &read, &made("trusted", "r1", &[("libprobe.rmeta", "sha256:meta")]))?, Written::New(_)));
            let found = |u: &Unit, ws: &PathBuf| -> anyhow::Result<bool> { Ok(lookup_as(c, None, "acme/app", &Domain::Trusted, u, &now_in(ws.clone()))?.hit.is_some()) };
            anyhow::ensure!(found(&u, &one2)?, "what it read is as it was");
            anyhow::ensure!(found(&u, &two2)?, "from another workspace, its paths placed alike (W24-R05)");
            std::fs::write(one2.join("README.md"), "changed, and never read\n")?;
            anyhow::ensure!(found(&u, &one2)?, "a file it didn't read");
            std::fs::write(one2.join("src/data.txt"), "changed\n")?;
            anyhow::ensure!(!found(&u, &one2)?, "a file include_str! read changed");
            anyhow::ensure!(found(&u, &two2)?, "the other workspace's is unchanged");
            std::fs::write(one2.join("src/data.txt"), "what the crate reads\n")?;
            anyhow::ensure!(found(&u, &one2)?, "as it was again");
            anyhow::ensure!(!found(&unit_with("second"), &one2)?, "a variable it read changed");
            Ok(())
        })
    }
}
