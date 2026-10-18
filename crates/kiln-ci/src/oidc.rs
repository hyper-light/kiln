//! kiln as an OpenID Connect issuer (docs/ci.md W10-R06 to W10-R10), the deciding half: which
//! key signs and which are published as time passes, what an ID token says, and its subject.
//!
//! - **Rotation.** Every 90 days a new key is made. It is published 7 days before it signs, and
//!   the key it replaces stays published 7 days after it stops, so a provider that caches the
//!   key set never meets a token signed by a key it doesn't know.
//! - **Claims.** GitHub's, all strings as GitHub sends them, so existing trust policies work.
//!   kiln adds its own `kiln_*` claims.
//! - **Subjects.** GitHub's default, or built from claim keys as GitHub's
//!   `include_claim_keys` builds it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A key is made this long before it signs.
pub const LEAD_SECS: i64 = 7 * 86_400;
/// A key stays published this long after it stops signing.
pub const TAIL_SECS: i64 = 7 * 86_400;
/// How long a key signs before its successor takes over.
pub const LIFETIME_SECS: i64 = 90 * 86_400;
/// ID tokens live 5 minutes by default, at most an hour (W10-R08).
pub const DEFAULT_TTL_SECS: i64 = 300;
pub const MAX_TTL_SECS: i64 = 3600;

/// A signing key's times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyTimes {
    pub kid: String,
    /// When it was made (and published).
    pub created_at: i64,
    /// When it starts signing.
    pub sign_from: i64,
}

/// What rotation says to do now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rotation {
    /// The key that signs now.
    pub sign: Option<String>,
    /// The keys in the published set.
    pub publish: Vec<String>,
    /// Make a key that starts signing at this time.
    pub create: Option<i64>,
    /// Keys past their tail, to delete.
    pub retire: Vec<String>,
}

/// The rotation plan at `now` (W10-R06, W10-AC05).
pub fn rotation(keys: &[KeyTimes], now: i64) -> Rotation {
    let mut sorted: Vec<&KeyTimes> = keys.iter().collect();
    sorted.sort_by_key(|k| (k.sign_from, k.created_at));
    let mut out = Rotation::default();
    // A key signs until its successor starts.
    for (i, k) in sorted.iter().enumerate() {
        let until = sorted.get(i + 1).map(|n| n.sign_from);
        if k.sign_from <= now && until.is_none_or(|u| now < u) {
            out.sign = Some(k.kid.clone());
        }
        let published = k.created_at <= now && until.is_none_or(|u| now < u + TAIL_SECS);
        if published {
            out.publish.push(k.kid.clone());
        } else if until.is_some_and(|u| now >= u + TAIL_SECS) {
            out.retire.push(k.kid.clone());
        }
    }
    let newest = sorted.last();
    match newest {
        // The first key signs at once.
        None => out.create = Some(now),
        Some(n) if n.sign_from <= now && now >= n.sign_from + LIFETIME_SECS - LEAD_SECS => out.create = Some(now + LEAD_SECS),
        _ => {}
    }
    out
}

/// What the claims are made of: the run's `github` context and the leg's facts.
#[derive(Debug, Clone, Default)]
pub struct Facts<'a> {
    /// The `github` context.
    pub github: &'a Value,
    pub environment: Option<&'a str>,
    /// `owner/repo/<path>@<ref>` of the workflow the job is in (a called workflow's, for its
    /// jobs).
    pub job_workflow_ref: Option<&'a str>,
    pub job_workflow_sha: Option<&'a str>,
    pub trusted: bool,
    pub executor: &'a str,
    pub runner: &'a str,
    pub pool: Option<&'a str>,
    /// The leg.
    pub job: &'a str,
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// GitHub's subject for a job (W10-R09), or one built from `keys` (`include_claim_keys`).
pub fn subject(claims: &Map<String, Value>, keys: Option<&[String]>) -> String {
    let get = |k: &str| claims.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let context = || {
        if let Some(e) = claims.get("environment").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            format!("environment:{e}")
        } else if get("event_name") == "pull_request" {
            "pull_request".to_owned()
        } else {
            format!("ref:{}", get("ref"))
        }
    };
    match keys.filter(|k| !k.is_empty()) {
        None => format!("repo:{}:{}", get("repository"), context()),
        Some(keys) => keys
            .iter()
            .map(|k| match k.as_str() {
                "repo" => format!("repo:{}", get("repository")),
                "context" => context(),
                other => format!("{other}:{}", get(other)),
            })
            .collect::<Vec<_>>()
            .join(":"),
    }
}

/// An ID token's claims (W10-R08), all strings as GitHub's are.
#[allow(clippy::too_many_arguments)]
pub fn claims(f: &Facts<'_>, issuer: &str, audience: &str, jti: &str, now: i64, ttl: i64, subject_keys: Option<&[String]>) -> Map<String, Value> {
    let g = f.github;
    let mut c = Map::new();
    let mut put = |k: &str, v: String| {
        c.insert(k.to_owned(), Value::String(v));
    };
    for k in [
        "repository",
        "repository_id",
        "repository_owner",
        "repository_owner_id",
        "ref",
        "ref_type",
        "sha",
        "actor",
        "actor_id",
        "event_name",
        "workflow",
        "workflow_ref",
        "workflow_sha",
        "run_id",
        "run_number",
        "run_attempt",
        "head_ref",
        "base_ref",
    ] {
        put(k, s(g, k));
    }
    put("ref_protected", g.get("ref_protected").and_then(Value::as_bool).unwrap_or(false).to_string());
    put("repository_visibility", g.pointer("/event/repository/visibility").and_then(Value::as_str).unwrap_or("private").to_owned());
    put("job_workflow_ref", f.job_workflow_ref.map_or_else(|| s(g, "workflow_ref"), str::to_owned));
    put("job_workflow_sha", f.job_workflow_sha.map_or_else(|| s(g, "workflow_sha"), str::to_owned));
    put("runner_environment", "self-hosted".to_owned());
    if let Some(e) = f.environment.filter(|e| !e.is_empty()) {
        put("environment", e.to_owned());
    }
    put("kiln_trust", if f.trusted { "trusted" } else { "untrusted" }.to_owned());
    put("kiln_executor", f.executor.to_owned());
    put("kiln_runner", f.runner.to_owned());
    if let Some(p) = f.pool {
        put("kiln_pool", p.to_owned());
    }
    put("kiln_job", f.job.to_owned());
    let sub = subject(&c, subject_keys);
    c.insert("sub".into(), Value::String(sub));
    c.insert("iss".into(), Value::String(issuer.to_owned()));
    c.insert("aud".into(), Value::String(audience.to_owned()));
    c.insert("jti".into(), Value::String(jti.to_owned()));
    let ttl = ttl.clamp(60, MAX_TTL_SECS);
    c.insert("iat".into(), json!(now));
    c.insert("nbf".into(), json!(now - 5));
    c.insert("exp".into(), json!(now + ttl));
    c
}

/// The discovery document (W10-R06), as GitHub's issuer publishes it.
pub fn discovery(issuer: &str, jwks_uri: &str) -> Value {
    json!({
        "issuer": issuer,
        "jwks_uri": jwks_uri,
        "subject_types_supported": ["public", "pairwise"],
        "response_types_supported": ["id_token"],
        "claims_supported": [
            "sub", "aud", "exp", "iat", "iss", "jti", "nbf", "ref", "sha", "repository", "repository_id", "repository_owner",
            "repository_owner_id", "repository_visibility", "run_id", "run_number", "run_attempt", "runner_environment", "actor",
            "actor_id", "workflow", "workflow_ref", "workflow_sha", "head_ref", "base_ref", "event_name", "ref_type", "ref_protected",
            "environment", "job_workflow_ref", "job_workflow_sha", "kiln_trust", "kiln_executor", "kiln_runner", "kiln_pool", "kiln_job"
        ],
        "id_token_signing_alg_values_supported": ["RS256"],
        "scopes_supported": ["openid"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn k(kid: &str, created: i64, from: i64) -> KeyTimes {
        KeyTimes { kid: kid.into(), created_at: created, sign_from: from }
    }

    #[test]
    fn w10_ac05_keys_publish_early_and_retire_late() {
        // No key: make one that signs now.
        assert_eq!(rotation(&[], 100).create, Some(100));
        let a = k("a", 0, 0);
        // Day 10: a signs, alone.
        let r = rotation(std::slice::from_ref(&a), 10 * DAY);
        assert_eq!((r.sign.as_deref(), r.publish.clone(), r.create), (Some("a"), vec!["a".to_owned()], None));
        // Day 83 (90 - 7): make the next, signing at day 90.
        assert_eq!(rotation(std::slice::from_ref(&a), 83 * DAY).create, Some(90 * DAY));
        let b = k("b", 83 * DAY, 90 * DAY);
        // Day 85: a still signs; both are published.
        let r = rotation(&[a.clone(), b.clone()], 85 * DAY);
        assert_eq!((r.sign.as_deref(), r.publish.clone(), r.create), (Some("a"), vec!["a".to_owned(), "b".to_owned()], None));
        // Day 91: b signs; a is still published for tokens it signed just before.
        let r = rotation(&[a.clone(), b.clone()], 91 * DAY);
        assert_eq!((r.sign.as_deref(), r.publish.clone()), (Some("b"), vec!["a".to_owned(), "b".to_owned()]));
        // Day 97: a is retired.
        let r = rotation(&[a, b], 97 * DAY);
        assert_eq!((r.sign.as_deref(), r.publish.clone(), r.retire.clone()), (Some("b"), vec!["b".to_owned()], vec!["a".to_owned()]));
    }

    fn github(event: &str, r: &str) -> Value {
        json!({ "repository": "acme/app", "repository_id": 7, "repository_owner": "acme", "ref": r, "ref_type": "branch", "sha": "abc", "actor": "ada",
                "event_name": event, "workflow": "ci", "workflow_ref": "acme/app/.github/workflows/ci.yml@refs/heads/main", "run_id": 42, "run_number": 3, "run_attempt": 1, "ref_protected": true })
    }

    #[test]
    fn w10_ac06_subjects_default_and_customized() {
        let g = github("push", "refs/heads/main");
        let f = Facts { github: &g, trusted: true, executor: "host", runner: "r1", job: "leg", ..Facts::default() };
        let c = claims(&f, "https://kiln/oidc", "sts.amazonaws.com", "j1", 1000, 300, None);
        assert_eq!(c.get("sub").and_then(Value::as_str), Some("repo:acme/app:ref:refs/heads/main"));
        // Every claim a string, as GitHub's; the times numbers.
        assert_eq!(c.get("run_id"), Some(&json!("42")));
        assert_eq!(c.get("ref_protected"), Some(&json!("true")));
        assert_eq!(c.get("repository_id"), Some(&json!("7")));
        assert_eq!((c.get("aud"), c.get("exp")), (Some(&json!("sts.amazonaws.com")), Some(&json!(1300))));
        assert_eq!(c.get("kiln_trust"), Some(&json!("trusted")));
        // A pull request; an environment.
        let pr = github("pull_request", "refs/pull/5/merge");
        let c = claims(&Facts { github: &pr, ..f.clone() }, "i", "a", "j", 0, 300, None);
        assert_eq!(c.get("sub").and_then(Value::as_str), Some("repo:acme/app:pull_request"));
        let c = claims(&Facts { environment: Some("prod"), ..f.clone() }, "i", "a", "j", 0, 300, None);
        assert_eq!(c.get("sub").and_then(Value::as_str), Some("repo:acme/app:environment:prod"));
        // Customized: GitHub's include_claim_keys.
        let keys = vec!["repo".to_owned(), "context".to_owned(), "kiln_trust".to_owned()];
        let c = claims(&f, "i", "a", "j", 0, 300, Some(&keys));
        assert_eq!(c.get("sub").and_then(Value::as_str), Some("repo:acme/app:ref:refs/heads/main:kiln_trust:trusted"));
        // Lifetimes are kept between a minute and an hour.
        let c = claims(&f, "i", "a", "j", 0, 99_999, None);
        assert_eq!(c.get("exp"), Some(&json!(3600)));
    }
}
