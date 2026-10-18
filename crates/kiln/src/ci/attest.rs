//! Attestations (docs/ci.md W18-R01, R02, R03): provenance and SBOMs of what a leg built.
//!
//! An attest step (`kiln/attest@v1`, or a job's `attest:`) has the runner report its subjects'
//! digests over its own channel to the server, never through anything a job's steps can reach,
//! and upload the workspace's SBOM when asked (`POST /_kiln/attestations/sbom?nonce=`, under a
//! random name only the report knows). The server builds the SLSA v1.0 statement from what it
//! knows of the run itself (the workflow, ref and inputs, the source commit, the actions' pins,
//! the images, the runner, executor and isolation), so a job can't put words in it, and signs it
//! and the SBOM's as DSSE envelopes with its own P-256 key, which never leaves the server. They
//! are kept as Sigstore bundles:
//! - the runner fetches its own (`GET /_kiln/attestations?nonce=`);
//! - the run lists them (`GET /api/ci/runs/:id/attestations`);
//! - GitHub's attestations API on the facade finds them by subject digest, and keeps those a job
//!   uploads itself (`actions/attest-build-provenance` with a Fulcio of its own), as the job's;
//! - verifiers take the public key from `GET /_kiln/attestation-key.pem`
//!   (`cosign verify-blob-attestation --key`).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;
use http::StatusCode;
use kiln_ci::attest::{self as a, Subject};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The most an uploaded SBOM may be.
const MAX_SBOM: usize = 64 << 20;
/// GitHub's most subjects in one statement.
pub const MAX_SUBJECTS: usize = 1024;

/// The server's attestation key: its key id is the SHA-256 of its public key's DER.
pub struct Signer {
    pair: EcdsaKeyPair,
    spki: Vec<u8>,
    pub keyid: String,
}

impl Signer {
    /// Loads the key under `dir`, or makes it (once, readable by this user alone).
    fn open(dir: &Path) -> anyhow::Result<Signer> {
        let path = dir.join("attest-signing.p8");
        let pkcs8 = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                std::fs::create_dir_all(dir)?;
                let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new()).map_err(|_| anyhow::anyhow!("no attestation key could be made"))?;
                crate::runner::auth::write_private(&path, doc.as_ref())?;
                doc.as_ref().to_vec()
            }
        };
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8).map_err(|_| anyhow::anyhow!("{} isn't a P-256 key", path.display()))?;
        let spki = pair.public_key().as_der().map_err(|_| anyhow::anyhow!("the attestation key's public half"))?.as_ref().to_vec();
        let keyid = crate::util::sha256_hex(&spki);
        Ok(Signer { pair, spki, keyid })
    }

    pub fn sign(&self, msg: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(self.pair.sign(&SystemRandom::new(), msg).map_err(|_| anyhow::anyhow!("signing failed"))?.as_ref().to_vec())
    }

    /// The public key, as PEM (`-----BEGIN PUBLIC KEY-----`).
    pub fn public_pem(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.spki);
        let lines: Vec<&str> = b64.as_bytes().chunks(64).filter_map(|c| std::str::from_utf8(c).ok()).collect();
        format!("-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n", lines.join("\n"))
    }

    /// A Sigstore bundle of `statement`, signed. The bundle's hint names the key; the signature
    /// names none, since DSSE verifiers (cosign's) match ids of their own making.
    pub fn bundle(&self, statement: &Value) -> anyhow::Result<Value> {
        let payload = serde_json::to_vec(statement)?;
        let sig = self.sign(&a::pae(a::PAYLOAD_TYPE, &payload))?;
        Ok(a::bundle(a::envelope(a::PAYLOAD_TYPE, &payload, "", &sig), &self.keyid))
    }
}

/// The server's signer.
pub fn signer(hub: &Hub) -> anyhow::Result<&'static Signer> {
    static S: OnceLock<Signer> = OnceLock::new();
    if let Some(s) = S.get() {
        return Ok(s);
    }
    let s = Signer::open(&hub.cfg.server.data_dir.join("keys"))?;
    Ok(S.get_or_init(|| s))
}

fn rfc3339(t: i64) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp(t).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
}

/// How a leg on `executor` was kept from its runner.
fn isolation(executor: &str) -> &'static str {
    match executor {
        "docker" | "k8s" => "container",
        "firecracker" | "shards" => "microvm",
        _ => "host",
    }
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// What the server knows of the leg's build: its run, and the provenance's facts.
async fn facts(hub: &Hub, leg: &str) -> anyhow::Result<Option<(String, a::Facts)>> {
    let leg = leg.to_owned();
    type Row = (String, Option<String>, Option<String>, Option<i64>, String, String, String, i64, String, Option<String>, Option<String>);
    let row: Option<Row> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT j.run_id, j.agent, j.executor, j.started_at, p.name, COALESCE(r.workflow_id, ''), r.sha, COALESCE(r.attempt, 1), r.event, i.input,
                        (SELECT facts FROM ci_attempts t WHERE t.leg_id = j.id ORDER BY t.number DESC LIMIT 1)
                 FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id LEFT JOIN ci_plans i ON i.run_id = r.id WHERE j.id = ?1",
                [&leg],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?, x.get(6)?, x.get(7)?, x.get(8)?, x.get(9)?, x.get(10)?)),
            )
            .optional()?)
        })
        .await?;
    let Some((run, agent, executor, started, repo, workflow, sha, attempt, event, input, leg_facts)) = row else { return Ok(None) };
    let input: Value = input.and_then(|i| serde_json::from_str(&i).ok()).unwrap_or_default();
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    // The actions as pinned when the run started: `uses` → `{ repo, sha, … }`.
    let actions = input
        .get("actions")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(uses, x)| Some((uses.clone(), x.get("repo")?.as_str()?.to_owned(), x.get("sha")?.as_str()?.to_owned()))).collect())
        .unwrap_or_default();
    // The images its containers ran, as the leg reported them (W19-R03's facts).
    let images = leg_facts
        .and_then(|f| serde_json::from_str::<Value>(&f).ok())
        .and_then(|f| f.get("images").and_then(Value::as_array).cloned())
        .map(|a| a.iter().filter_map(|i| Some((i.get("image")?.as_str()?.to_owned(), i.get("id")?.as_str()?.to_owned()))).collect())
        .unwrap_or_default();
    let executor = executor.unwrap_or_else(|| "host".into());
    Ok(Some((
        run.clone(),
        a::Facts {
            builder_id: format!("{base}/ci@v1"),
            repository: format!("{base}/{repo}"),
            workflow_path: workflow,
            git_ref: input.pointer("/github/ref").and_then(Value::as_str).unwrap_or_default().to_owned(),
            sha,
            inputs: input.get("inputs").cloned().unwrap_or(Value::Null),
            event,
            actions,
            images,
            run_url: format!("{base}/runs/{run}"),
            attempt: u32::try_from(attempt).unwrap_or(1),
            runner: agent.unwrap_or_default(),
            isolation: isolation(&executor).to_owned(),
            executor,
            started: started.and_then(rfc3339),
            finished: rfc3339(crate::util::now_unix()),
        },
    )))
}

/// `POST /_kiln/attestations/sbom?nonce=`: the SBOM a runner made for its attest step, kept until
/// the step's report (over the runner's own channel) has it signed.
pub async fn upload(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Err((StatusCode::UNAUTHORIZED, "a leg's runtime token is needed".into())) };
    let nonce = srv::query(&req).get("nonce").cloned().unwrap_or_default();
    if nonce.len() < 32 {
        return Err((StatusCode::BAD_REQUEST, "name the upload (`nonce`)".into()));
    }
    let doc: Value = srv::body_json(req.into_body(), MAX_SBOM).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if a::sbom_type(&doc).is_none() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "an SBOM is an SPDX or CycloneDX document".into()));
    }
    let (leg, text) = (claims.sub, doc.to_string());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            // Those whose report never came.
            c.execute("DELETE FROM ci_attestation_uploads WHERE created_at < ?1", [now - 86_400])?;
            c.execute("INSERT OR REPLACE INTO ci_attestation_uploads (leg_id, nonce, doc, created_at) VALUES (?1, ?2, ?3, ?4)", params![leg, nonce, text, now])?;
            Ok(())
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::CREATED, &json!({})))
}

/// Keeps `made` (predicate type, bundle) about `subjects`; their ids.
#[allow(clippy::too_many_arguments)]
async fn keep(hub: &Hub, run: String, leg: String, idx: u32, nonce: String, origin: &'static str, subjects: &[Subject], made: Vec<(String, Value)>) -> anyhow::Result<Vec<i64>> {
    let subjects_json = serde_json::to_string(subjects)?;
    let digests: Vec<String> = subjects.iter().flat_map(|s| s.digest.iter().map(|(alg, hex)| format!("{}:{}", alg.to_ascii_lowercase(), hex.to_ascii_lowercase()))).collect();
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            let mut ids = Vec::new();
            for (kind, bundle) in made {
                tx.execute(
                    "INSERT INTO ci_attestations (run_id, leg_id, idx, nonce, origin, predicate_type, subjects, bundle, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![run, leg, idx, nonce, origin, kind, subjects_json, bundle.to_string(), now],
                )?;
                let id = tx.last_insert_rowid();
                for d in &digests {
                    tx.execute("INSERT INTO ci_attestation_subjects (attestation_id, digest) VALUES (?1, ?2)", params![id, d])?;
                }
                ids.push(id);
            }
            tx.commit()?;
            Ok(ids)
        })
        .await
}

/// A leg's attest step reported (W18-R01): its statements made from the server's own records,
/// signed and kept.
#[allow(clippy::too_many_arguments)]
pub async fn reported(hub: &Hub, leg: &str, idx: u32, nonce: &str, mut subjects: Vec<Subject>, provenance: bool, sbom: bool) -> anyhow::Result<()> {
    subjects.truncate(MAX_SUBJECTS);
    // The SBOM the runner uploaded for this report, once.
    let doc = if sbom {
        let (l, n) = (leg.to_owned(), nonce.to_owned());
        let text: Option<String> = hub
            .store
            .call(move |c| {
                let t = c.query_row("SELECT doc FROM ci_attestation_uploads WHERE leg_id = ?1 AND nonce = ?2", params![l, n], |r| r.get(0)).optional()?;
                c.execute("DELETE FROM ci_attestation_uploads WHERE leg_id = ?1 AND nonce = ?2", params![l, n])?;
                Ok(t)
            })
            .await?;
        let doc: Value = serde_json::from_str(&text.ok_or_else(|| anyhow::anyhow!("no SBOM was uploaded for the report"))?)?;
        // Only an SBOM is signed as one: never a statement or anything else a workspace held.
        Some((a::sbom_type(&doc).ok_or_else(|| anyhow::anyhow!("the upload is neither SPDX nor CycloneDX"))?, doc))
    } else {
        None
    };
    let Some((run, f)) = facts(hub, leg).await? else { return Ok(()) };
    // An SBOM with nothing named is the source's.
    if subjects.is_empty() {
        subjects.push(Subject { name: format!("git+{}@{}", f.repository, f.git_ref), digest: [("gitCommit".to_owned(), f.sha.clone())].into() });
    }
    let s = signer(hub)?;
    let mut made: Vec<(String, Value)> = Vec::new();
    if provenance {
        made.push((a::PROVENANCE.into(), s.bundle(&a::statement(&subjects, a::PROVENANCE, a::provenance(&f)))?));
    }
    if let Some((kind, doc)) = doc {
        made.push((kind.into(), s.bundle(&a::statement(&subjects, kind, doc))?));
    }
    keep(hub, run, leg.to_owned(), idx, nonce.to_owned(), "kiln", &subjects, made).await?;
    Ok(())
}

type Row = (i64, String, i64, String, String, String, String, i64);

const COLUMNS: &str = "a.id, a.leg_id, a.idx, a.origin, a.predicate_type, a.subjects, a.bundle, a.created_at";

async fn rows(hub: &Hub, sql: String, key: String, also: String) -> anyhow::Result<Vec<Row>> {
    hub.store
        .call(move |c| {
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![key, also], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?, x.get(6)?, x.get(7)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

fn row_json((id, leg, idx, origin, kind, subjects, bundle, at): Row) -> Value {
    json!({
        "id": id, "leg": leg, "step": idx, "origin": origin, "predicate_type": kind, "created_at": at,
        "subjects": serde_json::from_str::<Value>(&subjects).unwrap_or_default(), "bundle": serde_json::from_str::<Value>(&bundle).unwrap_or_default(),
    })
}

/// `GET /_kiln/attestations?nonce=`: the bundles of one of a leg's reports, for its runner.
pub async fn for_leg(hub: &Arc<Hub>, req: &Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Err((StatusCode::UNAUTHORIZED, "a leg's runtime token is needed".into())) };
    let nonce = srv::query(req).get("nonce").cloned().unwrap_or_default();
    let list = rows(hub, format!("SELECT {COLUMNS} FROM ci_attestations a WHERE a.leg_id = ?1 AND a.nonce = ?2 ORDER BY a.id"), claims.sub.clone(), nonce).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "attestations": list.into_iter().map(row_json).collect::<Vec<_>>() })))
}

/// `GET /api/ci/runs/:id/attestations`.
pub async fn for_run(hub: &Hub, run: &str) -> Result<Response, Fail> {
    let list = rows(hub, format!("SELECT {COLUMNS} FROM ci_attestations a WHERE a.run_id = ?1 AND ?2 = '' ORDER BY a.id"), run.to_owned(), String::new()).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "attestations": list.into_iter().map(row_json).collect::<Vec<_>>() })))
}

/// `GET /_kiln/attestation-key.pem`: the public key kiln's bundles are signed with.
pub fn key_pem(hub: &Hub) -> Result<Response, Fail> {
    let s = signer(hub).map_err(internal)?;
    Ok(srv::text(StatusCode::OK, &s.public_pem()))
}

/// `GET /api/v3/repos/:o/:r/attestations/:digest` (GitHub's shape): the repository's bundles of a
/// subject digest, newest first; `predicate_type` (`provenance`, `sbom` or a URI) narrows them.
pub async fn facade_list(hub: &Hub, repo_id: &str, digest: &str, predicate_type: Option<&str>) -> Result<Response, Fail> {
    let want: Vec<&str> = match predicate_type {
        None | Some("") => Vec::new(),
        Some("provenance") => vec![a::PROVENANCE],
        Some("sbom") => vec![a::SPDX, a::CYCLONEDX],
        Some(other) => vec![other],
    };
    let sql = format!("SELECT {COLUMNS} FROM ci_attestations a JOIN ci_attestation_subjects s ON s.attestation_id = a.id JOIN runs r ON r.id = a.run_id WHERE s.digest = ?1 AND r.repo_id = ?2 ORDER BY a.id DESC LIMIT 100");
    let list = rows(hub, sql, digest.to_ascii_lowercase(), repo_id.to_owned()).await.map_err(internal)?;
    let out: Vec<Value> = list
        .into_iter()
        .filter(|r| want.is_empty() || want.contains(&r.4.as_str()))
        .map(|r| json!({ "id": r.0, "bundle": serde_json::from_str::<Value>(&r.6).unwrap_or_default(), "repository_id": 0, "bundle_url": null }))
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "attestations": out })))
}

/// `POST /api/v3/repos/:o/:r/attestations` with `{ bundle }`: a job's own bundle, kept as the
/// job's (not kiln's: kiln signed nothing of it) under its subjects' digests.
pub async fn facade_create(hub: &Hub, claims: &token::Job, body: &Value) -> Result<Response, Fail> {
    let bundle = body.get("bundle").cloned().unwrap_or(Value::Null);
    let Some(statement) = a::statement_of(&bundle) else { return Err((StatusCode::UNPROCESSABLE_ENTITY, "`bundle` isn't a Sigstore bundle of an in-toto statement".into())) };
    let subjects: Vec<Subject> = statement.get("subject").and_then(|s| serde_json::from_value(s.clone()).ok()).unwrap_or_default();
    let kind = statement.get("predicateType").and_then(Value::as_str).unwrap_or_default().to_owned();
    if subjects.is_empty() || subjects.len() > MAX_SUBJECTS || kind.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "the statement names no subject or predicate type".into()));
    }
    let ids = keep(hub, claims.run.clone(), claims.leg.clone(), 0, String::new(), "job", &subjects, vec![(kind, bundle)]).await.map_err(internal)?;
    Ok(srv::json(StatusCode::CREATED, &json!({ "id": ids.first() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundle verifies against the key's public half, and a changed statement doesn't.
    #[test]
    fn w18_r02_signed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let s = Signer::open(dir.path())?;
        assert_eq!(Signer::open(dir.path())?.keyid, s.keyid, "made once, then kept");
        let st = a::statement(&[Subject { name: "app".into(), digest: [("sha256".to_owned(), "beef".to_owned())].into() }], a::PROVENANCE, json!({ "x": 1 }));
        let b = s.bundle(&st)?;
        let (signed, sig) = a::signed_of(&b).ok_or_else(|| anyhow::anyhow!("no signature"))?;
        let public = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1, s.pair.public_key().as_ref());
        assert!(public.verify(&signed, &sig).is_ok());
        assert!(public.verify(&a::pae(a::PAYLOAD_TYPE, b"{\"forged\":true}"), &sig).is_err());
        assert_eq!(a::statement_of(&b), Some(st));
        assert!(s.public_pem().starts_with("-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE"), "a P-256 SubjectPublicKeyInfo");
        Ok(())
    }
}
