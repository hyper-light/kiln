//! GitHub's webhooks into observe (`docs/observe.md` §8, §10): what kiln already takes on
//! `POST /api/hooks/github` (its signature checked, `X-Hub-Signature-256`) as events and metrics,
//! as Datadog's GitHub integration makes them:
//!
//! - `push`: an event, and `github.pushes` (a count) and `github.push.commits` tagged `repo`,
//!   `branch`;
//! - `pull_request`: an event, and `github.pull_requests` tagged `repo`, `action` (`merged` for one
//!   closed merged);
//! - `workflow_run` and `workflow_job` that completed: an event, and `github.actions.run.duration`
//!   / `github.actions.job.duration` (seconds) with `github.actions.runs` / `.jobs` (counts) tagged
//!   `repo`, `workflow` (`job`), `conclusion`, `branch`;
//! - `release` (published), `deployment_status`, `issues`: events, and counts.
//!
//! Every event is a record of source `kiln.event` and service `github`, its attributes the
//! payload's names (`github.event`, `github.action`, `repo`, `sender`, ...).

use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use serde_json::Value;

/// The value at `path` in `v`, null where there is none.
fn at<'a>(v: &'a Value, path: &[&str]) -> &'a Value {
    static NULL: Value = Value::Null;
    path.iter().try_fold(v, |v, k| v.get(*k)).unwrap_or(&NULL)
}

fn text(v: &Value, path: &[&str]) -> String {
    match at(v, path) {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// An RFC 3339 time, as GitHub writes them, in unix ms.
fn ms(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok().map(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(0))
}

/// What GitHub said, written: `event` its `X-GitHub-Event`, `body` its payload.
pub fn webhook(event: &str, body: &[u8]) {
    let Some(o) = super::api::get() else { return };
    let Ok(p) = serde_json::from_slice::<Value>(body) else { return };
    let now = crate::util::now_ms();
    let repo = text(&p, &["repository", "full_name"]);
    let sender = text(&p, &["sender", "login"]);
    let action = text(&p, &["action"]);
    let mut points: Vec<(SeriesKey, Vec<(i64, f64)>)> = Vec::new();
    let mut add = |metric: &str, kind: Kind, tags: &[(&str, &str)], ts: i64, v: f64| {
        if let Ok(k) = SeriesKey::new(metric, tags.iter().copied(), kind) {
            points.push((k, vec![(ts, v)]));
        }
    };
    let mut attrs: Vec<(String, String)> = vec![("event.type".into(), format!("github.{event}")), ("github.event".into(), event.into()), ("repo".into(), repo.clone()), ("sender".into(), sender.clone())];
    if !action.is_empty() {
        attrs.push(("github.action".into(), action.clone()));
    }
    let (message, severity, ts) = match event {
        "push" => {
            let branch = text(&p, &["ref"]).trim_start_matches("refs/heads/").to_owned();
            let commits = at(&p, &["commits"]).as_array().map_or(0, Vec::len);
            add("github.pushes", Kind::Count, &[("repo", &repo), ("branch", &branch)], now, 1.0);
            add("github.push.commits", Kind::Count, &[("repo", &repo), ("branch", &branch)], now, commits as f64);
            attrs.extend([("branch".into(), branch.clone()), ("commits".into(), commits.to_string()), ("url".into(), text(&p, &["compare"]))]);
            (format!("{sender} pushed {commits} commit(s) to {repo} {branch}"), 9, now)
        }
        "pull_request" => {
            let merged = at(&p, &["pull_request", "merged"]).as_bool() == Some(true);
            let act = if action == "closed" && merged { "merged".to_owned() } else { action.clone() };
            let number = text(&p, &["number"]);
            add("github.pull_requests", Kind::Count, &[("repo", &repo), ("action", &act)], now, 1.0);
            attrs.extend([("pr.number".into(), number.clone()), ("pr.title".into(), text(&p, &["pull_request", "title"])), ("url".into(), text(&p, &["pull_request", "html_url"])), ("branch".into(), text(&p, &["pull_request", "head", "ref"]))]);
            (format!("{sender} {act} #{number} on {repo}: {}", text(&p, &["pull_request", "title"])), 9, now)
        }
        "workflow_run" | "workflow_job" if action == "completed" => {
            let (obj, what) = if event == "workflow_run" { ("workflow_run", "run") } else { ("workflow_job", "job") };
            let conclusion = text(&p, &[obj, "conclusion"]);
            let workflow = if event == "workflow_run" { text(&p, &[obj, "name"]) } else { text(&p, &[obj, "workflow_name"]) };
            let branch = text(&p, &[obj, "head_branch"]);
            let started = ms(&text(&p, &[obj, if event == "workflow_run" { "run_started_at" } else { "started_at" }]));
            let finished = ms(&text(&p, &[obj, if event == "workflow_run" { "updated_at" } else { "completed_at" }])).unwrap_or(now);
            let job = text(&p, &[obj, "name"]);
            let mut tags: Vec<(&str, &str)> = vec![("repo", &repo), ("workflow", &workflow), ("conclusion", &conclusion), ("branch", &branch)];
            if event == "workflow_job" {
                tags.push(("job", &job));
            }
            if let Some(s) = started {
                add(&format!("github.actions.{what}.duration"), Kind::Gauge, &tags, finished, ((finished - s).max(0) as f64) / 1000.0);
            }
            add(&format!("github.actions.{what}s"), Kind::Count, &tags, finished, 1.0);
            attrs.extend([("workflow".into(), workflow.clone()), ("conclusion".into(), conclusion.clone()), ("branch".into(), branch), ("url".into(), text(&p, &[obj, "html_url"]))]);
            let failed = matches!(conclusion.as_str(), "failure" | "timed_out" | "startup_failure");
            (format!("{repo} {workflow}{} {conclusion}", if event == "workflow_job" { format!(" / {job}") } else { String::new() }), if failed { 17 } else { 9 }, finished)
        }
        "release" if action == "published" => {
            let tag = text(&p, &["release", "tag_name"]);
            add("github.releases", Kind::Count, &[("repo", &repo)], now, 1.0);
            attrs.extend([("release.tag".into(), tag.clone()), ("url".into(), text(&p, &["release", "html_url"]))]);
            (format!("{repo} released {tag}"), 9, now)
        }
        "deployment_status" => {
            let state = text(&p, &["deployment_status", "state"]);
            let env = text(&p, &["deployment", "environment"]);
            add("github.deployments", Kind::Count, &[("repo", &repo), ("environment", &env), ("state", &state)], now, 1.0);
            attrs.extend([("environment".into(), env.clone()), ("state".into(), state.clone())]);
            (format!("{repo} deployment to {env}: {state}"), if matches!(state.as_str(), "failure" | "error") { 17 } else { 9 }, now)
        }
        "issues" => {
            add("github.issues", Kind::Count, &[("repo", &repo), ("action", &action)], now, 1.0);
            attrs.extend([("issue.number".into(), text(&p, &["issue", "number"])), ("issue.title".into(), text(&p, &["issue", "title"]))]);
            (format!("{sender} {action} issue #{} on {repo}: {}", text(&p, &["issue", "number"]), text(&p, &["issue", "title"])), 9, now)
        }
        _ => return,
    };
    o.write(points);
    attrs.retain(|(_, v)| !v.is_empty());
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity, service: "github".into(), host: String::new(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }]);
}
