//! `azure-static`'s I/O half (docs/cd-targets.md §3): Resource Manager's
//! `Microsoft.Web/staticSites`, and Azure Storage for what it deploys. The pure half,
//! `kiln_cd::providers::azure_static`, renders the objects.
//!
//! A release's zip goes to the storage account (by its hash, so a zip goes up once), and the app
//! deploys it from a URL signed for that one blob (its `zipdeploy`); kiln waits until the
//! production build is ready. The same release again deploys nothing while that build stands.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::azure_static::{self as p, API, GROUP};
use serde_json::{Value, json};

use super::azure::{SETTLE, blob_url, call, get, group_path, location, missing, operation, provisioned, put_blob, storage, tags};
use super::{Cx, applied, until};
use crate::http::client::Method;

fn path(cx: &Cx<'_>, name: &str) -> Result<String> {
    Ok(format!("{}/providers/Microsoft.Web/staticSites/{name}", group_path(cx)?))
}

async fn build(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    get(cx, &format!("{}/builds/default", path(cx, name)?), API).await
}

fn built(b: Option<&Value>) -> Option<String> {
    b.and_then(|b| b.pointer("/properties/lastUpdatedOn")).and_then(Value::as_str).map(str::to_owned)
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(answer) = get(cx, &path(cx, name)?, API).await? else { return Ok(None) };
    let b = build(cx, name).await?;
    let rec = cx.applied(GROUP, "StaticSite", "", name);
    // Production serves kiln's deployment while its build is the one that deployment made.
    let runs = rec.as_ref().is_some_and(|r| r.made.get("built").and_then(Value::as_str).is_some() && r.made.get("built").and_then(Value::as_str) == built(b.as_ref()).as_deref());
    Ok(p::live_site(&answer, b.as_ref(), rec.as_ref().map(|r| &r.object), runs))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "StaticSite" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the resource group holds: its static web apps (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, Method::Get, &format!("{}/providers/Microsoft.Web/staticSites", group_path(cx)?), API, None).await?.body;
    let out: Vec<Value> = page
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| json!({ "kind": "StaticSite", "name": s.get("name"), "summary": format!("{} · {}", s.pointer("/sku/name").and_then(Value::as_str).unwrap_or_default(), s.pointer("/properties/defaultHostname").and_then(Value::as_str).unwrap_or_default()) }))
        .collect();
    Ok(json!({ "resources": out }))
}

/// An app as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(answer) = get(cx, &path(cx, name)?, API).await? else { bail!("no static web app {name} here") };
    let sku = answer.pointer("/sku/name").cloned();
    Ok(json!({ "definition": { "app": name, "site": "${{ release.artifacts.site.digest }}", "sku": sku }, "note": "Azure doesn't say where a site came from: point site at the zip CI builds" }))
}

/// Applies a StaticSite: made if it isn't there, then its zip deployed unless production serves
/// it already.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let at = path(cx, name)?;
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    if get(cx, &at, API).await?.is_none() {
        let sku = spec.get("sku").cloned().unwrap_or_else(|| json!({ "name": "Free", "tier": "Free" }));
        let body = json!({ "location": location(cx).await?, "sku": sku, "tags": tags(desired), "properties": {} });
        let a = call(cx, Method::Put, &at, API, Some(&body)).await?;
        operation(cx, &a, &format!("making static web app {name}")).await?;
        provisioned(cx, &at, API, &format!("making static web app {name}")).await?;
    }
    let artifact = spec.pointer("/site/artifact").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("static web app {name} has no site"))?.to_owned();
    let prev = cx.applied(GROUP, "StaticSite", "", name).map(|a| a.made).unwrap_or(Value::Null);
    let now = built(build(cx, name).await?.as_ref());
    let serves = prev.get("artifact").and_then(Value::as_str) == Some(artifact.as_str()) && now.is_some() && prev.get("built").and_then(Value::as_str) == now.as_deref();
    let made = if serves {
        prev
    } else {
        let account = storage(cx, spec.get("storage").and_then(Value::as_str)).await?;
        let blob = put_blob(cx, &account, "staticwebapps", artifact.trim_start_matches("sha256:"), ".zip").await?;
        let url = blob_url(cx, &account, &blob, 3600).await?;
        let body = json!({ "properties": { "appZipUrl": url, "deploymentTitle": format!("kiln: {}", super::app_of(desired).unwrap_or_default()), "provider": "kiln" } });
        let a = call(cx, Method::Post, &format!("{at}/zipdeploy"), API, Some(&body)).await?;
        let what = format!("deploying static web app {name}");
        operation(cx, &a, &what).await?;
        let b = until(cx.conn, SETTLE, &what, || async {
            let b = cx.arm()?.call(Method::Get, &format!("{at}/builds/default"), API, None).await?;
            Ok(matches!(b.pointer("/properties/status").and_then(Value::as_str), Some("Ready" | "Failed")).then_some(b))
        })
        .await?;
        if b.pointer("/properties/status").and_then(Value::as_str) != Some("Ready") {
            bail!("{what}: its build failed");
        }
        json!({ "artifact": artifact, "built": built(Some(&b)), "blob": blob })
    };
    cx.record(GROUP, "StaticSite", "", name, Some(applied(desired, made))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("static web app {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if &*key.kind != "StaticSite" {
        bail!("azure-static doesn't delete {}", key.kind);
    }
    if let Some(a) = missing(call(cx, Method::Delete, &path(cx, &key.name)?, API, None).await)? {
        operation(cx, &a, &format!("deleting static web app {}", key.name)).await?;
    }
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

    /// A static web app on a faked Azure, release by release: the first makes the app and kiln's
    /// storage account, puts the zip there and deploys it from a URL signed for it; the same
    /// release again deploys nothing; the next uploads and deploys its own; the release before,
    /// rolled back to, deploys its zip again with nothing uploaded; one whose build fails fails
    /// and reads as degraded; then it goes.
    #[test]
    fn static_sites_deploy_each_releases_zip() -> anyhow::Result<()> {
        let _one = fake_azure::AZURE.lock();
        crate::rt::block_on(async {
            let az = Arc::new(FakeAzure::default());
            fake_azure::start(az.clone()).await?;
            let (target, place) = fake_azure::target("azure-static")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_azure::TOKEN.into()), records: &records, files: &files };
            let spec = json!({ "app": "shop-site", "site": "${{ release.artifacts.site.digest }}" });
            let release = |version: &str, zip: &str| -> anyhow::Result<Value> {
                let digest = format!("sha256:{}", files.add(zip.as_bytes()));
                released_to("azure-static", &place, spec.clone(), version, &[], &[("site", &digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no site"))
            };
            let names = [(String::new(), "shop-site".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let count = |prefix: &str| az.state.lock().calls.iter().filter(|c| c.starts_with(prefix)).count();

            let s1 = release("1.0.0", "zip of the site 1.0.0")?;
            let up = apply(&cx, &s1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&s1, &cx.with_applied(up.clone())), "in sync: {up}");
            let calls = az.state.lock().calls.clone();
            assert_eq!(calls.first().map(String::as_str), Some("PutStaticSite shop-site"));
            assert!(calls.iter().any(|c| c.starts_with("PutStorageAccount kiln")), "kiln's storage account made: {calls:?}");
            assert_eq!((count("PutBlob"), count("ZipDeployStaticSite")), (1, 1));
            apply(&cx, &release("1.0.0", "zip of the site 1.0.0")?).await?;
            assert_eq!((count("PutBlob"), count("ZipDeployStaticSite")), (1, 1), "the same release again deploys nothing");

            apply(&cx, &release("1.1.0", "zip of the site 1.1.0")?).await?;
            assert_eq!((count("PutBlob"), count("ZipDeployStaticSite")), (2, 2));

            // Rolled back to the release before: its zip is there; deployed again, nothing uploaded.
            let back = apply(&cx, &s1).await?;
            assert_eq!((count("PutBlob"), count("ZipDeployStaticSite")), (2, 3));
            assert!(synced(&s1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            // One whose build fails: it fails, and reads as degraded.
            let failed = apply(&cx, &release("1.2.0", "a broken site")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("build failed"), "{failed}");
            assert_eq!(h(&list(&cx, "StaticSite", &names).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded));

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop-site")));
            delete(&cx, &key_of(&s1)).await?;
            assert!(list(&cx, "StaticSite", &names).await?.is_empty(), "deleted");
            Ok(())
        })
    }
}
