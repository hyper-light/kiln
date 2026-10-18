//! kiln as an OpenID Connect issuer for jobs (docs/ci.md W10-R06 to W10-R10).
//!
//! - **Keys.** RS256, each a 2048-bit RSA key. The private half is sealed by the master key, the
//!   public half is published as a JWK. Rotation (when to make, sign with, publish and retire a
//!   key) is `kiln_ci::oidc::rotation`'s, applied whenever a key is needed.
//! - **Endpoints.** `<issuer>/.well-known/openid-configuration` and `<issuer>/jwks`, open: they
//!   are what providers fetch. Also `/_kiln/oidc/token?api-version=1&audience=…`, the toolkit's
//!   `core.getIDToken()`, for a leg's `ACTIONS_ID_TOKEN_REQUEST_TOKEN`. That one answers
//!   `{"value": "<jwt>"}`, GitHub-exact, and only for legs granted `id-token: write`.

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

/// The published key set (JWKS).
pub async fn jwks(hub: &Hub) -> anyhow::Result<Value> {
    let (keys, plan) = rotate(hub).await?;
    let list: Vec<Value> = keys.iter().filter(|k| plan.publish.contains(&k.times.kid)).map(|k| json!({ "kty": "RSA", "alg": "RS256", "use": "sig", "kid": k.times.kid, "n": k.n, "e": k.e })).collect();
    Ok(json!({ "keys": list }))
}

/// Signs `claims` with the current key: a JWT.
async fn sign(hub: &Hub, claims: &serde_json::Map<String, Value>) -> anyhow::Result<String> {
    let (_, plan) = rotate(hub).await?;
    let kid = plan.sign.ok_or_else(|| anyhow::anyhow!("no OIDC key signs now"))?;
    let k = kid.clone();
    let (sealed, nonce): (Vec<u8>, Vec<u8>) = hub.store.call(move |c| Ok(c.query_row("SELECT sealed, nonce FROM oidc_keys WHERE kid = ?1", [&k], |r| Ok((r.get(0)?, r.get(1)?)))?)).await?;
    let pkcs8 = secrets::open_master(hub, &sealed, &nonce, &aad(&kid))?;
    let kp = KeyPair::from_pkcs8(&pkcs8).map_err(|_| anyhow::anyhow!("the OIDC key can't be read"))?;
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
}
