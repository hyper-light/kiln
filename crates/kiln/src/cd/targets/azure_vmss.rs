//! `azure-vmss`' I/O half (docs/cd-targets.md §3): Resource Manager's
//! `Microsoft.Compute/virtualMachineScaleSets`. The pure half, `kiln_cd::providers::azure_vmss`,
//! renders the objects.
//!
//! A release moves the scale set's model to its image (made, the first time, from the spec), and
//! Azure rolls the instances onto it by the rolling upgrade policy. Its health follows the
//! instances (how many run the model) and the latest rolling upgrade.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::azure_vmss::{self as p, API, GROUP};
use kiln_cd::providers::gcp_run::covers;
use serde_json::{Value, json};

use super::azure::{call, get, group_path, location, missing, operation, provisioned, tags};
use super::{Cx, applied};
use crate::http::client::Method;

fn path(cx: &Cx<'_>, name: &str) -> Result<String> {
    Ok(format!("{}/providers/Microsoft.Compute/virtualMachineScaleSets/{name}", group_path(cx)?))
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let at = path(cx, name)?;
    let Some(answer) = get(cx, &at, API).await? else { return Ok(None) };
    let vms = call(cx, Method::Get, &format!("{at}/virtualMachines"), API, None).await?.body;
    let list = vms.get("value").and_then(Value::as_array).cloned().unwrap_or_default();
    let on = list.iter().filter(|v| v.pointer("/properties/latestModelApplied").and_then(Value::as_bool) == Some(true)).count() as u64;
    let upgrade = get(cx, &format!("{at}/rollingUpgrades/latest"), API).await?;
    Ok(p::live_scale_set(&answer, Some((on, list.len() as u64)), upgrade.as_ref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "ScaleSet" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the resource group holds: its scale sets (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, Method::Get, &format!("{}/providers/Microsoft.Compute/virtualMachineScaleSets", group_path(cx)?), API, None).await?.body;
    let out: Vec<Value> = page
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| json!({ "kind": "ScaleSet", "name": s.get("name"), "summary": format!("{} × {}", s.pointer("/sku/capacity").and_then(Value::as_u64).unwrap_or(0), s.pointer("/sku/name").and_then(Value::as_str).unwrap_or_default()) }))
        .collect();
    Ok(json!({ "resources": out }))
}

/// A scale set as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(live) = live(cx, name).await? else { bail!("no scale set {name} here") };
    let r = live.pointer("/spec/properties/virtualMachineProfile/storageProfile/imageReference").cloned().unwrap_or(Value::Null);
    let image = r.get("id").and_then(Value::as_str).map(str::to_owned).or_else(|| {
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        (!s("publisher").is_empty()).then(|| format!("{}:{}:{}:{}", s("publisher"), s("offer"), s("sku"), s("version")))
    });
    let upgrade = live.pointer("/spec/properties/upgradePolicy/rollingUpgradePolicy").cloned();
    Ok(json!({ "definition": { "scale-set": name, "image": image, "upgrade": upgrade }, "live": live }))
}

/// Applies a ScaleSet: made from its spec if it isn't there, or its model changed where it
/// differs, which Azure rolls out.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let at = path(cx, name)?;
    let spec = desired.get("spec").cloned().unwrap_or_else(|| json!({}));
    let what = format!("rolling scale set {name} onto its model");
    match get(cx, &at, API).await? {
        None => {
            let mut body = spec.clone();
            if let Value::Object(m) = &mut body {
                if m.get("location").is_none_or(Value::is_null) {
                    m.insert("location".into(), json!(location(cx).await?));
                }
                m.insert("tags".into(), Value::Object(tags(desired)));
            }
            let a = call(cx, Method::Put, &at, API, Some(&body)).await?;
            operation(cx, &a, &format!("making scale set {name}")).await?;
            provisioned(cx, &at, API, &format!("making scale set {name}")).await?;
        }
        Some(have) if !covers(&have, &spec) => {
            let mut body = spec.clone();
            if let Value::Object(m) = &mut body {
                let mut t = have.get("tags").and_then(Value::as_object).cloned().unwrap_or_default();
                t.extend(tags(desired));
                m.insert("tags".into(), Value::Object(t));
                m.remove("location");
            }
            let a = call(cx, Method::Patch, &at, API, Some(&body)).await?;
            operation(cx, &a, &what).await?;
            provisioned(cx, &at, API, &what).await?;
        }
        Some(_) => {}
    }
    cx.record(GROUP, "ScaleSet", "", name, Some(applied(desired, Value::Null))).await?;
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("scale set {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if &*key.kind != "ScaleSet" {
        bail!("azure-vmss doesn't delete {}", key.kind);
    }
    if let Some(a) = missing(call(cx, Method::Delete, &path(cx, &key.name)?, API, None).await)? {
        operation(cx, &a, &format!("deleting scale set {}", key.name)).await?;
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

    /// A scale set on a faked Azure, release by release: the first makes it on its image; the same
    /// release again changes nothing; the next moves its model to its image, and the rolling
    /// upgrade moves the instances onto it; one whose image doesn't start stops the upgrade
    /// (Faulted), read as degraded, its instances on the image before; the release before,
    /// rolled back to, moves the model back; then it goes.
    #[test]
    fn scale_sets_roll_onto_each_releases_image() -> anyhow::Result<()> {
        let _one = fake_azure::AZURE.lock();
        crate::rt::block_on(async {
            let az = Arc::new(FakeAzure::default());
            fake_azure::start(az.clone()).await?;
            let (target, place) = fake_azure::target("azure-vmss")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_azure::TOKEN.into()), records: &records, files: &files };
            let spec = json!({ "scale-set": "web", "image": "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Compute/galleries/acme/images/web/versions/${{ release.version }}",
                "spec": { "sku": { "name": "Standard_B2s", "capacity": 2 }, "properties": { "virtualMachineProfile": { "osProfile": { "computerNamePrefix": "web", "adminUsername": "kiln" } } } },
                "upgrade": { "maxBatchInstancePercent": 50 } });
            let release = |version: &str| -> anyhow::Result<Value> { released_to("azure-vmss", &place, spec.clone(), version, &[], &[])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no scale set")) };
            let names = [(String::new(), "web".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let settled = || async {
                for _ in 0..20 {
                    let s = list(&cx, "ScaleSet", &names).await?.into_iter().next().unwrap_or_default();
                    if matches!(h(&s), Some(Health::Healthy | Health::Degraded)) {
                        return Ok::<_, anyhow::Error>(s);
                    }
                }
                anyhow::bail!("the scale set didn't settle")
            };
            let last_call = || az.state.lock().calls.last().cloned().unwrap_or_default();

            let s1 = release("1.0.0")?;
            apply(&cx, &s1).await?;
            let up = settled().await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&s1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(az.state.lock().calls, ["PutScaleSet web"]);
            apply(&cx, &release("1.0.0")?).await?;
            assert_eq!(az.state.lock().calls.len(), 1, "the same release again changes nothing");

            // The next: the model moves, and the rolling upgrade moves the instances.
            let s2 = release("1.1.0")?;
            let rolling = apply(&cx, &s2).await?;
            assert!(last_call().starts_with("PatchScaleSet web") && last_call().ends_with("/versions/1.1.0"), "{}", last_call());
            assert_eq!(h(&rolling), Some(Health::Progressing), "{rolling}");
            assert_eq!(h(&settled().await?), Some(Health::Healthy));

            // One whose image doesn't start: the upgrade stops, and the instances stay.
            apply(&cx, &release("broken-1.2.0")?).await?;
            let down = settled().await?;
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            assert!(kiln_cd::providers::health(&down).is_some_and(|s| s.message.contains("faulted")), "{down}");

            // Rolled back to the release before: the model moves back.
            apply(&cx, &s2).await?;
            assert!(last_call().ends_with("/versions/1.1.0"));
            let back = settled().await?;
            assert_eq!(h(&back), Some(Health::Healthy), "{back}");
            assert!(synced(&s2, &cx.with_applied(back.clone())), "{back}");

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("web")));
            let adopted = adopt(&cx, "web").await?;
            assert!(adopted.pointer("/definition/image").and_then(Value::as_str).is_some_and(|i| i.ends_with("/versions/1.1.0")), "{adopted}");
            delete(&cx, &key_of(&s2)).await?;
            assert!(az.state.lock().scale_sets.is_empty());
            Ok(())
        })
    }
}
