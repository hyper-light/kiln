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

use base64::Engine as _;
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

/// The commit's signature, as kiln's own statement of it says (`check`: `signature`, what a push's
/// check found; `made`, a commit kiln made and signed itself): who signed it, in words.
pub fn signed_commit(body: &Value) -> Result<String, String> {
    let field = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default();
    match (field("check"), field("seen")) {
        ("made", _) => Ok(format!("made and signed by {}", field("by"))),
        ("signature", "by") => Ok(format!("signed by {}", field("words"))),
        ("signature", "unsigned") => Err("it isn't signed".into()),
        ("signature", "unverified") => Err(format!("its signature doesn't verify: {}", field("words"))),
        _ => Err("kiln's word on its signature says neither".into()),
    }
}

/// The land link: `land` (signed as the queue) landed `commit`, the candidate it tested, every check
/// on it passing.
pub fn landed(commit: &str, land: &Land) -> Result<String, String> {
    if !land.candidate.eq_ignore_ascii_case(commit) {
        return Err(format!("the queue tested {}, not {}", short(&land.candidate), short(commit)));
    }
    if let Some(c) = land.checks.iter().find(|c| c.conclusion != "success") {
        return Err(format!("its check {} concluded {}", c.name, c.conclusion));
    }
    Ok(format!("landed on {} by its queue, the candidate tested ({} checks passed)", land.trunk, land.checks.len()))
}

/// The review link: verdict `b`, signed by `signer`, holds on version `landed` of its diff (kept as
/// `versions`): given there, or carried there by verbatim patch-id.
pub fn approved(b: &Body, signer: &Identity, versions: &[Kept], landed: u32) -> Result<String, String> {
    let who = match &signer.actor {
        Actor::Person { name } => name.clone(),
        Actor::Agent { person, agent, .. } => format!("agent {agent} for {person}"),
        _ => signer.to_string(),
    };
    match verdict::holds_on(&b.revision, versions, landed) {
        Holds::Given => Ok(format!("approved by {who} (v{landed})")),
        Holds::Carried { from } => Ok(format!("approved by {who} (v{from}, carried to v{landed})")),
        Holds::Not(why) => Err(format!("{who}'s verdict doesn't hold on v{landed}: {why}")),
    }
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
        assert_eq!(signed_commit(&json!({ "check": "signature", "seen": "by", "words": "kiln://hyper-light/person/sam" })), Ok("signed by kiln://hyper-light/person/sam".into()));
        assert_eq!(signed_commit(&json!({ "check": "made", "by": "kiln://hyper-light/server" })), Ok("made and signed by kiln://hyper-light/server".into()));
        assert_eq!(signed_commit(&json!({ "check": "signature", "seen": "unsigned" })), Err("it isn't signed".into()));

        let land = Land { repo: "acme/app".into(), trunk: "main".into(), base: "b".repeat(40), candidate: commit.clone(), batch: vec![Landed { diff: "D1".into(), version: 2, commit: commit.clone(), verdicts: vec![] }], checks: vec![Ran { name: "gate".into(), conclusion: "success".into(), results: vec![] }] };
        assert_eq!(landed(&commit, &land), Ok("landed on main by its queue, the candidate tested (1 checks passed)".into()));
        assert_eq!(landed(&"d".repeat(40), &land), Err("the queue tested ccccccc, not ddddddd".into()), "a commit that wasn't tested");
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
}
