//! `fly`'s I/O half (docs/cd-targets.md §3): Fly.io's Machines API. The pure half,
//! `kiln_cd::providers::fly`, renders the objects.
//!
//! A release updates the app's machines one at a time, as flyctl's rolling strategy does: each
//! under a lease (so no one else changes it meanwhile), waited for until it started on its new
//! config and its checks pass, the lease let go either way. A machine that doesn't start stops
//! the rollout there. Then machines are made or destroyed to the count.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::fly::{self as p, GROUP};
use kiln_cd::providers::gcp_run::covers;
use serde_json::{Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::hosting::{Api, HostingError};
use crate::http::client::Method;

/// The longest a machine takes to start and pass its checks.
const START: std::time::Duration = std::time::Duration::from_secs(600);
/// The header a leased machine's changes carry.
const NONCE: &str = "fly-machine-lease-nonce";

fn api<'a>(cx: &'a Cx<'_>) -> Result<Api<'a>> {
    Ok(Api { kind: "fly", token: cx.token()?.0 })
}

async fn call(cx: &Cx<'_>, method: Method, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<Value> {
    cx.conn.budget().await;
    api(cx)?.call(method, path, body, headers).await
}

fn app_path(app: &str) -> String {
    format!("/v1/apps/{}", encode(app, false))
}

/// The app's machines, destroyed ones aside; none when there's no app.
async fn machines(cx: &Cx<'_>, app: &str) -> Result<Option<Vec<Value>>> {
    match call(cx, Method::Get, &format!("{}/machines", app_path(app)), None, &[]).await {
        Ok(v) => Ok(Some(v.as_array().into_iter().flatten().filter(|m| !matches!(m.get("state").and_then(Value::as_str), Some("destroyed" | "destroying"))).cloned().collect())),
        Err(e) if HostingError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(ms) = machines(cx, name).await? else { return Ok(None) };
    let rec = cx.applied(GROUP, "App", "", name);
    Ok(p::live_app(name, &ms, rec.as_ref().map(|r| &r.object)))
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

fn org(cx: &Cx<'_>) -> String {
    cx.token().ok().and_then(|(_, c)| c.get("org").and_then(Value::as_str).map(str::to_owned)).filter(|o| !o.is_empty()).unwrap_or_else(|| "personal".into())
}

/// What the organization holds: its apps (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let apps = call(cx, Method::Get, &format!("/v1/apps?org_slug={}", encode(&org(cx), false)), None, &[]).await?;
    let out: Vec<Value> = apps.get("apps").and_then(Value::as_array).into_iter().flatten().map(|a| json!({ "kind": "App", "name": a.get("name"), "summary": format!("{} machines", a.get("machine_count").and_then(Value::as_u64).unwrap_or(0)) })).collect();
    Ok(json!({ "resources": out }))
}

/// An app as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(ms) = machines(cx, name).await? else { bail!("no app {name} here") };
    let mut config = ms.first().and_then(|m| m.get("config")).cloned().unwrap_or_else(|| json!({}));
    let image = config.as_object_mut().and_then(|c| c.remove("image"));
    Ok(json!({ "definition": { "app": name, "image": image, "config": config, "count": ms.len().max(1) } }))
}

/// Waits for a machine to start on its new instance, and its checks to pass: why, if it didn't.
async fn started(cx: &Cx<'_>, app: &str, id: &str, instance: &str, what: &str) -> Result<()> {
    let path = format!("{}/machines/{id}", app_path(app));
    let waited = call(cx, Method::Get, &format!("{path}/wait?instance_id={}&state=started&timeout=60", encode(instance, false)), None, &[]).await;
    let m = call(cx, Method::Get, &path, None, &[]).await?;
    if waited.is_err() || m.get("state").and_then(Value::as_str) != Some("started") {
        let exit = m.get("events").and_then(Value::as_array).into_iter().flatten().find(|e| e.get("type").and_then(Value::as_str) == Some("exit")).and_then(|e| e.pointer("/request/exit_event/exit_code")).and_then(Value::as_i64);
        bail!("{what}: machine {id} didn't start{}", exit.map(|c| format!(" (it exited with code {c})")).unwrap_or_default());
    }
    let m = until(cx.conn, START, what, || async {
        let m = api(cx)?.call(Method::Get, &path, None, &[]).await?;
        let checks: Vec<String> = m.get("checks").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("status").and_then(Value::as_str).map(str::to_owned)).collect();
        Ok(checks.iter().all(|c| c == "passing" || c == "critical").then_some(m))
    })
    .await?;
    if let Some(c) = m.get("checks").and_then(Value::as_array).into_iter().flatten().find(|c| c.get("status").and_then(Value::as_str) == Some("critical")) {
        bail!("{what}: machine {id}'s check {} fails", c.get("name").and_then(Value::as_str).unwrap_or_default());
    }
    Ok(())
}

/// Updates one machine to `config` under a lease, waiting until it started: the lease let go
/// whatever happened.
async fn roll(cx: &Cx<'_>, app: &str, id: &str, config: &Value, what: &str) -> Result<()> {
    let path = format!("{}/machines/{id}", app_path(app));
    let lease = call(cx, Method::Post, &format!("{path}/lease"), Some(&json!({ "ttl": 120, "description": "kiln: a release rolling out" })), &[]).await?;
    let nonce = lease.pointer("/data/nonce").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Fly gave machine {id} no lease"))?.to_owned();
    let result = async {
        let m = call(cx, Method::Post, &path, Some(&json!({ "config": config })), &[(NONCE, &nonce)]).await?;
        let instance = m.get("instance_id").and_then(Value::as_str).unwrap_or_default().to_owned();
        started(cx, app, id, &instance, what).await
    }
    .await;
    // Let go of the lease whatever happened; it expires anyway.
    if let Err(e) = call(cx, Method::Delete, &format!("{path}/lease"), None, &[(NONCE, &nonce)]).await {
        tracing::debug!(machine = %id, error = %format!("{e:#}"), "deployments: letting go of a Fly machine's lease failed");
    }
    result
}

/// Applies an App: made if it isn't there, its machines rolled onto the release one at a time,
/// then made or destroyed to its count.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let config = desired.pointer("/spec/config").cloned().unwrap_or_else(|| json!({}));
    let count = desired.pointer("/spec/count").and_then(Value::as_u64).unwrap_or(1) as usize;
    let region = desired.pointer("/spec/region").and_then(Value::as_str);
    let what = format!("rolling {name} out");
    let ms = match machines(cx, name).await? {
        Some(ms) => ms,
        None => {
            call(cx, Method::Post, "/v1/apps", Some(&json!({ "app_name": name, "org_slug": org(cx) })), &[]).await?;
            Vec::new()
        }
    };
    cx.record(GROUP, "App", "", name, Some(applied(desired, Value::Null))).await?;
    // One machine at a time; one that doesn't start stops the rollout there.
    for m in ms.iter().take(count) {
        if !covers(m.get("config").unwrap_or(&Value::Null), &config) {
            roll(cx, name, m.get("id").and_then(Value::as_str).unwrap_or_default(), &config, &what).await?;
        }
    }
    for _ in ms.len()..count {
        let mut body = json!({ "config": config });
        if let (Some(r), Value::Object(b)) = (region, &mut body) {
            b.insert("region".into(), json!(r));
        }
        let m = call(cx, Method::Post, &format!("{}/machines", app_path(name)), Some(&body), &[]).await?;
        let (id, instance) = (m.get("id").and_then(Value::as_str).unwrap_or_default().to_owned(), m.get("instance_id").and_then(Value::as_str).unwrap_or_default().to_owned());
        started(cx, name, &id, &instance, &what).await?;
    }
    for m in ms.iter().skip(count) {
        call(cx, Method::Delete, &format!("{}/machines/{}?force=true", app_path(name), m.get("id").and_then(Value::as_str).unwrap_or_default()), None, &[]).await?;
    }
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("app {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    match call(cx, Method::Delete, &format!("{}?force=true", app_path(&key.name)), None, &[]).await {
        Ok(_) => Ok(()),
        Err(e) if HostingError::is_not_found(&e) => Ok(()),
        Err(e) => Err(e),
    }
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

    const FLY_TOML: &str = "app = 'shop-web'\nprimary_region = 'ams'\n[env]\nMODE = 'prod'\n[http_service]\ninternal_port = 8080\nforce_https = true\n";

    /// An app's machines on a faked Fly.io, release by release: the first makes the app and its
    /// two machines from fly.toml; the same release again changes nothing; the next rolls each
    /// machine onto it in turn, under a lease; one whose image doesn't start stops the rollout at
    /// the first machine, which reads as degraded, the second on the release before; the release
    /// before, rolled back to, rolls the first machine back; then the app goes.
    #[test]
    fn machines_roll_onto_each_release() -> anyhow::Result<()> {
        let _one = fake_hosting::HOSTING.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(FakeHosting::default());
            fake_hosting::start(fake.clone()).await?;
            let (target, place) = fake_hosting::target("fly")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: fake_hosting::TOKEN.into(), config: json!({ "org": "acme" }) }, records: &records, files: &files };
            let spec = json!({ "app": "shop-web", "image": "registry.fly.io/shop-web@${{ release.artifacts.image.digest }}", "count": 2 });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("fly", &place, spec.clone(), version, &[("fly.toml", FLY_TOML)], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no app"))
            };
            let names = [(String::new(), "shop-web".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let images = || fake.state.lock().fly.get("shop-web").map(|ms| ms.iter().map(|m| m.config.get("image").and_then(Value::as_str).unwrap_or_default().to_owned()).collect::<Vec<_>>()).unwrap_or_default();
            let calls = || fake.state.lock().calls.clone();

            let a1 = release("1.0.0", "sha256:aaaa")?;
            assert_eq!(a1.pointer("/spec/region"), Some(&json!("ams")), "fly.toml's primary region");
            let up = apply(&cx, &a1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&a1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(calls(), ["CreateApp shop-web", "CreateMachine shop-web", "CreateMachine shop-web"]);
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(calls().len(), 3, "the same release again changes nothing");

            // The next: each machine rolled onto it in turn.
            let a2 = release("1.1.0", "sha256:bbbb")?;
            let up2 = apply(&cx, &a2).await?;
            assert_eq!(h(&up2), Some(Health::Healthy), "{up2}");
            assert_eq!(images(), ["registry.fly.io/shop-web@sha256:bbbb"; 2]);
            assert_eq!(calls().iter().filter(|c| c.starts_with("UpdateMachine")).count(), 2);
            assert!(fake.state.lock().fly.get("shop-web").is_some_and(|ms| ms.iter().all(|m| m.lease.is_none())), "every lease let go");

            // One whose image doesn't start: the rollout stops at the first machine.
            let failed = apply(&cx, &release("1.2.0", "sha256:broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("didn't start") && failed.contains("code 1"), "{failed}");
            assert_eq!(images(), ["registry.fly.io/shop-web@sha256:broken", "registry.fly.io/shop-web@sha256:bbbb"], "the second machine on the release before");
            let down = list(&cx, "App", &names).await?.into_iter().next().unwrap_or_default();
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            assert!(fake.state.lock().fly.get("shop-web").is_some_and(|ms| ms.iter().all(|m| m.lease.is_none())), "the lease let go after the failure");

            // Rolled back to the release before: the first machine rolled back.
            let back = apply(&cx, &a2).await?;
            assert_eq!(images(), ["registry.fly.io/shop-web@sha256:bbbb"; 2]);
            assert!(synced(&a2, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-web")));
            let adopted = adopt(&cx, "shop-web").await?;
            assert_eq!(adopted.pointer("/definition/image"), Some(&json!("registry.fly.io/shop-web@sha256:bbbb")), "{adopted}");
            delete(&cx, &key_of(&a2)).await?;
            assert!(list(&cx, "App", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
