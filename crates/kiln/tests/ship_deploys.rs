//! Ship's Deploys on a server of its own with KilnCD running (docs/ci.md W26-R24, W26-AC18): apps
//! of two repositories are rows by project, each as CD holds it; an app's pane has its settings and
//! its history, its source's commits each one a rollback can go to; and each of the strip's verbs
//! reaches its CD operation as the caller: done for a maintainer and audited as theirs in the
//! request's one row, the operation's own refusal said, refused for a viewer, and an agent held to
//! what its environment lets agents do. There's no cluster: what needs one says so.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{Options, Server, file_url, git_as_ada, http, start_cd};
use serde_json::{Value, json};

/// A repository at `dir` with two commits on main, deploy/ holding a ConfigMap: its commits,
/// newest first.
fn repo(dir: &Path) -> anyhow::Result<Vec<String>> {
    std::fs::create_dir_all(dir.join("deploy"))?;
    git_as_ada(dir, &["init", "-q"])?;
    let mut shas = Vec::new();
    for (n, color) in ["blue", "green"].iter().enumerate() {
        std::fs::write(dir.join("deploy/cm.yaml"), format!("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\ndata:\n  color: {color}\n"))?;
        git_as_ada(dir, &["add", "."])?;
        git_as_ada(dir, &["commit", "-q", "-m", &format!("Colour {n}: {color}")])?;
        shas.push(git_as_ada(dir, &["rev-parse", "HEAD"])?);
    }
    shas.reverse();
    Ok(shas)
}

/// An app kept in kiln, deploying `repo`'s deploy/ at main to a cluster kiln has no connection to.
fn app(name: &str, repo: &str, environment: &str) -> Value {
    json!({
        "name": name, "project": "default",
        "sources": [{ "repo": repo, "path": "deploy", "revision": "main", "chart": null, "as_ref": null, "engine": { "Directory": { "recurse": false, "include": null, "exclude": null } } }],
        "destination": { "cluster": "nowhere", "namespace": name },
        "sync": { "auto": false, "prune": true, "self_heal": false, "allow_empty": false, "options": { "create_namespace": true }, "retry": null },
        "ignore": [], "tracking": "Kiln", "origin": "Api", "environment": environment, "labels": {},
        "history_limit": 10, "cascade": true, "managed_namespace": null,
    })
}

fn call(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
    let (code, text) = http(s.port, method, path, token, body)?;
    Ok((code, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path).unwrap_or(&Value::Null)
}

/// A token for a new user with `grants`; `agent`: an agent's, acting for them.
fn token(s: &Server, user: &str, grants: &[&str], agent: Option<&str>) -> anyhow::Result<String> {
    let (st, body) = call(s, &s.admin, "POST", "/api/users", Some(&json!({ "name": user, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st) || st == 409, "user: {st} {body}");
    let ask = match agent {
        Some(a) => json!({ "kind": "agent", "agent": a, "user": user, "grants": grants }),
        None => json!({ "kind": "pat", "user": user, "grants": ["owner"] }),
    };
    let (st, body) = call(s, &s.admin, "POST", "/api/tokens", Some(&ask))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {body}");
    at(&body, "/token").as_str().map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// Asks until `done` says yes, or fails saying what it last saw.
fn until(within: Duration, what: &str, mut done: impl FnMut() -> anyhow::Result<(bool, Value)>) -> anyhow::Result<Value> {
    let start = Instant::now();
    loop {
        let (ok, seen) = done()?;
        if ok {
            return Ok(seen);
        }
        anyhow::ensure!(start.elapsed() < within, "{what}: never; last {seen}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Every row of the list, by app id, with the project it's under.
fn rows(list: &Value) -> Vec<(String, Value)> {
    at(list, "/projects").as_array().into_iter().flatten().flat_map(|p| at(p, "/apps").as_array().cloned().unwrap_or_default().into_iter().map(|r| (at(p, "/project").as_str().unwrap_or_default().to_owned(), r))).collect()
}

#[test]
fn w26_r24_ship_lists_cds_apps_by_project_and_each_verb_reaches_its_operation_as_the_caller() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start_cd(dir.path(), &Options::default())?;
    let kiln = dir.path().join("git/hyper-light/kiln");
    let quiver = dir.path().join("git/hyper-light/quiver");
    let kiln_commits = repo(&kiln)?;
    repo(&quiver)?;
    for (name, r, env) in [("kiln-production", &kiln, "production"), ("kiln-staging", &kiln, "staging"), ("quiver-production", &quiver, "production")] {
        let (st, body) = call(&s, &s.admin, "POST", "/api/cd/apps", Some(&app(name, &file_url(r), env)))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    }

    // The list: the apps of each repository under it, each as CD holds it.
    let list = until(Duration::from_secs(60), "three apps listed", || {
        let (st, v) = call(&s, &s.admin, "GET", "/api/ship/deploys", None)?;
        Ok((st == 200 && at(&v, "/total") == &json!(3), v))
    })?;
    let projects: Vec<(&str, usize)> = at(&list, "/projects").as_array().into_iter().flatten().map(|p| (at(p, "/project").as_str().unwrap_or_default(), at(p, "/apps").as_array().map_or(0, Vec::len))).collect();
    let mut sorted = projects.clone();
    sorted.sort();
    assert_eq!(sorted, [("hyper-light/kiln", 2), ("hyper-light/quiver", 1)], "{list}");
    let (_, cd) = call(&s, &s.admin, "GET", "/api/cd/apps", None)?;
    for (project, row) in rows(&list) {
        let id = at(&row, "/app").as_str().unwrap_or_default().to_owned();
        let held = at(&cd, "/apps").as_array().into_iter().flatten().find(|a| at(a, "/app").as_str() == Some(id.as_str())).cloned().unwrap_or_default();
        assert_eq!((at(&row, "/sync"), at(&row, "/health"), at(&row, "/auto_sync"), at(&row, "/environment")), (at(&held, "/sync"), at(&held, "/health"), at(&held, "/auto"), at(&held, "/environment")), "{id}: {row} against {held}");
        assert_eq!(project, if id.contains("quiver") { "hyper-light/quiver" } else { "hyper-light/kiln" });
        assert!(at(&row, "/stand").is_string() && at(&row, "/word").is_string() && at(&row, "/line").is_string() && at(&row, "/pips").as_array().is_some_and(|p| p.len() == 3), "{row}");
    }
    let (st, only) = call(&s, &s.admin, "GET", "/api/ship/deploys?q=staging", None)?;
    assert_eq!((st, rows(&only).iter().map(|(_, r)| at(r, "/name").clone()).collect::<Vec<_>>()), (200, vec![json!("kiln-staging")]));

    // The pane: its settings as CD says them, and its history once CD has read its repository:
    // main's commits newest first, none deployed, each one a rollback can go to.
    let app_id = "default/kiln-production";
    let pane = until(Duration::from_secs(90), "the pane's history", || {
        let (st, v) = call(&s, &s.admin, "GET", "/api/ship/deploys/default/kiln-production", None)?;
        Ok((st == 200 && at(&v, "/history").as_array().is_some_and(|h| h.len() == 2), v))
    })?;
    let history: Vec<(Value, Value, Value)> = at(&pane, "/history").as_array().into_iter().flatten().map(|l| (at(l, "/revision").clone(), at(l, "/deploy").clone(), at(l, "/subject").clone())).collect();
    assert_eq!(history, kiln_commits.iter().zip(["Colour 1: green", "Colour 0: blue"]).map(|(c, m)| (json!(c), json!(null), json!(m))).collect::<Vec<_>>());
    assert_eq!(at(&pane, "/settings/0"), &json!({ "key": "auto_sync", "on": false, "from": "deploy-file", "what": "syncs when you say", "can": true }));
    assert_eq!(at(&pane, "/about/source"), &json!("deploy @ main"));
    assert_eq!(at(&pane, "/app/app"), &json!(app_id));
    assert!(at(&pane, "/why_not/stop").is_string() && at(&pane, "/why_not/promote").is_string(), "{pane}");
    let (st, _) = call(&s, &s.admin, "GET", "/api/ship/deploys/default/nothing", None)?;
    assert_eq!(st, 404);

    // A maintainer's verbs: each its operation, audited as theirs in this request's one row.
    let maintainer = token(&s, "mia", &["maintainer"], None)?;
    let act = |token: &str, verb: &str, extra: Value| -> anyhow::Result<(u16, Value)> {
        let mut body = json!({ "app": app_id, "verb": verb });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        call(&s, token, "POST", "/api/ship/deploys", Some(&body))
    };
    let (st, v) = act(&maintainer, "auto_sync", json!({ "on": true }))?;
    assert_eq!((st, at(&v, "/op"), at(&v, "/done/on"), at(&v, "/done/from")), (200, &json!("cd.apps.set_auto_sync"), &json!(true), &json!("app")), "{v}");
    let (_, held) = call(&s, &s.admin, "GET", "/api/cd/apps/default/kiln-production/auto-sync", None)?;
    assert_eq!((at(&held, "/on"), at(&held, "/from")), (&json!(true), &json!("app")), "{held}");
    let (_, audit) = call(&s, &s.admin, "GET", "/api/audit?target=/api/ship/deploys&limit=10", None)?;
    let mine: Vec<&Value> = audit.as_array().into_iter().flatten().collect();
    assert_eq!(mine.len(), 1, "{audit}");
    let row = mine.first().copied().cloned().unwrap_or_default();
    assert!(at(&row, "/on_behalf_of") == &json!("user:mia") && at(&row, "/detail").as_str().is_some_and(|d| d.contains("cd.auto_sync")) && at(&row, "/result") == &json!("200"), "{row}");
    let list = until(Duration::from_secs(10), "auto-sync shown", || {
        let (_, v) = call(&s, &s.admin, "GET", "/api/ship/deploys", None)?;
        let on = rows(&v).iter().any(|(_, r)| at(r, "/app") == &json!(app_id) && at(r, "/auto_sync") == &json!(true));
        Ok((on, v))
    })?;
    assert!(rows(&list).iter().any(|(_, r)| at(r, "/app") == &json!(app_id)));
    let (st, v) = act(&maintainer, "refresh", json!({ "hard": true }))?;
    assert_eq!((st, at(&v, "/done/refreshing")), (200, &json!(app_id)), "{v}");
    let (st, v) = act(&maintainer, "stop", json!({}))?;
    assert_eq!((st, at(&v, "/done/terminating")), (200, &json!(app_id)), "{v}");
    let (st, v) = act(&maintainer, "auto_rollback", json!({ "on": false }))?;
    assert_eq!((st, at(&v, "/done/on")), (200, &json!(false)), "{v}");
    let (st, v) = act(&maintainer, "follow", json!({}))?;
    assert_eq!((st, at(&v, "/done/following")), (200, &json!(app_id)), "{v}");
    // What the operation itself refuses, it says: no rollout to promote; no cluster to render a
    // rollback for; a verb that isn't one.
    let (st, v) = act(&maintainer, "promote", json!({}))?;
    assert!(st == 404 && at(&v, "/error").as_str().is_some_and(|m| m.contains("has no rollout")), "{st} {v}");
    let older = kiln_commits.last().cloned().unwrap_or_default();
    let (st, v) = act(&maintainer, "rollback", json!({ "revision": older }))?;
    assert!(st == 503 && at(&v, "/error").as_str().is_some_and(|m| m.contains("nowhere")), "{st} {v}");
    let (st, _) = act(&maintainer, "launch", json!({}))?;
    assert_eq!(st, 400);

    // A viewer reads the list and the pane, and acts on nothing.
    let viewer = token(&s, "vic", &["viewer"], None)?;
    let (st, _) = call(&s, &viewer, "GET", "/api/ship/deploys", None)?;
    assert_eq!(st, 200);
    for verb in ["sync", "refresh", "rollback", "stop", "follow", "promote", "abort", "auto_sync", "auto_rollback", "delete"] {
        let (st, v) = act(&viewer, verb, json!({ "on": true }))?;
        assert!(st == 403 && v.to_string().contains("environments.deploy"), "{verb}: {st} {v}");
    }
    // An agent, where its environment doesn't let agents deploy: the operation refuses it.
    let agent = token(&s, "ada", &["maintainer"], Some("fixer"))?;
    let (st, v) = act(&agent, "sync", json!({}))?;
    assert!(st == 403 && v.to_string().contains("agents may not sync"), "{st} {v}");
    // Delete, as the Settings group's last line asks it: the app made through the API goes, its
    // objects left where they are, and the list no longer has it.
    let body = json!({ "app": "default/kiln-staging", "verb": "delete", "cascade": "orphan" });
    let (st, v) = call(&s, &maintainer, "POST", "/api/ship/deploys", Some(&body))?;
    assert_eq!((st, at(&v, "/op"), at(&v, "/done/deleted"), at(&v, "/done/cascade")), (200, &json!("cd.apps.delete"), &json!("default/kiln-staging"), &json!(false)), "{v}");
    until(Duration::from_secs(30), "two apps listed", || {
        let (st, v) = call(&s, &s.admin, "GET", "/api/ship/deploys", None)?;
        Ok((st == 200 && at(&v, "/total") == &json!(2), v))
    })?;
    // Gone, it isn't there to delete again.
    let (st, _) = call(&s, &maintainer, "POST", "/api/ship/deploys", Some(&body))?;
    assert_eq!(st, 404);
    Ok(())
}
