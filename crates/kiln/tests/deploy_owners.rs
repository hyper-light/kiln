//! Owners as groups (docs/deploy-api.md §3.1, §15), over HTTP: kiln's groups and their members
//! (`/api/groups`, `users.manage`'s), a person's groups in `/api/me`, an app made and changed only
//! with an owner team its maker is in, a group that owns apps kept, and the deploy routes letting
//! a person in a group through as far as their app's owner teams and its environment allow, a
//! deployer of one environment there only, and anyone else refused at the door.

#![cfg(unix)]

mod common;

use common::{Options, Server, http, start_cd};
use serde_json::{Value, json};

/// A person with these grants, and a token of theirs that does all they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!(st == 201 || st == 200, "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

#[test]
fn p5_owners_as_groups() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start_cd(dir.path(), &Options::default())?;
    let ada = person(&s, "ada", &["viewer", "environments.manage"])?;
    let bob = person(&s, "bob", &["viewer", "environments.manage"])?;
    let eve = person(&s, "eve", &["viewer"])?;
    let dave = person(&s, "dave", &["viewer", "environments.deploy@deploy:staging"])?;

    // kiln's groups: an administrator's to make and fill.
    let (st, _) = http(s.port, "POST", "/api/groups", &ada, Some(&json!({ "name": "shop" })))?;
    assert_eq!(st, 403, "making a group takes users.manage");
    let (st, body) = http(s.port, "POST", "/api/groups", &s.admin, Some(&json!({ "name": "shop", "description": "the shop team", "members": ["ada"] })))?;
    anyhow::ensure!(st == 201, "shop: {st} {body}");
    let made: Value = serde_json::from_str(&body)?;
    assert_eq!(made.get("members"), Some(&json!(["ada"])), "{made}");
    assert_eq!(http(s.port, "POST", "/api/groups", &s.admin, Some(&json!({ "name": "SHOP" })))?.0, 409, "a name in any case is taken");
    assert_eq!(http(s.port, "POST", "/api/groups", &s.admin, Some(&json!({ "name": "the shop" })))?.0, 400);
    assert_eq!(http(s.port, "POST", "/api/groups", &s.admin, Some(&json!({ "name": "x", "members": ["nobody"] })))?.0, 404, "a member who isn't a user");
    assert_eq!(http(s.port, "PUT", "/api/groups/shop/members/bob", &ada, None)?.0, 403);
    assert_eq!(http(s.port, "PUT", "/api/groups/shop/members/bob", &s.admin, None)?.0, 204);
    let (_, body) = http(s.port, "GET", "/api/groups/shop", &s.admin, None)?;
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("members"), Some(&json!(["ada", "bob"])), "{body}");
    assert_eq!(http(s.port, "DELETE", "/api/groups/shop/members/bob", &s.admin, None)?.0, 204);
    assert_eq!(http(s.port, "DELETE", "/api/groups/shop/members/bob", &s.admin, None)?.0, 404, "out already");
    // A person's groups: the owner teams they may name.
    let (_, body) = http(s.port, "GET", "/api/me", &ada, None)?;
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("groups"), Some(&json!(["shop"])), "{body}");

    // Staging lets an app's owners release it; production doesn't.
    for (name, owners) in [("staging", true), ("production", false)] {
        let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&json!({ "name": name, "provider": "kilncd", "owners": { "release": owners } })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    }
    let (_, body) = http(s.port, "GET", "/api/deploy/environments/staging", &s.admin, None)?;
    assert_eq!(serde_json::from_str::<Value>(&body)?.pointer("/owners/release"), Some(&json!(true)), "{body}");

    // An app's owner team is one its maker is in.
    let app = |name: &str, owners: Value| json!({ "name": name, "project": "shop", "owners": owners, "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": name } });
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &bob, Some(&app("web", json!(["shop"]))))?;
    assert!(st == 403 && body.contains("isn't in shop"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &ada, Some(&app("web", json!([]))))?;
    assert!(st == 403 && body.contains("owner team"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &ada, Some(&app("web", json!(["slack:acme"]))))?;
    assert!(st == 400 && body.contains("isn't a group"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &ada, Some(&app("web", json!(["shop", "github:acme/platform"]))))?;
    anyhow::ensure!(st == 201, "web: {st} {body}");
    // Changing them: someone in the team it has.
    let (st, body) = http(s.port, "PATCH", "/api/deploy/apps/web", &bob, Some(&json!({ "owners": ["payments"] })))?;
    assert!(st == 403, "{st} {body}");
    // An administrator gives any.
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&app("pay", json!(["payments"]))))?;
    anyhow::ensure!(st == 201, "pay: {st} {body}");
    // A group that owns apps stays.
    let (st, body) = http(s.port, "DELETE", "/api/groups/shop", &s.admin, None)?;
    assert!(st == 409 && body.contains("web"), "{st} {body}");

    for env in ["staging", "production"] {
        let (st, body) = http(s.port, "POST", &format!("/api/deploy/apps/web/environments/{env}"), &s.admin, Some(&json!({})))?;
        anyhow::ensure!(st == 201, "register in {env}: {st} {body}");
    }
    let ask = |env: &str| json!({ "app": "web", "environment": env, "values": "replicaCount: 2\n" });

    // At the door: in no group and deploying nowhere, refused before the request is read.
    for (path, body) in [("/api/deploy/releases", json!({})), ("/api/deploy/releases", ask("staging")), ("/api/deploy/releases/1/cancel", json!({})), ("/api/deploy/apps/web/environments/staging/actions", json!({}))] {
        let (st, said) = http(s.port, "POST", path, &eve, Some(&body))?;
        assert!(st == 403 && said.contains("environments.deploy"), "eve {path}: {st} {said}");
    }
    let (st, said) = http(s.port, "POST", "/api/deploy/fleets", &eve, Some(&json!({ "environment": "staging", "set": { "a": "1" } })))?;
    assert_eq!(st, 403, "{said}");

    // ada, in web's owner team: production doesn't let owners release; staging does.
    let (st, body) = http(s.port, "POST", "/api/deploy/releases", &ada, Some(&ask("production")))?;
    assert!(st == 403 && body.contains("doesn't let owners release"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/releases", &ada, Some(&ask("staging")))?;
    assert!(st != 403 && st != 401, "an owner releases in staging: {st} {body}");
    // Nor does an owner's release reach across the environment.
    let (st, body) = http(s.port, "POST", "/api/deploy/fleets", &ada, Some(&json!({ "environment": "staging", "set": { "a": "1" } })))?;
    assert_eq!(st, 403, "a fleet is a deployer's: {body}");

    // dave deploys in staging only, whoever owns the app.
    let (st, body) = http(s.port, "POST", "/api/deploy/releases", &dave, Some(&ask("production")))?;
    assert!(st == 403 && body.contains("deploy:production"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/fleets", &dave, Some(&json!({ "environment": "production", "set": { "a": "1" } })))?;
    assert_eq!(st, 403, "{body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/fleets", &dave, Some(&json!({ "environment": "staging", "set": { "a": "1" } })))?;
    assert!(st != 403 && st != 401, "a deployer of staging changes its fleet: {st} {body}");
    Ok(())
}
