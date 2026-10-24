//! `gcp-appengine`'s I/O half (docs/cd-targets.md §3): App Engine's Admin API. The pure half,
//! `kiln_cd::providers::gcp_appengine`, renders the objects.
//!
//! A release's version is made unless App Engine has it: a zip CI built goes to the app's staging
//! bucket first (by its hash, so a zip goes up once), the version is created from it, and kiln
//! waits until it serves. Then the service's traffic split moves to it. A rollback finds the
//! earlier version there and moves the traffic only. A version with traffic is never deleted (App
//! Engine refuses), and neither is the default service: an app's first service lasts as long as
//! the app.

use anyhow::{Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::gcp_appengine::{self as p, GROUP};
use serde_json::{Value, json};

use super::gcp::{call, get, missing, put_object, wait};
use super::{Cx, applied};
use crate::http::client::Method;

const API: &str = "appengine";

fn app(cx: &Cx<'_>) -> Result<String> {
    Ok(format!("apps/{}", cx.place("project")?))
}

async fn version(cx: &Cx<'_>, service: &str, id: &str) -> Result<Option<Value>> {
    cx.conn.budget().await;
    missing(cx.gcp()?.call(API, Method::Get, &format!("/v1/{}/services/{service}/versions/{id}", app(cx)?), &[("view", "FULL")], None).await)
}

async fn service(cx: &Cx<'_>, name: &str) -> Result<Option<Value>> {
    get(cx, API, &format!("/v1/{}/services/{name}", app(cx)?)).await
}

fn split(name: &str) -> Result<(&str, &str)> {
    p::split_name(name).ok_or_else(|| anyhow::anyhow!("`{name}` isn't service:version"))
}

async fn live(cx: &Cx<'_>, kind: &str, name: &str) -> Result<Option<Value>> {
    Ok(match kind {
        "Version" => {
            let (s, id) = split(name)?;
            let applied = cx.applied(GROUP, "Version", "", name);
            version(cx, s, id).await?.and_then(|v| p::live_version(s, &v, applied.as_ref().map(|a| &a.object)))
        }
        "Service" => service(cx, name).await?.and_then(|s| p::live_service(&s)),
        _ => None,
    })
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (_, name) in names {
        out.extend(live(cx, kind, name).await?);
    }
    Ok(out)
}

/// What the app holds: its services and where their traffic goes (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let page = call(cx, API, Method::Get, &format!("/v1/{}/services", app(cx)?), &[("pageSize", "100")], None).await?;
    let out: Vec<Value> = page
        .get("services")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| {
            let split: Vec<String> = s.pointer("/split/allocations").and_then(Value::as_object).into_iter().flatten().map(|(v, w)| format!("{v} {}%", (w.as_f64().unwrap_or(0.0) * 100.0).round())).collect();
            json!({ "kind": "Service", "name": s.get("id"), "summary": split.join(", ") })
        })
        .collect();
    Ok(json!({ "resources": out }))
}

/// A service as it runs now, as the definition that adopts it (§8.2 step 2): the version with
/// most of its traffic.
pub async fn adopt(cx: &Cx<'_>, name: &str) -> Result<Value> {
    let Some(s) = service(cx, name).await? else { bail!("no service {name} here") };
    let top = s.pointer("/split/allocations").and_then(Value::as_object).and_then(|a| a.iter().max_by(|x, y| x.1.as_f64().unwrap_or(0.0).total_cmp(&y.1.as_f64().unwrap_or(0.0))).map(|(v, _)| v.clone()));
    let Some(id) = top else { bail!("service {name} sends its traffic nowhere") };
    let v = version(cx, name, &id).await?.ok_or_else(|| anyhow::anyhow!("version {id} of {name} is gone"))?;
    let live = p::live_version(name, &v, None).ok_or_else(|| anyhow::anyhow!("App Engine answered no version"))?;
    let mut app = live.get("spec").cloned().unwrap_or_else(|| json!({}));
    let image = app.pointer("/deployment/container/image").cloned();
    if let Some(m) = app.as_object_mut() {
        m.remove("deployment");
    }
    let (def, note) = match image {
        Some(i) => (json!({ "service": name, "image": i, "app": app }), None),
        None => (json!({ "service": name, "source": "${{ release.artifacts.site.digest }}", "app": app }), Some("App Engine keeps no zip of a version: point source at the zip CI builds (an artifact), or a gs:// object")),
    };
    Ok(json!({ "definition": def, "note": note, "live": live }))
}

/// Applies a Version (made unless App Engine has it) or a Service (its split moved).
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let name = obj::name(desired);
    match obj::kind(desired) {
        "Version" => {
            let (s, id) = split(name)?;
            let mut made = json!({});
            if version(cx, s, id).await?.is_none() {
                let zip = desired.pointer("/spec/deployment/zip").cloned().unwrap_or(Value::Null);
                let url = match zip.get("artifact").and_then(Value::as_str) {
                    Some(a) => {
                        let hex = a.trim_start_matches("sha256:");
                        let bucket = match zip.get("bucket").and_then(Value::as_str) {
                            Some(b) => b.to_owned(),
                            None => format!("staging.{}.appspot.com", cx.place("project")?),
                        };
                        let object = format!("kiln/appengine/{hex}.zip");
                        put_object(cx, &bucket, &object, hex, "application/zip").await?;
                        Some(format!("https://storage.googleapis.com/{bucket}/{object}"))
                    }
                    None => None,
                };
                let body = p::create_body(desired, id, url.as_deref());
                let op = call(cx, API, Method::Post, &format!("/v1/{}/services/{s}/versions", app(cx)?), &[], Some(&body)).await?;
                wait(cx, API, "v1", op, &format!("deploying version {id} of {s}")).await?;
                made = json!({ "sourceUrl": url });
            }
            cx.record(GROUP, "Version", "", name, Some(applied(desired, made))).await?;
        }
        "Service" => {
            let want = desired.pointer("/spec/split").cloned().unwrap_or(Value::Null);
            let have = service(cx, name).await?.ok_or_else(|| anyhow::anyhow!("service {name} has no version yet: its version comes first"))?;
            if p::live_service(&have).and_then(|l| l.pointer("/spec/split").cloned()).is_none_or(|h| !kiln_cd::providers::gcp_run::covers(&h, &want)) {
                let path = format!("/v1/{}/services/{name}", app(cx)?);
                let op = call(cx, API, Method::Patch, &path, &[("updateMask", "split"), ("migrateTraffic", "false")], Some(&json!({ "split": want }))).await?;
                wait(cx, API, "v1", op, &format!("moving {name}'s traffic")).await?;
            }
            cx.record(GROUP, "Service", "", name, Some(applied(desired, Value::Null))).await?;
        }
        other => bail!("gcp-appengine doesn't apply {other}"),
    }
    live(cx, obj::kind(desired), name).await?.ok_or_else(|| anyhow::anyhow!("{} {name} is gone", obj::kind(desired)))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let path = match &*key.kind {
        "Version" => {
            let (s, id) = split(&key.name)?;
            format!("/v1/{}/services/{s}/versions/{id}", app(cx)?)
        }
        // App Engine never deletes an app's default service: it lasts as long as the app.
        "Service" if &*key.name == "default" => {
            tracing::info!(target = %cx.conn.target.name(), "deployments: App Engine's default service stays: it lasts as long as the app");
            return Ok(());
        }
        "Service" => format!("/v1/{}/services/{}", app(cx)?, key.name),
        other => bail!("gcp-appengine doesn't delete {other}"),
    };
    if let Some(op) = missing(call(cx, API, Method::Delete, &path, &[], None).await)? {
        wait(cx, API, "v1", op, &format!("deleting {} {}", key.kind, key.name)).await?;
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

    const APP_YAML: &str = "runtime: python312\ninstance_class: F2\nentrypoint: gunicorn -b :$PORT main:app\nhandlers:\n- url: /.*\n  script: auto\n  secure: always\n";

    fn spec() -> Value {
        json!({ "service": "shop", "source": "${{ release.artifacts.site.digest }}", "app": "app.yaml", "set": { "envVariables.MODE": "${{ env.name }}" } })
    }

    /// A service on a faked App Engine, release by release: the first puts its zip in the app's
    /// staging bucket and makes a version, which takes the new service's traffic; the same
    /// release again makes and uploads nothing; the next makes its own version, and the traffic
    /// moves to it; the release before, rolled back to, finds its version there and moves only
    /// the traffic; one that doesn't start fails, the traffic where it was; a version with
    /// traffic isn't deleted, one without is; then the service goes.
    #[test]
    fn services_split_to_each_releases_version() -> anyhow::Result<()> {
        let _one = fake_gcp::GCP.lock();
        crate::rt::block_on(async {
            let gcp = Arc::new(FakeGcp::default());
            fake_gcp::start(gcp.clone()).await?;
            let (target, place) = fake_gcp::target("gcp-appengine")?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Bearer(fake_gcp::TOKEN.into()), records: &records, files: &files };
            let release = |version: &str, zip: &str| -> anyhow::Result<Vec<Value>> {
                let digest = format!("sha256:{}", files.add(zip.as_bytes()));
                released_to("gcp-appengine", &place, spec(), version, &[("app.yaml", APP_YAML)], &[("site", &digest)])
            };
            let cxr = &cx;
            let apply_all = |objs: Vec<Value>| async move {
                for o in &objs {
                    apply(cxr, o).await?;
                }
                Ok::<_, anyhow::Error>(objs)
            };
            let one = |kind: &'static str, name: String| async move { Ok::<_, anyhow::Error>(list(cxr, kind, &[(String::new(), name)]).await?.into_iter().next()) };
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let count = |prefix: &str| gcp.state.lock().calls.iter().filter(|c| c.starts_with(prefix)).count();
            let last_call = || gcp.state.lock().calls.last().cloned().unwrap_or_default();
            let id_of = |o: &Value| p::split_name(obj::name(o)).map(|(_, id)| id.to_owned()).unwrap_or_default();

            // The first release: its zip in the staging bucket, its version, the service's traffic.
            let first = apply_all(release("1.0.0", "zip of shop 1.0.0")?).await?;
            let (v1, svc1) = (first.first().cloned().unwrap_or_default(), first.get(1).cloned().unwrap_or_default());
            let id1 = id_of(&v1);
            let live_v1 = one("Version", obj::name(&v1).to_owned()).await?.unwrap_or_default();
            assert_eq!(h(&live_v1), Some(Health::Healthy), "{live_v1}");
            assert!(synced(&v1, &cx.with_applied(live_v1.clone())), "{live_v1}");
            let live_s = one("Service", "shop".into()).await?.unwrap_or_default();
            assert!(synced(&svc1, &cx.with_applied(live_s.clone())) && h(&live_s) == Some(Health::Healthy), "{live_s}");
            let calls = gcp.state.lock().calls.clone();
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(calls.first().is_some_and(|c| c.starts_with("UploadObject staging.acme-prod.appspot.com/kiln/appengine/")), "{calls:?}");
            assert_eq!(calls.get(1), Some(&format!("CreateVersion shop {id1}")), "a new service's traffic goes to its first version");

            // The same release again: nothing made or uploaded.
            apply_all(release("1.0.0", "zip of shop 1.0.0")?).await?;
            assert_eq!(gcp.state.lock().calls.len(), 2);

            // The next: its own version, and the traffic moved to it.
            let second = apply_all(release("1.1.0", "zip of shop 1.1.0")?).await?;
            let id2 = id_of(second.first().unwrap_or(&Value::Null));
            assert_ne!(id1, id2);
            assert_eq!((count("UploadObject"), count("CreateVersion")), (2, 2));
            assert_eq!(last_call(), format!("UpdateService shop split={id2}:1"));

            // Rolled back to the release before: its version is there, only the traffic moves.
            apply_all(release("1.0.0", "zip of shop 1.0.0")?).await?;
            assert_eq!((count("UploadObject"), count("CreateVersion")), (2, 2), "nothing made or uploaded for a rollback");
            assert_eq!(last_call(), format!("UpdateService shop split={id1}:1"));

            // One that doesn't start: its version fails, and the traffic stays where it was.
            let broken = release("1.2.0", "a broken zip")?;
            let failed = apply(&cx, broken.first().unwrap_or(&Value::Null)).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("failed to start"), "{failed}");
            let still = one("Service", "shop".into()).await?.unwrap_or_default();
            assert!(synced(&svc1, &cx.with_applied(still.clone())), "the traffic stayed: {still}");

            // A version with traffic isn't deleted; one without is.
            assert!(delete(&cx, &key_of(&v1)).await.is_err_and(|e| format!("{e:#}").contains("non-zero traffic")));
            delete(&cx, &key_of(second.first().unwrap_or(&Value::Null))).await?;
            assert!(one("Version", format!("shop:{id2}")).await?.is_none(), "pruned");

            // What the app holds, and the definition that adopts it; then the service goes.
            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("shop")));
            let adopted = adopt(&cx, "shop").await?;
            assert_eq!(adopted.pointer("/definition/app/runtime"), Some(&json!("python312")), "{adopted}");
            delete(&cx, &key_of(&svc1)).await?;
            assert!(one("Service", "shop".into()).await?.is_none(), "deleted");
            // The default service is the app's for as long as it lasts.
            delete(&cx, &ResKey::new(GROUP, "Service", "", "default")).await?;
            Ok(())
        })
    }
}
