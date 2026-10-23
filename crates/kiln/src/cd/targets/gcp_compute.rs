//! `gcp-compute`'s I/O half (docs/cd-targets.md §3): Compute Engine's API. The pure half,
//! `kiln_cd::providers::gcp_compute`, renders the objects.
//!
//! A release's instance template is made unless kiln made it before (they never change, so the
//! same properties are the same template), then the group's version moves to it, at the
//! fingerprint kiln read, and Compute Engine rolls the instances over by the group's update
//! policy. Its health follows the group: stable on its version, or the errors listErrors gives
//! since kiln's change. kiln keeps the templates it made until the app goes, so a rollback finds
//! its template there and only the group moves.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::gcp_compute::{self as p, GROUP};
use kiln_cd::providers::gcp_run::{covers, short};
use serde_json::{Value, json};

use super::gcp::{SETTLE, call, get, missing};
use super::{Cx, applied, until};
use crate::ci::integrations::gcp::GcpError;
use crate::http::client::Method;

const API: &str = "compute";

fn project<'a>(cx: &'a Cx<'_>) -> Result<&'a str> {
    cx.place("project")
}

fn zone_path(cx: &Cx<'_>) -> Result<String> {
    Ok(format!("/compute/v1/projects/{}/zones/{}", project(cx)?, cx.place("zone")?))
}

fn templates_path(cx: &Cx<'_>) -> Result<String> {
    Ok(format!("/compute/v1/projects/{}/global/instanceTemplates", project(cx)?))
}

/// Waits for a Compute Engine operation (at its `selfLink`) to be done; why it failed, if it did.
async fn wait(cx: &Cx<'_>, op: Value, what: &str) -> Result<()> {
    let done = |o: &Value| o.get("status").and_then(Value::as_str) == Some("DONE");
    let op = if done(&op) {
        op
    } else {
        let link = op.get("selfLink").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("{what}: Compute Engine answered no operation"))?;
        let path = format!("/compute/v1/{}", link.split_once("/compute/v1/").map_or(link, |(_, rest)| rest));
        until(cx.conn, SETTLE, what, || async { Ok(cx.gcp()?.call(API, Method::Get, &path, &[], None).await.map(|o| done(&o).then_some(o))?) }).await?
    };
    let errors: Vec<String> = op.pointer("/error/errors").and_then(Value::as_array).into_iter().flatten().filter_map(|e| e.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
    if !errors.is_empty() {
        bail!("{what}: {}", errors.join("; "));
    }
    Ok(())
}

async fn template(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    get(cx, API, &format!("{}/{name}", templates_path(cx)?)).await
}

async fn group(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    get(cx, API, &format!("{}/instanceGroupManagers/{name}", zone_path(cx)?)).await
}

/// A group as kiln reads it: with the errors its instances met since kiln last changed it, while
/// it isn't stable.
async fn live_group(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    let Some(mut answer) = group(cx, name).await? else { return Ok(None) };
    if answer.pointer("/status/isStable").and_then(Value::as_bool) != Some(true) {
        let since = cx.applied(GROUP, "InstanceGroupManager", "", name).map_or(0, |a| a.at_ms);
        let errors = call(cx, API, Method::Get, &format!("{}/instanceGroupManagers/{name}/listErrors", zone_path(cx)?), &[], None).await?;
        let recent: Vec<Value> = errors
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|e| {
                let at = e.get("timestamp").and_then(Value::as_str).and_then(|t| time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok());
                at.is_none_or(|t| t.unix_timestamp_nanos() / 1_000_000 >= i128::from(since))
            })
            .cloned()
            .collect();
        if let Value::Object(m) = &mut answer {
            m.insert("errors".into(), Value::Array(recent));
        }
    }
    Ok(p::live_group(&answer))
}

async fn live(cx: &Cx<'_>, kind: &str, name: &str) -> Result<Option<Value>> {
    match kind {
        "InstanceTemplate" => {
            let Some(rec) = cx.applied(GROUP, "InstanceTemplate", "", name) else { return Ok(None) };
            let Some(made) = rec.made.get("name").and_then(Value::as_str) else { return Ok(None) };
            Ok(template(cx, made).await?.and_then(|t| p::live_template(name, &t, Some(&rec.object))))
        }
        "InstanceGroupManager" => live_group(cx, name).await,
        _ => Ok(None),
    }
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        out.extend(live(cx, kind, name).await?);
    }
    Ok(out)
}

/// What the zone holds: its managed instance groups (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, API, Method::Get, &format!("{}/instanceGroupManagers", zone_path(cx)?), &[("maxResults", "500")], None).await?;
    let out: Vec<Value> = page
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|g| {
            let template = g.pointer("/versions/0/instanceTemplate").or_else(|| g.get("instanceTemplate")).and_then(Value::as_str).map(short).unwrap_or_default();
            json!({ "kind": "InstanceGroupManager", "name": g.get("name"), "summary": format!("{} instances · {template}", g.get("targetSize").and_then(Value::as_u64).unwrap_or(0)), "updated": g.get("creationTimestamp") })
        })
        .collect();
    Ok(json!({ "resources": out }))
}

/// A group as it runs now, as the definition that adopts it (§8.2 step 2): its template's
/// properties, image apart.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(g) = group(cx, name).await? else { bail!("no instance group {name} here") };
    let live = p::live_group(&g).ok_or_else(|| anyhow::anyhow!("Compute Engine answered no group"))?;
    let t = g.pointer("/versions/0/instanceTemplate").or_else(|| g.get("instanceTemplate")).and_then(Value::as_str).map(short).unwrap_or_default().to_owned();
    let tmpl = template(cx, &t).await?.ok_or_else(|| anyhow::anyhow!("template {t} is gone"))?;
    let mut properties = p::unprefixed(tmpl.get("properties").unwrap_or(&Value::Null));
    let image = properties.get_mut("disks").and_then(Value::as_array_mut).and_then(|d| d.iter_mut().find(|d| d.get("boot").and_then(Value::as_bool) == Some(true))).and_then(|d| d.pointer_mut("/initializeParams")).and_then(Value::as_object_mut).and_then(|p| p.remove("sourceImage"));
    let mut spec = live.get("spec").cloned().unwrap_or_else(|| json!({}));
    let update = spec.as_object_mut().and_then(|m| {
        m.remove("versions");
        m.remove("updatePolicy")
    });
    // kiln names its templates <template>-<hash>.
    let base = t.rsplit_once('-').filter(|(_, h)| h.len() == 10 && h.bytes().all(|b| b.is_ascii_hexdigit())).map_or(t.as_str(), |(b, _)| b);
    let def = json!({ "group": name, "template": base, "image": image, "properties": properties, "group-spec": spec, "update": update });
    Ok(json!({ "definition": def, "note": "kiln makes a template of each release's properties, named for them, and moves the group to it", "live": live }))
}

/// Applies an InstanceTemplate (made unless it's there) or an InstanceGroupManager (made, or its
/// version moved, at the fingerprint kiln read).
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    match obj::kind(desired) {
        "InstanceTemplate" => {
            let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
            let made = p::template_name(name, &spec);
            if template(cx, &made).await?.is_none() {
                let body = json!({ "name": made, "description": format!("kiln: {}", super::app_of(desired).unwrap_or_default()), "properties": spec.get("properties") });
                let op = match call(cx, API, Method::Post, &templates_path(cx)?, &[], Some(&body)).await {
                    Ok(op) => op,
                    // Made meanwhile: the same name is the same properties.
                    Err(e) if GcpError::is_conflict(&e) => json!({ "status": "DONE" }),
                    Err(e) => return Err(e),
                };
                wait(cx, op, &format!("making instance template {made}")).await?;
            }
            // Every template kiln made for it, to let go of when the app goes.
            let mut all: Vec<String> = cx.applied(GROUP, "InstanceTemplate", "", name).and_then(|a| a.made.get("made").and_then(|m| serde_json::from_value(m.clone()).ok())).unwrap_or_default();
            if !all.contains(&made) {
                all.push(made.clone());
            }
            cx.record(GROUP, "InstanceTemplate", "", name, Some(applied(desired, json!({ "name": made, "made": all })))).await?;
        }
        "InstanceGroupManager" => {
            let body = p::group_body(desired, project(cx)?);
            match group(cx, name).await? {
                None => {
                    let mut b = body.clone();
                    if let Value::Object(m) = &mut b {
                        m.insert("name".into(), json!(name));
                    }
                    let op = call(cx, API, Method::Post, &format!("{}/instanceGroupManagers", zone_path(cx)?), &[], Some(&b)).await?;
                    wait(cx, op, &format!("making instance group {name}")).await?;
                }
                Some(g) => {
                    let live = p::live_group(&g).ok_or_else(|| anyhow::anyhow!("Compute Engine answered no group"))?;
                    if !covers(live.get("spec").unwrap_or(&Value::Null), desired.get("spec").unwrap_or(&Value::Null)) {
                        let mut b = body.clone();
                        if let (Value::Object(m), Some(f)) = (&mut b, g.get("fingerprint")) {
                            m.insert("fingerprint".into(), f.clone());
                        }
                        let op = call(cx, API, Method::Patch, &format!("{}/instanceGroupManagers/{name}", zone_path(cx)?), &[], Some(&b)).await.map_err(|e| match e.downcast_ref::<GcpError>() {
                            Some(g) if g.status == 412 => anyhow::anyhow!("instance group {name} changed meanwhile ({g}): kiln reads it again and retries"),
                            _ => e,
                        })?;
                        wait(cx, op, &format!("moving instance group {name}")).await?;
                    }
                }
            }
            cx.record(GROUP, "InstanceGroupManager", "", name, Some(applied(desired, Value::Null))).await?;
        }
        other => bail!("gcp-compute doesn't apply {other}"),
    }
    live(cx, obj::kind(desired), name).await?.ok_or_else(|| anyhow::anyhow!("{} {name} is gone", obj::kind(desired)))
}

/// Deletes a group (its instances go with it), or every template kiln made for an
/// InstanceTemplate.
pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    match &*key.kind {
        "InstanceGroupManager" => {
            if let Some(op) = missing(call(cx, API, Method::Delete, &format!("{}/instanceGroupManagers/{}", zone_path(cx)?, key.name), &[], None).await)? {
                wait(cx, op, &format!("deleting instance group {}", key.name)).await?;
            }
        }
        "InstanceTemplate" => {
            let rec = cx.applied(GROUP, "InstanceTemplate", "", &key.name);
            let made: Vec<String> = rec.and_then(|a| a.made.get("made").and_then(|m| serde_json::from_value(m.clone()).ok())).unwrap_or_default();
            for t in made {
                if let Some(op) = missing(call(cx, API, Method::Delete, &format!("{}/{t}", templates_path(cx)?), &[], None).await)? {
                    wait(cx, op, &format!("deleting instance template {t}")).await?;
                }
            }
        }
        other => bail!("gcp-compute doesn't delete {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::{self, FakeGcp, synced};
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    fn spec() -> Value {
        json!({ "group": "web", "template": "web", "image": "projects/acme-prod/global/images/${{ release.artifacts.image.digest }}",
            "properties": { "machineType": "e2-medium", "networkInterfaces": [{ "network": "projects/acme-prod/global/networks/default" }], "metadata": { "items": [{ "key": "env", "value": "${{ env.name }}" }] } },
            "group-spec": { "targetSize": 2, "namedPorts": [{ "name": "http", "port": 8080 }] }, "update": { "maxSurge": { "fixed": 1 }, "maxUnavailable": { "fixed": 0 } } })
    }

    /// A group on a faked Compute Engine, release by release: the first makes its template and the
    /// group, whose instances start; the same release again makes nothing; the next makes its own
    /// template, and the group replaces its instances onto it; one whose image doesn't boot
    /// leaves the group failing, read as degraded, its instances before serving still; the
    /// release before, rolled back to, finds its template there and the group goes back to it;
    /// then it all goes, every template kiln made too.
    #[test]
    fn groups_roll_onto_each_releases_template() -> anyhow::Result<()> {
        let _one = fake_gcp::GCP.lock();
        crate::rt::block_on(async {
            let gcp = Arc::new(FakeGcp::default());
            fake_gcp::start(gcp.clone()).await?;
            let (target, place) = fake_gcp::target("gcp-compute")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_gcp::TOKEN.into()), records: &records, files: &files };
            let release = |version: &str, image: &str| released_to("gcp-compute", &place, spec(), version, &[], &[("image", image)]);
            let cxr = &cx;
            let apply_all = |objs: Vec<Value>| async move {
                for o in &objs {
                    apply(cxr, o).await?;
                }
                Ok::<_, anyhow::Error>(objs)
            };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let names = [(String::new(), "web".to_owned())];
            let group_now = || async { Ok::<_, anyhow::Error>(list(cxr, "InstanceGroupManager", &names).await?.into_iter().next().unwrap_or_default()) };
            let settled = || async {
                for _ in 0..20 {
                    let g = group_now().await?;
                    if matches!(h(&g), Some(Health::Healthy | Health::Degraded)) {
                        return Ok::<_, anyhow::Error>(g);
                    }
                }
                anyhow::bail!("the group didn't settle")
            };
            let count = |prefix: &str| gcp.state.lock().calls.iter().filter(|c| c.starts_with(prefix)).count();
            let last_call = || gcp.state.lock().calls.last().cloned().unwrap_or_default();
            let made_of = |objs: &[Value]| objs.get(1).and_then(|g| g.pointer("/spec/versions/0/instanceTemplate")).and_then(Value::as_str).unwrap_or_default().to_owned();

            // The first release: its template, and the group, whose instances start.
            let first = apply_all(release("1.0.0", "web-1-0-0")?).await?;
            let t1 = made_of(&first);
            assert_eq!(gcp.state.lock().calls, [format!("InsertTemplate {t1}"), format!("InsertGroup web template={t1}")]);
            let up = settled().await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(first.get(1).unwrap_or(&Value::Null), &cx.with_applied(up.clone())), "in sync: {up}");
            let tmpl = list(&cx, "InstanceTemplate", &names).await?.into_iter().next().unwrap_or_default();
            assert!(synced(first.first().unwrap_or(&Value::Null), &cx.with_applied(tmpl.clone())), "the template reads as kiln made it: {tmpl}");

            // The same release again: nothing made or moved.
            apply_all(release("1.0.0", "web-1-0-0")?).await?;
            assert_eq!(gcp.state.lock().calls.len(), 2);

            // The next: its own template, and the group's instances replaced onto it.
            let second = release("1.1.0", "web-1-1-0")?;
            apply(&cx, second.first().unwrap_or(&Value::Null)).await?;
            let rolling = apply(&cx, second.get(1).unwrap_or(&Value::Null)).await?;
            let t2 = made_of(&second);
            assert_ne!(t1, t2);
            assert_eq!(last_call(), format!("PatchGroup web template={t2}"));
            assert_eq!(h(&rolling), Some(Health::Progressing), "rolling: {rolling}");
            let up2 = settled().await?;
            assert_eq!(h(&up2), Some(Health::Healthy), "{up2}");
            assert!(gcp.state.lock().groups.get("web").is_some_and(|g| g.instances.iter().all(|i| i.template == t2)));

            // One whose image doesn't boot: the group fails, read as degraded, and its instances
            // before serve still.
            let third = apply_all(release("1.2.0", "broken-1-2-0")?).await?;
            let down = settled().await?;
            assert_eq!(h(&down), Some(Health::Degraded), "{down}");
            assert!(kiln_cd::providers::health(&down).is_some_and(|s| s.message.contains("failed to start")));
            assert!(gcp.state.lock().groups.get("web").is_some_and(|g| g.instances.iter().all(|i| i.template == t2 && i.running)), "the instances before serve");

            // Rolled back to the release before: its template is there, and only the group moves.
            apply_all(release("1.1.0", "web-1-1-0")?).await?;
            assert_eq!(count("InsertTemplate"), 3, "no template made for a rollback");
            assert_eq!(last_call(), format!("PatchGroup web template={t2}"));
            let back = settled().await?;
            assert_eq!(h(&back), Some(Health::Healthy), "{back}");
            assert!(synced(second.get(1).unwrap_or(&Value::Null), &cx.with_applied(back.clone())), "{back}");

            // What the zone holds, and the definition that adopts it; then it all goes.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("web")));
            let adopted = adopt(&cx, "web").await?;
            assert_eq!((adopted.pointer("/definition/template"), adopted.pointer("/definition/image")), (Some(&json!("web")), Some(&json!("projects/acme-prod/global/images/web-1-1-0"))), "{adopted}");
            delete(&cx, &key_of(third.get(1).unwrap_or(&Value::Null))).await?;
            delete(&cx, &key_of(third.first().unwrap_or(&Value::Null))).await?;
            assert!(gcp.state.lock().groups.is_empty() && gcp.state.lock().templates.is_empty(), "every template kiln made goes with it");
            assert_eq!(count("DeleteTemplate"), 3);
            Ok(())
        })
    }
}
