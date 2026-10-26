//! `digitalocean`'s I/O half (docs/cd-targets.md §3): App Platform's API. The pure half,
//! `kiln_cd::providers::digitalocean`, renders the objects.
//!
//! A release makes the app, or sets its spec, and waits for the deployment that makes: active, or
//! its error. kiln keeps the deployments it made by release, so a rollback to one of them is App
//! Platform's rollback to that deployment.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::digitalocean::{self as p, GROUP};
use kiln_cd::providers::gcp_run::covers;
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::hosting::{Api, HostingError};
use crate::http::client::Method;

/// The longest a deployment takes.
const ACTIVE: std::time::Duration = std::time::Duration::from_secs(1800);
/// How many of its deployments kiln keeps to roll back to.
const KEEP: usize = 20;

fn api<'a>(cx: &'a Cx<'_>) -> Result<Api<'a>> {
    Ok(Api { kind: "digitalocean", token: cx.token()?.0 })
}

async fn call(cx: &Cx<'_>, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
    cx.conn.budget().await;
    api(cx)?.call(method, path, body, &[]).await
}

/// An app by its name.
async fn app(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let page = call(cx, Method::Get, "/v2/apps?per_page=200", None).await?;
    Ok(page.get("apps").and_then(Value::as_array).into_iter().flatten().find(|a| a.pointer("/spec/name").and_then(Value::as_str) == Some(name)).cloned())
}

async fn deployments(cx: &Cx<'_>, id: &str) -> Result<Vec<Value>> {
    Ok(call(cx, Method::Get, &format!("/v2/apps/{id}/deployments?per_page=20"), None).await?.get("deployments").and_then(Value::as_array).cloned().unwrap_or_default())
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(a) = app(cx, name).await? else { return Ok(None) };
    let ds = deployments(cx, a.get("id").and_then(Value::as_str).unwrap_or_default()).await?;
    Ok(p::live_app(&a, &ds))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "App" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the account holds: its apps (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, Method::Get, "/v2/apps?per_page=200", None).await?;
    let out: Vec<Value> = page.get("apps").and_then(Value::as_array).into_iter().flatten().map(|a| json!({ "kind": "App", "name": a.pointer("/spec/name"), "summary": a.get("live_url"), "updated": a.get("updated_at") })).collect();
    Ok(json!({ "resources": out }))
}

/// An app as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(a) = app(cx, name).await? else { bail!("no app {name} here") };
    Ok(json!({ "definition": { "app": name, "spec": a.get("spec") }, "note": "kept in the source as .do/app.yaml, the spec reads from there by default" }))
}

/// Waits for an app's deployment to end: why, if it didn't go active.
async fn settle(cx: &Cx<'_>, id: &str, deployment: &str, what: &str) -> Result<()> {
    let d = until(cx.conn, ACTIVE, what, || async {
        let d = api(cx)?.call(Method::Get, &format!("/v2/apps/{id}/deployments/{deployment}"), None, &[]).await?;
        let d = d.get("deployment").cloned().unwrap_or(Value::Null);
        Ok(matches!(d.get("phase").and_then(Value::as_str), Some("ACTIVE" | "SUPERSEDED" | "ERROR" | "CANCELED")).then_some(d))
    })
    .await?;
    match d.get("phase").and_then(Value::as_str).unwrap_or_default() {
        "ACTIVE" | "SUPERSEDED" => Ok(()),
        p => bail!("{what}: its deployment is {p}"),
    }
}

/// Applies an App: made, or its spec set, unless it runs the release already; a release kiln
/// deployed before is rolled back to.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    let want = kiln_cd::obj::hash_value(&spec).to_string();
    let prev = cx.applied(GROUP, "App", "", name).map(|a| a.made).unwrap_or(Value::Null);
    let mut made: Map<String, Value> = prev.get("deployments").and_then(Value::as_object).cloned().unwrap_or_default();
    let what = format!("deploying app {name}");
    let (id, deployment) = match app(cx, name).await? {
        None => {
            let a = call(cx, Method::Post, "/v2/apps", Some(&json!({ "spec": spec }))).await?;
            let a = a.get("app").cloned().unwrap_or(Value::Null);
            let id = a.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let d = a.pointer("/pending_deployment/id").or_else(|| a.pointer("/in_progress_deployment/id")).and_then(Value::as_str).map(str::to_owned);
            (id, d)
        }
        Some(a) => {
            let id = a.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let active = a.pointer("/active_deployment/id").and_then(Value::as_str).map(str::to_owned);
            let runs = prev.get("release").and_then(Value::as_str) == Some(want.as_str()) && active.is_some() && active.as_deref() == prev.get("deployment").and_then(Value::as_str) && a.get("spec").is_some_and(|have| covers(have, &spec));
            if runs {
                (id, None)
            } else {
                let d = match made.get(&want).and_then(Value::as_str) {
                    // A deployment kiln made of this release before: App Platform rolls back to it.
                    Some(before) => match call(cx, Method::Post, &format!("/v2/apps/{id}/rollback"), Some(&json!({ "deployment_id": before, "skip_pin": true }))).await {
                        Ok(d) => d.pointer("/deployment/id").and_then(Value::as_str).map(str::to_owned),
                        Err(e) if HostingError::is_not_found(&e) => None,
                        Err(e) => return Err(e),
                    },
                    None => None,
                };
                let d = match d {
                    Some(d) => Some(d),
                    None => {
                        let a = call(cx, Method::Put, &format!("/v2/apps/{id}"), Some(&json!({ "spec": spec }))).await?;
                        a.pointer("/app/pending_deployment/id").or_else(|| a.pointer("/app/in_progress_deployment/id")).and_then(Value::as_str).map(str::to_owned)
                    }
                };
                (id, d)
            }
        }
    };
    let deployment = match deployment {
        Some(d) => {
            cx.record(GROUP, "App", "", name, Some(applied(desired, json!({ "release": want, "deployment": d, "deployments": made })))).await?;
            settle(cx, &id, &d, &what).await?;
            d
        }
        None => prev.get("deployment").and_then(Value::as_str).unwrap_or_default().to_owned(),
    };
    made.insert(want.clone(), json!(deployment));
    while made.len() > KEEP {
        let Some(k) = made.keys().next().cloned() else { break };
        made.remove(&k);
    }
    cx.record(GROUP, "App", "", name, Some(applied(desired, json!({ "release": want, "deployment": deployment, "deployments": made })))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("app {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if let Some(a) = app(cx, &key.name).await? {
        let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
        call(cx, Method::Delete, &format!("/v2/apps/{id}"), None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_hosting::{self, FakeHosting};
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    const APP_YAML: &str = "name: placeholder\nservices:\n- name: web\n  http_port: 8080\n  envs:\n  - key: MODE\n    value: prod\n";

    /// An app on a faked App Platform, release by release: the first makes the app from
    /// `.do/app.yaml` with the release's image, and its deployment goes active; the same release
    /// again changes nothing; the next sets its spec; the release before, rolled back to, is App
    /// Platform's rollback to its deployment; one whose deployment errs reads as degraded; then
    /// the app goes.
    #[test]
    fn apps_deploy_each_releases_spec() -> anyhow::Result<()> {
        let _one = fake_hosting::HOSTING.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(FakeHosting::default());
            fake_hosting::start(fake.clone()).await?;
            let (target, place) = fake_hosting::target("digitalocean")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: fake_hosting::TOKEN.into(), config: json!({}) }, records: &records, files: &files };
            let spec = json!({ "app": "shop", "image": "registry.digitalocean.com/acme/web@${{ release.artifacts.image.digest }}" });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("digitalocean", &place, spec.clone(), version, &[(".do/app.yaml", APP_YAML)], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no app"))
            };
            let names = [(String::new(), "shop".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let last_call = || fake.state.lock().calls.last().cloned().unwrap_or_default();

            let a1 = release("1.0.0", "sha256:aaaa")?;
            assert_eq!(a1.pointer("/spec/services/0/image"), Some(&json!({ "registry_type": "DOCR", "registry": "acme", "repository": "web", "digest": "sha256:aaaa" })));
            let up = apply(&cx, &a1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&a1, &cx.with_applied(up.clone())), "in sync, App Platform's defaults aside: {up}");
            assert_eq!(fake.state.lock().calls, ["CreateApp shop"]);
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(fake.state.lock().calls.len(), 1, "the same release again changes nothing");

            let a2 = release("1.1.0", "sha256:bbbb")?;
            let up2 = apply(&cx, &a2).await?;
            assert_eq!((h(&up2), last_call()), (Some(Health::Healthy), "UpdateApp shop".to_owned()));

            // Rolled back to the release before: App Platform's rollback to its deployment.
            let back = apply(&cx, &a1).await?;
            assert!(last_call().starts_with("RollbackApp shop dep-"), "{}", last_call());
            assert!(synced(&a1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One whose deployment errs: it fails, and reads as degraded.
            let failed = apply(&cx, &release("1.2.0", "sha256:broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("ERROR"), "{failed}");
            assert_eq!(h(&list(&cx, "App", &names).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded));

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop")));
            delete(&cx, &key_of(&a1)).await?;
            assert!(list(&cx, "App", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
