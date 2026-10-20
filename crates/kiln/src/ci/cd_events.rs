//! kiln's `deploy` event (docs/ci.md W03-R09, W15-R06): CD drives CI. As KilnCD publishes its
//! apps' statuses (`cd_apps`), an app that turns `synced` (in sync and healthy, or a sync of it
//! succeeding), `degraded`, or whose sync `failed`, raises `deploy` in the repository whose
//! `.kiln/deploy.yaml` defines it: that repository's default-branch workflows that take it run
//! (smoke tests after staging turns healthy, diagnostics when it degrades); one that turns
//! degraded wakes the agents attached to its environment (W20-R08's `degraded`).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use kiln_ci::trigger::{Event, Source};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::hub::Hub;

/// What an app was, as last published: in sync, healthy, its latest operation and that
/// operation's phase.
#[derive(Debug, Clone, Default, PartialEq)]
struct Seen {
    synced: bool,
    healthy: bool,
    degraded: bool,
    op: Option<(i64, String)>,
}

impl Seen {
    fn of(a: &Value) -> Seen {
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default();
        let op = a.get("operation").filter(|o| !o.is_null()).map(|o| (o.get("id").and_then(Value::as_i64).unwrap_or(0), o.get("phase").and_then(Value::as_str).unwrap_or_default().to_owned()));
        Seen { synced: s("sync") == "Synced", healthy: s("health") == "Healthy", degraded: s("health") == "Degraded", op }
    }
}

/// The state an app moved into, if it's one `deploy` fires on: `failed` (its sync), `degraded`, or
/// `synced`. The first status seen of an app only records it.
fn moved(before: Option<&Seen>, now: &Seen) -> Option<&'static str> {
    let before = before?;
    let new_op = now.op.as_ref().map(|(id, _)| *id) != before.op.as_ref().map(|(id, _)| *id);
    let phase = |s: &Seen| s.op.as_ref().map(|(_, p)| p.to_ascii_lowercase()).unwrap_or_default();
    let failed = |s: &Seen| matches!(phase(s).as_str(), "failed" | "error");
    if failed(now) && (new_op || !failed(before)) {
        return Some("failed");
    }
    if now.degraded && !before.degraded {
        return Some("degraded");
    }
    let succeeded_again = new_op && phase(now) == "succeeded";
    if now.synced && now.healthy && (!(before.synced && before.healthy) || succeeded_again) {
        return Some("synced");
    }
    None
}

static SEEN: LazyLock<Mutex<HashMap<String, Seen>>> = LazyLock::new(Default::default);

/// A `cd_apps` event from the hub: each app's move, as `deploy`.
pub async fn observe(hub: &Arc<Hub>, v: &Value) {
    let Some(apps) = v.get("apps").and_then(Value::as_array) else { return };
    let mut fired = Vec::new();
    {
        let mut seen = SEEN.lock();
        for a in apps {
            let Some(app) = a.get("app").and_then(Value::as_str) else { continue };
            let now = Seen::of(a);
            if let Some(state) = moved(seen.get(app), &now) {
                fired.push((app.to_owned(), state, a.clone()));
            }
            seen.insert(app.to_owned(), now);
        }
    }
    for (app, state, a) in fired {
        if state == "degraded"
            && let Some((repo, environment)) = home(hub, &app).await
        {
            super::attach::degraded(hub, &repo.name, &app, &environment, &a).await;
        }
        if let Err(e) = raise(hub, &app, state, &a).await {
            tracing::warn!(%app, %state, error = %format!("{e:#}"), "`deploy` started no workflows");
        }
    }
}

/// The repository whose `.kiln/deploy.yaml` makes `app`, and the environment it deploys to; none
/// for an app made otherwise (by API, or Argo CD's).
async fn home(hub: &Hub, app: &str) -> Option<(crate::store::Repo, String)> {
    let cd = crate::cd::get()?;
    let (origin, environment) = {
        let apps = cd.apps.read();
        let entry = apps.get(app)?;
        let origin = match &entry.def.origin {
            kiln_cd::apps::Origin::Deploy { repo } => repo.clone(),
            _ => return None,
        };
        (origin, entry.def.environment.clone().unwrap_or_default())
    };
    let repo = hub.store.repos().await.ok()?.into_iter().find(|r| r.name == origin || r.id == origin)?;
    Some((repo, environment))
}

/// `deploy` for `app` moving into `state`, in the repository that defines it.
async fn raise(hub: &Hub, app: &str, state: &str, status: &Value) -> anyhow::Result<()> {
    if !hub.ci.enabled() {
        return Ok(());
    }
    let Some((repo, environment)) = home(hub, app).await else { return Ok(()) };
    let f = super::events::facts(hub, &repo).await;
    let Ok((full_ref, sha)) = super::events::named_ref(hub, &repo, &f.default_branch).await else { return Ok(()) };
    let commit = super::load::load(hub, &repo, &sha).await?;
    let listening: Vec<&super::load::Loaded> = commit.workflows.iter().filter(|l| l.runs && !l.has_errors() && l.workflow.as_ref().is_some_and(|w| w.on.iter().any(|t| t.event == "deploy"))).collect();
    if listening.is_empty() {
        return Ok(());
    }
    let (message, _) = crate::gitops::commit_info(hub, &repo, &sha).await;
    let by = status.pointer("/operation/by").and_then(Value::as_str).filter(|b| !b.is_empty()).unwrap_or("kiln").to_owned();
    let payload = json!({
        "action": state,
        "repository": f.repo.payload(),
        "sender": { "login": by, "type": "User" },
        "kiln": { "deploy": {
            "app": app,
            "environment": environment,
            "state": state,
            "sync": status.get("sync"),
            "health": status.get("health"),
            "revision": status.get("revision"),
            "operation": status.get("operation"),
        } },
    });
    let e = Event { name: "deploy".into(), action: state.to_owned(), repo: repo.name.clone(), git_ref: full_ref, sha, actor: by.clone(), trusted: true, source: Source::Cd, payload, message: Some(message.clone()), ..Event::default() };
    for l in listening {
        super::events::start(hub, &repo, &e, l, &message, &by, Value::Null).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(sync: &str, health: &str, op: Option<(i64, &str)>) -> Seen {
        Seen::of(&json!({ "sync": sync, "health": health, "operation": op.map(|(id, phase)| json!({ "id": id, "phase": phase })) }))
    }

    #[test]
    fn w03_r09_deploy_states() {
        let healthy = seen("Synced", "Healthy", Some((1, "Succeeded")));
        assert_eq!(moved(None, &healthy), None, "the first status only records");
        assert_eq!(moved(Some(&seen("OutOfSync", "Progressing", Some((1, "Running")))), &healthy), Some("synced"));
        assert_eq!(moved(Some(&healthy), &healthy), None, "nothing moved");
        assert_eq!(moved(Some(&healthy), &seen("Synced", "Healthy", Some((2, "Succeeded")))), Some("synced"), "a sync of it succeeded again");
        assert_eq!(moved(Some(&healthy), &seen("Synced", "Degraded", Some((1, "Succeeded")))), Some("degraded"));
        assert_eq!(moved(Some(&seen("Synced", "Degraded", None)), &seen("Synced", "Degraded", None)), None, "still degraded");
        assert_eq!(moved(Some(&healthy), &seen("OutOfSync", "Healthy", Some((2, "Failed")))), Some("failed"));
        assert_eq!(moved(Some(&seen("OutOfSync", "Healthy", Some((2, "Failed")))), &seen("OutOfSync", "Healthy", Some((2, "Failed")))), None, "the same failure");
    }
}
