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

use std::collections::{BTreeMap, BTreeSet};

use kiln_ci::work::eclass::{self, ClassId, Classes as _, Member, Refused, Rule, Rules, Split, Why, Witness};
use kiln_ci::work::key::{self, Key, PathSet, Unit};
use kiln_ci::work::tests as test_results;
use kiln_ci::work::tests::{Binary, Choice, Context, Found, Kept, Marks, Ran};
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
    /// The runners that reproduced it (W24-R23), and its signatures (W24-R22).
    pub verified_by: Vec<String>,
    pub signatures: Vec<String>,
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

/// Records what a unit produced (W24-R02, R30): its path set kept by digest and listed under its
/// weak key (the newest [`PATH_SETS`] kept), its result under its strong key, and that key in the
/// class of its outputs' digest, with its canonical key claimed into it when that isn't its own.
/// Only successes are to be recorded (W24-R07). Refused, with nothing written, when `scope`'s
/// lookups may not read what the result's domain writes (W24-R21). Returns the strong key.
pub fn record(c: &Connection, scope: &str, unit: &Unit, read: &PathSet, result: &Recorded) -> anyhow::Result<Key> {
    let writer = writer(scope, result)?;
    let tx = crate::store::tx(c)?;
    let keyed = keyed(&tx, scope, unit)?;
    let now = crate::util::now_unix();
    let set = serde_json::to_vec(read)?;
    let id = crate::util::sha256_hex(&set);
    put(&tx, Family::PathSet, &id, &set, now)?;
    list(&tx, Family::Weak, &weak_prefix(scope, &keyed.weak), &id)?;
    let strong = key::strong(&keyed.weak, read);
    put(&tx, Family::Result, &format!("{scope}{}", strong.hex()), &serde_json::to_vec(result)?, now)?;
    join(&mut Rows { c: &tx, scope }, &keyed, &strong, read, &id, &writer, &digest(result))?;
    tx.commit()?;
    Ok(strong)
}

/// The trust domain that wrote `result` into `scope`: the result's, else the scope's own (W24-R21).
/// Refused when the scope's lookups may not read what that domain writes.
fn writer(scope: &str, result: &Recorded) -> anyhow::Result<String> {
    let reader = domain_of(scope);
    let writer = if result.domain.is_empty() { reader } else { result.domain.as_str() };
    if !eclass::reads(reader, writer) {
        return Err(Refused::Domain { reader: reader.to_owned(), writer: writer.to_owned() }.into());
    }
    Ok(writer.to_owned())
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

/// A result found for a unit (W24-R04, R30).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The strong key it was found under: the unit's own, or its canonical form's. A unit after it
    /// that takes its outputs before they're materialized names it by this key ([`eclass::refer`]).
    pub key: Key,
    pub recorded: Recorded,
    /// How it was found by class, when it wasn't under the unit's own key: what verification
    /// re-checks (W24-R23).
    pub class: Option<ByClass>,
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

/// Finds a unit's result (W24-R04, R30). First under its strong key: its weak key's path sets,
/// newest first, each made a strong key from what is at its paths now (`now` reads them), and the
/// first with a recorded result. Then by class, under its canonical key: the path sets of the units
/// whose canonical form it keys, each made the canonical form's strong key the same way, and the
/// first whose member serves a class-based hit, with its class's result.
pub fn lookup(c: &Connection, scope: &str, unit: &Unit, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Option<Hit>> {
    let keyed = keyed(c, scope, unit)?;
    for id in path_sets(c, scope, &keyed.weak)? {
        let Some(read) = path_set(c, &id)? else { continue };
        let strong = key::strong(&keyed.weak, &now(&read));
        if let Some(found) = get(c, Family::Result, &format!("{scope}{}", strong.hex()))? {
            return Ok(Some(Hit { key: strong, recorded: serde_json::from_slice(&found)?, class: None }));
        }
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
        let class = ByClass { class: p.member.class, source, why: keyed.canonical.why.clone(), took: keyed.took.clone() };
        return Ok(Some(Hit { key: at, recorded, class: Some(class) }));
    }
    Ok(None)
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
/// [`record`], under the binary's weak key and what it read (its test key,
/// [`kiln_ci::work::tests::key`]), so [`lookup`] finds it only while its digest, arguments,
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
    let strong = record(c, scope, &test_results::weak(b, cx), read, &result)?;
    put(c, Family::Test, &last_at(scope, b, cx), &serde_json::to_vec(&(&kept, read))?, now)?;
    Ok(Some(strong))
}

/// What the ledger has for a test binary (W24-R10), as [`kiln_ci::work::tests::select`] asks: a
/// pass under its test key by two-phase lookup (only for a binary with its digest), with that key's
/// flaky tests as they are now; and its identity's newest pass, with whether what that read holds.
/// `now` gives a path set as it is now.
pub fn found_test(c: &Connection, scope: &str, b: &Binary, cx: &Context, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Found> {
    let mut by_key = None;
    if b.digest.is_some()
        && let Some((strong, found)) = lookup(c, scope, &test_results::weak(b, cx), now)?
        && let Some(kept) = found.outputs.get("test").map(|t| serde_json::from_str::<Kept>(t)).transpose()?
    {
        by_key = Some(Kept { flaky: marks(c, scope, &strong)?.flaky(), ..kept });
    }
    let last: Option<(Kept, PathSet)> = get(c, Family::Test, &last_at(scope, b, cx))?.map(|v| serde_json::from_slice(&v)).transpose()?;
    let last_holds = last.as_ref().is_some_and(|(_, read)| now(read) == *read);
    Ok(Found { by_key, last: last.map(|(k, _)| k), last_holds })
}

/// Which of a run's test binaries run and which replay their passes (W24-R10), each saying why:
/// [`kiln_ci::work::tests::select`] over what the ledger has for each ([`found_test`]).
pub fn select_tests(c: &Connection, scope: &str, binaries: &[Binary], cx: &Context, affected: &BTreeSet<String>, now: &dyn Fn(&PathSet) -> PathSet) -> anyhow::Result<Vec<(Binary, Choice)>> {
    let found: Vec<Found> = binaries.iter().map(|b| found_test(c, scope, b, cx, now)).collect::<anyhow::Result<_>>()?;
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
    fn made(out: &str, run: &str) -> Recorded {
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
                let a = record(c, &sc, &fetch("d1", "s1"), &lock(), &made("sha256:src1", "r1"))?;
                let b = record(c, &sc, &fetch("d2", "s1"), &lock(), &made("sha256:src1", "r2"))?;
                let other = record(c, &sc, &fetch("d1", "s2"), &lock(), &made("sha256:src2", "r3"))?;
                let class = |k: &Key| member(c, &sc, k).map(|m| m.map(|m| m.class));
                let d = digest(&made("sha256:src1", "r1"));
                anyhow::ensure!(a != b && class(&a)? == Some(ClassId::Output(d.clone())) && class(&b)? == class(&a)?, "one class: {:?} {:?}", class(&a)?, class(&b)?);
                anyhow::ensure!(class(&other)? != class(&a)?, "other outputs, another class");
                // A build over a's outputs ran; one over b, named by key, takes b by class.
                record(c, &sc, &step("make", &[("src", d.clone())]), &lock(), &made("sha256:bin1", "r1"))?;
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
                let k = record(c, &sc, &make, &lock(), &made("sha256:bin", "r1"))?;
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
                let v = record(c, &sc, &variant, &lock(), &made("sha256:bin", "r2"))?;
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
                let k = record(c, &sc, &make, &lock(), &made("sha256:bin", "r1"))?;
                let d = digest(&made("sha256:bin", "r1"));
                record(c, &sc, &step("test", &[("bin", d.clone())]), &lock(), &made("sha256:passed", "r1"))?;
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
                let l = record(c, &sc, &lint_over, &lock(), &made("sha256:clean", "r1"))?;
                let lint = step("lint", &[("bin", d.clone())]);
                anyhow::ensure!(lookup(c, &sc, &lint, &same)?.is_some_and(|h| h.class.is_some()));
                // Verification runs the echoing unit: other outputs.
                let observed = digest(&made("sha256:bin+stamp", "verify"));
                let quarantined = verified(c, &sc, &hit, &observed)?;
                anyhow::ensure!(quarantined.contains(&k) && quarantined.contains(&l), "{quarantined:?}");
                anyhow::ensure!(lookup(c, &sc, &echoes, &same)?.is_none(), "the class serves no class-based hit");
                anyhow::ensure!(lookup(c, &sc, &test_over, &same)?.is_none(), "nor does what takes it by class");
                anyhow::ensure!(lookup(c, &sc, &lint, &same)?.is_none(), "its lineage's claims are dropped");
                anyhow::ensure!(lookup(c, &sc, &make, &same)?.is_some_and(|h| h.key == k && h.class.is_none()), "make's own result stands");
                anyhow::ensure!(lookup(c, &sc, &lint_over, &same)?.is_some_and(|h| h.key == l && h.class.is_none()), "as does the lint's");
                anyhow::ensure!(member(c, &sc, &k)?.is_some_and(|m| m.quarantined && m.anchored()));
                // The verification's run, recorded, is the echoing unit's own class, claimed into none.
                let e = record(c, &sc, &echoes, &lock(), &made("sha256:bin+stamp", "verify"))?;
                anyhow::ensure!(lookup(c, &sc, &echoes, &same)?.is_some_and(|h| h.key == e && h.class.is_none()));
                anyhow::ensure!(member(c, &sc, &e)?.is_some_and(|m| m.class == ClassId::Output(observed.clone()) && !m.quarantined));
                // Verification sees make's own result differ: make is nondeterministic.
                let again = lookup(c, &sc, &make, &same)?.context("make's own result")?;
                let quarantined = verified(c, &sc, &again, &digest(&made("sha256:other", "v2")))?;
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
                let forked = |out: &str| Recorded { domain: "untrusted".into(), ..made(out, "fork") };
                anyhow::ensure!(record(c, &trusted, &step("make", &[]), &lock(), &forked("sha256:bin")).is_err());
                anyhow::ensure!(scan(c, Family::Result, &trusted)?.is_empty() && scan(c, Family::EClass, &trusted)?.is_empty(), "nothing of it written");
                let t = record(c, &trusted, &step("make", &[]), &lock(), &made("sha256:bin", "main"))?;
                let u = record(c, &untrusted, &Unit { trust: "untrusted".into(), ..step("make --fast", &[]) }, &lock(), &forked("sha256:bin"))?;
                anyhow::ensure!(member(c, &untrusted, &t)?.is_none() && member(c, &trusted, &u)?.is_none(), "each domain's classes hold its own");
                anyhow::ensure!(lookup(c, &trusted, &step("test", &[("bin", eclass::refer(&u))]), &same)?.is_none());
                let (no_op, witness) = rule("drop-true", Rewrite::Drop { op: Pattern::Is("true".into()) }, "make\ntrue", "make");
                register(c, &untrusted, &no_op, &witness)?;
                anyhow::ensure!(rules(c, &trusted)?.rules().count() == 0);
                anyhow::ensure!(lookup(c, &trusted, &step("make\ntrue", &[]), &same)?.is_none());
                // Untrusted lookups read trusted entries (R21): one written to their scope joins
                // the class of its outputs there.
                let both = record(c, &untrusted, &step("make", &[]), &lock(), &made("sha256:bin", "main"))?;
                anyhow::ensure!(member(c, &untrusted, &both)?.map(|m| m.class) == member(c, &untrusted, &u)?.map(|m| m.class));
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
                            made(&truth(name, script, &inputs), &format!("r{n}"))
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
}
