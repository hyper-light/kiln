//! Notifications (docs/ci.md W19-R06) through chat, paging and webhook channels (W11-R06).
//!
//! - **Rules** come from trunk's `.kiln/notify.yaml` and the server's `[[ci.notify]]`:
//!   `on` (events), `where` (an expression over `github` and `run`), `to` (channels) and
//!   `throttle` (at most one message per channel, event, workflow and branch in that long).
//! - **Events**: `run.failed`, `run.fixed` (the first success after failures on the same branch
//!   and workflow), `run.succeeded`, `run.cancelled`, `approval.waiting`.
//! - **Channels** are `kind:target`: an integration of that kind (one named `target`, or the
//!   first, with `target` as its channel or chat): `slack:#ci`, `discord:team-hook`,
//!   `teams:ops`, `telegram:-1001234`, `pagerduty:oncall`, `opsgenie:ops`, `webhook:audit`.
//! - **Deliveries** retry with backoff, webhooks are signed (`X-Kiln-Signature-256`, HMAC-SHA256
//!   of the body with the integration's secret), and each is recorded (`notify_deliveries`).
//! - `kiln/notify@v1` sends a message from a step (`POST /_kiln/notify`, the leg's token).

use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use super::integrations::{self, Integration};
use super::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// The events rules can name.
pub const EVENTS: &[&str] = &["run.failed", "run.fixed", "run.succeeded", "run.cancelled", "approval.waiting"];
/// The integration kinds that are channels.
pub const CHANNELS: &[&str] = &["slack", "discord", "teams", "telegram", "pagerduty", "opsgenie", "webhook"];

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Either::deserialize(d)? {
        Either::One(s) => vec![s],
        Either::Many(v) => v,
    })
}

/// A rule, as `.kiln/notify.yaml` and `[[ci.notify]]` write it.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct Rule {
    #[serde(deserialize_with = "one_or_many")]
    pub on: Vec<String>,
    #[serde(rename = "where", default)]
    pub cond: Option<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub to: Vec<String>,
    #[serde(default)]
    pub throttle: Option<String>,
}

impl Rule {
    fn check(&self) -> Result<(), String> {
        if let Some(e) = self.on.iter().find(|e| !EVENTS.contains(&e.as_str())) {
            return Err(format!("`{e}` isn't an event kiln notifies of: {}", EVENTS.join(", ")));
        }
        if self.to.is_empty() {
            return Err("a rule needs somewhere to send to (`to`)".into());
        }
        if let Some(t) = &self.throttle
            && seconds(t).is_none()
        {
            return Err(format!("throttle `{t}` isn't a duration like 10m"));
        }
        if let Some(c) = &self.cond {
            kiln_ci::expr::Condition::parse(c, &kiln_ci::expr::ParseOptions::permissive(kiln_ci::expr::Dialect::Kiln)).map_err(|e| format!("where: {}", e.message))?;
        }
        Ok(())
    }
}

/// `30s`, `10m`, `2h`, `1d`.
fn seconds(s: &str) -> Option<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = n.parse().ok()?;
    Some(match unit.trim() {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    })
}

/// `.kiln/notify.yaml`'s rules; errors name the rule.
pub fn parse_rules(yaml: &str) -> Result<Vec<Rule>, String> {
    if yaml.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rules: Vec<Rule> = serde_yaml::from_str(yaml).map_err(|e| format!(".kiln/notify.yaml: {e}"))?;
    for (i, r) in rules.iter().enumerate() {
        r.check().map_err(|e| format!(".kiln/notify.yaml, rule {}: {e}", i + 1))?;
    }
    Ok(rules)
}

/// Every rule a repository's events go through: the server's, then trunk's.
async fn rules_for(hub: &Hub, repo: &Repo) -> Vec<(String, Rule)> {
    let mut out: Vec<(String, Rule)> = hub.cfg.ci.notify.iter().enumerate().filter(|(_, r)| r.check().is_ok()).map(|(i, r)| (format!("server#{}", i + 1), r.clone())).collect();
    if let Some((_, text)) = super::repo_config::trunk_file(hub, repo, &[".kiln/notify.yaml", ".kiln/notify.yml"]).await {
        match parse_rules(&String::from_utf8_lossy(&text)) {
            Ok(rules) => out.extend(rules.into_iter().enumerate().map(|(i, r)| (format!("{}#{}", repo.name, i + 1), r))),
            Err(e) => tracing::warn!(repo = %repo.name, error = %e, "notification rules could not be read"),
        }
    }
    out
}

/// What an event is about.
#[derive(Debug, Clone, Default)]
pub struct Subject {
    pub repo: String,
    pub run: String,
    pub number: i64,
    pub workflow: String,
    pub name: String,
    pub branch: String,
    pub sha: String,
    pub message: String,
    pub actor: String,
    pub conclusion: String,
    pub url: String,
    /// The run's `github` context.
    pub github: Value,
    /// For approvals: which environments.
    pub environments: Vec<String>,
}

impl Subject {
    fn run_value(&self) -> Value {
        json!({ "repo": self.repo, "id": self.run, "number": self.number, "workflow": self.workflow, "name": self.name, "branch": self.branch, "sha": self.sha, "actor": self.actor, "conclusion": self.conclusion, "url": self.url })
    }
}

/// Whether `rule` takes `event` about `s`.
fn takes(rule: &Rule, event: &str, s: &Subject) -> bool {
    if !rule.on.iter().any(|e| e == event) {
        return false;
    }
    let Some(c) = &rule.cond else { return true };
    let Ok(cond) = kiln_ci::expr::Condition::parse(c, &kiln_ci::expr::ParseOptions::permissive(kiln_ci::expr::Dialect::Kiln)) else { return false };
    let cx = kiln_ci::expr::MapContexts::default().with("github", kiln_ci::expr::Value::from_json(&s.github)).with("run", kiln_ci::expr::Value::from_json(&s.run_value()));
    kiln_ci::expr::eval_condition(&cond, &cx, &mut kiln_ci::expr::Budget::default()).unwrap_or(false)
}

/// The message: a title, and a line with where and what.
fn message(event: &str, s: &Subject) -> (String, String) {
    // A step's own message (`kiln/notify@v1`), or Verify's: the text, then where it's from.
    if event == "message" {
        let line = if s.run.is_empty() { s.url.clone() } else { format!("{} · {} · {}", s.repo, s.branch, s.url) };
        return (s.name.clone(), line);
    }
    let what = if s.name.is_empty() { s.workflow.clone() } else { s.name.clone() };
    let title = match event {
        "run.failed" => format!("✗ {what} #{} failed on {}", s.number, s.branch),
        "run.fixed" => format!("✓ {what} #{} is fixed on {}", s.number, s.branch),
        "run.succeeded" => format!("✓ {what} #{} succeeded on {}", s.number, s.branch),
        "run.cancelled" => format!("◌ {what} #{} was cancelled on {}", s.number, s.branch),
        "approval.waiting" => format!("⏸ {what} #{} waits for approval to deploy to {}", s.number, s.environments.join(", ")),
        other => format!("{other}: {what} #{}", s.number),
    };
    let first = s.message.lines().next().unwrap_or_default();
    let short = s.sha.get(..8).unwrap_or(&s.sha);
    (title, format!("{} · {short} {first} · {} · {}", s.repo, s.actor, s.url))
}

/// Whether a message for `key` may go now, `throttle` seconds after the last; records it if so.
async fn throttled(hub: &Hub, key: &str, throttle: Option<i64>) -> bool {
    let Some(t) = throttle.filter(|t| *t > 0) else { return false };
    let (k, now) = (key.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let last: Option<i64> = c.query_row("SELECT at FROM notify_sent WHERE key = ?1", [&k], |r| r.get(0)).optional()?;
            if last.is_some_and(|l| now - l < t) {
                return Ok(true);
            }
            c.execute("INSERT INTO notify_sent (key, at) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET at = ?2", params![k, now])?;
            Ok(false)
        })
        .await
        .unwrap_or(false)
}

/// The integration a channel names for `repo`, and the target in it (a Slack channel, a chat).
async fn channel(hub: &Hub, repo: &Repo, spec: &str) -> Result<(Integration, Option<String>), String> {
    let (kind, target) = spec.split_once(':').map_or((spec, ""), |(k, t)| (k, t));
    if kind == "inbox" || kind == "email" {
        return Err(format!("`{kind}` channels aren't built yet"));
    }
    if !CHANNELS.contains(&kind) {
        return Err(format!("`{kind}` isn't a channel: {} (each an integration)", CHANNELS.join(", ")));
    }
    let all = integrations::list(hub).await.map_err(|e| format!("{e:#}"))?;
    let usable = |i: &&Integration| i.kind == kind && usable_by(i, repo);
    if let Some(i) = all.iter().filter(usable).find(|i| i.name == target) {
        return Ok((i.clone(), None));
    }
    let i = all.iter().find(usable).ok_or_else(|| format!("no {kind} integration {} may use", repo.name))?;
    Ok((i.clone(), Some(target.to_owned()).filter(|t| !t.is_empty())))
}

/// Whether `repo`'s events may go through integration `i` (its scope holds the repository).
fn usable_by(i: &Integration, repo: &Repo) -> bool {
    let owner = repo.name.split('/').next().unwrap_or_default();
    match i.scope.split_once(':') {
        None => i.scope == "server" || i.scope.is_empty(),
        Some(("org", o)) => o.eq_ignore_ascii_case(owner),
        Some(("repo", r)) => r == repo.id || r.eq_ignore_ascii_case(&repo.name),
        _ => false,
    }
}

/// Sends one message through `i`.
async fn send(hub: &Hub, i: &Integration, target: Option<&str>, event: &str, s: &Subject) -> Result<(), String> {
    let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let secret = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?;
    let (title, line) = message(event, s);
    let text = format!("{title}\n{line}");
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.is_empty());
    let need = |what: &str| secret.clone().ok_or_else(|| format!("{} has no {what} stored", i.name));
    let (url, body, bearer): (String, Value, Option<String>) = match i.kind.as_str() {
        "slack" => {
            let s = need("webhook URL or bot token")?;
            if s.starts_with("xoxb-") || s.starts_with("xoxp-") {
                let channel = target.map(str::to_owned).or_else(|| cfg("channel")).ok_or("a Slack bot needs a channel: `slack:#ci`")?;
                (integrations::endpoint("slack:api", || "https://slack.com/api/chat.postMessage".into()), json!({ "channel": channel, "text": text }), Some(s))
            } else {
                (s, json!({ "text": text }), None)
            }
        }
        "discord" => (need("webhook URL")?, json!({ "content": text }), None),
        "teams" => (
            need("workflow webhook URL")?,
            json!({ "type": "message", "attachments": [{ "contentType": "application/vnd.microsoft.card.adaptive", "content": { "type": "AdaptiveCard", "version": "1.4", "body": [{ "type": "TextBlock", "text": title, "weight": "Bolder", "wrap": true }, { "type": "TextBlock", "text": line, "wrap": true }] } }] }),
            None,
        ),
        "telegram" => {
            let chat = target.map(str::to_owned).or_else(|| cfg("chat_id")).ok_or("Telegram needs a chat: `telegram:<chat id>`")?;
            (integrations::endpoint("telegram:api", || "https://api.telegram.org".into()) + &format!("/bot{}/sendMessage", need("bot token")?), json!({ "chat_id": chat, "text": text }), None)
        }
        "pagerduty" => {
            // Failures trigger an incident a fix resolves: one per repository, workflow and branch.
            let action = if matches!(event, "run.fixed" | "run.succeeded") { "resolve" } else { "trigger" };
            let dedup = format!("kiln:{}:{}:{}", s.repo, s.workflow, s.branch);
            (integrations::endpoint("pagerduty:events", || "https://events.pagerduty.com/v2/enqueue".into()), json!({ "routing_key": need("routing key")?, "event_action": action, "dedup_key": dedup, "payload": { "summary": title, "source": "kiln", "severity": "error", "custom_details": { "line": line } }, "links": [{ "href": s.url, "text": "the run" }] }), None)
        }
        "opsgenie" => (
            integrations::endpoint("opsgenie:alerts", || "https://api.opsgenie.com/v2/alerts".into()),
            json!({ "message": title, "description": line, "alias": format!("kiln:{}:{}:{}", s.repo, s.workflow, s.branch), "source": "kiln" }),
            Some(format!("GenieKey {}", need("API key")?)),
        ),
        "webhook" => (cfg("url").ok_or("a webhook integration needs its `url`")?, json!({ "event": event, "title": title, "text": line, "run": s.run_value(), "environments": s.environments }), None),
        other => return Err(format!("`{other}` isn't a channel")),
    };
    let payload = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let mut req = client.post(&url).header("content-type", "application/json").body(payload.clone()).timeout(Duration::from_secs(20));
    if let Some(b) = bearer {
        // Opsgenie's scheme is its own; the rest are bearer tokens.
        req = if b.starts_with("GenieKey ") { req.header("authorization", &b) } else { req.bearer(&b) };
    }
    if i.kind == "webhook"
        && let Some(key) = &secret
    {
        req = req.header("x-kiln-signature-256", &format!("sha256={}", crate::util::hmac_hex(key.as_bytes(), &payload))).header("x-kiln-event", event);
    }
    let resp = req.send().await.map_err(|e| format!("{e:#}"))?;
    let status = resp.status();
    let answer = resp.bytes(64 << 10).await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", crate::util::truncate(String::from_utf8_lossy(&answer).trim(), 200)));
    }
    // Slack's API answers 200 with `ok: false`.
    if i.kind == "slack"
        && let Ok(v) = serde_json::from_slice::<Value>(&answer)
        && v.get("ok") == Some(&Value::Bool(false))
    {
        return Err(format!("Slack: {}", v.get("error").and_then(Value::as_str).unwrap_or("refused")));
    }
    Ok(())
}

/// Delivers through `spec`, retrying (now, after 2 s, after 15 s), and records how it went.
async fn deliver(hub: &Hub, repo: &Repo, rule: &str, event: &str, s: &Subject, spec: &str) {
    let mut attempts = 0;
    let result = match channel(hub, repo, spec).await {
        Err(e) => Err(e),
        Ok((i, target)) => {
            let mut last = Err(String::new());
            for wait in [0u64, 2, 15] {
                if wait > 0 {
                    crate::rt::sleep(Duration::from_secs(wait)).await;
                }
                attempts += 1;
                last = send(hub, &i, target.as_deref(), event, s).await;
                if last.is_ok() {
                    break;
                }
            }
            last
        }
    };
    if let Err(e) = &result {
        tracing::warn!(channel = spec, event, run = %s.run, error = %e, "a notification didn't go out");
    }
    let (rule, event, run, spec, now) = (rule.to_owned(), event.to_owned(), s.run.clone(), spec.to_owned(), crate::util::now_unix());
    let (status, error) = match result {
        Ok(()) => ("sent", None),
        Err(e) => ("failed", Some(e)),
    };
    let _ = hub
        .store
        .call(move |c| Ok(c.execute("INSERT INTO notify_deliveries (ts, rule, event, run_id, channel, status, error, attempts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![now, rule, event, run, spec, status, error, attempts]).map(|_| ())?))
        .await;
}

/// Sends `event` about `s` through every rule of `repo` that takes it.
pub async fn emit(hub: &Hub, repo: &Repo, event: &str, s: &Subject) {
    for (name, rule) in rules_for(hub, repo).await {
        if !takes(&rule, event, s) {
            continue;
        }
        for spec in &rule.to {
            let key = format!("{name}\u{0}{spec}\u{0}{event}\u{0}{}\u{0}{}", s.workflow, s.branch);
            if throttled(hub, &key, rule.throttle.as_deref().and_then(seconds)).await {
                continue;
            }
            deliver(hub, repo, &name, event, s, spec).await;
        }
    }
}

/// A run's facts for its events.
async fn subject(hub: &Hub, run: &str) -> anyhow::Result<Option<(Repo, Subject)>> {
    let r = run.to_owned();
    let row: Option<(String, i64, String, String, String, String, String, String, Option<String>, Option<String>)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT r.repo_id, r.number, COALESCE(r.workflow_id, ''), COALESCE(r.run_name, r.pipeline, ''), r.branch, r.sha, r.message, COALESCE(r.actor, ''), r.conclusion, p.input FROM runs r LEFT JOIN ci_plans p ON p.run_id = r.id WHERE r.id = ?1",
                [&r],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?, x.get(6)?, x.get(7)?, x.get(8)?, x.get(9)?)),
            )
            .optional()?)
        })
        .await?;
    let Some((repo_id, number, workflow, name, branch, sha, message, actor, conclusion, input)) = row else { return Ok(None) };
    let Some(repo) = hub.store.repos().await?.into_iter().find(|x| x.id == repo_id) else { return Ok(None) };
    let github = input.and_then(|i| serde_json::from_str::<Value>(&i).ok()).and_then(|v| v.get("github").cloned()).unwrap_or(Value::Null);
    let url = format!("{}/runs/{run}", hub.cfg.server.public_url.trim_end_matches('/'));
    Ok(Some((repo.clone(), Subject { repo: repo.name, run: run.to_owned(), number, workflow, name, branch, sha, message, actor, conclusion: conclusion.unwrap_or_default(), url, github, environments: Vec::new() })))
}

/// A run ended (W19-R06): `run.failed`, `run.cancelled`, or `run.succeeded` and, after failures
/// on its branch, `run.fixed`.
pub async fn run_finished(hub: Arc<Hub>, run: String) {
    let Ok(Some((repo, s))) = subject(&hub, &run).await else { return };
    let events: Vec<&str> = match s.conclusion.as_str() {
        "success" => {
            let (repo_id, wf, branch, r) = (repo.id.clone(), s.workflow.clone(), s.branch.clone(), run.clone());
            let before: Option<String> = hub
                .store
                .call(move |c| Ok(c.query_row("SELECT conclusion FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 AND id != ?4 AND finished_at IS NOT NULL ORDER BY finished_at DESC LIMIT 1", params![repo_id, wf, branch, r], |x| x.get(0)).optional()?.flatten()))
                .await
                .unwrap_or(None);
            let fixed = before.as_deref().is_some_and(|c| matches!(c, "failure" | "timed_out" | "startup_failure"));
            if fixed { vec!["run.succeeded", "run.fixed"] } else { vec!["run.succeeded"] }
        }
        "cancelled" => vec!["run.cancelled"],
        "skipped" | "neutral" | "" => Vec::new(),
        _ => vec!["run.failed"],
    };
    for e in events {
        emit(&hub, &repo, e, &s).await;
    }
}

/// A run waits for a reviewer to deploy to `environments` (W15-R03).
pub async fn approval_waiting(hub: Arc<Hub>, run: String, environments: Vec<String>) {
    let Ok(Some((repo, mut s))) = subject(&hub, &run).await else { return };
    s.environments = environments;
    emit(&hub, &repo, "approval.waiting", &s).await;
}

/// `POST /_kiln/notify` with `{ to: [channels], message }`: a step's own message
/// (`kiln/notify@v1`), through channels its repository may use.
pub async fn from_leg(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Err((StatusCode::UNAUTHORIZED, "a leg's runtime token is needed".into())) };
    let body: Value = srv::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let to: Vec<String> = match body.get("to") {
        Some(Value::String(s)) => s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    let text = body.get("message").and_then(Value::as_str).unwrap_or_default().to_owned();
    if to.is_empty() || text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "say `to` (channels) and `message`".into()));
    }
    let Some((repo, mut s)) = subject(hub, &claims.run).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? else { return Err((StatusCode::NOT_FOUND, "the run is gone".into())) };
    s.name = text;
    s.number = 0;
    let mut sent = Vec::new();
    for spec in &to {
        let r = match channel(hub, &repo, spec).await {
            Ok((i, target)) => send(hub, &i, target.as_deref(), "message", &s).await,
            Err(e) => Err(e),
        };
        sent.push(json!({ "to": spec, "ok": r.is_ok(), "error": r.err() }));
    }
    Ok(srv::json(StatusCode::OK, &json!({ "sent": sent })))
}

/// Integration Verify for a channel: a test message.
pub async fn verify(hub: &Hub, i: &Integration) -> Result<String, String> {
    let s = Subject { name: "kiln: a test message; this channel works".into(), url: hub.cfg.server.public_url.clone(), ..Subject::default() };
    send(hub, i, None, "message", &s).await.map(|()| format!("sent a test message through {}", i.name))
}

/// `GET /api/ci/notify/deliveries?run=&limit=`: what went out, newest first.
pub async fn deliveries(hub: &Hub, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let run = q.get("run").cloned();
    let limit: i64 = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).clamp(1, 1000);
    let rows: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT ts, rule, event, run_id, channel, status, error, attempts FROM notify_deliveries WHERE (?1 IS NULL OR run_id = ?1) ORDER BY id DESC LIMIT ?2")?;
            let rows = st.query_map(params![run, limit], |x| Ok(json!({ "ts": x.get::<_, i64>(0)?, "rule": x.get::<_, String>(1)?, "event": x.get::<_, String>(2)?, "run": x.get::<_, Option<String>>(3)?, "channel": x.get::<_, String>(4)?, "status": x.get::<_, String>(5)?, "error": x.get::<_, Option<String>>(6)?, "attempts": x.get::<_, i64>(7)? })))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &json!({ "deliveries": rows })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> Subject {
        Subject { repo: "acme/app".into(), run: "r1".into(), number: 7, workflow: ".github/workflows/ci.yml".into(), name: "CI".into(), branch: "main".into(), sha: "0123456789abcdef".into(), message: "fix the parser\n\nlong".into(), actor: "ada".into(), conclusion: "failure".into(), url: "https://kiln.example/runs/r1".into(), github: json!({ "ref_name": "main", "event_name": "push" }), environments: Vec::new() }
    }

    /// W19-AC05's rules, without the network: events and `where` pick rules; bad rules are refused.
    #[test]
    fn w19_ac05_rules() {
        let rules = parse_rules("- on: [run.failed, run.fixed]\n  where: \"github.ref_name == 'main'\"\n  to: [slack:#ci, webhook:audit]\n  throttle: 10m\n- on: approval.waiting\n  to: discord:team-hook\n").unwrap_or_default();
        assert_eq!(rules.len(), 2);
        let s = subject();
        let (first, second) = (rules.first().cloned().unwrap_or_else(|| Rule { on: vec![], cond: None, to: vec![], throttle: None }), rules.get(1).map(|r| r.to.clone()).unwrap_or_default());
        assert!(takes(&first, "run.failed", &s) && takes(&first, "run.fixed", &s));
        assert!(!takes(&first, "run.succeeded", &s), "not an event it names");
        let other = Subject { github: json!({ "ref_name": "feature" }), ..subject() };
        assert!(!takes(&first, "run.failed", &other), "where: only main");
        assert_eq!(second, vec!["discord:team-hook".to_owned()], "one channel without a list");
        assert_eq!(seconds("10m"), Some(600));
        assert!(parse_rules("- on: run.exploded\n  to: [slack:#x]\n").is_err());
        assert!(parse_rules("- on: run.failed\n  to: []\n").is_err());
        assert!(parse_rules("- on: run.failed\n  to: [slack:#x]\n  throttle: soon\n").is_err());
        assert!(parse_rules("- on: run.failed\n  to: [slack:#x]\n  where: \"github.ref ==\"\n").is_err());
        let (title, line) = message("run.failed", &s);
        assert_eq!(title, "✗ CI #7 failed on main");
        assert_eq!(line, "acme/app · 01234567 fix the parser · ada · https://kiln.example/runs/r1");
        assert_eq!(message("run.fixed", &s).0, "✓ CI #7 is fixed on main");
    }
}
