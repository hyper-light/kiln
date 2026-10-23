//! `azure-appservice`'s and `azure-functions`' I/O half (docs/cd-targets.md §3): Resource
//! Manager's `Microsoft.Web/sites`, and an app's SCM site for packages (zip deploy, with
//! Resource Manager's token). The pure half, `kiln_cd::providers::azure_web`, renders the objects.
//!
//! A release sets the app settings and site config kiln sets (merged into the app's own), then
//! the image, or deploys the package and waits for the deployment; then waits until the site
//! runs. With a slot, all of that goes to the slot, which is swapped into production once it
//! runs, and kiln keeps what each side holds: a rollback to the release in the slot is one swap.
//! Apps and slots are made with their plans, outside kiln.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::azure_web::{self as p, API};
use kiln_cd::providers::gcp_run::covers;
use serde_json::{Map, Value, json};

use super::azure::{SETTLE, call, get, group_path, missing, operation};
use super::{Cx, applied, until};
use crate::ci::integrations::azure::{self, Body};
use crate::http::client::Method;

fn site_path(cx: &Cx<'_>, name: &str, slot: Option<&str>) -> Result<String> {
    let site = format!("{}/providers/Microsoft.Web/sites/{name}", group_path(cx)?);
    Ok(match slot {
        Some(s) => format!("{site}/slots/{s}"),
        None => site,
    })
}

async fn settings(cx: &Cx<'_>, path: &str) -> Result<Value> {
    Ok(call(cx, Method::Post, &format!("{path}/config/appsettings/list"), API, None).await?.body)
}

async fn config(cx: &Cx<'_>, path: &str) -> Result<Value> {
    Ok(get(cx, &format!("{path}/config/web"), API).await?.unwrap_or(Value::Null))
}

/// A site's newest deployment, from its SCM site.
async fn latest(cx: &Cx<'_>, site: &Value) -> Result<Option<Value>> {
    let Some(host) = p::scm_host(site) else { return Ok(None) };
    cx.conn.budget().await;
    Ok(missing(cx.arm()?.send(Method::Get, &format!("{}/api/deployments/latest", azure::host(&host)), Body::None, &[], true).await)?.map(|a| a.body))
}

fn made_of<'a>(made: &'a Value, k: &str) -> Option<&'a str> {
    made.get(k).and_then(Value::as_str)
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let (group, kind) = p::kind_of(cx.conn.def.name);
    let path = site_path(cx, name, None)?;
    let Some(site) = get(cx, &path, API).await? else { return Ok(None) };
    let rec = cx.applied(group, kind, "", name);
    let (cfg, set) = (config(cx, &path).await?, settings(cx, &path).await?);
    let slot = rec.as_ref().and_then(|r| r.object.pointer("/spec/slot")).is_some();
    let package = rec.as_ref().and_then(|r| r.object.pointer("/spec/package")).is_some();
    let deployment = if package && !slot { latest(cx, &site).await? } else { None };
    // Production runs kiln's release while kiln's last change stands: its deployment the newest
    // (without a slot), or its swap (with one).
    let runs = rec.as_ref().is_some_and(|r| {
        made_of(&r.made, "production") == Some(p::marker(&r.object).as_str()) && (slot || !package || deployment.as_ref().and_then(|d| d.get("id")).and_then(Value::as_str) == made_of(&r.made, "deployment"))
    });
    Ok(p::live_site(cx.conn.def.name, &site, &cfg, &set, rec.as_ref().map(|r| &r.object), runs, deployment.as_ref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == p::kind_of(cx.conn.def.name).1 {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the resource group holds: its web apps, or its function apps (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let functions = cx.conn.def.name == "azure-functions";
    let page = call(cx, Method::Get, &format!("{}/providers/Microsoft.Web/sites", group_path(cx)?), API, None).await?.body;
    let out: Vec<Value> = page
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("kind").and_then(Value::as_str).unwrap_or_default().contains("functionapp") == functions)
        .map(|s| {
            let v = |k: &str| s.pointer(k).and_then(Value::as_str).unwrap_or_default();
            json!({ "kind": p::kind_of(cx.conn.def.name).1, "name": v("/name"), "summary": format!("{} · {}", v("/kind"), v("/properties/defaultHostName")) })
        })
        .collect();
    Ok(json!({ "resources": out }))
}

/// An app as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let path = site_path(cx, name, None)?;
    if get(cx, &path, API).await?.is_none() {
        bail!("no app {name} here");
    }
    let image = p::image_of(&config(cx, &path).await?);
    let (def, note) = match image {
        Some(i) => (json!({ "app": name, "image": i }), None),
        None => (json!({ "app": name, "package": "${{ release.artifacts.site.digest }}" }), Some("Azure doesn't say where an app's package came from: point package at the zip CI builds")),
    };
    Ok(json!({ "definition": def, "note": note }))
}

/// Deploys a package (by its SHA-256, in hex) through a site's SCM site: the deployment's id.
async fn zip_deploy(cx: &Cx<'_>, site: &Value, hex: &str, what: &str) -> Result<String> {
    let host = p::scm_host(site).ok_or_else(|| anyhow::anyhow!("{what}: the app has no SCM site to deploy through"))?;
    let base = azure::host(&host);
    let (rx, len) = cx.files.open(hex).await?;
    cx.conn.budget().await;
    let a = cx.arm()?.send(Method::Post, &format!("{base}/api/zipdeploy?isAsync=true"), Body::Stream { rx, len, content_type: "application/zip".into() }, &[], true).await?;
    let poll = a.header("Location").map_or_else(|| format!("{base}/api/deployments/latest"), str::to_owned);
    let d = until(cx.conn, SETTLE, what, || async {
        let d = cx.arm()?.send(Method::Get, &poll, Body::None, &[], true).await?.body;
        Ok((d.get("complete").and_then(Value::as_bool) == Some(true)).then_some(d))
    })
    .await?;
    if d.get("status").and_then(Value::as_i64) != Some(4) {
        bail!("{what}: the deployment failed: {}", d.get("status_text").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("Azure says no more"));
    }
    Ok(d.get("id").and_then(Value::as_str).unwrap_or_default().to_owned())
}

/// Deploys a release to a site (an app, or a slot of one): its settings and config, then its
/// image or package; then waits until it runs. The package's deployment, if it has one.
async fn deploy(cx: &Cx<'_>, path: &str, desired: &Value, what: &str) -> Result<Option<String>> {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    if let Some(Value::Object(want)) = spec.get("appSettings") {
        let have = settings(cx, path).await?.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut merged: Map<String, Value> = have.clone();
        merged.extend(want.clone());
        if merged != have {
            call(cx, Method::Put, &format!("{path}/config/appsettings"), API, Some(&json!({ "properties": merged }))).await?;
        }
    }
    let mut patch = spec.get("siteConfig").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(i) = spec.get("image").and_then(Value::as_str) {
        patch.insert("linuxFxVersion".into(), json!(format!("DOCKER|{i}")));
    }
    if !patch.is_empty() {
        let have = config(cx, path).await?.get("properties").cloned().unwrap_or(Value::Null);
        let patch = Value::Object(patch);
        if !covers(&have, &patch) {
            call(cx, Method::Patch, &format!("{path}/config/web"), API, Some(&json!({ "properties": patch }))).await?;
        }
    }
    let mut id = None;
    if let Some(a) = spec.pointer("/package/artifact").and_then(Value::as_str) {
        let site = get(cx, path, API).await?.ok_or_else(|| anyhow::anyhow!("{what}: the site is gone"))?;
        id = Some(zip_deploy(cx, &site, a.trim_start_matches("sha256:"), what).await?);
    }
    until(cx.conn, SETTLE, what, || async {
        let s = cx.arm()?.call(Method::Get, path, API, None).await?;
        Ok((s.pointer("/properties/state").and_then(Value::as_str) == Some("Running")).then_some(()))
    })
    .await?;
    Ok(id)
}

/// Applies a WebApp or FunctionApp: deploys its release to the app, or to its slot and swaps it
/// in, unless production runs it already; a release the slot holds is swapped in alone.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let (group, kind) = p::kind_of(cx.conn.def.name);
    let name = obj::name(desired);
    let prod = site_path(cx, name, None)?;
    if get(cx, &prod, API).await?.is_none() {
        bail!("{name} isn't there: make the app with its plan first, and kiln deploys to it");
    }
    let want = p::marker(desired);
    let prev = cx.applied(group, kind, "", name).map(|a| a.made).unwrap_or(Value::Null);
    let made = match desired.pointer("/spec/slot").and_then(Value::as_str) {
        None => {
            let package = desired.pointer("/spec/package").is_some();
            let newest = match (package, get(cx, &prod, API).await?) {
                (true, Some(site)) => latest(cx, &site).await?.and_then(|d| d.get("id").and_then(Value::as_str).map(str::to_owned)),
                _ => None,
            };
            let runs = made_of(&prev, "production") == Some(want.as_str()) && (!package || newest.as_deref() == made_of(&prev, "deployment"));
            let deployment = if runs { made_of(&prev, "deployment").map(str::to_owned) } else { deploy(cx, &prod, desired, &format!("deploying {name}")).await? };
            json!({ "production": want, "deployment": deployment })
        }
        Some(slot) => {
            let path = site_path(cx, name, Some(slot))?;
            if get(cx, &path, API).await?.is_none() {
                bail!("slot {slot} of {name} isn't there: make it first, and kiln deploys through it");
            }
            if made_of(&prev, "production") == Some(want.as_str()) {
                prev.clone()
            } else {
                // The slot holds this release already (the one before, rolled back to): a swap.
                if made_of(&prev, "slot") != Some(want.as_str()) {
                    deploy(cx, &path, desired, &format!("deploying {name} to slot {slot}")).await?;
                }
                let a = call(cx, Method::Post, &format!("{prod}/slots/{slot}/slotsswap"), API, Some(&json!({ "targetSlot": "production", "preserveVnet": true }))).await?;
                operation(cx, &a, &format!("swapping {name}'s slot {slot} into production")).await?;
                json!({ "production": want, "slot": prev.get("production") })
            }
        }
    };
    cx.record(group, kind, "", name, Some(applied(desired, made))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("{name} is gone"))
}

/// kiln deploys to an app made with its plan: deleting the object leaves the app.
pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    tracing::info!(target = %cx.conn.target.name(), app = %key.name, "deployments: kiln leaves an App Service app it deploys to; delete the app with its plan");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_azure::{self, FakeAzure};
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    /// A web app on a faked App Service, through its staging slot, release by release: the first
    /// sets the slot's settings, config and image and swaps it in; the same release again
    /// changes nothing; the next does so again; the release before, rolled back to, is in the
    /// slot, so it's swapped back with nothing deployed; one whose image doesn't start reads as
    /// degraded once swapped in, and going back is a swap again. kiln leaves the app when its
    /// object goes: the app was made with its plan.
    #[test]
    fn web_apps_swap_each_release_in() -> anyhow::Result<()> {
        let _one = fake_azure::AZURE.lock();
        crate::rt::block_on(async {
            let az = Arc::new(FakeAzure::default());
            fake_azure::start(az.clone()).await?;
            az.state.lock().site("shop-web", "app,linux,container", &["staging"]);
            let (target, place) = fake_azure::target("azure-appservice")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_azure::TOKEN.into()), records: &records, files: &files };
            let spec = json!({ "app": "shop-web", "image": "acme.azurecr.io/web@${{ release.artifacts.image.digest }}", "slot": "staging", "settings": { "MODE": "${{ env.name }}" }, "config": { "healthCheckPath": "/healthz" } });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("azure-appservice", &place, spec.clone(), version, &[], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no app"))
            };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let calls = || az.state.lock().calls.clone();

            // The first release: the slot set up, then swapped in.
            let r1 = release("1.0.0", "sha256:aaaa")?;
            let up = apply(&cx, &r1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&r1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(calls(), ["PutSettings shop-web/staging", "PatchConfig shop-web/staging DOCKER|acme.azurecr.io/web@sha256:aaaa", "SwapSlot shop-web staging"]);

            // The same release again: nothing changes.
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(calls().len(), 3);

            // The next: deployed to the slot, then swapped in.
            let up2 = apply(&cx, &release("1.1.0", "sha256:bbbb")?).await?;
            assert_eq!(up2.pointer("/spec/image"), Some(&json!("acme.azurecr.io/web@sha256:bbbb")));
            assert_eq!(calls().last().map(String::as_str), Some("SwapSlot shop-web staging"));

            // Rolled back to the release before: it's in the slot, so one swap, nothing deployed.
            let n = calls().len();
            let back = apply(&cx, &r1).await?;
            assert_eq!(calls().get(n..).map(<[String]>::to_vec), Some(vec!["SwapSlot shop-web staging".to_owned()]));
            assert!(synced(&r1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One whose image doesn't start: degraded once swapped in; going back is a swap.
            let down = apply(&cx, &release("1.2.0", "sha256:broken")?).await?;
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            let n = calls().len();
            let again = apply(&cx, &r1).await?;
            assert_eq!(calls().get(n..).map(<[String]>::to_vec), Some(vec!["SwapSlot shop-web staging".to_owned()]));
            assert_eq!(h(&again), Some(Health::Healthy), "{again}");

            // The app stays when its object goes: it was made with its plan.
            delete(&cx, &key_of(&r1)).await?;
            assert!(!list(&cx, "WebApp", &[(String::new(), "shop-web".into())]).await?.is_empty(), "the app stays");
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-web")));
            Ok(())
        })
    }

    /// A function app on a faked Azure Functions, without a slot, release by release: the first
    /// sets its settings and zip-deploys its package through its SCM site; the same release again
    /// deploys nothing; a deployment someone else made reads as drift, and kiln deploys its own
    /// again; one whose package fails to deploy fails and reads as degraded; the release before,
    /// rolled back to, deploys again.
    #[test]
    fn function_apps_deploy_each_releases_package() -> anyhow::Result<()> {
        let _one = fake_azure::AZURE.lock();
        crate::rt::block_on(async {
            let az = Arc::new(FakeAzure::default());
            fake_azure::start(az.clone()).await?;
            az.state.lock().site("shop-fn", "functionapp,linux", &[]);
            let (target, place) = fake_azure::target("azure-functions")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_azure::TOKEN.into()), records: &records, files: &files };
            let spec = json!({ "app": "shop-fn", "package": "${{ release.artifacts.package.digest }}", "settings": { "MODE": "${{ env.name }}" } });
            let release = |version: &str, zip: &str| -> anyhow::Result<Value> {
                let digest = format!("sha256:{}", files.add(zip.as_bytes()));
                released_to("azure-functions", &place, spec.clone(), version, &[], &[("package", &digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no app"))
            };
            let names = [(String::new(), "shop-fn".to_owned())];
            let read = || async { Ok::<_, anyhow::Error>(list(&cx, "FunctionApp", &names).await?.into_iter().next().unwrap_or_default()) };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let deploys = || az.state.lock().calls.iter().filter(|c| c.starts_with("ZipDeploy")).count();

            let f1 = release("1.0.0", "zip of shop-fn 1.0.0")?;
            let up = apply(&cx, &f1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&f1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(az.state.lock().calls, ["PutSettings shop-fn", "ZipDeploy shop-fn.scm.azurewebsites.net"]);
            apply(&cx, &release("1.0.0", "zip of shop-fn 1.0.0")?).await?;
            assert_eq!(deploys(), 1, "the same release again deploys nothing");

            // A deployment someone else made: drift; kiln's next apply deploys its own again.
            az.state.lock().deploy_by_hand("shop-fn");
            let theirs = read().await?;
            assert!(!synced(&f1, &cx.with_applied(theirs.clone())), "drift: {theirs}");
            apply(&cx, &f1).await?;
            assert_eq!(deploys(), 2);

            // One whose package fails to deploy: it fails, and reads as degraded.
            let failed = apply(&cx, &release("1.1.0", "a broken zip")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("deployment failed"), "{failed}");
            assert_eq!(h(&read().await?), Some(Health::Degraded));

            // Rolled back to the release before: deployed again, and healthy.
            let back = apply(&cx, &f1).await?;
            assert_eq!((deploys(), h(&back)), (4, Some(Health::Healthy)), "{back}");
            assert!(synced(&f1, &cx.with_applied(back.clone())), "{back}");
            Ok(())
        })
    }
}
