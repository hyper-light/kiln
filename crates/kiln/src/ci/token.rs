//! A leg's token (docs/ci.md W08-R04, W10's precursor). `ACTIONS_RUNTIME_TOKEN` and
//! `GITHUB_TOKEN` carry it, and so do action downloads and git fetches.
//!
//! It is a JWT signed by the server (HS256), so it is checked without any state. Its `scp`
//! claim has the scopes GitHub's toolkit parses for its backend ids:
//! `Actions.Results:<run>:<leg>`. Its `ac` claim lists the cache scopes the leg may read and
//! write, in lookup order, as GitHub's does (W08-R06, W08-R14). W10's job tokens, with
//! permissions and principals, replace it.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::hub::Hub;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What may be done in a cache scope. GitHub's numbers: 1 read, 2 write, 3 both.
pub const READ: u8 = 1;
pub const WRITE: u8 = 2;

/// One cache scope a leg may use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    #[serde(rename = "Scope")]
    pub scope: String,
    #[serde(rename = "Permission")]
    pub permission: u8,
}

/// A leg token's claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    /// The leg.
    pub sub: String,
    pub run: String,
    /// The run's repository (`repos.id`).
    pub repo: String,
    pub scp: String,
    /// Cache scopes in lookup order (W08-R06).
    #[serde(default)]
    pub ac: Vec<Access>,
    /// The leg's cache mode (W08-R14).
    #[serde(default)]
    pub cm: String,
    pub iat: i64,
    pub nbf: i64,
    pub exp: i64,
}

impl Claims {
    /// Scopes the leg may restore from, in order.
    pub fn read_scopes(&self) -> Vec<String> {
        self.ac.iter().filter(|a| a.permission & READ != 0).map(|a| a.scope.clone()).collect()
    }

    /// The scope the leg saves to, if it may save.
    pub fn write_scope(&self) -> Option<String> {
        self.ac.iter().find(|a| a.permission & WRITE != 0).map(|a| a.scope.clone())
    }
}

/// What a token is minted for.
pub struct Leg<'a> {
    pub leg: &'a str,
    pub run: &'a str,
    pub repo: &'a str,
    pub cache: Vec<Access>,
    pub cache_mode: &'a str,
    /// How long the token lives, in seconds.
    pub ttl: i64,
}

fn key(hub: &Hub) -> Vec<u8> {
    hmac_raw(hub.admin_token.as_bytes(), b"kiln ci leg tokens v1")
}

fn hmac_raw(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::{KeyInit as _, Mac as _};
    match hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            m.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// A token for a leg, valid from now for `ttl`.
pub fn mint(hub: &Hub, l: &Leg<'_>) -> String {
    mint_at(&key(hub), l, crate::util::now_unix())
}

fn mint_at(key: &[u8], l: &Leg<'_>, now: i64) -> String {
    let claims = Claims {
        iss: "kiln".into(),
        aud: "kiln".into(),
        sub: l.leg.to_owned(),
        run: l.run.to_owned(),
        repo: l.repo.to_owned(),
        scp: format!("Actions.GenericRead:{run} Actions.UploadArtifacts:{run}:{leg} Actions.Results:{run}:{leg}", run = l.run, leg = l.leg),
        ac: l.cache.clone(),
        cm: l.cache_mode.to_owned(),
        iat: now,
        nbf: now - 60,
        exp: now + l.ttl,
    };
    let head = B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = B64.encode(serde_json::to_vec(&claims).unwrap_or_default());
    let signing = format!("{head}.{body}");
    let sig = B64.encode(hmac_raw(key, signing.as_bytes()));
    format!("{signing}.{sig}")
}

/// The claims of a genuine token that is valid now.
pub fn verify(hub: &Hub, token: &str) -> Option<Claims> {
    verify_at(&key(hub), token, crate::util::now_unix())
}

fn verify_at(key: &[u8], token: &str, now: i64) -> Option<Claims> {
    let (signing, sig) = token.trim().rsplit_once('.')?;
    let (head, body) = signing.split_once('.')?;
    let want = B64.encode(hmac_raw(key, signing.as_bytes()));
    if !crate::util::ct_eq(sig.as_bytes(), want.as_bytes()) {
        return None;
    }
    let h: serde_json::Value = serde_json::from_slice(&B64.decode(head).ok()?).ok()?;
    if h.get("alg").and_then(serde_json::Value::as_str) != Some("HS256") {
        return None;
    }
    let c: Claims = serde_json::from_slice(&B64.decode(body).ok()?).ok()?;
    (c.iss == "kiln" && c.nbf <= now && now < c.exp).then_some(c)
}

/// The token in a request: `Authorization: Bearer <token>` or `token <token>`, or Basic with the
/// token as the password (git's `x-access-token:<token>`, as `actions/checkout` sends it).
pub fn from_headers(headers: &http::HeaderMap) -> Option<String> {
    let v = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token") {
        return Some(value.trim().to_owned());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD.decode(value.trim()).ok()?;
        let s = String::from_utf8(decoded).ok()?;
        return s.split_once(':').map(|(_, p)| p.to_owned());
    }
    None
}

/// The leg a request's token is for.
pub fn of_request(hub: &Hub, headers: &http::HeaderMap) -> Option<Claims> {
    from_headers(headers).and_then(|t| verify(hub, &t))
}

// ------------------------------------------------------------------ job tokens (W10-R03, R04)

/// The prefix of a job token: masking and secret scanners recognize it.
pub const JOB_PREFIX: &str = "kiln_job_";

/// A job token's facts: whose it is and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub leg: String,
    pub run: String,
    /// `repos.id`.
    pub repo: String,
    /// The job's resolved `permissions` (W04-R06).
    pub permissions: std::collections::BTreeMap<String, kiln_ci::plan::perms::Level>,
}

impl Job {
    /// Whether the token grants at least `need` on `scope` (`contents`, `actions`, …).
    pub fn may(&self, scope: &str, need: kiln_ci::plan::perms::Level) -> bool {
        self.permissions.get(scope).copied().unwrap_or(kiln_ci::plan::perms::Level::None) >= need
    }
}

/// A leg attempt's `GITHUB_TOKEN`: `kiln_job_<leg>_<random>`, stored as its hash, with the job's
/// permissions; it lives `ttl` seconds unless revoked first.
pub async fn mint_job(hub: &Hub, leg: &str, run: &str, repo: &str, permissions: &kiln_ci::plan::perms::Resolved, ttl: i64) -> anyhow::Result<String> {
    let secret = crate::util::random_token();
    let token = format!("{JOB_PREFIX}{leg}_{secret}");
    let hash = crate::util::sha256_hex(token.as_bytes());
    let scopes = serde_json::json!({ "permissions": permissions, "repo": repo }).to_string();
    let (leg, run) = (leg.to_owned(), run.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO tokens (id, kind, principal, on_behalf_of, hash, scopes, created_at, expires_at) VALUES (?1, 'job', ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![crate::util::new_id(), leg, run, hash, scopes, now, now + ttl],
            )?;
            Ok(())
        })
        .await?;
    Ok(token)
}

/// The job a token is, while it is live (not expired, not revoked).
pub async fn job_of(hub: &Hub, token: &str) -> Option<Job> {
    if !token.starts_with(JOB_PREFIX) {
        return None;
    }
    let hash = crate::util::sha256_hex(token.trim().as_bytes());
    hub.store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            let now = crate::util::now_unix();
            let row: Option<(String, String, String, String)> = c
                .query_row(
                    "SELECT id, principal, COALESCE(on_behalf_of, ''), scopes FROM tokens WHERE hash = ?1 AND kind = 'job' AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?2)",
                    rusqlite::params![hash, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((id, leg, run, scopes)) = row else { return Ok(None) };
            c.execute("UPDATE tokens SET last_used_at = ?2 WHERE id = ?1", rusqlite::params![id, now])?;
            let v: serde_json::Value = serde_json::from_str(&scopes).unwrap_or_default();
            let permissions = v.get("permissions").and_then(|p| serde_json::from_value(p.clone()).ok()).unwrap_or_default();
            let repo = v.get("repo").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            Ok(Some(Job { leg, run, repo, permissions }))
        })
        .await
        .ok()
        .flatten()
}

/// The job token of a request (`Bearer`, `token`, or git's Basic password).
pub async fn job_of_request(hub: &Hub, headers: &http::HeaderMap) -> Option<Job> {
    let t = from_headers(headers)?;
    job_of(hub, &t).await
}

/// Revokes a leg's job tokens: its attempt ended (W10-R03).
pub async fn revoke_leg(hub: &Hub, leg: &str) -> anyhow::Result<()> {
    let leg = leg.to_owned();
    hub.store.call(move |c| Ok(c.execute("UPDATE tokens SET revoked_at = ?2 WHERE kind = 'job' AND principal = ?1 AND revoked_at IS NULL", rusqlite::params![leg, crate::util::now_unix()]).map(|_| ())?)).await
}

/// Drops job tokens that ended over a day ago.
pub async fn prune(hub: &Hub) -> anyhow::Result<usize> {
    hub.store
        .call(|c| {
            let before = crate::util::now_unix() - 86_400;
            Ok(c.execute("DELETE FROM tokens WHERE kind = 'job' AND (revoked_at < ?1 OR expires_at < ?1)", rusqlite::params![before])?)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg() -> Leg<'static> {
        Leg {
            leg: "01leg",
            run: "01run",
            repo: "r1",
            cache: vec![Access { scope: "refs/pull/7/merge".into(), permission: READ | WRITE }, Access { scope: "refs/heads/main".into(), permission: READ }],
            cache_mode: "write",
            ttl: 3600,
        }
    }

    #[test]
    fn w08_r04_tokens_carry_githubs_scopes_and_verify_until_they_expire() -> Result<(), String> {
        let k = b"key";
        let t = mint_at(k, &leg(), 1000);
        let c = verify_at(k, &t, 1500).ok_or("a fresh token verifies")?;
        assert_eq!(c.sub, "01leg");
        // What `@actions/artifact` parses: the first `Actions.Results` scope, three parts.
        let results = c.scp.split(' ').find(|s| s.starts_with("Actions.Results:")).map(|s| s.split(':').count());
        assert_eq!(results, Some(3));
        assert_eq!(c.read_scopes(), vec!["refs/pull/7/merge", "refs/heads/main"]);
        assert_eq!(c.write_scope().as_deref(), Some("refs/pull/7/merge"));
        // Expired, not yet valid, another key, or tampered: refused.
        assert!(verify_at(k, &t, 1000 + 3600).is_none());
        assert!(verify_at(k, &t, 1000 - 61).is_none());
        assert!(verify_at(b"other", &t, 1500).is_none());
        let mut parts: Vec<String> = t.split('.').map(str::to_owned).collect();
        let body = parts.get_mut(1).ok_or("three parts")?;
        let mut claims: serde_json::Value = serde_json::from_slice(&B64.decode(body.as_bytes()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        claims.as_object_mut().ok_or("an object")?.insert("sub".into(), serde_json::json!("another-leg"));
        *body = B64.encode(claims.to_string());
        assert!(verify_at(k, &parts.join("."), 1500).is_none());
        assert!(verify_at(k, "kiln.leg.abc", 1500).is_none());
        Ok(())
    }

    #[test]
    fn tokens_come_as_bearer_or_basic() -> Result<(), String> {
        let mut h = http::HeaderMap::new();
        h.insert(http::header::AUTHORIZATION, http::HeaderValue::from_static("Bearer abc.def.ghi"));
        assert_eq!(from_headers(&h).as_deref(), Some("abc.def.ghi"));
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("x-access-token:abc.def.ghi"));
        h.insert(http::header::AUTHORIZATION, http::HeaderValue::from_str(&basic).map_err(|e| e.to_string())?);
        assert_eq!(from_headers(&h).as_deref(), Some("abc.def.ghi"));
        Ok(())
    }
}
