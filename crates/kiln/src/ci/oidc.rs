//! kiln as an OpenID Connect issuer for jobs (docs/ci.md W10-R06 to W10-R10).
//!
//! - **Keys.** RS256, each a 2048-bit RSA key. The private half is sealed by the master key, the
//!   public half is published as a JWK. Rotation (when to make, sign with, publish and retire a
//!   key) is `kiln_ci::oidc::rotation`'s, applied whenever a key is needed.
//! - **Endpoints.** `<issuer>/.well-known/openid-configuration` and `<issuer>/jwks`, open: they
//!   are what providers fetch. Also `/_kiln/oidc/token?api-version=1&audience=…`, the toolkit's
//!   `core.getIDToken()`, for a leg's `ACTIONS_ID_TOKEN_REQUEST_TOKEN`. That one answers
//!   `{"value": "<jwt>"}`, GitHub-exact, and only for legs granted `id-token: write`.
//! - **Publishing** (W10-R12). Where providers can't reach the server, the two documents go to a
//!   static host that serves them at the issuer URL: `[ci.oidc] publish_dir`, rewritten by the
//!   hourly upkeep and at each rotation, or `kiln oidc publish --to <dir>` by hand.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use base64::Engine as _;
use http::StatusCode;
use kiln_ci::oidc::{self as rules, KeyTimes};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Fail, secrets, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The issuer URL.
pub fn issuer(hub: &Hub) -> String {
    hub.cfg.ci.oidc.issuer.clone().unwrap_or_else(|| format!("{}/oidc", hub.cfg.server.public_url.trim_end_matches('/')))
}

/// The path the issuer's documents are served under on this server (`/oidc` by default).
fn issuer_path(hub: &Hub) -> String {
    let i = issuer(hub);
    let after_scheme = i.split_once("://").map_or(i.as_str(), |(_, r)| r);
    after_scheme.find('/').and_then(|p| after_scheme.get(p..)).unwrap_or("/oidc").trim_end_matches('/').to_owned()
}

/// Whether the issuer's documents are served: while the CI engine runs (its jobs' tokens), or
/// while an account federates kiln's identity (an `aws`, `gcp` or `azure` integration, whose CD
/// syncs mint tokens). Only reads: fetching the keys makes the first one, so with neither,
/// nothing is served and no key is made.
pub async fn active(hub: &Hub) -> bool {
    hub.ci.enabled() || crate::ci::integrations::federates(hub).await
}

/// Whether `seg` is one of the issuer's documents here.
pub fn is_issuer(hub: &Hub, seg: &[&str]) -> Option<&'static str> {
    let path = format!("/{}", seg.join("/"));
    let base = issuer_path(hub);
    if path == format!("{base}/.well-known/openid-configuration") {
        Some("discovery")
    } else if path == format!("{base}/jwks") || path == format!("{base}/.well-known/jwks") {
        Some("jwks")
    } else {
        None
    }
}

struct Stored {
    times: KeyTimes,
    n: String,
    e: String,
}

/// The public modulus and exponent of a DER `RSAPublicKey` (PKCS#1: a sequence of two integers).
fn rsa_parts(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    fn len(b: &[u8], i: &mut usize) -> Option<usize> {
        let first = *b.get(*i)?;
        *i += 1;
        if first < 0x80 {
            return Some(usize::from(first));
        }
        let mut n = 0usize;
        for _ in 0..usize::from(first & 0x7f) {
            n = (n << 8) | usize::from(*b.get(*i)?);
            *i += 1;
        }
        Some(n)
    }
    fn int(b: &[u8], i: &mut usize) -> Option<Vec<u8>> {
        if *b.get(*i)? != 0x02 {
            return None;
        }
        *i += 1;
        let l = len(b, i)?;
        let v = b.get(*i..*i + l)?;
        *i += l;
        let start = v.iter().position(|x| *x != 0).unwrap_or(v.len().saturating_sub(1));
        Some(v.get(start..).unwrap_or_default().to_vec())
    }
    let mut i = 0;
    if *der.first()? != 0x30 {
        return None;
    }
    i += 1;
    len(der, &mut i)?;
    let n = int(der, &mut i)?;
    let e = int(der, &mut i)?;
    Some((n, e))
}

fn aad(kid: &str) -> Vec<u8> {
    format!("kiln oidc key\0{kid}").into_bytes()
}

/// A new key that starts signing at `sign_from`.
async fn make_key(hub: &Hub, sign_from: i64) -> anyhow::Result<()> {
    let kp = crate::rt::blocking(|| KeyPair::generate(KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("RSA key generation failed"))).await??;
    let pkcs8 = kp.as_der().map_err(|_| anyhow::anyhow!("the key can't be encoded"))?.as_ref().to_vec();
    let (n, e) = rsa_parts(kp.public_key().as_ref()).ok_or_else(|| anyhow::anyhow!("the public key can't be read"))?;
    let kid = crate::util::new_id();
    let (sealed, nonce) = secrets::seal_master(hub, &pkcs8, &aad(&kid))?;
    let (n, e) = (B64.encode(n), B64.encode(e));
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO oidc_keys (kid, alg, sealed, nonce, n, e, created_at, sign_from) VALUES (?1, 'RS256', ?2, ?3, ?4, ?5, ?6, ?7)",
                params![kid, sealed, nonce, n, e, crate::util::now_unix(), sign_from],
            )?;
            Ok(())
        })
        .await?;
    tracing::info!(sign_from, "OIDC: a new signing key");
    Ok(())
}

async fn stored(hub: &Hub) -> anyhow::Result<Vec<Stored>> {
    hub.store
        .call(|c| {
            let mut q = c.prepare("SELECT kid, created_at, sign_from, n, e FROM oidc_keys")?;
            let rows = q.query_map([], |r| Ok(Stored { times: KeyTimes { kid: r.get(0)?, created_at: r.get(1)?, sign_from: r.get(2)? }, n: r.get(3)?, e: r.get(4)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// One lock for rotation, so two requests don't both make a key.
static ROTATE: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// Applies rotation now and returns the keys and the plan.
async fn rotate(hub: &Hub) -> anyhow::Result<(Vec<Stored>, rules::Rotation)> {
    let _g = ROTATE.lock().await;
    let now = crate::util::now_unix();
    let mut keys = stored(hub).await?;
    let mut plan = rules::rotation(&keys.iter().map(|k| k.times.clone()).collect::<Vec<_>>(), now);
    if let Some(at) = plan.create {
        make_key(hub, at).await?;
        keys = stored(hub).await?;
        plan = rules::rotation(&keys.iter().map(|k| k.times.clone()).collect::<Vec<_>>(), now);
    }
    if !plan.retire.is_empty() {
        let gone = plan.retire.clone();
        hub.store
            .call(move |c| {
                for kid in &gone {
                    c.execute("DELETE FROM oidc_keys WHERE kid = ?1", [kid])?;
                }
                Ok(())
            })
            .await?;
    }
    Ok((keys, plan))
}

/// A public key as a JWK.
fn jwk(kid: &str, n: &str, e: &str) -> Value {
    json!({ "kty": "RSA", "alg": "RS256", "use": "sig", "kid": kid, "n": n, "e": e })
}

/// The published key set (JWKS).
pub async fn jwks(hub: &Hub) -> anyhow::Result<Value> {
    let (keys, plan) = rotate(hub).await?;
    let list: Vec<Value> = keys.iter().filter(|k| plan.publish.contains(&k.times.kid)).map(|k| jwk(&k.times.kid, &k.n, &k.e)).collect();
    Ok(json!({ "keys": list }))
}

/// The files a static host serves at the issuer URL, by path under it: the discovery document,
/// and the key set where the discovery document says it is.
pub fn published_files(discovery: &Value, jwks: &Value) -> anyhow::Result<[(&'static str, String); 2]> {
    Ok([(".well-known/openid-configuration", serde_json::to_string_pretty(discovery)?), ("jwks", serde_json::to_string_pretty(jwks)?)])
}

/// Writes them under `dir`, each by rename so a host never serves half a file. Whether anything
/// changed.
pub fn write_published(dir: &Path, discovery: &Value, jwks: &Value) -> anyhow::Result<bool> {
    let mut changed = false;
    for (rel, body) in published_files(discovery, jwks)? {
        let path = dir.join(rel);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("kiln-tmp");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &path)?;
        changed = true;
    }
    Ok(changed)
}

/// Rewrites `[ci.oidc] publish_dir` when it is set and the keys changed; where it is.
pub async fn publish(hub: &Hub) -> anyhow::Result<Option<PathBuf>> {
    let Some(dir) = hub.cfg.ci.oidc.publish_dir.clone() else { return Ok(None) };
    let i = issuer(hub);
    let (discovery, set) = (rules::discovery(&i, &format!("{i}/jwks")), jwks(hub).await?);
    let to = dir.clone();
    if crate::rt::blocking(move || write_published(&to, &discovery, &set)).await?? {
        tracing::info!(dir = %dir.display(), "OIDC: published the issuer's documents");
    }
    Ok(Some(dir))
}

/// A token for a workload of the server's own (W10-R11): a CD sync, say. The claims come from
/// `kiln_ci::oidc::workload_claims`; this signs them with the issuer's current key.
pub async fn mint(hub: &Hub, claims: &serde_json::Map<String, Value>) -> anyhow::Result<String> {
    sign(hub, claims).await
}

/// Signs `claims` with the current key: a JWT.
async fn sign(hub: &Hub, claims: &serde_json::Map<String, Value>) -> anyhow::Result<String> {
    let (_, plan) = rotate(hub).await?;
    let kid = plan.sign.ok_or_else(|| anyhow::anyhow!("no OIDC key signs now"))?;
    let k = kid.clone();
    let (sealed, nonce): (Vec<u8>, Vec<u8>) = hub.store.call(move |c| Ok(c.query_row("SELECT sealed, nonce FROM oidc_keys WHERE kid = ?1", [&k], |r| Ok((r.get(0)?, r.get(1)?)))?)).await?;
    let pkcs8 = secrets::open_master(hub, &sealed, &nonce, &aad(&kid))?;
    let kp = KeyPair::from_pkcs8(&pkcs8).map_err(|_| anyhow::anyhow!("the OIDC key can't be read"))?;
    sign_with(&kp, &kid, claims)
}

fn sign_with(kp: &KeyPair, kid: &str, claims: &serde_json::Map<String, Value>) -> anyhow::Result<String> {
    let head = B64.encode(serde_json::to_vec(&json!({ "alg": "RS256", "typ": "JWT", "kid": kid }))?);
    let body = B64.encode(serde_json::to_vec(claims)?);
    let signing = format!("{head}.{body}");
    let mut sig = vec![0u8; kp.public_modulus_len()];
    kp.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing.as_bytes(), &mut sig).map_err(|_| anyhow::anyhow!("signing failed"))?;
    Ok(format!("{signing}.{}", B64.encode(sig)))
}

/// The open documents: discovery and keys.
pub async fn document(hub: &Arc<Hub>, which: &str) -> Result<Response, Fail> {
    let i = issuer(hub);
    match which {
        "discovery" => Ok(srv::json(StatusCode::OK, &rules::discovery(&i, &format!("{i}/jwks")))),
        _ => {
            let set = jwks(hub).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(srv::json(StatusCode::OK, &set))
        }
    }
}

/// `/api/ci/oidc…`, behind the console's auth (W10-R12):
/// - `GET /api/ci/oidc`: the issuer and its keys;
/// - `GET /api/ci/oidc/jwks-export`: the discovery document and key set to publish elsewhere;
/// - `POST /api/ci/oidc/rotate { now }`: a new key, signing in 7 days, or at once;
/// - `POST /api/ci/oidc/publish`: rewrite `[ci.oidc] publish_dir` now.
pub async fn admin(hub: &Hub, method: &http::Method, rest: &[&str], body: Value) -> Result<Response, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let i = issuer(hub);
    match (method, rest) {
        (&http::Method::GET, []) => {
            let (keys, plan) = rotate(hub).await.map_err(fail)?;
            let list: Vec<Value> = keys
                .iter()
                .map(|k| json!({ "kid": k.times.kid, "created_at": k.times.created_at, "sign_from": k.times.sign_from, "signing": plan.sign.as_deref() == Some(k.times.kid.as_str()), "published": plan.publish.contains(&k.times.kid) }))
                .collect();
            Ok(srv::json(StatusCode::OK, &json!({ "issuer": i, "discovery": format!("{i}/.well-known/openid-configuration"), "jwks_uri": format!("{i}/jwks"), "serving": hub.ci.enabled(), "keys": list, "subject_claims": hub.cfg.ci.oidc.subject_claims })))
        }
        (&http::Method::GET, ["jwks-export"]) => {
            let set = jwks(hub).await.map_err(fail)?;
            Ok(srv::json(StatusCode::OK, &json!({ "issuer": i, "discovery": rules::discovery(&i, &format!("{i}/jwks")), "jwks": set })))
        }
        (&http::Method::POST, ["rotate"]) => {
            let now = body.get("now").and_then(Value::as_bool).unwrap_or(false);
            let at = crate::util::now_unix() + if now { 0 } else { rules::LEAD_SECS };
            // Released before publishing, which rotates (and locks) too.
            let g = ROTATE.lock().await;
            make_key(hub, at).await.map_err(fail)?;
            drop(g);
            hub.store.audit("admin".into(), "ci.oidc.rotate".into(), format!("a new signing key, signing from {at}")).await;
            let published = publish(hub).await.map_err(fail)?;
            Ok(srv::json(StatusCode::OK, &json!({ "sign_from": at, "published": published })))
        }
        (&http::Method::POST, ["publish"]) => match publish(hub).await.map_err(fail)? {
            Some(dir) => Ok(srv::json(StatusCode::OK, &json!({ "published": dir }))),
            None => Err((StatusCode::BAD_REQUEST, "no [ci.oidc] publish_dir on this server; `kiln oidc publish --to <dir>` writes the documents wherever you run it".into())),
        },
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

/// What the token endpoint needs to know about a leg.
struct LegFacts {
    spec: Value,
    agent: Option<String>,
    trusted: bool,
    github: Value,
}

async fn leg_facts(hub: &Hub, leg: &str) -> anyhow::Result<Option<LegFacts>> {
    let leg = leg.to_owned();
    hub.store
        .call(move |c| {
            let row: Option<(Option<String>, Option<String>, String, String)> = c
                .query_row(
                    "SELECT j.leg, j.agent, r.trust, p.input FROM jobs j JOIN runs r ON r.id = j.run_id JOIN ci_plans p ON p.run_id = r.id WHERE j.id = ?1",
                    [&leg],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            Ok(row.map(|(spec, agent, trust, input)| {
                let input: Value = serde_json::from_str(&input).unwrap_or_default();
                LegFacts { spec: spec.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(), agent, trusted: trust == "trusted", github: input.get("github").cloned().unwrap_or_default() }
            }))
        })
        .await
}

/// `GET /_kiln/oidc/token?api-version=1&audience=…` (W10-R07).
pub async fn token(hub: &Arc<Hub>, req: &Request) -> Result<Response, Fail> {
    let deny = |code: StatusCode, m: &str| srv::json(code, &json!({ "message": m }));
    let Some(claims) = token::of_request(hub, req.headers()) else { return Ok(deny(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_ID_TOKEN_REQUEST_TOKEN is needed")) };
    let Some(f) = leg_facts(hub, &claims.sub).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? else {
        return Ok(deny(StatusCode::NOT_FOUND, "the leg is gone"));
    };
    // GitHub-exact: only with `permissions: id-token: write`.
    let level = f.spec.pointer("/permissions/id-token").and_then(Value::as_str).unwrap_or("none");
    if level != "write" {
        return Ok(deny(StatusCode::FORBIDDEN, "this job lacks `permissions: id-token: write`"));
    }
    if !f.trusted && !hub.cfg.ci.oidc.untrusted {
        return Ok(deny(StatusCode::FORBIDDEN, "untrusted runs get no ID tokens on this server ([ci.oidc] untrusted)"));
    }
    let q = srv::query(req);
    let iss = issuer(hub);
    let audience = q.get("audience").filter(|a| !a.is_empty()).cloned().or_else(|| hub.cfg.ci.oidc.default_audience.clone()).unwrap_or_else(|| iss.clone());
    let environment = f.spec.pointer("/environment/name").and_then(Value::as_str).map(str::to_owned);
    let executor = f.spec.pointer("/executor").and_then(Value::as_str).unwrap_or("host").to_owned();
    let keys = hub.cfg.ci.oidc.subject_claims.clone();
    let facts = rules::Facts { github: &f.github, environment: environment.as_deref(), job_workflow_ref: None, job_workflow_sha: None, trusted: f.trusted, executor: &executor, runner: f.agent.as_deref().unwrap_or_default(), pool: None, job: &claims.sub };
    let c = rules::claims(&facts, &iss, &audience, &crate::util::new_id(), crate::util::now_unix(), rules::DEFAULT_TTL_SECS, (!keys.is_empty()).then_some(keys.as_slice()));
    let jwt = sign(hub, &c).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &json!({ "count": jwt.len(), "value": jwt })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w10_r06_public_keys_read_from_der_and_signatures_verify() -> anyhow::Result<()> {
        let kp = KeyPair::generate(KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
        let (n, e) = rsa_parts(kp.public_key().as_ref()).ok_or_else(|| anyhow::anyhow!("parts"))?;
        assert_eq!(n.len(), 256, "a 2048-bit modulus, no leading zero");
        assert_eq!(e, vec![1, 0, 1], "65537");
        // A signature made with the key verifies with (n, e), as a provider checks it.
        let mut sig = vec![0u8; kp.public_modulus_len()];
        kp.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), b"head.body", &mut sig).map_err(|_| anyhow::anyhow!("sign"))?;
        let public = aws_lc_rs::signature::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() };
        assert!(public.verify(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, b"head.body", &sig).is_ok());
        assert!(public.verify(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, b"head.other", &sig).is_err());
        Ok(())
    }

    /// A provider's check, reading only what a static host at `issuer` would serve from `dir`:
    /// discovery, then the keys where it points, then the token's signature and issuer.
    fn verify_published(dir: &Path, issuer: &str, jwt: &str) -> anyhow::Result<serde_json::Map<String, Value>> {
        let fetch = |url: &str| -> anyhow::Result<Value> {
            let rel = url.strip_prefix(issuer).and_then(|r| r.strip_prefix('/')).ok_or_else(|| anyhow::anyhow!("{url} is not under the issuer"))?;
            Ok(serde_json::from_str(&std::fs::read_to_string(dir.join(rel))?)?)
        };
        let discovery = fetch(&format!("{issuer}/.well-known/openid-configuration"))?;
        anyhow::ensure!(discovery.get("issuer").and_then(Value::as_str) == Some(issuer), "discovery names another issuer");
        let set = fetch(discovery.get("jwks_uri").and_then(Value::as_str).unwrap_or_default())?;
        let mut parts = jwt.split('.');
        let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { anyhow::bail!("not a JWT") };
        let header: Value = serde_json::from_slice(&B64.decode(head)?)?;
        let kid = header.get("kid").and_then(Value::as_str).unwrap_or_default();
        let keys = set.get("keys").and_then(Value::as_array).cloned().unwrap_or_default();
        let key = keys.iter().find(|k| k.get("kid").and_then(Value::as_str) == Some(kid)).ok_or_else(|| anyhow::anyhow!("no published key {kid}"))?;
        let part = |n: &str| B64.decode(key.get(n).and_then(Value::as_str).unwrap_or_default());
        let (n, e) = (part("n")?, part("e")?);
        let public = aws_lc_rs::signature::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() };
        public.verify(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, format!("{head}.{body}").as_bytes(), &B64.decode(sig)?).map_err(|_| anyhow::anyhow!("bad signature"))?;
        let claims: serde_json::Map<String, Value> = serde_json::from_slice(&B64.decode(body)?)?;
        anyhow::ensure!(claims.get("iss").and_then(Value::as_str) == Some(issuer), "the token names another issuer");
        Ok(claims)
    }

    #[test]
    fn w10_ac07_published_documents_verify_tokens() -> anyhow::Result<()> {
        let issuer = "https://oidc.example.com/kiln";
        let key = |kid: &str| -> anyhow::Result<(KeyPair, Value)> {
            let kp = KeyPair::generate(KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
            let (n, e) = rsa_parts(kp.public_key().as_ref()).ok_or_else(|| anyhow::anyhow!("parts"))?;
            Ok((kp, jwk(kid, &B64.encode(n), &B64.encode(e))))
        };
        let ((current, a), (next, b), (stray, _)) = (key("a")?, key("b")?, key("c")?);
        let dir = tempfile::tempdir()?;
        let discovery = rules::discovery(issuer, &format!("{issuer}/jwks"));
        assert!(write_published(dir.path(), &discovery, &json!({ "keys": [a, b] }))?);
        assert!(!write_published(dir.path(), &discovery, &json!({ "keys": [a, b] }))?, "unchanged documents aren't rewritten");
        let w = rules::Workload { repository: "acme/app", environment: Some("prod"), actor: "kiln", source: "cd", ..rules::Workload::default() };
        let claims = rules::workload_claims(&w, issuer, "sts.amazonaws.com", "j1", 1_000, 300, None);
        // The signing key and the next one (published early) both verify.
        for (kp, kid) in [(&current, "a"), (&next, "b")] {
            let got = verify_published(dir.path(), issuer, &sign_with(kp, kid, &claims)?)?;
            assert_eq!(got.get("sub").and_then(Value::as_str), Some("repo:acme/app:environment:prod"));
        }
        // An unpublished key, a key claiming another's kid, a changed body: refused.
        assert!(verify_published(dir.path(), issuer, &sign_with(&stray, "c", &claims)?).is_err());
        assert!(verify_published(dir.path(), issuer, &sign_with(&stray, "a", &claims)?).is_err());
        let good = sign_with(&current, "a", &claims)?;
        let mut forged: Vec<&str> = good.split('.').collect();
        let other = B64.encode(serde_json::to_vec(&json!({ "iss": issuer, "sub": "repo:evil/app:environment:prod" }))?);
        if let Some(b) = forged.get_mut(1) {
            *b = &other;
        }
        assert!(verify_published(dir.path(), issuer, &forged.join(".")).is_err());
        // Another issuer's token, even with a published key, is refused.
        let elsewhere = rules::workload_claims(&w, "https://other", "a", "j2", 1_000, 300, None);
        assert!(verify_published(dir.path(), issuer, &sign_with(&current, "a", &elsewhere)?).is_err());
        Ok(())
    }
}
