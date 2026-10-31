//! A review's verdicts and comments, signed by whoever gave them, over their revision (docs/ci.md
//! W27-R04). kiln says what to sign (`POST /api/diffs/:id/statement`: the statement of the verdict
//! or comment asked, on the diff's latest version by its commit and verbatim patch-id); its giver
//! signs it where their key is (a person's device or browser, an agent's runner) and gives it with
//! the verdict or comment (`signed`). kiln takes it as of when it does, against its root and the
//! revocations: signed by whoever gives it, saying what's asked. It's kept beside them
//! (`review_signatures`), and where the repository requires signatures (its trunk's
//! `.kiln/review.yaml` `signatures: mode`, else its own) an unsigned one is refused and the gate
//! counts a verdict only where its signature holds ([`holding`]).

use std::collections::HashMap;
use std::sync::OnceLock;

use base64::Engine as _;
use http::StatusCode;
use kiln_ci::identity::statement;
use kiln_ci::identity::verdict::{self, Asked, Holds, Kept, Place, Revision};
use kiln_ci::land::Action;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{Diff, Version};
use crate::authz::Caller;
use crate::hub::Hub;

/// Verbatim patch-ids remembered, by commit: a commit's never changes.
const PATCH_IDS_KEPT: usize = 10_000;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(why: String) -> Fail {
    (StatusCode::UNPROCESSABLE_ENTITY, why)
}

/// `commit`'s verbatim patch-id: `git patch-id --verbatim` of its diff as `git show` gives it,
/// context and whitespace as they are. W26-R16's replays make this same comparison for their
/// `same_patch`; once both are on main, a replay's carrying can read that instead. Remembered.
pub async fn verbatim(hub: &Hub, path: &str, commit: &str) -> Option<String> {
    static KEPT: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let kept = KEPT.get_or_init(Default::default);
    if let Some(p) = kept.lock().get(commit) {
        return Some(p.clone());
    }
    let patch = super::diffs::git_run(hub, &["-C", path, "show", "--no-color", "--format=", commit], &[], Vec::new()).await.ok()?.out;
    let out = super::diffs::git_with_input(hub, &["-C", path, "patch-id", "--verbatim"], patch).await.ok()?;
    let id = String::from_utf8_lossy(&out).split_whitespace().next().map(str::to_owned)?;
    let mut k = kept.lock();
    if k.len() >= PATCH_IDS_KEPT {
        k.clear();
    }
    k.insert(commit.to_owned(), id.clone());
    Some(id)
}

/// Version `v` of `d`, as a verdict or comment is signed over it.
pub async fn revision(hub: &Hub, d: &Diff, v: &Version) -> Result<Revision, Fail> {
    let (_, path) = super::diffs::home(hub, d).await?;
    let patch_id = verbatim(hub, &path, &v.commit).await.ok_or_else(|| (StatusCode::UNPROCESSABLE_ENTITY, format!("{} v{} has no patch to sign over", d.name, v.n)))?;
    Ok(Revision { repo: d.repo.clone(), diff: d.name.clone(), version: v.n, commit: v.commit.clone(), patch_id })
}

/// Whether `d`'s repository requires signatures (W27-R03, R04): its trunk's `.kiln/review.yaml`
/// `signatures: mode`, else the repository's own.
pub async fn required(hub: &Hub, d: &Diff) -> Result<bool, Fail> {
    let (repo, path) = super::diffs::home(hub, d).await?;
    let (policy, _) = super::review::policy(hub, &path, &d.trunk).await;
    Ok(match policy.signatures.mode {
        Some(m) => m == kiln_ci::land::SignatureMode::Required,
        None => hub.store.repo_requires_signatures(repo.id).await.map_err(fail)?,
    })
}

/// What `by` signs to give what `asked` asks: its payload type, the statement, its payload and the
/// bytes signed (DSSE's PAE of it), those two base64.
pub fn to_sign(by: &str, asked: &Asked<'_>) -> Value {
    let (payload_type, s) = match asked.kind {
        "comment" => (verdict::PAYLOAD_COMMENT, verdict::comment(by, &asked.revision, &asked.place, asked.words, asked.suggestion)),
        _ => (verdict::PAYLOAD_VERDICT, verdict::verdict(by, &asked.revision, asked.action, asked.words)),
    };
    let payload = statement::payload(&s).unwrap_or_default();
    let b64 = base64::engine::general_purpose::STANDARD;
    json!({ "payload_type": payload_type, "statement": s, "payload": b64.encode(&payload), "pae": b64.encode(statement::to_sign(payload_type, &payload)) })
}

/// Where a comment asked for with `body` is, as `POST /api/diffs/:id/comments` places it: in its
/// `thread`, or on its `file`'s `side` (`new` unless said) from `start_line` (or `line`) to
/// `end_line` (that line unless said), or on the diff as a whole.
pub fn place_of(body: &Value) -> Place {
    let line = |k: &str| body.get(k).and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok());
    let file = body.get("file").and_then(Value::as_str).map(str::to_owned);
    let start = line("start_line").or_else(|| line("line")).filter(|_| file.is_some());
    Place {
        thread: body.get("thread").and_then(Value::as_str).map(str::to_owned),
        side: file.as_ref().map(|_| body.get("side").and_then(Value::as_str).unwrap_or("new").to_owned()),
        end: line("end_line").or(start).filter(|_| file.is_some()),
        start,
        file,
    }
}

/// Whom `caller` signs as: the person they are, or the agent session's identity, the one kiln
/// issued it last.
async fn signer_of(hub: &Hub, caller: &Caller) -> Result<String, Fail> {
    match caller.agent.as_deref() {
        None => crate::identities::person_uri(hub, caller).await.map_err(|e| (StatusCode::FORBIDDEN, format!("a verdict or comment is signed by whoever gives it: a person signed in, or an agent through its runner ({e})"))),
        Some(session) => crate::identities::agent_uri(&hub.store, session).await.map_err(fail)?.ok_or_else(|| (StatusCode::CONFLICT, format!("agent session {session} has no identity yet: its runner asks for one as it starts"))),
    }
}

/// `POST /api/diffs/:id/statement` `{kind: verdict | comment, action, body, suggestion, version,
/// thread, file, side, start_line, end_line}`: what its giver signs to give that verdict (on the
/// diff's latest version) or comment (on its `version`, the latest unless said), as whoever asks,
/// and whether the repository requires it.
pub async fn statement(hub: &Hub, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or("verdict");
    let words = body.get("body").and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let suggestion = body.get("suggestion").and_then(Value::as_str);
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let wanted = body.get("version").and_then(Value::as_u64).filter(|_| kind == "comment");
    let v = match wanted {
        Some(n) => versions.iter().find(|v| u64::from(v.n) == n),
        None => versions.last(),
    }
    .ok_or_else(|| bad(format!("{} has no such version", d.name)))?;
    let revision = revision(hub, d, v).await?;
    let action = body.get("action").and_then(Value::as_str).unwrap_or_default();
    let asked = match kind {
        "verdict" if Action::parse(action).is_some() => Asked { kind, revision, action, words: &words, ..Asked::default() },
        "verdict" => return Err(bad("`action`: accept, request_changes, comment or resign".into())),
        "comment" => Asked { kind, revision, place: place_of(body), words: &words, suggestion, ..Asked::default() },
        other => return Err(bad(format!("`kind` is `verdict` or `comment`, not `{other}`"))),
    };
    let mut out = to_sign(&signer_of(hub, caller).await?, &asked);
    if let Some(o) = out.as_object_mut() {
        o.insert("required".into(), json!(required(hub, d).await?));
    }
    Ok(out)
}

/// An edit of a comment's words: its diff, and what the comment says once edited, in its thread,
/// on the version its thread is on.
pub struct Edit {
    pub diff: Diff,
    pub revision: Revision,
    pub place: Place,
    pub words: String,
    pub suggestion: Option<String>,
}

impl Edit {
    pub fn asked(&self) -> Asked<'_> {
        Asked { kind: "comment", revision: self.revision.clone(), place: self.place.clone(), words: &self.words, suggestion: self.suggestion.as_deref(), ..Asked::default() }
    }
}

/// What editing comment `id` with `body` (`body`, `suggestion`: what isn't said stays) makes it.
pub async fn edit_of(hub: &Hub, id: &str, body: &Value) -> Result<Edit, Fail> {
    let key = id.to_owned();
    let row: Option<(String, u32, String, String, Option<String>)> = hub
        .store
        .call(move |c| {
            let sql = "SELECT t.diff_id, t.version, t.id, c.body, c.suggestion FROM review_comments c JOIN review_threads t ON t.id = c.thread_id WHERE c.id = ?1";
            Ok(c.query_row(sql, [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?)
        })
        .await
        .map_err(fail)?;
    let Some((diff_id, version, thread, was, was_suggestion)) = row else { return Err((StatusCode::NOT_FOUND, "no such comment".into())) };
    let diff = super::diffs::find(hub, &diff_id).await.map_err(fail)?.ok_or((StatusCode::NOT_FOUND, "the comment's diff is gone".to_owned()))?;
    let versions = super::diffs::versions(hub, &diff).await.map_err(fail)?;
    let v = versions.iter().find(|v| v.n == version).ok_or_else(|| bad(format!("{} has no version {version}", diff.name)))?;
    let revision = revision(hub, &diff, v).await?;
    let words = body.get("body").and_then(Value::as_str).map_or(was, str::to_owned);
    let suggestion = body.get("suggestion").and_then(Value::as_str).map(str::to_owned).or(was_suggestion);
    Ok(Edit { diff, revision, place: Place { thread: Some(thread), ..Place::default() }, words, suggestion })
}

/// `POST /api/comments/:id/statement` `{body, suggestion}`: what its author signs to edit comment
/// `id`'s words to these, and whether its repository requires it.
pub async fn edit_statement(hub: &Hub, id: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let e = edit_of(hub, id, body).await?;
    let mut out = to_sign(&signer_of(hub, caller).await?, &e.asked());
    if let Some(o) = out.as_object_mut() {
        o.insert("required".into(), json!(required(hub, &e.diff).await?));
    }
    Ok(out)
}

/// What a giver means to sign: a verdict's action, or a comment's, and its words.
#[derive(Clone, Copy, Debug, Default)]
pub struct Meant<'a> {
    /// `verdict` or `comment`.
    pub kind: &'a str,
    pub action: &'a str,
    pub words: &'a str,
    pub suggestion: Option<&'a str>,
}

/// `to_sign` (what `POST /api/diffs/:id/statement` answered) signed where the giver's key is: the
/// agent session's runner (`as_agent`, the way kiln-git-sign signs its commits; never the person's
/// key), else `kiln login`'s on this device. Its bundle, once the statement says what's `meant`.
pub fn sign(to_sign: &Value, meant: Meant<'_>, as_agent: bool) -> anyhow::Result<Value> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let field = |k: &str| to_sign.get(k).and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("kiln's statement has no {k}"));
    let payload_type = field("payload_type")?;
    let payload = b64.decode(field("payload")?)?;
    let s: statement::Statement = serde_json::from_slice(&payload)?;
    let b = verdict::read(payload_type, &s).map_err(|e| anyhow::anyhow!("kiln's statement isn't a verdict's or a comment's: {e}"))?;
    anyhow::ensure!(b.kind == meant.kind && b.action == meant.action && b.said == verdict::said(meant.words) && b.suggestion == meant.suggestion.map(verdict::said), "kiln's statement doesn't say what's given: refused, unsigned");
    let message = statement::to_sign(payload_type, &payload);
    let (signature, certificate) = if as_agent { crate::gitsign::by_the_runner(&message)? } else { crate::gitsign::with_device_key(&message)? };
    Ok(statement::bundle(payload_type, &payload, &signature, &certificate))
}

/// A signed verdict or comment kiln took: its digest, signer and envelope, and when.
#[derive(Debug, Clone)]
pub struct Taken {
    pub digest: String,
    pub signer: String,
    pub bundle: String,
    pub at: i64,
}

/// Takes `bundle`, given by `caller` with what `asked` asks: checked now against kiln's root and
/// the revocations, signed by whoever gives it, saying what's asked. Else why not.
pub async fn take(hub: &Hub, caller: &Caller, asked: &Asked<'_>, bundle: &Value) -> Result<Taken, Fail> {
    let at = crate::util::now_unix();
    let revocations = crate::identities::revocations(&hub.store).await.map_err(fail)?;
    let c = statement::verify(bundle, &hub.pki.ca_der, &revocations, at).map_err(|e| bad(format!("its signature isn't taken: {e}")))?;
    let body = verdict::read(&c.payload_type, &c.statement).map_err(|e| bad(format!("what's signed isn't a {}: {e}", asked.kind)))?;
    if !crate::identities::is_caller(hub, caller, &c.signer.identity).await {
        return Err((StatusCode::FORBIDDEN, format!("it's signed by {}, and {} gives it: a verdict or comment is signed by whoever gives it", c.custodian, caller.name)));
    }
    verdict::asks(&body, &c.custodian, asked).map_err(|e| bad(format!("its signature isn't for this: {e}")))?;
    Ok(Taken { digest: c.digest, signer: c.custodian, bundle: bundle.to_string(), at })
}

/// Keeps `t`, the signature of `kind` `id` on version `version` of diff `diff_id`, in `tx`.
pub fn keep(tx: &rusqlite::Connection, kind: &str, id: &str, diff_id: &str, version: u32, t: &Taken) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO review_signatures (kind, id, diff_id, version, digest, signer, taken_at, bundle) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (kind, id) DO UPDATE SET version = excluded.version, digest = excluded.digest, signer = excluded.signer, taken_at = excluded.taken_at, bundle = excluded.bundle",
        params![kind, id, diff_id, version, t.digest, t.signer, t.at, t.bundle],
    )
    .map(|_| ())
}

/// The refusal of an unsigned verdict or comment where `d`'s repository requires signatures,
/// saying how to sign it.
pub fn unsigned(d: &Diff, kind: &str) -> Fail {
    (
        StatusCode::PRECONDITION_REQUIRED,
        format!("{}'s repository requires signatures: a {kind} is signed by whoever gives it (`kiln review` and kiln's pages sign it; POST /api/diffs/{}/statement says what to sign)", d.name, d.name),
    )
}

/// `GET /api/diffs/:id/verdicts`: each verdict on `d`, oldest first, its signature's signer and
/// digest, and where its signature stands on the latest version: `given` there, `carried` there
/// from the version it was given on (the same verbatim patch-id), or `not`, saying why;
/// `unsigned`, with none.
pub async fn list(hub: &Hub, d: &Diff) -> Result<Value, Fail> {
    let versions = super::diffs::versions(hub, d).await.map_err(fail)?;
    let holding = holding(hub, d, &versions).await?;
    let id = d.id.clone();
    let rows: Vec<(String, String, u32, String, String, i64, Option<String>, Option<i64>, Option<String>)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare(
                "SELECT r.id, r.reviewer, r.version, r.action, r.body, r.created_at, s.signer, s.taken_at, s.digest FROM reviews r
                 LEFT JOIN review_signatures s ON s.kind = 'verdict' AND s.id = r.id WHERE r.diff_id = ?1 ORDER BY r.created_at, r.rowid",
            )?;
            let rows = q.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(fail)?;
    let latest = versions.last().map_or(0, |v| v.n);
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(review, reviewer, version, action, body, at, signer, taken_at, digest)| {
            let (standing, why) = match holding.get(&review) {
                None => ("unsigned", None),
                Some(Holds::Given) => ("given", None),
                Some(Holds::Carried { .. }) => ("carried", None),
                Some(Holds::Not(why)) => ("not", Some(why.clone())),
            };
            json!({ "id": review, "reviewer": reviewer, "version": version, "action": action, "body": body, "at": at, "signer": signer, "digest": digest, "taken_at": taken_at, "on": latest, "standing": standing, "why": why })
        })
        .collect();
    Ok(json!({ "diff": d.name, "latest": latest, "verdicts": out }))
}

/// The digests of the signed accepts on `d` that hold on its latest version, oldest first: what
/// lets it land where signatures count, as its land's statement names them (W27-R07).
pub async fn holding_accepts(hub: &Hub, d: &Diff, versions: &[Version]) -> Result<Vec<String>, Fail> {
    let holds = holding(hub, d, versions).await?;
    let id = d.id.clone();
    let rows: Vec<(String, String)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT r.id, s.digest FROM reviews r JOIN review_signatures s ON s.kind = 'verdict' AND s.id = r.id WHERE r.diff_id = ?1 AND r.action = 'accept' ORDER BY r.created_at, r.rowid")?;
            let rows = q.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(fail)?;
    Ok(rows.into_iter().filter(|(review, _)| holds.get(review).is_some_and(Holds::holds)).map(|(_, digest)| digest).collect())
}

/// Where each signed verdict on `d` stands on its latest version, by its review's id: each
/// checked again as of when kiln took it, then held to that version (its own, or a later one with
/// its verbatim patch-id).
pub async fn holding(hub: &Hub, d: &Diff, versions: &[Version]) -> Result<HashMap<String, Holds>, Fail> {
    let id = d.id.clone();
    let rows: Vec<(String, String, i64)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, bundle, taken_at FROM review_signatures WHERE kind = 'verdict' AND diff_id = ?1")?;
            let rows = q.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(fail)?;
    let Some(latest) = versions.last().filter(|_| !rows.is_empty()) else { return Ok(HashMap::new()) };
    let (_, path) = super::diffs::home(hub, d).await?;
    let revocations = crate::identities::revocations(&hub.store).await.map_err(fail)?;
    let latest_patch = verbatim(hub, &path, &latest.commit).await;
    let kept: Vec<Kept> = versions.iter().map(|v| Kept { n: v.n, commit: v.commit.clone(), patch_id: (v.n == latest.n).then(|| latest_patch.clone()).flatten() }).collect();
    let mut out = HashMap::new();
    for (review, bundle, at) in rows {
        let checked = serde_json::from_str::<Value>(&bundle).map_err(|e| e.to_string()).and_then(|b| statement::verify(&b, &hub.pki.ca_der, &revocations, at).map_err(|e| e.to_string()));
        let holds = match checked.and_then(|c| verdict::read(&c.payload_type, &c.statement)) {
            Ok(b) => verdict::holds_on(&b.revision, &kept, latest.n),
            Err(why) => Holds::Not(format!("its signature doesn't verify: {why}")),
        };
        out.insert(review, holds);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use kiln_ci::identity::chain::{self, Actor, Identity};

    /// A CSR and a signature over [`MESSAGE`] made by the browser's own code (ui/src/lib/signing.ts:
    /// a WebCrypto ECDSA P-256 key, its PKCS #10 request built by hand, its signature turned to DER).
    const BROWSER_CSR: &str = "-----BEGIN CERTIFICATE REQUEST-----\nMIHTMHoCAQAwGDEWMBQGA1UEAwwNa2lsbi1pZGVudGl0eTBZMBMGByqGSM49AgEG\nCCqGSM49AwEHA0IABClrtavmGdiNqJ3OBcu5un1sVauiNFNnWUu++lg7Q5Mlmahi\n0s0xsgNbKz3aEzQtbdbCA2DpREsKOCmVqUkleRugADAKBggqhkjOPQQDAgNJADBG\nAiEA/OdHqyMou0Bf5Pm4+XHJcPECUUprjmp3bHscMcbknegCIQDj+kLJRsU7qzbA\naugxDsHJcVFbCoCX1w3g6CfYMCv+JA==\n-----END CERTIFICATE REQUEST-----";
    const BROWSER_SIG: &str = "MEUCIAPd4wR53HYZhm04BVFTCFvJDuqNvyEkdkh3ZLIrA+fQAiEAqGiHNHCx8pm9qhy3xt5is7xzPGtUyMgsGDBfTf3adgI=";
    const MESSAGE: &[u8] = b"DSSEv1 39 application/vnd.kiln.review.verdict+json 2 {}";

    /// W27-R04: a browser's key is certified as its person from the CSR the browser makes, and what
    /// it signs verifies to kiln's root as them: kiln's own checks of the browser's request and
    /// signature, not only the browser's of itself.
    #[test]
    fn w27_r04_a_browsers_key_is_certified_and_what_it_signs_verifies() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let kim = Identity::new(pki.organization(), Actor::Person { name: "kim".into() });
        let cert = crate::pki::der_of(&pki.issue_identity(BROWSER_CSR, &kim, &[], time::Duration::hours(1))?)?;
        let sig = base64::engine::general_purpose::STANDARD.decode(BROWSER_SIG)?;
        let v = chain::verify_signed(&[&cert], &pki.ca_der, &[], crate::util::now_unix(), MESSAGE, &sig)?;
        assert_eq!(v.identity, kim);
        assert!(chain::verify_signed(&[&cert], &pki.ca_der, &[], crate::util::now_unix(), b"another message", &sig).is_err(), "over another message");
        Ok(())
    }
}
