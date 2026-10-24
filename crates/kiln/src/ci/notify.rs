//! Notifications (docs/ci.md W19-R06) through chat, paging and webhook channels (W11-R06).
//!
//! - **Rules** come from trunk's `.kiln/notify.yaml` and the server's `[[ci.notify]]`:
//!   `on` (events), `where` (an expression over `github` and `run`), `to` (channels) and
//!   `throttle` (at most one message per channel, event, workflow and branch in that long).
//! - **Events**: `run.failed`, `run.fixed` (the first success after failures on the same branch
//!   and workflow), `run.succeeded`, `run.cancelled`, `approval.waiting`; and an agent's
//!   (docs/agentic.md N2): `agent.needs_you`, `agent.finished`, `agent.stopped`, `agent.paused`,
//!   which only the server's rules take (an agent is no repository's), `where` reading `agent`;
//!   and a release's (docs/deploy-api.md §12.3): `release.started`, `release.waiting_approval`,
//!   `release.succeeded`, `release.failed`, `release.cancelled`, which the server's rules take
//!   (`where` reading `release`) and which go to the app's own channels too (its
//!   `contacts.channels`): where a channel can change a message afterwards, as one message a
//!   release has there, edited as it moves (`crate::ci::chat`); for people,
//!   about a diff or change request: `review.requested`, `comment.reply`, `mention`; and
//!   `quota.near` (a repository's cache past 90 % of its quota) and `secret.unused` (no run read
//!   it in 90 days); `diff.landed` (a diff on its trunk) and `test.quarantined` (a test the
//!   trunk's `.kiln/quarantine.yaml` took in).
//! - **Channels** are `kind:target`: an integration of that kind (one named `target`, or the
//!   first, with `target` as its channel, chat or address): `slack:#ci`, `discord:team-hook`,
//!   `teams:ops`, `telegram:-1001234`, `pagerduty:oncall`, `opsgenie:ops`, `webhook:audit`,
//!   `email:ada@example.com`; kiln's own `inbox` (the people an event is about: a run's actor,
//!   the reviewers asked, a thread's people, whom a comment mentions) or `inbox:ada`; and issue
//!   trackers, `linear` and `jira` (`linear:ENG`: the team its new issues go in), which comment
//!   on the issues a commit's trailers name (`crate::ci::trackers`).
//! - **Deliveries** a tracker had nothing to do with (no issue of its named) are `skipped`.
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
pub const EVENTS: &[&str] = &[
    "run.failed",
    "run.fixed",
    "run.succeeded",
    "run.cancelled",
    "approval.waiting",
    "deploy.succeeded",
    "deploy.failed",
    "deploy.held",
    "deploy.rolled_back",
    "deploy.rollback_failed",
    "leg.flaky",
    "land.ejected",
    "runner.offline",
    "agent.needs_you",
    "agent.finished",
    "agent.stopped",
    "agent.paused",
    "release.started",
    "release.waiting_approval",
    "release.succeeded",
    "release.failed",
    "release.cancelled",
    "review.requested",
    "comment.reply",
    "mention",
    "quota.near",
    "secret.unused",
    "diff.landed",
    "test.quarantined",
];
/// The events about an agent (`crate::push`): the server's rules take them, a repository's can't.
pub const AGENT_EVENTS: &[&str] = &["agent.needs_you", "agent.finished", "agent.stopped", "agent.paused"];
/// The events about a release deployed by API (`crate::deploy`): the server's rules take them, a
/// repository's can't, and each goes to the app's own channels as well.
pub const RELEASE_EVENTS: &[&str] = &["release.started", "release.waiting_approval", "release.succeeded", "release.failed", "release.cancelled"];
/// The events about a runner: the server's rules take them, a repository's can't.
pub const RUNNER_EVENTS: &[&str] = &["runner.offline"];
/// Events about a deploy of kiln itself (docs/resilience.md, G7): the server's rules take them.
pub const SELF_DEPLOY_EVENTS: &[&str] = &["deploy.held", "deploy.rolled_back", "deploy.rollback_failed"];
/// How long a runner is gone before `runner.offline` is said of it.
const OFFLINE_AFTER: Duration = Duration::from_secs(120);
/// How long a release's event, once sent through a channel, isn't sent through it again.
const RELEASE_ONCE: i64 = 90 * 86_400;
/// How long a test quarantined at a commit, once told of, isn't told of again (a push seen twice).
const QUARANTINE_ONCE: i64 = 90 * 86_400;
/// The integration kinds that are channels.
pub const CHANNELS: &[&str] = &["slack", "discord", "teams", "telegram", "pagerduty", "opsgenie", "webhook", "email", "linear", "jira"];
/// How long a secret goes unread before `secret.unused` is said of it, and said again.
const UNUSED_AFTER: i64 = 90 * 86_400;
/// A repository's cache this full is `quota.near` (in hundredths); under the second, it may be said
/// again.
const NEAR: (u64, u64) = (90, 80);

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
    /// Whether it's a rule kiln can follow: `server` for the server's own (`[[ci.notify]]`), which
    /// alone may take agents' events.
    fn check(&self, server: bool) -> Result<(), String> {
        if let Some(e) = self.on.iter().find(|e| !EVENTS.contains(&e.as_str())) {
            return Err(format!("`{e}` isn't an event kiln notifies of: {}", EVENTS.join(", ")));
        }
        if !server && let Some(e) = self.on.iter().find(|e| AGENT_EVENTS.contains(&e.as_str())) {
            return Err(format!("`{e}` is about an agent, not a repository: the server's rules take it (`[[ci.notify]]` in kiln.toml)"));
        }
        if !server && let Some(e) = self.on.iter().find(|e| RUNNER_EVENTS.contains(&e.as_str())) {
            return Err(format!("`{e}` is about a runner, not a repository: the server's rules take it (`[[ci.notify]]` in kiln.toml)"));
        }
        if !server && let Some(e) = self.on.iter().find(|e| SELF_DEPLOY_EVENTS.contains(&e.as_str())) {
            return Err(format!("`{e}` is about a deploy of kiln itself, not a repository: the server's rules take it (`[[ci.notify]]` in kiln.toml)"));
        }
        if !server && let Some(e) = self.on.iter().find(|e| RELEASE_EVENTS.contains(&e.as_str())) {
            return Err(format!("`{e}` is about a release, not a repository: the server's rules take it (`[[ci.notify]]` in kiln.toml), and an app's own channels"));
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

/// A person's own rule (W19-R06): a repository's events, to their `inbox` or their `email`.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct Mine {
    #[serde(flatten)]
    pub rule: Rule,
    /// The repositories it's for, by name or glob (`acme/*`); every one they may read when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// Only what's about them: runs they started, reviews they're asked for, replies to them,
    /// mentions of them.
    #[serde(default)]
    pub about_me: bool,
}

impl Mine {
    fn check(&self) -> Result<(), String> {
        self.rule.check(false)?;
        if let Some(t) = self.rule.to.iter().find(|t| !matches!(t.as_str(), "inbox" | "email")) {
            return Err(format!("`{t}` isn't where a person's own rule goes: their `inbox`, or their `email`"));
        }
        if self.repos.iter().any(|r| r.trim().is_empty()) {
            return Err("`repos` names repositories (or globs, `acme/*`)".into());
        }
        Ok(())
    }
}

/// The most rules a person keeps.
const MINE_MAX: usize = 50;

/// `GET /api/me/notifications`: the caller's own rules (W19-R06) and the address `email` sends to;
/// `PUT` `{ rules, email? }` replaces them (and that address). A person's: the owner's are the
/// server's (`[[ci.notify]]`), and an agent doesn't change its person's.
pub async fn mine(hub: &Hub, caller: &crate::authz::Caller, put: Option<Value>) -> Result<Response, Fail> {
    let events: Vec<&str> = EVENTS.iter().copied().filter(|e| !AGENT_EVENTS.contains(e) && !RUNNER_EVENTS.contains(e) && !RELEASE_EVENTS.contains(e) && !SELF_DEPLOY_EVENTS.contains(e)).collect();
    let Some(uid) = caller.user.clone() else {
        if put.is_some() {
            return Err((StatusCode::BAD_REQUEST, "notification rules are a person's own: the owner's are the server's ([[ci.notify]] in kiln.toml)".into()));
        }
        return Ok(srv::json(StatusCode::OK, &json!({ "person": false, "rules": [], "email": null, "events": events })));
    };
    if let Some(body) = put {
        if caller.kind == crate::authz::gate::Kind::Agent {
            return Err((StatusCode::FORBIDDEN, "an agent doesn't change its person's notifications".into()));
        }
        let rules: Vec<Mine> = serde_json::from_value(body.get("rules").cloned().unwrap_or_else(|| json!([]))).map_err(|e| (StatusCode::BAD_REQUEST, format!("rules: {e}")))?;
        if rules.len() > MINE_MAX {
            return Err((StatusCode::BAD_REQUEST, format!("at most {MINE_MAX} rules")));
        }
        for (i, r) in rules.iter().enumerate() {
            r.check().map_err(|e| (StatusCode::BAD_REQUEST, format!("rule {}: {e}", i + 1)))?;
        }
        let email: Option<Option<String>> = match body.get("email") {
            Some(Value::String(e)) if e.trim().is_empty() => Some(None),
            Some(Value::String(e)) if super::smtp::valid_address(e.trim()) => Some(Some(e.trim().to_owned())),
            Some(Value::String(e)) => return Err((StatusCode::BAD_REQUEST, format!("`{e}` isn't an email address"))),
            Some(Value::Null) => Some(None),
            _ => None,
        };
        let (id, text) = (uid.clone(), serde_json::to_string(&rules).unwrap_or_else(|_| "[]".into()));
        hub.store
            .call(move |c| {
                let tx = crate::store::tx(c)?;
                if rules.is_empty() {
                    tx.execute("DELETE FROM notify_subscriptions WHERE user_id = ?1", [&id])?;
                } else {
                    tx.execute("INSERT INTO notify_subscriptions (user_id, rules, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT (user_id) DO UPDATE SET rules = ?2, updated_at = ?3", params![id, text, crate::util::now_unix()])?;
                }
                if let Some(e) = email {
                    tx.execute("UPDATE users SET email = ?2 WHERE id = ?1", params![id, e])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    }
    let id = uid.clone();
    let (rules, email): (Option<String>, Option<String>) = hub
        .store
        .call(move |c| {
            let rules = c.query_row("SELECT rules FROM notify_subscriptions WHERE user_id = ?1", [&id], |r| r.get(0)).optional()?;
            let email = c.query_row("SELECT email FROM users WHERE id = ?1", [&id], |r| r.get::<_, Option<String>>(0)).optional()?.flatten();
            Ok((rules, email))
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let rules: Vec<Mine> = rules.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    Ok(srv::json(StatusCode::OK, &json!({ "person": true, "rules": rules, "email": email, "events": events })))
}

/// Every person's own rules, with who they are: their id, name and address.
async fn subscribers(hub: &Hub) -> Vec<(String, String, Option<String>, Vec<Mine>)> {
    hub.store
        .call(|c| {
            let mut st = c.prepare("SELECT u.id, u.name, u.email, s.rules FROM notify_subscriptions s JOIN users u ON u.id = s.user_id WHERE u.disabled_at IS NULL")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?)))?;
            Ok(rows.filter_map(Result::ok).map(|(id, name, email, rules)| (id, name, email, serde_json::from_str(&rules).unwrap_or_default())).collect())
        })
        .await
        .unwrap_or_default()
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
        r.check(false).map_err(|e| format!(".kiln/notify.yaml, rule {}: {e}", i + 1))?;
    }
    Ok(rules)
}

/// Every rule a repository's events go through: the server's, then trunk's.
async fn rules_for(hub: &Hub, repo: &Repo) -> Vec<(String, Rule)> {
    let mut out: Vec<(String, Rule)> = hub.cfg.ci.notify.iter().enumerate().filter(|(_, r)| r.check(true).is_ok()).map(|(i, r)| (format!("server#{}", i + 1), r.clone())).collect();
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
    /// Whom it's about, for the `inbox` (a run's actor when none).
    pub people: Vec<String>,
    /// The review thread it's about (`comment.reply`, `mention`): what a reply to it answers
    /// (docs/ci.md W14-R05).
    pub thread: Option<String>,
}

impl Subject {
    fn run_value(&self) -> Value {
        json!({ "repo": self.repo, "id": self.run, "number": self.number, "workflow": self.workflow, "name": self.name, "branch": self.branch, "sha": self.sha, "message": self.message, "actor": self.actor, "conclusion": self.conclusion, "url": self.url })
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
    // A diff's or change request's, for people (`name` is it and its title, `message` what was
    // said), the cache's and a secret's.
    // People by name: `user:ada` is ada.
    let shown = |p: &str| p.strip_prefix("user:").unwrap_or(p).to_owned();
    let (actor, people) = (shown(&s.actor), s.people.iter().map(|p| shown(p)).collect::<Vec<_>>().join(", "));
    match event {
        "review.requested" => return (format!("👀 {actor} asks for a review of {}", s.name), format!("{} · for {people} · {}", s.repo, s.url)),
        "comment.reply" => return (format!("💬 {actor} replied on {}", s.name), format!("{} · {} · {}", s.repo, s.message.lines().next().unwrap_or_default(), s.url)),
        "mention" => return (format!("@ {actor} mentioned {people} on {}", s.name), format!("{} · {} · {}", s.repo, s.message.lines().next().unwrap_or_default(), s.url)),
        "quota.near" => return (format!("⚠ {}'s cache is near its quota", s.repo), format!("{} · {}", s.message, s.url)),
        "secret.unused" => return (format!("⚠ secret {} hasn't been used in {} days", s.name, UNUSED_AFTER / 86_400), format!("{} · {} · {}", s.repo, s.message, s.url)),
        "diff.landed" => return (format!("✓ {} landed on {}", s.name, s.branch), format!("{} · {} by {actor} · {}", s.repo, s.sha.get(..8).unwrap_or(&s.sha), s.url)),
        "test.quarantined" => return (format!("⊘ {} is quarantined in {}", s.name, s.repo), format!("{} · {}", s.message.lines().next().unwrap_or_default(), s.url)),
        _ => {}
    }
    // A land queue's entry: its diffs, why, and where they are.
    if event == "land.ejected" {
        return (format!("⏏ {} left the land queue of {}", s.name, s.branch), format!("{} · {} · {}", s.repo, s.message, s.url));
    }
    let what = if s.name.is_empty() { s.workflow.clone() } else { s.name.clone() };
    let title = match event {
        "run.failed" => format!("✗ {what} #{} failed on {}", s.number, s.branch),
        "run.fixed" => format!("✓ {what} #{} is fixed on {}", s.number, s.branch),
        "run.succeeded" => format!("✓ {what} #{} succeeded on {}", s.number, s.branch),
        "run.cancelled" => format!("◌ {what} #{} was cancelled on {}", s.number, s.branch),
        "approval.waiting" if s.environments.is_empty() => format!("⏸ {what} #{} waits for a maintainer's approval to run (an outside contributor's)", s.number),
        "approval.waiting" => format!("⏸ {what} #{} waits for approval to deploy to {}", s.number, s.environments.join(", ")),
        "deploy.succeeded" => format!("✓ {what} #{} deployed to {}", s.number, s.environments.join(", ")),
        "deploy.failed" => format!("✗ {what} #{} failed to deploy to {}", s.number, s.environments.join(", ")),
        "leg.flaky" => format!("≈ {what} #{} had flaky tests on {}", s.number, s.branch),
        other => format!("{other}: {what} #{}", s.number),
    };
    let first = s.message.lines().next().unwrap_or_default();
    let short = s.sha.get(..8).unwrap_or(&s.sha);
    (title, format!("{} · {short} {first} · {} · {}", s.repo, s.actor, s.url))
}

/// What a channel is sent: the message, and what some channels need besides.
struct Msg {
    title: String,
    line: String,
    url: String,
    /// What the link is to ("the run", "the agent").
    what: &'static str,
    /// One incident per thing it's about (PagerDuty, Opsgenie).
    dedup: String,
    /// It ends that incident.
    resolve: bool,
    /// A webhook channel's body.
    webhook: Value,
    /// Whom it's about: whose inboxes `inbox` puts it in.
    people: Vec<String>,
    /// The issues it names (its commit's trailers): what an issue tracker comments on.
    refs: Vec<String>,
    /// The review thread it's about: email and Slack messages take replies to it (W14-R05).
    thread: Option<String>,
}

impl Msg {
    fn of_run(event: &str, s: &Subject) -> Msg {
        let (title, line) = message(event, s);
        let webhook = json!({ "event": event, "title": title, "text": line, "run": s.run_value(), "environments": s.environments });
        // A deployment's incident is the environment's; a run's, its workflow's on its branch.
        let dedup = if event.starts_with("deploy.") { format!("kiln:{}:deploy:{}", s.repo, s.environments.join(",")) } else { format!("kiln:{}:{}:{}", s.repo, s.workflow, s.branch) };
        let what = match event {
            "land.ejected" | "review.requested" | "comment.reply" | "mention" | "diff.landed" => "the diff",
            "quota.near" => "the cache",
            "test.quarantined" => "the tests",
            "secret.unused" => "the secrets",
            _ => "the run",
        };
        let people = if s.people.is_empty() { vec![s.actor.clone()] } else { s.people.clone() };
        let people = people.into_iter().filter(|p| !p.is_empty()).collect();
        Msg {
            title,
            line,
            url: s.url.clone(),
            what,
            dedup,
            resolve: matches!(event, "run.fixed" | "run.succeeded" | "deploy.succeeded"),
            webhook,
            people,
            refs: super::trackers::references(&s.message),
            thread: s.thread.clone(),
        }
    }

    /// An agent's event, as `crate::push` sends it: `{type, agent: {id, name, machine, …}, title, text, url}`.
    fn of_agent(event: &str, ev: &Value) -> Msg {
        let get = |p: &str| ev.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (id, name, machine, text, url) = (get("/agent/id"), get("/agent/name"), get("/agent/machine"), get("/text"), get("/url"));
        let title = match event {
            "agent.needs_you" => format!("⏸ {name} needs you"),
            "agent.finished" => format!("✓ {name} finished"),
            "agent.stopped" => format!("✗ {name} stopped"),
            "agent.paused" => format!("◌ {name} paused"),
            other => format!("{other}: {name}"),
        };
        let line = format!("{text} · {machine} · {url}");
        let webhook = json!({ "event": event, "title": title, "text": line, "agent": ev.get("agent").cloned().unwrap_or(Value::Null), "url": url });
        Msg { title, line, url, what: "the agent", dedup: format!("kiln:agent:{id}"), resolve: event == "agent.finished", webhook, people: Vec::new(), refs: Vec::new(), thread: None }
    }
}

impl Msg {
    /// A release's event: `release` as `/api/deploy/releases/{id}` answers it, with the people who
    /// may approve it (`approvers`) when it waits for them. `event` may be the state it's in now
    /// (`release.committing`, `release.rolled_back`): what a message it has says as it moves.
    fn of_release(event: &str, r: &Value, url: &str) -> Msg {
        let get = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (app, env, version, by, message) = (get("app"), get("environment"), get("version"), get("requested_by"), get("message"));
        let id = r.get("id").and_then(Value::as_i64).unwrap_or_default();
        let what = match get("kind").as_str() {
            "deploy" => version,
            "rollback" => format!("a rollback to {version}"),
            "config" => "new values".to_owned(),
            "enable" => "enabling".to_owned(),
            "disable" => "disabling".to_owned(),
            other => other.to_owned(),
        };
        let title = match event {
            "release.started" => format!("▸ {app} {what} → {env} is on its way"),
            "release.waiting" => format!("◷ {app} {what} → {env} waits"),
            "release.committing" => format!("▸ {app} {what} → {env} is being committed"),
            "release.delivering" => format!("▸ {app} {what} → {env} is being delivered"),
            "release.waiting_approval" => format!("⏸ {app} {what} → {env} waits for approval"),
            "release.succeeded" => format!("✓ {app} {what} landed in {env}"),
            "release.failed" => format!("✗ {app} {what} failed in {env}"),
            "release.cancelled" => format!("◌ {app} {what} → {env} was cancelled"),
            "release.rolled_back" => format!("↩ {app} {what} in {env} was rolled back"),
            "release.superseded" => format!("✓ {app} {what} landed in {env}, replaced since"),
            other => format!("{other}: {app} → {env}"),
        };
        let approvers: Vec<&str> = r.get("approvers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let mut parts = vec![format!("release {id}, asked by {by}")];
        if event == "release.waiting_approval" && !approvers.is_empty() {
            parts.push(format!("approvers: {}", approvers.join(", ")));
        }
        // Each person's answer so far.
        for a in r.get("approvals").and_then(Value::as_array).into_iter().flatten() {
            let who = crate::deploy::gates::person(a.get("by").and_then(Value::as_str).unwrap_or_default()).to_owned();
            parts.push(if a.get("approved") == Some(&Value::Bool(false)) { format!("rejected by {who}") } else { format!("approved by {who}") });
        }
        if let Some(first) = message.lines().next().filter(|l| !l.is_empty()) {
            parts.push(first.to_owned());
        }
        parts.push(url.to_owned());
        let line = parts.join(" · ");
        let webhook = json!({ "event": event, "title": title, "text": line, "release": r, "url": url });
        // Its approvers while it waits for them; else whoever asked for it.
        let people = if event == "release.waiting_approval" { approvers.iter().map(|a| (*a).to_owned()).collect() } else { vec![by.clone()] };
        Msg {
            title,
            line,
            url: url.to_owned(),
            what: "the release",
            dedup: format!("kiln:release:{app}:{env}"),
            resolve: event == "release.succeeded",
            webhook,
            people,
            refs: Vec::new(),
            thread: None,
        }
    }
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

/// The integration a channel names for `repo` (`None`: for the server, as an agent's events
/// are), and the target in it (a Slack channel, a chat).
async fn channel(hub: &Hub, repo: Option<&Repo>, spec: &str) -> Result<(Integration, Option<String>), String> {
    let (kind, target) = spec.split_once(':').map_or((spec, ""), |(k, t)| (k, t));
    if kind == "inbox" {
        return Err("the inbox is kiln's own, not an integration".into());
    }
    if !CHANNELS.contains(&kind) {
        return Err(format!("`{kind}` isn't a channel: {} (each an integration)", CHANNELS.join(", ")));
    }
    let all = integrations::list(hub).await.map_err(|e| format!("{e:#}"))?;
    let usable = |i: &&Integration| i.kind == kind && repo.map_or(i.scope == "server" || i.scope.is_empty(), |r| usable_by(i, r));
    if let Some(i) = all.iter().filter(usable).find(|i| i.name == target) {
        return Ok((i.clone(), None));
    }
    let who = repo.map_or_else(|| "the server".to_owned(), |r| r.name.clone());
    let i = all.iter().find(usable).ok_or_else(|| format!("no {kind} integration {who} may use"))?;
    Ok((i.clone(), Some(target.to_owned()).filter(|t| !t.is_empty())))
}

/// Whether `repo`'s events may go through integration `i` (its scope holds the repository).
fn usable_by(i: &Integration, repo: &Repo) -> bool {
    integrations::usable_by(i, repo)
}

/// What a channel did with a message: took it, or had nothing to do with it (why).
enum Went {
    Sent,
    Skipped(String),
}

/// Sends one message through `i`.
async fn send(hub: &Hub, i: &Integration, target: Option<&str>, event: &str, m: &Msg) -> Result<Went, String> {
    // An issue tracker's: comments on the issues it names, or nothing.
    if super::trackers::KINDS.contains(&i.kind.as_str()) {
        let note = super::trackers::Note { event, title: &m.title, line: &m.line, url: &m.url, refs: &m.refs };
        return Ok(match Box::pin(super::trackers::deliver(hub, i, target, &note)).await? {
            Some(_) => Went::Sent,
            None => Went::Skipped(if m.refs.is_empty() { "it names no issue".to_owned() } else { format!("it names no issue of {}'s ({})", i.name, m.refs.join(", ")) }),
        });
    }
    let secret = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?;
    if i.kind == "email" {
        return send_email(hub, i, target, secret, m).await.map(|()| Went::Sent);
    }
    let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let (title, line) = (m.title.clone(), m.line.clone());
    let text = format!("{title}\n{line}");
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.is_empty());
    let need = |what: &str| secret.clone().ok_or_else(|| format!("{} has no {what} stored", i.name));
    let (url, body, bearer): (String, Value, Option<String>) = match i.kind.as_str() {
        "slack" => {
            let s = need("webhook URL or bot token")?;
            if s.starts_with("xoxb-") || s.starts_with("xoxp-") {
                let channel = target.map(str::to_owned).or_else(|| cfg("channel")).ok_or("a Slack bot needs a channel: `slack:#ci`")?;
                (integrations::endpoint("slack:api", || format!("{}/chat.postMessage", super::replies::slack_api(i))), json!({ "channel": channel, "text": text }), Some(s))
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
            // Failures trigger an incident a fix resolves: one per repository, workflow and branch
            // (an agent's: one per agent, which finishing resolves).
            let action = if m.resolve { "resolve" } else { "trigger" };
            (integrations::endpoint("pagerduty:events", || "https://events.pagerduty.com/v2/enqueue".into()), json!({ "routing_key": need("routing key")?, "event_action": action, "dedup_key": m.dedup, "payload": { "summary": title, "source": "kiln", "severity": "error", "custom_details": { "line": line } }, "links": [{ "href": m.url, "text": m.what }] }), None)
        }
        "opsgenie" => (
            integrations::endpoint("opsgenie:alerts", || "https://api.opsgenie.com/v2/alerts".into()),
            json!({ "message": title, "description": line, "alias": m.dedup, "source": "kiln" }),
            Some(format!("GenieKey {}", need("API key")?)),
        ),
        "webhook" => (cfg("url").ok_or("a webhook integration needs its `url`")?, m.webhook.clone(), None),
        other => return Err(format!("`{other}` isn't a channel")),
    };
    let payload = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let mut req = client.post(&url).header("content-type", "application/json").body(payload.clone()).timeout(Duration::from_secs(20));
    // A URL someone gave kiln (a webhook's, not a service's own API): never this machine or a
    // private network unless allowed (W22-AC07).
    if matches!(i.kind.as_str(), "discord" | "teams" | "webhook") || (i.kind == "slack" && bearer.is_none()) {
        req = req.reaching(&hub.cfg.ci.reach());
    }
    let bot = bearer.is_some();
    if let Some(b) = bearer {
        // Opsgenie's scheme is its own; the rest are bearer tokens.
        req = if b.starts_with("GenieKey ") { req.header("authorization", &b) } else { req.bearer(&b) };
    }
    if i.kind == "webhook"
        && let Some(key) = &secret
    {
        req = req.header("x-kiln-signature-256", &format!("sha256={}", crate::util::hmac_hex(key.as_bytes(), &payload)));
    }
    if i.kind == "webhook" {
        req = req.header("x-kiln-event", event).header("x-kiln-delivery", &crate::util::new_id());
    }
    // On the heap: the request's future (the reach check, TLS) is deep.
    let resp = Box::pin(req.send()).await.map_err(|e| format!("{e:#}"))?;
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
    // A bot's message about a review thread: remembered, so a reply in its Slack thread finds the
    // review thread (W14-R05).
    if i.kind == "slack"
        && bot
        && let Some(thread) = &m.thread
        && let Ok(v) = serde_json::from_slice::<Value>(&answer)
        && let (Some(channel), Some(ts)) = (v.get("channel").and_then(Value::as_str), v.get("ts").and_then(Value::as_str))
    {
        super::replies::chat_posted(hub, &i.id, channel, ts, thread).await;
    }
    Ok(Went::Sent)
}

/// An email (W11-R06): through the integration's SMTP server, to `target` (addresses,
/// comma-separated) or its `to`.
async fn send_email(hub: &Hub, i: &Integration, target: Option<&str>, password: Option<String>, m: &Msg) -> Result<(), String> {
    let cfg = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.is_empty());
    let tls = super::smtp::Tls::parse(&cfg("tls").unwrap_or_default())?;
    let host = cfg("host").ok_or_else(|| format!("{} has no `host`", i.name))?;
    let port = match cfg("port") {
        Some(p) => p.parse::<u16>().map_err(|_| format!("`port: {p}` isn't a port"))?,
        None => tls.default_port(),
    };
    let from = cfg("from").ok_or_else(|| format!("{} has no `from`", i.name))?;
    let to: Vec<String> = target.map(str::to_owned).or_else(|| cfg("to")).ok_or("email needs an address: `email:ada@example.com`, or the integration's `to`")?.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect();
    let server = super::smtp::Server { host, port, tls, user: cfg("user"), password, reach: Some(hub.cfg.ci.reach()) };
    let body = format!("{}\n\n{}\n", m.line, m.url);
    // About a review thread, with a mailbox for replies (W14-R05): each address gets one of its
    // own, with a reply address only it has.
    if let (Some(thread), Some(replies)) = (&m.thread, cfg("replies")) {
        for address in &to {
            let token = super::replies::token_for(hub, thread, super::smtp::address(address)).await.map_err(|e| format!("{e:#}"))?;
            let message = super::smtp::Message {
                from: from.clone(),
                to: vec![address.clone()],
                subject: m.title.clone(),
                body: format!("{body}\nReply to this email to answer in the thread.\n"),
                reply_to: super::replies::reply_to(&replies, &token),
            };
            super::smtp::send(&server, &message).await.map_err(|e| format!("{e:#}"))?;
        }
        return Ok(());
    }
    let message = super::smtp::Message { from, to, subject: m.title.clone(), body, reply_to: None };
    super::smtp::send(&server, &message).await.map_err(|e| format!("{e:#}"))
}

/// Delivers through `spec`, retrying (now, after 2 s, after 15 s), and records how it went (with
/// what it's about: a run's id, or `agent:<id>`). The inbox takes it at once.
async fn deliver(hub: &Hub, repo: Option<&Repo>, rule: &str, event: &str, about: &str, m: &Msg, spec: &str) {
    let mut attempts = 0;
    let inbox = spec == "inbox" || spec.starts_with("inbox:");
    let result = match channel(hub, repo, spec).await {
        Err(_) if inbox => {
            attempts = 1;
            let people: Vec<String> = match spec.strip_prefix("inbox:") {
                Some(named) => named.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect(),
                None => m.people.clone(),
            };
            match super::inbox::put(hub, &people, event, &m.title, &m.line, &m.url, repo.map(|r| r.name.as_str())).await {
                Ok(0) => Err("it's about nobody with an inbox".to_owned()),
                Ok(_) => Ok(Went::Sent),
                Err(e) => Err(format!("{e:#}")),
            }
        }
        Err(e) => Err(e),
        Ok((i, target)) => {
            // A tracker retries each of its changes itself: again here would repeat its comments.
            let waits: &[u64] = if super::trackers::KINDS.contains(&i.kind.as_str()) { &[0] } else { &[0, 2, 15] };
            let mut last = Err(String::new());
            for &wait in waits {
                if wait > 0 {
                    crate::rt::sleep(Duration::from_secs(wait)).await;
                }
                attempts += 1;
                last = send(hub, &i, target.as_deref(), event, m).await;
                if last.is_ok() {
                    break;
                }
            }
            last
        }
    };
    if let Err(e) = &result {
        tracing::warn!(channel = spec, event, about, error = %e, "a notification didn't go out");
    }
    let (rule, event, run, spec, now) = (rule.to_owned(), event.to_owned(), about.to_owned(), spec.to_owned(), crate::util::now_unix());
    let (status, error) = match result {
        Ok(Went::Sent) => ("sent", None),
        Ok(Went::Skipped(why)) => ("skipped", Some(why)),
        Err(e) => ("failed", Some(e)),
    };
    let _ = hub
        .store
        .call(move |c| Ok(c.execute("INSERT INTO notify_deliveries (ts, rule, event, run_id, channel, status, error, attempts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![now, rule, event, run, spec, status, error, attempts]).map(|_| ())?))
        .await;
}

/// Sends `event` about `s` through every rule of `repo` that takes it.
pub async fn emit(hub: &Hub, repo: &Repo, event: &str, s: &Subject) {
    // For outgoing webhooks' subscribers too (W19-R07), and the UI.
    hub.emit(&json!({ "type": event, "repo": { "id": repo.id, "name": repo.name }, "run": s.run_value(), "environments": s.environments }));
    let m = Msg::of_run(event, s);
    for (name, rule) in rules_for(hub, repo).await {
        if !takes(&rule, event, s) {
            continue;
        }
        for spec in &rule.to {
            let key = format!("{name}\u{0}{spec}\u{0}{event}\u{0}{}\u{0}{}", s.workflow, s.branch);
            if throttled(hub, &key, rule.throttle.as_deref().and_then(seconds)).await {
                continue;
            }
            deliver(hub, Some(repo), &name, event, &s.run, &m, spec).await;
        }
    }
    // Each person's own rules (W19-R06), for repositories they may read.
    for (uid, person, email, rules) in subscribers(hub).await {
        let me = format!("user:{person}");
        let wanted: Vec<(usize, &Mine)> = rules
            .iter()
            .enumerate()
            .filter(|(_, r)| r.check().is_ok() && takes(&r.rule, event, s))
            .filter(|(_, r)| r.repos.is_empty() || r.repos.iter().any(|g| crate::pipeline::glob_match(g.trim(), &repo.name)))
            .filter(|(_, r)| !r.about_me || m.people.iter().any(|p| super::inbox::person(p) == me))
            .collect();
        if wanted.is_empty() || !crate::authz::gate::user_may(hub, &uid, "runs.read", &crate::authz::Scope::Repo { repo: repo.name.clone() }).await {
            continue;
        }
        for (i, r) in wanted {
            let name = format!("{me}#{}", i + 1);
            for to in &r.rule.to {
                let spec = match (to.as_str(), &email) {
                    ("inbox", _) => format!("inbox:{person}"),
                    ("email", Some(address)) => format!("email:{address}"),
                    // No address of theirs: nowhere to send.
                    _ => continue,
                };
                let key = format!("{name}\u{0}{spec}\u{0}{event}\u{0}{}\u{0}{}", s.workflow, s.branch);
                if throttled(hub, &key, r.rule.throttle.as_deref().and_then(seconds)).await {
                    continue;
                }
                deliver(hub, Some(repo), &name, event, &s.run, &m, &spec).await;
            }
        }
    }
}

/// Whether `rule` takes an agent's `event`: `where` reads `agent` (its `id`, `name`, `harness`,
/// `harness_name`, `machine`, `cwd`, `opener`, and the note's `text`).
fn takes_agent(rule: &Rule, event: &str, ev: &Value) -> bool {
    if !rule.on.iter().any(|e| e == event) {
        return false;
    }
    let Some(c) = &rule.cond else { return true };
    let Ok(cond) = kiln_ci::expr::Condition::parse(c, &kiln_ci::expr::ParseOptions::permissive(kiln_ci::expr::Dialect::Kiln)) else { return false };
    let mut agent = ev.get("agent").cloned().unwrap_or_else(|| json!({}));
    if let Some(o) = agent.as_object_mut() {
        o.insert("text".into(), ev.get("text").cloned().unwrap_or(Value::Null));
        o.insert("event".into(), json!(event));
    }
    let cx = kiln_ci::expr::MapContexts::default().with("agent", kiln_ci::expr::Value::from_json(&agent));
    kiln_ci::expr::eval_condition(&cond, &cx, &mut kiln_ci::expr::Budget::default()).unwrap_or(false)
}

/// An agent's event (docs/agentic.md N2, from `crate::push`), through every server rule that takes
/// it: at most once per channel, event and agent in a rule's `throttle`.
pub async fn agent_event(hub: Arc<Hub>, ev: Value) {
    let Some(event) = ev.get("type").and_then(Value::as_str).filter(|e| AGENT_EVENTS.contains(e)).map(str::to_owned) else { return };
    let id = ev.pointer("/agent/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let m = Msg::of_agent(&event, &ev);
    for (n, rule) in hub.cfg.ci.notify.iter().enumerate() {
        if rule.check(true).is_err() || !takes_agent(rule, &event, &ev) {
            continue;
        }
        let name = format!("server#{}", n + 1);
        for spec in &rule.to {
            let key = format!("{name}\u{0}{spec}\u{0}{event}\u{0}agent:{id}");
            if throttled(&hub, &key, rule.throttle.as_deref().and_then(seconds)).await {
                continue;
            }
            deliver(&hub, None, &name, &event, &format!("agent:{id}"), &m, spec).await;
        }
    }
}

/// Whether `rule` takes `event`, its `where` reading `value` as the context `name`.
fn takes_ctx(rule: &Rule, event: &str, name: &'static str, value: &Value) -> bool {
    if !rule.on.iter().any(|e| e == event) {
        return false;
    }
    let Some(c) = &rule.cond else { return true };
    let Ok(cond) = kiln_ci::expr::Condition::parse(c, &kiln_ci::expr::ParseOptions::permissive(kiln_ci::expr::Dialect::Kiln)) else { return false };
    let cx = kiln_ci::expr::MapContexts::default().with(name, kiln_ci::expr::Value::from_json(value));
    kiln_ci::expr::eval_condition(&cond, &cx, &mut kiln_ci::expr::Budget::default()).unwrap_or(false)
}

/// Whether `rule` takes a release's `event`: `where` reads `release` (its `id`, `app`,
/// `environment`, `kind`, `version`, `state`, `requested_by`, `message`, `approvers`).
fn takes_release(rule: &Rule, event: &str, release: &Value) -> bool {
    takes_ctx(rule, event, "release", release)
}

/// A channel an app names for its release messages: `kind:target`, or a bare `#channel`, which is
/// Slack's.
fn app_channel(c: &str) -> String {
    if c.contains(':') { c.to_owned() } else { format!("slack:{c}") }
}

/// What a release's message says now: its state, as [`Msg::of_release`] names it.
fn release_now(r: &Value) -> String {
    format!("release.{}", r.get("state").and_then(Value::as_str).unwrap_or("waiting"))
}

/// Releases whose messages are being brought up to date: the events each has waiting, and
/// whether it moved again meanwhile.
static MOVING: parking_lot::Mutex<Option<std::collections::HashMap<i64, (Vec<&'static str>, bool)>>> = parking_lot::Mutex::new(None);

/// A release moved (docs/deploy-api.md §12.3): `events` said of it through every server rule that
/// takes them and to the app's own channels, and every message it has brought up to date. Where a
/// channel can change a message afterwards (a Slack bot, a Discord webhook, a Telegram bot), a
/// release has one message there, posted the first time something of it is sent there and edited
/// as it moves on; elsewhere each event is a message of its own, once per release, event and
/// channel (kept, so a restart or the same state said again sends nothing more), and the app's
/// own channels take `release.started` only where it becomes their one message. One release's
/// moves are taken one at a time, each reading it as it is by then.
pub async fn release_moved(hub: Arc<Hub>, id: i64, events: Vec<&'static str>) {
    {
        let mut moving = MOVING.lock();
        let all = moving.get_or_insert_with(std::collections::HashMap::new);
        if let Some((waiting, again)) = all.get_mut(&id) {
            waiting.extend(events);
            *again = true;
            return;
        }
        all.insert(id, (events, false));
    }
    // A panic in the work below lets the release's later moves be taken again.
    struct Unwinding(i64);
    impl Drop for Unwinding {
        fn drop(&mut self) {
            if std::thread::panicking() {
                MOVING.lock().get_or_insert_with(std::collections::HashMap::new).remove(&self.0);
            }
        }
    }
    let _unwinding = Unwinding(id);
    loop {
        let events = MOVING.lock().get_or_insert_with(std::collections::HashMap::new).get_mut(&id).map(|(e, again)| {
            *again = false;
            std::mem::take(e)
        });
        Box::pin(release_up_to_date(&hub, id, &events.unwrap_or_default())).await;
        let mut moving = MOVING.lock();
        let all = moving.get_or_insert_with(std::collections::HashMap::new);
        if all.get(&id).is_some_and(|(_, again)| *again) {
            continue;
        }
        all.remove(&id);
        return;
    }
}

/// A message a release has where its channel can change it.
struct Post {
    spec: String,
    integration: String,
    at: super::chat::posts::Posted,
    shown: String,
}

async fn posts_of(hub: &Hub, id: i64) -> Vec<Post> {
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT spec, integration, channel, message, shown FROM release_chat_posts WHERE release_id = ?1")?;
            let rows = st.query_map([id], |r| Ok(Post { spec: r.get(0)?, integration: r.get(1)?, at: super::chat::posts::Posted { channel: r.get(2)?, message: r.get(3)? }, shown: r.get(4)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default()
}

/// Records how a release's message went through `spec`.
async fn delivered(hub: &Hub, rule: &str, event: &str, id: i64, spec: &str, result: &Result<(), String>) {
    if let Err(e) = result {
        tracing::warn!(channel = spec, event, release = id, error = %e, "a release's chat message didn't go");
    }
    let (rule, event, about, spec, now) = (rule.to_owned(), event.to_owned(), format!("release:{id}"), spec.to_owned(), crate::util::now_unix());
    let (status, error) = match result {
        Ok(()) => ("sent", None),
        Err(e) => ("failed", Some(e.clone())),
    };
    let _ = hub
        .store
        .call(move |c| Ok(c.execute("INSERT INTO notify_deliveries (ts, rule, event, run_id, channel, status, error, attempts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)", params![now, rule, event, about, spec, status, error]).map(|_| ())?))
        .await;
}

/// A release's message as `i` shows it now.
fn view_now(release: &Value, url: &str) -> super::chat::posts::View {
    let m = Msg::of_release(&release_now(release), release, url);
    let asks = (release.get("state").and_then(Value::as_str) == Some("waiting_approval")).then(|| release.get("id").and_then(Value::as_i64)).flatten();
    super::chat::posts::View { title: m.title, line: m.line, asks }
}

/// [`release_moved`]'s work: what `events` send, then each message the release has made to say
/// what it is now.
async fn release_up_to_date(hub: &Arc<Hub>, id: i64, events: &[&'static str]) {
    let Ok(Some(r)) = crate::deploy::store::release(&*hub.records, id).await else { return };
    let release = crate::deploy::flow::release_value(&r);
    let url = format!("{}/deployments/releases/{id}", hub.cfg.server.public_url.trim_end_matches('/'));
    let channels = crate::deploy::store::app(&*hub.records, &r.app).await.ok().flatten().map(|a| a.contacts.channels).unwrap_or_default();
    let mut posted: std::collections::HashSet<String> = posts_of(hub, id).await.into_iter().map(|p| p.spec).collect();
    for &event in events {
        let m = Msg::of_release(event, &release, &url);
        let mut sends: Vec<(String, String)> = Vec::new();
        for (n, rule) in hub.cfg.ci.notify.iter().enumerate() {
            if rule.check(true).is_err() || !takes_release(rule, event, &release) {
                continue;
            }
            sends.extend(rule.to.iter().map(|spec| (format!("server#{}", n + 1), spec.clone())));
        }
        sends.extend(channels.iter().map(|c| ("app".to_owned(), app_channel(c))));
        let mut seen = std::collections::HashSet::new();
        for (rule, spec) in sends {
            if !seen.insert(spec.clone()) {
                continue;
            }
            // Its one message, where the channel can change it.
            if let Ok((i, target)) = channel(hub, None, &spec).await
                && let Ok(Some(secret)) = integrations::token(hub, &i).await
                && super::chat::posts::edits(&i, Some(&secret))
            {
                if posted.contains(&spec) {
                    continue;
                }
                let v = view_now(&release, &url);
                let made = Box::pin(super::chat::posts::post(hub, &i, &secret, target.as_deref(), &v)).await;
                if let Ok(at) = &made {
                    let (s, iid, ch, msg, shown, now) = (spec.clone(), i.id.clone(), at.channel.clone(), at.message.clone(), super::chat::posts::shown(&i, &v), crate::util::now_unix());
                    let _ = hub
                        .store
                        .call(move |c| {
                            c.execute(
                                "INSERT OR IGNORE INTO release_chat_posts (release_id, spec, integration, channel, message, shown, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                                params![id, s, iid, ch, msg, shown, now],
                            )?;
                            Ok(())
                        })
                        .await;
                    posted.insert(spec.clone());
                }
                delivered(hub, &rule, event, id, &spec, &made.map(|_| ())).await;
                continue;
            }
            // A message per event elsewhere; the app's own channels aren't told each release starts.
            if rule == "app" && event == "release.started" {
                continue;
            }
            if throttled(hub, &format!("release\u{0}{id}\u{0}{event}\u{0}{spec}"), Some(RELEASE_ONCE)).await {
                continue;
            }
            deliver(hub, None, &rule, event, &format!("release:{id}"), &m, &spec).await;
        }
    }
    // Every message it has, saying what it is now.
    let all = integrations::list(hub).await.unwrap_or_default();
    for p in posts_of(hub, id).await {
        let Some(i) = all.iter().find(|i| i.id == p.integration) else { continue };
        let v = view_now(&release, &url);
        let shown = super::chat::posts::shown(i, &v);
        if shown == p.shown {
            continue;
        }
        let Ok(Some(secret)) = integrations::token(hub, i).await else { continue };
        let result = Box::pin(super::chat::posts::update(hub, i, &secret, &p.at, &v)).await;
        if result.is_ok() {
            let (spec, now) = (p.spec.clone(), crate::util::now_unix());
            let _ = hub.store.call(move |c| Ok(c.execute("UPDATE release_chat_posts SET shown = ?3, updated_at = ?4 WHERE release_id = ?1 AND spec = ?2", params![id, spec, shown, now]).map(|_| ())?)).await;
        }
        delivered(hub, "edit", &release_now(&release), id, &p.spec, &result).await;
    }
}

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
    let input: Value = input.and_then(|i| serde_json::from_str(&i).ok()).unwrap_or(Value::Null);
    let github = input.get("github").cloned().unwrap_or(Value::Null);
    // Whom a run is about: the person in kiln who started it (W10's `started_by`: an agent's, the
    // person it acts for), when one did; else its actor.
    let people = match input.pointer("/event/started_by/user").and_then(Value::as_str).map(str::to_owned) {
        Some(uid) => hub.store.call(move |c| Ok(c.query_row("SELECT name FROM users WHERE id = ?1", [&uid], |x| x.get::<_, String>(0)).optional()?)).await.ok().flatten().map(|n| vec![format!("user:{n}")]).unwrap_or_default(),
        None => Vec::new(),
    };
    let url = format!("{}/runs/{run}", hub.cfg.server.public_url.trim_end_matches('/'));
    Ok(Some((
        repo.clone(),
        Subject {
            repo: repo.name,
            run: run.to_owned(),
            number,
            workflow,
            name,
            branch,
            sha,
            message,
            actor,
            conclusion: conclusion.unwrap_or_default(),
            url,
            github,
            environments: Vec::new(),
            people,
            thread: None,
        },
    )))
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
    // W16-R04 in the run itself: tests a retry passed, or that an attempt failed and another
    // passed (`leg.flaky`, the tests named in the message).
    match super::results::flaky_in_run(&hub, &run).await {
        Ok(tests) if !tests.is_empty() => {
            let shown: Vec<&str> = tests.iter().take(10).map(String::as_str).collect();
            let more = if tests.len() > shown.len() { format!(" and {} more", tests.len() - shown.len()) } else { String::new() };
            let s = Subject { message: format!("flaky: {}{more}", shown.join(", ")), ..s };
            emit(&hub, &repo, "leg.flaky", &s).await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(run = %run, error = %format!("{e:#}"), "a run's flaky tests couldn't be read"),
    }
}

/// A deployment ended (W19-R06): `deploy.succeeded` or `deploy.failed`, about its run and
/// environment.
pub async fn deployed(hub: Arc<Hub>, run: String, environment: String, succeeded: bool) {
    let Ok(Some((repo, mut s))) = subject(&hub, &run).await else { return };
    s.environments = vec![environment];
    emit(&hub, &repo, if succeeded { "deploy.succeeded" } else { "deploy.failed" }, &s).await;
}

/// A land queue's entry was ejected (W19-R06): `land.ejected`, about its diffs (`D12 title`, the
/// first's link), with why.
pub async fn land_ejected(hub: Arc<Hub>, diffs: Vec<String>, reason: String) {
    let mut found = Vec::new();
    for id in &diffs {
        if let Ok(Some(d)) = super::diffs::find(&hub, id).await {
            found.push(d);
        }
    }
    let Some(first) = found.first() else { return };
    let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.name == first.repo)) else { return };
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let names: Vec<&str> = found.iter().map(|d| d.name.as_str()).collect();
    let s = Subject {
        repo: repo.name.clone(),
        name: format!("{} ({})", names.join(", "), first.title),
        branch: first.trunk.clone(),
        message: reason,
        actor: first.author.clone(),
        conclusion: "ejected".into(),
        url: format!("{base}/diffs/{}", first.name),
        github: json!({ "repository": repo.name, "ref": format!("refs/heads/{}", first.trunk), "ref_name": first.trunk }),
        ..Subject::default()
    };
    emit(&hub, &repo, "land.ejected", &s).await;
}

/// A diff landed on its trunk at `commit` (W11-R07): `diff.landed`, about it (`D12 title`, its
/// link), its author's, with its latest version's message (whose trailers name its issues).
pub async fn diff_landed(hub: Arc<Hub>, diff: String, commit: String) {
    let Ok(Some(d)) = super::diffs::find(&hub, &diff).await else { return };
    let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.name == d.repo)) else { return };
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let id = d.id.clone();
    let message = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT message FROM diff_versions WHERE diff_id = ?1 ORDER BY n DESC LIMIT 1", [&id], |r| r.get::<_, Option<String>>(0)).optional()?.flatten()))
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| format!("{}\n\n{}", d.title, d.summary));
    let s = Subject {
        repo: repo.name.clone(),
        run: format!("diff:{}", d.name),
        workflow: d.name.clone(),
        name: format!("{} {}", d.name, d.title),
        branch: d.trunk.clone(),
        sha: commit,
        message,
        actor: d.author.clone(),
        conclusion: "landed".into(),
        url: format!("{base}/diffs/{}", d.name),
        github: json!({ "repository": repo.name, "ref": format!("refs/heads/{}", d.trunk), "ref_name": d.trunk, "actor": d.author, "event_name": "diff.landed" }),
        people: vec![d.author_for.clone().unwrap_or_else(|| d.author.clone())],
        ..Subject::default()
    };
    emit(&hub, &repo, "diff.landed", &s).await;
}

/// Tests trunk pushes took into quarantine, for the scheduler's sweep to tell of: the
/// repository's id, the entry, the trunk and the commit.
static QUARANTINED: parking_lot::Mutex<Vec<(String, kiln_ci::results::quarantine::Entry, String, String)>> = parking_lot::Mutex::new(Vec::new());

/// A push moved trunk `branch` of `repo` from `before` to `after` (W16-R05, W11-R07): the tests
/// its `.kiln/quarantine.yaml` took in, for `test.quarantined`. A repository's first push is
/// what's quarantined already, said of nothing.
pub async fn trunk_moved(hub: &Hub, repo: &Repo, branch: &str, before: &str, after: &str) {
    const NAMES: [&str; 2] = [".kiln/quarantine.yaml", ".kiln/quarantine.yml"];
    let changed = crate::gitops::changed_files(hub, repo, before, after).await.unwrap_or_default();
    if !changed.iter().any(|p| NAMES.contains(&p.as_str())) {
        return;
    }
    let facts = super::events::facts(hub, repo).await;
    if branch != facts.default_branch {
        return;
    }
    let at = |sha: String| {
        let mirror = facts.mirror.clone();
        async move {
            for name in NAMES {
                if let Ok(text) = crate::gitops::run_git(hub, &["-C", &mirror, "show", &format!("{sha}:{name}")], crate::gitops::LS_REMOTE_TIMEOUT).await {
                    return kiln_ci::results::quarantine::parse(&String::from_utf8_lossy(&text)).unwrap_or_default();
                }
            }
            Vec::new()
        }
    };
    let (was, now) = (at(before.to_owned()).await, at(after.to_owned()).await);
    let added: Vec<_> = now.into_iter().filter(|e| !was.iter().any(|w| w.test == e.test)).collect();
    QUARANTINED.lock().extend(added.into_iter().map(|e| (repo.id.clone(), e, branch.to_owned(), after.to_owned())));
}

/// `test.quarantined` for each test trunk pushes took into quarantine since the last sweep: once
/// per repository, test and commit. Called as the scheduler sweeps.
pub fn quarantined(hub: &Arc<Hub>) {
    let taken = std::mem::take(&mut *QUARANTINED.lock());
    if taken.is_empty() {
        return;
    }
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        let repos = hub.store.repos().await.unwrap_or_default();
        let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
        for (repo_id, e, branch, sha) in taken {
            let Some(repo) = repos.iter().find(|r| r.id == repo_id) else { continue };
            if throttled(&hub, &format!("test.quarantined\u{0}{repo_id}\u{0}{}\u{0}{sha}", e.test), Some(QUARANTINE_ONCE)).await {
                continue;
            }
            let mut message = e.reason.clone();
            if let Some(until) = &e.until {
                message.push_str(&format!(" · until {until}"));
            }
            // The issue it names, if any, is what a tracker comments on (`Issue:`, a trailer).
            if let Some(issue) = &e.issue {
                message.push_str(&format!("\n\nIssue: {issue}"));
            }
            let s = Subject {
                repo: repo.name.clone(),
                run: format!("test:{}", e.test),
                workflow: e.test.clone(),
                name: e.test.clone(),
                branch,
                sha,
                message,
                url: format!("{base}/insights?repo={}", repo.name),
                github: json!({ "repository": repo.name, "event_name": "test.quarantined" }),
                ..Subject::default()
            };
            emit(&hub, repo, "test.quarantined", &s).await;
        }
    });
}

/// Runners told of as offline, until they're back.
static OFFLINE: parking_lot::Mutex<Option<std::collections::HashSet<String>>> = parking_lot::Mutex::new(None);

/// Runners (not pods or outposts) gone past `OFFLINE_AFTER`: `runner.offline` for each, once
/// each time it goes. Called as the scheduler sweeps.
pub fn runners_gone(hub: &Arc<Hub>) {
    let gone: Vec<(String, u64)> = hub
        .agents
        .lock()
        .values()
        .filter(|a| !a.connected && !a.ephemeral && !a.outpost && a.lost_at.is_some_and(|t| t.elapsed() > OFFLINE_AFTER))
        .map(|a| (a.name.clone(), a.lost_at.map_or(0, |t| t.elapsed().as_secs())))
        .collect();
    let mut told = OFFLINE.lock();
    let told = told.get_or_insert_with(std::collections::HashSet::new);
    // Those back since are told of again when they next go.
    told.retain(|n| gone.iter().any(|(g, _)| g == n));
    for (name, secs) in gone {
        if told.insert(name.clone()) {
            crate::rt::spawn(runner_offline(Arc::clone(hub), name, secs));
        }
    }
}

/// A runner went away and hasn't come back (W19-R06): `runner.offline`, through the server's
/// rules that take it (`where` reads `runner`: its `name`, `labels` and `gone_for` seconds), and
/// to the server's webhook subscribers.
pub async fn runner_offline(hub: Arc<Hub>, name: String, gone_for: u64) {
    let live = hub.agents.lock().get(&name).map(|a| a.labels.clone());
    let labels = match live {
        Some(l) => l,
        // Gone from the live runners: the labels it has on record.
        None => hub.store.agents().await.unwrap_or_default().into_iter().find(|a| a.name == name).map(|a| a.labels).unwrap_or_default(),
    };
    let runner = json!({ "name": name, "labels": labels, "gone_for": gone_for });
    hub.emit(&json!({ "type": "runner.offline", "runner": runner }));
    let url = format!("{}/runners/{name}", hub.cfg.server.public_url.trim_end_matches('/'));
    let title = format!("⚠ runner {name} is offline");
    let line = format!("gone {} min · {} · {url}", gone_for / 60, if labels.is_empty() { "no labels".to_owned() } else { labels.join(", ") });
    let m = Msg {
        title: title.clone(),
        line: line.clone(),
        url,
        what: "the runner",
        dedup: format!("kiln:runner:{name}"),
        resolve: false,
        webhook: json!({ "event": "runner.offline", "title": title, "text": line, "runner": runner }),
        people: Vec::new(),
        refs: Vec::new(),
        thread: None,
    };
    for (n, rule) in hub.cfg.ci.notify.iter().enumerate() {
        if rule.check(true).is_err() || !takes_ctx(rule, "runner.offline", "runner", &runner) {
            continue;
        }
        let rule_name = format!("server#{}", n + 1);
        for spec in &rule.to {
            let key = format!("{rule_name}\u{0}{spec}\u{0}runner.offline\u{0}{name}");
            if throttled(&hub, &key, rule.throttle.as_deref().and_then(seconds)).await {
                continue;
            }
            deliver(&hub, None, &rule_name, "runner.offline", &format!("runner:{name}"), &m, spec).await;
        }
    }
}

/// A deploy of kiln itself that didn't simply take (docs/resilience.md, G7), told by the server
/// that finishes its record: `deploy.held` (a check that holds failed; it was kept),
/// `deploy.rolled_back`, or `deploy.rollback_failed` (the version put back couldn't start).
/// Server rules see `deploy`: its `app`, `from`, `to` and `revision`.
pub async fn self_deploy(hub: Arc<Hub>, event: &'static str, deploy: Value, message: String) {
    hub.emit(&json!({ "type": event, "deploy": deploy, "message": message }));
    let url = format!("{}/deployments", hub.cfg.server.public_url.trim_end_matches('/'));
    let app = deploy.get("app").and_then(Value::as_str).unwrap_or("kiln").to_owned();
    let title = match event {
        "deploy.held" => format!("⚠ {app}: kiln's deploy was held"),
        "deploy.rolled_back" => format!("✗ {app}: kiln's deploy was rolled back"),
        _ => format!("✗ {app}: kiln's rollback couldn't start"),
    };
    let m = Msg {
        title: title.clone(),
        line: message.clone(),
        url,
        what: "the deploy",
        dedup: format!("kiln:self-deploy:{app}"),
        resolve: false,
        webhook: json!({ "event": event, "title": title, "text": message, "deploy": deploy }),
        people: Vec::new(),
        refs: Vec::new(),
        thread: None,
    };
    for (n, rule) in hub.cfg.ci.notify.iter().enumerate() {
        if rule.check(true).is_err() || !takes_ctx(rule, event, "deploy", &deploy) {
            continue;
        }
        let rule_name = format!("server#{}", n + 1);
        for spec in &rule.to {
            deliver(&hub, None, &rule_name, event, &format!("self-deploy:{app}"), &m, spec).await;
        }
    }
}

/// A person-facing event about a diff or change request (W19-R06): `review.requested` (`people`:
/// the reviewers asked), `comment.reply` (the people in the thread) or `mention` (whom it
/// mentions), done by `actor`, about review thread `thread` when it's one's. The repository's
/// rules see `run` as the diff: `name` it and its title, `workflow` its name, `branch` its trunk,
/// `actor`, `message` (what was said), `url`.
#[allow(clippy::too_many_arguments)]
pub async fn diff_event(hub: Arc<Hub>, repo: String, event: &'static str, diff: String, title: String, trunk: String, actor: String, text: String, people: Vec<String>, thread: Option<String>) {
    let mut people: Vec<String> = people.into_iter().filter(|p| !p.eq_ignore_ascii_case(&actor) && super::inbox::person(p) != super::inbox::person(&actor)).collect();
    people.sort();
    people.dedup();
    if people.is_empty() {
        return;
    }
    let Some(r) = hub.store.repos().await.ok().and_then(|all| all.into_iter().find(|x| x.name == repo)) else { return };
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let s = Subject {
        repo: repo.clone(),
        run: format!("diff:{diff}"),
        workflow: diff.clone(),
        name: format!("{diff} {title}"),
        branch: trunk.clone(),
        message: text,
        actor: actor.clone(),
        url: format!("{base}/diffs/{diff}"),
        github: json!({ "repository": repo, "ref": format!("refs/heads/{trunk}"), "ref_name": trunk, "actor": actor, "event_name": event }),
        people,
        thread,
        ..Subject::default()
    };
    emit(&hub, &r, event, &s).await;
}

/// Repositories told their cache is near its quota, until it's well under it again.
static NEAR_QUOTA: parking_lot::Mutex<Option<std::collections::HashSet<String>>> = parking_lot::Mutex::new(None);

/// A repository's cache uses `used` of `quota` bytes (after eviction): past 90 %, `quota.near`,
/// once until it's under 80 % again.
pub async fn cache_use(hub: &Hub, repo_id: String, used: u64, quota: u64) {
    if quota == 0 {
        return;
    }
    let pct = used.saturating_mul(100) / quota;
    {
        let mut told = NEAR_QUOTA.lock();
        let told = told.get_or_insert_with(std::collections::HashSet::new);
        if pct < NEAR.1 {
            told.remove(&repo_id);
            return;
        }
        if pct < NEAR.0 || !told.insert(repo_id.clone()) {
            return;
        }
    }
    let Some(repo) = hub.store.repos().await.ok().and_then(|all| all.into_iter().find(|r| r.id == repo_id)) else { return };
    let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let s = Subject {
        repo: repo.name.clone(),
        name: "the cache".into(),
        message: format!("{:.1} GiB of {:.1} GiB ({pct} %); the least recently used entries go first", gib(used), gib(quota)),
        url: format!("{base}/ci/caches?repo={}", repo.name),
        github: json!({ "repository": repo.name }),
        ..Subject::default()
    };
    emit(hub, &repo, "quota.near", &s).await;
}

/// When unused secrets were last looked for.
static UNUSED_LOOKED: parking_lot::Mutex<i64> = parking_lot::Mutex::new(0);

/// Secrets no run read in 90 days (W19-R06): `secret.unused` for each, again 90 days after it was
/// said. A repository's and its environments' go through its rules; the server's, through the
/// server's (`where` reads `secret`: its `name`, `scope` and `last_used_at`). Looked for hourly,
/// as the scheduler sweeps.
pub fn secrets_unused(hub: &Arc<Hub>) {
    let now = crate::util::now_unix();
    {
        let mut at = UNUSED_LOOKED.lock();
        if now - *at < 3600 {
            return;
        }
        *at = now;
    }
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        let before = now - UNUSED_AFTER;
        let rows: Vec<(String, String, String, String, Option<i64>)> = hub
            .store
            .call(move |c| {
                let mut st = c.prepare("SELECT id, name, scope_kind, scope_id, last_used_at FROM ci_secrets WHERE scope_kind != 'integration' AND COALESCE(last_used_at, created_at) < ?1 ORDER BY name")?;
                let rows = st.query_map([before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap_or_default();
        let repos = hub.store.repos().await.unwrap_or_default();
        let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
        for (id, name, kind, scope, last) in rows {
            if throttled(&hub, &format!("secret.unused\u{0}{id}"), Some(UNUSED_AFTER)).await {
                continue;
            }
            let when = last.map_or_else(|| "never read".to_owned(), |t| format!("last read {} days ago", (now - t) / 86_400));
            // A repository's own, or one of its workflows' or environments' (`<repo id>:<name>`).
            let repo = repos.iter().find(|r| matches!(kind.as_str(), "repository" | "workflow" | "environment") && (scope == r.id || scope.starts_with(&format!("{}:", r.id))));
            let s = Subject {
                repo: repo.map(|r| r.name.clone()).unwrap_or_else(|| "the server".to_owned()),
                run: format!("secret:{name}"),
                name: name.clone(),
                message: format!("{kind} {}, {when}", repo.map_or_else(|| scope.clone(), |r| scope.strip_prefix(&format!("{}:", r.id)).map_or_else(|| r.name.clone(), |rest| format!("{} {rest}", r.name)))),
                url: format!("{base}/settings/secrets"),
                github: json!({ "repository": repo.map(|r| r.name.clone()) }),
                ..Subject::default()
            };
            match repo {
                Some(r) => emit(&hub, r, "secret.unused", &s).await,
                None => {
                    let secret = json!({ "name": name, "scope": kind, "last_used_at": last });
                    let m = Msg::of_run("secret.unused", &s);
                    for (n, rule) in hub.cfg.ci.notify.iter().enumerate() {
                        if rule.check(true).is_err() || !takes_ctx(rule, "secret.unused", "secret", &secret) {
                            continue;
                        }
                        for spec in &rule.to {
                            deliver(&hub, None, &format!("server#{}", n + 1), "secret.unused", &s.run, &m, spec).await;
                        }
                    }
                }
            }
        }
    });
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
    let m = Msg::of_run("message", &s);
    for spec in &to {
        let r = match channel(hub, Some(&repo), spec).await {
            Ok((i, target)) => send(hub, &i, target.as_deref(), "message", &m).await,
            Err(e) => Err(e),
        };
        let (ok, skipped, error) = match r {
            Ok(Went::Sent) => (true, None, None),
            Ok(Went::Skipped(why)) => (true, Some(why), None),
            Err(e) => (false, None, Some(e)),
        };
        sent.push(json!({ "to": spec, "ok": ok, "skipped": skipped, "error": error }));
    }
    Ok(srv::json(StatusCode::OK, &json!({ "sent": sent })))
}

/// Integration Verify for a channel: a test message.
pub async fn verify(hub: &Hub, i: &Integration) -> Result<String, String> {
    // An issue tracker says who it is, rather than comment anywhere.
    if super::trackers::KINDS.contains(&i.kind.as_str()) {
        return Box::pin(super::trackers::verify(hub, i)).await;
    }
    let s = Subject { name: "kiln: a test message; this channel works".into(), url: hub.cfg.server.public_url.clone(), ..Subject::default() };
    send(hub, i, None, "message", &Msg::of_run("message", &s)).await.map(|_| format!("sent a test message through {}", i.name))
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
        Subject {
            repo: "acme/app".into(),
            run: "r1".into(),
            number: 7,
            workflow: ".github/workflows/ci.yml".into(),
            name: "CI".into(),
            branch: "main".into(),
            sha: "0123456789abcdef".into(),
            message: "fix the parser\n\nlong".into(),
            actor: "ada".into(),
            conclusion: "failure".into(),
            url: "https://kiln.example/runs/r1".into(),
            github: json!({ "ref_name": "main", "event_name": "push" }),
            environments: Vec::new(),
            people: Vec::new(),
            thread: None,
        }
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

    /// W19-R06's events for people, the cache and secrets: their messages, whom they're about, and
    /// the rules that take them.
    #[test]
    fn w19_r06_people_cache_and_secret_messages() {
        let s = Subject { repo: "acme/app".into(), name: "D3 Parse dates".into(), workflow: "D3".into(), branch: "main".into(), actor: "user:dev".into(), message: "Is this right, @rev?\nmore".into(), url: "https://kiln.example/diffs/D3".into(), people: vec!["user:rev".into()], ..Subject::default() };
        assert_eq!(message("review.requested", &s), ("👀 dev asks for a review of D3 Parse dates".to_owned(), "acme/app · for rev · https://kiln.example/diffs/D3".to_owned()));
        assert_eq!(message("comment.reply", &s).1, "acme/app · Is this right, @rev? · https://kiln.example/diffs/D3");
        assert_eq!(message("mention", &s).0, "@ dev mentioned rev on D3 Parse dates");
        let m = Msg::of_run("mention", &s);
        assert_eq!((m.people.clone(), m.what), (vec!["user:rev".to_owned()], "the diff"));
        // A run's is its actor's.
        assert_eq!(Msg::of_run("run.failed", &subject()).people, vec!["ada".to_owned()]);
        let q = Subject { repo: "acme/app".into(), message: "9.2 GiB of 10.0 GiB (92 %)".into(), url: "https://kiln.example/ci/caches?repo=acme/app".into(), ..Subject::default() };
        assert_eq!(message("quota.near", &q).0, "⚠ acme/app's cache is near its quota");
        let rules = parse_rules("- on: [review.requested, comment.reply, mention, quota.near, secret.unused]\n  to: [inbox, email:ada@example.com]\n").unwrap_or_default();
        assert!(rules.first().is_some_and(|r| takes(r, "mention", &s) && takes(r, "quota.near", &q)));
    }

    /// docs/agentic.md N2: an agent's notes through the server's rules, `where` reading `agent`.
    #[test]
    fn n2_agents_events_go_through_the_servers_rules() {
        let ev = json!({ "type": "agent.needs_you", "agent": { "id": "ag1", "name": "Mantle", "harness": "claude", "machine": "metal-1", "cwd": "/src/kiln", "opener": "owner" }, "title": "Mantle", "text": "Asks: which database?", "url": "https://kiln.example/agents/ag1" });
        let rule = |on: &str, cond: Option<&str>| Rule { on: vec![on.into()], cond: cond.map(str::to_owned), to: vec!["slack:#agents".into()], throttle: Some("5m".into()) };
        assert!(takes_agent(&rule("agent.needs_you", None), "agent.needs_you", &ev));
        assert!(!takes_agent(&rule("agent.finished", None), "agent.needs_you", &ev));
        assert!(takes_agent(&rule("agent.needs_you", Some("agent.machine == 'metal-1' && agent.harness == 'claude'")), "agent.needs_you", &ev));
        assert!(!takes_agent(&rule("agent.needs_you", Some("agent.machine == 'metal-2'")), "agent.needs_you", &ev));
        assert!(takes_agent(&rule("agent.needs_you", Some("startsWith(agent.text, 'Asks')")), "agent.needs_you", &ev));
        // A server rule may name them; a repository's may not.
        assert!(rule("agent.paused", None).check(true).is_ok());
        assert!(rule("agent.paused", None).check(false).is_err());
        assert!(parse_rules("- on: agent.finished\n  to: slack:#x\n").is_err());
        let m = Msg::of_agent("agent.needs_you", &ev);
        assert_eq!(m.title, "⏸ Mantle needs you");
        assert_eq!(m.line, "Asks: which database? · metal-1 · https://kiln.example/agents/ag1");
        assert_eq!(m.dedup, "kiln:agent:ag1");
        assert!(!m.resolve && Msg::of_agent("agent.finished", &ev).resolve);
        assert_eq!(m.webhook.pointer("/agent/opener").and_then(Value::as_str), Some("owner"));
        // A run's message is as it was.
        let r = Msg::of_run("run.failed", &subject());
        assert_eq!((r.title.as_str(), r.dedup.as_str(), r.resolve), ("✗ CI #7 failed on main", "kiln:acme/app:.github/workflows/ci.yml:main", false));
    }

    /// docs/deploy-api.md §12.3: a release's events through the server's rules, `where` reading
    /// `release`, and to the app's own channels.
    #[test]
    fn a_releases_events_go_through_the_servers_rules_and_the_apps_channels() {
        let r = json!({ "id": 42, "app": "web", "environment": "production", "kind": "deploy", "state": "waiting_approval", "version": "1.4.2", "requested_by": "ada", "message": "the parser fix\n\nlong", "approvers": ["grace", "linus"] });
        let rule = |on: &str, cond: Option<&str>| Rule { on: vec![on.into()], cond: cond.map(str::to_owned), to: vec!["slack:#deploys".into()], throttle: None };
        assert!(takes_release(&rule("release.waiting_approval", None), "release.waiting_approval", &r));
        assert!(!takes_release(&rule("release.failed", None), "release.waiting_approval", &r));
        assert!(takes_release(&rule("release.waiting_approval", Some("release.environment == 'production'")), "release.waiting_approval", &r));
        assert!(!takes_release(&rule("release.waiting_approval", Some("release.app == 'api'")), "release.waiting_approval", &r));
        // A server rule may name them; a repository's may not.
        assert!(rule("release.succeeded", None).check(true).is_ok());
        assert!(parse_rules("- on: release.succeeded\n  to: slack:#x\n").is_err());
        let m = Msg::of_release("release.waiting_approval", &r, "https://kiln.example/deployments/releases/42");
        assert_eq!(m.title, "⏸ web 1.4.2 → production waits for approval");
        assert_eq!(m.line, "release 42, asked by ada · approvers: grace, linus · the parser fix · https://kiln.example/deployments/releases/42");
        assert_eq!((m.dedup.as_str(), m.resolve), ("kiln:release:web:production", false));
        let rollback = json!({ "id": 43, "app": "web", "environment": "production", "kind": "rollback", "version": "1.4.1", "requested_by": "grace" });
        let landed = Msg::of_release("release.succeeded", &rollback, "u");
        assert_eq!((landed.title.as_str(), landed.resolve), ("✓ web a rollback to 1.4.1 landed in production", true));
        assert_eq!(app_channel("#deploys"), "slack:#deploys", "a bare channel is Slack's");
        assert_eq!(app_channel("teams:ops"), "teams:ops");
    }

    /// A deploy of kiln itself that didn't take is told (docs/resilience.md, G7): the server's
    /// rules take it, reading `deploy`; a repository's can't.
    #[test]
    fn kilns_own_deploys_go_through_the_servers_rules() {
        let d = json!({ "app": "kiln-production", "from": "kiln:8adeadba", "to": "kiln:61b07ceb", "revision": "61b07ceb" });
        let rule = |on: &str, cond: Option<&str>| Rule { on: vec![on.into()], cond: cond.map(str::to_owned), to: vec!["slack:#ops".into()], throttle: None };
        for e in SELF_DEPLOY_EVENTS {
            assert!(EVENTS.contains(e), "{e}");
            assert!(rule(e, None).check(true).is_ok(), "{e}");
        }
        assert!(takes_ctx(&rule("deploy.rollback_failed", Some("deploy.to == 'kiln:61b07ceb'")), "deploy.rollback_failed", "deploy", &d));
        assert!(!takes_ctx(&rule("deploy.held", None), "deploy.rolled_back", "deploy", &d));
        assert!(parse_rules("- on: deploy.held\n  to: slack:#x\n").is_err_and(|e| e.contains("a deploy of kiln itself")));
    }

    /// W19-R06's other events: a deployment's end, a land queue's ejection, a runner gone. A
    /// repository's rules may take the first two; only the server's take `runner.offline`.
    #[test]
    fn w19_r06_deploy_land_and_runner_events() {
        let s = Subject { environments: vec!["production".into()], ..subject() };
        assert_eq!(message("deploy.succeeded", &s).0, "✓ CI #7 deployed to production");
        assert_eq!(message("deploy.failed", &s).0, "✗ CI #7 failed to deploy to production");
        let (failed, fixed) = (Msg::of_run("deploy.failed", &s), Msg::of_run("deploy.succeeded", &s));
        assert_eq!((failed.dedup.as_str(), failed.resolve, fixed.resolve), ("kiln:acme/app:deploy:production", false, true), "one incident per environment, resolved by the next success");
        let flaky = Subject { message: "flaky: calc adds, calc divides".into(), ..subject() };
        assert_eq!(message("leg.flaky", &flaky), ("≈ CI #7 had flaky tests on main".to_owned(), "acme/app · 01234567 flaky: calc adds, calc divides · ada · https://kiln.example/runs/r1".to_owned()));
        let ejected = Subject { repo: "acme/app".into(), name: "D12, D13 (the parser fix)".into(), branch: "main".into(), message: "needs rebase: conflicts in src/a.rs".into(), url: "https://kiln.example/diffs/D12".into(), ..Subject::default() };
        assert_eq!(message("land.ejected", &ejected), ("⏏ D12, D13 (the parser fix) left the land queue of main".to_owned(), "acme/app · needs rebase: conflicts in src/a.rs · https://kiln.example/diffs/D12".to_owned()));
        assert_eq!(Msg::of_run("land.ejected", &ejected).what, "the diff");
        assert!(parse_rules("- on: [deploy.failed, land.ejected]\n  to: slack:#ci\n").is_ok());
        assert!(parse_rules("- on: runner.offline\n  to: slack:#ops\n").is_err(), "a repository's rules can't take runner.offline");
        let rule = Rule { on: vec!["runner.offline".into()], cond: Some("contains(runner.labels, 'gpu')".into()), to: vec!["pagerduty:ops".into()], throttle: None };
        assert!(rule.check(true).is_ok());
        assert!(takes_ctx(&rule, "runner.offline", "runner", &json!({ "name": "metal-1", "labels": ["linux", "gpu"], "gone_for": 180 })));
        assert!(!takes_ctx(&rule, "runner.offline", "runner", &json!({ "name": "metal-2", "labels": ["linux"], "gone_for": 180 })));
    }
}
