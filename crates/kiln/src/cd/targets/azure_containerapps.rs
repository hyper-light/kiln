//! `azure-containerapps`' I/O half (docs/cd-targets.md §3): Resource Manager's
//! `Microsoft.App/containerApps`. The pure half, `kiln_cd::providers::azure_containerapps`,
//! renders the objects.
//!
//! A release's template goes to the app under its revision's name, and the traffic to that
//! revision; kiln waits for Azure to provision the app and for the revision to run. A revision
//! the app ran before (a rollback) is there still: kiln activates it again if it was let go,
//! moves the traffic back, and leaves the template as it is. Without ingress, the release's
//! revision is the one kiln keeps active, its others let go once it runs.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::azure_containerapps::{self as p, API, GROUP};
use serde_json::{Value, json};

use super::azure::{SETTLE, call, get, group_path, location, missing, operation, provisioned, tags};
use super::{Cx, applied, until};
use crate::http::client::Method;

fn app_path(cx: &Cx<'_>, name: &str) -> Result<String> {
    Ok(format!("{}/providers/Microsoft.App/containerApps/{name}", group_path(cx)?))
}

/// A managed environment's id: as given, or a name in the target's resource group.
fn environment_id(cx: &Cx<'_>, given: &str) -> Result<String> {
    Ok(if given.starts_with("/subscriptions/") { given.to_owned() } else { format!("{}/providers/Microsoft.App/managedEnvironments/{given}", group_path(cx)?) })
}

async fn revision(cx: &Cx<'_>, app: &str, name: &str) -> Result<Option<Value>> {
    get(cx, &format!("{}/revisions/{name}", app_path(cx, app)?), API).await
}

/// A revision's state, as the object's status says it.
fn revision_state(r: &Value) -> Value {
    let p = r.get("properties").cloned().unwrap_or(Value::Null);
    let s = |k: &str| p.get(k).cloned().unwrap_or(Value::Null);
    let message = p.get("provisioningError").or_else(|| p.get("runningStateDetails")).cloned().unwrap_or(Value::Null);
    json!({ "name": r.get("name"), "active": s("active"), "provisioningState": s("provisioningState"), "runningState": s("runningState"), "healthState": s("healthState"), "replicas": s("replicas"), "message": message })
}

async fn live(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(answer) = get(cx, &app_path(cx, name)?, API).await? else { return Ok(None) };
    let rec = cx.applied(GROUP, "ContainerApp", "", name);
    let env = rec.as_ref().and_then(|r| r.object.pointer("/spec/properties/environmentId")).and_then(Value::as_str).map(str::to_owned);
    let env_id = env.as_deref().map(|e| environment_id(cx, e)).transpose()?;
    let environment = env.as_deref().zip(env_id.as_deref());
    // Without ingress, the revision kiln keeps active is its release's.
    let ours = rec.as_ref().and_then(|r| r.made.get("revision")).and_then(Value::as_str).map(str::to_owned);
    let active = match (answer.pointer("/properties/configuration/ingress"), &ours) {
        (None | Some(Value::Null), Some(r)) => Some(r.clone()),
        (None | Some(Value::Null), None) => answer.pointer("/properties/latestReadyRevisionName").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    };
    let first = p::live_app(&answer, rec.as_ref().map(|r| &r.object), environment, None, active.as_deref());
    let serving = first.as_ref().and_then(|o| p::serving(o, active.as_deref()));
    let state = match serving {
        Some(r) => revision(cx, name, &r).await?.map(|r| revision_state(&r)),
        None => None,
    };
    Ok(p::live_app(&answer, rec.as_ref().map(|r| &r.object), environment, state.as_ref(), active.as_deref()))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        if kind == "ContainerApp" {
            out.extend(live(cx, name).await?);
        }
    }
    Ok(out)
}

/// What the resource group holds: its container apps (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, Method::Get, &format!("{}/providers/Microsoft.App/containerApps", group_path(cx)?), API, None).await?.body;
    let out: Vec<Value> = page
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| {
            let s = |k: &str| a.pointer(k).and_then(Value::as_str).unwrap_or_default();
            json!({ "kind": "ContainerApp", "name": s("/name"), "summary": format!("{} · {}", s("/properties/template/containers/0/image"), s("/properties/configuration/ingress/fqdn")) })
        })
        .collect();
    Ok(json!({ "resources": out }))
}

/// An app as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(live) = live(cx, name).await? else { bail!("no container app {name} here") };
    let mut spec = live.get("spec").cloned().unwrap_or_else(|| json!({}));
    let image = spec.pointer("/properties/template/containers/0/image").cloned();
    let environment = spec.pointer("/properties/environmentId").and_then(Value::as_str).map(|e| e.rsplit('/').next().unwrap_or(e).to_owned());
    if let Some(props) = spec.get_mut("properties").and_then(Value::as_object_mut) {
        props.remove("environmentId");
        if let Some(t) = props.get_mut("template").and_then(Value::as_object_mut) {
            t.remove("revisionSuffix");
        }
        if let Some(c) = props.get_mut("configuration").and_then(Value::as_object_mut) {
            c.remove("activeRevisionsMode");
            if let Some(i) = c.get_mut("ingress").and_then(Value::as_object_mut) {
                i.remove("traffic");
            }
        }
    }
    if let Some(c) = spec.pointer_mut("/properties/template/containers/0").and_then(Value::as_object_mut) {
        c.remove("image");
    }
    Ok(json!({ "definition": { "app": name, "environment": environment, "image": image, "spec": spec }, "note": "kiln runs the app in Multiple revision mode, each release's revision named for its template", "live": live }))
}

/// Waits for a revision to run: why, if it failed.
async fn running(cx: &Cx<'_>, app: &str, name: &str) -> Result<()> {
    let path = format!("{}/revisions/{name}", app_path(cx, app)?);
    let r = until(cx.conn, SETTLE, &format!("starting revision {name}"), || async {
        let r = cx.arm()?.call(Method::Get, &path, API, None).await?;
        let s = |k: &str| r.pointer(&format!("/properties/{k}")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let settled = s("provisioningState") == "Failed" || matches!(s("runningState").as_str(), "Running" | "RunningAtMaxScale" | "Failed" | "Degraded");
        Ok(settled.then_some(r))
    })
    .await?;
    let state = revision_state(&r);
    let s = |k: &str| state.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    if s("provisioningState") == "Failed" || matches!(s("runningState").as_str(), "Failed" | "Degraded") {
        bail!("revision {name} failed: {}", if s("message").is_empty() { s("runningState") } else { s("message") });
    }
    Ok(())
}

/// Applies a ContainerApp: its template under its revision's name unless that revision is there
/// (activated again if it was let go), the traffic to it, then waits for it to run.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    let path = app_path(cx, name)?;
    let sfx = desired.pointer("/spec/properties/template/revisionSuffix").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("app {name} names no revision"))?;
    let want = p::revision_name(name, sfx);
    let env = environment_id(cx, desired.pointer("/spec/properties/environmentId").and_then(Value::as_str).unwrap_or_default())?;
    let current = get(cx, &path, API).await?;
    let mut keep = false;
    if let Some(c) = &current {
        let there = revision(cx, name, &want).await?;
        keep = c.pointer("/properties/template/revisionSuffix").and_then(Value::as_str) == Some(sfx) || there.is_some();
        // A revision let go is activated again: a rollback to it.
        if there.as_ref().is_some_and(|r| r.pointer("/properties/active").and_then(Value::as_bool) == Some(false)) {
            call(cx, Method::Post, &format!("{path}/revisions/{want}/activate"), API, None).await?;
        }
    }
    let mut body = p::app_body(current.as_ref(), desired, &env, keep, tags(desired));
    if body.get("location").is_none_or(Value::is_null)
        && let Value::Object(m) = &mut body
    {
        m.insert("location".into(), json!(location(cx).await?));
    }
    if current.as_ref().is_none_or(|c| !p::unchanged(c, &body)) {
        let a = call(cx, Method::Put, &path, API, Some(&body)).await?;
        cx.record(GROUP, "ContainerApp", "", name, Some(applied(desired, json!({ "revision": want })))).await?;
        operation(cx, &a, &format!("updating container app {name}")).await?;
        provisioned(cx, &path, API, &format!("provisioning container app {name}")).await?;
    } else {
        cx.record(GROUP, "ContainerApp", "", name, Some(applied(desired, json!({ "revision": want })))).await?;
    }
    running(cx, name, &want).await?;
    // Without ingress, the release's revision is the one active: kiln lets its others go.
    if desired.pointer("/spec/properties/configuration/ingress").is_none_or(Value::is_null) {
        let revs = call(cx, Method::Get, &format!("{path}/revisions"), API, None).await?.body;
        for r in revs.get("value").and_then(Value::as_array).into_iter().flatten() {
            let rn = r.get("name").and_then(Value::as_str).unwrap_or_default();
            if rn != want && r.pointer("/properties/active").and_then(Value::as_bool) == Some(true) {
                call(cx, Method::Post, &format!("{path}/revisions/{rn}/deactivate"), API, None).await?;
            }
        }
    }
    live(cx, name).await?.ok_or_else(|| anyhow::anyhow!("container app {name} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    if &*key.kind != "ContainerApp" {
        bail!("azure-containerapps doesn't delete {}", key.kind);
    }
    if let Some(a) = missing(call(cx, Method::Delete, &app_path(cx, &key.name)?, API, None).await)? {
        operation(cx, &a, &format!("deleting container app {}", key.name)).await?;
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

    fn spec(app: &str, ingress: bool) -> Value {
        let mut config = json!({});
        if ingress {
            config = json!({ "ingress": { "external": true, "targetPort": 8080 } });
        }
        json!({ "app": app, "environment": "shop-env", "image": "acme.azurecr.io/shop@${{ release.artifacts.image.digest }}",
            "spec": { "location": "westeurope", "properties": { "configuration": config, "template": { "containers": [{ "name": "web", "env": [{ "name": "MODE", "value": "${{ env.name }}" }] }] } } } })
    }

    /// An app on a faked Container Apps, release by release: the first makes the app and its
    /// revision, which takes all the traffic once it runs; the same release again changes
    /// nothing; the next makes its own revision and the traffic moves to it; one whose container
    /// doesn't start fails and reads as degraded; the release before, rolled back to, moves the
    /// traffic back to its revision and makes nothing. An app without ingress keeps its release's
    /// revision the active one, and a rollback activates the one before again. Then they go.
    #[test]
    fn apps_run_each_releases_revision() -> anyhow::Result<()> {
        let _one = fake_azure::AZURE.lock();
        crate::rt::block_on(async {
            let az = Arc::new(FakeAzure::default());
            fake_azure::start(az.clone()).await?;
            let (target, place) = fake_azure::target("azure-containerapps")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_azure::TOKEN.into()), records: &records, files: &files };
            let release = |app: &str, ingress: bool, version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("azure-containerapps", &place, spec(app, ingress), version, &[], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no app"))
            };
            let cxr = &cx;
            let read = |app: &'static str| async move { Ok::<_, anyhow::Error>(list(cxr, "ContainerApp", &[(String::new(), app.to_owned())]).await?.into_iter().next().unwrap_or_default()) };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let last_call = || az.state.lock().calls.last().cloned().unwrap_or_default();
            let rev_of = |o: &Value| format!("{}--{}", obj::name(o), o.pointer("/spec/properties/template/revisionSuffix").and_then(Value::as_str).unwrap_or_default());

            // The first release: the app, its revision, all the traffic once it runs.
            let a1 = release("shop", true, "1.0.0", "sha256:aaaa")?;
            let r1 = rev_of(&a1);
            let up = apply(&cx, &a1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&a1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(az.state.lock().calls, [format!("PutContainerApp shop revision={r1} traffic={r1}:100")]);
            assert_eq!(obj::label(&up, kiln_cd::engine::APP_LABEL), obj::label(&a1, kiln_cd::engine::APP_LABEL), "tracking in the app's tags");

            // The same release again: nothing changes.
            apply(&cx, &release("shop", true, "1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(az.state.lock().calls.len(), 1);

            // The next: its own revision, and the traffic to it.
            let a2 = release("shop", true, "1.1.0", "sha256:bbbb")?;
            let r2 = rev_of(&a2);
            let up2 = apply(&cx, &a2).await?;
            assert_eq!(h(&up2), Some(Health::Healthy), "{up2}");
            assert_eq!(last_call(), format!("PutContainerApp shop revision={r2} traffic={r2}:100"));

            // One whose container doesn't start: it fails, and reads as degraded.
            let failed = apply(&cx, &release("shop", true, "1.2.0", "sha256:broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("Failed to provision revision"), "{failed}");
            assert_eq!(h(&read("shop").await?), Some(Health::Degraded));

            // Rolled back to the release before: its revision is there; only the traffic moves.
            let made = az.state.lock().apps.get("shop").map_or(0, |a| a.revisions.len());
            let back = apply(&cx, &release("shop", true, "1.1.0", "sha256:bbbb")?).await?;
            assert_eq!(az.state.lock().apps.get("shop").map_or(0, |a| a.revisions.len()), made, "no revision made for a rollback");
            assert_eq!(last_call(), format!("PutContainerApp shop traffic={r2}:100"));
            assert_eq!(h(&back), Some(Health::Healthy), "{back}");
            assert!(synced(&a2, &cx.with_applied(back.clone())), "it reads as the release's: {back}");

            // Without ingress, the release's revision is the one active; a rollback activates the
            // one before again.
            let w1 = release("worker", false, "1.0.0", "sha256:cccc")?;
            let w2 = release("worker", false, "1.1.0", "sha256:dddd")?;
            apply(&cx, &w1).await?;
            apply(&cx, &w2).await?;
            let active = |r: &str| az.state.lock().apps.get("worker").and_then(|a| a.revisions.get(r).map(|x| x.active));
            assert_eq!((active(&rev_of(&w1)), active(&rev_of(&w2))), (Some(false), Some(true)), "the release before let go");
            let wb = apply(&cx, &w1).await?;
            assert_eq!((active(&rev_of(&w1)), active(&rev_of(&w2))), (Some(true), Some(false)), "the release before active again");
            assert_eq!(h(&wb), Some(Health::Healthy), "{wb}");
            assert!(synced(&w1, &cx.with_applied(wb.clone())), "{wb}");

            // What the resource group holds, and the definition that adopts it; then they go.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop")));
            let adopted = adopt(&cx, "shop").await?;
            assert_eq!((adopted.pointer("/definition/environment"), adopted.pointer("/definition/image")), (Some(&json!("shop-env")), Some(&json!("acme.azurecr.io/shop@sha256:bbbb"))), "{adopted}");
            delete(&cx, &key_of(&a2)).await?;
            delete(&cx, &key_of(&w1)).await?;
            assert!(az.state.lock().apps.is_empty(), "deleted");
            Ok(())
        })
    }
}
