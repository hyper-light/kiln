//! External checks (docs/deploy-api.md §9.1): a URL kiln asks whether a release may go, and a kiln
//! workflow run for it. Each is asked once per release and kept: its answer, with its evidence, is
//! the check's result on the release, and the gates take that from then on (a URL that said pass or
//! fail isn't asked again; a workflow runs once, and is followed until its run ends).
//!
//! - **A URL** is asked `POST <url>` with the release as JSON (`{check, server, release: {id, app,
//!   environment, kind, version, artifacts, requested_by, promoted_from, url}}`) and kiln's OIDC
//!   token for it (`Authorization: Bearer`, the URL its audience), so it knows kiln asks. It
//!   answers 2xx with `{"result": "pass" | "fail" | "pending", "detail"?, "retry_after"?}`; one
//!   pending is asked again (in `retry_after` seconds, 15 by default). One that can't be reached,
//!   or answers anything else, is asked again in 30 seconds, the release waiting and saying why.
//!   URLs are reached as webhooks are: not on private networks unless `[ci]
//!   allow_private_targets` allows them.
//! - **A workflow** of one of kiln's repositories runs by `workflow_dispatch` at its ref, given what
//!   it declares of the release's `app`, `environment`, `version`, `release` and `kind` as inputs.
//!   The check passes when the run succeeds and fails when it doesn't; the run is its evidence.

use std::time::Duration;

use serde_json::{Value, json};

use super::Deploy;
use super::model::{CheckSpec, GateResult, HttpCheck, Release, WorkflowCheck};
use crate::store::Status;

/// What a check says now.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Pass { detail: String, evidence: Value },
    Fail { detail: String, evidence: Value },
    /// Not answered yet: looked at again at `again` (unix seconds).
    Pending { detail: String, evidence: Value, again: i64 },
}

/// How soon a URL that said `pending` is asked again, when it doesn't say.
const PENDING_SECS: i64 = 15;
/// How soon a URL, or the CI engine, that couldn't be asked is asked again.
const UNREACHABLE_SECS: i64 = 30;
/// How soon a running workflow is looked at again.
const RUNNING_SECS: i64 = 10;
/// How long a URL may take to answer: by default, and at most.
const TIMEOUT_SECS: (u64, u64) = (10, 120);

/// What `spec` says of `r` now, given what it said before (`prior`).
pub async fn run(d: &Deploy, r: &Release, spec: &CheckSpec, prior: Option<&GateResult>) -> Outcome {
    match (&spec.http, &spec.workflow) {
        (Some(h), _) => http(d, r, &spec.name, h, prior).await,
        (None, Some(w)) => workflow(d, r, w, prior).await,
        (None, None) => Outcome::Fail { detail: format!("`{}` is neither a URL nor a workflow", spec.name), evidence: Value::Null },
    }
}

/// What a URL said before, when it said pass or fail: it isn't asked again.
fn answered(prior: Option<&GateResult>) -> Option<Outcome> {
    let p = prior?;
    let (detail, evidence) = (p.detail.clone(), p.evidence.clone());
    match p.evidence.get("result").and_then(Value::as_str)? {
        "pass" => Some(Outcome::Pass { detail, evidence }),
        "fail" => Some(Outcome::Fail { detail, evidence }),
        _ => None,
    }
}

/// kiln's OIDC token for asking `url` about `r`: the URL is its audience.
async fn identity(d: &Deploy, r: &Release, name: &str, url: &str) -> anyhow::Result<String> {
    let now = crate::util::now_unix();
    let mut claims = serde_json::Map::new();
    claims.insert("iss".into(), json!(crate::ci::oidc::issuer(&d.hub)));
    claims.insert("sub".into(), json!(format!("deploy:{}:{}", r.environment, r.app)));
    claims.insert("aud".into(), json!(url));
    claims.insert("iat".into(), json!(now));
    claims.insert("exp".into(), json!(now + 300));
    claims.insert("release".into(), json!(r.id));
    claims.insert("check".into(), json!(name));
    crate::ci::oidc::mint(&d.hub, &claims).await
}

async fn http(d: &Deploy, r: &Release, name: &str, c: &HttpCheck, prior: Option<&GateResult>) -> Outcome {
    if let Some(said) = answered(prior) {
        return said;
    }
    let now = crate::util::now_unix();
    let base = d.hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let unreachable = |why: String| Outcome::Pending {
        detail: format!("can't ask {}: {why}; asking again", c.url),
        evidence: json!({ "url": c.url, "result": "unreachable", "error": why, "at": now }),
        again: now + UNREACHABLE_SECS,
    };
    let body = json!({
        "check": name,
        "server": base,
        "release": {
            "id": r.id, "app": r.app, "environment": r.environment, "kind": r.kind.as_str(), "version": r.version,
            "artifacts": r.artifacts, "requested_by": r.requested_by, "promoted_from": r.promoted_from,
            "url": format!("{base}/deployments/releases/{}", r.id),
        },
    });
    let timeout = c.timeout.as_deref().and_then(kiln_ci::trigger::cron::seconds).unwrap_or(TIMEOUT_SECS.0).clamp(1, TIMEOUT_SECS.1);
    let token = match identity(d, r, name, &c.url).await {
        Ok(t) => t,
        Err(e) => return unreachable(format!("kiln's identity for it: {e:#}")),
    };
    let client = match crate::http::client::Client::public() {
        Ok(c) => c,
        Err(e) => return unreachable(format!("{e:#}")),
    };
    let sent = client.post(&c.url).reaching(&d.hub.cfg.ci.reach()).bearer(&token).header("user-agent", "kiln-deploy").json(&body).timeout(Duration::from_secs(timeout)).send().await;
    let resp = match sent {
        Ok(resp) => resp,
        Err(e) => return unreachable(format!("{e:#}")),
    };
    let status = resp.status();
    let bytes = match resp.bytes(64 << 10).await {
        Ok(b) => b,
        Err(e) => return unreachable(format!("{e:#}")),
    };
    let answer: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).chars().take(500).collect()));
    let detail = answer.get("detail").and_then(Value::as_str).map(|s| s.chars().take(500).collect::<String>());
    let evidence = |result: &str| json!({ "url": c.url, "status": status, "answer": answer, "result": result, "at": now });
    let result = if (200..300).contains(&status) { answer.get("result").and_then(Value::as_str) } else { None };
    match result {
        Some("pass") => Outcome::Pass { detail: detail.unwrap_or_else(|| format!("{} said pass", c.url)), evidence: evidence("pass") },
        Some("fail") => Outcome::Fail { detail: detail.unwrap_or_else(|| format!("{} said fail", c.url)), evidence: evidence("fail") },
        Some("pending") => {
            let wait = answer.get("retry_after").and_then(Value::as_i64).unwrap_or(PENDING_SECS).clamp(5, 300);
            Outcome::Pending { detail: detail.map_or_else(|| format!("waits for {} to answer", c.url), |d| format!("waits for {}: {d}", c.url)), evidence: evidence("pending"), again: now + wait }
        }
        _ => {
            let mut said = unreachable(format!("it answered {status} without a result (pass, fail or pending)"));
            if let Outcome::Pending { evidence: e, .. } = &mut said {
                set(e, "status", json!(status));
                set(e, "answer", answer.clone());
            }
            said
        }
    }
}

async fn workflow(d: &Deploy, r: &Release, c: &WorkflowCheck, prior: Option<&GateResult>) -> Outcome {
    let now = crate::util::now_unix();
    let base = d.hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let mut evidence = json!({ "repository": c.repository, "workflow": c.workflow, "ref": c.git_ref });
    // Started before: how its run stands.
    if let Some(run) = prior.and_then(|p| p.evidence.get("run")).and_then(Value::as_str) {
        match d.hub.store.run(run.to_owned()).await {
            Ok(Some(found)) => return ran(&found, prior.map_or(evidence, |p| p.evidence.clone()), now),
            // Gone (its records were pruned): it runs again.
            Ok(None) => {}
            Err(e) => return Outcome::Pending { detail: format!("can't read run {run}: {e:#}"), evidence: prior.map_or(Value::Null, |p| p.evidence.clone()), again: now + UNREACHABLE_SECS },
        }
    }
    let repos = match d.hub.store.repos().await {
        Ok(rs) => rs,
        Err(e) => return Outcome::Pending { detail: format!("can't read kiln's repositories: {e:#}"), evidence, again: now + UNREACHABLE_SECS },
    };
    let Some(repo) = repos.into_iter().find(|x| x.name == c.repository) else {
        return Outcome::Fail { detail: format!("kiln has no repository {} to run {} from", c.repository, c.workflow), evidence };
    };
    let git_ref = match &c.git_ref {
        Some(g) => g.clone(),
        None => crate::ci::events::facts(&d.hub, &repo).await.default_branch,
    };
    let offered: serde_json::Map<String, Value> = [
        ("app", json!(r.app)),
        ("environment", json!(r.environment)),
        ("version", json!(r.version.clone().unwrap_or_default())),
        ("release", json!(r.id.to_string())),
        ("kind", json!(r.kind.as_str())),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    match crate::ci::events::dispatch_run(&d.hub, &repo, &git_ref, &c.workflow, &offered, "kiln").await {
        Ok(run) => {
            set(&mut evidence, "ref", json!(git_ref));
            set(&mut evidence, "run", json!(run));
            set(&mut evidence, "url", json!(format!("{base}/runs/{run}")));
            set(&mut evidence, "status", json!(Status::Queued.as_str()));
            Outcome::Pending { detail: format!("waits for {} to run ({}/runs/{run})", c.workflow, base), evidence, again: now + RUNNING_SECS }
        }
        // kiln's own trouble: tried again.
        Err(e) if e.status.is_server_error() => Outcome::Pending { detail: format!("{} didn't start: {}; trying again", c.workflow, e.message), evidence, again: now + UNREACHABLE_SECS },
        Err(e) => Outcome::Fail { detail: format!("{} can't run for it: {}", c.workflow, e.message), evidence },
    }
}

/// Sets `key` of evidence that's an object.
fn set(evidence: &mut Value, key: &str, value: Value) {
    if let Some(o) = evidence.as_object_mut() {
        o.insert(key.to_owned(), value);
    }
}

/// A check's run, as it stands.
fn ran(run: &crate::store::Run, mut evidence: Value, now: i64) -> Outcome {
    set(&mut evidence, "status", json!(run.status.as_str()));
    set(&mut evidence, "number", json!(run.number));
    let what = format!("{} (run #{})", run.pipeline, run.number);
    match run.status {
        Status::Pending | Status::Queued | Status::Running => Outcome::Pending { detail: format!("waits for {what}: {}", run.status.as_str()), evidence, again: now + RUNNING_SECS },
        Status::Succeeded => Outcome::Pass { detail: format!("{what} succeeded"), evidence },
        Status::Failed | Status::Cancelled | Status::Skipped | Status::TimedOut => {
            let why = run.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default();
            Outcome::Fail { detail: format!("{what} {}{why}", run.status.as_str().replace('_', " ")), evidence }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_of(status: Status) -> crate::store::Run {
        crate::store::Run {
            id: "r1".into(),
            repo_id: "1".into(),
            repo_name: "acme/web".into(),
            number: 7,
            pipeline: ".kiln/smoke.yaml".into(),
            event: "workflow_dispatch".into(),
            branch: "main".into(),
            sha: "abc".into(),
            message: String::new(),
            author: "kiln".into(),
            status,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        }
    }

    #[test]
    fn a_workflow_check_waits_for_its_run_and_takes_its_result() {
        let e = json!({ "run": "r1" });
        assert!(matches!(ran(&run_of(Status::Queued), e.clone(), 100), Outcome::Pending { again: 110, .. }));
        assert!(matches!(ran(&run_of(Status::Running), e.clone(), 100), Outcome::Pending { ref detail, .. } if detail.contains("running")));
        assert!(matches!(ran(&run_of(Status::Succeeded), e.clone(), 100), Outcome::Pass { ref detail, ref evidence } if detail.contains("run #7") && evidence.get("status") == Some(&json!("succeeded"))));
        assert!(matches!(ran(&run_of(Status::TimedOut), e.clone(), 100), Outcome::Fail { ref detail, .. } if detail.contains("timed out")));
        assert!(matches!(ran(&run_of(Status::Cancelled), e, 100), Outcome::Fail { .. }));
    }

    #[test]
    fn a_url_that_said_pass_or_fail_is_not_asked_again() {
        let prior = |result: &str| GateResult { gate: "check:smoke".into(), passed: result == "pass", detail: "said so".into(), evidence: json!({ "result": result }) };
        assert!(matches!(answered(Some(&prior("pass"))), Some(Outcome::Pass { .. })));
        assert!(matches!(answered(Some(&prior("fail"))), Some(Outcome::Fail { .. })));
        assert!(answered(Some(&prior("pending"))).is_none(), "asked again");
        assert!(answered(Some(&prior("unreachable"))).is_none());
        assert!(answered(None).is_none());
    }
}
