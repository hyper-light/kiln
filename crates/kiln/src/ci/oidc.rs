//! kiln as an OpenID Connect issuer for jobs (docs/ci.md W10-R06 to W10-R10).
//!
//! - **Keys.** RS256, each a 2048-bit RSA key. The private half is sealed by the master key, the
//!   public half is published as a JWK. Rotation (when to make, sign with, publish and retire a
//!   key) is `kiln_ci::oidc::rotation`'s, applied whenever a key is needed.
//! - **Endpoints.** `<issuer>/.well-known/openid-configuration` and `<issuer>/jwks`, open: they
//!   are what providers fetch. Also `/_kiln/oidc/token?api-version=1&audience=…`, the toolkit's
//!   `core.getIDToken()`, for a leg's `ACTIONS_ID_TOKEN_REQUEST_TOKEN`. That one answers
//!   `{"value": "<jwt>"}`, GitHub-exact, and only for legs granted `id-token: write`.
//! - **Reaching it** (W10-R12). Where providers can't reach the server, the two documents go where
//!   they can: an address of the issuer's own that serves them and nothing else, rate-limited
//!   ([`Only`], `[ci.oidc] listen`); or published, to a directory a static host serves, an S3
//!   bucket or a Cloud Storage one (`[ci.oidc] publish_to`, rewritten by the hourly upkeep and at
//!   each rotation, or `kiln oidc publish --to` by hand). Providers that take keys pasted in get
//!   them from `kiln oidc jwks`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Whether the issuer's documents are served: while the CI engine runs (its jobs' tokens), while
/// an account federates kiln's identity (an `aws`, `gcp` or `azure` integration, whose CD syncs
/// mint tokens), or while releases are made here (what runs here is signed for linked servers,
/// and checks are asked with kiln's token: docs/deploy-api.md §9.1, §11). Only reads: fetching the
/// keys makes the first one, so with none of them, nothing is served and no key is made.
pub async fn active(hub: &Hub) -> bool {
    hub.ci.enabled() || crate::deploy::get().is_some() || crate::ci::integrations::federates(hub).await
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
        // At the time it was made, not `now`: a key made as the second turned would sign but go
        // unpublished, and the first key set served or published would be empty.
        plan = rules::rotation(&keys.iter().map(|k| k.times.clone()).collect::<Vec<_>>(), crate::util::now_unix());
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

/// Where the issuer's documents are published (W10-R12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A directory a static host serves.
    Dir(PathBuf),
    /// An S3-compatible bucket, under a prefix (empty, or ending in `/`).
    S3 { bucket: String, prefix: String },
    /// A Cloud Storage bucket, under a prefix.
    Gcs { bucket: String, prefix: String },
}

impl Target {
    /// `s3://<bucket>[/<prefix>]`, `gs://<bucket>[/<prefix>]`, or a directory.
    pub fn parse(to: &str) -> anyhow::Result<Target> {
        let bucket = |rest: &str| -> anyhow::Result<(String, String)> {
            let (b, p) = rest.split_once('/').unwrap_or((rest, ""));
            anyhow::ensure!(!b.is_empty(), "`{to}` names no bucket");
            let p = p.trim_matches('/');
            Ok((b.to_owned(), if p.is_empty() { String::new() } else { format!("{p}/") }))
        };
        if let Some(rest) = to.strip_prefix("s3://") {
            let (bucket, prefix) = bucket(rest)?;
            return Ok(Target::S3 { bucket, prefix });
        }
        if let Some(rest) = to.strip_prefix("gs://") {
            let (bucket, prefix) = bucket(rest)?;
            return Ok(Target::Gcs { bucket, prefix });
        }
        anyhow::ensure!(!to.contains("://"), "`{to}`: publish to a directory, s3://<bucket>[/<prefix>] or gs://<bucket>[/<prefix>]");
        anyhow::ensure!(!to.trim().is_empty(), "publish to where?");
        Ok(Target::Dir(PathBuf::from(to)))
    }
}

/// Puts the documents in an S3-compatible bucket at `base` (`https://s3.<region>.amazonaws.com`),
/// as JSON that caches for five minutes. The bucket lets anyone read them (a bucket policy):
/// providers fetch them unsigned.
pub async fn put_s3(aws: &crate::ci::integrations::aws::Aws, base: &str, bucket: &str, prefix: &str, files: &[(&'static str, String)]) -> anyhow::Result<()> {
    use crate::ci::integrations::aws::{self, Call};
    for (rel, body) in files {
        let path = format!("/{bucket}/{}", aws::encode(&format!("{prefix}{rel}"), true));
        let headers = vec![("content-type".into(), "application/json".into()), ("cache-control".into(), "max-age=300".into())];
        let resp = aws.send(Call { service: "s3", base, method: crate::http::client::Method::Put, path: &path, query: &[], headers, body: body.clone().into_bytes().into() }).await?;
        if !resp.is_success() {
            let status = resp.status();
            let text = String::from_utf8_lossy(&resp.bytes(64 << 10).await.unwrap_or_default()).into_owned();
            let why = aws::xml_text(&text, "Message").or_else(|| aws::xml_text(&text, "Code")).unwrap_or(text);
            anyhow::bail!("putting {prefix}{rel} in {bucket}: HTTP {status}: {why}");
        }
    }
    Ok(())
}

/// Puts the documents in a Cloud Storage bucket with an access token (the JSON API's upload).
/// The bucket lets anyone read them (`allUsers` as a reader).
pub async fn put_gcs(token: &str, bucket: &str, prefix: &str, files: &[(&'static str, String)]) -> anyhow::Result<()> {
    let base = crate::ci::integrations::endpoint("gcp:storage", || "https://storage.googleapis.com".into());
    let http = crate::http::client::Client::new(crate::http::client::Roots::WebPki, None)?;
    for (rel, body) in files {
        let name = crate::ci::integrations::aws::encode(&format!("{prefix}{rel}"), false);
        let url = format!("{base}/upload/storage/v1/b/{}/o?uploadType=media&name={name}", crate::ci::integrations::aws::encode(bucket, false));
        let resp = http
            .request(crate::http::client::Method::Post, &url)
            .timeout(Duration::from_secs(60))
            .header("authorization", &format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body.clone().into_bytes())
            .send()
            .await?;
        if !resp.is_success() {
            let status = resp.status();
            let text = String::from_utf8_lossy(&resp.bytes(64 << 10).await.unwrap_or_default()).into_owned();
            let why = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned)).unwrap_or(text);
            anyhow::bail!("putting {prefix}{rel} in gs://{bucket}: HTTP {status}: {why}");
        }
    }
    Ok(())
}

/// Where `[ci.oidc]` publishes, if anywhere: `publish_to`, else `publish_dir`.
fn publish_target(hub: &Hub) -> anyhow::Result<Option<(String, Target)>> {
    let c = &hub.cfg.ci.oidc;
    let to = match (&c.publish_to, &c.publish_dir) {
        (Some(t), _) => t.clone(),
        (None, Some(d)) => d.display().to_string(),
        (None, None) => return Ok(None),
    };
    let target = Target::parse(&to)?;
    Ok(Some((to, target)))
}

/// What the last publishing did, for Settings → OIDC: where to, when the documents last
/// changed there, when it was last checked, and what went wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Published {
    pub to: String,
    pub hash: String,
    pub at: i64,
    pub checked_at: i64,
    pub error: Option<String>,
}

const PUBLISHED: &str = "ci.oidc.published";

/// What the last publishing did, if any.
pub async fn published(hub: &Hub) -> Option<Published> {
    hub.store.kv_get(PUBLISHED.into()).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok())
}

/// Publishes the documents where `[ci.oidc]` says, when they changed since they last went there:
/// a directory rewritten in place, a bucket's two objects put again. Where, if anywhere. What
/// happened is kept ([`published`]).
pub async fn publish(hub: &Hub) -> anyhow::Result<Option<String>> {
    let Some((to, target)) = publish_target(hub)? else { return Ok(None) };
    let i = issuer(hub);
    let (discovery, set) = (rules::discovery(&i, &format!("{i}/jwks")), jwks(hub).await?);
    let files = published_files(&discovery, &set)?;
    let hash = crate::util::sha256_hex(files.iter().map(|(rel, body)| format!("{to}/{rel}\n{body}\n")).collect::<String>().as_bytes());
    let last = published(hub).await.filter(|p| p.to == to).unwrap_or_default();
    let fresh = last.hash != hash || last.error.is_some();
    let result = match &target {
        Target::Dir(dir) => {
            let (d, a, b) = (dir.clone(), discovery.clone(), set.clone());
            crate::rt::blocking(move || write_published(&d, &a, &b)).await?.map(|_| ())
        }
        _ if !fresh => Ok(()),
        Target::S3 { bucket, prefix } => put_s3_here(hub, bucket, prefix, &files).await,
        Target::Gcs { bucket, prefix } => put_gcs_here(hub, bucket, prefix, &files).await,
    };
    let now = crate::util::now_unix();
    let kept = match &result {
        Ok(()) => Published { to: to.clone(), at: if last.hash == hash { last.at } else { now }, hash, checked_at: now, error: None },
        Err(e) => Published { to: to.clone(), hash: last.hash.clone(), at: last.at, checked_at: now, error: Some(format!("{e:#}")) },
    };
    hub.store.kv_set(PUBLISHED.into(), serde_json::to_string(&kept)?).await?;
    result?;
    if kept.at == now {
        tracing::info!(to = %to, "OIDC: published the issuer's documents");
    }
    Ok(Some(to))
}

/// `[ci.oidc] publish_to = "s3://…"`, signed in as `publish_credentials` says.
async fn put_s3_here(hub: &Hub, bucket: &str, prefix: &str, files: &[(&'static str, String)]) -> anyhow::Result<()> {
    let c = &hub.cfg.ci.oidc;
    let region = c.publish_region.clone().unwrap_or_else(|| "us-east-1".into());
    let base = c.publish_endpoint.clone().unwrap_or_else(|| crate::ci::integrations::aws::endpoint("s3", &region));
    let creds = crate::ci::services::s3::credentials(hub, &c.publish_credentials, "[ci.oidc] publish_credentials").await?;
    put_s3(&crate::ci::integrations::aws::Aws { creds, region }, base.trim_end_matches('/'), bucket, prefix, files).await
}

/// `[ci.oidc] publish_to = "gs://…"`, through the `gcp` integration `publish_credentials` names.
async fn put_gcs_here(hub: &Hub, bucket: &str, prefix: &str, files: &[(&'static str, String)]) -> anyhow::Result<()> {
    let what = "[ci.oidc] publish_credentials";
    let Some(name) = &hub.cfg.ci.oidc.publish_credentials.integration else { anyhow::bail!("{what}: gs:// is published through a gcp integration (integration = \"…\")") };
    let i = crate::ci::integrations::get(hub, name).await?.ok_or_else(|| anyhow::anyhow!("{what}: no integration `{name}`"))?;
    anyhow::ensure!(i.kind == "gcp", "{what}: `{name}` is a {} integration; gs:// is published through a gcp one", i.kind);
    let w = kiln_ci::oidc::Workload { repository: "kiln", environment: None, git_ref: None, sha: None, actor: "kiln", source: "service", app: None, deployment: None, trigger: Some("oidc-publish") };
    let t = crate::ci::integrations::gcp::token(hub, &i, &w).await?;
    put_gcs(&t.access_token, bucket, prefix, files).await
}

/// Whether `url` names a host providers on the internet could reach: HTTPS, and not this
/// machine, a private network or a name only a LAN resolves.
pub fn looks_public(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let name = if host.starts_with('[') { host.split(']').next().unwrap_or_default().trim_start_matches('[') } else { host.split(':').next().unwrap_or_default() };
    if let Ok(ip) = name.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v) => {
                // 100.64.0.0/10 too: carrier-grade NAT, and tailnets.
                let [a, b, _, _] = v.octets();
                !(v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || (a == 100 && (64..128).contains(&b)))
            }
            IpAddr::V6(v) => {
                let [first, ..] = v.segments();
                !(v.is_loopback() || v.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80)
            }
        };
    }
    let name = name.to_ascii_lowercase();
    !name.is_empty()
        && name.contains('.')
        && !["localhost", ".local", ".lan", ".home", ".internal", ".test", ".localdomain", ".ts.net", ".home.arpa"].iter().any(|s| name == s.trim_start_matches('.') || name.ends_with(s))
}

/// The issuer alone, for `[ci.oidc] listen` (W10-R12): its two documents and nothing else, each
/// client held to [`PER_MINUTE`] requests a minute after a burst of [`BURST`].
pub struct Only {
    pub hub: Arc<Hub>,
    asked: parking_lot::Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

/// Requests a client may make a minute, after its first [`BURST`].
const PER_MINUTE: f64 = 60.0;
const BURST: f64 = 30.0;
/// Clients remembered at once; past it, those quiet for a minute are forgotten.
const CLIENTS: usize = 10_000;

impl Only {
    pub fn new(hub: Arc<Hub>) -> Self {
        Only { hub, asked: parking_lot::Mutex::new(HashMap::new()) }
    }

    /// Whether `ip` may ask now: a bucket of [`BURST`], refilled at [`PER_MINUTE`].
    fn allow(&self, ip: IpAddr) -> bool {
        let mut asked = self.asked.lock();
        if asked.len() >= CLIENTS {
            asked.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(60));
            // Every one of them busy this minute: start over rather than scan for each newcomer.
            if asked.len() >= CLIENTS {
                asked.clear();
            }
        }
        let now = Instant::now();
        let (tokens, at) = asked.entry(ip).or_insert((BURST, now));
        *tokens = (*tokens + at.elapsed().as_secs_f64() * PER_MINUTE / 60.0).min(BURST);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl srv::Handler for Only {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        if let Some(r) = req.extensions().get::<srv::RemoteAddr>()
            && !self.allow(r.0.ip())
        {
            let mut resp = srv::json(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "too many requests" }));
            resp.headers_mut().insert(http::header::RETRY_AFTER, http::HeaderValue::from_static("1"));
            return resp;
        }
        if req.method() != http::Method::GET && req.method() != http::Method::HEAD {
            return srv::json(StatusCode::METHOD_NOT_ALLOWED, &json!({ "error": "only GET" }));
        }
        let path = req.uri().path().to_owned();
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some(doc) = is_issuer(&self.hub, &segs) else { return srv::json(StatusCode::NOT_FOUND, &json!({ "error": "only kiln's OIDC issuer is here" })) };
        if !active(&self.hub).await {
            return srv::json(StatusCode::NOT_FOUND, &json!({ "error": "not found" }));
        }
        match document(&self.hub, doc).await {
            Ok(r) => r,
            Err((code, message)) => srv::json(code, &json!({ "error": message })),
        }
    }
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
/// The server's own token for `audience` (W10-R11): `kiln:<server-id>:service:<name>`, for what
/// it federates into as itself (W11's broker: a secret store, a model provider).
pub async fn service_token(hub: &Hub, name: &str, audience: &str, ttl: i64) -> anyhow::Result<String> {
    let id = crate::discovery::instance_id(&hub.cfg.server.data_dir);
    mint(hub, &rules::service_claims(&id, name, &issuer(hub), audience, &crate::util::random_token(), crate::util::now_unix(), ttl)).await
}

/// `POST /api/ci/oidc/token` (W10-R11, `ci.oidc.token`): an agent kiln started asks for its own
/// token, `{ audience, ttl? }`: `kiln:agent:<session>`, acting for whoever started it (ten minutes
/// by default, an hour at most). Nobody but an agent session has one.
pub async fn agent_token(hub: &Hub, caller: &crate::authz::Caller, body: &Value) -> Result<Value, Fail> {
    let Some(session) = caller.agent.clone() else { return Err((StatusCode::FORBIDDEN, "only an agent kiln started has a workload identity: ask from one".into())) };
    let audience = body.get("audience").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).ok_or((StatusCode::BAD_REQUEST, "`audience`: who the token is for".to_owned()))?;
    if !active(hub).await {
        return Err((StatusCode::CONFLICT, "kiln's OIDC issuer isn't serving: turn it on first (Settings → OIDC)".into()));
    }
    let ttl = body.get("ttl").and_then(Value::as_i64).unwrap_or(600);
    let person = super::diffs::acting_for(&caller.name).unwrap_or_else(|| "owner".to_owned());
    let (token, claims) = agent_token_for(hub, &session, &person, audience, ttl).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit(caller.name.clone(), "ci.oidc.agent_token".into(), format!("for {audience}")).await;
    Ok(json!({ "token": token, "subject": claims.get("sub"), "expires_at": claims.get("exp") }))
}

/// Agent `session`'s token for `audience`, acting for `person`, and its claims.
async fn agent_token_for(hub: &Hub, session: &str, person: &str, audience: &str, ttl: i64) -> anyhow::Result<(String, serde_json::Map<String, Value>)> {
    let harness = hub.harness.info(session).map(|i| i.kind);
    let claims = rules::agent_claims(session, person, harness.as_deref(), &issuer(hub), audience, &crate::util::random_token(), crate::util::now_unix(), ttl);
    Ok((mint(hub, &claims).await?, claims))
}

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
/// - `POST /api/ci/oidc/publish`: publish to `[ci.oidc] publish_to` now.
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
            // How providers reach it (W10-R12): this server's URL, addresses of its own, published.
            let c = &hub.cfg.ci.oidc;
            let to = c.publish_to.clone().or_else(|| c.publish_dir.as_ref().map(|d| d.display().to_string()));
            let last = published(hub).await.filter(|p| Some(&p.to) == to.as_ref());
            let reach = json!({
                "public": looks_public(&i),
                "listen": c.listen,
                "publish": to.map(|to| json!({ "to": to, "at": last.as_ref().map(|p| p.at).filter(|a| *a > 0), "checked_at": last.as_ref().map(|p| p.checked_at), "error": last.as_ref().and_then(|p| p.error.clone()) })),
            });
            Ok(srv::json(
                StatusCode::OK,
                &json!({ "issuer": i, "discovery": format!("{i}/.well-known/openid-configuration"), "jwks_uri": format!("{i}/jwks"), "serving": active(hub).await, "keys": list, "subject_claims": hub.cfg.ci.oidc.subject_claims, "reach": reach }),
            ))
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
            Some(to) => Ok(srv::json(StatusCode::OK, &json!({ "published": to }))),
            None => Err((StatusCode::BAD_REQUEST, "no [ci.oidc] publish_to on this server; `kiln oidc publish --to <dir | s3://… | gs://…>` publishes the documents from wherever you run it".into())),
        },
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

/// A scope's subject setting (W10-R09): `use_default` (repositories only), and its claim keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubjectSetting {
    pub use_default: bool,
    pub keys: Vec<String>,
}

/// The claim keys that build a repository's subject, GitHub's way: the repository's own setting
/// (`use_default` meaning GitHub's default subject), else its organization's, else the server's,
/// else `[ci.oidc] subject_claims`; `None` for the default subject.
pub fn subject_keys_of(repo: Option<&SubjectSetting>, org: Option<&SubjectSetting>, server: Option<&SubjectSetting>, config: &[String]) -> Option<Vec<String>> {
    if let Some(r) = repo {
        return (!r.use_default && !r.keys.is_empty()).then(|| r.keys.clone());
    }
    for s in [org, server].into_iter().flatten() {
        if !s.keys.is_empty() {
            return Some(s.keys.clone());
        }
    }
    (!config.is_empty()).then(|| config.to_vec())
}

async fn subject_setting(hub: &Hub, scope: &str) -> anyhow::Result<Option<SubjectSetting>> {
    let scope = scope.to_owned();
    hub.store
        .call(move |c| {
            let row: Option<(bool, String)> = c.query_row("SELECT use_default, claim_keys FROM ci_oidc_subjects WHERE scope = ?1", [&scope], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            Ok(row.map(|(use_default, keys)| SubjectSetting { use_default, keys: serde_json::from_str(&keys).unwrap_or_default() }))
        })
        .await
}

/// The claim keys for `repo`'s tokens (`owner/name`), as [`subject_keys_of`] picks them.
pub async fn subject_keys(hub: &Hub, repo: &str) -> Option<Vec<String>> {
    let owner = repo.split('/').next().unwrap_or_default();
    let (r, o, s) = (subject_setting(hub, &format!("repo:{repo}")).await.ok().flatten(), subject_setting(hub, &format!("org:{owner}")).await.ok().flatten(), subject_setting(hub, "server").await.ok().flatten());
    subject_keys_of(r.as_ref(), o.as_ref(), s.as_ref(), &hub.cfg.ci.oidc.subject_claims)
}

/// The claim keys a subject can be built from: every claim an ID token has, and GitHub's
/// `repo` and `context`.
pub fn known_claim_keys() -> Vec<String> {
    let github = json!({ "repository": "o/r", "ref": "refs/heads/main" });
    let f = rules::Facts { github: &github, environment: Some("e"), job_workflow_ref: Some("x"), job_workflow_sha: Some("y"), trusted: true, executor: "host", runner: "r", pool: Some("p"), job: "j" };
    let mut keys: Vec<String> = rules::claims(&f, "https://i", "a", "j", 0, 60, None).keys().cloned().collect();
    keys.retain(|k| !matches!(k.as_str(), "iss" | "aud" | "exp" | "iat" | "nbf" | "jti" | "sub"));
    keys.extend(["repo".to_owned(), "context".to_owned()]);
    keys.sort();
    keys.dedup();
    keys
}

/// `GET /api/ci/oidc/subject?scope=` and `PUT` with `{ use_default, include_claim_keys }`
/// (W10-R09): a scope's subject setting, and what its tokens' subjects look like.
pub async fn subject(hub: &Hub, method: &http::Method, q: &std::collections::HashMap<String, String>, body: Value, by: &str) -> Result<Response, Fail> {
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let scope = q.get("scope").cloned().unwrap_or_else(|| "server".into());
    let scope = match crate::authz::Scope::parse(&scope) {
        Ok(crate::authz::Scope::Server) => "server".to_owned(),
        Ok(crate::authz::Scope::Org { org }) => format!("org:{org}"),
        Ok(crate::authz::Scope::Repo { repo }) => format!("repo:{repo}"),
        _ => return Err((StatusCode::BAD_REQUEST, format!("`{scope}` isn't a scope for subjects: server, org:<owner> or repo:<owner/name>"))),
    };
    if method == http::Method::PUT {
        let use_default = body.get("use_default").and_then(Value::as_bool).unwrap_or(false);
        if use_default && !scope.starts_with("repo:") {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "use_default is a repository's choice; an organization or the server sets include_claim_keys".into()));
        }
        let keys: Vec<String> = body.get("include_claim_keys").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        if !use_default && keys.is_empty() {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "include_claim_keys is required unless use_default is true".into()));
        }
        let known = known_claim_keys();
        if let Some(k) = keys.iter().find(|k| !known.contains(k)) {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("`{k}` isn't a claim of kiln's ID tokens; the claim keys are {}", known.join(", "))));
        }
        let (s, k, who) = (scope.clone(), serde_json::to_string(&keys).unwrap_or_else(|_| "[]".into()), by.to_owned());
        hub.store
            .call(move |c| {
                c.execute(
                    "INSERT INTO ci_oidc_subjects (scope, use_default, claim_keys, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (scope) DO UPDATE SET use_default = ?2, claim_keys = ?3, updated_at = ?4, updated_by = ?5",
                    params![s, use_default, k, crate::util::now_unix(), who],
                )?;
                Ok(())
            })
            .await
            .map_err(fail)?;
        hub.store.audit(by.to_owned(), "ci.oidc.subject".into(), format!("{scope}: {}", if use_default { "the default subject".to_owned() } else { keys.join(", ") })).await;
    } else if method == http::Method::DELETE {
        let s = scope.clone();
        hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_oidc_subjects WHERE scope = ?1", [&s]).map(|_| ())?)).await.map_err(fail)?;
        hub.store.audit(by.to_owned(), "ci.oidc.subject".into(), format!("{scope}: inherited again")).await;
    }
    let own = subject_setting(hub, &scope).await.map_err(fail)?;
    // What a token of a job on main without an environment would say, for a repository.
    let effective = match scope.strip_prefix("repo:") {
        Some(repo) => subject_keys(hub, repo).await,
        None => own.as_ref().map(|s| s.keys.clone()).filter(|k| !k.is_empty()).or_else(|| (!hub.cfg.ci.oidc.subject_claims.is_empty()).then(|| hub.cfg.ci.oidc.subject_claims.clone())),
    };
    // A trusted push to main's token, as the issuer would make it.
    let example = scope.strip_prefix("repo:").map(|repo| {
        let github = json!({ "repository": repo, "repository_owner": repo.split('/').next().unwrap_or_default(), "ref": "refs/heads/main", "ref_type": "branch", "event_name": "push", "workflow": "CI" });
        let f = rules::Facts { github: &github, environment: None, job_workflow_ref: None, job_workflow_sha: None, trusted: true, executor: "host", runner: "", pool: None, job: "" };
        rules::claims(&f, &issuer(hub), "example", "example", 0, 60, effective.as_deref()).get("sub").and_then(Value::as_str).unwrap_or_default().to_owned()
    });
    Ok(srv::json(StatusCode::OK, &json!({
        "scope": scope,
        "use_default": own.as_ref().is_some_and(|s| s.use_default),
        "include_claim_keys": own.map(|s| s.keys).unwrap_or_default(),
        "effective_claim_keys": effective,
        "example_subject": example,
        "claim_keys": known_claim_keys(),
    })))
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
    // The repository's subject, as it, its organization or the server customized it (W10-R09).
    let keys = subject_keys(hub, f.github.get("repository").and_then(Value::as_str).unwrap_or_default()).await.unwrap_or_default();
    let facts = rules::Facts { github: &f.github, environment: environment.as_deref(), job_workflow_ref: None, job_workflow_sha: None, trusted: f.trusted, executor: &executor, runner: f.agent.as_deref().unwrap_or_default(), pool: None, job: &claims.sub };
    let c = rules::claims(&facts, &iss, &audience, &crate::util::new_id(), crate::util::now_unix(), rules::DEFAULT_TTL_SECS, (!keys.is_empty()).then_some(keys.as_slice()));
    let jwt = sign(hub, &c).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &json!({ "count": jwt.len(), "value": jwt })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W10-R09: a repository's setting, else its organization's, else the server's, else the
    /// configuration; a repository's `use_default` is GitHub's default whatever the others say.
    /// W10-R11: an agent session gets its own token, acting for whoever started it; a person or a
    /// plain token doesn't; the server's own token names it and the service.
    #[test]
    fn w10_r11_agent_and_service_tokens() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let payload = |token: &str| -> anyhow::Result<Value> {
                let body = token.split('.').nth(1).ok_or_else(|| anyhow::anyhow!("not a JWT"))?;
                Ok(serde_json::from_slice(&B64.decode(body)?)?)
            };
            let (token, _) = agent_token_for(&hub, "s1abcdef", "user:ada", "api.anthropic.com", 600).await?;
            let claims = payload(&token)?;
            assert_eq!((claims.get("sub"), claims.get("aud"), claims.get("kiln_on_behalf_of")), (Some(&json!("kiln:agent:s1abcdef")), Some(&json!("api.anthropic.com")), Some(&json!("user:ada"))));
            // Who may ask: an agent session, naming an audience; the issuer must be on.
            let mut agent = crate::authz::Caller::owner();
            agent.name = "agent:s1abcdef for user:ada".into();
            agent.agent = Some("s1abcdef".into());
            assert!(agent_token(&hub, &crate::authz::Caller::owner(), &json!({ "audience": "x" })).await.is_err_and(|(c, _)| c == StatusCode::FORBIDDEN));
            assert!(agent_token(&hub, &agent, &json!({})).await.is_err_and(|(c, _)| c == StatusCode::BAD_REQUEST));
            assert!(agent_token(&hub, &agent, &json!({ "audience": "x" })).await.is_err_and(|(c, _)| c == StatusCode::CONFLICT), "the engine is off here");
            let service = payload(&service_token(&hub, "broker", "vault", 300).await?)?;
            let id = crate::discovery::instance_id(&hub.cfg.server.data_dir);
            assert_eq!(service.get("sub"), Some(&json!(format!("kiln:{id}:service:broker"))));
            Ok(())
        })
    }

    #[test]
    fn w10_r09_subject_claims_by_scope() {
        let keys = |k: &[&str]| SubjectSetting { use_default: false, keys: k.iter().map(|s| (*s).to_owned()).collect() };
        let config = vec!["repo".to_owned(), "context".to_owned(), "kiln_trust".to_owned()];
        let (repo, org, server) = (keys(&["repo", "job_workflow_ref"]), keys(&["repository_owner", "context"]), keys(&["repo", "environment"]));
        assert_eq!(subject_keys_of(Some(&repo), Some(&org), Some(&server), &config), Some(repo.keys.clone()));
        assert_eq!(subject_keys_of(None, Some(&org), Some(&server), &config), Some(org.keys.clone()));
        assert_eq!(subject_keys_of(None, None, Some(&server), &config), Some(server.keys.clone()));
        assert_eq!(subject_keys_of(None, None, None, &config), Some(config.clone()));
        assert_eq!(subject_keys_of(None, None, None, &[]), None, "GitHub's default subject");
        let default = SubjectSetting { use_default: true, keys: Vec::new() };
        assert_eq!(subject_keys_of(Some(&default), Some(&org), Some(&server), &config), None, "use_default wins over the organization");
        let known = known_claim_keys();
        for k in ["repo", "context", "repository", "repository_owner", "environment", "job_workflow_ref", "kiln_trust", "ref", "workflow"] {
            assert!(known.iter().any(|x| x == k), "{k} in {known:?}");
        }
        assert!(!known.iter().any(|x| x == "exp" || x == "sub"), "not the token's own times or its subject");
    }

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

    /// W10-R12's targets (a directory, an S3 bucket, a Cloud Storage one, each with a prefix),
    /// and which issuer URLs providers on the internet could reach.
    #[test]
    fn w10_r12_publish_targets_and_public_urls() -> anyhow::Result<()> {
        assert_eq!(Target::parse("s3://kiln-oidc")?, Target::S3 { bucket: "kiln-oidc".into(), prefix: String::new() });
        assert_eq!(Target::parse("s3://kiln-oidc/acme/kiln/")?, Target::S3 { bucket: "kiln-oidc".into(), prefix: "acme/kiln/".into() });
        assert_eq!(Target::parse("gs://pub/oidc")?, Target::Gcs { bucket: "pub".into(), prefix: "oidc/".into() });
        assert_eq!(Target::parse("/srv/www/oidc")?, Target::Dir("/srv/www/oidc".into()));
        assert!(Target::parse("s3://").is_err() && Target::parse("ftp://x/y").is_err() && Target::parse(" ").is_err());
        for url in ["https://oidc.acme.com", "https://kiln-oidc.s3.us-east-1.amazonaws.com/acme", "https://storage.googleapis.com/acme-oidc/kiln", "https://203.0.113.9/oidc"] {
            assert!(looks_public(url), "{url}");
        }
        for url in [
            "http://oidc.acme.com",
            "https://localhost:7070/oidc",
            "https://kiln.local/oidc",
            "https://192.168.1.4/oidc",
            "https://10.0.0.2:7070/oidc",
            "https://100.101.102.103/oidc",
            "https://kiln.tail1234.ts.net/oidc",
            "https://[fd00::1]/oidc",
            "https://kiln/oidc",
        ] {
            assert!(!looks_public(url), "{url}");
        }
        Ok(())
    }

    /// The issuer's own address: a client's burst, then one a second; another client has its own.
    #[test]
    fn w10_r12_the_issuers_address_is_rate_limited() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let only = Only::new(crate::hub::tests::scratch_hub().await?);
            let (a, b): (IpAddr, IpAddr) = ("198.51.100.1".parse()?, "198.51.100.2".parse()?);
            assert_eq!((0..40).filter(|_| only.allow(a)).count(), 30, "a burst of 30");
            assert!(only.allow(b), "another client has its own");
            Ok(())
        })
    }

    /// W10-R12: published to an S3 bucket (each request signed as S3 checks it), the documents a
    /// provider fetches from there verify kiln's tokens. Published again unchanged, nothing is
    /// sent. A wrong secret is refused, and kept as what went wrong.
    #[test]
    fn w10_r12_published_to_a_bucket() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (base, fake) = crate::ci::services::s3::tests::fake_s3()?;
            let dir = tempfile::tempdir()?;
            let issuer = "https://kiln-oidc.s3.us-east-1.amazonaws.com/acme";
            let hub_with = |secret: &str| -> anyhow::Result<_> {
                let file = dir.path().join(format!("secret-{secret}"));
                std::fs::write(&file, secret)?;
                let b = base.clone();
                Ok(crate::hub::tests::scratch_hub_with(move |c| {
                    c.ci.oidc.issuer = Some(issuer.into());
                    c.ci.oidc.publish_to = Some("s3://kiln-oidc/acme".into());
                    c.ci.oidc.publish_endpoint = Some(b);
                    c.ci.oidc.publish_credentials = crate::config::ci::S3Credentials { access_key_id: Some("AKIDTEST".into()), secret_access_key_file: Some(file), ..Default::default() };
                }))
            };
            let hub = hub_with("s3cr3t")?.await?;
            assert_eq!(publish(&hub).await?.as_deref(), Some("s3://kiln-oidc/acme"));
            assert!(fake.refused.lock().is_empty(), "{:?}", fake.refused.lock());
            let object = |k: &str| fake.objects.lock().get(&format!("/kiln-oidc/acme/{k}")).cloned().unwrap_or_default();
            let discovery: Value = serde_json::from_slice(&object(".well-known/openid-configuration"))?;
            assert_eq!((discovery.get("issuer"), discovery.get("jwks_uri")), (Some(&json!(issuer)), Some(&json!(format!("{issuer}/jwks")))));
            // A provider's check, against what the bucket serves.
            let served = tempfile::tempdir()?;
            std::fs::create_dir_all(served.path().join(".well-known"))?;
            std::fs::write(served.path().join(".well-known/openid-configuration"), object(".well-known/openid-configuration"))?;
            std::fs::write(served.path().join("jwks"), object("jwks"))?;
            let jwt = mint(&hub, &rules::service_claims("i", "check", issuer, "sts.amazonaws.com", "j", crate::util::now_unix(), 300)).await?;
            verify_published(served.path(), issuer, &jwt)
                .map_err(|e| anyhow::anyhow!("{e:#}; the bucket's key set: {}; its objects: {:?}", String::from_utf8_lossy(&object("jwks")), fake.objects.lock().keys().collect::<Vec<_>>()))?;
            let puts = *fake.puts.lock();
            publish(&hub).await?;
            assert_eq!(*fake.puts.lock(), puts, "unchanged: nothing sent");
            let kept = published(&hub).await.ok_or_else(|| anyhow::anyhow!("nothing kept"))?;
            assert!(kept.at > 0 && kept.error.is_none(), "{kept:?}");

            let other = hub_with("not-it")?.await?;
            assert!(publish(&other).await.is_err());
            let said = published(&other).await.and_then(|p| p.error).unwrap_or_default();
            assert!(said.contains("HTTP 403"), "{said}");
            Ok(())
        })
    }

    /// Cloud Storage as its JSON API takes an upload: a bearer token, each object named.
    #[derive(Default)]
    struct Gcs {
        objects: parking_lot::Mutex<HashMap<String, (String, Vec<u8>)>>,
    }

    impl srv::Handler for Gcs {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            let (path, q) = (req.uri().path().to_owned(), srv::query(&req));
            let header = |h: &http::header::HeaderName| srv::header_str(&req, h).unwrap_or("").to_owned();
            let (auth, kind) = (header(&http::header::AUTHORIZATION), header(&http::header::CONTENT_TYPE));
            let body = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            if auth != "Bearer ya29.test" {
                return srv::json(StatusCode::UNAUTHORIZED, &json!({ "error": { "code": 401, "message": "Invalid Credentials" } }));
            }
            if path != "/upload/storage/v1/b/acme-oidc/o" || q.get("uploadType").map(String::as_str) != Some("media") {
                return srv::json(StatusCode::NOT_FOUND, &json!({ "error": { "code": 404, "message": format!("nothing at {path}") } }));
            }
            let name = q.get("name").cloned().unwrap_or_default();
            self.objects.lock().insert(name.clone(), (kind, body.to_vec()));
            srv::json(StatusCode::OK, &json!({ "bucket": "acme-oidc", "name": name }))
        }
    }

    #[test]
    fn w10_r12_published_to_cloud_storage() -> anyhow::Result<()> {
        // Cloud Storage's endpoint is the process's, as the GCP fake's tests point it.
        let _one = crate::cd::targets::fake_gcp::GCP.lock();
        crate::rt::block_on(async {
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
            let fake = Arc::new(Gcs::default());
            let f = Arc::clone(&fake);
            crate::rt::spawn(async move {
                while let Ok(s) = tcp.accept().await {
                    let (h, r) = (Arc::clone(&f), s.peer_addr().ok());
                    crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
                }
            });
            crate::ci::integrations::override_endpoint("gcp:storage", &format!("http://{addr}"));
            let files = published_files(&json!({ "issuer": "https://storage.googleapis.com/acme-oidc/kiln" }), &json!({ "keys": [] }))?;
            put_gcs("ya29.test", "acme-oidc", "kiln/", &files).await?;
            let mut names: Vec<String> = fake.objects.lock().keys().cloned().collect();
            names.sort();
            assert_eq!(names, ["kiln/.well-known/openid-configuration", "kiln/jwks"]);
            assert!(fake.objects.lock().values().all(|(kind, body)| kind == "application/json" && !body.is_empty()));
            let e = put_gcs("ya29.wrong", "acme-oidc", "kiln/", &files).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(e.contains("Invalid Credentials"), "{e}");
            Ok(())
        })
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
