//! kiln verify, the pure part (docs/ci.md W27-R10): the chain from a deploy back to whoever made
//! what it runs, a link at a time.
//!
//! - **Each link rests on a statement kiln took** ([`Held`]): checked as of when kiln took it (its
//!   signer's chain to kiln's root, the revocations as of then), and found in the trail by an
//!   inclusion proof against a checkpoint kiln's server signed ([`held`]). kiln's own words (what it
//!   checked that isn't a statement itself) carry no signature of their own: the checkpoint vouches
//!   for them.
//! - **What each link says** is the statement's own: the provenance of an artifact names it and the
//!   commit it was built from ([`artifact`]), signed by the leg that built it ([`built_by`]); a
//!   commit's signature as kiln checked it ([`signed_commit`]); the land the queue signed, its
//!   candidate the commit that landed ([`landed`]); a verdict that holds on the revision that landed,
//!   its own or carried by verbatim patch-id ([`approved`]).
//! - **The first link that doesn't hold is named** ([`first_broken`]).
//! - **For an auditor**: a trail export ([`Export`]) carries every statement a chain rested on, each
//!   with its proof, and the checkpoint the proofs lead to, checked offline against kiln's root
//!   ([`offline`]); a revision's Source VSA (SLSA's source track, [`source_vsa`]) names the level
//!   its chain verifies at.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::chain::{self, Actor, Identity, Revocation};
use super::landing::Land;
use super::statement;
use super::trail::{self, Entry, Hash};
use super::verdict::{self, Body, Holds, Kept};

/// Where the trail holds a statement: its entry, the entry's proof in the tree a checkpoint names,
/// and that checkpoint: its signed note, the certificate it's signed under (DER), and when.
#[derive(Debug, Clone, PartialEq)]
pub struct Proven {
    pub entry: Entry,
    pub proof: Vec<Hash>,
    pub note: String,
    pub certificate: Vec<u8>,
    pub signed_at: i64,
}

/// A statement as a chain has it: its payload type and payload, its signature as it came (a Sigstore
/// bundle) or none (kiln's own words), when kiln took it, and where the trail holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub bundle: Option<Value>,
    pub taken_at: i64,
    pub proven: Option<Proven>,
}

/// What a chain is checked against: kiln's root (DER), every revocation, and kiln's organization.
pub struct Against<'a> {
    pub root: &'a [u8],
    pub revocations: &'a [Revocation],
    pub org: &'a str,
}

/// One link of a chain: what it is, in words, and whether it holds, or why not.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub words: String,
    pub holds: Result<(), String>,
}

impl Link {
    pub fn holds(words: impl Into<String>) -> Self {
        Self { words: words.into(), holds: Ok(()) }
    }

    pub fn broken(words: impl Into<String>, why: impl Into<String>) -> Self {
        Self { words: words.into(), holds: Err(why.into()) }
    }
}

/// The first link of `chain` that doesn't hold.
pub fn first_broken(chain: &[Link]) -> Option<&Link> {
    chain.iter().find(|l| l.holds.is_err())
}

/// Whether `h` holds: the trail has it, under a checkpoint kiln's server signed, and it's signed by
/// whom its certificate names, as of when kiln took it. Its signer, when it's signed; none for kiln's
/// own words, which the checkpoint over them vouches for.
pub fn held(h: &Held, a: &Against<'_>) -> Result<Option<Identity>, String> {
    let digest = statement::digest(&h.payload_type, &h.payload);
    let p = h.proven.as_ref().ok_or_else(|| format!("{digest} isn't in the trail"))?;
    if p.entry.statement != digest {
        return Err(format!("the trail's entry {} names {}, not this statement ({digest})", p.entry.index, p.entry.statement));
    }
    let (cp, _) = trail::verify_note(&p.note, a.org, &[&p.certificate], a.root, a.revocations, p.signed_at).map_err(|e| format!("the checkpoint over it doesn't verify: {e}"))?;
    if p.entry.index >= cp.size || !trail::verify_inclusion(&p.entry.hash(), p.entry.index, cp.size, &p.proof, &cp.root) {
        return Err(format!("the trail's entry {} isn't in the tree its checkpoint (of {}) signs", p.entry.index, cp.size));
    }
    match &h.bundle {
        None => Ok(None),
        Some(b) => signed_by(b, h, a).map(Some),
    }
}

/// Who signed bundle `b` of `h`'s statement: its one signature over DSSE's PAE, under the chain of
/// certificates it carries, verified to kiln's root as of when kiln took it.
fn signed_by(b: &Value, h: &Held, a: &Against<'_>) -> Result<Identity, String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let text = |p: &str| b.pointer(p).and_then(Value::as_str);
    if text("/dsseEnvelope/payloadType") != Some(h.payload_type.as_str()) || text("/dsseEnvelope/payload").and_then(|p| b64.decode(p).ok()).as_deref() != Some(h.payload.as_slice()) {
        return Err("its signature is of another statement".into());
    }
    let sig = text("/dsseEnvelope/signatures/0/sig").and_then(|s| b64.decode(s).ok()).ok_or_else(|| "it carries no signature".to_owned())?;
    let chain: Vec<Vec<u8>> = match b.pointer("/verificationMaterial/x509CertificateChain/certificates").and_then(Value::as_array) {
        Some(certs) => certs.iter().filter_map(|c| c.get("rawBytes").and_then(Value::as_str)).filter_map(|c| b64.decode(c).ok()).collect(),
        None => text("/verificationMaterial/certificate/rawBytes").and_then(|c| b64.decode(c).ok()).into_iter().collect(),
    };
    let chain: Vec<&[u8]> = chain.iter().map(Vec::as_slice).collect();
    let v = chain::verify_signed(&chain, a.root, a.revocations, h.taken_at, &statement::to_sign(&h.payload_type, &h.payload), &sig).map_err(|e| e.to_string())?;
    Ok(v.identity)
}

/// What an artifact's provenance says it was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The source's URI as the provenance names it (`git+https://<host>/<repo>@<ref>`).
    pub source: String,
    pub commit: String,
}

/// The artifact link: provenance `payload` (an in-toto statement) names `digest` (`sha256:<hex>`)
/// as what it's about, and the source commit it was built from.
pub fn artifact(digest: &str, payload: &[u8]) -> Result<Built, String> {
    let st: Value = serde_json::from_slice(payload).map_err(|_| "its provenance isn't JSON".to_owned())?;
    let want = digest.strip_prefix("sha256:").unwrap_or(digest).to_ascii_lowercase();
    let names_it = st.get("subject").and_then(Value::as_array).into_iter().flatten().any(|s| s.pointer("/digest/sha256").and_then(Value::as_str).is_some_and(|h| h.eq_ignore_ascii_case(&want)));
    if !names_it {
        return Err(format!("its provenance is of something else than {digest}"));
    }
    st.pointer("/predicate/buildDefinition/resolvedDependencies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|d| Some(Built { source: d.get("uri").and_then(Value::as_str).filter(|u| u.starts_with("git+"))?.to_owned(), commit: d.pointer("/digest/gitCommit").and_then(Value::as_str)?.to_ascii_lowercase() }))
        .ok_or_else(|| "its provenance names no commit it was built from".to_owned())
}

/// The build link: `signer` of the provenance is the leg that built it, in words.
pub fn built_by(signer: &Identity) -> Result<String, String> {
    match &signer.actor {
        Actor::Leg { machine, run, attempt, leg, .. } => Ok(format!("build leg '{leg}' of run {run} (attempt {attempt}) on {machine}")),
        _ => Err(format!("its provenance is signed by {signer}, not the leg that built it")),
    }
}

/// Who `id` is, in words: a person by name, an agent for its person, kiln's server or CD.
pub fn who(id: &Identity) -> String {
    match &id.actor {
        Actor::Person { name } => name.clone(),
        Actor::Agent { person, agent, .. } => format!("agent {agent} for {person}"),
        Actor::Server => "kiln's server".into(),
        Actor::Cd => "kiln's CD".into(),
        _ => id.to_string(),
    }
}

/// Identity `uri` in words, as [`who`] says it; as it's written when it isn't one.
fn named(uri: &str) -> String {
    Identity::parse(uri).map_or_else(|_| uri.to_owned(), |id| who(&id))
}

/// The commit's signature, as kiln's own statement of it says (`check`: `signature`, what a push's
/// check found; `made`, a commit kiln made and signed itself): who signed it, in words.
pub fn signed_commit(body: &Value) -> Result<String, String> {
    let field = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default();
    match (field("check"), field("seen")) {
        ("made", _) => Ok(format!("made and signed by {}", named(field("by")))),
        ("signature", "by") => Ok(format!("signed by {}", named(field("words")))),
        ("signature", "unsigned") => Err("it isn't signed".into()),
        ("signature", "unverified") => Err(format!("its signature doesn't verify: {}", field("words"))),
        _ => Err("kiln's word on its signature says neither".into()),
    }
}

/// The land link: `land` (signed as the queue) landed `commit`: the commit it moved the trunk to, or
/// a diff's commit under it in the batch, every check on the candidate it tested passing. That's
/// the candidate itself, unless the queue applied the batch again onto a tip that moved since only
/// for changes it shares nothing with (`rebased`), which the link says.
pub fn landed(commit: &str, land: &Land) -> Result<String, String> {
    let top = land.to.eq_ignore_ascii_case(commit);
    if !top && !land.batch.iter().any(|l| l.commit.eq_ignore_ascii_case(commit)) {
        return Err(format!("the queue moved {} to {}, which doesn't carry {}", land.trunk, short(&land.to), short(commit)));
    }
    if let Some(c) = land.checks.iter().find(|c| c.conclusion != "success") {
        return Err(format!("its check {} concluded {}", c.name, c.conclusion));
    }
    let tested = match (top, land.rebased) {
        (true, false) => "the candidate tested".to_owned(),
        (false, false) => format!("in a batch of {} under the candidate tested, {}", land.batch.len(), short(&land.candidate)),
        (_, true) => format!("in a batch of {} applied again onto {} after the candidate tested, {}", land.batch.len(), short(&land.from), short(&land.candidate)),
    };
    // Merged by its host after kiln's queue tested it (W26-R19): the candidate tested, the host's
    // commit; one merged while Review held it was never tested.
    if land.via.starts_with(crate::land::queue::HOST_MERGES) && land.via.ends_with(crate::land::queue::HELD_NOTE) {
        return Ok(format!("landed on {}: merged on its host while Review held it, untested by kiln's queue", land.trunk));
    }
    if land.via == crate::land::queue::HOST_MERGES && land.rebased {
        let onto = if land.from.is_empty() { "a tip kiln couldn't read".to_owned() } else { short(&land.from).to_string() };
        return Ok(format!("landed on {}: its host merged what kiln's queue tested in candidate {}, applied again onto {onto}", land.trunk, short(&land.candidate)));
    }
    if land.via == crate::land::queue::HOST_MERGES {
        return Ok(format!("landed on {}: kiln's queue tested candidate {}, and its host merged it ({} checks passed)", land.trunk, short(&land.candidate), land.checks.len()));
    }
    // A host's own queue tested and landed it (W26-R19): kiln's server says what it saw.
    if !land.via.is_empty() {
        return Ok(format!("landed on {} by its host's {}, as kiln saw it land ({} checks passed)", land.trunk, land.via, land.checks.len()));
    }
    Ok(format!("landed on {} by its queue, {tested} ({} checks passed)", land.trunk, land.checks.len()))
}

/// The review link: verdict `b`, signed by `signer`, holds on version `landed` of its diff (kept as
/// `versions`): given there, or carried there by verbatim patch-id.
pub fn approved(b: &Body, signer: &Identity, versions: &[Kept], landed: u32) -> Result<String, String> {
    let who = who(signer);
    match verdict::holds_on(&b.revision, versions, landed) {
        Holds::Given => Ok(format!("approved by {who} (v{landed})")),
        Holds::Carried { from } => Ok(format!("approved by {who} (v{from}, carried to v{landed})")),
        Holds::Not(why) => Err(format!("{who}'s verdict doesn't hold on v{landed}: {why}")),
    }
}

/// The predicate type of SLSA's verification summary: a Source VSA's.
pub const VSA: &str = "https://slsa.dev/verification_summary/v1";

/// What a revision's Source VSA is about (SLSA v1.2's source track): its name (`<repository>@<commit>`),
/// where a person finds it, its commit, the refs that point at it (fully qualified), its
/// repository's URI (SPDX's download location, `git+https://<host>/<repo>`), who verified it, when,
/// against which policy, and whether its chain verified.
pub struct SourceRevision<'a> {
    pub name: &'a str,
    pub uri: &'a str,
    pub commit: &'a str,
    pub refs: &'a [String],
    pub repository: &'a str,
    pub verifier: &'a str,
    pub at: i64,
    pub policy: &'a str,
    pub passed: bool,
}

/// A revision's Source VSA (SLSA v1.2's source track), an in-toto statement for kiln to sign. It's
/// about the revision's commit (`gitCommit`, and the refs pointing at it as `sourceRefs`), verified
/// at `at` against `policy`, and names only the highest source level it meets: 3 when its chain
/// verifies (landed through its queue, tested and reviewed as its trunk's policy asks, by identities
/// kiln verifies); 1 when it doesn't (it's in version control: that's all kiln vouches for).
pub fn source_vsa(r: &SourceRevision<'_>) -> Value {
    serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": r.name, "uri": r.uri, "digest": { "gitCommit": r.commit }, "annotations": { "sourceRefs": r.refs } }],
        "predicateType": VSA,
        "predicate": {
            "verifier": { "id": r.verifier },
            "timeVerified": crate::trigger::context::rfc3339(r.at),
            "resourceUri": r.repository,
            "policy": { "uri": r.policy },
            "verificationResult": if r.passed { "PASSED" } else { "FAILED" },
            "verifiedLevels": [if r.passed { "SLSA_SOURCE_LEVEL_3" } else { "SLSA_SOURCE_LEVEL_1" }],
        },
    })
}

/// What a trail export is (`kiln verify export`).
pub const EXPORT_KIND: &str = "kiln-trail-export/v1";

/// A trail export, for an auditor to check offline against kiln's root ([`offline`]): its target,
/// kiln's organization, the checkpoint every statement's proof leads to, every statement a link
/// rested on (and kiln's own statements of each revocation), and the links as kiln found them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Export {
    pub kind: String,
    pub target: String,
    pub org: String,
    pub checkpoint: ExportedCheckpoint,
    pub statements: Vec<Exported>,
    pub links: Vec<ExportedLink>,
}

/// The checkpoint an export's proofs lead to: its signed note, the certificate it's signed under
/// (DER, base64), and when it was signed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedCheckpoint {
    pub note: String,
    pub certificate: String,
    pub signed_at: i64,
}

/// A statement as an export carries it: what it says (its payload base64), its entry, the entry's
/// proof (hex), each signature as kiln kept it, and whom the walk found it signed by (none: any,
/// or kiln's own words).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exported {
    pub payload_type: String,
    pub payload: String,
    pub entry: ExportedEntry,
    pub proof: Vec<String>,
    #[serde(default)]
    pub signatures: Vec<ExportedSignature>,
    #[serde(default)]
    pub signer: Option<String>,
}

/// A trail entry as an export carries it ([`trail::Entry`], the hash before it hex).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedEntry {
    pub index: u64,
    pub prev: String,
    pub statement: String,
    pub kind: String,
    pub subjects: Vec<String>,
    pub taken_at: i64,
}

/// A signature as an export carries it: its Sigstore bundle, and when kiln took it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedSignature {
    pub bundle: Value,
    pub taken_at: i64,
}

/// A link as kiln found it, and the digests of the statements it rests on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedLink {
    pub link: String,
    pub holds: bool,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub rests_on: Vec<String>,
}

/// Export `e` checked offline against kiln's root `root` (`kiln verify --offline`): the checkpoint
/// is signed by the org's server under it; each statement is the one its entry names, in the tree
/// the checkpoint signs, and signed as the walk found it, verified to the root as of when kiln took
/// it (with the revocations kiln's own statements of them in the export say). Each link as kiln
/// found it, broken where a statement it rests on doesn't hold; the checkpoint's link first.
pub fn offline(e: &Export, root: &[u8]) -> Result<Vec<Link>, String> {
    if e.kind != EXPORT_KIND {
        return Err(format!("it's a {}, not a trail export", e.kind));
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let certificate = b64.decode(&e.checkpoint.certificate).map_err(|_| "its checkpoint's certificate isn't base64".to_owned())?;
    let held_of = |s: &Exported| -> Result<Held, String> {
        let payload = b64.decode(&s.payload).map_err(|_| format!("statement {}'s payload isn't base64", s.entry.statement))?;
        let hash = |h: &str| -> Result<Hash, String> { trail::unhex(h).and_then(|b| b.try_into().ok()).ok_or_else(|| format!("{h:?} isn't a SHA-256")) };
        let entry = Entry { index: s.entry.index, prev: hash(&s.entry.prev)?, statement: s.entry.statement.clone(), kind: s.entry.kind.clone(), subjects: s.entry.subjects.clone(), taken_at: s.entry.taken_at };
        let proof = s.proof.iter().map(|h| hash(h)).collect::<Result<Vec<_>, _>>()?;
        let proven = Proven { entry, proof, note: e.checkpoint.note.clone(), certificate: certificate.clone(), signed_at: e.checkpoint.signed_at };
        Ok(Held { payload_type: s.payload_type.clone(), payload, bundle: None, taken_at: s.entry.taken_at, proven: Some(proven) })
    };
    // The revocations: kiln's own statements of them that the export carries, each in the trail.
    let mut revocations = Vec::new();
    for s in e.statements.iter().filter(|s| s.payload_type == trail::PAYLOAD_REVOCATION) {
        let h = held_of(s)?;
        held(&h, &Against { root, revocations: &[], org: &e.org }).map_err(|why| format!("revocation {}: {why}", s.entry.statement))?;
        let st: statement::Statement = serde_json::from_slice(&h.payload).map_err(|_| format!("revocation {} doesn't read", s.entry.statement))?;
        let text = |k: &str| st.body.get(k).and_then(Value::as_str).map(str::to_owned);
        let at = st.body.get("at").and_then(Value::as_i64).ok_or_else(|| format!("revocation {} says no time", s.entry.statement))?;
        revocations.push(Revocation { serial: text("serial"), key: text("key"), at, reason: text("reason").unwrap_or_default(), by: text("by").unwrap_or_default() });
    }
    let a = Against { root, revocations: &revocations, org: &e.org };
    let (cp, _) = trail::verify_note(&e.checkpoint.note, &e.org, &[&certificate], root, &revocations, e.checkpoint.signed_at).map_err(|why| format!("its checkpoint doesn't verify: {why}"))?;
    let mut found: std::collections::BTreeMap<&str, Result<(), String>> = std::collections::BTreeMap::new();
    for s in &e.statements {
        let r = held_of(s).and_then(|h| {
            if s.signatures.is_empty() {
                return held(&h, &a).map(|_| ());
            }
            let mut why = None;
            for sig in &s.signatures {
                match held(&Held { bundle: Some(sig.bundle.clone()), taken_at: h.taken_at.max(sig.taken_at), ..h.clone() }, &a) {
                    Ok(Some(id)) if s.signer.as_deref().is_none_or(|want| id.uri().is_ok_and(|u| u == want)) => return Ok(()),
                    Ok(Some(id)) => {
                        why.get_or_insert(format!("it's signed by {id}, not {}", s.signer.as_deref().unwrap_or_default()));
                    }
                    Ok(None) => {}
                    Err(w) => {
                        why.get_or_insert(w);
                    }
                }
            }
            Err(why.unwrap_or_else(|| "it carries no signature".into()))
        });
        found.insert(s.entry.statement.as_str(), r);
    }
    let mut out = vec![Link::holds(format!("the trail's checkpoint of {} entries, signed by {}'s server", cp.size, e.org))];
    for l in &e.links {
        let broken = l.rests_on.iter().find_map(|d| match found.get(d.as_str()) {
            Some(Ok(())) => None,
            Some(Err(why)) => Some(why.clone()),
            None => Some(format!("the export carries no statement {d}")),
        });
        out.push(match (broken, l.holds) {
            (Some(why), _) => Link::broken(l.link.clone(), why),
            (None, true) => Link::holds(l.link.clone()),
            (None, false) => Link::broken(l.link.clone(), l.why.clone().unwrap_or_else(|| "kiln found it doesn't hold".into())),
        });
    }
    Ok(out)
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::landing::{Landed, Ran};
    use crate::identity::trail::Checkpoint;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SigningKey as _, date_time_ymd};
    use serde_json::json;

    const AT: i64 = 1_790_000_000;
    const ORG: &str = "hyper-light";

    struct Ca {
        der: Vec<u8>,
        issuer: Issuer<'static, KeyPair>,
    }

    fn ca() -> anyhow::Result<Ca> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, ORG);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2025, 1, 1);
        p.not_after = date_time_ymd(2036, 1, 1);
        let cert = p.self_signed(&key)?;
        Ok(Ca { der: cert.der().to_vec(), issuer: Issuer::new(p, key) })
    }

    fn signer(ca: &Ca, uri: &str) -> anyhow::Result<(Vec<u8>, KeyPair)> {
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.subject_alt_names = vec![SanType::URI(uri.try_into()?)];
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.not_before = date_time_ymd(2026, 1, 1);
        p.not_after = date_time_ymd(2027, 1, 1);
        let key = KeyPair::generate()?;
        let der = p.signed_by(&key, &ca.issuer)?.der().to_vec();
        Ok((der, key))
    }

    /// `payload` of `payload_type` signed by `key` under `cert`, as kiln keeps it.
    fn bundle(payload_type: &str, payload: &[u8], cert: &[u8], key: &KeyPair) -> anyhow::Result<Value> {
        Ok(statement::bundle(payload_type, payload, &key.sign(&statement::to_sign(payload_type, payload))?, cert))
    }

    /// A trail of `statements` (payload type, payload), checkpointed by `server`: each held, proven.
    fn trailed(statements: &[(String, Vec<u8>)], server: (&[u8], &KeyPair)) -> anyhow::Result<Vec<Proven>> {
        let mut entries: Vec<Entry> = Vec::new();
        for (t, p) in statements {
            let (digest, kind, subjects) = trail::about(t, p);
            let e = trail::next(entries.last(), &digest, &kind, subjects, AT)?;
            entries.push(e);
        }
        let cp = Checkpoint::of(&trail::origin(ORG), &entries);
        let note = trail::signed(&cp, None, |m| Ok((server.1.sign(m).map_err(|e| e.to_string())?, server.0.to_vec())))?;
        let leaves: Vec<Hash> = entries.iter().map(Entry::hash).collect();
        Ok(entries.iter().enumerate().map(|(i, e)| Proven { entry: e.clone(), proof: trail::inclusion(&leaves, i).unwrap_or_default(), note: note.clone(), certificate: server.0.to_vec(), signed_at: AT }).collect())
    }

    #[test]
    fn w27_r10_a_statement_holds_signed_and_in_the_trail() -> anyhow::Result<()> {
        let ca = ca()?;
        let server = signer(&ca, "kiln://hyper-light/server")?;
        let (kim, kim_key) = signer(&ca, "kiln://hyper-light/person/kim")?;
        let said = statement::payload(&statement::Statement::new(json!({ "words": "a verdict" })))?;
        let own = statement::payload(&trail::checked_commit("acme/app", "refs/heads/main", &"a".repeat(40), None, "by", Some("kiln://hyper-light/person/kim")))?;
        let proven = trailed(&[("application/vnd.kiln.review.verdict+json".into(), said.clone()), (trail::PAYLOAD_CHECKED.into(), own.clone())], (&server.0, &server.1))?;
        let a = Against { root: &ca.der, revocations: &[], org: ORG };
        let signed = Held { payload_type: "application/vnd.kiln.review.verdict+json".into(), payload: said.clone(), bundle: Some(bundle("application/vnd.kiln.review.verdict+json", &said, &kim, &kim_key)?), taken_at: AT, proven: proven.first().cloned() };
        assert_eq!(held(&signed, &a).map_err(anyhow::Error::msg)?.map(|i| i.actor), Some(Actor::Person { name: "kim".into() }));
        let kilns = Held { payload_type: trail::PAYLOAD_CHECKED.into(), payload: own, bundle: None, taken_at: AT, proven: proven.get(1).cloned() };
        assert_eq!(held(&kilns, &a).map_err(anyhow::Error::msg)?, None, "kiln's own words: vouched for by the checkpoint");

        // Not in the trail; its entry edited; another statement's entry; a checkpoint not the server's;
        // a signature of something else; a signer revoked before kiln took it.
        assert!(held(&Held { proven: None, ..signed.clone() }, &a).is_err_and(|e| e.contains("isn't in the trail")));
        let mut edited = signed.clone();
        if let Some(p) = edited.proven.as_mut() {
            p.entry.taken_at += 1;
        }
        assert!(held(&edited, &a).is_err_and(|e| e.contains("isn't in the tree")), "an entry edited");
        assert!(held(&Held { proven: kilns.proven.clone(), ..signed.clone() }, &a).is_err_and(|e| e.contains("names")), "another statement's entry");
        let (sam, sam_key) = signer(&ca, "kiln://hyper-light/person/sam")?;
        let by_sam = trailed(&[("application/vnd.kiln.review.verdict+json".into(), said.clone())], (&sam, &sam_key))?;
        assert!(held(&Held { proven: by_sam.first().cloned(), ..signed.clone() }, &a).is_err_and(|e| e.contains("checkpoint")), "a checkpoint sam signed");
        let other = statement::payload(&statement::Statement::new(json!({ "words": "another" })))?;
        let wrong = Held { bundle: Some(bundle("application/vnd.kiln.review.verdict+json", &other, &kim, &kim_key)?), ..signed.clone() };
        assert!(held(&wrong, &a).is_err_and(|e| e.contains("another statement")));
        let (_, cert) = x509_parser::parse_x509_certificate(&kim).map_err(|e| anyhow::anyhow!("{e}"))?;
        let key = chain::hex(&<sha2::Sha256 as sha2::Digest>::digest(cert.public_key().raw));
        let revoked = [Revocation { serial: None, key: Some(key), at: AT - 10, reason: "key lost".into(), by: "kim".into() }];
        assert!(held(&signed, &Against { revocations: &revoked, ..a }).is_err(), "kim's certificate revoked before kiln took it");
        Ok(())
    }

    #[test]
    fn w27_r10_each_link_says_what_it_found() -> anyhow::Result<()> {
        let commit = "c".repeat(40);
        let provenance = serde_json::to_vec(&json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{ "name": "image", "digest": { "sha256": "ab".repeat(32) } }],
            "predicateType": "https://slsa.dev/provenance/v1",
            "predicate": { "buildDefinition": { "resolvedDependencies": [{ "uri": "git+https://kiln.home/acme/app@refs/heads/main", "digest": { "gitCommit": commit.to_ascii_uppercase() } }] } },
        }))?;
        let digest = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(artifact(&digest, &provenance), Ok(Built { source: "git+https://kiln.home/acme/app@refs/heads/main".into(), commit: commit.clone() }));
        assert!(artifact(&format!("sha256:{}", "cd".repeat(32)), &provenance).is_err_and(|e| e.contains("something else")), "an artifact rebuilt");
        let leg = Identity::parse("kiln://hyper-light/person/admin/machine/fedora-2/instance/main/run/1/attempt/1/leg/image").map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(built_by(&leg), Ok("build leg 'image' of run 1 (attempt 1) on fedora-2".into()));
        assert!(built_by(&Identity::new(ORG, Actor::Server)).is_err());
        assert_eq!(signed_commit(&json!({ "check": "signature", "seen": "by", "words": "kiln://hyper-light/person/sam" })), Ok("signed by sam".into()));
        assert_eq!(signed_commit(&json!({ "check": "made", "by": "kiln://hyper-light/server" })), Ok("made and signed by kiln's server".into()));
        assert_eq!(signed_commit(&json!({ "check": "signature", "seen": "unsigned" })), Err("it isn't signed".into()));

        let land = Land {
            repo: "acme/app".into(),
            trunk: "main".into(),
            from: "b".repeat(40),
            to: commit.clone(),
            candidate: commit.clone(),
            base: "b".repeat(40),
            rebased: false,
            batch: vec![Landed { diff: "D1".into(), version: 2, commit: commit.clone(), verdicts: vec![] }],
            checks: vec![Ran { name: "gate".into(), conclusion: "success".into(), results: vec![] }],
            via: String::new(),
        };
        assert_eq!(landed(&commit, &land), Ok("landed on main by its queue, the candidate tested (1 checks passed)".into()));
        let seen = Land { via: "merge queue".into(), ..land.clone() };
        assert_eq!(landed(&commit, &seen), Ok("landed on main by its host's merge queue, as kiln saw it land (1 checks passed)".into()), "W26-R19");
        let merged = Land { via: crate::land::queue::HOST_MERGES.into(), ..land.clone() };
        assert_eq!(landed(&commit, &merged), Ok("landed on main: kiln's queue tested candidate ccccccc, and its host merged it (1 checks passed)".into()), "W26-R19");
        let held = Land { via: format!("{}{}", crate::land::queue::HOST_MERGES, crate::land::queue::HELD_NOTE), ..land.clone() };
        assert_eq!(landed(&commit, &held), Ok("landed on main: merged on its host while Review held it, untested by kiln's queue".into()), "W26-R19");
        let again = Land { rebased: true, from: "e".repeat(40), ..merged.clone() };
        assert_eq!(landed(&commit, &again), Ok("landed on main: its host merged what kiln's queue tested in candidate ccccccc, applied again onto eeeeeee".into()), "W26-R19");
        assert_eq!(landed(&"d".repeat(40), &land), Err("the queue moved main to ccccccc, which doesn't carry ddddddd".into()), "a commit that wasn't tested");
        let again = Land { from: "e".repeat(40), to: "f".repeat(40), rebased: true, batch: vec![Landed { diff: "D1".into(), version: 2, commit: "f".repeat(40), verdicts: vec![] }], ..land.clone() };
        assert_eq!(landed(&"f".repeat(40), &again), Ok("landed on main by its queue, in a batch of 1 applied again onto eeeeeee after the candidate tested, ccccccc (1 checks passed)".into()), "applied again, it says so");
        let batch = Land { batch: vec![Landed { diff: "D0".into(), version: 1, commit: "a".repeat(40), verdicts: vec![] }, Landed { diff: "D1".into(), version: 2, commit: commit.clone(), verdicts: vec![] }], ..land.clone() };
        assert_eq!(landed(&"a".repeat(40), &batch), Ok("landed on main by its queue, in a batch of 2 under the candidate tested, ccccccc (1 checks passed)".into()), "a diff's commit under the candidate");
        let failed = Land { checks: vec![Ran { name: "gate".into(), conclusion: "failure".into(), results: vec![] }], ..land.clone() };
        assert!(landed(&commit, &failed).is_err_and(|e| e.contains("gate concluded failure")));

        // Kim's verdict on v1, carried to v2 by its verbatim patch-id; on another patch, it doesn't hold.
        let kim = Identity::new(ORG, Actor::Person { name: "kim".into() });
        let rev = |n: u32, patch: &str| verdict::Revision { repo: "acme/app".into(), diff: "D1".into(), version: n, commit: format!("{n}").repeat(40), patch_id: patch.into() };
        let body = verdict::read(verdict::PAYLOAD_VERDICT, &verdict::verdict("kiln://hyper-light/person/kim", &rev(1, &"e".repeat(40)), "accept", "fine")).map_err(anyhow::Error::msg)?;
        let kept = [Kept { n: 1, commit: "1".repeat(40), patch_id: Some("e".repeat(40)) }, Kept { n: 2, commit: "2".repeat(40), patch_id: Some("e".repeat(40)) }];
        assert_eq!(approved(&body, &kim, &kept, 2), Ok("approved by kim (v1, carried to v2)".into()));
        let other = [Kept { n: 1, commit: "1".repeat(40), patch_id: Some("e".repeat(40)) }, Kept { n: 2, commit: "2".repeat(40), patch_id: Some("f".repeat(40)) }];
        assert!(approved(&body, &kim, &other, 2).is_err_and(|e| e.starts_with("kim's verdict doesn't hold on v2")), "a verdict on another patch");
        let chain = [Link::holds("artifact"), Link::broken("approved by kim", "another patch"), Link::broken("later", "x")];
        assert_eq!(first_broken(&chain).map(|l| l.words.as_str()), Some("approved by kim"));
        Ok(())
    }

    #[test]
    fn w27_r10_a_source_vsa_says_what_slsa_asks() {
        let (commit, refs) = ("c".repeat(40), vec!["refs/heads/main".to_owned()]);
        let r = |passed: bool| SourceRevision {
            name: "acme/app@cccc",
            uri: "https://kiln.home/ship/acme/app/cccc",
            commit: &commit,
            refs: &refs,
            repository: "git+https://kiln.home/acme/app",
            verifier: "https://kiln.home/api/verify",
            at: AT,
            policy: "git+https://kiln.home/acme/app@refs/heads/main#.kiln/review.yaml",
            passed,
        };
        let v = source_vsa(&r(true));
        let at = |p: &str| v.pointer(p).cloned().unwrap_or_default();
        assert_eq!((at("/_type"), at("/predicateType"), at("/subject/0/name")), (json!("https://in-toto.io/Statement/v1"), json!(VSA), json!("acme/app@cccc")));
        assert_eq!((at("/subject/0/digest/gitCommit"), at("/subject/0/annotations/sourceRefs"), at("/subject/0/uri")), (json!(commit), json!(["refs/heads/main"]), json!("https://kiln.home/ship/acme/app/cccc")));
        assert_eq!((at("/predicate/resourceUri"), at("/predicate/verifier/id"), at("/predicate/policy/uri")), (json!("git+https://kiln.home/acme/app"), json!("https://kiln.home/api/verify"), json!("git+https://kiln.home/acme/app@refs/heads/main#.kiln/review.yaml")));
        assert_eq!(at("/predicate/timeVerified"), json!(crate::trigger::context::rfc3339(AT)));
        assert_eq!((at("/predicate/verificationResult"), at("/predicate/verifiedLevels")), (json!("PASSED"), json!(["SLSA_SOURCE_LEVEL_3"])), "only the highest level met");
        let failed = source_vsa(&r(false));
        assert_eq!((failed.pointer("/predicate/verificationResult"), failed.pointer("/predicate/verifiedLevels")), (Some(&json!("FAILED")), Some(&json!(["SLSA_SOURCE_LEVEL_1"]))));
    }

    /// An export of kim's verdict and kiln's word on a commit, the trail holding kiln's statement of
    /// a revocation of `revoked`'s key at `AT - 10`; the server's checkpoint over all three.
    fn export_of(server: &(Vec<u8>, KeyPair), kim: &(Vec<u8>, KeyPair), revoked: &[u8]) -> anyhow::Result<Export> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let verdict_t = "application/vnd.kiln.review.verdict+json";
        let said = statement::payload(&statement::Statement::new(json!({ "words": "a verdict" })))?;
        let own = statement::payload(&trail::checked_commit("acme/app", "refs/heads/main", &"c".repeat(40), None, "by", Some("kiln://hyper-light/person/kim")))?;
        let (_, cert) = x509_parser::parse_x509_certificate(revoked).map_err(|e| anyhow::anyhow!("{e}"))?;
        let key = chain::hex(&<sha2::Sha256 as sha2::Digest>::digest(cert.public_key().raw));
        let revocation = statement::payload(&trail::revocation(None, Some(&key), "someone", AT - 10, "key lost", "admin"))?;
        let stated = [(verdict_t.to_owned(), said.clone()), (trail::PAYLOAD_CHECKED.to_owned(), own.clone()), (trail::PAYLOAD_REVOCATION.to_owned(), revocation.clone())];
        let proven = trailed(&stated, (&server.0, &server.1))?;
        let ex = |i: usize, signatures: Vec<ExportedSignature>, signer: Option<&str>| -> anyhow::Result<Exported> {
            let (p, (t, payload)) = proven.get(i).zip(stated.get(i)).ok_or_else(|| anyhow::anyhow!("statement {i}"))?;
            Ok(Exported {
                payload_type: t.clone(),
                payload: b64.encode(payload),
                entry: ExportedEntry { index: p.entry.index, prev: chain::hex(&p.entry.prev), statement: p.entry.statement.clone(), kind: p.entry.kind.clone(), subjects: p.entry.subjects.clone(), taken_at: p.entry.taken_at },
                proof: p.proof.iter().map(|h| chain::hex(h)).collect(),
                signatures,
                signer: signer.map(str::to_owned),
            })
        };
        let digest = |i: usize| proven.get(i).map(|p| p.entry.statement.clone()).unwrap_or_default();
        Ok(Export {
            kind: EXPORT_KIND.into(),
            target: "commit acme/app@cccc".into(),
            org: ORG.into(),
            checkpoint: ExportedCheckpoint { note: proven.first().map(|p| p.note.clone()).unwrap_or_default(), certificate: b64.encode(&server.0), signed_at: AT },
            statements: vec![ex(0, vec![ExportedSignature { bundle: bundle(verdict_t, &said, &kim.0, &kim.1)?, taken_at: AT }], Some("kiln://hyper-light/person/kim"))?, ex(1, Vec::new(), None)?, ex(2, Vec::new(), None)?],
            links: vec![
                ExportedLink { link: "approved by kim (v1)".into(), holds: true, why: None, rests_on: vec![digest(0)] },
                ExportedLink { link: "ccccccc on main, signed by kim".into(), holds: true, why: None, rests_on: vec![digest(1)] },
            ],
        })
    }

    /// W27-R10: a trail export verifies offline against kiln's root, and doesn't once anything in it
    /// is changed: a statement, a proof, the signer, the checkpoint; nor where the trail's own
    /// statement of a revocation says the signer's key was revoked before kiln took it.
    #[test]
    fn w27_r10_an_export_holds_offline_and_fails_when_changed() -> anyhow::Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let ca = ca()?;
        let server = signer(&ca, "kiln://hyper-light/server")?;
        let kim = signer(&ca, "kiln://hyper-light/person/kim")?;
        let sam = signer(&ca, "kiln://hyper-light/person/sam")?;
        let lee = signer(&ca, "kiln://hyper-light/person/lee")?;
        let export = export_of(&server, &kim, &lee.0)?;
        let links = offline(&export, &ca.der).map_err(anyhow::Error::msg)?;
        assert!(links.len() == 3 && links.iter().all(|l| l.holds.is_ok()), "{links:?}");
        let back: Export = serde_json::from_value(serde_json::to_value(&export)?)?;
        assert_eq!(back, export, "as `kiln verify export` writes it and `--offline` reads it");
        let broken = |e: &Export| -> anyhow::Result<(String, String)> {
            let links = offline(e, &ca.der).map_err(anyhow::Error::msg)?;
            let b = first_broken(&links).ok_or_else(|| anyhow::anyhow!("nothing broken: {links:?}"))?;
            Ok((b.words.clone(), b.holds.clone().err().unwrap_or_default()))
        };
        let mut changed = export.clone();
        if let Some(s) = changed.statements.get_mut(1) {
            s.payload = b64.encode(br#"{"check":"signature","seen":"by","words":"kiln://hyper-light/person/mallory"}"#);
        }
        let (link, why) = broken(&changed)?;
        assert!(link.starts_with("ccccccc on main") && why.contains("names"), "a statement changed: {link}: {why}");
        let mut proof = export.clone();
        if let Some(h) = proof.statements.first_mut().and_then(|s| s.proof.first_mut()) {
            *h = "00".repeat(32);
        }
        let (link, why) = broken(&proof)?;
        assert!(link.starts_with("approved by kim") && why.contains("isn't in the tree"), "a proof changed: {link}: {why}");
        let mut other = export.clone();
        if let Some(s) = other.statements.first_mut() {
            let said = b64.decode(&s.payload)?;
            s.signatures = vec![ExportedSignature { bundle: bundle(&s.payload_type, &said, &sam.0, &sam.1)?, taken_at: AT }];
        }
        let (link, why) = broken(&other)?;
        assert!(link.starts_with("approved by kim") && why.contains("not kiln://hyper-light/person/kim"), "signed by another: {link}: {why}");
        let mut note = export.clone();
        note.checkpoint.note = note.checkpoint.note.replacen("kiln://", "kiln:/", 1);
        assert!(offline(&note, &ca.der).is_err_and(|e| e.contains("checkpoint")), "the checkpoint changed");
        let revoked = export_of(&server, &kim, &kim.0)?;
        let (link, _) = broken(&revoked)?;
        assert!(link.starts_with("approved by kim"), "kim's key revoked before kiln took her verdict, as the trail says: {link}");
        Ok(())
    }
}
