//! `render`'s I/O half (docs/cd-targets.md §3): Render's REST API. The pure half,
//! `kiln_cd::providers::render`, renders the objects.
//!
//! A release deploys its image (or commit) to the service and waits until the deploy is live, or
//! failed. kiln keeps the deploys it made by what they ran, so a rollback to one of them is
//! Render's rollback to that deploy, which builds nothing.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::render::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::hosting::{Api, HostingError};
use crate::http::client::Method;

/// The longest a deploy takes to go live.
const LIVE: std::time::Duration = std::time::Duration::from_secs(1800);
/// How many of its deploys kiln keeps to roll back to.
const KEEP: usize = 20;

fn api<'a>(cx: &'a Cx<'_>) -> Result<Api<'a>> {
    Ok(Api { kind: "render", token: cx.token()?.0 })
}

async fn call(cx: &Cx<'_>, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
    cx.conn.budget().await;
    api(cx)?.call(method, path, body, &[]).await
}

/// A service by its name.
async fn service(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let found = call(cx, Method::Get, &format!("/v1/services?name={}&limit=20", encode(name, false)), None).await?;
    Ok(found.as_array().into_iter().flatten().filter_map(|x| x.get("service")).find(|s| s.get("name").and_then(Value::as_str) == Some(name)).cloned())
}

/// A service's newest deploys, newest first.
async fn deploys(cx: &Cx<'_>, id: &str) -> Result<Vec<Value>> {
    let page = call(cx, Method::Get, &format!("/v1/services/{id}/deploys?limit=20"), None).await?;
    Ok(page.as_array().into_iter().flatten().filter_map(|x| x.get("deploy").cloned()).collect())
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(svc) = service(cx, name).await? else { return Ok(None) };
    let id = svc.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let ds = deploys(cx, &id).await?;
    let rec = cx.applied(GROUP, "Service", "", name);
    let ours = rec.as_ref().and_then(|r| r.made.get("deploy").and_then(Value::as_str).map(|d| (d.to_owned(), r.object.clone())));
    Ok(p::live_service(&svc, &ds, ours.as_ref().map(|(d, o)| (d.as_str(), o))))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "Service" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the workspace holds: its services (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, Method::Get, "/v1/services?limit=100", None).await?;
    let out: Vec<Value> = page
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.get("service"))
        .map(|s| json!({ "kind": "Service", "name": s.get("name"), "summary": format!("{} · {}", s.get("type").and_then(Value::as_str).unwrap_or_default(), s.pointer("/serviceDetails/url").and_then(Value::as_str).unwrap_or_default()), "updated": s.get("updatedAt") }))
        .collect();
    Ok(json!({ "resources": out }))
}

/// A service as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(live) = live(cx, name).await? else { bail!("no service {name} here") };
    let mut def = Map::new();
    def.insert("service".into(), json!(name));
    for (k, v) in live.get("spec").and_then(Value::as_object).into_iter().flatten() {
        def.insert(k.clone(), v.clone());
    }
    Ok(json!({ "definition": def, "live": live }))
}

/// Waits for a deploy to go live: why, if it didn't.
async fn settle(cx: &Cx<'_>, service: &str, id: &str, what: &str) -> Result<()> {
    let d = until(cx.conn, LIVE, what, || async {
        let d = api(cx)?.call(Method::Get, &format!("/v1/services/{service}/deploys/{id}"), None, &[]).await?;
        let s = d.get("status").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(matches!(s.as_str(), "live" | "deactivated" | "build_failed" | "update_failed" | "pre_deploy_failed" | "canceled").then_some(d))
    })
    .await?;
    match d.get("status").and_then(Value::as_str).unwrap_or_default() {
        "live" => Ok(()),
        s => bail!("{what}: its deploy is {s}"),
    }
}

/// Applies a Service: its release deployed unless it serves already; a release kiln deployed
/// before is rolled back to.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let svc = service(cx, name).await?.ok_or_else(|| anyhow::anyhow!("service {name} isn't on Render: make it there first, and kiln deploys to it"))?;
    let id = svc.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let want = kiln_cd::obj::hash_value(desired.get("spec").unwrap_or(&Value::Null)).to_string();
    let prev = cx.applied(GROUP, "Service", "", name).map(|a| a.made).unwrap_or(Value::Null);
    let serving = deploys(cx, &id).await?.into_iter().find(|d| d.get("status").and_then(Value::as_str) == Some("live")).and_then(|d| d.get("id").and_then(Value::as_str).map(str::to_owned));
    let mut made: Map<String, Value> = prev.get("deploys").and_then(Value::as_object).cloned().unwrap_or_default();
    let what = format!("deploying {name}");
    let deploy = if prev.get("release").and_then(Value::as_str) == Some(want.as_str()) && serving.is_some() && serving.as_deref() == prev.get("deploy").and_then(Value::as_str) {
        serving.unwrap_or_default()
    } else {
        let d = match made.get(&want).and_then(Value::as_str) {
            // A deploy kiln made of this release before: Render rolls back to it.
            Some(before) => match call(cx, Method::Post, &format!("/v1/services/{id}/rollback"), Some(&json!({ "deployId": before }))).await {
                Ok(d) => d,
                Err(e) if HostingError::is_not_found(&e) => deploy_new(cx, &id, desired).await?,
                Err(e) => return Err(e),
            },
            None => deploy_new(cx, &id, desired).await?,
        };
        let did = d.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Render answered no deploy"))?.to_owned();
        cx.record(GROUP, "Service", "", name, Some(applied(desired, json!({ "release": want, "deploy": did, "deploys": made })))).await?;
        settle(cx, &id, &did, &what).await?;
        did
    };
    made.insert(want.clone(), json!(deploy));
    while made.len() > KEEP {
        let Some(k) = made.keys().next().cloned() else { break };
        made.remove(&k);
    }
    cx.record(GROUP, "Service", "", name, Some(applied(desired, json!({ "release": want, "deploy": deploy, "deploys": made })))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("service {name} is gone"))
}

/// A new deploy of what `desired` runs.
async fn deploy_new(cx: &Cx<'_>, id: &str, desired: &Value) -> Result<Value> {
    let mut body = json!({ "clearCache": "do_not_clear" });
    if let (Some(i), Value::Object(m)) = (desired.pointer("/spec/image").and_then(Value::as_str), &mut body) {
        m.insert("imageUrl".into(), json!(i));
    }
    if let (Some(c), Value::Object(m)) = (desired.pointer("/spec/commit").and_then(Value::as_str), &mut body) {
        m.insert("commitId".into(), json!(c));
    }
    call(cx, Method::Post, &format!("/v1/services/{id}/deploys"), Some(&body)).await
}

/// kiln deploys to a service made on Render: deleting the object leaves it.
pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    tracing::info!(target = %cx.conn.target.name(), service = %key.name, "deployments: kiln leaves a Render service it deploys to; delete it on Render");
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

    /// A service on a faked Render, release by release: the first deploys its image, which goes
    /// live; the same release again deploys nothing; the next deploys its own; the release
    /// before, rolled back to, is Render's rollback to its deploy, built again from nothing; one
    /// that fails to start fails and reads as degraded, the deploy before live; then the service
    /// is left as it was, made on Render.
    #[test]
    fn services_deploy_each_releases_image() -> anyhow::Result<()> {
        let _one = fake_hosting::HOSTING.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(FakeHosting::default());
            fake_hosting::start(fake.clone()).await?;
            fake.state.lock().render_service("shop-web");
            let (target, place) = fake_hosting::target("render")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: fake_hosting::TOKEN.into(), config: json!({}) }, records: &records, files: &files };
            let spec = json!({ "service": "shop-web", "image": "docker.io/acme/web@${{ release.artifacts.image.digest }}" });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("render", &place, spec.clone(), version, &[], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no service"))
            };
            let names = [(String::new(), "shop-web".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let last_call = || fake.state.lock().calls.last().cloned().unwrap_or_default();

            let r1 = release("1.0.0", "sha256:aaaa")?;
            let up = apply(&cx, &r1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&r1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(fake.state.lock().calls, ["Deploy shop-web docker.io/acme/web@sha256:aaaa"]);
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(fake.state.lock().calls.len(), 1, "the same release again deploys nothing");

            let r2 = release("1.1.0", "sha256:bbbb")?;
            let up2 = apply(&cx, &r2).await?;
            assert_eq!((h(&up2), last_call()), (Some(Health::Healthy), "Deploy shop-web docker.io/acme/web@sha256:bbbb".to_owned()));

            // Rolled back to the release before: Render's rollback to its deploy.
            let back = apply(&cx, &r1).await?;
            assert!(last_call().starts_with("Rollback shop-web dep-"), "{}", last_call());
            assert!(synced(&r1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One that fails to start: it fails, read as degraded, the deploy before live.
            let failed = apply(&cx, &release("1.2.0", "sha256:broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("update_failed"), "{failed}");
            let down = list(&cx, "Service", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            assert!(synced(&r1, &cx.with_applied(down.clone())), "the deploy before serves: {down}");

            // Forward again to the release after: rolled back to its deploy too.
            apply(&cx, &r2).await?;
            assert!(last_call().starts_with("Rollback shop-web dep-"));

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-web")));
            let adopted = adopt(&cx, "shop-web").await?;
            assert_eq!(adopted.pointer("/definition/image"), Some(&json!("docker.io/acme/web@sha256:bbbb")), "{adopted}");
            delete(&cx, &key_of(&r2)).await?;
            assert!(!list(&cx, "Service", &names).await?.is_empty(), "the service stays");
            Ok(())
        })
    }
}
