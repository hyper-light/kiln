//! Routing rules (docs/ci.md W05-R21): declared, ordered data from
//!
//! 1. the server's configuration (`[[ci.routing]]`);
//! 2. the database (`ci_routing_rules`), set through `/api/ci/routing/rules` for the server and
//!    for each organization;
//! 3. each repository's `.kiln/routing.yaml` on its default branch;
//! 4. the defaults: the native rule, and GitHub's hosted labels when `[ci] github_labels` is on.
//!
//! Every rule a leg matches adds its constraints; the first that names executors (or an image)
//! decides them. The server's rules come first, so an administrator's executor and image win. An
//! organization's rules apply only to its repositories and a repository's only to itself, so
//! neither can reach past its scope; they can only add to what comes before them.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln_ci::sched::routing::{self, Constraint, LegRoute, Rule};
use parking_lot::Mutex;
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// Every rule, by where it comes from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sources {
    pub config: Vec<Rule>,
    pub server: Vec<Rule>,
    /// By organization (owner), already limited to it.
    pub orgs: Vec<(String, Vec<Rule>)>,
    /// By repository (`owner/name`), already limited to it.
    pub repos: HashMap<String, Vec<Rule>>,
    pub defaults: Vec<Rule>,
}

impl Sources {
    /// The rules in the order they apply, each with where it comes from.
    pub fn labelled(&self) -> Vec<(String, Rule)> {
        let mut out: Vec<(String, Rule)> = Vec::new();
        out.extend(self.config.iter().cloned().map(|r| ("config".to_owned(), r)));
        out.extend(self.server.iter().cloned().map(|r| ("server".to_owned(), r)));
        for (org, rules) in &self.orgs {
            out.extend(rules.iter().cloned().map(|r| (format!("org:{org}"), r)));
        }
        let mut repos: Vec<(&String, &Vec<Rule>)> = self.repos.iter().collect();
        repos.sort_by(|a, b| a.0.cmp(b.0));
        for (repo, rules) in repos {
            out.extend(rules.iter().cloned().map(|r| (format!("repo:{repo}"), r)));
        }
        out.extend(self.defaults.iter().cloned().map(|r| ("default".to_owned(), r)));
        out
    }

    pub fn effective(&self) -> Vec<Rule> {
        self.labelled().into_iter().map(|(_, r)| r).collect()
    }

    /// A repository's rules changed: whether anything did.
    pub fn set_repo(&mut self, repo: &str, rules: Vec<Rule>) -> bool {
        let before = self.repos.get(repo).cloned().unwrap_or_default();
        if before == rules {
            return false;
        }
        if rules.is_empty() {
            self.repos.remove(repo);
        } else {
            self.repos.insert(repo.to_owned(), rules);
        }
        true
    }
}

/// The defaults (W05-R21): the native rule, then GitHub's hosted labels when they're on.
pub fn defaults(github_labels: bool) -> Vec<Rule> {
    let mut v = vec![routing::native_rule()];
    if github_labels {
        v.extend(routing::github_label_rules());
    }
    v
}

/// A rule set's problems: every constraint must read.
pub fn check(rules: &[Rule]) -> Result<(), String> {
    for (i, r) in rules.iter().enumerate() {
        for c in &r.require {
            Constraint::parse(c).map_err(|e| format!("rule {}: `{c}`: {e}", i + 1))?;
        }
        for g in &r.matches.repos {
            kiln_ci::trigger::pattern::Pattern::new(g).map_err(|e| format!("rule {}: repos `{g}`: {e}", i + 1))?;
        }
    }
    Ok(())
}

/// `rules`, each limited to the repositories `glob` names: a rule naming repositories keeps those
/// within it; one naming none outside it applies to nothing and is dropped.
fn within(rules: Vec<Rule>, prefix: &str, glob: &str) -> Vec<Rule> {
    rules
        .into_iter()
        .filter_map(|mut r| {
            if r.matches.repos.is_empty() {
                r.matches.repos = vec![glob.to_owned()];
            } else {
                r.matches.repos.retain(|g| g.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()));
                if r.matches.repos.is_empty() {
                    return None;
                }
            }
            Some(r)
        })
        .collect()
}

/// An organization's rules, limited to its repositories.
pub fn for_org(org: &str, rules: Vec<Rule>) -> Vec<Rule> {
    within(rules, &format!("{org}/"), &format!("{org}/*"))
}

/// A repository's rules, limited to it.
pub fn for_repo(repo: &str, rules: Vec<Rule>) -> Vec<Rule> {
    rules
        .into_iter()
        .map(|mut r| {
            r.matches.repos = vec![repo.to_owned()];
            r
        })
        .collect()
}

/// `.kiln/routing.yaml`: a list of rules, or `{ rules: [...] }`.
pub fn parse_file(text: &[u8]) -> Result<Vec<Rule>, String> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum File {
        List(Vec<Rule>),
        Map { rules: Vec<Rule> },
    }
    let rules = match serde_yaml::from_slice::<File>(text).map_err(|e| e.to_string())? {
        File::List(r) | File::Map { rules: r } => r,
    };
    check(&rules)?;
    Ok(rules)
}

/// How long a repository's file is read again at most.
const FRESH: Duration = Duration::from_secs(20);

static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<Rule>)>>> = Mutex::new(None);

/// A repository's rules, from `.kiln/routing.yaml` on its mirror's default branch (limited to
/// it); none when there is no file or it can't be read (which is logged).
pub async fn repo_rules(hub: &Hub, repo: &Repo) -> Vec<Rule> {
    if let Some((at, r)) = CACHE.lock().get_or_insert_with(HashMap::new).get(&repo.id)
        && at.elapsed() < FRESH
    {
        return r.clone();
    }
    let mut rules = Vec::new();
    if let Some((name, text)) = super::repo_config::trunk_file(hub, repo, &[".kiln/routing.yaml", ".kiln/routing.yml"]).await {
        match parse_file(&text) {
            Ok(r) => rules = for_repo(&repo.name, r),
            Err(e) => tracing::warn!(repo = %repo.name, file = name, error = %e, "the repository's routing rules can't be read; it has none"),
        }
    }
    CACHE.lock().get_or_insert_with(HashMap::new).insert(repo.id.clone(), (Instant::now(), rules.clone()));
    rules
}

/// The rules the database holds: the server's, and each organization's.
async fn stored(hub: &Hub) -> anyhow::Result<(Vec<Rule>, Vec<(String, Vec<Rule>)>)> {
    let rows: Vec<(String, String)> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT scope, rules FROM ci_routing_rules ORDER BY scope")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    let (mut server, mut orgs) = (Vec::new(), Vec::new());
    for (scope, text) in rows {
        let rules: Vec<Rule> = serde_json::from_str(&text).unwrap_or_default();
        match scope.strip_prefix("org:") {
            Some(org) => orgs.push((org.to_owned(), for_org(org, rules))),
            None => server = rules,
        }
    }
    Ok((server, orgs))
}

/// Every source as it stands: the configuration, the database and every repository's file.
pub async fn sources(hub: &Hub) -> Sources {
    let (server, orgs) = stored(hub).await.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "the stored routing rules can't be read");
        Default::default()
    });
    let mut repos = HashMap::new();
    for r in hub.store.repos().await.unwrap_or_default() {
        let rules = repo_rules(hub, &r).await;
        if !rules.is_empty() {
            repos.insert(r.name.clone(), rules);
        }
    }
    Sources { config: hub.cfg.ci.routing.clone(), server, orgs, repos, defaults: defaults(hub.cfg.ci.github_labels) }
}

// ------------------------------------------------------------------ API

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

/// `GET /api/ci/routing/rules`: every rule in the order they apply, with where it comes from.
pub async fn get(hub: &Hub) -> Result<Response, Fail> {
    let s = sources(hub).await;
    let rules: Vec<Value> = s.labelled().into_iter().enumerate().map(|(i, (source, r))| json!({ "index": i, "source": source, "rule": r })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "rules": rules, "github_labels": hub.cfg.ci.github_labels })))
}

/// `PUT /api/ci/routing/rules?scope=server|org:<owner>` with `{ rules: [...] }`: replaces that
/// scope's stored rules (an empty list removes them).
pub async fn put(hub: &Hub, q: &HashMap<String, String>, body: Value, by: &str) -> Result<Response, Fail> {
    let scope = q.get("scope").map(String::as_str).unwrap_or("server").trim().to_owned();
    let scope = match scope.strip_prefix("org:") {
        Some(o) if !o.is_empty() && !o.contains('/') => format!("org:{}", o.to_ascii_lowercase()),
        Some(_) => return Err(bad("an organization's scope is `org:<owner>`")),
        None if scope == "server" => scope,
        None => return Err(bad("routing rules are kept for `server` or `org:<owner>`; a repository's are its `.kiln/routing.yaml`")),
    };
    let rules: Vec<Rule> = serde_json::from_value(body.get("rules").cloned().unwrap_or(json!([]))).map_err(|e| bad(format!("rules: {e}")))?;
    check(&rules).map_err(bad)?;
    let (s, text, by_) = (scope.clone(), serde_json::to_string(&rules).unwrap_or_default(), by.to_owned());
    let n = rules.len();
    hub.store
        .call(move |c| {
            if n == 0 {
                c.execute("DELETE FROM ci_routing_rules WHERE scope = ?1", [&s])?;
            } else {
                c.execute(
                    "INSERT INTO ci_routing_rules (scope, rules, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(scope) DO UPDATE SET rules = ?2, updated_at = ?3, updated_by = ?4",
                    params![s, text, crate::util::now_unix(), by_],
                )?;
            }
            Ok(())
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.store.audit(by.to_owned(), "ci.routing.rules".into(), format!("{scope}: {n} rules")).await;
    hub.ci.send(super::dispatch::Msg::RoutingChanged);
    // What the change does now (W20-R03): the dispatcher has the new rules before it answers.
    let (diagnostics, preview) = effect(hub, &rules).await;
    Ok(srv::json(StatusCode::OK, &json!({ "scope": scope, "rules": rules, "diagnostics": diagnostics, "preview": preview })))
}

/// Where `route` goes now, as the dispatcher explains it.
async fn route_now(hub: &Hub, route: LegRoute) -> Option<Value> {
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::ExplainRoute { route: Box::new(route), reply: tx });
    crate::rt::timeout(Duration::from_secs(5), rx.recv()).await.ok()?.ok()
}

/// The runners a route's explanation says take it now: connected, and every check passing.
fn takers(v: &Value) -> Vec<String> {
    v.get("runners").and_then(Value::as_array).into_iter().flatten().filter(|r| r.get("connected") == Some(&json!(true)) && r.get("eligible") == Some(&json!(true))).filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_owned)).collect()
}

/// What a change of rules does now (W20-R03): for each rule that matches labels, a warning when
/// no runner connected now meets it (its legs would wait); and for the label sets the last week's
/// legs asked for, where each would go now (at most 20).
pub async fn effect(hub: &Hub, rules: &[Rule]) -> (Vec<Value>, Vec<Value>) {
    let leg = |labels: Vec<String>, repo: String| LegRoute { labels, repo, event: "push".into(), caps: u64::from(crate::proto::CAP_CI2), ..LegRoute::default() };
    let mut diagnostics = Vec::new();
    for (i, r) in rules.iter().enumerate().filter(|(_, r)| !r.matches.labels.is_empty()) {
        // A repository the rule is for, when it names one plainly.
        let repo = r.matches.repos.iter().find(|g| !g.contains('*')).cloned().unwrap_or_default();
        if let Some(v) = route_now(hub, leg(r.matches.labels.clone(), repo)).await
            && takers(&v).is_empty()
        {
            diagnostics.push(json!({ "rule": i + 1, "level": "warning", "message": format!("no runner connected now meets rule {} ({}): its legs would wait", i + 1, r.matches.labels.join(", ")) }));
        }
    }
    let since = crate::util::now_unix() - 7 * 86_400;
    let recent: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT labels FROM jobs WHERE queued_at >= ?1 GROUP BY labels ORDER BY MAX(queued_at) DESC LIMIT 20")?;
            let rows = st.query_map([since], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap_or_default();
    let mut preview = Vec::new();
    for text in recent {
        let labels: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
        if labels.is_empty() {
            continue;
        }
        if let Some(v) = route_now(hub, leg(labels.clone(), String::new())).await {
            preview.push(json!({ "labels": labels, "runners": takers(&v), "executors": v.get("executors"), "constraints": v.get("constraints") }));
        }
    }
    (diagnostics, preview)
}

/// `POST /api/ci/routing/explain` with a leg's routing (`labels`, `repo`, `job`, `workflow`,
/// `event`, `requires`): where it would go now, and why each runner can or can't take it.
pub async fn explain(hub: &Hub, body: Value) -> Result<Response, Fail> {
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off".into()));
    }
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let list = |k: &str| body.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
    let labels: Vec<String> = match body.get("labels") {
        Some(Value::String(l)) => vec![l.clone()],
        _ => list("labels"),
    };
    let route = LegRoute {
        job: s("job"),
        job_name: s("job"),
        workflow: s("workflow"),
        repo: s("repo"),
        event: if s("event").is_empty() { "push".into() } else { s("event") },
        labels,
        group: body.get("group").and_then(Value::as_str).map(str::to_owned),
        requires: list("requires"),
        caps: u64::from(crate::proto::CAP_CI2),
        // `untrusted: true`: where an untrusted leg would go (W18-R07).
        untrusted: body.get("untrusted").and_then(Value::as_bool).unwrap_or(false).then(|| hub.cfg.ci.untrusted.route()),
    };
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::ExplainRoute { route: Box::new(route), reply: tx });
    let late = || (StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned());
    let v = crate::rt::timeout(Duration::from_secs(5), rx.recv()).await.map_err(|_| late())?.map_err(|_| late())?;
    Ok(srv::json(StatusCode::OK, &v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w05_r21_scopes_order_and_limit_the_rules() -> Result<(), String> {
        let file = parse_file(b"- match: { labels: [ubuntu-latest] }\n  executor: [host]\n  require: ['os == linux']\n- match: { labels: [gpu], repos: ['other/*'] }\n  require: ['gpu.count >= 1']\n")?;
        assert_eq!(file.len(), 2);
        let mine = for_repo("acme/app", file.clone());
        assert!(mine.iter().all(|r| r.matches.repos == vec!["acme/app".to_owned()]), "a repository's rules reach only it");
        let org = for_org("acme", file);
        assert_eq!(org.len(), 1, "a rule naming another organization's repositories is dropped");
        assert_eq!(org.first().map(|r| r.matches.repos.clone()), Some(vec!["acme/*".to_owned()]));
        assert!(parse_file(b"rules:\n  - require: ['os ==']\n").is_err(), "a constraint that doesn't read");
        let s = Sources { config: vec![Rule::default()], repos: [("acme/app".to_owned(), mine)].into_iter().collect(), defaults: defaults(true), ..Sources::default() };
        let order: Vec<String> = s.labelled().into_iter().map(|(src, _)| src).collect();
        assert_eq!(order.first().map(String::as_str), Some("config"));
        assert_eq!(order.get(1).map(String::as_str), Some("repo:acme/app"));
        assert_eq!(order.last().map(String::as_str), Some("default"));
        Ok(())
    }

    /// The server's executor wins over a repository's, which wins over the defaults; a
    /// repository's constraints add to them.
    #[test]
    fn w05_r21_the_first_executor_wins_and_constraints_add_up() -> Result<(), String> {
        let server = parse_file(b"- match: { labels: [ubuntu-latest] }\n  executor: [docker]\n")?;
        let repo = for_repo("acme/app", parse_file(b"- match: { labels: [ubuntu-latest] }\n  executor: [host]\n  require: ['disk.free >= 10Gi']\n")?);
        let s = Sources { config: server, repos: [("acme/app".to_owned(), repo)].into_iter().collect(), defaults: defaults(true), ..Sources::default() };
        let leg = LegRoute { repo: "acme/app".into(), labels: vec!["ubuntu-latest".into()], ..LegRoute::default() };
        let req = routing::resolve(&leg, &s.effective())?;
        assert_eq!(req.executors, vec!["docker".to_owned()]);
        assert!(req.constraints.iter().any(|(t, _)| t == "disk.free >= 10Gi") && req.constraints.iter().any(|(t, _)| t == "os == linux"));
        let other = LegRoute { repo: "acme/other".into(), ..leg };
        assert!(!routing::resolve(&other, &s.effective())?.constraints.iter().any(|(t, _)| t == "disk.free >= 10Gi"), "another repository's rule doesn't reach it");
        Ok(())
    }
}
