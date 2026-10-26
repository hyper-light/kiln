//! Fleet changes in apps' sources on kiln's own git server (docs/deploy-api.md §3.3): kiln's own
//! has no pull requests, so a change is a branch and its diff, which kiln merges when asked (a
//! merge commit where the repository moved meanwhile, a conflict refused); withdrawn, its branch
//! goes. An app without a source repository is skipped, and only someone with
//! environments.manage changes apps' sources. (On GitHub, GitLab and Bitbucket: each host's test,
//! through `common::fleet_on_host`.)

#![cfg(unix)]

mod common;

use common::{Options, git_as_ada, http, lay_chart, start_cd};
use serde_json::{Value, json};

#[test]
fn p5_fleet_changes_in_sources_on_kiln_s_own() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start_cd(dir.path(), &Options::default())?;
    let post = |path: &str, body: &Value| -> anyhow::Result<(u16, Value)> {
        let (st, text) = http(s.port, "POST", path, &s.admin, Some(body))?;
        Ok((st, serde_json::from_str(&text).unwrap_or(Value::Null)))
    };

    // An app whose repository its template makes on kiln's own, a chart laid in it; and one with none.
    let t = json!({ "name": "svc-src", "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "{app}" }, "repository": { "host": "kiln", "name": "acme/{app}", "branch": "main" } });
    let (st, v) = post("/api/deploy/templates", &t)?;
    anyhow::ensure!((200..300).contains(&st), "the template: {st} {v}");
    let (st, v) = post("/api/deploy/apps", &json!({ "name": "web", "project": "shop", "template": "svc-src" }))?;
    anyhow::ensure!(st == 201 && v.get("source") == Some(&json!("acme/web")), "web: {st} {v}\n{}", s.log());
    let (st, v) = post("/api/deploy/apps", &json!({ "name": "api", "project": "shop", "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "api" } }))?;
    anyhow::ensure!(st == 201, "api: {st} {v}");
    let bare = dir.path().join("data/hosted/acme/web.git");
    lay_chart(&bare, "web")?;
    let before = git_as_ada(&bare, &["rev-parse", "main"])?;

    // Only someone with environments.manage changes apps' sources.
    let (st, _) = post("/api/users", &json!({ "name": "bob", "grants": ["viewer", "environments.deploy"] }))?;
    anyhow::ensure!((200..300).contains(&st), "bob: {st}");
    let (st, v) = post("/api/tokens", &json!({ "kind": "pat", "user": "bob", "grants": ["owner"] }))?;
    anyhow::ensure!((200..300).contains(&st), "bob's token: {st} {v}");
    let bob = v.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, text) = http(s.port, "POST", "/api/deploy/fleets", &bob, Some(&json!({ "source": { "base": "0.1.9" } })))?;
    assert!(st == 403 && text.contains("environments.manage"), "{st} {text}");

    // kiln-base moved: a branch and its diff; the app without a source skipped; main as it was.
    let (st, v) = post("/api/deploy/fleets", &json!({ "apps": ["web", "api"], "source": { "base": "0.1.9" }, "idempotency_key": "kiln-base-019" }))?;
    anyhow::ensure!(st == 201, "{st} {v}\n{}", s.log());
    let p = v.pointer("/proposals/0").cloned().unwrap_or_default();
    assert_eq!((p.get("repo"), p.get("host"), p.get("state"), p.get("branch")), (Some(&json!("acme/web")), Some(&json!("kiln")), Some(&json!("open")), Some(&json!("kiln/fleet/kiln-base-019"))), "{v}");
    assert!(v.get("skipped").and_then(Value::as_array).is_some_and(|k| k.iter().any(|x| x.get("app") == Some(&json!("api")))), "{v}");
    let diff = p.get("diff").and_then(Value::as_str).unwrap_or_default();
    assert!(diff.contains("-    version: 0.1.2") && diff.contains("+    version: 0.1.9") && diff.contains("charts/web/Chart.yaml"), "{diff}");
    assert_eq!(git_as_ada(&bare, &["rev-parse", "main"])?, before, "nothing merged yet");

    // Someone moves main meanwhile; kiln merges the branch: a merge commit, both kept, the branch gone.
    let work = tempfile::tempdir()?;
    git_as_ada(work.path(), &["clone", "-q", &bare.to_string_lossy(), "."])?;
    std::fs::write(work.path().join("README.md"), "web\n")?;
    git_as_ada(work.path(), &["add", "."])?;
    git_as_ada(work.path(), &["commit", "-qm", "a readme"])?;
    git_as_ada(work.path(), &["push", "-q", "origin", "HEAD:main"])?;
    let (st, v) = post("/api/deploy/fleets/kiln-base-019/merge", &json!({}))?;
    anyhow::ensure!(st == 200, "merge: {st} {v}\n{}", s.log());
    assert!(v.pointer("/proposals/0/state") == Some(&json!("merged")) && v.get("done") == Some(&json!(true)), "{v}");
    assert!(git_as_ada(&bare, &["show", "main:charts/web/Chart.yaml"])?.contains("version: 0.1.9"));
    assert_eq!(git_as_ada(&bare, &["show", "main:README.md"])?, "web");
    assert_eq!(git_as_ada(&bare, &["rev-list", "--parents", "-n", "1", "main"])?.split(' ').count(), 3, "a merge commit of the two");
    assert!(git_as_ada(&bare, &["rev-parse", "--verify", "--quiet", "refs/heads/kiln/fleet/kiln-base-019"]).is_err(), "its branch goes");

    // Values set, while someone sets the same value on main: a conflict, refused, main as they left it.
    let (st, v) = post("/api/deploy/fleets", &json!({ "apps": ["web"], "source": { "set": { "replicaCount": "3" } }, "idempotency_key": "three" }))?;
    anyhow::ensure!(st == 201 && v.pointer("/proposals/0/state") == Some(&json!("open")), "{st} {v}");
    git_as_ada(work.path(), &["pull", "-q", "--no-rebase", "origin", "main"])?;
    std::fs::write(work.path().join("charts/web/values.yaml"), "replicaCount: 5\nimage:\n  repository: ghcr.io/acme/web\n")?;
    git_as_ada(work.path(), &["commit", "-qam", "five"])?;
    git_as_ada(work.path(), &["push", "-q", "origin", "HEAD:main"])?;
    let theirs = git_as_ada(&bare, &["rev-parse", "main"])?;
    let (st, text) = http(s.port, "POST", "/api/deploy/fleets/three/merge", &s.admin, Some(&json!({ "repo": "acme/web" })))?;
    assert!(st == 409 && text.contains("conflicts") && text.contains("charts/web/values.yaml"), "{st} {text}");
    assert_eq!(git_as_ada(&bare, &["rev-parse", "main"])?, theirs);

    // Withdrawn: closed, its branch gone.
    let (st, text) = http(s.port, "DELETE", "/api/deploy/fleets/three", &s.admin, None)?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    assert!(st == 200 && v.pointer("/proposals/0/state") == Some(&json!("closed")), "{st} {text}");
    assert!(git_as_ada(&bare, &["rev-parse", "--verify", "--quiet", "refs/heads/kiln/fleet/three"]).is_err());
    Ok(())
}
